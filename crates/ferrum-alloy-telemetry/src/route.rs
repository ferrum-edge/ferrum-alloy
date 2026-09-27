//! Route template capture after routing.
//!
//! The telemetry layer runs before routing so that early rejections are
//! measured, which means the route template is not yet known when a request
//! enters it. [`RecordRouteLayer`], applied with `Router::layer`, runs after
//! route matching (including for the fallback) and writes the matched template
//! into a per-request slot the telemetry layer reads when headers are produced.

use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use http::Request;
use tower_layer::Layer;
use tower_service::Service;

/// Label for requests the router did not match (its fallback ran).
pub const UNMATCHED_ROUTE: &str = "__unmatched__";
/// Label for responses produced before routing (early rejection), or when no
/// route recorder is installed.
pub const NOT_ROUTED: &str = "__not_routed__";

/// A route label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteLabel {
    /// A matched route template, e.g. `/orders/{id}`.
    Template(Arc<str>),
    /// The router fallback handled the request.
    Unmatched,
}

impl RouteLabel {
    /// The label text.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Template(template) => template,
            Self::Unmatched => UNMATCHED_ROUTE,
        }
    }
}

/// Per-request slot written once after routing.
#[derive(Debug, Clone, Default)]
pub struct RouteSlot(Arc<OnceLock<RouteLabel>>);

impl RouteSlot {
    /// Records the label. Later writes are ignored (nested routers record the
    /// outermost match first, which already contains the full template).
    pub fn set(&self, label: RouteLabel) {
        let _ = self.0.set(label);
    }

    /// The recorded label, if routing happened.
    pub fn get(&self) -> Option<&RouteLabel> {
        self.0.get()
    }
}

/// Reads the route template from request extensions, if the router has
/// already matched (i.e. the caller runs after routing).
pub fn matched_route<B>(request: &Request<B>) -> Option<RouteLabel> {
    #[cfg(feature = "axum")]
    if let Some(path) = request.extensions().get::<axum::extract::MatchedPath>() {
        return Some(RouteLabel::Template(Arc::from(path.as_str())));
    }
    request
        .extensions()
        .get::<ExplicitRoute>()
        .map(|r| RouteLabel::Template(Arc::clone(&r.0)))
}

/// A route template set by non-axum routers or by Alloy's own built-in
/// endpoints (for example health checks).
#[derive(Debug, Clone)]
pub struct ExplicitRoute(pub Arc<str>);

/// Layer that records the matched route into the request's [`RouteSlot`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RecordRouteLayer;

impl<S> Layer<S> for RecordRouteLayer {
    type Service = RecordRoute<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RecordRoute { inner }
    }
}

/// Service produced by [`RecordRouteLayer`].
#[derive(Debug, Clone)]
pub struct RecordRoute<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for RecordRoute<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        if let Some(slot) = request.extensions().get::<RouteSlot>() {
            slot.set(matched_route(&request).unwrap_or(RouteLabel::Unmatched));
        }
        self.inner.call(request)
    }
}
