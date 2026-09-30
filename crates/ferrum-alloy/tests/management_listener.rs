//! `AlloyParts::serve_on` applies the management access policy to the
//! address the management listener is actually bound to, whatever
//! `management.bind` says: off loopback it refuses to serve without a
//! management token, before either listener serves anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use ferrum_alloy::config::{AlloyConfig, ConfigError, Secret};
use ferrum_alloy::{AlloyApp, AlloyError, AlloyParts, TelemetryInit};
use support::{TOKEN, fetch, fetch_with};
use tokio::net::{TcpListener, TcpStream};

fn parts(mut config: AlloyConfig) -> AlloyParts {
    config.shutdown.drain_timeout_ms = 2_000;
    let router = Router::new().route("/hello", get(|| async { "hello" }));
    AlloyApp::new("management-listener")
        .router(router)
        .config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .shutdown_signal(std::future::pending())
        .into_parts()
        .unwrap()
}

/// A listener on every IPv4 interface, and the loopback address that
/// reaches it.
async fn on_every_interface() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, SocketAddr::from(([127, 0, 0, 1], port)))
}

async fn on_loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[tokio::test]
async fn a_listener_off_loopback_without_a_token_is_refused_before_serving() {
    // The defaults: a loopback `management.bind` and no token, which
    // validation accepts.
    let config = AlloyConfig::default();
    assert!(config.management.bind.ip().is_loopback());
    assert!(config.management.token.is_none());
    let parts = parts(config);
    let (app, app_addr) = on_loopback().await;
    let (management, management_addr) = on_every_interface().await;

    // It must fail at once; serving would run until shutdown.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        parts.serve_on(app, Some(management)),
    )
    .await
    .expect("serve_on refused the listener instead of serving");
    match result {
        Err(AlloyError::Config(ConfigError::Invalid(errors))) => {
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(errors[0].contains("is not loopback"), "{errors:?}");
            assert!(errors[0].contains("management.token"), "{errors:?}");
        }
        other => panic!("expected a configuration error, got {other:?}"),
    }
    // Both listeners were dropped without serving.
    for addr in [app_addr, management_addr] {
        assert!(TcpStream::connect(addr).await.is_err(), "{addr} is closed");
    }
}

#[tokio::test]
async fn a_listener_off_loopback_with_a_token_requires_it() {
    let mut config = AlloyConfig::default();
    config.management.token = Some(Secret::new(TOKEN));
    let parts = parts(config);
    let lifecycle = parts.lifecycle.clone();
    let (app, _) = on_loopback().await;
    let (management, management_addr) = on_every_interface().await;
    let task = tokio::spawn(parts.serve_on(app, Some(management)));

    let url = |path: &str| format!("http://{management_addr}{path}");
    assert_eq!(fetch(&url("/livez")).await.status, 200);
    for path in ["/health", "/metrics"] {
        let reply = fetch(&url(path)).await;
        assert_eq!(reply.status, 401, "{path}");
        let bearer = format!("Bearer {TOKEN}");
        let reply = fetch_with(&url(path), &[("authorization", &bearer)]).await;
        assert_eq!(reply.status, 200, "{path}");
    }

    lifecycle.trigger_shutdown();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("server stopped in time")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_loopback_listener_on_another_port_serves_without_a_token() {
    // `management.bind` says port 9090; the listener's port does not
    // matter, only whether its address is loopback.
    let parts = parts(AlloyConfig::default());
    let lifecycle = parts.lifecycle.clone();
    let (app, _) = on_loopback().await;
    let (management, management_addr) = on_loopback().await;
    assert_ne!(management_addr.port(), 9090);
    let task = tokio::spawn(parts.serve_on(app, Some(management)));

    let reply = fetch(&format!("http://{management_addr}/health")).await;
    assert_eq!(reply.status, 200);

    lifecycle.trigger_shutdown();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("server stopped in time")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn an_application_listener_alone_is_not_checked() {
    // Without a management listener there is nothing to protect.
    let parts = parts(AlloyConfig::default());
    let lifecycle = parts.lifecycle.clone();
    let (app, app_addr) = on_every_interface().await;
    let task = tokio::spawn(parts.serve_on(app, None));

    let reply = fetch(&format!("http://{app_addr}/hello")).await;
    assert_eq!(reply.status, 200);

    lifecycle.trigger_shutdown();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("server stopped in time")
        .unwrap()
        .unwrap();
}
