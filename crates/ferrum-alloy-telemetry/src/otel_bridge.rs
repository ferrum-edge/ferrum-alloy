//! Bridges the request span to OpenTelemetry when the `otel` feature is on
//! and the application's subscriber includes the OpenTelemetry layer.
//!
//! When the bridge is inactive, the telemetry layer generates its own ids so
//! logs still carry a trace id; nothing is exported.

use tracing::Span;

use crate::trace_context::{SpanId, TraceId, TraceParent, TraceState};

/// Ids of the exported server span.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OtelIds {
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: SpanId,
    pub(crate) sampled: bool,
}

/// Sets the accepted remote parent (before the span starts), optionally links
/// an untrusted context, then starts the span and returns its ids.
///
/// Returns `None` when OpenTelemetry is not active for this span.
#[cfg(feature = "otel")]
pub(crate) fn attach(
    span: &Span,
    accepted: Option<&TraceParent>,
    tracestate: Option<&TraceState>,
    untrusted_link: Option<&TraceParent>,
) -> Option<OtelIds> {
    use opentelemetry::trace::{
        SpanContext, SpanId as OtelSpanId, TraceContextExt, TraceFlags, TraceId as OtelTraceId,
        TraceState as OtelTraceState,
    };
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let remote = |parent: &TraceParent, state: Option<&TraceState>| {
        let state = state
            .and_then(|s| s.as_str().parse::<OtelTraceState>().ok())
            .unwrap_or_default();
        SpanContext::new(
            OtelTraceId::from_bytes(parent.trace_id.0),
            OtelSpanId::from_bytes(parent.parent_id.0),
            TraceFlags::new(parent.flags & 0x01),
            true,
            state,
        )
    };
    if let Some(parent) = accepted {
        let cx = opentelemetry::Context::new().with_remote_span_context(remote(parent, tracestate));
        if span.set_parent(cx).is_err() {
            return None;
        }
    }
    if let Some(untrusted) = untrusted_link {
        span.add_link_with_attributes(
            remote(untrusted, None),
            vec![opentelemetry::KeyValue::new(
                "alloy.link.reason",
                "untrusted_parent",
            )],
        );
    }
    let cx = span.context();
    let span_ref = cx.span();
    let context = span_ref.span_context();
    context.is_valid().then(|| OtelIds {
        trace_id: TraceId(context.trace_id().to_bytes()),
        span_id: SpanId(context.span_id().to_bytes()),
        sampled: context.is_sampled(),
    })
}

/// Without the `otel` feature nothing is exported.
#[cfg(not(feature = "otel"))]
pub(crate) fn attach(
    _span: &Span,
    _accepted: Option<&TraceParent>,
    _tracestate: Option<&TraceState>,
    _untrusted_link: Option<&TraceParent>,
) -> Option<OtelIds> {
    None
}

/// Renames the exported span once the route template is known. A started
/// span ignores later `otel.name` field updates, so the OpenTelemetry span is
/// renamed directly.
#[cfg(feature = "otel")]
pub(crate) fn rename(span: &Span, name: String) {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    span.record("otel.name", name.as_str());
    span.context().span().update_name(name);
}

#[cfg(not(feature = "otel"))]
pub(crate) fn rename(span: &Span, name: String) {
    span.record("otel.name", name.as_str());
}
