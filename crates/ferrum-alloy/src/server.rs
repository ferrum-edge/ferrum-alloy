//! HTTP/1.1 and HTTP/2 serving with bounded resources and graceful drain.
//!
//! The accept loop enforces a connection limit, request-head limits and
//! read timeout, and hands each request the transport identity of its
//! connection ([`PeerInfo`], and axum's `ConnectInfo`). The header read
//! timeout also bounds the time from a ready connection (after any TLS
//! handshake) to its first request head, whatever the protocol, so a peer
//! that sends nothing or only part of the HTTP/2 preface cannot keep a
//! connection slot.
//!
//! On shutdown it stops accepting, abandons unfinished TLS handshakes, asks
//! every connection to finish (HTTP/1.1 `Connection: close` after the current
//! response, HTTP/2 `GOAWAY`), waits up to the drain budget for in-flight
//! requests and response streams, then force-closes the rest. The listener
//! returns only after every connection task has ended.
//!
//! Upgraded connections (WebSocket) leave Hyper's control after the `101`
//! response: they are not counted against the connection limit and are not
//! drained. Applications own them and should watch
//! [`crate::Lifecycle::shutdown_token`].

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
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

/// Holds a connection slot and the `active_connections` gauge for the life of
/// a connection task, including a task aborted at the end of the drain.
struct ActiveConnection {
    stats: Arc<ServerStats>,
    _permit: OwnedSemaphorePermit,
}

impl ActiveConnection {
    fn open(stats: Arc<ServerStats>, permit: OwnedSemaphorePermit) -> Self {
        stats.active_connections.fetch_add(1, Ordering::Relaxed);
        Self {
            stats,
            _permit: permit,
        }
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.stats
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
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
/// drains within the budget. Returns once every connection task has ended:
/// connections still open when the budget runs out are aborted and counted
/// in `force_closed_connections`.
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    options: ServeOptions,
    lifecycle: Lifecycle,
    stats: Arc<ServerStats>,
) -> io::Result<()> {
    let builder = builder(&options);
    let permits = Arc::new(Semaphore::new(options.max_connections));
    let mut connections = JoinSet::new();
    let mut backoff = Duration::from_millis(5);
    loop {
        let accepted = tokio::select! {
            biased;
            () = lifecycle.stop_accepting().cancelled() => break,
            // Reap finished connection tasks so the set stays small.
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
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
        connections.spawn(async move {
            let _active = ActiveConnection::open(Arc::clone(&stats), permit);
            handle(stream, remote, builder, app, &options, &lifecycle, &stats).await;
        });
    }
    drop(listener);
    lifecycle.drain_connections().cancel();
    let drained = tokio::time::timeout(options.drain_timeout, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_ok();
    if !drained {
        while connections.try_join_next().is_some() {}
        let remaining = connections.len();
        tracing::warn!(
            target: "ferrum_alloy::server",
            listener = options.name,
            remaining,
            "drain budget exhausted; closing remaining connections"
        );
        stats
            .force_closed_connections
            .fetch_add(remaining as u64, Ordering::Relaxed);
        // Aborting a task drops its connection and socket; `shutdown` returns
        // once every task has ended.
        connections.shutdown().await;
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
        let accept = tokio::time::timeout(tls.handshake_timeout, tls.acceptor.accept(stream));
        // A handshake that finishes after shutdown began could never serve a
        // request, so stop waiting for it.
        let handshake = tokio::select! {
            biased;
            () = lifecycle.drain_connections().cancelled() => {
                tracing::debug!(target: "ferrum_alloy::tls", %remote, "TLS handshake abandoned at shutdown");
                return;
            }
            handshake = accept => handshake,
        };
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
        serve_io(stream, peer, builder, app, options, lifecycle).await;
        return;
    }
    let _ = stats;
    let peer = PeerInfo {
        remote_addr: Some(remote),
        tls: None,
    };
    serve_io(stream, peer, builder, app, options, lifecycle).await;
}

async fn serve_io<I>(
    io: I,
    peer: PeerInfo,
    builder: Builder<TokioExecutor>,
    app: Router,
    options: &ServeOptions,
    lifecycle: &Lifecycle,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let remote = peer.remote_addr;
    let first_request = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&first_request);
    let service = app.map_request(move |request: http::Request<Incoming>| {
        seen.store(true, Ordering::Release);
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
    // Protocol detection waits for enough bytes to rule out the HTTP/2
    // preface before either protocol starts, Hyper's HTTP/1 header timer
    // starts only after detection, and HTTP/2 has no request-head timer. This
    // deadline covers all of it: no request head by then closes the
    // connection. Once a request arrived, it no longer applies.
    let first_request_deadline = tokio::time::sleep(options.header_read_timeout);
    tokio::pin!(first_request_deadline);
    let mut awaiting_first_request = true;
    let mut draining = false;
    let result = loop {
        tokio::select! {
            result = connection.as_mut() => break result,
            () = first_request_deadline.as_mut(), if awaiting_first_request => {
                awaiting_first_request = false;
                if !first_request.load(Ordering::Acquire) {
                    tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "no request head within the header read timeout; closing");
                    return;
                }
            }
            () = lifecycle.drain_connections().cancelled(), if !draining => {
                draining = true;
                connection.as_mut().graceful_shutdown();
            }
        }
    };
    if let Err(error) = result {
        tracing::debug!(target: "ferrum_alloy::server", %error, "connection ended with an error");
    }
}
