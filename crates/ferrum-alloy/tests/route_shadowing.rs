//! Startup fails when one of Alloy's paths on the application listener would
//! silently shadow an application route, and finding out runs none of the
//! application's handlers, layers, or fallbacks.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use axum::routing::{get, post};
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::error::RouteConflict;
use ferrum_alloy::{AlloyApp, AlloyError, TelemetryInit};
use support::config;
use tower::{ServiceExt, service_fn};

fn app(router: Router) -> AlloyApp {
    AlloyApp::new("shadowing").router(router)
}

fn compose(app: AlloyApp, config: AlloyConfig) -> Result<(), AlloyError> {
    app.config(config)
        .telemetry(TelemetryInit::ApplicationOwned)
        .into_parts()
        .map(drop)
}

/// Asserts that composition failed with exactly `expected`, each as
/// `(route, path, setting)`, and that the message lists every one.
fn assert_shadowed(
    result: Result<(), AlloyError>,
    expected: &[(Option<&str>, &str, &'static str)],
) {
    let expected: Vec<RouteConflict> = expected
        .iter()
        .map(|&(route, path, setting)| RouteConflict {
            route: route.map(str::to_owned),
            path: path.to_owned(),
            setting,
        })
        .collect();
    match result {
        Err(AlloyError::ShadowedRoute { conflicts }) => {
            let message = AlloyError::ShadowedRoute {
                conflicts: conflicts.clone(),
            }
            .to_string();
            assert_eq!(conflicts, expected, "{message}");
            for conflict in &conflicts {
                assert!(message.contains(&conflict.to_string()), "{message}");
            }
        }
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

/// Wraps every endpoint of `router`, fallbacks included, in a layer that
/// counts the requests reaching the application.
fn counted(router: Router) -> (Router, Arc<AtomicUsize>) {
    count(router, false)
}

/// Like [`counted`], but added with `route_layer`, so only routes are
/// wrapped.
fn route_counted(router: Router) -> (Router, Arc<AtomicUsize>) {
    count(router, true)
}

fn count(router: Router, route_layer: bool) -> (Router, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let layer = from_fn(move |request: Request, next: Next| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            next.run(request).await
        }
    });
    let router = if route_layer {
        router.route_layer(layer)
    } else {
        router.layer(layer)
    };
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
    assert_shadowed(result, &[(Some("/docs"), "/docs", "openapi.ui_path")]);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[cfg(feature = "openapi-ui")]
#[tokio::test]
async fn a_route_beneath_the_public_ui_path_fails_startup() {
    // A parameter matches every asset, and each is reported.
    let router = Router::new().route("/docs/{file}", get(|| async { "mine" }));
    let result = compose(app(router).openapi(&document()), public_ui());
    let assets = [
        "/docs/swagger-ui.css",
        "/docs/swagger-ui-bundle.js",
        "/docs/swagger-ui-bundle.js.LICENSE.txt",
        "/docs/swagger-initializer.js",
    ];
    let expected: Vec<_> = assets
        .iter()
        .map(|&asset| (Some("/docs/{file}"), asset, "openapi.ui_path"))
        .collect();
    assert_shadowed(result, &expected);

    let mut cfg = public_ui();
    cfg.openapi.ui_path = "/api/docs".into();
    let router = Router::new().route("/api/docs/swagger-initializer.js", get(|| async { "x" }));
    let result = compose(app(router).openapi(&document()), cfg);
    assert_shadowed(
        result,
        &[(
            Some("/api/docs/swagger-initializer.js"),
            "/api/docs/swagger-initializer.js",
            "openapi.ui_path",
        )],
    );
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn a_route_at_the_public_document_path_fails_startup_for_any_method() {
    let mut cfg = config();
    cfg.openapi.public = true;
    let router = Router::new().route("/openapi.json", post(|| async { "mine" }));
    let result = compose(app(router).openapi(&document()), cfg);
    assert_shadowed(
        result,
        &[(Some("/openapi.json"), "/openapi.json", "openapi.path")],
    );
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
        &[(Some("/readyz"), "/readyz", "health.readiness_path")],
    );

    let mut cfg = config();
    cfg.health.app_endpoints = false;
    let router = Router::new().route("/readyz", get(|| async { "mine" }));
    compose(app(router), cfg).unwrap();
}

#[tokio::test]
async fn every_conflict_is_reported_at_once() {
    // A root parameter matches both health paths: GET /livez and GET /readyz
    // would never reach it, though it still serves every other value.
    let router = Router::new().route("/{code}", get(|| async { "mine" }));
    let result = compose(app(router), config());
    assert_shadowed(
        result,
        &[
            (Some("/{code}"), "/livez", "health.liveness_path"),
            (Some("/{code}"), "/readyz", "health.readiness_path"),
        ],
    );

    // Moving the health paths under a prefix resolves it.
    let mut cfg = config();
    cfg.health.liveness_path = "/_alloy/livez".into();
    cfg.health.readiness_path = "/_alloy/readyz".into();
    let router = Router::new().route("/{code}", get(|| async { "mine" }));
    compose(app(router), cfg).unwrap();
}

#[tokio::test]
async fn the_error_names_the_path_and_the_route_it_would_never_reach() {
    let router = Router::new()
        .route("/livez", get(|| async { "mine" }))
        .nest_service("/svc", get(|| async { "service" }));
    let mut cfg = config();
    cfg.health.readiness_path = "/svc/readyz".into();
    let message = compose(app(router), cfg).unwrap_err().to_string();
    assert_eq!(
        message,
        "Alloy serves paths on the application listener ahead of the application's router: \
         requests for /livez would never reach application route /livez \
         (health.liveness_path); requests for /svc/readyz would never reach a nest_service \
         whose prefix contains /svc/readyz (health.readiness_path); change the settings \
         named or the routes"
    );
}

#[tokio::test]
async fn a_route_in_a_nested_router_is_detected_without_running_it() {
    let nested = Router::new().route("/livez", get(|| async { "mine" }));
    let (router, calls) = counted(Router::new().nest("/api", nested));
    let mut cfg = config();
    cfg.health.liveness_path = "/api/livez".into();
    assert_shadowed(
        compose(app(router), cfg),
        &[(Some("/api/livez"), "/api/livez", "health.liveness_path")],
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[tokio::test]
async fn a_wildcard_beneath_the_root_is_detected_without_running_it() {
    let router = Router::new().route("/docs/{*rest}", get(|| async { "mine" }));
    let (router, calls) = counted(router);
    let mut cfg = config();
    cfg.health.liveness_path = "/docs/health/livez".into();
    assert_shadowed(
        compose(app(router), cfg),
        &[(
            Some("/docs/{*rest}"),
            "/docs/health/livez",
            "health.liveness_path",
        )],
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[tokio::test]
async fn a_route_service_is_detected_without_running_it() {
    let service = service_fn(|_: Request| async { Ok::<_, Infallible>("mine") });
    let (router, calls) = counted(Router::new().route_service("/livez", service));
    assert_shadowed(
        compose(app(router), config()),
        &[(Some("/livez"), "/livez", "health.liveness_path")],
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}

#[tokio::test]
async fn route_layer_middleware_does_not_run() {
    let method_router = post(|| async { "mine" }).fallback(|| async { "method fallback" });
    let router = Router::new()
        .route("/livez", method_router)
        .route("/orders", get(|| async { "orders" }));
    let (router, calls) = route_counted(router);
    assert_shadowed(
        compose(app(router.clone()), config()),
        &[(Some("/livez"), "/livez", "health.liveness_path")],
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");

    // The counting layer does see the route when it is actually called.
    let request = http::Request::get("/orders").body(Body::empty());
    router.oneshot(request.unwrap()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_method_fallback_is_detected_without_running_it() {
    let method_router = post(|| async { "mine" }).fallback(|| async { "method fallback" });
    let (router, calls) = counted(Router::new().route("/livez", method_router));
    assert_shadowed(
        compose(app(router), config()),
        &[(Some("/livez"), "/livez", "health.liveness_path")],
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
        &[(None, "/svc/livez", "health.liveness_path")],
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
    let (router, calls) = counted(Router::new().route("/{*path}", get(|| async { "spa" })));
    compose(app(router), config()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no application code ran");
}
