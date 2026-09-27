//! HTTP/1.1 and HTTP/2 serving with bounded resources and graceful drain.
//!
//! The accept loop enforces a connection limit, request-head limits and
//! read timeout, and hands each request the transport identity of its
//! connection ([`PeerInfo`], and axum's `ConnectInfo`). The header read
//! timeout also bounds the time from a ready connection (after any TLS
//! handshake) to its first request head, whatever the protocol, so a peer
//! that sends nothing or only part of the HTTP/2 preface cannot keep a
//! connection slot. After that, the idle timeout bounds the time a
//! connection may spend with no request in flight, so a peer that sends one
//! cheap request and then only answers HTTP/2 keep-alive pings cannot keep
//! its slot either. An established HTTP/2 connection is sent `GOAWAY` at
//! either deadline, so a client can retry elsewhere, and is closed shortly
//! after.
//!
//! On shutdown it stops accepting, abandons unfinished TLS handshakes, asks
//! every connection to finish (HTTP/1.1 `Connection: close` after the current
//! response, HTTP/2 `GOAWAY`), waits up to the drain budget for in-flight
//! requests and response streams, then force-closes the rest. HTTP/2 stream
//! tasks, which run a request's handler and response body apart from their
//! connection task, are tracked by the drain as well and cancelled at the
//! budget. The listener returns only after every connection task and stream
//! task has ended, including aborted tasks that are still unwinding, so every
//! connection socket is closed and no handler is running by then.
//!
//! Upgraded connections (WebSocket) leave Hyper's control after the `101`
//! response: they are not counted against the connection limit and are not
//! drained. Applications own them and should watch
//! [`crate::Lifecycle::shutdown_token`].

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use ferrum_alloy_telemetry::PeerInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use pin_project_lite::pin_project;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
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
    /// Connections closed because no request head arrived within the header
    /// read timeout.
    pub first_request_timeouts: AtomicU64,
    /// Connections closed after serving a request because no request was in
    /// flight for the idle timeout.
    pub idle_timeouts: AtomicU64,
    /// HTTP/2 stream tasks (a request's handler and response body) still
    /// running at the end of the drain budget, then cancelled.
    pub force_closed_streams: AtomicU64,
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
            (
                "ferrum_alloy_first_request_timeouts_total",
                "counter",
                "Connections closed without a request head within the header read timeout.",
                &self.first_request_timeouts,
            ),
            (
                "ferrum_alloy_idle_timeouts_total",
                "counter",
                "Connections closed with no request in flight for the idle timeout.",
                &self.idle_timeouts,
            ),
            (
                "ferrum_alloy_force_closed_streams_total",
                "counter",
                "HTTP/2 stream tasks cancelled after the drain budget.",
                &self.force_closed_streams,
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
    pub(crate) idle_timeout: Duration,
    pub(crate) drain_timeout: Duration,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<crate::tls::TlsServer>,
}

/// Runs HTTP/2 stream tasks where the drain can wait for them and cancel
/// them at the budget. Hyper spawns one per request: it runs the handler and
/// then sends the response body, apart from the connection task.
#[derive(Clone, Default)]
struct StreamExecutor {
    tracker: TaskTracker,
    cancel: CancellationToken,
}

impl<F> hyper::rt::Executor<F> for StreamExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let cancel = self.cancel.clone();
        self.tracker.spawn(async move {
            // Cancelling drops the handler and response body, like aborting
            // the task would.
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                _ = future => {}
            }
        });
    }
}

fn builder(options: &ServeOptions, streams: StreamExecutor) -> Builder<StreamExecutor> {
    let mut builder = Builder::new(streams);
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
/// drains within the budget. Returns once every connection task and HTTP/2
/// stream task has ended: connections still open when the budget runs out
/// are aborted and counted in `force_closed_connections`, and stream tasks
/// still running are cancelled and counted in `force_closed_streams`.
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    options: ServeOptions,
    lifecycle: Lifecycle,
    stats: Arc<ServerStats>,
) -> io::Result<()> {
    let streams = StreamExecutor::default();
    let builder = builder(&options, streams.clone());
    let permits = Arc::new(Semaphore::new(options.max_connections));
    let mut connections = JoinSet::new();
    let mut backoff = Duration::from_millis(5);
    loop {
        let accepted = tokio::select! {
            biased;
            () = lifecycle.stop_accepting().cancelled() => break,
            // Reap finished connection tasks so the set stays small.
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Some(error) = joined.err().filter(JoinError::is_panic) {
                    tracing::warn!(target: "ferrum_alloy::server", listener = options.name, %error, "connection task panicked");
                }
                continue;
            }
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
        // Every connection task has ended, so no stream task can start now.
        streams.tracker.close();
        streams.tracker.wait().await;
    })
    .await
    .is_ok();
    if !drained {
        while connections.try_join_next().is_some() {}
        let remaining = connections.len();
        let remaining_streams = streams.tracker.len();
        tracing::warn!(
            target: "ferrum_alloy::server",
            listener = options.name,
            remaining,
            remaining_streams,
            "drain budget exhausted; closing remaining connections"
        );
        stats
            .force_closed_connections
            .fetch_add(remaining as u64, Ordering::Relaxed);
        stats
            .force_closed_streams
            .fetch_add(remaining_streams as u64, Ordering::Relaxed);
        streams.cancel.cancel();
        // Aborting a task drops its connection and socket; `shutdown` returns
        // once every task has ended, including aborted tasks still unwinding.
        connections.shutdown().await;
        // A cancelled stream task drops its handler and response body the
        // next time it runs; `wait` returns once every one has done so.
        streams.tracker.close();
        streams.tracker.wait().await;
    }
    Ok(())
}

async fn handle(
    stream: TcpStream,
    remote: SocketAddr,
    builder: Builder<StreamExecutor>,
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
        serve_io(stream, peer, builder, app, options, lifecycle, stats).await;
        return;
    }
    let peer = PeerInfo {
        remote_addr: Some(remote),
        tls: None,
    };
    serve_io(stream, peer, builder, app, options, lifecycle, stats).await;
}

/// Request activity on one connection, shared by the connection task with
/// the request futures and response bodies it produces.
#[derive(Default)]
struct Activity {
    /// Requests received so far.
    started: AtomicU64,
    /// Requests whose response has not yet ended or been dropped.
    in_flight: AtomicUsize,
    /// Notified when `in_flight` drops to zero.
    idle: Notify,
}

impl Activity {
    fn started(&self) -> u64 {
        self.started.load(Ordering::Acquire)
    }

    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }
}

/// One request in flight, from the service call until its response body ends
/// or is dropped, or until the request future is dropped unfinished.
struct InFlight(Arc<Activity>);

impl InFlight {
    fn start(activity: &Arc<Activity>) -> Self {
        activity.in_flight.fetch_add(1, Ordering::AcqRel);
        activity.started.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(activity))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_one();
        }
    }
}

/// Counts the requests on a connection. Hyper calls a service on the
/// connection task as soon as it has parsed a request head, for both
/// protocols, before any stream task is spawned, so the count is up to date
/// whenever the connection task checks it.
struct TrackRequests<S> {
    inner: S,
    activity: Arc<Activity>,
}

impl<S, R, B> hyper::service::Service<R> for TrackRequests<S>
where
    S: hyper::service::Service<R, Response = http::Response<B>>,
{
    type Response = http::Response<TrackedBody<B>>;
    type Error = S::Error;
    type Future = TrackedResponse<S::Future>;

    fn call(&self, request: R) -> Self::Future {
        TrackedResponse {
            inner: self.inner.call(request),
            in_flight: Some(InFlight::start(&self.activity)),
        }
    }
}

pin_project! {
    /// A request future that hands its [`InFlight`] guard to the response
    /// body, or releases it if the request fails or is dropped.
    struct TrackedResponse<F> {
        #[pin]
        inner: F,
        in_flight: Option<InFlight>,
    }
}

impl<F, B, E> Future for TrackedResponse<F>
where
    F: Future<Output = Result<http::Response<B>, E>>,
{
    type Output = Result<http::Response<TrackedBody<B>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let response = ready!(this.inner.poll(cx))?;
        let in_flight = this.in_flight.take();
        Poll::Ready(Ok(response.map(|inner| TrackedBody { inner, in_flight })))
    }
}

pin_project! {
    /// A response body that keeps its request in flight until it ends or is
    /// dropped.
    struct TrackedBody<B> {
        #[pin]
        inner: B,
        in_flight: Option<InFlight>,
    }
}

impl<B> http_body::Body for TrackedBody<B>
where
    B: http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let frame = ready!(this.inner.poll_frame(cx));
        if frame.is_none() {
            // The response has ended, even if Hyper holds on to the body.
            *this.in_flight = None;
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Where a connection is in closing itself for inactivity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Serving. The deadline is the first-request or idle deadline.
    Open,
    /// Asked to close for inactivity. The deadline ends a short grace period.
    /// Holds the request count when the close began.
    Closing { started: u64 },
    /// A request raced the close. The deadline is the hard cap for it to
    /// finish.
    Finishing,
}

async fn serve_io<I>(
    io: I,
    peer: PeerInfo,
    builder: Builder<StreamExecutor>,
    app: Router,
    options: &ServeOptions,
    lifecycle: &Lifecycle,
    stats: &ServerStats,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let remote = peer.remote_addr;
    let activity = Arc::new(Activity::default());
    let service = app.map_request(move |request: http::Request<Incoming>| {
        let mut request = request.map(Body::new);
        request.extensions_mut().insert(peer.clone());
        if let Some(remote) = remote {
            request.extensions_mut().insert(ConnectInfo(remote));
        }
        request
    });
    let service = TrackRequests {
        inner: TowerToHyperService::new(service),
        activity: Arc::clone(&activity),
    };
    let connection = builder.serve_connection_with_upgrades(TokioIo::new(io), service);
    tokio::pin!(connection);
    // Protocol detection waits for enough bytes to rule out the HTTP/2
    // preface before either protocol starts, Hyper's HTTP/1 header timer
    // starts only after detection, and HTTP/2 has no request-head timer. This
    // deadline covers all of it. After the first request it becomes the idle
    // deadline: re-armed whenever the last request in flight ends, it closes
    // the connection once no request has been in flight for the idle
    // timeout. HTTP/2 keep-alive pings do not count as activity.
    let deadline = tokio::time::sleep(options.header_read_timeout);
    tokio::pin!(deadline);
    // The request count when the deadline was armed with no request in
    // flight, or `None` when it was armed with requests in flight.
    let mut idle_since = Some(0);
    let mut phase = Phase::Open;
    let mut draining = false;
    let result = loop {
        tokio::select! {
            result = connection.as_mut() => break result,
            () = activity.idle.notified(), if phase == Phase::Open => {
                if activity.in_flight() == 0 {
                    idle_since = Some(activity.started());
                    deadline
                        .as_mut()
                        .reset(Instant::now() + options.idle_timeout);
                }
            }
            () = deadline.as_mut() => match phase {
                Phase::Open => {
                    let started = activity.started();
                    let busy = activity.in_flight() > 0;
                    if busy || idle_since != Some(started) {
                        // Not idle for the whole period: look again later.
                        // The idle notification re-arms the deadline as soon
                        // as the last request in flight ends.
                        idle_since = (!busy).then_some(started);
                        deadline
                            .as_mut()
                            .reset(Instant::now() + options.idle_timeout);
                    } else {
                        if started == 0 {
                            stats.first_request_timeouts.fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "no request head within the header read timeout; closing");
                        } else {
                            stats.idle_timeouts.fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "no request in flight within the idle timeout; closing");
                        }
                        // During protocol detection or on an idle HTTP/1.1
                        // connection this ends the connection at once. An
                        // established HTTP/2 connection is sent `GOAWAY`
                        // first, so a request already on the wire is either
                        // served or refused in a way the client can safely
                        // retry. The grace period bounds the wait for the
                        // connection to end.
                        phase = Phase::Closing { started };
                        if !draining {
                            draining = true;
                            connection.as_mut().graceful_shutdown();
                        }
                        let grace = options.header_read_timeout.min(Duration::from_secs(1));
                        deadline.as_mut().reset(Instant::now() + grace);
                    }
                }
                Phase::Closing { started } => {
                    if activity.started() != started || activity.in_flight() > 0 {
                        // A request that raced the `GOAWAY` gets one bounded
                        // opportunity to finish. Hyper also keeps an HTTP/2
                        // connection alive while waiting for the peer to
                        // acknowledge its shutdown ping. Bound both by the
                        // same hard cap used for server draining.
                        phase = Phase::Finishing;
                        deadline
                            .as_mut()
                            .reset(Instant::now() + options.drain_timeout);
                    } else {
                        tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "connection still open after the close grace period; dropping");
                        return;
                    }
                }
                Phase::Finishing => {
                    tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "connection still open after the request drain budget; dropping");
                    return;
                }
            },
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
