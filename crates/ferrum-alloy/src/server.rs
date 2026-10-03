//! HTTP/1.1 and HTTP/2 serving with bounded resources and graceful drain.
//!
//! The accept loop enforces a connection limit, request-head limits and
//! read timeout, and hands each request the transport identity of its
//! connection ([`PeerInfo`], and axum's `ConnectInfo`). The header read
//! timeout also bounds the time from a ready connection (after any TLS
//! handshake) to its first request head, whatever the protocol, so a peer
//! that sends nothing or only part of the HTTP/2 preface cannot keep a
//! connection slot. After that, the idle timeout bounds the time a
//! connection may spend with no request in flight and no response data
//! written, so a peer that sends one cheap request and then only answers
//! HTTP/2 keep-alive pings cannot keep its slot either, while a slow reader
//! still taking the end of a response is not cut. A response the peer will
//! not take counts as in flight, and a connection whose transport cannot
//! take a write is never idle, so the write stall timeout bounds the time a
//! connection may go without writing any response data while a response
//! waits to be written: a peer that withholds HTTP/2 `WINDOW_UPDATE` or
//! keeps a zero TCP receive window cannot keep its slot. On HTTP/2, response
//! data waits from the moment a response body hands it to Hyper until Hyper
//! takes it for writing or drops it, including the part of a chunk that Hyper
//! holds beyond the peer's flow-control window while the body goes back to
//! waiting for the application. An established HTTP/2 connection is sent
//! `GOAWAY` at any of these deadlines, so a client can retry elsewhere, and is
//! closed shortly after.
//!
//! On shutdown it stops accepting, abandons unfinished TLS handshakes, asks
//! every connection to finish (HTTP/1.1 `Connection: close` after the current
//! response, HTTP/2 `GOAWAY`), waits up to the drain budget for in-flight
//! requests and response streams, then force-closes the rest. HTTP/2 stream
//! tasks, which run a request's handler and response body apart from their
//! connection task, are tracked by the drain as well and cancelled at the
//! budget. The listener returns only after every connection task and stream
//! task has ended, including aborted tasks that are still unwinding, so every
//! socket a connection task served is closed and no handler is running by
//! then.
//!
//! An upgraded connection (WebSocket) leaves Hyper after the `101` response,
//! but its slot stays with its socket: it counts against the connection limit
//! and in `active_connections` until the application drops it. The drain
//! waits for upgraded connections as well. Applications should close them
//! when [`crate::Lifecycle::shutdown_token`] is cancelled; those still open
//! at the drain budget are force-closed: every read and write on them fails
//! and up to 16 tasks waiting on them are woken, so an application that reads
//! or writes its session ends it, and the listener waits briefly for that.
//! Alloy cannot drop an upgraded connection for the application: one that the
//! application keeps without reading or writing it stays open after the
//! listener returns, until the application drops it.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::task::{Context, Poll, Wake, Waker, ready};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bytes::Buf;
use ferrum_alloy_telemetry::PeerInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tower::ServiceExt;

use crate::lifecycle::Lifecycle;

/// How long the listener waits, after the drain budget, for applications to
/// drop upgraded connections whose reads and writes now fail.
const UPGRADED_CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Connection-level counters.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ServerStats {
    /// Connections currently open, including upgraded (WebSocket)
    /// connections that the application has not dropped yet.
    pub active_connections: AtomicU64,
    /// Connections closed immediately because the limit was reached.
    pub rejected_connections: AtomicU64,
    /// TLS handshakes that failed or timed out.
    pub tls_handshake_failures: AtomicU64,
    /// TLS reloads that swapped in changed certificates, key, client CA
    /// bundle, or CRLs.
    pub tls_reloads: AtomicU64,
    /// TLS reloads whose files could not be read or failed validation, the
    /// same way as at the reload before; the previous material kept serving.
    pub tls_reload_failures: AtomicU64,
    /// Streaks of TLS reloads whose files changed between the two reads
    /// three reloads in a row, with no swap or unchanged result in between,
    /// counted once per streak; the previous material kept serving.
    pub tls_reload_stalls: AtomicU64,
    /// When the serving TLS certificate and client CRLs stop being valid.
    #[cfg(feature = "tls")]
    pub(crate) tls_expiry: std::sync::Mutex<crate::tls::Expiry>,
    /// Connections, upgraded ones included, force-closed after the drain
    /// budget.
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
    /// Connections closed because a response waiting to be written got no
    /// data written for the write stall timeout.
    pub write_stall_timeouts: AtomicU64,
}

impl ServerStats {
    /// Prometheus text for these counters.
    pub fn render_prometheus(&self, listener: &str) -> String {
        let mut out = String::new();
        for (name, kind, help, value) in [
            (
                "ferrum_alloy_active_connections",
                "gauge",
                "Open connections, including upgraded ones.",
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
                "ferrum_alloy_tls_reloads_total",
                "counter",
                "TLS reloads that swapped in changed certificates, key, client CAs, or CRLs.",
                &self.tls_reloads,
            ),
            (
                "ferrum_alloy_tls_reload_failures_total",
                "counter",
                "Repeated TLS reload failures; the previous material kept serving.",
                &self.tls_reload_failures,
            ),
            (
                "ferrum_alloy_tls_reload_stalls_total",
                "counter",
                "Streaks of TLS reloads stalled by files that kept changing between reads.",
                &self.tls_reload_stalls,
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
            (
                "ferrum_alloy_write_stall_timeouts_total",
                "counter",
                "Connections closed with a response unwritten for the write stall timeout.",
                &self.write_stall_timeouts,
            ),
        ] {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{{listener=\"{listener}\"}} {}\n",
                value.load(Ordering::Relaxed)
            ));
        }
        #[cfg(feature = "tls")]
        {
            let expiry = self
                .tls_expiry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            out.push_str(&expiry.render_prometheus(listener));
        }
        out
    }
}

/// Holds a connection slot, the `active_connections` gauge, and the drain's
/// count of open sockets for as long as the connection's socket is open. The
/// connection task holds it until its transport exists, then the transport
/// does, so that it stays with the socket when Hyper hands the socket over to
/// the application for an upgraded (WebSocket) connection. Dropping it, with
/// the socket or with an aborted connection task, releases all three, once.
struct ActiveConnection {
    stats: Arc<ServerStats>,
    /// Fails the transport once the listener force-closes its connections.
    close: ForceClose,
    _permit: OwnedSemaphorePermit,
    _socket: TaskTrackerToken,
}

impl ActiveConnection {
    fn open(
        stats: Arc<ServerStats>,
        close: ForceClose,
        permit: OwnedSemaphorePermit,
        socket: TaskTrackerToken,
    ) -> Self {
        stats.active_connections.fetch_add(1, Ordering::Relaxed);
        Self {
            stats,
            close,
            _permit: permit,
            _socket: socket,
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
    pub(crate) write_stall_timeout: Duration,
    pub(crate) drain_timeout: Duration,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<crate::tls::TlsServer>,
}

/// The HTTP/2 stream tasks of one listener, and the signal that force-closes
/// its connections. Hyper spawns one stream task per request: it runs the
/// handler and then sends the response body, apart from the connection task.
/// The drain waits for them; at the budget it cancels them and fails every
/// read and write on the listener's connections, upgraded ones included.
#[derive(Default)]
struct Streams {
    tracker: TaskTracker,
    token: CancellationToken,
    cancelled: Arc<AtomicBool>,
}

impl Streams {
    /// The force-close signal of one connection, shared by its transport and
    /// its stream tasks. It gets a child token, so tasks on different
    /// connections never share the lock behind a token or its wake-up list.
    fn connection(&self) -> ForceClose {
        ForceClose {
            token: self.token.child_token(),
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    /// An executor for the stream tasks of the connection `close` belongs to.
    fn executor(&self, close: &ForceClose) -> StreamExecutor {
        StreamExecutor {
            tracker: self.tracker.clone(),
            close: close.clone(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.token.cancel();
    }
}

/// Tells one connection's transport and stream tasks that the listener has
/// force-closed its connections at the end of the drain budget.
#[derive(Clone)]
struct ForceClose {
    token: CancellationToken,
    cancelled: Arc<AtomicBool>,
}

impl ForceClose {
    /// Whether the listener has force-closed its connections. Takes no lock.
    fn is_set(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn waiter(&self) -> CloseWaiter {
        CloseWaiter {
            wake: self.token.clone().cancelled_owned(),
            cancelled: Arc::clone(&self.cancelled),
            registered: None,
        }
    }
}

pin_project! {
    /// Wakes the task that polls it when its listener force-closes its
    /// connections. It registers for the cancellation wake-up only on its
    /// first poll or when its waker changes, and otherwise checks an atomic
    /// flag, so a busy task takes no lock.
    struct CloseWaiter {
        #[pin]
        wake: WaitForCancellationFutureOwned,
        cancelled: Arc<AtomicBool>,
        registered: Option<Waker>,
    }
}

impl CloseWaiter {
    /// `true` once the listener has force-closed its connections; otherwise
    /// the task of `cx` is woken when it does.
    fn poll_closed(self: Pin<&mut Self>, cx: &mut Context<'_>) -> bool {
        let this = self.project();
        // The flag is set before the token is cancelled, so the wake-up that
        // cancellation sends always finds it set.
        if this.cancelled.load(Ordering::Acquire) {
            return true;
        }
        let registered = matches!(this.registered, Some(waker) if waker.will_wake(cx.waker()));
        if !registered {
            if this.wake.poll(cx).is_ready() {
                return true;
            }
            *this.registered = Some(cx.waker().clone());
        }
        false
    }
}

/// Spawns the stream tasks of one connection on its listener's [`Streams`].
#[derive(Clone)]
struct StreamExecutor {
    tracker: TaskTracker,
    close: ForceClose,
}

impl<F> hyper::rt::Executor<F> for StreamExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        self.tracker.spawn(Cancellable {
            future,
            closed: self.close.waiter(),
        });
    }
}

pin_project! {
    /// A stream task that ends once its listener cancels its streams, which
    /// drops the handler and response body like aborting the task would.
    struct Cancellable<F> {
        #[pin]
        future: F,
        #[pin]
        closed: CloseWaiter,
    }
}

impl<F: Future> Future for Cancellable<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project();
        if this.closed.poll_closed(cx) {
            return Poll::Ready(());
        }
        this.future.poll(cx).map(|_| ())
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
/// stream task has ended, and once every upgraded connection has been
/// dropped or `UPGRADED_CLOSE_GRACE` has passed after the budget: connections
/// still open when the budget runs out are aborted, or for upgraded ones
/// failed, and counted in `force_closed_connections`, and stream tasks still
/// running are cancelled and counted in `force_closed_streams`. An upgraded
/// connection that the application keeps without reading or writing it is
/// still open when this returns.
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    options: ServeOptions,
    lifecycle: Lifecycle,
    stats: Arc<ServerStats>,
) -> io::Result<()> {
    let streams = Streams::default();
    // Every open connection socket, whether its connection task still serves
    // it or Hyper has handed it over to the application for an upgrade.
    let sockets = TaskTracker::new();
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
        let close = streams.connection();
        let executor = streams.executor(&close);
        let socket = sockets.token();
        let app = app.clone();
        let lifecycle = lifecycle.clone();
        let stats = Arc::clone(&stats);
        let options = options.clone();
        connections.spawn(async move {
            let active = ActiveConnection::open(stats, close, permit, socket);
            let builder = builder(&options, executor);
            handle(stream, remote, active, builder, app, &options, &lifecycle).await;
        });
    }
    drop(listener);
    lifecycle.drain_connections().cancel();
    let drained = tokio::time::timeout(options.drain_timeout, async {
        while connections.join_next().await.is_some() {}
        // Every connection task has ended, so no stream task can start now,
        // and the sockets still open are upgraded connections.
        streams.tracker.close();
        streams.tracker.wait().await;
        sockets.close();
        sockets.wait().await;
    })
    .await
    .is_ok();
    if !drained {
        // Every open socket holds one token, in its connection task or in an
        // upgraded connection, so this counts each connection still open at
        // the budget once.
        let remaining = sockets.len();
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
        // This also fails every read and write on the listener's transports
        // and wakes the tasks waiting on them, including the application
        // tasks that own upgraded connections.
        streams.cancel();
        // Aborting a task drops its connection and socket; `shutdown` returns
        // once every task has ended, including aborted tasks still unwinding.
        connections.shutdown().await;
        // A cancelled stream task drops its handler and response body the
        // next time it runs; `wait` returns once every one has done so.
        streams.tracker.close();
        streams.tracker.wait().await;
        // An upgraded socket closes when the application drops it, which an
        // application reading or writing it does once that fails. Alloy
        // cannot drop it for the application, so the wait is bounded: one
        // the application keeps without reading or writing stays open.
        sockets.close();
        if tokio::time::timeout(UPGRADED_CLOSE_GRACE, sockets.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                target: "ferrum_alloy::server",
                listener = options.name,
                remaining_upgraded = sockets.len(),
                "upgraded connections still held by the application; their reads and writes fail"
            );
        }
    }
    Ok(())
}

async fn handle(
    stream: TcpStream,
    remote: SocketAddr,
    active: ActiveConnection,
    builder: Builder<StreamExecutor>,
    app: Router,
    options: &ServeOptions,
    lifecycle: &Lifecycle,
) {
    #[cfg(feature = "tls")]
    if let Some(tls) = &options.tls {
        let accept = tokio::time::timeout(tls.handshake_timeout, tls.acceptor().accept(stream));
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
                active
                    .stats
                    .tls_handshake_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(target: "ferrum_alloy::tls", %remote, %error, "TLS handshake failed");
                return;
            }
            Err(_) => {
                active
                    .stats
                    .tls_handshake_failures
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(target: "ferrum_alloy::tls", %remote, "TLS handshake timed out");
                return;
            }
        };
        let peer = PeerInfo {
            remote_addr: Some(remote),
            tls: crate::tls::peer_identity(stream.get_ref().1),
        };
        serve_io(stream, peer, active, builder, app, options, lifecycle).await;
        return;
    }
    let peer = PeerInfo {
        remote_addr: Some(remote),
        tls: None,
    };
    serve_io(stream, peer, active, builder, app, options, lifecycle).await;
}

/// Request activity on one connection, shared by the connection task with
/// its transport and the request futures and response bodies it produces.
#[derive(Default)]
struct Activity {
    /// Requests received so far.
    started: AtomicU64,
    /// Requests whose response has not yet ended or been dropped.
    in_flight: AtomicUsize,
    /// Notified when `in_flight` drops to zero.
    idle: Notify,
    /// Responses whose body produced data that the connection has not come
    /// back for, because it cannot send that data yet.
    unsent: AtomicUsize,
    /// Response data bytes written to the transport so far.
    written: AtomicU64,
    /// Whether the last write to the transport could not complete.
    write_blocked: AtomicBool,
    /// HTTP/2 response data that bodies have handed to Hyper and that Hyper
    /// has neither taken for writing nor dropped (see [`HeldData`]). Hyper
    /// takes a whole chunk as soon as the peer's flow-control window has
    /// room for one byte of it, holds what does not fit, and goes back to the
    /// body, which may then wait for the application: only this count still
    /// shows the held data.
    held: AtomicU64,
}

impl Activity {
    // The counts are only compared, never used to publish other data, and
    // `idle` synchronizes its own wake-up, so relaxed ordering is enough.
    fn started(&self) -> u64 {
        self.started.load(Ordering::Relaxed)
    }

    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Whether the last write to the transport could not complete: data is
    /// on its way to a peer that takes none of it for now.
    fn write_blocked(&self) -> bool {
        self.write_blocked.load(Ordering::Relaxed)
    }

    /// Whether response data is waiting for the transport: the transport
    /// cannot take a write, a response body has produced data that the
    /// connection cannot send yet, or Hyper holds HTTP/2 data that a response
    /// body handed over (see `held`).
    fn response_pending(&self) -> bool {
        self.write_blocked()
            || self.unsent.load(Ordering::Relaxed) > 0
            || self.held.load(Ordering::Relaxed) > 0
    }
}

/// One request in flight, from the service call until its response body ends
/// or is dropped, or until the request future is dropped unfinished.
struct InFlight {
    activity: Arc<Activity>,
    /// Whether this response is counted in `unsent`.
    unsent: bool,
    /// Whether the response goes out on HTTP/2, whose flow control lets Hyper
    /// hold data that the transport has not written.
    http2: bool,
}

impl InFlight {
    fn start(activity: &Arc<Activity>, http2: bool) -> Self {
        activity.in_flight.fetch_add(1, Ordering::Relaxed);
        activity.started.fetch_add(1, Ordering::Relaxed);
        Self {
            activity: Arc::clone(activity),
            unsent: false,
            http2,
        }
    }

    /// Records whether the response body last produced data (`true`) or is
    /// waiting for the application or has ended (`false`). The shared count
    /// changes only when this does, so a body that always has data ready
    /// touches it once.
    fn set_unsent(&mut self, unsent: bool) {
        if self.unsent != unsent {
            self.unsent = unsent;
            if unsent {
                self.activity.unsent.fetch_add(1, Ordering::Relaxed);
            } else {
                self.activity.unsent.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Where data this response hands to Hyper counts as held: only on
    /// HTTP/2.
    fn held_in(&self) -> Option<Arc<Activity>> {
        self.http2.then(|| Arc::clone(&self.activity))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.set_unsent(false);
        if self.activity.in_flight.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.activity.idle.notify_one();
        }
    }
}

/// A chunk of response data handed to Hyper, counted in its connection's
/// `held` until Hyper has taken it for writing or dropped it. Hyper and h2
/// advance a chunk as they write it, keep what flow control does not let
/// through, and drop it when its stream is reset or its connection closes,
/// so the count is exact for every stream, whatever the others do. h2
/// copies small frames into its write buffer before the transport writes
/// them; the transport's own blocked state covers that buffer.
struct HeldData<D> {
    inner: D,
    /// The connection's activity on HTTP/2, `None` otherwise.
    activity: Option<Arc<Activity>>,
    /// The bytes of this chunk still counted in `held`.
    held: u64,
}

impl<D: Buf> HeldData<D> {
    fn new(inner: D, activity: Option<Arc<Activity>>) -> Self {
        let mut held = 0;
        if let Some(activity) = &activity {
            held = inner.remaining() as u64;
            activity.held.fetch_add(held, Ordering::Relaxed);
        }
        Self {
            inner,
            activity,
            held,
        }
    }

    /// Takes up to `len` bytes of this chunk off `held`.
    fn release(&mut self, len: u64) {
        let len = len.min(self.held);
        if let Some(activity) = &self.activity
            && len > 0
        {
            self.held -= len;
            activity.held.fetch_sub(len, Ordering::Relaxed);
        }
    }
}

impl<D: Buf> Buf for HeldData<D> {
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }

    fn chunk(&self) -> &[u8] {
        self.inner.chunk()
    }

    fn chunks_vectored<'a>(&'a self, dst: &mut [io::IoSlice<'a>]) -> usize {
        self.inner.chunks_vectored(dst)
    }

    fn advance(&mut self, cnt: usize) {
        self.inner.advance(cnt);
        self.release(cnt as u64);
    }

    fn copy_to_bytes(&mut self, len: usize) -> bytes::Bytes {
        let bytes = self.inner.copy_to_bytes(len);
        self.release(len as u64);
        bytes
    }
}

impl<D> Drop for HeldData<D> {
    fn drop(&mut self) {
        if let Some(activity) = &self.activity
            && self.held > 0
        {
            activity.held.fetch_sub(self.held, Ordering::Relaxed);
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

impl<S, R, B> hyper::service::Service<http::Request<R>> for TrackRequests<S>
where
    S: hyper::service::Service<http::Request<R>, Response = http::Response<B>>,
{
    type Response = http::Response<TrackedBody<B>>;
    type Error = S::Error;
    type Future = TrackedResponse<S::Future>;

    fn call(&self, request: http::Request<R>) -> Self::Future {
        let http2 = request.version() == http::Version::HTTP_2;
        TrackedResponse {
            inner: self.inner.call(request),
            in_flight: Some(InFlight::start(&self.activity, http2)),
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
    /// dropped. Between producing data and being polled again, it counts as
    /// unsent: Hyper polls a body again only once it can take more data. On
    /// HTTP/2, each chunk it produces also counts as held (see [`HeldData`]).
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
    type Data = HeldData<B::Data>;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let frame = this.inner.poll_frame(cx);
        match &frame {
            // The response has ended, even if Hyper holds on to the body.
            Poll::Ready(None) => *this.in_flight = None,
            frame => {
                if let Some(in_flight) = this.in_flight.as_mut() {
                    in_flight.set_unsent(matches!(frame, Poll::Ready(Some(Ok(_)))));
                }
            }
        }
        let held_in = this.in_flight.as_ref().and_then(InFlight::held_in);
        let hold = move |frame: http_body::Frame<B::Data>| {
            frame.map_data(|data| HeldData::new(data, held_in))
        };
        frame.map(|frame| frame.map(|frame| frame.map(hold)))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// The length of an HTTP/2 frame header.
const FRAME_HEADER_LEN: usize = 9;
/// The HTTP/2 DATA frame type.
const DATA_FRAME: u8 = 0x0;

/// What a connection writes, as far as response progress is concerned.
enum Output {
    /// Nothing written yet.
    Unknown,
    /// HTTP/1: every byte written belongs to a response.
    Http1,
    /// HTTP/2: only DATA frame payloads are response data. Other frames, such
    /// as PING and SETTINGS acknowledgements, are written whenever the peer
    /// asks for them, so they are not progress.
    Http2(Frames),
}

impl Output {
    /// The response data bytes in `buf`, the next bytes written.
    fn response_bytes(&mut self, buf: &[u8]) -> u64 {
        if let Output::Unknown = self {
            // An HTTP/1 connection starts with a status line, `HTTP/1.x`. An
            // HTTP/2 server starts with a SETTINGS frame, too short for the
            // first byte of its length to be `H`.
            *self = match buf.first() {
                None => return 0,
                Some(b'H') => Output::Http1,
                Some(_) => Output::Http2(Frames::default()),
            };
        }
        match self {
            Output::Unknown => 0,
            Output::Http1 => buf.len() as u64,
            Output::Http2(frames) => frames.data_bytes(buf),
        }
    }
}

/// Follows HTTP/2 frame boundaries through the bytes a connection writes.
#[derive(Default)]
struct Frames {
    /// The part of the next frame header written so far.
    header: [u8; FRAME_HEADER_LEN],
    header_len: usize,
    /// Payload bytes of the current frame not yet written.
    payload_left: usize,
    /// Whether the current frame is a DATA frame.
    data: bool,
}

impl Frames {
    /// The DATA payload bytes in `buf`, the next bytes written.
    fn data_bytes(&mut self, mut buf: &[u8]) -> u64 {
        let mut data = 0;
        while !buf.is_empty() {
            if self.payload_left > 0 {
                let (payload, rest) = buf.split_at(self.payload_left.min(buf.len()));
                self.payload_left -= payload.len();
                if self.data {
                    data += payload.len() as u64;
                }
                buf = rest;
                continue;
            }
            let wanted = FRAME_HEADER_LEN - self.header_len;
            let (part, rest) = buf.split_at(wanted.min(buf.len()));
            let end = self.header_len + part.len();
            self.header[self.header_len..end].copy_from_slice(part);
            self.header_len = end;
            buf = rest;
            if self.header_len == FRAME_HEADER_LEN {
                let [a, b, c, kind, ..] = self.header;
                self.payload_left = u32::from_be_bytes([0, a, b, c]) as usize;
                self.data = kind == DATA_FRAME;
                self.header_len = 0;
            }
        }
        data
    }
}

/// The transport of one connection, which records response progress for the
/// write stall timeout: whether the last write could not complete, and how
/// many response data bytes have been written. The connection task uses it
/// and reads what it records. After an upgrade (WebSocket) the application
/// owns it, and with it the connection's slot.
///
/// Once the listener force-closes its connections at the end of the drain
/// budget, every read and write fails, and the tasks waiting on one are
/// woken so that they fail too (see [`CloseWatch`]). Checking for that is an
/// atomic load; a task other than the connection task takes this
/// connection's own lock only when it waits in a direction where another
/// task waited last.
struct Transport<I> {
    io: I,
    activity: Arc<Activity>,
    output: Output,
    response_written: u64,
    blocked: bool,
    /// The connection's slot, released when the socket is dropped.
    active: ActiveConnection,
    watch: CloseWatch,
}

/// The error of every read and write on a force-closed transport.
fn force_closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "connection force-closed at the end of the drain budget",
    )
}

/// Wakes the tasks waiting to read or write on one transport when the
/// listener force-closes its connections: up to [`WakeAll::MAX_TASKS`] of
/// them, not only the last one to wait, so that tasks sharing an upgraded
/// connection all fail.
///
/// The connection task is left out: the drain aborts it at the budget, so it
/// needs no wake-up, and leaving it out takes no lock while it serves and
/// keeps its allocation from outliving it in an upgraded connection.
struct CloseWatch {
    /// The connection task's waker.
    owner: Waker,
    /// Made the first time another task waits.
    waiters: Option<Waiters>,
    /// The waker each direction last saw, so that a task waiting again takes
    /// no lock.
    read: Option<Waker>,
    write: Option<Waker>,
}

impl CloseWatch {
    fn new(owner: Waker) -> Self {
        Self {
            owner,
            waiters: None,
            read: None,
            write: None,
        }
    }

    /// `result` of a read, or a force-close error (see [`Self::unless_closed`]).
    fn reading<T>(
        &mut self,
        close: &ForceClose,
        result: Poll<io::Result<T>>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<T>> {
        self.unless_closed(close, result, cx, false)
    }

    /// `result` of a write, flush, or shutdown, or a force-close error (see
    /// [`Self::unless_closed`]).
    fn writing<T>(
        &mut self,
        close: &ForceClose,
        result: Poll<io::Result<T>>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<T>> {
        self.unless_closed(close, result, cx, true)
    }

    /// `result`, or a force-close error if it is pending and the listener has
    /// force-closed the connection. Otherwise the task of `cx`, unless it is
    /// the connection task, is added to the waiters, which wake it when the
    /// listener does, so that its next attempt fails.
    fn unless_closed<T>(
        &mut self,
        close: &ForceClose,
        result: Poll<io::Result<T>>,
        cx: &mut Context<'_>,
        writing: bool,
    ) -> Poll<io::Result<T>> {
        if result.is_pending() && self.closed_while_waiting(close, cx.waker(), writing) {
            return Poll::Ready(Err(force_closed()));
        }
        result
    }

    /// `true` once the listener has force-closed its connections; otherwise
    /// the task of `waker` will be woken when it does, if it needs to be.
    fn closed_while_waiting(&mut self, close: &ForceClose, waker: &Waker, writing: bool) -> bool {
        let last = if writing {
            &mut self.write
        } else {
            &mut self.read
        };
        if matches!(last, Some(known) if known.will_wake(waker)) {
            return close.is_set();
        }
        *last = Some(waker.clone());
        if self.owner.will_wake(waker) {
            return close.is_set();
        }
        let waiters = self.waiters.get_or_insert_with(|| Waiters::new(close));
        if waiters.tasks.add(waker) {
            // The forgotten task may be the one the other direction saw last:
            // make that task add itself again the next time it waits.
            if writing {
                self.read = None;
            } else {
                self.write = None;
            }
        }
        waiters.poll_closed()
    }
}

/// The tasks waiting on one transport, and the single registration that
/// wakes them all when the listener force-closes its connections.
struct Waiters {
    tasks: Arc<WakeAll>,
    closed: Pin<Box<CloseWaiter>>,
}

impl Waiters {
    fn new(close: &ForceClose) -> Self {
        Self {
            tasks: Arc::default(),
            closed: Box::pin(close.waiter()),
        }
    }

    /// `true` once the listener has force-closed its connections. Registers
    /// [`WakeAll`] for the wake-up the first time; its waker never changes,
    /// so later calls only check the flag.
    fn poll_closed(&mut self) -> bool {
        let waker = Waker::from(Arc::clone(&self.tasks));
        let mut cx = Context::from_waker(&waker);
        self.closed.as_mut().poll_closed(&mut cx)
    }
}

/// The wakers of the tasks waiting on one transport. Waking it wakes them
/// all, once.
#[derive(Default)]
struct WakeAll(std::sync::Mutex<Vec<Waker>>);

impl WakeAll {
    /// How many distinct tasks it remembers; beyond that the oldest is
    /// forgotten.
    const MAX_TASKS: usize = 16;

    /// Adds the task of `waker`, unless it is already there. Returns whether
    /// the oldest task was forgotten to make room.
    fn add(&self, waker: &Waker) -> bool {
        let mut wakers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if wakers.iter().any(|known| known.will_wake(waker)) {
            return false;
        }
        let full = wakers.len() == Self::MAX_TASKS;
        if full {
            wakers.remove(0);
        }
        wakers.push(waker.clone());
        full
    }
}

impl Wake for WakeAll {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let wakers = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        for waker in wakers {
            waker.wake();
        }
    }
}

impl<I> Transport<I> {
    /// A transport for `io`, served by the connection task whose waker is
    /// `owner`.
    fn new(io: I, activity: Arc<Activity>, active: ActiveConnection, owner: Waker) -> Self {
        Self {
            io,
            activity,
            output: Output::Unknown,
            response_written: 0,
            blocked: false,
            active,
            watch: CloseWatch::new(owner),
        }
    }

    /// Whether the listener has force-closed the connection.
    fn closed(&self) -> bool {
        self.active.close.is_set()
    }

    /// Records whether a write, flush, or shutdown could not complete. Only
    /// a change reaches the shared state.
    fn set_blocked(&mut self, blocked: bool) {
        if self.blocked != blocked {
            self.blocked = blocked;
            self.activity
                .write_blocked
                .store(blocked, Ordering::Relaxed);
        }
    }

    /// Records the outcome of writing `bufs`, of which the first `len` bytes
    /// were written if it completed.
    fn record_write<'a>(
        &mut self,
        result: &Poll<io::Result<usize>>,
        bufs: impl IntoIterator<Item = &'a [u8]>,
    ) {
        self.set_blocked(result.is_pending());
        let Poll::Ready(Ok(mut len)) = *result else {
            return;
        };
        let mut data = 0;
        for buf in bufs {
            if len == 0 {
                break;
            }
            let (sent, _) = buf.split_at(len.min(buf.len()));
            len -= sent.len();
            data += self.output.response_bytes(sent);
        }
        if data > 0 {
            self.response_written += data;
            self.activity
                .written
                .store(self.response_written, Ordering::Relaxed);
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for Transport<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.closed() {
            return Poll::Ready(Err(force_closed()));
        }
        let result = Pin::new(&mut this.io).poll_read(cx, buf);
        this.watch.reading(&this.active.close, result, cx)
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Transport<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.closed() {
            return Poll::Ready(Err(force_closed()));
        }
        let result = Pin::new(&mut this.io).poll_write(cx, buf);
        this.record_write(&result, [buf]);
        this.watch.writing(&this.active.close, result, cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.closed() {
            return Poll::Ready(Err(force_closed()));
        }
        let result = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.record_write(&result, bufs.iter().map(|buf| &**buf));
        this.watch.writing(&this.active.close, result, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.closed() {
            return Poll::Ready(Err(force_closed()));
        }
        let result = Pin::new(&mut this.io).poll_flush(cx);
        this.set_blocked(result.is_pending());
        this.watch.writing(&this.active.close, result, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.closed() {
            return Poll::Ready(Err(force_closed()));
        }
        let result = Pin::new(&mut this.io).poll_shutdown(cx);
        this.set_blocked(result.is_pending());
        this.watch.writing(&this.active.close, result, cx)
    }
}

/// Where a connection is in closing itself for inactivity or a write stall.
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
    /// Asked to close because a response could not be written. The deadline
    /// ends a short grace period; the stalled response will not finish.
    Stalled,
}

async fn serve_io<I>(
    io: I,
    peer: PeerInfo,
    active: ActiveConnection,
    builder: Builder<StreamExecutor>,
    app: Router,
    options: &ServeOptions,
    lifecycle: &Lifecycle,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let stats = Arc::clone(&active.stats);
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
    // The transport holds the connection's slot from here on, so that the
    // slot stays with the socket if Hyper hands it over for an upgrade.
    let owner = std::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
    let io = TokioIo::new(Transport::new(io, Arc::clone(&activity), active, owner));
    let connection = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(connection);
    // Protocol detection waits for enough bytes to rule out the HTTP/2
    // preface before either protocol starts, Hyper's HTTP/1 header timer
    // starts only after detection, and HTTP/2 has no request-head timer. This
    // deadline covers all of it. After the first request it becomes the idle
    // deadline: re-armed whenever the last request in flight ends, it closes
    // the connection once no request has been in flight and no response data
    // has been written for the idle timeout. Hyper lets go of a response body
    // as soon as it has taken the last chunk, which can still be on its way
    // to a slow reader, so the idle time counts from the last response byte
    // written, and a connection whose transport cannot take a write is never
    // idle: the peer is not taking data that is on its way to it, which is
    // for the write stall timeout to bound. On HTTP/1 the whole of a large
    // response can be left in Hyper's write buffer when its body ends, and a
    // reader that keeps taking it is not cut by a pause on its side or in
    // the network. HTTP/2 keep-alive pings do not count as activity.
    let deadline = tokio::time::sleep(options.header_read_timeout);
    tokio::pin!(deadline);
    // The write stall check samples response progress every half period. It
    // closes the connection once response data has been waiting for the
    // transport at three checks in a row with none written in between: at
    // least the write stall timeout, and at most one and a half times it,
    // after the last response data was written. Keep-alive pings and other
    // control frames are not response data.
    let stall_check = options.write_stall_timeout / 2;
    let stall_timer = tokio::time::sleep(stall_check);
    tokio::pin!(stall_timer);
    // The response data written when the check last saw none waiting or some
    // written, and how many checks since have seen it waiting with none
    // written. `None` while no response data is waiting.
    let mut stalled: Option<(u64, u32)> = None;
    let grace = options.header_read_timeout.min(Duration::from_secs(1));
    // The request count when the deadline was armed with no request in
    // flight, or `None` when it was armed with requests in flight, and the
    // response data written by then.
    let mut idle_since = Some(0);
    let mut idle_written = 0;
    let mut phase = Phase::Open;
    let mut draining = false;
    let result = loop {
        tokio::select! {
            result = connection.as_mut() => break result,
            () = activity.idle.notified(), if phase == Phase::Open => {
                if activity.in_flight() == 0 {
                    idle_since = Some(activity.started());
                    idle_written = activity.written();
                    deadline
                        .as_mut()
                        .reset(Instant::now() + options.idle_timeout);
                }
            }
            () = deadline.as_mut() => match phase {
                Phase::Open => {
                    let started = activity.started();
                    let busy = activity.in_flight() > 0;
                    let written = activity.written();
                    // Before the first request this is the header read
                    // deadline, which nothing defers.
                    let sending = started > 0 && activity.write_blocked();
                    if busy || sending || idle_since != Some(started) || written != idle_written {
                        // Not idle for the whole period: look again later.
                        // The idle notification re-arms the deadline as soon
                        // as the last request in flight ends. A blocked write
                        // is left to the write stall check, which counts it
                        // as response data waiting.
                        idle_since = (!busy).then_some(started);
                        idle_written = written;
                        deadline
                            .as_mut()
                            .reset(Instant::now() + options.idle_timeout);
                    } else {
                        if started == 0 {
                            stats.first_request_timeouts.fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "no request head within the header read timeout; closing");
                        } else if !draining {
                            // Only an idle close is counted; a connection
                            // that shutdown already asked to finish is not.
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
                Phase::Stalled => {
                    tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "connection still open after the write stall grace period; dropping");
                    return;
                }
            },
            () = stall_timer.as_mut(), if phase == Phase::Open => {
                let written = activity.written();
                stalled = match stalled {
                    _ if !activity.response_pending() => None,
                    Some((mark, checks)) if mark == written => Some((mark, checks + 1)),
                    _ => Some((written, 0)),
                };
                if matches!(stalled, Some((_, checks)) if checks >= 2) {
                    stats.write_stall_timeouts.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(target: "ferrum_alloy::server", listener = options.name, ?remote, "no response data written within the write stall timeout; closing");
                    // An HTTP/2 peer is sent `GOAWAY` first, if the transport
                    // still takes it. The stalled response cannot finish, so
                    // the connection is dropped after the grace period
                    // whatever else is in flight on it.
                    phase = Phase::Stalled;
                    if !draining {
                        draining = true;
                        connection.as_mut().graceful_shutdown();
                    }
                    deadline.as_mut().reset(Instant::now() + grace);
                } else {
                    stall_timer.as_mut().reset(Instant::now() + stall_check);
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
