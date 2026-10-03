//! Connection hardening over real sockets: a peer that never sends a
//! complete request head, goes idle after a request, or stops taking its
//! response cannot keep a connection slot, on either listener and whatever
//! protocol it starts, and shutdown leaves no connection or HTTP/2 handler
//! behind, including a connection stuck in a TLS handshake.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::AlloyConfig;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const HEADER_READ_TIMEOUT: Duration = Duration::from_millis(300);
const IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const WRITE_STALL_TIMEOUT: Duration = Duration::from_millis(600);
/// Upper bound for the server to act on a timer or shutdown. Generous, so
/// slow CI hosts do not flake; the defects it catches never resolve at all.
const WITHIN: Duration = Duration::from_secs(5);
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// An empty SETTINGS frame: with the preface, a valid HTTP/2 connection with
/// no request.
const H2_EMPTY_SETTINGS: &[u8] = &[0, 0, 0, 4, 0, 0, 0, 0, 0];
/// A PING frame, which the server must acknowledge with a PING of its own.
const H2_PING_FRAME: &[u8] = &[0, 0, 8, 6, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
/// The initial HTTP/2 flow-control window, for a stream and a connection.
const H2_INITIAL_WINDOW: u32 = 65_535;
const H2_DATA: u8 = 0x0;
const H2_HEADERS: u8 = 0x1;
const H2_PING: u8 = 0x6;
const H2_GOAWAY: u8 = 0x7;
/// The size of each chunk of the endless `/flood` response.
const FLOOD_CHUNK: usize = 16 * 1024;
/// The size of the `/big` response, sent in one chunk.
const BIG_BODY: usize = 2 * 1024 * 1024;
/// How much a slow reader takes at a time, and how long it waits in between.
const BITE: usize = 16 * 1024;
/// Keep each pause well below the configured idle timeout, with room for CI
/// scheduling delays, while making the complete body take several timeouts.
const PAUSE: Duration = IDLE_TIMEOUT / 8;

fn router() -> Router {
    Router::new()
        .route("/hello", get(|| async { "hello" }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(HEADER_READ_TIMEOUT * 3).await;
                "slow"
            }),
        )
        .route(
            "/forever",
            get(|| async {
                let (mut sender, body) =
                    http_body_util::channel::Channel::<Bytes, Infallible>::new(1);
                tokio::spawn(async move {
                    let tick = Bytes::from_static(b"tick\n");
                    while sender.send_data(tick.clone()).await.is_ok() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                });
                Body::new(body)
            }),
        )
        .route(
            "/flood",
            get(|| async {
                let (mut sender, body) =
                    http_body_util::channel::Channel::<Bytes, Infallible>::new(1);
                tokio::spawn(async move {
                    let chunk = Bytes::from(vec![b'x'; FLOOD_CHUNK]);
                    while sender.send_data(chunk.clone()).await.is_ok() {}
                });
                Body::new(body)
            }),
        )
        .route("/big", get(big))
}

/// A response of [`BIG_BODY`] bytes in one chunk.
async fn big() -> Bytes {
    Bytes::from(vec![b'x'; BIG_BODY])
}

fn hardened() -> AlloyConfig {
    let mut config = support::config();
    config.server.header_read_timeout_ms = HEADER_READ_TIMEOUT.as_millis() as u64;
    config
}

fn idle_limited() -> AlloyConfig {
    let mut config = hardened();
    config.server.idle_timeout_ms = IDLE_TIMEOUT.as_millis() as u64;
    config
}

fn write_stall_limited() -> AlloyConfig {
    let mut config = hardened();
    config.server.write_stall_timeout_ms = WRITE_STALL_TIMEOUT.as_millis() as u64;
    config
}

/// A HEADERS frame for `GET path` on stream 1 that ends the stream: HPACK
/// indexed `:method: GET` and `:scheme: http`, then literal `:path` and
/// `:authority` without indexing.
fn h2_get(path: &str) -> Vec<u8> {
    let mut block = vec![0x82, 0x86];
    for (index, value) in [(0x04, path), (0x01, "localhost")] {
        block.push(index);
        block.push(value.len() as u8);
        block.extend_from_slice(value.as_bytes());
    }
    let mut frame = (block.len() as u32).to_be_bytes()[1..].to_vec();
    // Type HEADERS, flags END_STREAM | END_HEADERS, stream 1.
    frame.extend_from_slice(&[H2_HEADERS, 0x5, 0, 0, 0, 1]);
    frame.extend_from_slice(&block);
    frame
}

/// Sets its flag when dropped.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn request(uri: &str) -> Request<Empty<Bytes>> {
    Request::get(uri)
        .header("host", "localhost")
        .body(Empty::new())
        .unwrap()
}

/// Waits up to [`WITHIN`] for the server to close `stream` (EOF or reset),
/// discarding anything it sends first.
async fn closed_by_server<S: AsyncRead + Unpin>(stream: &mut S) -> bool {
    let mut buf = [0u8; 1024];
    let closed = async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    };
    tokio::time::timeout(WITHIN, closed).await.is_ok()
}

/// Reads HTTP/2 frames until the server closes `stream` and returns their
/// types, or `None` when it is still open after [`WITHIN`].
async fn frame_types_until_closed(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut received = Vec::new();
    let read_all = async {
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
        }
    };
    tokio::time::timeout(WITHIN, read_all).await.ok()?;
    Some(frames(&received).iter().map(|&(kind, _)| kind).collect())
}

/// Like [`frame_types_until_closed`], but sends a PING every 50 ms and never
/// a WINDOW_UPDATE, and returns each frame's type and payload length.
async fn frames_until_closed_pinging<S>(stream: &mut S) -> Option<Vec<(u8, usize)>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut received = Vec::new();
    let read_all = async {
        let mut buf = [0u8; 4096];
        let mut ping = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                read = stream.read(&mut buf) => match read {
                    Ok(0) | Err(_) => return,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                },
                _ = ping.tick() => {
                    // The server may have closed the connection already.
                    let _ = stream.write_all(H2_PING_FRAME).await;
                    let _ = stream.flush().await;
                }
            }
        }
    };
    tokio::time::timeout(WITHIN, read_all).await.ok()?;
    Some(frames(&received))
}

/// The type and payload length of each HTTP/2 frame in `bytes`.
fn frames(bytes: &[u8]) -> Vec<(u8, usize)> {
    let mut frames = Vec::new();
    let mut rest = bytes;
    while rest.len() >= 9 {
        let length = u32::from_be_bytes([0, rest[0], rest[1], rest[2]]) as usize;
        frames.push((rest[3], length));
        rest = rest.get(9 + length..).unwrap_or_default();
    }
    frames
}

/// Waits up to [`WITHIN`] for `counter` to become non-zero.
async fn counted(counter: &AtomicU64) {
    let deadline = Instant::now() + WITHIN;
    while counter.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "not counted within {WITHIN:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Connects with a small receive buffer, so that the server's writes block
/// soon after the client stops reading.
async fn connect_with_small_window(addr: SocketAddr) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(16 * 1024).unwrap();
    socket.connect(addr).await.unwrap()
}

/// Binds a loopback listener whose connections have a small send buffer, so
/// that a large response waits in the server rather than in the kernel, and
/// the server keeps writing it for as long as the client reads it.
fn listener_with_small_send_buffer() -> (TcpListener, u32) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_send_buffer_size(16 * 1024).unwrap();
    let effective_send_buffer_size = socket.send_buffer_size().unwrap();
    socket.bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    (socket.listen(1024).unwrap(), effective_send_buffer_size)
}

/// Reads the HTTP/1.1 response to `GET /big` from `stream`, [`BITE`] bytes
/// at most at a time and [`PAUSE`] apart, and returns how much of its body
/// arrived before it was complete or the connection ended.
async fn read_big_http1_slowly(stream: &mut TcpStream) -> usize {
    let mut head = Vec::new();
    let mut body = None;
    let mut buf = vec![0u8; BITE];
    loop {
        let n = tokio::time::timeout(WITHIN, stream.read(&mut buf))
            .await
            .expect("the HTTP/1.1 response keeps coming")
            .unwrap_or(0);
        body = match body {
            Some(body) => Some(body + n),
            None => {
                head.extend_from_slice(&buf[..n]);
                let end = head.windows(4).position(|window| window == b"\r\n\r\n");
                end.map(|end| head.len() - end - 4)
            }
        };
        if n == 0 || body >= Some(BIG_BODY) {
            return body.unwrap_or_default();
        }
        tokio::time::sleep(PAUSE).await;
    }
}

/// Reads the HTTP/2 response body of `GET /big`, one DATA frame (at most
/// [`BITE`] bytes) at a time and [`PAUSE`] apart, and returns how much of it
/// arrived before it ended or failed.
async fn read_big_http2_slowly(body: &mut Incoming) -> usize {
    let mut received = 0;
    loop {
        let frame = tokio::time::timeout(WITHIN, body.frame())
            .await
            .expect("the HTTP/2 response keeps coming");
        let Some(Ok(frame)) = frame else {
            return received;
        };
        if let Ok(data) = frame.into_data() {
            received += data.len();
        }
        tokio::time::sleep(PAUSE).await;
    }
}

/// Reads `body` for `period` and then up to one more data frame, failing if
/// the stream ends or fails first.
async fn still_streaming_after(body: &mut Incoming, period: Duration) {
    let until = Instant::now() + period;
    loop {
        let frame = tokio::time::timeout(WITHIN, body.frame())
            .await
            .expect("the stream keeps sending")
            .expect("the stream has not ended")
            .expect("the stream has not failed");
        if frame.is_data() && Instant::now() >= until {
            return;
        }
    }
}

/// One HTTP/1.1 request on a fresh connection; `None` when the connection is
/// closed without a response (for example, at the connection limit).
async fn try_get(addr: SocketAddr, path: &str) -> Option<StatusCode> {
    let tcp = TcpStream::connect(addr).await.ok()?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(tcp)).await.ok()?;
    tokio::spawn(connection);
    let response = sender.send_request(request(path)).await.ok()?;
    Some(response.status())
}

/// Retries until the listener serves a request again, which needs a free
/// connection slot.
async fn served_again(addr: SocketAddr, path: &str) -> StatusCode {
    let deadline = Instant::now() + WITHIN;
    loop {
        let attempt = tokio::time::timeout(Duration::from_secs(2), try_get(addr, path)).await;
        if let Ok(Some(status)) = attempt {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{addr} did not serve a request again within {WITHIN:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_silent_connection_is_closed_and_releases_its_slot() {
    let mut config = hardened();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let mut silent = TcpStream::connect(server.addr).await.unwrap();
    let started = Instant::now();
    assert!(
        closed_by_server(&mut silent).await,
        "a connection that sends nothing is closed"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= HEADER_READ_TIMEOUT / 2,
        "closed by the header read timeout, not at once ({elapsed:?})"
    );
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_partial_http2_preface_is_closed_and_releases_its_slot() {
    let mut config = hardened();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let mut partial = TcpStream::connect(server.addr).await.unwrap();
    partial.write_all(&H2_PREFACE[..16]).await.unwrap();
    assert!(
        closed_by_server(&mut partial).await,
        "a connection that stops inside the HTTP/2 preface is closed"
    );
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_http2_connection_without_a_request_is_closed() {
    let mut config = hardened();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let mut idle = TcpStream::connect(server.addr).await.unwrap();
    idle.write_all(H2_PREFACE).await.unwrap();
    idle.write_all(H2_EMPTY_SETTINGS).await.unwrap();
    assert!(
        closed_by_server(&mut idle).await,
        "an HTTP/2 connection that never sends HEADERS is closed"
    );
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    server.shutdown().await.unwrap();
}

/// A client that pre-warmed an HTTP/2 connection learns from `GOAWAY` that
/// it is closing, so a request it sends at that moment can be retried
/// safely instead of meeting a bare reset.
#[tokio::test]
async fn an_http2_connection_without_a_request_is_sent_goaway() {
    let server = support::start(AlloyApp::new("hardening").router(router()), hardened()).await;
    let mut idle = TcpStream::connect(server.addr).await.unwrap();
    idle.write_all(H2_PREFACE).await.unwrap();
    idle.write_all(H2_EMPTY_SETTINGS).await.unwrap();
    let started = Instant::now();
    let frames = frame_types_until_closed(&mut idle)
        .await
        .expect("the idle HTTP/2 connection is closed");
    let elapsed = started.elapsed();
    assert!(
        frames.contains(&H2_GOAWAY),
        "GOAWAY is sent before the connection closes (frame types: {frames:?})"
    );
    assert!(
        elapsed >= HEADER_READ_TIMEOUT / 2,
        "closed by the header read timeout, not at once ({elapsed:?})"
    );
    assert_eq!(
        server.stats.first_request_timeouts.load(Ordering::Relaxed),
        1
    );
    server.shutdown().await.unwrap();
}

/// A peer that sends one request and then only keeps the connection open is
/// sent `GOAWAY` once no request has been in flight for the idle timeout,
/// and its slot is freed.
#[tokio::test]
async fn an_idle_http2_connection_after_a_request_is_sent_goaway() {
    let mut config = idle_limited();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let mut idle = TcpStream::connect(server.addr).await.unwrap();
    idle.write_all(H2_PREFACE).await.unwrap();
    idle.write_all(H2_EMPTY_SETTINGS).await.unwrap();
    idle.write_all(&h2_get("/hello")).await.unwrap();
    let started = Instant::now();
    let frames = frame_types_until_closed(&mut idle)
        .await
        .expect("the idle HTTP/2 connection is closed");
    let elapsed = started.elapsed();
    let response = frames.iter().position(|&kind| kind == H2_HEADERS);
    let goaway = frames.iter().position(|&kind| kind == H2_GOAWAY);
    assert!(
        response.is_some() && goaway > response,
        "the request is answered, then GOAWAY is sent (frame types: {frames:?})"
    );
    assert!(
        elapsed >= IDLE_TIMEOUT / 2,
        "closed by the idle timeout, not at once ({elapsed:?})"
    );
    let stats = &server.stats;
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.first_request_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    server.shutdown().await.unwrap();
}

/// A well-behaved client that keeps its HTTP/2 connection open after a
/// request is disconnected by the idle timeout, and its slot is freed. A
/// request that runs longer than the idle timeout is not idle time.
#[tokio::test]
async fn an_idle_http2_client_is_disconnected_and_releases_its_slot() {
    let mut config = idle_limited();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let io = TokioIo::new(tcp);
    let (mut sender, connection) = http2::handshake(TokioExecutor::new(), io).await.unwrap();
    let connection = tokio::spawn(connection);
    let slow = format!("http://{}/slow", server.addr);
    let response = sender.send_request(request(&slow)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The client keeps `sender`, so only the server can end the connection.
    let closed = tokio::time::timeout(WITHIN, connection).await;
    assert!(closed.is_ok(), "the server closed the idle connection");
    drop(sender);
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    let stats = &server.stats;
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 1);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_management_listener_closes_silent_connections_and_frees_its_slots() {
    let server = support::start(AlloyApp::new("hardening").router(router()), hardened()).await;
    // The management listener admits 64 connections: occupy all of them.
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(TcpStream::connect(server.management).await.unwrap());
    }
    held[0].write_all(&H2_PREFACE[..16]).await.unwrap();
    for stream in &mut held {
        assert!(
            closed_by_server(stream).await,
            "a management connection without a request is closed"
        );
    }
    assert_eq!(
        served_again(server.management, "/livez").await,
        StatusCode::OK
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_responses_and_idle_http2_connections_are_not_cut() {
    let server = support::start(AlloyApp::new("hardening").router(router()), hardened()).await;
    // HTTP/1.1: the handler takes longer than the header read timeout.
    assert_eq!(try_get(server.addr, "/slow").await, Some(StatusCode::OK));

    // HTTP/2: one connection, idle for longer than the timeout between
    // requests, then a slow request on the same connection.
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let io = TokioIo::new(tcp);
    let (mut sender, connection) = http2::handshake(TokioExecutor::new(), io).await.unwrap();
    tokio::spawn(connection);
    let hello = format!("http://{}/hello", server.addr);
    let response = sender.send_request(request(&hello)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), http::Version::HTTP_2);
    tokio::time::sleep(HEADER_READ_TIMEOUT * 3).await;
    let slow = format!("http://{}/slow", server.addr);
    let response = sender.send_request(request(&slow)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    server.shutdown().await.unwrap();
}

/// A request stays in flight until its response body ends, not just until
/// its handler returns, so a response stream that outlasts the idle timeout
/// several times over is never cut, on either protocol.
#[tokio::test]
async fn long_response_streams_outlive_the_idle_timeout_on_either_protocol() {
    let server = support::start(AlloyApp::new("hardening").router(router()), idle_limited()).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, connection) = http1::handshake(TokioIo::new(tcp)).await.unwrap();
    tokio::spawn(connection);
    let response = sender.send_request(request("/forever")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), http::Version::HTTP_11);
    let mut http1_body = response.into_body();

    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let io = TokioIo::new(tcp);
    let (mut sender, connection) = http2::handshake(TokioExecutor::new(), io).await.unwrap();
    tokio::spawn(connection);
    let forever = format!("http://{}/forever", server.addr);
    let response = sender.send_request(request(&forever)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), http::Version::HTTP_2);
    let mut http2_body = response.into_body();

    tokio::join!(
        still_streaming_after(&mut http1_body, IDLE_TIMEOUT * 4),
        still_streaming_after(&mut http2_body, IDLE_TIMEOUT * 4),
    );
    assert_eq!(server.stats.idle_timeouts.load(Ordering::Relaxed), 0);
    drop((http1_body, http2_body));
    server.shutdown().await.unwrap();
}

/// An upgraded (WebSocket) connection leaves Hyper after the `101` response,
/// so neither the idle timeout nor the header read timeout cuts the session.
#[tokio::test]
async fn a_websocket_session_outlives_the_idle_timeout() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let routes = router().route(
        "/ws",
        get(|upgrade: WebSocketUpgrade| async move {
            upgrade.on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if let Message::Text(text) = message {
                        let _ = socket
                            .send(Message::Text(format!("echo:{text}").into()))
                            .await;
                    }
                }
            })
        }),
    );
    let server = support::start(AlloyApp::new("hardening").router(routes), idle_limited()).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nhost: t\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n",
        )
        .await
        .unwrap();
    let mut head = vec![0u8; 1024];
    let n = stream.read(&mut head).await.unwrap();
    let text = String::from_utf8_lossy(&head[..n]);
    assert!(text.starts_with("HTTP/1.1 101"), "{text}");
    tokio::time::sleep(IDLE_TIMEOUT * 3 + HEADER_READ_TIMEOUT).await;
    // Masked text frame "hi".
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x81, 0x80 | 2];
    frame.extend_from_slice(&mask);
    frame.extend(b"hi".iter().zip(mask).map(|(byte, key)| byte ^ key));
    stream.write_all(&frame).await.unwrap();
    let mut reply = [0u8; 16];
    let n = tokio::time::timeout(WITHIN, stream.read(&mut reply))
        .await
        .expect("the session answers")
        .unwrap();
    assert_eq!(&reply[..n], b"\x81\x07echo:hi");
    assert_eq!(server.stats.idle_timeouts.load(Ordering::Relaxed), 0);
    drop(stream);
    server.shutdown().await.unwrap();
}

/// Hyper lets go of a response body as soon as it has taken the last chunk,
/// long before a slow reader has all of a large response. Idle time counts
/// from the last response data written, so a reader that keeps taking data
/// gets the whole response however long it takes, on either protocol.
#[tokio::test]
async fn slow_readers_of_a_finished_response_outlive_the_idle_timeout() {
    let (listener, effective_send_buffer_size) = listener_with_small_send_buffer();
    let server = support::start_on(
        AlloyApp::new("hardening").router(router()),
        idle_limited(),
        listener,
    )
    .await;
    assert!(
        BIG_BODY > effective_send_buffer_size as usize * 4,
        "response ({BIG_BODY} bytes) must exceed the effective listener send buffer ({effective_send_buffer_size} bytes) by at least four times"
    );
    let minimum_transfer_time = PAUSE * (BIG_BODY / BITE) as u32;
    assert!(
        minimum_transfer_time > IDLE_TIMEOUT * 3,
        "the configured pacing must keep the download slower than three idle timeouts ({minimum_transfer_time:?})"
    );
    let mut http1 = connect_with_small_window(server.addr).await;
    http1
        .write_all(b"GET /big HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();

    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, connection) = http2::Builder::new(TokioExecutor::new())
        .initial_stream_window_size(H2_INITIAL_WINDOW)
        .initial_connection_window_size(H2_INITIAL_WINDOW)
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(connection);
    let big = format!("http://{}/big", server.addr);
    let response = sender.send_request(request(&big)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), http::Version::HTTP_2);
    let mut http2_body = response.into_body();

    let started = Instant::now();
    let (http1_received, http2_received) = tokio::join!(
        read_big_http1_slowly(&mut http1),
        read_big_http2_slowly(&mut http2_body),
    );
    let elapsed = started.elapsed();
    let close_reasons = format!(
        "idle={}, write_stall={}, first_request={}",
        server.stats.idle_timeouts.load(Ordering::Relaxed),
        server.stats.write_stall_timeouts.load(Ordering::Relaxed),
        server.stats.first_request_timeouts.load(Ordering::Relaxed),
    );
    eprintln!(
        "slow-reader server close reasons: {close_reasons}; effective SO_SNDBUF={effective_send_buffer_size} bytes; transfer elapsed={elapsed:?}"
    );
    assert_eq!(
        http1_received, BIG_BODY,
        "the whole HTTP/1.1 response arrived"
    );
    assert_eq!(
        http2_received, BIG_BODY,
        "the whole HTTP/2 response arrived"
    );
    assert!(
        elapsed > IDLE_TIMEOUT * 3,
        "the downloads outlasted the idle timeout several times ({elapsed:?})"
    );
    assert_eq!(
        server.stats.idle_timeouts.load(Ordering::Relaxed),
        0,
        "the slow-reader connections must not close by timeout (server close reasons: {close_reasons})"
    );
    assert_eq!(
        server.stats.write_stall_timeouts.load(Ordering::Relaxed),
        0,
        "the slow-reader connections must not close by write stall (server close reasons: {close_reasons})"
    );
    drop((http1, http2_body));
    server.shutdown().await.unwrap();
}

/// A peer that asks for a response larger than the initial HTTP/2 window and
/// never sends WINDOW_UPDATE takes none of the rest of it once the response
/// body has ended, so no response data is written and the idle timeout still
/// closes the connection, however many PINGs the peer sends.
#[tokio::test]
async fn a_finished_http2_response_without_window_updates_is_closed_by_the_idle_timeout() {
    let mut config = idle_limited();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let stats = Arc::clone(&server.stats);
    let mut stalled = TcpStream::connect(server.addr).await.unwrap();
    stalled.write_all(H2_PREFACE).await.unwrap();
    stalled.write_all(H2_EMPTY_SETTINGS).await.unwrap();
    stalled.write_all(&h2_get("/big")).await.unwrap();
    let started = Instant::now();
    let frames = frames_until_closed_pinging(&mut stalled)
        .await
        .expect("the HTTP/2 connection is closed");
    let elapsed = started.elapsed();
    let kinds: Vec<u8> = frames.iter().map(|&(kind, _)| kind).collect();
    let response = kinds.iter().position(|&kind| kind == H2_HEADERS);
    let goaway = kinds.iter().position(|&kind| kind == H2_GOAWAY);
    assert!(
        response.is_some() && goaway > response,
        "the response starts, then GOAWAY is sent (frame types: {kinds:?})"
    );
    let data: usize = frames
        .iter()
        .filter(|&&(kind, _)| kind == H2_DATA)
        .map(|&(_, length)| length)
        .sum();
    assert!(
        (1..=H2_INITIAL_WINDOW as usize).contains(&data),
        "the response filled the initial window and no more ({data} bytes)"
    );
    assert!(
        elapsed >= IDLE_TIMEOUT / 2,
        "closed by the idle timeout, not at once ({elapsed:?})"
    );
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    server.shutdown().await.unwrap();
}

/// A peer that asks for a response larger than the initial HTTP/2 window and
/// never sends WINDOW_UPDATE, while sending PINGs the server must answer, is
/// sent `GOAWAY` once no response data has been written for the write stall
/// timeout. Its slot is freed and its stream handler ends.
#[tokio::test]
async fn an_http2_response_without_window_updates_is_sent_goaway_and_releases_its_slot() {
    let mut config = write_stall_limited();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let stats = Arc::clone(&server.stats);
    let mut stalled = TcpStream::connect(server.addr).await.unwrap();
    stalled.write_all(H2_PREFACE).await.unwrap();
    stalled.write_all(H2_EMPTY_SETTINGS).await.unwrap();
    stalled.write_all(&h2_get("/flood")).await.unwrap();
    let started = Instant::now();
    let frames = frames_until_closed_pinging(&mut stalled)
        .await
        .expect("the stalled HTTP/2 connection is closed");
    let elapsed = started.elapsed();
    let kinds: Vec<u8> = frames.iter().map(|&(kind, _)| kind).collect();
    let response = kinds.iter().position(|&kind| kind == H2_HEADERS);
    let goaway = kinds.iter().position(|&kind| kind == H2_GOAWAY);
    assert!(
        response.is_some() && goaway > response,
        "the response starts, then GOAWAY is sent (frame types: {kinds:?})"
    );
    assert!(
        kinds.contains(&H2_PING),
        "the server kept answering PINGs (frame types: {kinds:?})"
    );
    let data: usize = frames
        .iter()
        .filter(|&&(kind, _)| kind == H2_DATA)
        .map(|&(_, length)| length)
        .sum();
    assert!(
        (1..=H2_INITIAL_WINDOW as usize).contains(&data),
        "the response filled the initial window and no more ({data} bytes)"
    );
    assert!(
        elapsed >= WRITE_STALL_TIMEOUT / 2,
        "closed by the write stall timeout, not at once ({elapsed:?})"
    );
    assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    let text = stats.render_prometheus("app");
    assert!(
        text.contains("ferrum_alloy_write_stall_timeouts_total{listener=\"app\"} 1\n"),
        "{text}"
    );
    server.shutdown().await.unwrap();
    assert_eq!(
        stats.force_closed_streams.load(Ordering::Relaxed),
        0,
        "the stalled stream handler ended with its connection"
    );
}

/// An HTTP/1.1 client that stops reading its response, so that the server's
/// writes block on a zero TCP receive window, is disconnected once no
/// response data has been written for the write stall timeout, and its slot
/// is freed.
#[tokio::test]
async fn an_http1_client_that_stops_reading_is_disconnected_and_releases_its_slot() {
    let mut config = write_stall_limited();
    config.server.max_connections = 1;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let mut stream = connect_with_small_window(server.addr).await;
    stream
        .write_all(b"GET /flood HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let mut head = [0u8; 64];
    let n = stream.read(&mut head).await.unwrap();
    assert!(
        head[..n].starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head[..n])
    );
    // Stop reading until the server gives up on the response.
    counted(&server.stats.write_stall_timeouts).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= WRITE_STALL_TIMEOUT / 2,
        "closed by the write stall timeout, not at once ({elapsed:?})"
    );
    assert!(
        closed_by_server(&mut stream).await,
        "the connection is closed"
    );
    let stats = &server.stats;
    assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 1);
    assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 0);
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
    server.shutdown().await.unwrap();
}

/// Readers far slower than the server, which keep it waiting on the
/// transport (HTTP/1.1) or on flow control (HTTP/2) for most of the time, are
/// not cut while they keep taking data.
#[tokio::test]
async fn slow_readers_that_keep_reading_are_not_cut_on_either_protocol() {
    let server = support::start(
        AlloyApp::new("hardening").router(router()),
        write_stall_limited(),
    )
    .await;
    let mut http1 = TcpStream::connect(server.addr).await.unwrap();
    http1
        .write_all(b"GET /flood HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();

    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, connection) = http2::Builder::new(TokioExecutor::new())
        .initial_stream_window_size(H2_INITIAL_WINDOW)
        .initial_connection_window_size(H2_INITIAL_WINDOW)
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(connection);
    let flood = format!("http://{}/flood", server.addr);
    let response = sender.send_request(request(&flood)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), http::Version::HTTP_2);
    let mut http2_body = response.into_body();

    let period = WRITE_STALL_TIMEOUT * 3;
    let pause = Duration::from_millis(10);
    tokio::join!(
        async {
            let until = Instant::now() + period;
            let mut buf = vec![0u8; 64 * 1024];
            while Instant::now() < until {
                let n = tokio::time::timeout(WITHIN, http1.read(&mut buf))
                    .await
                    .expect("the HTTP/1.1 response keeps coming")
                    .expect("the HTTP/1.1 connection has not failed");
                assert!(n > 0, "the HTTP/1.1 response was not cut");
                tokio::time::sleep(pause).await;
            }
        },
        async {
            let until = Instant::now() + period;
            while Instant::now() < until {
                let frame = tokio::time::timeout(WITHIN, http2_body.frame())
                    .await
                    .expect("the HTTP/2 response keeps coming")
                    .expect("the HTTP/2 response has not ended")
                    .expect("the HTTP/2 response has not failed");
                assert!(frame.is_data());
                tokio::time::sleep(pause).await;
            }
        },
    );
    assert_eq!(server.stats.write_stall_timeouts.load(Ordering::Relaxed), 0);
    drop((http1, http2_body));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_connection_is_closed_when_serve_on_returns() {
    let mut config = hardened();
    config.shutdown.drain_timeout_ms = 300;
    let server = support::start(AlloyApp::new("hardening").router(router()), config).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /forever HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf).await.unwrap();
    server.lifecycle.trigger_shutdown();
    tokio::time::timeout(WITHIN, server.task)
        .await
        .expect("serve_on returned within the drain budget")
        .unwrap()
        .unwrap();
    assert!(
        closed_by_server(&mut stream).await,
        "the endless stream was force-closed"
    );
    assert_eq!(
        server
            .stats
            .force_closed_connections
            .load(Ordering::Relaxed),
        1,
        "the force-closed connection is counted"
    );
    // The counter reaches `/metrics` under its documented name.
    let text = server.stats.render_prometheus("app");
    assert!(
        text.contains("ferrum_alloy_force_closed_connections_total{listener=\"app\"} 1\n"),
        "{text}"
    );
}

/// HTTP/2 handlers run on stream tasks apart from their connection. One
/// still running at the drain budget is cancelled and counted, and is gone
/// by the time `serve_on` returns.
#[tokio::test]
async fn a_hanging_http2_handler_is_cancelled_at_the_drain_budget() {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let hang = {
        let entered = Arc::clone(&entered);
        let dropped = Arc::clone(&dropped);
        move || {
            let entered = Arc::clone(&entered);
            let dropped = Arc::clone(&dropped);
            async move {
                let _guard = SetOnDrop(dropped);
                entered.notify_one();
                tokio::time::sleep(Duration::from_secs(3_600)).await;
                "done"
            }
        }
    };
    let mut config = hardened();
    config.shutdown.drain_timeout_ms = 300;
    let routes = router().route("/hang", get(hang));
    let server = support::start(AlloyApp::new("hardening").router(routes), config).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let io = TokioIo::new(tcp);
    let (mut sender, connection) = http2::handshake(TokioExecutor::new(), io).await.unwrap();
    tokio::spawn(connection);
    let hang = format!("http://{}/hang", server.addr);
    tokio::spawn(async move {
        let _ = sender.send_request(request(&hang)).await;
    });
    tokio::time::timeout(WITHIN, entered.notified())
        .await
        .expect("the handler is running");
    server.lifecycle.trigger_shutdown();
    tokio::time::timeout(WITHIN, server.task)
        .await
        .expect("serve_on returned within the drain budget")
        .unwrap()
        .unwrap();
    assert!(
        dropped.load(Ordering::SeqCst),
        "the handler was cancelled before serve_on returned"
    );
    assert_eq!(
        server.stats.force_closed_streams.load(Ordering::Relaxed),
        1,
        "the cancelled stream task is counted"
    );
    let text = server.stats.render_prometheus("app");
    assert!(
        text.contains("ferrum_alloy_force_closed_streams_total{listener=\"app\"} 1\n"),
        "{text}"
    );
}

#[cfg(feature = "tls")]
mod tls {
    use super::*;

    use ferrum_alloy::config::TlsSettings;

    use crate::support::pki::{self, Ca};

    /// Longer than every bound in these tests: a handshake that ends is
    /// ended by the server, not by this timeout.
    const HANDSHAKE_TIMEOUT_MS: u64 = 10_000;

    struct Tls {
        _dir: tempfile::TempDir,
        ca: Ca,
        config: AlloyConfig,
    }

    fn tls_listener(drain_timeout_ms: u64) -> Tls {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::new("hardening-test-ca");
        let server = ca.server();
        let mut config = hardened();
        let mut tls = TlsSettings::new(
            pki::write(dir.path(), "server.pem", &server.cert_pem),
            pki::write(dir.path(), "server.key", &server.key_pem),
        );
        tls.handshake_timeout_ms = HANDSHAKE_TIMEOUT_MS;
        tls.reload_interval_ms = 0;
        config.server.tls = Some(tls);
        config.shutdown.drain_timeout_ms = drain_timeout_ms;
        Tls {
            _dir: dir,
            ca,
            config,
        }
    }

    /// Connects without sending a ClientHello, lets the accept loop start the
    /// handshake, then shuts down. `serve_on` must return well before the
    /// handshake timeout, with the socket already closed.
    async fn stalled_handshake_at_shutdown(drain_timeout_ms: u64) {
        let tls = tls_listener(drain_timeout_ms);
        let server = support::start(AlloyApp::new("hardening").router(router()), tls.config).await;
        let mut stalled = TcpStream::connect(server.addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        server.lifecycle.trigger_shutdown();
        tokio::time::timeout(WITHIN, server.task)
            .await
            .expect("serve_on returned before the handshake timeout")
            .unwrap()
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            "shutdown did not wait for the handshake timeout ({elapsed:?})"
        );
        assert!(
            closed_by_server(&mut stalled).await,
            "the stalled handshake's socket is closed once serve_on returns"
        );
    }

    #[tokio::test]
    async fn a_stalled_handshake_does_not_survive_a_short_drain() {
        stalled_handshake_at_shutdown(50).await;
    }

    #[tokio::test]
    async fn a_stalled_handshake_does_not_hold_up_a_long_drain() {
        stalled_handshake_at_shutdown(30_000).await;
    }

    #[tokio::test]
    async fn a_tls_connection_without_a_request_is_closed_and_releases_its_slot() {
        let mut tls = tls_listener(2_000);
        tls.config.server.max_connections = 1;
        let client = pki::client_config(&tls.ca, None);
        let server = support::start(AlloyApp::new("hardening").router(router()), tls.config).await;
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream = tokio_rustls::TlsConnector::from(client)
            .connect(name, tcp)
            .await
            .unwrap();
        assert!(
            closed_by_server(&mut stream).await,
            "a TLS connection that sends no request is closed"
        );
        // The slot is free again: a full TLS handshake and request succeed.
        let client = pki::client_config(&tls.ca, None);
        let deadline = Instant::now() + WITHIN;
        loop {
            if let Some(status) = tls_get(server.addr, &client).await {
                assert_eq!(status, StatusCode::OK);
                break;
            }
            assert!(Instant::now() < deadline, "the TLS listener serves again");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server.shutdown().await.unwrap();
    }

    fn write_stall_limited_tls() -> Tls {
        let mut tls = tls_listener(2_000);
        tls.config.server.write_stall_timeout_ms = WRITE_STALL_TIMEOUT.as_millis() as u64;
        tls
    }

    /// Completes a TLS handshake with the test listener over `tcp`.
    async fn tls_connect(tcp: TcpStream, ca: &Ca) -> tokio_rustls::client::TlsStream<TcpStream> {
        let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
        tokio_rustls::TlsConnector::from(pki::client_config(ca, None))
            .connect(name, tcp)
            .await
            .unwrap()
    }

    /// The write stall timeout works above TLS: an HTTP/1.1 client that
    /// stops reading its response over TLS, so that rustls cannot pass on
    /// the server's writes, is disconnected, and its slot is freed.
    #[tokio::test]
    async fn a_tls_client_that_stops_reading_is_disconnected_and_releases_its_slot() {
        let mut tls = write_stall_limited_tls();
        tls.config.server.max_connections = 1;
        let server = support::start(AlloyApp::new("hardening").router(router()), tls.config).await;
        let tcp = connect_with_small_window(server.addr).await;
        let mut stream = tls_connect(tcp, &tls.ca).await;
        stream
            .write_all(b"GET /flood HTTP/1.1\r\nhost: t\r\n\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let started = Instant::now();
        let mut head = [0u8; 64];
        let n = stream.read(&mut head).await.unwrap();
        assert!(
            head[..n].starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&head[..n])
        );
        // Stop reading until the server gives up on the response.
        counted(&server.stats.write_stall_timeouts).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= WRITE_STALL_TIMEOUT / 2,
            "closed by the write stall timeout, not at once ({elapsed:?})"
        );
        assert!(
            closed_by_server(&mut stream).await,
            "the connection is closed"
        );
        let stats = &server.stats;
        assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 1);
        assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 0);
        let client = pki::client_config(&tls.ca, None);
        let deadline = Instant::now() + WITHIN;
        loop {
            if let Some(status) = tls_get(server.addr, &client).await {
                assert_eq!(status, StatusCode::OK);
                break;
            }
            assert!(Instant::now() < deadline, "the TLS listener serves again");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server.shutdown().await.unwrap();
    }

    /// The write stall timeout counts the HTTP/2 response data a TLS
    /// connection carries, not the TLS records beneath it: a peer that never
    /// sends WINDOW_UPDATE is sent `GOAWAY`, although every PING it sends is
    /// answered in a TLS record of its own.
    #[tokio::test]
    async fn an_http2_response_over_tls_without_window_updates_is_sent_goaway() {
        let tls = write_stall_limited_tls();
        let server = support::start(AlloyApp::new("hardening").router(router()), tls.config).await;
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let mut stalled = tls_connect(tcp, &tls.ca).await;
        stalled.write_all(H2_PREFACE).await.unwrap();
        stalled.write_all(H2_EMPTY_SETTINGS).await.unwrap();
        stalled.write_all(&h2_get("/flood")).await.unwrap();
        stalled.flush().await.unwrap();
        let frames = frames_until_closed_pinging(&mut stalled)
            .await
            .expect("the stalled HTTP/2 connection is closed");
        let kinds: Vec<u8> = frames.iter().map(|&(kind, _)| kind).collect();
        let response = kinds.iter().position(|&kind| kind == H2_HEADERS);
        let goaway = kinds.iter().position(|&kind| kind == H2_GOAWAY);
        assert!(
            response.is_some() && goaway > response,
            "the response starts, then GOAWAY is sent (frame types: {kinds:?})"
        );
        assert!(
            kinds.contains(&H2_PING),
            "the server kept answering PINGs (frame types: {kinds:?})"
        );
        let stats = &server.stats;
        assert_eq!(stats.write_stall_timeouts.load(Ordering::Relaxed), 1);
        assert_eq!(stats.idle_timeouts.load(Ordering::Relaxed), 0);
        server.shutdown().await.unwrap();
    }

    async fn tls_get(
        addr: SocketAddr,
        client: &std::sync::Arc<rustls::ClientConfig>,
    ) -> Option<StatusCode> {
        let tcp = TcpStream::connect(addr).await.ok()?;
        let name = rustls_pki_types::ServerName::try_from("localhost").ok()?;
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::clone(client));
        let stream = connector.connect(name, tcp).await.ok()?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await.ok()?;
        tokio::spawn(connection);
        let response = sender.send_request(request("/hello")).await.ok()?;
        Some(response.status())
    }
}
