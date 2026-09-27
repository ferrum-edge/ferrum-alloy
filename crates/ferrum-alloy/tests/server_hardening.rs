//! Connection hardening over real sockets: a peer that never sends a
//! complete request head cannot keep a connection slot, on either listener
//! and whatever protocol it starts, and shutdown leaves no connection behind,
//! including one stuck in a TLS handshake.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::AlloyConfig;
use http::{Request, StatusCode};
use http_body_util::Empty;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const HEADER_READ_TIMEOUT: Duration = Duration::from_millis(300);
/// Upper bound for the server to act on a timer or shutdown. Generous, so
/// slow CI hosts do not flake; the defects it catches never resolve at all.
const WITHIN: Duration = Duration::from_secs(5);
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

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
}

fn hardened() -> AlloyConfig {
    let mut config = support::config();
    config.server.header_read_timeout_ms = HEADER_READ_TIMEOUT.as_millis() as u64;
    config
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
    // An empty SETTINGS frame: a valid HTTP/2 connection with no request.
    idle.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();
    assert!(
        closed_by_server(&mut idle).await,
        "an HTTP/2 connection that never sends HEADERS is closed"
    );
    assert_eq!(served_again(server.addr, "/hello").await, StatusCode::OK);
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
}

#[cfg(feature = "tls")]
mod tls {
    use super::*;

    use ferrum_alloy::config::{ClientAuth, TlsSettings};

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
        config.server.tls = Some(TlsSettings {
            cert_path: pki::write(dir.path(), "server.pem", &server.cert_pem),
            key_path: pki::write(dir.path(), "server.key", &server.key_pem),
            client_ca_path: None,
            client_auth: ClientAuth::None,
            handshake_timeout_ms: HANDSHAKE_TIMEOUT_MS,
        });
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
