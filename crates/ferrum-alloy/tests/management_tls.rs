//! TLS on the management listener (`management.tls`): probes and the
//! token-protected endpoints are served over TLS only, and the listener's
//! certificate is reported under its own `listener` label.

#![cfg(feature = "tls")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::config::TlsSettings;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use support::pki::{self, Ca};
use support::{TOKEN, config, start};

/// One HTTP/1.1 request over TLS to the management listener.
async fn tls_get(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, String) {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let stream = tokio_rustls::TlsConnector::from(client)
        .connect(name, tcp)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut request = Request::get(path).header("host", "localhost");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = sender
        .send_request(request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn the_management_listener_serves_over_tls_only() {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("management-ca");
    let leaf = ca.server();
    let mut tls = TlsSettings::new(
        pki::write(dir.path(), "management.pem", &leaf.cert_pem),
        pki::write(dir.path(), "management.key", &leaf.key_pem),
    );
    tls.handshake_timeout_ms = 2_000;
    let mut cfg = config();
    cfg.management.tls = Some(tls);
    // The metrics poll below must not meet the management rate limit.
    cfg.management.rate_limit.enabled = false;
    let app = AlloyApp::new("management-tls")
        .router(Router::new().route("/hello", get(|| async { "hello" })));
    let server = start(app, cfg).await;

    let client = pki::client_config(&ca, None);
    let (status, body) = tls_get(server.management, Arc::clone(&client), "/livez", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = tls_get(server.management, Arc::clone(&client), "/metrics", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the token is still required"
    );
    // The reload task publishes the certificate's expiry once it starts.
    let expiry =
        "ferrum_alloy_tls_server_cert_not_after_timestamp_seconds{listener=\"management\"}";
    let mut metrics = String::new();
    for _ in 0..100 {
        let (status, body) = tls_get(
            server.management,
            Arc::clone(&client),
            "/metrics",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        metrics = body;
        if metrics.contains(expiry) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(metrics.contains(expiry), "{metrics}");

    // Plain HTTP gets no response from a TLS listener.
    let plain = tokio::time::timeout(
        Duration::from_secs(5),
        support::raw(
            server.management,
            b"GET /livez HTTP/1.1\r\nhost: localhost\r\n\r\n",
            Duration::from_secs(2),
        ),
    )
    .await
    .unwrap();
    assert!(
        !String::from_utf8_lossy(&plain).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&plain)
    );

    // The application listener stays plain HTTP.
    let reply = support::fetch(&server.url("/hello")).await;
    assert_eq!(reply.status, StatusCode::OK);
    server.lifecycle.trigger_shutdown();
}
