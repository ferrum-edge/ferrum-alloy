//! Existing-Axum adoption: application-owned routers, state, middleware,
//! route templates after matching, early rejections, and duplicate layers.

#![cfg(feature = "axum")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ferrum_alloy_telemetry::{
    AcceptPolicy, RecordRouteLayer, RequestContext, TelemetryConfig, TelemetryLayer,
};
use http_body_util::BodyExt;
use tower::{Layer, ServiceExt, service_fn};

#[derive(Clone)]
struct AppState {
    hits: Arc<AtomicUsize>,
}

async fn get_order(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    context: RequestContext,
) -> String {
    state.hits.fetch_add(1, Ordering::Relaxed);
    format!("{id}:{}", context.request_id)
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/orders/{id}", get(get_order))
        .with_state(state)
        // Application-owned middleware keeps working.
        .layer(middleware::from_fn(
            |request: Request<Body>, next: Next| async move {
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .insert("x-app-middleware", "ran".parse().unwrap());
                response
            },
        ))
        .layer(RecordRouteLayer)
}

async fn send(
    service: impl tower::Service<
        Request<Body>,
        Response = Response<
            impl http_body::Body<Data = bytes::Bytes, Error = impl std::fmt::Debug>,
        >,
        Error = Infallible,
    >,
    uri: &str,
    method: &str,
) -> (StatusCode, String, http::HeaderMap) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-request-id", "req-1")
        .body(Body::empty())
        .unwrap();
    let response = service.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap(), headers)
}

/// A layer that keeps every caller's request id, so handlers see the id the
/// test chose. By default only trusted peers' ids are kept.
fn keeping_request_ids() -> TelemetryLayer {
    let mut config = TelemetryConfig::default();
    config.request_id.accept_incoming = AcceptPolicy::Any;
    TelemetryLayer::new(config).unwrap()
}

#[tokio::test]
async fn matched_unmatched_and_method_not_allowed_routes_use_bounded_labels() {
    let state = AppState {
        hits: Arc::new(AtomicUsize::new(0)),
    };
    let telemetry = keeping_request_ids();
    let metrics = telemetry.metrics();
    let service = telemetry.layer(app(state.clone()));

    let (status, body, headers) = send(service.clone(), "/orders/42", "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "42:req-1", "handler sees the request context");
    assert_eq!(headers["x-app-middleware"], "ran");
    assert!(
        !headers.contains_key("x-request-id"),
        "a shared cache may store a bare GET 200, so the id is not echoed"
    );
    assert_eq!(metrics.request_count("GET", "/orders/{id}", 200), 1);

    for path in ["/orders/43", "/orders/44"] {
        send(service.clone(), path, "GET").await;
    }
    assert_eq!(
        metrics.request_count("GET", "/orders/{id}", 200),
        3,
        "raw paths never become labels"
    );

    let (status, _, _) = send(service.clone(), "/does/not/exist", "GET").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(metrics.request_count("GET", "__unmatched__", 404), 1);

    let (status, _, headers) = send(service.clone(), "/orders/42", "DELETE").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["x-request-id"], "req-1", "DELETE is not storable");
    assert_eq!(metrics.request_count("DELETE", "/orders/{id}", 405), 1);

    let (_, _, _) = send(service.clone(), "/x", "BREW").await;
    assert_eq!(metrics.request_count("_OTHER", "__unmatched__", 404), 1);
    assert_eq!(state.hits.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn early_rejections_before_routing_are_measured() {
    let telemetry = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let metrics = telemetry.metrics();
    // An admission guard between the telemetry layer and the router.
    let guard = tower::layer::layer_fn(|inner: Router| {
        service_fn(move |request: Request<Body>| {
            let inner = inner.clone();
            async move {
                if request.headers().contains_key("x-shed") {
                    return Ok::<_, Infallible>(StatusCode::SERVICE_UNAVAILABLE.into_response());
                }
                inner.oneshot(request).await
            }
        })
    });
    let state = AppState {
        hits: Arc::new(AtomicUsize::new(0)),
    };
    let service = telemetry.layer(guard.layer(app(state)));
    let request = Request::builder()
        .uri("/orders/1")
        .header("x-shed", "1")
        .body(Body::empty())
        .unwrap();
    let response = service.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(response);
    assert_eq!(metrics.request_count("GET", "__not_routed__", 503), 1);
}

#[tokio::test]
async fn telemetry_applied_with_router_layer_reads_the_matched_route() {
    let telemetry = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let metrics = telemetry.metrics();
    let router = Router::new()
        .route("/items/{id}", get(|| async { "ok" }))
        .layer(telemetry);
    let (status, _, _) = send(router, "/items/7", "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(metrics.request_count("GET", "/items/{id}", 200), 1);
}

#[tokio::test]
async fn duplicate_layers_do_not_double_count() {
    let outer = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let inner = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let outer_metrics = outer.metrics();
    let inner_metrics = inner.metrics();
    let state = AppState {
        hits: Arc::new(AtomicUsize::new(0)),
    };
    let service = outer.layer(inner.layer(app(state)));
    let (status, _, _) = send(service, "/orders/5", "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(outer_metrics.request_count("GET", "/orders/{id}", 200), 1);
    assert_eq!(inner_metrics.request_count("GET", "/orders/{id}", 200), 0);
    assert_eq!(
        inner_metrics
            .duplicate_instrumentation
            .load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn concurrent_requests_never_share_context() {
    let telemetry = keeping_request_ids();
    let metrics = telemetry.metrics();
    let router = Router::new().route(
        "/echo/{n}",
        get(|Path(n): Path<u32>, context: RequestContext| async move {
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(n % 5))).await;
            format!("{}|{}", context.request_id, context.trace_id)
        }),
    );
    let service = telemetry.layer(router.layer(RecordRouteLayer));
    let mut tasks = Vec::new();
    for n in 0..100u32 {
        let service = service.clone();
        tasks.push(tokio::spawn(async move {
            let request = Request::builder()
                .uri(format!("/echo/{n}"))
                .header("x-request-id", format!("req-{n}"))
                .body(Body::empty())
                .unwrap();
            let response = service.oneshot(request).await.unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (n, String::from_utf8(body.to_vec()).unwrap())
        }));
    }
    let mut trace_ids = std::collections::HashSet::new();
    for task in tasks {
        let (n, body) = task.await.unwrap();
        let (request_id, trace_id) = body.split_once('|').unwrap();
        assert_eq!(request_id, format!("req-{n}"));
        assert!(
            trace_ids.insert(trace_id.to_owned()),
            "trace ids must be unique per root request"
        );
    }
    assert_eq!(metrics.request_count("GET", "/echo/{n}", 200), 100);
    assert_eq!(metrics.in_flight(), 0);
}

#[tokio::test]
async fn metric_series_are_capped() {
    let telemetry = TelemetryLayer::new(TelemetryConfig::default())
        .unwrap()
        .with_metrics(Arc::new(ferrum_alloy_telemetry::Metrics::with_max_series(
            2,
        )));
    let metrics = telemetry.metrics();
    let router = Router::new()
        .route("/a", get(|| async { "a" }))
        .route("/b", get(|| async { "b" }))
        .route("/c", get(|| async { "c" }))
        .layer(RecordRouteLayer);
    let service = telemetry.layer(router);
    for path in ["/a", "/b", "/c", "/c"] {
        send(service.clone(), path, "GET").await;
    }
    assert_eq!(metrics.series_len(), 2);
    assert_eq!(metrics.overflow_count(), 2);
    let text = metrics.render_prometheus();
    assert!(text.contains("http_route=\"__overflow__\""));
}

#[test]
fn prometheus_rendering_escapes_labels() {
    let metrics = ferrum_alloy_telemetry::Metrics::default();
    let text = metrics.render_prometheus();
    assert!(text.contains("# TYPE http_server_request_duration_seconds histogram"));
    assert!(text.contains("ferrum_alloy_duplicate_instrumentation_total 0"));
}
