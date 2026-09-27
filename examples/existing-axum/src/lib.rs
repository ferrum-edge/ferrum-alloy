//! The application, factored as a library so tests can exercise it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::header::CACHE_CONTROL;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ferrum_alloy_telemetry::{
    Metrics, RecordRouteLayer, RequestContext, TelemetryConfig, TelemetryConfigError,
    TelemetryLayer,
};
use tower::Layer;

/// The application's own state.
#[derive(Clone, Default)]
pub struct AppState {
    /// A counter owned by the application.
    pub visits: Arc<AtomicU64>,
}

/// The application's own middleware, unchanged by Alloy.
async fn server_header(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "x-powered-by",
        axum::http::HeaderValue::from_static("existing-app"),
    );
    response
}

async fn visit(
    State(state): State<AppState>,
    Path(name): Path<String>,
    context: RequestContext,
) -> impl IntoResponse {
    let count = state.visits.fetch_add(1, Ordering::Relaxed) + 1;
    // The body is specific to this request, so shared caches must not store
    // it. Marking it `private` also lets Alloy echo the request id, which it
    // withholds from responses a shared cache may store.
    (
        [(CACHE_CONTROL, "private")],
        format!(
            "hello {name} (visit {count}, request {})",
            context.request_id
        ),
    )
}

/// Builds the router and wraps it with Alloy telemetry.
///
/// `RecordRouteLayer` goes on the router (it runs after matching and records
/// the route template); `TelemetryLayer` wraps the whole router so early
/// rejections by outer middleware are measured too.
pub fn app() -> Result<(Router, Arc<Metrics>), TelemetryConfigError> {
    let router = Router::new()
        .route("/hello/{name}", get(visit))
        .with_state(AppState::default())
        .layer(middleware::from_fn(server_header))
        .layer(RecordRouteLayer);
    let telemetry = TelemetryLayer::new(TelemetryConfig::default())?;
    let metrics = telemetry.metrics();
    // `Router::layer` would also work, but would only see requests that
    // reach a route. Wrapping the router as a service sees everything.
    let service = Router::new().fallback_service(telemetry.layer(router));
    Ok((service, metrics))
}
