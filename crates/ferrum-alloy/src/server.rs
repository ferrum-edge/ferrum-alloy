//! HTTP/1.1 and HTTP/2 serving with bounded resources and graceful drain.
//!
//! The accept loop enforces a connection limit, request-head limits and
//! read timeout, and hands each request the transport identity of its
//! connection ([`PeerInfo`], and axum's `ConnectInfo`). On shutdown it stops
//! accepting, asks every connection to finish (HTTP/1.1 `Connection: close`
//! after the current response, HTTP/2 `GOAWAY`), waits up to the drain budget
//! for in-flight requests and response streams, then force-closes the rest.
//!
//! Upgraded connections (WebSocket) leave Hyper's control after the `101`
//! response: they are not counted against the connection limit and are not
//! drained. Applications own them and should watch
//! [`crate::Lifecycle::shutdown_token`].

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use ferrum_alloy_telemetry::PeerInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use crate::lifecycle::Lifecycle;

/// Connection-level counters.
#[derive(Debug, Default)]
pub struct ServerStats {
    /// Connections currently open (excluding upgraded sessions).
    pub active_connections: AtomicU64,
    /// Connections closed immediately because the limit was reached.
    pub rejected_connections: AtomicU64,
    /// TLS handshakes that failed or timed out.
    pub tls_handshake_failures: AtomicU64,
    /// Connections force-closed after the drain budget.
    pub force_closed_connections: AtomicU64,
}

impl ServerStats {
    /// Prometheus text for these counters.
    pub fn render_prometheus(&self, listener: &str) -> String {
        let mut out = String::new();
        for (name, kind, help, value) in [
            (
                "ferrum_alloy_active_connections",
                "gauge",
                "Open connections.",
                &self.active_connections,
            ),
            (
                "ferrum_alloy_rejected_connections_total",
                "counter",
                "Connections closed at the connection limit.",
                &self.rejected_connections,
            ),
            (
                "ferrum_alloy_tls_handshake_failures_total",
                "counter",
                "Failed or timed-out TLS handshakes.",
                &self.tls_handshake_failures,
            ),
            (
                "ferrum_alloy_force_closed_connections_total",
                "counter",
                "Connections force-closed after the drain budget.",
                &self.force_closed_connections,
            ),
        ] {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{{listener=\"{listener}\"}} {}\n",
                value.load(Ordering::Relaxed)
            ));
        }
        out
    }
}

/// Per-listener options.
#[derive(Debug, Clone)]
pub(crate) struct ServeOptions {
    pub(crate) name: &'static str,
    pub(crate) max_connections: usize,
    pub(crate) max_header_count: usize,
    pub(crate) max_header_bytes: usize,
    pub(crate) http2_max_concurrent_streams: u32,
    pub(crate) header_read_timeout: Duration,
    pub(crate) drain_timeout: Duration,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<crate::tls::TlsServer>,
}

fn builder(options: &ServeOptions) -> Builder<TokioExecutor> {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(options.header_read_timeout)
        .max_headers(options.max_header_count)
        .max_buf_size(options.max_header_bytes.max(8_192));
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(options.http2_max_concurrent_streams)
        .max_header_list_size(u32::try_from(options.max_header_bytes).unwrap_or(u32::MAX))
        .max_pending_accept_reset_streams(Some(64))
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20));
    builder
}

/// Serves `app` on `listener` until the lifecycle stops accepting, then
/// drains within the budget.
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    options: ServeOptions,
    lifecycle: Lifecycle,
    stats: Arc<ServerStats>,
) -> io::Result<()> {
    let builder = builder(&options);
    let permits = Arc::new(Semaphore::new(options.max_connections));
    let tracker = TaskTracker::new();
    let mut backoff = Duration::from_millis(5);
    loop {
        let accepted = tokio::select! {
            biased;
            () = lifecycle.stop_accepting().cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, remote) = match accepted {
            Ok(accepted) => {
                backoff = Duration::from_millis(5);
                accepted
            }
            Err(error) => {
                // Resource exhaustion (EMFILE) and similar: back off instead
                // of spinning.
                tracing::warn!(target: "ferrum_alloy::server", listener = options.name, %error, "accept failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            stats.rejected_connections.fetch_add(1, Ordering::Relaxed);
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let builder = builder.clone();
        let app = app.clone();
        let lifecycle = lifecycle.clone();
        let stats = Arc::clone(&stats);
        let options = options.clone();
        tracker.spawn(async move {
            stats.active_connections.fetch_add(1, Ordering::Relaxed);
            handle(stream, remote, builder, app, &options, &lifecycle, &stats).await;
            stats.active_connections.fetch_sub(1, Ordering::Relaxed);
            drop(permit);
        });
    }
    drop(listener);
    tracker.close();
    lifecycle.drain_connections().cancel();
    if tokio::time::timeout(options.drain_timeout, tracker.wait())
        .await
        .is_err()
    {
        tracing::warn!(
            target: "ferrum_alloy::server",
            listener = options.name,
            remaining = tracker.len(),
            "drain budget exhausted; closing remaining connections"
        );
        lifecycle.force_close().cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), tracker.wait()).await;
    }
    Ok(())
}

async fn handle(
    stream: TcpStream,
    remote: SocketAddr,
    builder: Builder<TokioExecutor>,
    app: Router,
    options: &ServeOptions,
    lifecycle: &Lifecycle,
    stats: &ServerStats,
) {
    #[cfg(feature = "tls")]
    if let Some(tls) = &options.tls {
        let handshake =
            tokio::time::timeout(tls.handshake_timeout, tls.acceptor.accept(stream)).await;
        let stream = match handshake {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                stats.tls_handshake_failures.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(target: "ferrum_alloy::tls", %remote, %error, "TLS handshake failed");
                return;
            }
            Err(_) => {
                stats.tls_handshake_failures.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(target: "ferrum_alloy::tls", %remote, "TLS handshake timed out");
                return;
            }
        };
        let peer = PeerInfo {
            remote_addr: Some(remote),
            tls: crate::tls::peer_identity(stream.get_ref().1),
        };
        serve_io(stream, peer, builder, app, lifecycle, stats).await;
        return;
    }
    let _ = options;
    let peer = PeerInfo {
        remote_addr: Some(remote),
        tls: None,
    };
    serve_io(stream, peer, builder, app, lifecycle, stats).await;
}

async fn serve_io<I>(
    io: I,
    peer: PeerInfo,
    builder: Builder<TokioExecutor>,
    app: Router,
    lifecycle: &Lifecycle,
    stats: &ServerStats,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let remote = peer.remote_addr;
    let service = app.map_request(move |request: http::Request<Incoming>| {
        let mut request = request.map(Body::new);
        request.extensions_mut().insert(peer.clone());
        if let Some(remote) = remote {
            request.extensions_mut().insert(ConnectInfo(remote));
        }
        request
    });
    let connection =
        builder.serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(service));
    tokio::pin!(connection);
    let result = tokio::select! {
        result = connection.as_mut() => result,
        () = lifecycle.drain_connections().cancelled() => {
            connection.as_mut().graceful_shutdown();
            tokio::select! {
                result = connection.as_mut() => result,
                () = lifecycle.force_close().cancelled() => {
                    stats.force_closed_connections.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            }
        }
    };
    if let Err(error) = result {
        tracing::debug!(target: "ferrum_alloy::server", %error, "connection ended with an error");
    }
}
