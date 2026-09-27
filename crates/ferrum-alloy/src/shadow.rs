//! Refuses to start when Alloy's own paths on the application listener would
//! silently shadow an application route.
//!
//! Alloy serves its paths ahead of the application's router, which is its
//! fallback, so a route of the application's at one of those paths is never
//! reached, for any method. axum does not list a router's routes, so each
//! path is probed instead: a `GET` for it is routed through a copy of the
//! application's router whose every endpoint is replaced by a stub. `layer`
//! wraps every endpoint (routes, method-not-allowed handlers, nested
//! services, and fallbacks) in a stub that answers "no route", and
//! `route_layer` then wraps the path router's endpoints in an outer stub that
//! answers "route". Neither stub calls the service it wraps, so no
//! application handler, middleware, or fallback runs; routing itself is
//! axum's matching, with no side effects.
//!
//! A root catch-all route (`/{*path}`) matches every path, so it is treated
//! like a fallback: Alloy's paths take precedence over it by design.
//! Fallbacks, including those of nested routers, are not routes and are not
//! reported.

use std::convert::Infallible;
use std::future::{Ready, ready};

use axum::Router;
use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::Route;
use futures_util::FutureExt;
use http::StatusCode;
use tower::{ServiceExt, service_fn};
use tower_layer::layer_fn;

use crate::error::AlloyError;

/// Set on a probe's response when an application route matched: the route's
/// pattern, when axum reports one (it does not for a nested service's tail).
#[derive(Clone)]
struct Matched(Option<String>);

/// Fails with [`AlloyError::ShadowedRoute`] when an application route in
/// `router` matches one of `served`: Alloy's paths on the application
/// listener, each with the setting that places it there.
pub(crate) fn check(router: &Router, served: &[(&'static str, String)]) -> Result<(), AlloyError> {
    if served.is_empty() || !router.has_routes() {
        return Ok(());
    }
    let probe = router
        .clone()
        .layer(layer_fn(|_: Route| service_fn(no_route)))
        .route_layer(layer_fn(|_: Route| service_fn(route_found)));
    for &(setting, ref path) in served {
        // A path that is not a valid request target is never requested, so
        // it shadows nothing.
        let Ok(request) = http::Request::get(path.as_str()).body(Body::empty()) else {
            continue;
        };
        let Some(Ok(response)) = probe.clone().oneshot(request).now_or_never() else {
            return Err(AlloyError::Internal(format!(
                "route probe for {path} did not complete"
            )));
        };
        let Some(Matched(pattern)) = response.extensions().get::<Matched>().cloned() else {
            continue;
        };
        if pattern.as_deref().is_some_and(is_root_catch_all) {
            continue;
        }
        return Err(AlloyError::ShadowedRoute {
            route: pattern.unwrap_or_else(|| "<nested service>".to_owned()),
            path: path.clone(),
            setting,
        });
    }
    Ok(())
}

fn no_route(_request: Request) -> Ready<Result<Response, Infallible>> {
    ready(Ok(StatusCode::NOT_FOUND.into_response()))
}

fn route_found(request: Request) -> Ready<Result<Response, Infallible>> {
    let pattern = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned());
    let mut response = StatusCode::OK.into_response();
    response.extensions_mut().insert(Matched(pattern));
    ready(Ok(response))
}

/// `/{*name}`: a wildcard that is the whole path.
fn is_root_catch_all(pattern: &str) -> bool {
    pattern
        .strip_prefix("/{*")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|name| !name.contains(['/', '{', '}']))
}
