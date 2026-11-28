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

/// One HTTP/1.1 request over TLS to the management listener, returning status,
/// response headers, and body.
async fn tls_get_with_headers(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, http::HeaderMap, String) {
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
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

/// One HTTP/1.1 request over TLS to the management listener.
async fn tls_get(
    addr: SocketAddr,
    client: Arc<rustls::ClientConfig>,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, String) {
    let (status, _, body) = tls_get_with_headers(addr, client, path, token).await;
    (status, body)
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

/// Off loopback, Alloy's own listener serves retrieval over the management
/// TLS it loaded, even after the settings are edited, and plain HTTP gets
/// no answer; the preflight for a listener the application would serve
/// itself refuses the same address.
#[cfg(feature = "diagnostics")]
#[tokio::test]
async fn retrieval_is_served_off_loopback_over_the_loaded_management_tls() {
    use ferrum_alloy::TelemetryInit;
    use ferrum_alloy::diagnostics::{DiagnosticsAccess, DiagnosticsRequest, TenantTag};

    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("management-ca");
    let leaf = ca.server();
    let mut tls = TlsSettings::new(
        pki::write(dir.path(), "management.pem", &leaf.cert_pem),
        pki::write(dir.path(), "management.key", &leaf.key_pem),
    );
    tls.handshake_timeout_ms = 2_000;
    let mut cfg = config();
    cfg.management.bind = "0.0.0.0:9090".parse().unwrap();
    cfg.management.tls = Some(tls);
    let mut parts = AlloyApp::new("management-tls-retrieval")
        .router(Router::new().route(
            "/hello",
            get(|tag: TenantTag| async move {
                assert!(tag.set("tenant-a"));
                "hello"
            }),
        ))
        .diagnostics_authorizer(|_: DiagnosticsRequest| async {
            DiagnosticsAccess::tenant("tenant-a")
        })
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .shutdown_signal(std::future::pending())
        .into_parts()
        .unwrap();

    let off_loopback = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = parts
        .check_management_listener(&off_loopback)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("applies only to a listener Alloy serves itself"),
        "{error}"
    );

    // The loaded certificate decides, not the editable settings.
    parts.config.management.tls = None;
    let addr = SocketAddr::from(([127, 0, 0, 1], off_loopback.local_addr().unwrap().port()));
    let app = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_addr = app.local_addr().unwrap();
    let lifecycle = parts.lifecycle.clone();
    let task = tokio::spawn(parts.serve_on(app, Some(off_loopback)));

    // A tenant-tagged request on the application listener; with a
    // credential it is not shared-cacheable, so its generated id is echoed.
    let reply = support::fetch_with(
        &format!("http://{app_addr}/hello"),
        &[("authorization", "Bearer app-user")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let request_id = reply.headers["x-request-id"].to_str().unwrap().to_owned();

    // Its report, served by the retrieval route over the wildcard-bound
    // management TLS listener (the router's fallback would answer 404).
    let client = pki::client_config(&ca, None);
    let path = format!("/diagnostics/v1/requests/{request_id}");
    let (status, body) = tls_get(addr, Arc::clone(&client), &path, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ferrum.diagnostic_report"), "{body}");
    assert!(body.contains(&request_id), "{body}");
    let (status, headers, body) =
        tls_get_with_headers(addr, client, "/diagnostics/v1/requests/req-unknown", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("route-not-found"), "{body}");
    assert_eq!(
        headers
            .get(http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "diagnostic retrieval handler sets Cache-Control: no-store (unlike the generic fallback)"
    );

    let plain = tokio::time::timeout(
        Duration::from_secs(5),
        support::raw(
            addr,
            b"GET /diagnostics/v1/requests/req-unknown HTTP/1.1\r\nhost: localhost\r\n\r\n",
            Duration::from_secs(2),
        ),
    )
    .await
    .unwrap();
    assert!(
        !String::from_utf8_lossy(&plain).starts_with("HTTP/"),
        "{}",
        String::from_utf8_lossy(&plain)
    );
    lifecycle.trigger_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}

/// The TLS load at startup warns about a private key others could replace,
/// from the key file it read, for each listener; a key only its owner can
/// write is not reported.
#[cfg(unix)]
#[test]
fn startup_warns_about_a_private_key_others_could_replace() {
    use std::os::unix::fs::PermissionsExt as _;

    use ferrum_alloy::TelemetryInit;

    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("key-warning-ca");
    let leaf = ca.server();
    let cert = pki::write(dir.path(), "server.pem", &leaf.cert_pem);
    let key = pki::write(dir.path(), "server.key", &leaf.key_pem);
    let warnings = |mode: u32| {
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
        let mut cfg = config();
        cfg.server.tls = Some(TlsSettings::new(cert.clone(), key.clone()));
        cfg.management.tls = Some(TlsSettings::new(cert.clone(), key.clone()));
        let parts = AlloyApp::new("key-warning")
            .router(Router::new())
            .config(cfg)
            .telemetry(TelemetryInit::ApplicationOwned)
            .into_parts()
            .unwrap();
        parts
            .warnings
            .iter()
            .map(|issue| issue.message.clone())
            .filter(|message| message.contains("key_path"))
            .collect::<Vec<_>>()
    };
    assert_eq!(warnings(0o600), Vec::<String>::new());
    let found = warnings(0o620);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found[0].starts_with("server.tls.key_path: secret file "),
        "{found:?}"
    );
    assert!(
        found[1].starts_with("management.tls.key_path: secret file "),
        "{found:?}"
    );
    assert!(found.iter().all(|w| w.contains("(mode 620)")), "{found:?}");
}

/// Messages of the events logged on this thread.
#[derive(Clone, Default)]
struct Messages(Arc<std::sync::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Messages {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push(message.0);
    }
}

/// With `management.tls` set, validation does not warn about a cleartext
/// management token off loopback; the preflight for a listener the
/// application serves itself, which gets none of that TLS, does.
#[tokio::test(flavor = "current_thread")]
async fn a_custom_listener_off_loopback_is_warned_about_as_cleartext() {
    use ferrum_alloy::TelemetryInit;
    use tracing_subscriber::layer::SubscriberExt as _;

    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("custom-listener-ca");
    let leaf = ca.server();
    let mut cfg = config();
    cfg.management.bind = "0.0.0.0:9090".parse().unwrap();
    cfg.management.tls = Some(TlsSettings::new(
        pki::write(dir.path(), "management.pem", &leaf.cert_pem),
        pki::write(dir.path(), "management.key", &leaf.key_pem),
    ));
    let parts = AlloyApp::new("custom-listener")
        .router(Router::new())
        .config(cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap();
    assert!(
        parts
            .warnings
            .iter()
            .all(|w| !w.message.contains("cleartext")),
        "{:?}",
        parts.warnings
    );

    let messages = Messages::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(messages.clone()));
    let off_loopback = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    parts.check_management_listener(&off_loopback).unwrap();
    let loopback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    parts.check_management_listener(&loopback).unwrap();
    let logged = messages.0.lock().unwrap().clone();
    let warned: Vec<&String> = logged
        .iter()
        .filter(|m| m.contains("served as plain TCP") && m.contains("cleartext"))
        .collect();
    assert_eq!(
        warned.len(),
        1,
        "only the listener off loopback: {logged:?}"
    );

    // Default loopback `management.bind` with `management.tls` unset also warns
    // when a custom off-loopback listener is checked.
    let mut plain_cfg = config();
    plain_cfg.management.bind = "127.0.0.1:9090".parse().unwrap();
    plain_cfg.management.tls = None;
    let plain_parts = AlloyApp::new("custom-listener-plain")
        .router(Router::new())
        .config(plain_cfg)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap();
    messages.0.lock().unwrap().clear();
    plain_parts
        .check_management_listener(&off_loopback)
        .unwrap();
    plain_parts.check_management_listener(&loopback).unwrap();
    let plain_logged = messages.0.lock().unwrap().clone();
    let plain_warned: Vec<&String> = plain_logged
        .iter()
        .filter(|m| m.contains("served as plain TCP") && m.contains("cleartext"))
        .collect();
    assert_eq!(
        plain_warned.len(),
        1,
        "only the listener off loopback without management.tls: {plain_logged:?}"
    );
}
