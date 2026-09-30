//! The management listener: health, metrics, and published documents.
//!
//! It binds to loopback by default. Detailed health, metrics, and the
//! OpenAPI document require `Authorization: Bearer <management.token>` when a
//! token is configured; configuration validation refuses a non-loopback
//! management bind without a token, and `AlloyParts::serve_on` a management
//! listener actually bound off loopback without one. Every response is
//! `no-store`.
//!
//! Requests are rate-limited before any handler or token check runs: per
//! client, and except for the probes, which have a budget of their own, per
//! listener (see [`crate::rate_limit`]).
//!
//! With the `openapi-ui` feature and `openapi.ui`, the documentation UI is
//! served beside the OpenAPI document and requires the same token.
//!
//! With the `diagnostics` feature and an installed authorizer, it also serves
//! `GET /diagnostics/v1/requests/{request_id}`, which the authorizer rather
//! than the management token guards (see `crate::diagnostics`).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderValue, WWW_AUTHENTICATE};

use crate::config::Secret;
use crate::health::{self, Readiness};
use crate::lifecycle::Lifecycle;
use crate::problem::{Problem, ProblemKind};
use crate::rate_limit::{self, RateLimiter};
use crate::server::ServerStats;

/// Liveness probe path of the management listener.
const LIVENESS_PATH: &str = "/livez";

/// Readiness probe path of the management listener.
const READINESS_PATH: &str = "/readyz";

/// Detailed health path of the management listener.
const HEALTH_PATH: &str = "/health";

/// Metrics path of the management listener.
const METRICS_PATH: &str = "/metrics";

/// Probe paths, rate-limited separately from the rest of the listener.
pub(crate) const PROBE_PATHS: [&str; 2] = [LIVENESS_PATH, READINESS_PATH];

/// Paths the management listener always serves, whatever the configuration.
pub(crate) const FIXED_PATHS: [&str; 4] =
    [LIVENESS_PATH, READINESS_PATH, HEALTH_PATH, METRICS_PATH];

#[derive(Clone)]
pub(crate) struct ManagementState {
    pub(crate) readiness: Arc<Readiness>,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) token: Option<Secret>,
    pub(crate) service: String,
    pub(crate) version: Option<String>,
    pub(crate) app_stats: Arc<ServerStats>,
    pub(crate) openapi: Option<Arc<Vec<u8>>>,
    #[cfg(feature = "openapi-ui")]
    pub(crate) openapi_ui: Option<crate::openapi_ui::DocsUi>,
    pub(crate) rate_limiter: Option<Arc<RateLimiter>>,
    #[cfg(feature = "diagnostics")]
    pub(crate) diagnostics: Option<crate::diagnostics::Retrieval>,
}

/// Constant-time comparison of equal-length byte strings. Length differences
/// return early; token length is not treated as secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Applies the rule that validation applies to `management.bind` to `addr`,
/// the address a management listener is actually bound to: off loopback, a
/// token is required. `token` says whether one is configured.
pub(crate) fn check_listener(addr: SocketAddr, token: bool) -> Result<(), String> {
    if token || addr.ip().is_loopback() {
        return Ok(());
    }
    Err(format!(
        "the management listener is bound to {addr}, which is not loopback; set management.token (FERRUM_ALLOY_MANAGEMENT_TOKEN) or bind it to loopback"
    ))
}

pub(crate) fn authorized(headers: &HeaderMap, token: Option<&Secret>) -> bool {
    let Some(token) = token else {
        // Without a token every request is admitted. Validation refuses a
        // non-loopback `management.bind`, and `AlloyParts::serve` and
        // `serve_on` refuse a listener actually bound off loopback
        // (`check_listener`). An application serving `management_router`
        // itself must call `AlloyParts::check_management_listener`; nothing
        // here can see where the router is served.
        return true;
    };
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .is_some_and(|(_, presented)| {
            constant_time_eq(presented.trim().as_bytes(), token.expose().as_bytes())
        })
}

fn unauthorized() -> Response {
    Problem::new(ProblemKind::Unauthorized)
        .with_detail("A valid management bearer token is required.")
        .with_header(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))
        .into_response()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(crate) fn router(state: ManagementState, openapi_path: &str) -> Router {
    let mut router = Router::new()
        .route(LIVENESS_PATH, get(|| async { health::liveness() }))
        .route(
            READINESS_PATH,
            get(|State(state): State<ManagementState>| async move {
                health::readiness(&state.readiness, &state.lifecycle).await
            }),
        )
        .route(
            HEALTH_PATH,
            get(
                |State(state): State<ManagementState>, headers: HeaderMap| async move {
                    if !authorized(&headers, state.token.as_ref()) {
                        return unauthorized();
                    }
                    health::detailed(
                        &state.readiness,
                        &state.lifecycle,
                        &state.service,
                        state.version.as_deref(),
                    )
                    .await
                },
            ),
        )
        .route(
            METRICS_PATH,
            get(
                |State(state): State<ManagementState>, headers: HeaderMap| async move {
                    if !authorized(&headers, state.token.as_ref()) {
                        return unauthorized();
                    }
                    let mut text = state.lifecycle.metrics().render_prometheus();
                    text.push_str(&state.app_stats.render_prometheus("app"));
                    if let Some(limiter) = &state.rate_limiter {
                        text.push_str(&limiter.render_prometheus());
                    }
                    #[cfg(feature = "diagnostics")]
                    if let Some(retrieval) = &state.diagnostics {
                        text.push_str(&retrieval.store.render_prometheus());
                    }
                    no_store(
                        (
                            StatusCode::OK,
                            [(
                                CONTENT_TYPE,
                                HeaderValue::from_static(
                                    "text/plain; version=0.0.4; charset=utf-8",
                                ),
                            )],
                            text,
                        )
                            .into_response(),
                    )
                },
            ),
        );
    if state.openapi.is_some() {
        router = router.route(
            openapi_path,
            get(
                |State(state): State<ManagementState>, headers: HeaderMap| async move {
                    if !authorized(&headers, state.token.as_ref()) {
                        return unauthorized();
                    }
                    openapi_response(state.openapi.as_deref())
                },
            ),
        );
    }
    #[cfg(feature = "openapi-ui")]
    if let Some(ui) = &state.openapi_ui {
        let guard = middleware::from_fn_with_state(state.token.clone(), require_token);
        router = router.merge(ui.routes::<ManagementState>().route_layer(guard));
    }
    #[cfg(feature = "diagnostics")]
    if let Some(retrieval) = state.diagnostics.clone() {
        use axum::extract::rejection::PathRejection;
        use axum::extract::{Path, Request};
        let handler = move |request_id: Result<Path<String>, PathRejection>, request: Request| {
            let retrieval = retrieval.clone();
            async move { retrieval.retrieve(request_id, request).await }
        };
        router = router.route(crate::diagnostics::ROUTE, get(handler));
    }
    let limiter = state.rate_limiter.clone();
    let router = router
        .method_not_allowed_fallback(crate::normalize::method_not_allowed_response)
        .fallback(|| async { Problem::new(ProblemKind::RouteNotFound).into_response() })
        .with_state(state);
    let Some(limiter) = limiter else {
        return router;
    };
    // Outermost, so it also covers the fallback and `405` responses.
    let layer = middleware::from_fn_with_state(limiter, rate_limit::enforce);
    router.layer(layer)
}

/// Admits requests that carry the management token, as the OpenAPI
/// document's handler does.
#[cfg(feature = "openapi-ui")]
async fn require_token(
    State(token): State<Option<Secret>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if !authorized(request.headers(), token.as_ref()) {
        return unauthorized();
    }
    next.run(request).await
}

pub(crate) fn openapi_response(document: Option<&Vec<u8>>) -> Response {
    match document {
        Some(bytes) => no_store(
            (
                StatusCode::OK,
                [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                bytes.clone(),
            )
                .into_response(),
        ),
        None => Problem::new(ProblemKind::RouteNotFound).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_are_compared_exactly() {
        let token = Secret::new("0123456789abcdef0123456789abcdef");
        let mut headers = HeaderMap::new();
        assert!(!authorized(&headers, Some(&token)));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer 0123456789abcdef0123456789abcdee"),
        );
        assert!(!authorized(&headers, Some(&token)));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic 0123456789abcdef0123456789abcdef"),
        );
        assert!(
            !authorized(&headers, Some(&token)),
            "only the bearer scheme"
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("bearer 0123456789abcdef0123456789abcdef"),
        );
        assert!(
            authorized(&headers, Some(&token)),
            "auth schemes are case-insensitive (RFC 9110)"
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer 0123456789abcdef0123456789abcdef"),
        );
        assert!(authorized(&headers, Some(&token)));
        assert!(authorized(&HeaderMap::new(), None));
    }
}
