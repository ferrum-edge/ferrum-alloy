//! `AlloyParts::serve_on` applies the management access policy to the
//! address the management listener is actually bound to, whatever
//! `management.bind` says: off loopback it refuses to serve without a
//! management token, before either listener serves anything.
//! `AlloyParts::check_management_listener` applies the same policy for an
//! application that serves the management router itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use ferrum_alloy::config::{AlloyConfig, ConfigError, Secret};
use ferrum_alloy::{AlloyApp, AlloyError, AlloyParts, TelemetryInit};
use support::{TOKEN, fetch, fetch_with};
use tokio::net::TcpListener;

fn parts(config: AlloyConfig) -> AlloyParts {
    parts_with_check(config, false)
}

fn parts_with_check(mut config: AlloyConfig, private_failure: bool) -> AlloyParts {
    config.shutdown.drain_timeout_ms = 2_000;
    let router = Router::new().route("/hello", get(|| async { "hello" }));
    let app = AlloyApp::new("management-listener").router(router);
    let app = if private_failure {
        app.readiness_check("private-database-check", || async {
            Err(ferrum_alloy::health::CheckError::new(
                "private-dependency-error",
            ))
        })
    } else {
        app
    };
    #[cfg(feature = "openapi")]
    let app = app.openapi(&document());
    app.config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .shutdown_signal(std::future::pending())
        .into_parts()
        .unwrap()
}

#[cfg(feature = "openapi")]
fn document() -> ferrum_alloy::utoipa::openapi::OpenApi {
    ferrum_alloy::utoipa::openapi::OpenApiBuilder::new()
        .info(
            ferrum_alloy::utoipa::openapi::InfoBuilder::new()
                .title("management-listener")
                .version("1")
                .build(),
        )
        .build()
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
    let (app, _) = on_loopback().await;
    let (management, _) = on_every_interface().await;

    // It must fail at once; serving would run until shutdown, so returning
    // within the timeout shows that nothing was served.
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
    let paths: &[&str] = if cfg!(feature = "openapi") {
        &["/health", "/metrics", "/openapi.json"]
    } else {
        &["/health", "/metrics"]
    };
    for &path in paths {
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
async fn tokenless_loopback_exposes_only_minimal_probes() {
    // `management.bind` says port 9090; the listener's port does not
    // matter, only whether its address is loopback.
    #[allow(unused_mut, reason = "the UI needs its feature")]
    let mut config = AlloyConfig::default();
    #[cfg(feature = "openapi-ui")]
    {
        config.openapi.ui = true;
    }
    let parts = parts_with_check(config, true);
    let lifecycle = parts.lifecycle.clone();
    let (app, _) = on_loopback().await;
    let (management, management_addr) = on_loopback().await;
    assert_ne!(management_addr.port(), 9090);
    let task = tokio::spawn(parts.serve_on(app, Some(management)));

    let url = |path: &str| format!("http://{management_addr}{path}");
    for (path, status, label) in [("/livez", 200, "ok"), ("/readyz", 503, "not_ready")] {
        let reply = fetch(&url(path)).await;
        assert_eq!(reply.status, status, "{path}");
        assert_eq!(reply.headers["cache-control"], "no-store");
        assert_eq!(reply.json(), serde_json::json!({ "status": label }));
    }
    let paths: &[&str] = if cfg!(feature = "openapi") {
        &["/health", "/metrics", "/openapi.json"]
    } else {
        &["/health", "/metrics"]
    };
    for &path in paths {
        // This is exactly the transport a same-netns sidecar uses. Even a
        // spoofed operator header cannot authenticate it without a token.
        let reply = fetch_with(&url(path), &[("authorization", "Bearer attacker-token")]).await;
        assert_eq!(reply.status, 401, "{path}");
        assert_eq!(reply.headers["www-authenticate"], "Bearer");
        assert_eq!(reply.headers["cache-control"], "no-store");
        for private in [
            "management-listener",
            "private-database-check",
            "private-dependency-error",
            "active_connections",
            "openapi",
        ] {
            assert!(!reply.text().contains(private), "{path}: {private}");
        }
    }
    #[cfg(feature = "openapi-ui")]
    for path in ["/docs", "/docs/swagger-ui.css"] {
        assert_eq!(fetch(&url(path)).await.status, 401, "{path}");
    }

    lifecycle.trigger_shutdown();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("server stopped in time")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn authenticated_loopback_management_serves_private_details() {
    let mut config = AlloyConfig::default();
    config.management.token = Some(Secret::new(TOKEN));
    let parts = parts_with_check(config, true);
    let lifecycle = parts.lifecycle.clone();
    let (app, _) = on_loopback().await;
    let (management, addr) = on_loopback().await;
    let task = tokio::spawn(parts.serve_on(app, Some(management)));
    let url = format!("http://{addr}/health");
    assert_eq!(fetch(&url).await.status, 401);
    let bearer = format!("Bearer {TOKEN}");
    let reply = fetch_with(&url, &[("authorization", &bearer)]).await;
    assert_eq!(reply.status, 503, "the configured readiness check fails");
    assert!(reply.text().contains("private-database-check"));
    assert!(reply.text().contains("private-dependency-error"));
    assert!(reply.text().contains("management-listener"));
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

#[tokio::test]
async fn check_management_listener_applies_the_same_policy() {
    // For an application that serves `management_router` itself.
    let without_token = parts(AlloyConfig::default());
    let (off_loopback, _) = on_every_interface().await;
    match without_token.check_management_listener(&off_loopback) {
        Err(AlloyError::Config(ConfigError::Invalid(errors))) => {
            assert!(errors[0].contains("is not loopback"), "{errors:?}");
        }
        other => panic!("expected a configuration error, got {other:?}"),
    }
    let (loopback, _) = on_loopback().await;
    without_token.check_management_listener(&loopback).unwrap();

    // With a token, any address is accepted.
    let mut config = AlloyConfig::default();
    config.management.token = Some(Secret::new(TOKEN));
    let with_token = parts(config);
    with_token.check_management_listener(&off_loopback).unwrap();
    with_token.check_management_listener(&loopback).unwrap();
}
