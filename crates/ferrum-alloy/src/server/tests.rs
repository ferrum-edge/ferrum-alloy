//! HTTP/1 response completion over bounded IO. Socket buffer options do not
//! guarantee how much a platform accepts in one write; a duplex transport
//! gives these regressions an exact capacity on every hosted test platform.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;

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
        connection.stats.write_stall_timeouts.load(Ordering::Relaxed),
        0
    );
    let receive = async {
        let mut buf = [0u8; CAPACITY];
        loop {
            if let Some(end) = connection.received.windows(4).position(|w| w == b"\r\n\r\n")
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
        connection.stats.write_stall_timeouts.load(Ordering::Relaxed),
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
        connection.stats.write_stall_timeouts.load(Ordering::Relaxed),
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
        connection.stats.write_stall_timeouts.load(Ordering::Relaxed),
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
