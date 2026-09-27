//! The request instrumentation layer.
//!
//! Place [`TelemetryLayer`] outermost so early rejections by inner middleware
//! (limits, admission, authentication) are measured, and apply
//! [`crate::route::RecordRouteLayer`] with `Router::layer` so route templates
//! are recorded after matching. If the layer is instead applied with
//! `Router::layer`, it reads the matched route directly but only observes
//! requests that reach a route.
//!
//! Per request the layer:
//! 1. classifies the transport peer (never from headers);
//! 2. chooses a validated request id;
//! 3. decides whether incoming trace context is accepted or re-rooted;
//! 4. creates the SERVER span with no ambient parent;
//! 5. measures time to response headers;
//! 6. wraps the body and finalizes exactly once at completion, error, or drop.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use http::header::{AUTHORIZATION, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Version};
use pin_project_lite::pin_project;
use serde::{Deserialize, Serialize};
use tower_layer::Layer;
use tower_service::Service;
use tracing::Span;
use tracing::field::Empty;

use crate::body::InstrumentedBody;
use crate::cache::is_shared_cacheable;
use crate::context::{RequestContext, TraceDecision};
use crate::metrics::{Metrics, method_label};
use crate::peer::{PeerTrust, SharedClassifier, TrustNobody, peer_info};
use crate::request_id::{DEFAULT_REQUEST_ID_HEADER, RequestId, RequestIdSource};
use crate::route::{NOT_ROUTED, RouteLabel, RouteSlot, matched_route};
use crate::trace_context::{
    SpanId, TRACEPARENT, TRACESTATE, TraceContextError, TraceId, TraceParent, TraceState,
    extract_traceparent, extract_tracestate,
};

/// Whose propagation metadata is honored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptPolicy {
    /// Never honor incoming values.
    Never,
    /// Honor values only from peers the trust classifier trusts.
    #[default]
    TrustedPeers,
    /// Honor values from anyone. Lets any caller choose trace ids and force
    /// sampling; use only behind a boundary that already sanitizes them.
    Any,
}

/// Request id handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct RequestIdConfig {
    /// Header carrying the id (lowercase). Defaults to `x-request-id`, the
    /// Ferrum Edge `correlation_id` default.
    pub header: String,
    /// Whose incoming ids are kept. Defaults to `any`: ids are validated
    /// correlation aids, not credentials.
    pub accept_incoming: AcceptPolicy,
    /// Echo the id on responses that shared caches cannot store.
    pub echo_in_response: bool,
}

impl Default for RequestIdConfig {
    fn default() -> Self {
        Self {
            header: DEFAULT_REQUEST_ID_HEADER.to_owned(),
            accept_incoming: AcceptPolicy::Any,
            echo_in_response: true,
        }
    }
}

/// Incoming W3C trace context handling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct TraceContextConfig {
    /// Whose `traceparent` becomes the parent. Defaults to trusted peers.
    pub accept_incoming: AcceptPolicy,
    /// When re-rooting an untrusted but valid context, add it as a span link
    /// so authorized operators can correlate. Off by default because links
    /// point at caller-chosen trace ids.
    pub link_untrusted_parent: bool,
}

/// `Server-Timing` response header policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerTimingPolicy {
    /// Never emit.
    #[default]
    Disabled,
    /// Emit only to trusted peers (for example the gateway).
    TrustedPeers,
    /// Emit to every caller.
    Always,
}

/// Optional span attributes that can carry personal or high-cardinality data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct RecordConfig {
    /// Record `url.path` (raw path; may contain identifiers).
    pub url_path: bool,
    /// Record `client.address` (peer IP).
    pub client_address: bool,
    /// Record `user_agent.original`.
    pub user_agent: bool,
}

/// Telemetry layer configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct TelemetryConfig {
    /// Request id handling.
    pub request_id: RequestIdConfig,
    /// Trace context handling.
    pub trace_context: TraceContextConfig,
    /// `Server-Timing` policy.
    pub server_timing: ServerTimingPolicy,
    /// Optional attributes.
    pub record: RecordConfig,
    /// Emit one structured `ferrum_alloy::access` event per finalized request.
    pub access_log: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            request_id: RequestIdConfig::default(),
            trace_context: TraceContextConfig::default(),
            server_timing: ServerTimingPolicy::default(),
            record: RecordConfig::default(),
            access_log: true,
        }
    }
}

/// Invalid telemetry configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TelemetryConfigError {
    /// The request id header name is invalid or reserved.
    #[error("invalid request id header {0:?}: {1}")]
    RequestIdHeader(String, &'static str),
}

const RESERVED_ID_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "traceparent",
    "tracestate",
    "baggage",
    "x-forwarded-for",
    "forwarded",
    "x-consumer-username",
    "x-consumer-custom-id",
];

#[derive(Debug, Clone)]
pub(crate) struct Shared {
    config: TelemetryConfig,
    request_id_header: HeaderName,
    metrics: Arc<Metrics>,
    classifier: SharedClassifier,
}

/// Request instrumentation layer.
#[derive(Debug, Clone)]
pub struct TelemetryLayer {
    shared: Arc<Shared>,
}

impl TelemetryLayer {
    /// Creates a layer. The default trust classifier trusts nobody.
    pub fn new(config: TelemetryConfig) -> Result<Self, TelemetryConfigError> {
        let header = config.request_id.header.to_ascii_lowercase();
        if RESERVED_ID_HEADERS.contains(&header.as_str()) {
            return Err(TelemetryConfigError::RequestIdHeader(
                header,
                "reserved header name",
            ));
        }
        let request_id_header = HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
            TelemetryConfigError::RequestIdHeader(header.clone(), "not a valid header name")
        })?;
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                request_id_header,
                metrics: Arc::new(Metrics::default()),
                classifier: Arc::new(TrustNobody),
            }),
        })
    }

    /// Replaces the peer trust classifier.
    #[must_use]
    pub fn with_classifier(mut self, classifier: SharedClassifier) -> Self {
        Arc::make_mut(&mut self.shared).classifier = classifier;
        self
    }

    /// Uses an existing metrics registry (for example one shared with a
    /// management endpoint).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        Arc::make_mut(&mut self.shared).metrics = metrics;
        self
    }

    /// The metrics registry this layer records into.
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.shared.metrics)
    }

    /// The effective configuration.
    pub fn config(&self) -> &TelemetryConfig {
        &self.shared.config
    }
}

impl<S> Layer<S> for TelemetryLayer {
    type Service = TelemetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TelemetryService {
            inner,
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Service produced by [`TelemetryLayer`].
#[derive(Debug, Clone)]
pub struct TelemetryService<S> {
    inner: S,
    shared: Arc<Shared>,
}

/// Marks a request as instrumented so a second layer does not double count.
#[derive(Debug, Clone, Copy)]
struct Instrumented;

static DUPLICATE_WARNED: AtomicBool = AtomicBool::new(false);

struct TraceChoice {
    decision: TraceDecision,
    accepted: Option<TraceParent>,
    tracestate: Option<TraceState>,
    untrusted: Option<TraceParent>,
}

fn decide_trace(
    request: &Request<impl Sized>,
    policy: AcceptPolicy,
    trust: &PeerTrust,
) -> TraceChoice {
    let reroot = |decision, untrusted| TraceChoice {
        decision,
        accepted: None,
        tracestate: None,
        untrusted,
    };
    match extract_traceparent(request.headers()) {
        Err(TraceContextError::Missing) => reroot(TraceDecision::Root, None),
        Err(_) => reroot(TraceDecision::RerootedInvalid, None),
        Ok(parent) => {
            let accept = match policy {
                AcceptPolicy::Never => return reroot(TraceDecision::IgnoredByPolicy, None),
                AcceptPolicy::Any => true,
                AcceptPolicy::TrustedPeers => trust.is_trusted(),
            };
            if accept {
                TraceChoice {
                    decision: TraceDecision::AcceptedRemote,
                    accepted: Some(parent),
                    tracestate: extract_tracestate(request.headers()),
                    untrusted: None,
                }
            } else {
                reroot(TraceDecision::RerootedUntrusted, Some(parent))
            }
        }
    }
}

fn choose_request_id(
    request: &Request<impl Sized>,
    header: &HeaderName,
    policy: AcceptPolicy,
    trust: &PeerTrust,
) -> (RequestId, RequestIdSource) {
    let mut values = request.headers().get_all(header).iter();
    let (Some(first), None) = (values.next(), values.next()) else {
        return if request.headers().contains_key(header) {
            (RequestId::generate(), RequestIdSource::ReplacedInvalid)
        } else {
            (RequestId::generate(), RequestIdSource::Generated)
        };
    };
    let Some(id) = first.to_str().ok().and_then(RequestId::parse) else {
        return (RequestId::generate(), RequestIdSource::ReplacedInvalid);
    };
    let accept = match policy {
        AcceptPolicy::Never => false,
        AcceptPolicy::Any => true,
        AcceptPolicy::TrustedPeers => trust.is_trusted(),
    };
    if accept {
        (id, RequestIdSource::Accepted)
    } else {
        (RequestId::generate(), RequestIdSource::ReplacedUntrusted)
    }
}

fn protocol_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_11 => "1.1",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => "unknown",
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for TelemetryService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    ResBody: http_body::Body,
{
    type Response = Response<InstrumentedBody<ResBody>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        if request.extensions().get::<Instrumented>().is_some() {
            self.shared
                .metrics
                .duplicate_instrumentation
                .fetch_add(1, Ordering::Relaxed);
            if !DUPLICATE_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    target: "ferrum_alloy::telemetry",
                    "Ferrum Alloy telemetry layer applied twice; the inner layer passes requests through without recording them"
                );
            }
            return ResponseFuture {
                inner: self.inner.call(request),
                state: None,
            };
        }
        let start = Instant::now();
        let shared = Arc::clone(&self.shared);
        let config = &shared.config;
        request.extensions_mut().insert(Instrumented);

        let peer_trust = shared.classifier.classify(request.extensions());
        let (request_id, id_source) = choose_request_id(
            &request,
            &shared.request_id_header,
            config.request_id.accept_incoming,
            &peer_trust,
        );
        shared.metrics.request_id_decisions.inc(id_source.as_str());
        if let Ok(value) = HeaderValue::from_str(request_id.as_str()) {
            request
                .headers_mut()
                .insert(shared.request_id_header.clone(), value);
        }

        let trace = decide_trace(&request, config.trace_context.accept_incoming, &peer_trust);
        shared.metrics.trace_decisions.inc(trace.decision.as_str());
        if trace.accepted.is_none() {
            // Do not let application code forward rejected context.
            request.headers_mut().remove(TRACEPARENT);
            request.headers_mut().remove(TRACESTATE);
        }

        let method = method_label(request.method());
        let route_slot = RouteSlot::default();
        let early_route = matched_route(&request);
        let initial_name = match &early_route {
            Some(route) => format!("{method} {}", route.as_str()),
            None => method.to_owned(),
        };
        if let Some(route) = early_route {
            route_slot.set(route);
        }
        request.extensions_mut().insert(route_slot.clone());

        let span = tracing::info_span!(
            target: "ferrum_alloy::http",
            parent: None,
            "http.server.request",
            otel.name = %initial_name,
            otel.kind = "server",
            otel.status_code = Empty,
            http.request.method = method,
            http.route = Empty,
            url.scheme = Empty,
            url.path = Empty,
            client.address = Empty,
            user_agent.original = Empty,
            network.protocol.version = protocol_version(request.version()),
            http.response.status_code = Empty,
            error.type = Empty,
            trace_id = Empty,
            span_id = Empty,
            alloy.request_id = %request_id,
            alloy.trace.parent = trace.decision.as_str(),
            alloy.peer.trust = peer_trust.label(),
            alloy.server.time_to_headers_ms = Empty,
            alloy.server.body_duration_ms = Empty,
            alloy.server.duration_ms = Empty,
            alloy.response.body.outcome = Empty,
            alloy.response.body.bytes = Empty,
            alloy.response.upgraded = Empty,
            alloy.admission.wait_ms = Empty,
        );
        // Fields are recorded in one batch per phase: a JSON `fmt` layer
        // re-serializes every span field on each `record` call.
        let scheme = request.uri().scheme_str().map(str::to_owned);
        let path = config
            .record
            .url_path
            .then(|| request.uri().path().to_owned());
        let client_address = config
            .record
            .client_address
            .then(|| peer_info(request.extensions()).and_then(|p| p.remote_addr))
            .flatten()
            .map(|addr| addr.ip());
        let user_agent = config
            .record
            .user_agent
            .then(|| {
                request
                    .headers()
                    .get(http::header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            })
            .flatten();

        let ids = crate::otel_bridge::attach(
            &span,
            trace.accepted.as_ref(),
            trace.tracestate.as_ref(),
            trace
                .untrusted
                .as_ref()
                .filter(|_| config.trace_context.link_untrusted_parent),
        );
        let (trace_id, span_id, sampled, exported) = match ids {
            Some(ids) => (ids.trace_id, ids.span_id, ids.sampled, true),
            None => (
                trace.accepted.map_or_else(TraceId::random, |p| p.trace_id),
                SpanId::random(),
                trace.accepted.is_some_and(|p| p.sampled()),
                false,
            ),
        };
        tracing::record_all!(
            span,
            url.scheme = scheme.as_deref(),
            url.path = path.as_deref(),
            client.address = client_address.map(tracing::field::display),
            user_agent.original = user_agent.as_deref(),
            trace_id = %trace_id,
            span_id = %span_id,
        );

        let server_timing = match config.server_timing {
            ServerTimingPolicy::Disabled => false,
            ServerTimingPolicy::Always => true,
            ServerTimingPolicy::TrustedPeers => peer_trust.is_trusted(),
        };
        let echo = config
            .request_id
            .echo_in_response
            .then(|| HeaderValue::from_str(request_id.as_str()).ok())
            .flatten()
            .map(|value| (shared.request_id_header.clone(), value));

        request.extensions_mut().insert(RequestContext {
            request_id,
            request_id_source: id_source,
            trace_id,
            span_id,
            sampled,
            exported,
            trace_decision: trace.decision,
            remote_parent: trace.accepted,
            tracestate: trace.tracestate,
            peer_trust,
            span: span.clone(),
        });

        let is_head = request.method() == Method::HEAD;
        let has_authorization = request.headers().contains_key(AUTHORIZATION);
        let request_method = request.method().clone();
        shared.metrics.request_started();
        let finalizer = Finalizer {
            shared: Arc::clone(&shared),
            span: span.clone(),
            start,
            headers_at: None,
            method,
            is_head,
            route_slot,
            route: None,
            status: None,
            body_bytes: 0,
            trailers: false,
            done: false,
        };
        let inner = {
            let _entered = span.enter();
            self.inner.call(request)
        };
        ResponseFuture {
            inner,
            state: Some(FutureState {
                finalizer,
                span,
                request_method,
                has_authorization,
                echo,
                server_timing,
            }),
        }
    }
}

struct FutureState {
    finalizer: Finalizer,
    span: Span,
    request_method: Method,
    has_authorization: bool,
    echo: Option<(HeaderName, HeaderValue)>,
    server_timing: bool,
}

pin_project! {
    /// Response future of [`TelemetryService`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        // `None` for pass-through requests and after completion.
        state: Option<FutureState>,
    }
}

impl<F> std::fmt::Debug for ResponseFuture<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseFuture").finish_non_exhaustive()
    }
}

impl<F, B, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
    B: http_body::Body,
{
    type Output = Result<Response<InstrumentedBody<B>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let Some(state) = this.state.as_ref() else {
            return this
                .inner
                .poll(cx)
                .map(|result| result.map(|response| response.map(InstrumentedBody::passthrough)));
        };
        let result = {
            let _entered = state.span.enter();
            ready!(this.inner.poll(cx))
        };
        let Some(mut state) = this.state.take() else {
            // Unreachable: `state` was `Some` above and nothing else takes it.
            return Poll::Ready(result.map(|response| response.map(InstrumentedBody::passthrough)));
        };
        match result {
            Ok(mut response) => {
                let time_to_headers = state.finalizer.on_headers(response.status());
                let cacheable = is_shared_cacheable(
                    &state.request_method,
                    state.has_authorization,
                    response.status(),
                    response.headers(),
                );
                if (state.echo.is_some() || state.server_timing) && cacheable {
                    state
                        .finalizer
                        .shared
                        .metrics
                        .header_suppressions
                        .inc("shared_cacheable");
                } else {
                    if let Some((name, value)) = state.echo.take() {
                        response.headers_mut().insert(name, value);
                    }
                    if state.server_timing
                        && let Ok(value) = HeaderValue::from_str(&format!(
                            "alloy;dur={:.1};desc=\"service time to response headers\"",
                            time_to_headers.as_secs_f64() * 1_000.0
                        ))
                    {
                        response.headers_mut().append(
                            http::header::HeaderName::from_static("server-timing"),
                            value,
                        );
                    }
                }
                let finalizer = state.finalizer;
                Poll::Ready(Ok(
                    response.map(|body| InstrumentedBody::new(body, Some(finalizer)))
                ))
            }
            Err(error) => {
                state.finalizer.finish(BodyOutcome::ServiceError);
                Poll::Ready(Err(error))
            }
        }
    }
}

/// How a response ended from Alloy's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyOutcome {
    /// The body yielded its final frame to Hyper (not proof of delivery).
    Completed,
    /// The body stream returned an error.
    Error,
    /// The body was dropped before its end (client disconnect, gateway
    /// timeout, connection reset, shutdown).
    Cancelled,
    /// The protocol forbids a body (HEAD, 1xx, 204, 304); none was sent.
    NotSent,
    /// `101 Switching Protocols`; the upgraded session is not instrumented.
    Upgraded,
    /// The request future was dropped before response headers existed.
    CancelledBeforeHeaders,
    /// The inner service returned an error instead of a response.
    ServiceError,
}

impl BodyOutcome {
    /// Stable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::NotSent => "not_sent",
            Self::Upgraded => "upgraded",
            Self::CancelledBeforeHeaders => "cancelled_before_headers",
            Self::ServiceError => "service_error",
        }
    }
}

/// Owns a request's accounting; finalizes exactly once.
pub(crate) struct Finalizer {
    shared: Arc<Shared>,
    span: Span,
    start: Instant,
    headers_at: Option<Instant>,
    method: &'static str,
    is_head: bool,
    route_slot: RouteSlot,
    route: Option<RouteLabel>,
    status: Option<StatusCode>,
    body_bytes: u64,
    trailers: bool,
    done: bool,
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

impl Finalizer {
    pub(crate) fn span(&self) -> &Span {
        &self.span
    }

    fn route_label(&self) -> &str {
        self.route
            .as_ref()
            .or_else(|| self.route_slot.get())
            .map_or(NOT_ROUTED, RouteLabel::as_str)
    }

    fn on_headers(&mut self, status: StatusCode) -> Duration {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.start);
        self.headers_at = Some(now);
        self.status = Some(status);
        self.route = self.route_slot.get().cloned();
        let route = self.route_label().to_owned();
        let template = match &self.route {
            Some(RouteLabel::Template(template)) => Some(&**template),
            _ => None,
        };
        let name = template.map(|template| format!("{} {template}", self.method));
        let server_error = status.is_server_error();
        tracing::record_all!(
            self.span,
            http.response.status_code = status.as_u16(),
            alloy.server.time_to_headers_ms = millis(elapsed),
            http.route = template,
            otel.name = name.as_deref(),
            otel.status_code = server_error.then_some("error"),
            error.type = server_error.then(|| status.as_str()),
            alloy.response.upgraded = (status == StatusCode::SWITCHING_PROTOCOLS).then_some(true),
        );
        if let Some(name) = name {
            crate::otel_bridge::rename(&self.span, name);
        }
        self.shared
            .metrics
            .record_time_to_headers(self.method, &route, status.as_u16(), elapsed);
        elapsed
    }

    pub(crate) fn protocol_body_outcome(&self) -> Option<BodyOutcome> {
        let status = self.status?;
        if status == StatusCode::SWITCHING_PROTOCOLS {
            Some(BodyOutcome::Upgraded)
        } else if self.is_head
            || status.is_informational()
            || status == StatusCode::NO_CONTENT
            || status == StatusCode::NOT_MODIFIED
        {
            Some(BodyOutcome::NotSent)
        } else {
            None
        }
    }

    pub(crate) fn add_body_bytes(&mut self, bytes: u64) {
        self.body_bytes = self.body_bytes.saturating_add(bytes);
    }

    pub(crate) fn saw_trailers(&mut self) {
        self.trailers = true;
    }

    pub(crate) fn finish(&mut self, outcome: BodyOutcome) {
        if self.done {
            return;
        }
        self.done = true;
        let now = Instant::now();
        let total = now.saturating_duration_since(self.start);
        let body = self.headers_at.map(|at| now.saturating_duration_since(at));
        let status = self.status.map_or(0, |s| s.as_u16());
        let route = self.route_label().to_owned();

        let failed = matches!(outcome, BodyOutcome::Error | BodyOutcome::ServiceError);
        tracing::record_all!(
            self.span,
            alloy.server.duration_ms = millis(total),
            alloy.server.body_duration_ms = body.map(millis),
            alloy.response.body.outcome = outcome.as_str(),
            alloy.response.body.bytes = self.body_bytes,
            otel.status_code = failed.then_some("error"),
            error.type = failed.then(|| outcome.as_str()),
        );

        let metrics = &self.shared.metrics;
        metrics.record_duration(self.method, &route, status, total);
        metrics.body_outcomes.inc(outcome.as_str());
        metrics.request_finished();

        if self.shared.config.access_log {
            let time_to_headers = self
                .headers_at
                .map(|at| millis(at.saturating_duration_since(self.start)));
            tracing::event!(
                target: "ferrum_alloy::access",
                parent: &self.span,
                tracing::Level::INFO,
                { "http.response.status_code" = status,
                  "http.route" = %route,
                  "alloy.server.time_to_headers_ms" = time_to_headers,
                  "alloy.server.duration_ms" = millis(total),
                  "alloy.response.body.outcome" = outcome.as_str(),
                  "alloy.response.body.bytes" = self.body_bytes,
                  "alloy.response.trailers" = self.trailers },
                "request finished"
            );
        }
        // An enter/exit pair stamps the span's end at finalization; dropping
        // our handle then lets it close.
        drop(self.span.enter());
        self.span = Span::none();
    }
}

impl Drop for Finalizer {
    fn drop(&mut self) {
        if !self.done {
            let outcome = if self.headers_at.is_some() {
                BodyOutcome::Cancelled
            } else {
                BodyOutcome::CancelledBeforeHeaders
            };
            self.finish(outcome);
        }
    }
}
