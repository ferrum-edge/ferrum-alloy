//! The management listener: health, metrics, and published documents.
//!
//! It binds to loopback by default. Detailed health, metrics, and the
//! OpenAPI document require `Authorization: Bearer <management.token>` when a
//! token is configured; configuration validation refuses a non-loopback
//! management bind without a token. Every response is `no-store`.
//!
//! Requests are rate-limited before any handler or token check runs: per
//! client, and except for the probes, which have a budget of their own, per
//! listener (see [`crate::rate_limit`]).

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

/// Probe paths, rate-limited separately from the rest of the listener.
pub(crate) const PROBE_PATHS: [&str; 2] = [LIVENESS_PATH, READINESS_PATH];

#[derive(Clone)]
pub(crate) struct ManagementState {
    pub(crate) readiness: Arc<Readiness>,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) token: Option<Secret>,
    pub(crate) service: String,
    pub(crate) version: Option<String>,
    pub(crate) app_stats: Arc<ServerStats>,
    pub(crate) openapi: Option<Arc<Vec<u8>>>,
    pub(crate) rate_limiter: Option<Arc<RateLimiter>>,
}

/// Constant-time comparison of equal-length byte strings. Length differences
/// return early; token length is not treated as secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn authorized(headers: &HeaderMap, token: Option<&Secret>) -> bool {
    let Some(token) = token else {
        // Only reachable on a loopback bind (enforced by config validation).
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
            "/health",
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
            "/metrics",
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
    let limiter = state.rate_limiter.clone();
    let router = router
        .fallback(|| async { Problem::new(ProblemKind::RouteNotFound).into_response() })
        .with_state(state);
    let Some(limiter) = limiter else {
        return router;
    };
    // Outermost, so it also covers the fallback and `405` responses.
    let layer = middleware::from_fn_with_state(limiter, rate_limit::enforce);
    router.layer(layer)
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
