//! Response completion and HTTP/2 control backpressure over bounded IO.
//! Socket buffer options do not guarantee how much a platform accepts in
//! one write; a duplex transport
//! gives these regressions an exact capacity on every hosted test platform.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::future::Future;

use axum::routing::get;
use bytes::Bytes;
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

use super::*;

const CAPACITY: usize = 1024;
const BIG_BODY: usize = 64 * CAPACITY;
const IDLE_TIMEOUT: Duration = Duration::from_millis(100);
const WRITE_STALL_TIMEOUT: Duration = Duration::from_millis(400);
const WITHIN: Duration = Duration::from_secs(5);

/// One final frame, with notification only after Hyper drops the body that
/// yielded it. Its request guard is also released before the task yields.
struct FinishedBody {
    data: Option<Bytes>,
    finished: Arc<Notify>,
}

impl http_body::Body for FinishedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let frame = self.get_mut().data.take().map(Frame::data);
        Poll::Ready(frame.map(Ok))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        let len = self.data.as_ref().map_or(0, |data| data.len() as u64);
        SizeHint::with_exact(len)
    }
}

impl Drop for FinishedBody {
    fn drop(&mut self) {
        if self.data.is_none() {
            self.finished.notify_one();
        }
    }
}

/// Holds the request guard after yielding its final data until the test has
/// observed that data at the client transport.
struct GuardedFinishedBody {
    data: Option<Bytes>,
    release: tokio::sync::oneshot::Receiver<()>,
    released: bool,
    finished: Arc<Notify>,
}

impl http_body::Body for GuardedFinishedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.released {
            return Poll::Ready(None);
        }
        if let Some(data) = this.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        match Pin::new(&mut this.release).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => {
                this.released = true;
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.released
    }

    fn size_hint(&self) -> SizeHint {
        let len = self.data.as_ref().map_or(0, |data| data.len() as u64);
        SizeHint::with_exact(len)
    }
}

impl Drop for GuardedFinishedBody {
    fn drop(&mut self) {
        if self.released {
            self.finished.notify_one();
        }
    }
}

struct TestConnection {
    client: DuplexStream,
    received: Vec<u8>,
    stats: Arc<ServerStats>,
    slots: Arc<Semaphore>,
    task: JoinHandle<()>,
}

/// Starts the real connection loop and waits for Hyper to release its final
/// body frame. For BIG_BODY, CAPACITY cannot hold the rest of that frame, so
/// the server must still have response bytes blocked in the transport.
async fn finished_connection(body_size: usize, write_stall_timeout: Duration) -> TestConnection {
    let options = ServeOptions {
        name: "app",
        max_connections: 1,
        max_header_count: 100,
        max_header_bytes: 8192,
        http2_max_concurrent_streams: 1,
        // Keep Hyper's next-request timer later than either tested timeout.
        header_read_timeout: WITHIN,
        idle_timeout: IDLE_TIMEOUT,
        write_stall_timeout,
        drain_timeout: WITHIN,
        #[cfg(feature = "tls")]
        tls: None,
    };
    let stats = Arc::new(ServerStats::default());
    let slots = Arc::new(Semaphore::new(1));
    let sockets = TaskTracker::new();
    let streams = Streams::default();
    let close = streams.connection();
    let builder = builder(&options, streams.executor(&close));
    let active = ActiveConnection::open(
        Arc::clone(&stats),
        close,
        Arc::clone(&slots).acquire_owned().await.unwrap(),
        sockets.token(),
    );
    let finished = Arc::new(Notify::new());
    let body_finished = Arc::clone(&finished);
    let routes = Router::new().route(
        "/big",
        get(move || {
            let body = FinishedBody {
                data: Some(Bytes::from(vec![b'x'; body_size])),
                finished: Arc::clone(&body_finished),
            };
            async move { Body::new(body) }
        }),
    );
    let lifecycle = Lifecycle::new(Arc::default());
    let peer = PeerInfo {
        remote_addr: Some(SocketAddr::from(([127, 0, 0, 1], 12345))),
        tls: None,
    };
    let (mut client, server) = tokio::io::duplex(CAPACITY);
    let task = tokio::spawn(async move {
        serve_io(server, peer, active, builder, routes, &options, &lifecycle).await;
    });
    client
        .write_all(b"GET /big HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();
    let mut head = [0u8; 64];
    let n = tokio::time::timeout(WITHIN, client.read(&mut head))
        .await
        .expect("the response started")
        .unwrap();
    assert!(head[..n].starts_with(b"HTTP/1.1 200"));
    tokio::time::timeout(WITHIN, finished.notified())
        .await
        .expect("Hyper released the finished response body");
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 0);
    TestConnection {
        client,
        received: head[..n].to_vec(),
        stats,
        slots,
        task,
    }
}

fn response_body(received: &[u8], body_size: usize) -> &[u8] {
    let end = received
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the complete response head arrived");
    let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"));
    assert!(
        head.lines()
            .any(|line| line == format!("content-length: {body_size}"))
    );
    let body = &received[end + 4..];
    assert!(body.iter().all(|&byte| byte == b'x'));
    body
}

#[tokio::test]
async fn a_finished_http1_body_blocked_in_the_transport_outlives_idle() {
    let mut connection = finished_connection(BIG_BODY, WITHIN).await;
    // The body has ended, but the transport cannot hold its remaining data.
    tokio::time::sleep(IDLE_TIMEOUT * 3).await;
    assert_eq!(connection.stats.idle_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(
        connection
            .stats
            .write_stall_timeouts
            .load(Ordering::Relaxed),
        0
    );
    let receive = async {
        let mut buf = [0u8; CAPACITY];
        loop {
            if let Some(end) = connection
                .received
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                && connection.received.len() >= end + 4 + BIG_BODY
            {
                break;
            }
            let n = connection.client.read(&mut buf).await.unwrap();
            assert!(n > 0, "the paused reader's response must not be truncated");
            connection.received.extend_from_slice(&buf[..n]);
        }
    };
    tokio::time::timeout(WITHIN, receive)
        .await
        .expect("the whole response arrived after reading resumed");
    assert_eq!(
        response_body(&connection.received, BIG_BODY).len(),
        BIG_BODY
    );
    drop(connection.client);
    tokio::time::timeout(WITHIN, connection.task)
        .await
        .expect("the completed client closed its connection")
        .unwrap();
    assert_eq!(connection.stats.idle_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(
        connection
            .stats
            .write_stall_timeouts
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        connection.stats.active_connections.load(Ordering::Relaxed),
        0
    );
    assert_eq!(connection.slots.available_permits(), 1);
}

#[tokio::test]
async fn an_unread_finished_http1_response_is_closed_by_the_write_stall_timeout() {
    let mut connection = finished_connection(BIG_BODY, WRITE_STALL_TIMEOUT).await;
    let started = Instant::now();
    // No more reads: response data cannot leave the bounded transport.
    tokio::time::timeout(WITHIN, connection.task)
        .await
        .expect("the stalled connection closed")
        .unwrap();
    assert!(started.elapsed() >= WRITE_STALL_TIMEOUT / 2);
    assert_eq!(
        connection
            .stats
            .write_stall_timeouts
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(connection.stats.idle_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(
        connection.stats.active_connections.load(Ordering::Relaxed),
        0
    );
    assert_eq!(connection.slots.available_permits(), 1);
    connection
        .client
        .read_to_end(&mut connection.received)
        .await
        .unwrap();
    assert!(response_body(&connection.received, BIG_BODY).len() < BIG_BODY);
}

#[tokio::test]
async fn a_finished_http1_response_is_idle_while_the_client_reads_buffered_data() {
    const SMALL_BODY: usize = 128;
    let mut connection = finished_connection(SMALL_BODY, WRITE_STALL_TIMEOUT).await;
    // The whole response fits in the transport. The server has no pending
    // writes, although the client has not consumed the buffered body yet.
    tokio::time::timeout(WITHIN, connection.task)
        .await
        .expect("the connection closed after becoming idle")
        .unwrap();
    assert_eq!(connection.stats.idle_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(
        connection
            .stats
            .write_stall_timeouts
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        connection.stats.active_connections.load(Ordering::Relaxed),
        0
    );
    assert_eq!(connection.slots.available_permits(), 1);
    connection
        .client
        .read_to_end(&mut connection.received)
        .await
        .unwrap();
    assert_eq!(
        response_body(&connection.received, SMALL_BODY).len(),
        SMALL_BODY
    );
}

/// Observes actual backpressure from the bounded transport, rather than
/// assuming that sending control frames made the server's writes block.
struct ObservedIo {
    io: DuplexStream,
    blocked: Arc<AtomicBool>,
}

impl AsyncRead for ObservedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl AsyncWrite for ObservedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.io).poll_write(cx, buf);
        this.blocked.store(result.is_pending(), Ordering::Relaxed);
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.io).poll_flush(cx);
        this.blocked.store(result.is_pending(), Ordering::Relaxed);
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// Time is paused, so give the connection a bounded number of polls without
/// advancing to an unrelated timer while waiting for an IO state change.
async fn wait_for_write_state(blocked: &AtomicBool, expected: bool) {
    for _ in 0..1000 {
        if blocked.load(Ordering::Relaxed) == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        blocked.load(Ordering::Relaxed),
        expected,
        "the server reached the expected transport write state"
    );
}

async fn h2_frame(client: &mut DuplexStream) -> ([u8; FRAME_HEADER_LEN], Vec<u8>) {
    let mut header = [0; FRAME_HEADER_LEN];
    client.read_exact(&mut header).await.unwrap();
    let len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
    assert!(len <= CAPACITY, "the test receives only small frames");
    let mut payload = vec![0; len];
    client.read_exact(&mut payload).await.unwrap();
    (header, payload)
}

#[tokio::test(start_paused = true)]
async fn repeated_http2_control_backpressure_is_idle_and_releases_its_slot() {
    const SMALL_BODY: usize = 128;
    const CONTROL_CAPACITY: usize = 64;
    const PINGS: usize = 4;
    const SETTINGS: &[u8] = &[0, 0, 0, 4, 0, 0, 0, 0, 0];
    const PING: &[u8] = &[0, 0, 8, 6, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    // GET / on stream 1, with END_STREAM and END_HEADERS. HPACK uses
    // indexed method, scheme and path, and a literal authority of "t".
    const GET: &[u8] = &[0, 0, 6, 1, 5, 0, 0, 0, 1, 0x82, 0x86, 0x84, 0x01, 1, b't'];
    let idle_timeout = IDLE_TIMEOUT * 4;
    let options = ServeOptions {
        name: "app",
        max_connections: 1,
        max_header_count: 100,
        max_header_bytes: 8192,
        http2_max_concurrent_streams: 1,
        header_read_timeout: IDLE_TIMEOUT,
        idle_timeout,
        write_stall_timeout: IDLE_TIMEOUT,
        drain_timeout: WITHIN,
        #[cfg(feature = "tls")]
        tls: None,
    };
    let stats = Arc::new(ServerStats::default());
    let slots = Arc::new(Semaphore::new(1));
    let sockets = TaskTracker::new();
    let streams = Streams::default();
    let close = streams.connection();
    let builder = builder(&options, streams.executor(&close));
    let active = ActiveConnection::open(
        Arc::clone(&stats),
        close,
        Arc::clone(&slots).acquire_owned().await.unwrap(),
        sockets.token(),
    );
    let finished = Arc::new(Notify::new());
    let (release_body, release_body_rx) = tokio::sync::oneshot::channel();
    let release_body_rx = Arc::new(tokio::sync::Mutex::new(Some(release_body_rx)));
    let body_finished = Arc::clone(&finished);
    let routes = Router::new().route(
        "/",
        get(move || {
            let body_finished = Arc::clone(&body_finished);
            let release_body_rx = Arc::clone(&release_body_rx);
            async move {
                let release = release_body_rx
                    .lock()
                    .await
                    .take()
                    .expect("the test route is called once");
                Body::new(GuardedFinishedBody {
                    data: Some(Bytes::from(vec![b'x'; SMALL_BODY])),
                    release,
                    released: false,
                    finished: body_finished,
                })
            }
        }),
    );
    let lifecycle = Lifecycle::new(Arc::default());
    let peer = PeerInfo {
        remote_addr: Some(SocketAddr::from(([127, 0, 0, 1], 12345))),
        tls: None,
    };
    let blocked = Arc::new(AtomicBool::new(false));
    let (mut client, server) = tokio::io::duplex(CONTROL_CAPACITY);
    let io = ObservedIo {
        io: server,
        blocked: Arc::clone(&blocked),
    };
    let task = tokio::spawn(async move {
        serve_io(io, peer, active, builder, routes, &options, &lifecycle).await;
    });
    let receive = async {
        client
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        client.write_all(SETTINGS).await.unwrap();
        let (header, _) = h2_frame(&mut client).await;
        assert_eq!(header[3], 4, "the server selected HTTP/2");
        assert_eq!(header[4], 0, "the server's initial SETTINGS");
        client
            .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
            .await
            .unwrap();
        client.write_all(GET).await.unwrap();
        let mut body = Vec::new();
        let mut release_body = Some(release_body);
        let mut body_released = false;
        loop {
            let (header, payload) = h2_frame(&mut client).await;
            if header[3] == DATA_FRAME {
                assert_eq!(&header[5..], &[0, 0, 0, 1]);
                body.extend_from_slice(&payload);
                if body.len() >= SMALL_BODY && !body_released {
                    release_body
                        .take()
                        .expect("the body guard is released once")
                        .send(())
                        .expect("the response body is waiting for client progress");
                    body_released = true;
                }
                if header[4] & 1 != 0 {
                    break;
                }
            }
        }
        assert_eq!(body, vec![b'x'; SMALL_BODY]);
        finished.notified().await;
    };
    tokio::time::timeout(WITHIN, receive)
        .await
        .expect("the response completed before control-only traffic");
    streams.tracker.close();
    tokio::time::timeout(WITHIN, streams.tracker.wait())
        .await
        .expect("the response stream task released its request guard");
    wait_for_write_state(&blocked, false).await;
    let completed = Instant::now();
    assert_eq!(stats.active_connections.load(Ordering::Relaxed), 1);
    assert_eq!(slots.available_permits(), 0);

    // Four PING ACKs plus a SETTINGS ACK exceed the exact transport
    // capacity. Repeatedly fill it, then drain it before the stall timeout,
    // without another request, WINDOW_UPDATE, or response DATA byte.
    for _ in 0..3 {
        for _ in 0..PINGS {
            client.write_all(PING).await.unwrap();
        }
        client.write_all(SETTINGS).await.unwrap();
        wait_for_write_state(&blocked, true).await;
        tokio::time::advance(IDLE_TIMEOUT / 4).await;
        for _ in 0..PINGS {
            let (header, payload) = h2_frame(&mut client).await;
            assert_eq!(header[3], 6, "only PING ACKs, never response DATA");
            assert_eq!(header[4], 1);
            assert_eq!(payload, &PING[FRAME_HEADER_LEN..]);
        }
        let (header, payload) = h2_frame(&mut client).await;
        assert_eq!(header[3], 4, "the SETTINGS ACK followed the PING ACKs");
        assert_eq!(header[4], 1);
        assert!(payload.is_empty());
        wait_for_write_state(&blocked, false).await;
        // Let the stall sampler see no pending write between bursts.
        tokio::time::advance(IDLE_TIMEOUT / 2).await;
        tokio::task::yield_now().await;
        assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 0);
        assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 0);
    }

    // Put another short control-only blockage across the idle deadline.
    let until_block = completed + idle_timeout - IDLE_TIMEOUT / 4;
    tokio::time::advance(until_block - Instant::now()).await;
    tokio::task::yield_now().await;
    for _ in 0..PINGS {
        client.write_all(PING).await.unwrap();
    }
    client.write_all(SETTINGS).await.unwrap();
    wait_for_write_state(&blocked, true).await;
    tokio::time::advance(IDLE_TIMEOUT / 2).await;
    for _ in 0..1000 {
        if stats.idle_timeouts.load(Ordering::Relaxed) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 0);
    assert!(blocked.load(Ordering::Relaxed));

    // Keep the client and unread ACKs alive through the close grace period.
    // Only server-side cancellation can release this transport and its slot.
    tokio::time::timeout(WITHIN, task)
        .await
        .expect("the idle connection closed within its bounded grace period")
        .unwrap();
    assert!(completed.elapsed() <= idle_timeout + IDLE_TIMEOUT * 2);
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(stats.first_request_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(stats.force_closed_connections.load(Ordering::Relaxed), 0);
    assert_eq!(stats.force_closed_streams.load(Ordering::Relaxed), 0);
    assert_eq!(stats.active_connections.load(Ordering::Relaxed), 0);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(sockets.len(), 0);
    assert_eq!(streams.tracker.len(), 0);
    let mut remaining = Vec::new();
    client.read_to_end(&mut remaining).await.unwrap();
}
