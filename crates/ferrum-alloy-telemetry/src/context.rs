//! Per-request context available to handlers.

use crate::peer::PeerTrust;
use crate::request_id::{RequestId, RequestIdSource};
use crate::trace_context::{SpanId, TraceId, TraceParent, TraceState};

/// How the trace for this request was rooted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceDecision {
    /// A valid `traceparent` was accepted as the parent (trusted peer, or an
    /// `any` policy).
    AcceptedRemote,
    /// No `traceparent` was supplied; a new trace was started.
    Root,
    /// A valid `traceparent` came from an untrusted peer; a new trace was
    /// started (re-rooted at the trust boundary).
    RerootedUntrusted,
    /// The supplied `traceparent` was malformed or duplicated.
    RerootedInvalid,
    /// Incoming trace context is disabled by policy.
    IgnoredByPolicy,
}

impl TraceDecision {
    /// Stable label for spans and metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AcceptedRemote => "accepted_remote",
            Self::Root => "root",
            Self::RerootedUntrusted => "rerooted_untrusted",
            Self::RerootedInvalid => "rerooted_invalid",
            Self::IgnoredByPolicy => "ignored_by_policy",
        }
    }
}

/// Request-scoped telemetry context, inserted into request extensions by the
/// telemetry layer.
///
/// Fields may be added in any release, so code outside this crate builds one
/// with [`RequestContext::new`] and adjusts the public fields afterwards.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RequestContext {
    /// The validated request id (also written back to the request header).
    pub request_id: RequestId,
    /// How the request id was chosen.
    pub request_id_source: RequestIdSource,
    /// Trace id of this request's trace.
    pub trace_id: TraceId,
    /// Span id of Alloy's server span. When OpenTelemetry export is active
    /// this is the exported span's id.
    pub span_id: SpanId,
    /// Whether this request's trace is sampled.
    pub sampled: bool,
    /// Whether the span is exported through OpenTelemetry.
    pub exported: bool,
    /// How the trace was rooted.
    pub trace_decision: TraceDecision,
    /// The accepted remote parent, only for [`TraceDecision::AcceptedRemote`].
    pub remote_parent: Option<TraceParent>,
    /// The accepted `tracestate`, only for [`TraceDecision::AcceptedRemote`].
    pub tracestate: Option<TraceState>,
    /// Transport trust of the direct peer.
    pub peer_trust: PeerTrust,
    /// Alloy's server span. Use it as an explicit parent or link for work
    /// that outlives the request (never enter it across `.await`).
    pub span: tracing::Span,
}

impl RequestContext {
    /// A root context for `trace_id` and `span_id`, as the telemetry layer
    /// builds for a request with no accepted remote parent: a generated
    /// request id, [`TraceDecision::Root`], no remote parent or `tracestate`,
    /// not exported, an untrusted peer, and no span.
    ///
    /// Intended for tests and for driving handlers outside the telemetry
    /// layer. Set any other field on the returned value.
    pub fn new(trace_id: TraceId, span_id: SpanId, sampled: bool) -> Self {
        Self {
            request_id: RequestId::generate(),
            request_id_source: RequestIdSource::Generated,
            trace_id,
            span_id,
            sampled,
            exported: false,
            trace_decision: TraceDecision::Root,
            remote_parent: None,
            tracestate: None,
            peer_trust: PeerTrust::Untrusted,
            span: tracing::Span::none(),
        }
    }

    /// The `traceparent` value to send on an outbound request made on behalf
    /// of this request, with `span_id` as the parent (normally the outbound
    /// CLIENT span).
    pub fn child_traceparent(&self, span_id: SpanId) -> TraceParent {
        TraceParent {
            trace_id: self.trace_id,
            parent_id: span_id,
            flags: u8::from(self.sampled),
        }
    }
}

#[cfg(feature = "axum")]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for RequestContext {
    type Rejection = MissingRequestContext;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestContext>()
            .cloned()
            .ok_or(MissingRequestContext)
    }
}

/// Rejection when the telemetry layer is not installed.
#[derive(Debug, Clone, Copy)]
pub struct MissingRequestContext;

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for MissingRequestContext {
    fn into_response(self) -> axum::response::Response {
        // A wiring error in the application, not a client error.
        (
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "request context unavailable: the Ferrum Alloy telemetry layer is not installed",
        )
            .into_response()
    }
}
