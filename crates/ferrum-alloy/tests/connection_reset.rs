//! Client connection resets injected from a raw socket: mid-request-body and
//! mid-response-body. Each request must be finalized exactly once, with the
//! outcome Alloy can truthfully claim. The faults stay inside this process.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::routing::{get, post};
use ferrum_alloy::AlloyApp;
use ferrum_alloy::telemetry::metrics::{BODY_OUTCOMES, Metrics};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A response body that never ends and never fails: one chunk now, then one
/// every 20 ms. Only a disconnect can finalize it.
fn endless() -> Body {
    let chunks = futures_util::stream::unfold(true, |first| async move {
        if !first {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Some((
            Ok::<_, std::io::Error>(Bytes::from_static(b"tick\n")),
            false,
        ))
    });
    Body::from_stream(chunks)
}

/// `/upload` reads the whole request body, reports whether that failed, and
/// then answers with an endless body.
fn router(read_failures: mpsc::UnboundedSender<bool>) -> Router {
    Router::new()
        .route("/stream", get(|| async { endless() }))
        .route(
            "/upload",
            post(move |body: Body| {
                let read_failures = read_failures.clone();
                async move {
                    let failed = axum::body::to_bytes(body, usize::MAX).await.is_err();
                    let _ = read_failures.send(failed);
                    endless()
                }
            }),
        )
}

async fn start(read_failures: mpsc::UnboundedSender<bool>) -> support::TestServer {
    let app = AlloyApp::new("reset-test").router(router(read_failures));
    support::start(app, support::config()).await
}

/// Aborts the connection with a TCP reset instead of an orderly close.
fn reset(stream: TcpStream) {
    stream.set_zero_linger().unwrap();
    drop(stream);
}

fn finalized(metrics: &Metrics) -> u64 {
    BODY_OUTCOMES
        .iter()
        .map(|label| metrics.body_outcomes.get(label))
        .sum()
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for the request to be finalized, then checks that no second
/// finalization follows.
async fn assert_finalized_once(metrics: &Metrics) {
    let settled = || finalized(metrics) > 0 && metrics.in_flight() == 0;
    wait_until("finalization", settled).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(finalized(metrics), 1, "finalized exactly once");
    assert_eq!(metrics.in_flight(), 0);
}

#[tokio::test]
async fn a_reset_mid_response_body_is_cancelled_exactly_once() {
    let (read_failures, _unused) = mpsc::unbounded_channel();
    let server = start(read_failures).await;
    let metrics = server.lifecycle.metrics();

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nhost: reset-test\r\n\r\n")
        .await
        .unwrap();
    // Read until the response head and the first body chunk have arrived.
    let mut received = Vec::new();
    let mut buffer = [0u8; 1024];
    while !received.windows(5).any(|window| window == b"tick\n") {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "the server closed the connection early");
        received.extend_from_slice(&buffer[..read]);
    }
    assert!(received.starts_with(b"HTTP/1.1 200"));
    assert_eq!(metrics.in_flight(), 1, "headers alone do not finalize");
    assert_eq!(finalized(&metrics), 0);

    reset(stream);
    assert_finalized_once(&metrics).await;
    assert_eq!(
        metrics.body_outcomes.get("cancelled"),
        1,
        "headers were produced, the body never ended"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reset_mid_request_body_fails_the_read_and_finalizes_once() {
    let (read_failures, mut failures) = mpsc::unbounded_channel();
    let server = start(read_failures).await;
    let metrics = server.lifecycle.metrics();

    // Promise 64 KiB, send 1 KiB, and reset while the handler is reading.
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"POST /upload HTTP/1.1\r\nhost: reset-test\r\ncontent-length: 65536\r\n\r\n")
        .await
        .unwrap();
    stream.write_all(&[b'x'; 1024]).await.unwrap();
    wait_until("the request to start", || metrics.in_flight() == 1).await;
    reset(stream);

    let failed = tokio::time::timeout(Duration::from_secs(5), failures.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(failed, "the handler saw the request body fail");
    assert_finalized_once(&metrics).await;
    // The handler answered after the read failed, so headers exist; the
    // endless body can only end by being dropped with the connection.
    assert_eq!(
        metrics.body_outcomes.get("cancelled"),
        1,
        "the response could not be delivered"
    );
    server.shutdown().await.unwrap();
}
