//! Converts axum's own empty routing responses into Problem Details.
//!
//! Only responses that are certainly framework-generated are converted:
//!
//! * `404` with an empty body and no `Content-Type`, when the router's
//!   fallback handled the request (no route matched);
//! * `405` with an empty body, no `Content-Type`, and an `Allow` header
//!   (axum's method router).
//!
//! Application responses with a body or content type are never touched.

use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use ferrum_alloy_telemetry::route::{RouteLabel, RouteSlot};
use futures_util::future::BoxFuture;
use http::header::{ALLOW, CONTENT_TYPE};
use http::{Request, StatusCode};
use http_body::Body as _;
use tower_layer::Layer;
use tower_service::Service;

use crate::problem::{Problem, ProblemKind};

/// The Problem Details response used when a registered route rejects a method.
pub(crate) fn method_not_allowed_response() -> Response {
    Problem::new(ProblemKind::MethodNotAllowed)
        .with_detail("The route does not support this method.")
        .into_response()
}

/// Layer producing [`Normalize`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NormalizeLayer;

impl<S> Layer<S> for NormalizeLayer {
    type Service = Normalize<S>;
    fn layer(&self, inner: S) -> Self::Service {
        Normalize { inner }
    }
}

/// Framework-response normalizer.
#[derive(Debug, Clone)]
pub struct Normalize<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for Normalize<S>
where
    S: Service<Request<Body>, Response = Response> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let slot = request.extensions().get::<RouteSlot>().cloned();
        let future = self.inner.call(request);
        Box::pin(async move {
            let response = future.await?;
            Ok(normalize(response, slot.as_ref()))
        })
    }
}

fn is_empty_untyped(response: &Response) -> bool {
    response.body().size_hint().exact() == Some(0) && !response.headers().contains_key(CONTENT_TYPE)
}

fn normalize(response: Response, slot: Option<&RouteSlot>) -> Response {
    match response.status() {
        StatusCode::NOT_FOUND
            if is_empty_untyped(&response)
                && slot.and_then(RouteSlot::get) == Some(&RouteLabel::Unmatched) =>
        {
            Problem::new(ProblemKind::RouteNotFound)
                .with_detail("No route matches the request path.")
                .into_response()
        }
        StatusCode::METHOD_NOT_ALLOWED
            if is_empty_untyped(&response) && response.headers().contains_key(ALLOW) =>
        {
            let allow = response.headers().get(ALLOW).cloned();
            let mut problem = method_not_allowed_response();
            if let Some(allow) = allow {
                problem.headers_mut().insert(ALLOW, allow);
            }
            problem
        }
        _ => response,
    }
}

/// Converts a caught panic into a sanitized 500 problem. The panic message
/// is logged server-side only.
pub(crate) fn panic_response(panic: Box<dyn std::any::Any + Send + 'static>) -> Response<Body> {
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    tracing::error!(target: "ferrum_alloy::panic", panic = %message, "request handler panicked");
    Problem::new(ProblemKind::Internal).into_response()
}
