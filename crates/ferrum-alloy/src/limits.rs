//! Request limits with explicit, separate boundaries.
//!
//! * [`BodyLimitLayer`]: rejects a declared `Content-Length` above the limit
//!   before the handler runs, and caps streamed bodies without one.
//! * [`AdmissionLayer`]: bounds concurrently executing handlers. Waiting for
//!   a permit is measured on the request span (`alloy.admission.wait_ms`).
//!   The permit is released when response headers are produced; streaming
//!   bodies do not hold it.
//! * [`HeadersDeadlineLayer`]: bounds the time to produce response headers.
//!   It never applies to response body streaming (SSE) or upgraded sessions.
//!
//! Request-head read time is bounded separately by the server
//! (`server.header_read_timeout_ms`), idle connections by
//! `server.idle_timeout_ms`, and shutdown drain by
//! `shutdown.drain_timeout_ms`.

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use http::Request;
use http::header::CONTENT_LENGTH;
use tokio::sync::Semaphore;
use tower_layer::Layer;
use tower_service::Service;

use crate::problem::{Problem, ProblemKind};

/// Enforces the request body limit.
#[derive(Debug, Clone, Copy)]
pub struct BodyLimitLayer {
    limit: u64,
}

impl BodyLimitLayer {
    /// A limit in bytes.
    pub fn new(limit: u64) -> Self {
        Self { limit }
    }
}

impl<S> Layer<S> for BodyLimitLayer {
    type Service = BodyLimit<S>;
    fn layer(&self, inner: S) -> Self::Service {
        BodyLimit {
            inner,
            limit: self.limit,
        }
    }
}

/// Service produced by [`BodyLimitLayer`].
#[derive(Debug, Clone)]
pub struct BodyLimit<S> {
    inner: S,
    limit: u64,
}

impl<S> Service<Request<Body>> for BodyLimit<S>
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
        let declared = request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|len| len > self.limit) {
            let problem = Problem::new(ProblemKind::PayloadTooLarge)
                .with_detail("The request body exceeds the configured limit.")
                .with_extension("limit_bytes", self.limit);
            return Box::pin(async move { Ok(problem.into_response()) });
        }
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        let request = request.map(|body| Body::new(http_body_util::Limited::new(body, limit)));
        Box::pin(self.inner.call(request))
    }
}

/// Bounds concurrently executing handlers.
#[derive(Debug, Clone)]
pub struct AdmissionLayer {
    permits: Option<Arc<Semaphore>>,
    wait: Duration,
}

impl AdmissionLayer {
    /// `max_in_flight = 0` disables admission control.
    pub fn new(max_in_flight: usize, wait: Duration) -> Self {
        Self {
            permits: (max_in_flight > 0).then(|| Arc::new(Semaphore::new(max_in_flight))),
            wait,
        }
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = Admission<S>;
    fn layer(&self, inner: S) -> Self::Service {
        Admission {
            inner,
            permits: self.permits.clone(),
            wait: self.wait,
        }
    }
}

/// Service produced by [`AdmissionLayer`].
#[derive(Debug, Clone)]
pub struct Admission<S> {
    inner: S,
    permits: Option<Arc<Semaphore>>,
    wait: Duration,
}

impl<S> Service<Request<Body>> for Admission<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let Some(permits) = self.permits.clone() else {
            return Box::pin(self.inner.call(request));
        };
        // The inner service was driven to readiness by `poll_ready`; take
        // that instance and leave a fresh clone behind (tower's contract).
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let wait = self.wait;
        Box::pin(async move {
            let started = Instant::now();
            let permit = match permits.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) if wait.is_zero() => None,
                Err(_) => tokio::time::timeout(wait, permits.acquire_owned())
                    .await
                    .ok()
                    .and_then(Result::ok),
            };
            tracing::Span::current().record(
                "alloy.admission.wait_ms",
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            let Some(permit) = permit else {
                return Ok(Problem::new(ProblemKind::Overloaded)
                    .with_detail("The service is at its concurrent request limit.")
                    .into_response());
            };
            let response = inner.call(request).await;
            drop(permit);
            response
        })
    }
}

/// Deadline for producing response headers.
#[derive(Debug, Clone, Copy)]
pub struct HeadersDeadlineLayer {
    timeout: Option<Duration>,
}

impl HeadersDeadlineLayer {
    /// `Duration::ZERO` disables the deadline.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout: (!timeout.is_zero()).then_some(timeout),
        }
    }
}

impl<S> Layer<S> for HeadersDeadlineLayer {
    type Service = HeadersDeadline<S>;
    fn layer(&self, inner: S) -> Self::Service {
        HeadersDeadline {
            inner,
            timeout: self.timeout,
        }
    }
}

/// Service produced by [`HeadersDeadlineLayer`].
#[derive(Debug, Clone)]
pub struct HeadersDeadline<S> {
    inner: S,
    timeout: Option<Duration>,
}

impl<S> Service<Request<Body>> for HeadersDeadline<S>
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
        let future = self.inner.call(request);
        let Some(timeout) = self.timeout else {
            return Box::pin(future);
        };
        Box::pin(async move {
            match tokio::time::timeout(timeout, future).await {
                Ok(result) => result,
                // Dropping the handler future cancels cooperative work; it
                // cannot guarantee that a remote side effect was undone.
                Err(_) => Ok(Problem::new(ProblemKind::RequestTimeout)
                    .with_detail("The service did not produce a response in time.")
                    .into_response()),
            }
        })
    }
}
