//! Startup fails when one of Alloy's paths on the application listener would
//! silently shadow an application route, and finding out runs none of the
//! application's handlers, layers, or fallbacks.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use axum::routing::{get, post};
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::{AlloyApp, AlloyError, TelemetryInit};
use support::config;
use tower::ServiceExt;

fn app(router: Router) -> AlloyApp {
    AlloyApp::new("shadowing").router(router)
}

fn compose(app: AlloyApp, config: AlloyConfig) -> Result<(), AlloyError> {
    app.config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .map(drop)
}

fn assert_shadowed(result: Result<(), AlloyError>, route: &str, path: &str, setting: &str) {
    let expected = format!(
        "application route {route} matches {path}, which Alloy serves on the application listener ({setting})"
    );
    match result {
        Err(error @ AlloyError::ShadowedRoute { .. }) => {
            let message = error.to_string();
            assert!(message.starts_with(&expected), "{message}");
        }
        other => panic!("expected {expected}, got {other:?}"),
    }
}

/// Wraps every endpoint of `router`, fallbacks included, in a layer that
/// counts the requests reaching the application.
fn counted(router: Router) -> (Router, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let router = router.layer(from_fn(move |request: Request, next: Next| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            next.run(request).await
        }
    }));
    (router, calls)
}

#[cfg(feature = "openapi")]
fn document() -> ferrum_alloy::utoipa::openapi::OpenApi {
    ferrum_alloy::utoipa::openapi::OpenApiBuilder::new()
        .info(
            ferrum_alloy::utoipa::openapi::InfoBuilder::new()
                .title("shadowing")
                .version("1")
                .build(),
        )
        .build()
}

#[cfg(feature = "openapi-ui")]
fn public_ui() -> AlloyConfig {
    let mut cfg = config();
    cfg.openapi.ui = true;
    cfg.openapi.public = true;
    cfg
}

#[cfg(feature = "openapi-ui")]
#[tokio::test]
async fn a_route_at_the_public_ui_path_fails_startup() {
    let (router, calls) = counted(Router::new().route("/docs", get(|| async { "mine" })));
    let result = compose(app(router).openapi(&document()), public_ui());
    assert_shadowed(result, "/docs", "/docs", "openapi.ui_path");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[cfg(feature = "openapi-ui")]
#[tokio::test]
async fn a_route_beneath_the_public_ui_path_fails_startup() {
    let router = Router::new().route("/docs/{file}", get(|| async { "mine" }));
    let result = compose(app(router).openapi(&document()), public_ui());
    assert_shadowed(
        result,
        "/docs/{file}",
        "/docs/swagger-ui.css",
        "openapi.ui_path",
    );

    let mut cfg = public_ui();
    cfg.openapi.ui_path = "/api/docs".into();
    let router = Router::new().route("/api/docs/swagger-initializer.js", get(|| async { "x" }));
    let result = compose(app(router).openapi(&document()), cfg);
    assert_shadowed(
        result,
        "/api/docs/swagger-initializer.js",
        "/api/docs/swagger-initializer.js",
        "openapi.ui_path",
    );
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn a_route_at_the_public_document_path_fails_startup_for_any_method() {
    let mut cfg = config();
    cfg.openapi.public = true;
    let router = Router::new().route("/openapi.json", post(|| async { "mine" }));
    let result = compose(app(router).openapi(&document()), cfg);
    assert_shadowed(result, "/openapi.json", "/openapi.json", "openapi.path");
}

#[cfg(feature = "openapi-ui")]
#[tokio::test]
async fn docs_served_only_on_the_management_listener_shadow_nothing() {
    let mut cfg = public_ui();
    cfg.openapi.public = false;
    let router = Router::new()
        .route("/docs", get(|| async { "mine" }))
        .route("/openapi.json", get(|| async { "mine" }));
    compose(app(router).openapi(&document()), cfg).unwrap();

    // Without a registered document, nothing is served publicly either.
    let router = Router::new().route("/docs", get(|| async { "mine" }));
    compose(app(router), public_ui()).unwrap();
}

#[tokio::test]
async fn a_route_at_a_health_path_fails_startup() {
    let router = Router::new().route("/readyz", get(|| async { "mine" }));
    assert_shadowed(
        compose(app(router), config()),
        "/readyz",
        "/readyz",
        "health.readiness_path",
    );

    // A parameter matches too: GET /livez would never reach it.
    let router = Router::new().route("/{code}", get(|| async { "mine" }));
    assert_shadowed(
        compose(app(router), config()),
        "/{code}",
        "/livez",
        "health.liveness_path",
    );

    let mut cfg = config();
    cfg.health.app_endpoints = false;
    let router = Router::new().route("/readyz", get(|| async { "mine" }));
    compose(app(router), cfg).unwrap();
}

#[tokio::test]
async fn a_method_fallback_is_detected_without_running_it() {
    let method_router = post(|| async { "mine" }).fallback(|| async { "method fallback" });
    let (router, calls) = counted(Router::new().route("/livez", method_router));
    assert_shadowed(
        compose(app(router), config()),
        "/livez",
        "/livez",
        "health.liveness_path",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[tokio::test]
async fn a_nested_service_is_detected_without_running_it() {
    let router = Router::new().nest_service("/svc", get(|| async { "service" }));
    let (router, calls) = counted(router);
    let mut cfg = config();
    cfg.health.liveness_path = "/svc/livez".into();
    assert_shadowed(
        compose(app(router), cfg),
        "<nested service>",
        "/svc/livez",
        "health.liveness_path",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[tokio::test]
async fn fallbacks_and_a_root_catch_all_are_not_shadowed_and_do_not_run() {
    let nested = Router::new()
        .route("/items", get(|| async { "items" }))
        .fallback(|| async { "nested fallback" });
    let router = Router::new()
        .route("/orders", get(|| async { "orders" }))
        .nest("/api", nested)
        .fallback(|| async { "fallback" });
    let (router, calls) = counted(router);
    let mut cfg = config();
    cfg.health.liveness_path = "/api/livez".into();
    compose(app(router.clone()), cfg).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");

    // The counting layer does see fallbacks when they are actually called.
    let request = http::Request::get("/api/livez").body(Body::empty());
    router.oneshot(request.unwrap()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A root catch-all matches every path, like a fallback.
    let router = Router::new().route("/{*path}", get(|| async { "spa" }));
    compose(app(router), config()).unwrap();
}
