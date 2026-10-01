//! Converts OTLP/JSON trace exports into a diagnostic report.
//!
//! Input is the OpenTelemetry Collector `file` exporter format: one
//! `ExportTraceServiceRequest` JSON object per line (or a single object).
//! Trace and span ids must be hex-encoded as the OTLP/JSON specification
//! requires.
//!
//! Only attributes documented in `docs/edge-contract-inventory.md` and
//! `docs/measurement-semantics.md` are interpreted. A negative Ferrum Edge
//! latency value is its "unknown" sentinel and becomes `unavailable`, never
//! zero. Spans are linked only through explicit parent span ids; wall-clock
//! timestamps from different producers are never subtracted.
//!
//! Ferrum Edge v0.9.9 also exports one CLIENT span per backend attempt and
//! hands the service that span as its parent, so the service's SERVER span
//! nests under the attempt and the attempt under the Edge SERVER span. Each
//! attempt becomes an `edge.backend.attempt` link event plus attempt-scoped
//! duration and connection observations. The attempt duration comes from the
//! span's start and end, and is `unavailable` when either is missing or the
//! end precedes the start. Only a CLIENT span with `gateway.backend.attempt`,
//! which Edge sets on every attempt span, is an attempt: other Edge CLIENT
//! spans (for example mesh workload metrics on outbound traffic) are ignored.
//!
//! The resource `service.name` names the service that emitted an Alloy span,
//! or the gateway that emitted a Ferrum Edge span. It is never confused with
//! the producer, which names the telemetry library.
//!
//! A span record that repeats an earlier one exactly, as a collector retry
//! can write, is ignored and counted in a collection note. Span ids must be
//! unique within a trace: the same span id with different content fails
//! with [`ImportError::ConflictingSpans`], even across Edge and Alloy
//! producers.
//!
//! A successful import is always a report that
//! [`parse_offline`](crate::parse::parse_offline) accepts under
//! [`ImportLimits::report`], whether or not the caller writes it out: rules
//! only ever run on reports the parser accepts. A trace whose report would
//! exceed those limits fails with [`ImportError::ReportRejected`]; evidence
//! is never dropped to make it fit.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::catalog;
use crate::model::{
    Availability, Boundaries, ClockDomain, Collection, CollectionMethod, DiagnosticReport,
    Interval, Leg, Observation, ObservationKind, Producer, ProducerKind, Scope, SpanRef, Trust,
    Unit, Verification,
};
use crate::parse::{self, Limits, ReportError};

/// Import bounds. Start from [`ImportLimits::default`] and change fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportLimits {
    /// Maximum input size in bytes.
    pub max_bytes: usize,
    /// Maximum spans read across the whole file.
    pub max_spans: usize,
    /// Maximum JSON nesting depth per line.
    pub max_depth: usize,
    /// Limits the imported report must meet, as
    /// [`parse_offline`](crate::parse::parse_offline) applies them when the
    /// report is read back. Input bounds alone do not bound the report: one
    /// span can yield several observations.
    pub report: Limits,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_spans: 20_000,
            max_depth: 64,
            report: Limits::default(),
        }
    }
}

/// Why an import failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportError {
    /// Input exceeds a bound.
    #[error("OTLP input exceeds limit: {0}")]
    TooLarge(String),
    /// A line is not valid JSON.
    #[error("line {line}: invalid JSON: {message}")]
    InvalidJson {
        /// 1-based line number.
        line: usize,
        /// Parser message.
        message: String,
    },
    /// No spans matched.
    #[error("no spans found{}", .0.as_ref().map(|t| format!(" for trace {t}")).unwrap_or_default())]
    NoSpans(Option<String>),
    /// Several traces are present and none was selected.
    #[error("{0} traces present; select one with a trace id")]
    AmbiguousTrace(usize),
    /// The requested trace id is malformed.
    #[error("trace id must be 32 lowercase hex characters")]
    InvalidTraceId,
    /// The imported report exceeds [`ImportLimits::report`] or otherwise
    /// fails report validation, so it could not be read back.
    #[error("imported report would be rejected by the report parser: {0}")]
    ReportRejected(String),
    /// Two records of the selected trace share a span id but differ.
    #[error("span {0} appears twice with different content")]
    ConflictingSpans(String),
}

/// A span read from OTLP/JSON.
#[derive(Debug, Clone, PartialEq)]
struct RawSpan {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    kind: i64,
    start: u64,
    end: u64,
    attributes: BTreeMap<String, AttrValue>,
    producer: Producer,
    /// Resource `service.name`, or the span attribute when the resource
    /// has none.
    service: Option<String>,
}

#[derive(Debug, Clone)]
enum AttrValue {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

/// Identical parsed content: doubles compare by bit pattern, so a repeated
/// `NaN` is still a repeat.
impl PartialEq for AttrValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Double(a), Self::Double(b)) => a.to_bits() == b.to_bits(),
            (Self::Bool(a), Self::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl AttrValue {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(v) => Some(*v as f64),
            Self::Double(v) => Some(*v),
            Self::Str(s) => s.parse().ok(),
            Self::Bool(_) => None,
        }
    }
    fn as_string(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Int(v) => v.to_string(),
            Self::Double(v) => v.to_string(),
            Self::Bool(v) => v.to_string(),
        }
    }
}

const SPAN_KIND_SERVER: i64 = 2;
const SPAN_KIND_CLIENT: i64 = 3;
/// The attempt number Edge v0.9.9 sets on every backend attempt span.
const ATTEMPT_ATTRIBUTE: &str = "gateway.backend.attempt";
const MAX_ATTR_BYTES: usize = 512;

/// Lists the distinct trace ids in an OTLP/JSON export.
pub fn trace_ids(input: &str, limits: &ImportLimits) -> Result<Vec<String>, ImportError> {
    let spans = read_spans(input, limits)?;
    let ids: BTreeSet<String> = spans.into_iter().map(|s| s.trace_id).collect();
    Ok(ids.into_iter().collect())
}

/// Builds a report for one trace from an OTLP/JSON export.
///
/// When `trace_id` is `None` the file must contain exactly one trace.
pub fn import(
    input: &str,
    trace_id: Option<&str>,
    collector: Producer,
    limits: &ImportLimits,
) -> Result<DiagnosticReport, ImportError> {
    if let Some(id) = trace_id
        && !is_hex(id, 32)
    {
        return Err(ImportError::InvalidTraceId);
    }
    let spans = read_spans(input, limits)?;
    let selected: String = match trace_id {
        Some(id) => id.to_owned(),
        None => {
            let ids: BTreeSet<&str> = spans.iter().map(|s| s.trace_id.as_str()).collect();
            match ids.len() {
                0 => return Err(ImportError::NoSpans(None)),
                1 => ids.into_iter().next().unwrap_or_default().to_owned(),
                n => return Err(ImportError::AmbiguousTrace(n)),
            }
        }
    };
    let mut spans: Vec<RawSpan> = spans
        .into_iter()
        .filter(|s| s.trace_id == selected)
        .collect();
    if spans.is_empty() {
        return Err(ImportError::NoSpans(Some(selected)));
    }
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.span_id.cmp(&b.span_id))
    });
    let (spans, duplicates) = drop_exact_duplicates(spans)?;

    let mut report = DiagnosticReport::new(Collection {
        collector,
        method: CollectionMethod::OtlpFileImport,
        verification: Verification::Unverified,
        notes: vec![
            "converted from an OTLP/JSON file; producer attribution comes from resource attributes and is not authenticated".to_owned(),
        ],
    });
    report.subject.trace_id = Some(selected);
    if duplicates > 0 {
        let note = format!("{duplicates} duplicate span record(s) ignored");
        report.collection.notes.push(note);
    }
    for span in &spans {
        match span.producer.kind {
            ProducerKind::Edge if span.kind == SPAN_KIND_SERVER => {
                edge_observations(span, &mut report)
            }
            ProducerKind::Edge if is_edge_attempt(span) => {
                edge_attempt_observation(span, &mut report)
            }
            ProducerKind::Alloy => alloy_observations(span, &mut report),
            _ => {}
        }
        // Checked per span so an oversized trace stops early.
        if report.observations.len() > limits.report.max_observations {
            return Err(ImportError::ReportRejected(format!(
                "the trace yields more than {} observations",
                limits.report.max_observations
            )));
        }
        if report.subject.request_id.is_none()
            && let Some(id) = span.attributes.get("alloy.request_id")
        {
            report.subject.request_id = Some(id.as_string());
        }
        if report.subject.route.is_none()
            && span.producer.kind == ProducerKind::Alloy
            && let Some(route) = span.attributes.get("http.route")
        {
            report.subject.route = Some(route.as_string());
        }
    }
    report.subject.service = single_service(&report.observations);
    parse::check_report(&report, &limits.report).map_err(rejection)?;
    Ok(report)
}

/// The service every service-scoped observation names, when there is
/// exactly one. A trace across several services keeps its names on each
/// observation only; gateway names never count.
fn single_service(observations: &[Observation]) -> Option<String> {
    let mut names: BTreeSet<&str> = observations
        .iter()
        .filter_map(|o| o.scope.service.as_deref())
        .collect();
    if names.len() == 1 {
        names.pop_first().map(str::to_owned)
    } else {
        None
    }
}

/// Drops span records that repeat an earlier record exactly and returns how
/// many were dropped. The same span id with different content is a conflict
/// that is never resolved by picking one record.
fn drop_exact_duplicates(spans: Vec<RawSpan>) -> Result<(Vec<RawSpan>, usize), ImportError> {
    let mut first: BTreeMap<String, usize> = BTreeMap::new();
    let mut kept: Vec<RawSpan> = Vec::with_capacity(spans.len());
    let mut dropped = 0usize;
    for span in spans {
        match first.get(&span.span_id) {
            Some(&index) if kept.get(index) == Some(&span) => dropped += 1,
            Some(_) => return Err(ImportError::ConflictingSpans(span.span_id)),
            None => {
                first.insert(span.span_id.clone(), kept.len());
                kept.push(span);
            }
        }
    }
    Ok((kept, dropped))
}

/// Why the imported report would not be read back, on one line. Report
/// paths are left out: they name the generated report, not the input.
fn rejection(error: ReportError) -> ImportError {
    let message = match error {
        ReportError::Invalid(issues) => issues
            .iter()
            .take(3)
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    };
    ImportError::ReportRejected(message)
}

fn read_spans(input: &str, limits: &ImportLimits) -> Result<Vec<RawSpan>, ImportError> {
    if input.len() > limits.max_bytes {
        return Err(ImportError::TooLarge(format!(
            "{} bytes > {} bytes",
            input.len(),
            limits.max_bytes
        )));
    }
    let mut spans = Vec::new();
    for (line_no, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if depth_exceeds(line, limits.max_depth) {
            return Err(ImportError::TooLarge(format!(
                "line {} nesting exceeds {}",
                line_no + 1,
                limits.max_depth
            )));
        }
        let value: Value = serde_json::from_str(line).map_err(|e| ImportError::InvalidJson {
            line: line_no + 1,
            message: e.to_string(),
        })?;
        collect_request(&value, limits, &mut spans)?;
    }
    Ok(spans)
}

fn depth_exceeds(line: &str, max: usize) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for byte in line.bytes() {
        if in_string {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

fn collect_request(
    value: &Value,
    limits: &ImportLimits,
    out: &mut Vec<RawSpan>,
) -> Result<(), ImportError> {
    let Some(resource_spans) = value.get("resourceSpans").and_then(Value::as_array) else {
        return Ok(());
    };
    for resource_span in resource_spans {
        let resource_attrs = attributes(
            resource_span
                .get("resource")
                .and_then(|r| r.get("attributes")),
        );
        let Some(scope_spans) = resource_span.get("scopeSpans").and_then(Value::as_array) else {
            continue;
        };
        for scope_span in scope_spans {
            let scope_name = scope_span
                .get("scope")
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Some(items) = scope_span.get("spans").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if out.len() >= limits.max_spans {
                    return Err(ImportError::TooLarge(format!(
                        "more than {} spans",
                        limits.max_spans
                    )));
                }
                if let Some(span) = parse_span(item, &resource_attrs, scope_name) {
                    out.push(span);
                }
            }
        }
    }
    Ok(())
}

fn parse_span(
    item: &Value,
    resource: &BTreeMap<String, AttrValue>,
    scope: &str,
) -> Option<RawSpan> {
    let trace_id = item.get("traceId")?.as_str()?.to_ascii_lowercase();
    let span_id = item.get("spanId")?.as_str()?.to_ascii_lowercase();
    if !is_hex(&trace_id, 32) || !is_hex(&span_id, 16) {
        return None;
    }
    let parent_span_id = item
        .get("parentSpanId")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .filter(|p| is_hex(p, 16));
    let kind = match item.get("kind") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => match s.as_str() {
            "SPAN_KIND_INTERNAL" => 1,
            "SPAN_KIND_SERVER" => 2,
            "SPAN_KIND_CLIENT" => 3,
            "SPAN_KIND_PRODUCER" => 4,
            "SPAN_KIND_CONSUMER" => 5,
            _ => 0,
        },
        _ => 0,
    };
    let attributes = attributes(item.get("attributes"));
    let producer = producer_for(resource, scope, &attributes);
    // An empty name names nothing and never hides the span attribute.
    let named = |attrs: &BTreeMap<String, AttrValue>| {
        attrs
            .get("service.name")
            .map(AttrValue::as_string)
            .filter(|name| !name.trim().is_empty())
    };
    let service = named(resource).or_else(|| named(&attributes));
    Some(RawSpan {
        trace_id,
        span_id,
        parent_span_id,
        name: truncate(item.get("name").and_then(Value::as_str).unwrap_or_default()),
        kind,
        start: nanos(item.get("startTimeUnixNano")),
        end: nanos(item.get("endTimeUnixNano")),
        attributes,
        producer,
        service,
    })
}

fn producer_for(
    resource: &BTreeMap<String, AttrValue>,
    scope: &str,
    span_attrs: &BTreeMap<String, AttrValue>,
) -> Producer {
    let sdk = resource.get("telemetry.sdk.name").map(AttrValue::as_string);
    let kind = if sdk.as_deref() == Some("ferrum-edge") || scope == "ferrum-edge" {
        ProducerKind::Edge
    } else if scope.starts_with("ferrum-alloy")
        || span_attrs.contains_key("alloy.server.time_to_headers_ms")
        || span_attrs.contains_key("alloy.operation.duration_ms")
    {
        ProducerKind::Alloy
    } else {
        ProducerKind::Unrecognized("other".to_owned())
    };
    let name = match &kind {
        ProducerKind::Edge => "ferrum-edge".to_owned(),
        ProducerKind::Alloy => "ferrum-alloy-telemetry".to_owned(),
        _ => sdk.unwrap_or_else(|| "unknown".to_owned()),
    };
    let version = match &kind {
        ProducerKind::Edge => resource
            .get("telemetry.sdk.version")
            .or_else(|| resource.get("service.version")),
        _ => resource.get("service.version"),
    }
    .map(AttrValue::as_string);
    let instance = resource
        .get("service.instance.id")
        .or_else(|| resource.get("host.name"))
        .map(AttrValue::as_string);
    Producer {
        kind,
        name,
        version,
        instance,
    }
}

fn attributes(value: Option<&Value>) -> BTreeMap<String, AttrValue> {
    let mut out = BTreeMap::new();
    let Some(items) = value.and_then(Value::as_array) else {
        return out;
    };
    for item in items.iter().take(256) {
        let (Some(key), Some(value)) = (item.get("key").and_then(Value::as_str), item.get("value"))
        else {
            continue;
        };
        let parsed = if let Some(s) = value.get("stringValue").and_then(Value::as_str) {
            Some(AttrValue::Str(truncate(s)))
        } else if let Some(v) = value.get("intValue") {
            match v {
                Value::Number(n) => n.as_i64().map(AttrValue::Int),
                Value::String(s) => s.parse().ok().map(AttrValue::Int),
                _ => None,
            }
        } else if let Some(v) = value.get("doubleValue") {
            match v {
                Value::Number(n) => n.as_f64().map(AttrValue::Double),
                Value::String(s) => s.parse().ok().map(AttrValue::Double),
                _ => None,
            }
        } else {
            value
                .get("boolValue")
                .and_then(Value::as_bool)
                .map(AttrValue::Bool)
        };
        if let Some(parsed) = parsed {
            out.insert(truncate(key), parsed);
        }
    }
    out
}

fn truncate(value: &str) -> String {
    if value.len() <= MAX_ATTR_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_ATTR_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn nanos(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && value.bytes().any(|b| b != b'0')
}

fn span_ref(span: &RawSpan) -> SpanRef {
    SpanRef {
        trace_id: span.trace_id.clone(),
        span_id: span.span_id.clone(),
        parent_span_id: span.parent_span_id.clone(),
    }
}

fn interval(span: &RawSpan) -> Option<Interval> {
    (span.start > 0 && span.end >= span.start).then_some(Interval {
        start_unix_nano: span.start,
        end_unix_nano: span.end,
    })
}

/// Clock-reading slack allowed when fitting the header phase inside its span.
const HEADER_PHASE_SLACK_NANOS: u64 = 1_000_000;

/// The header phase of an Alloy SERVER span: from the span start (middleware
/// entry) until the span start plus the measured time to headers.
///
/// The span stays open through the response body, so its end is never used
/// as the headers boundary. Both inputs come from the same span of the same
/// process. When the duration is missing, or does not fit inside the span,
/// the interval stays unknown.
fn header_interval(span: &RawSpan, time_to_headers_ms: Option<f64>) -> Option<Interval> {
    let whole = interval(span)?;
    let nanos = time_to_headers_ms? * 1_000_000.0;
    if !nanos.is_finite() || nanos < 0.0 {
        return None;
    }
    let end = whole.start_unix_nano.checked_add(nanos.round() as u64)?;
    if end > whole.end_unix_nano.saturating_add(HEADER_PHASE_SLACK_NANOS) {
        return None;
    }
    Some(Interval {
        start_unix_nano: whole.start_unix_nano,
        end_unix_nano: end,
    })
}

struct Draft<'a> {
    span: &'a RawSpan,
    id_suffix: &'a str,
    name: &'a str,
    leg: Leg,
}

fn base(draft: Draft<'_>, kind: ObservationKind) -> Observation {
    let span = draft.span;
    let entry = catalog::entry(draft.name);
    let (service, gateway) = match span.producer.kind {
        ProducerKind::Edge => (None, span.service.clone()),
        _ => (span.service.clone(), None),
    };
    Observation {
        id: format!(
            "{}:{}:{}",
            if span.producer.kind == ProducerKind::Edge {
                "edge"
            } else {
                "alloy"
            },
            span.span_id,
            draft.id_suffix
        ),
        producer: span.producer.clone(),
        kind,
        name: draft.name.to_owned(),
        availability: Availability::Unknown,
        value: None,
        unit: None,
        boundaries: entry.map(|e| Boundaries {
            start: e.start.to_owned(),
            end: e.end.to_owned(),
        }),
        clock: None,
        interval: None,
        scope: Scope {
            leg: draft.leg,
            service,
            gateway,
            attempt: None,
        },
        span: Some(span_ref(span)),
        attributes: BTreeMap::new(),
        trust: Trust::Unverified,
        evidence_ref: Some(format!("otlp:span/{}", span.span_id)),
        unrecognized: BTreeMap::new(),
    }
}

/// A duration attribute. Negative values (Ferrum Edge's -1 sentinel) and
/// non-finite values become `unavailable`.
fn duration(draft: Draft<'_>, attr: &str) -> Option<Observation> {
    let span = draft.span;
    let raw = span.attributes.get(attr)?;
    let mut observation = base(draft, ObservationKind::Measurement);
    match raw.as_f64() {
        Some(value) if value.is_finite() && value >= 0.0 => {
            observation.availability = Availability::Measured;
            observation.value = Some(value);
            observation.unit = Some(Unit::Milliseconds);
            observation.clock = Some(ClockDomain::MonotonicLocal);
        }
        Some(value) => {
            observation.availability = Availability::Unavailable;
            observation
                .attributes
                .insert("producer.sentinel".to_owned(), value.to_string());
        }
        None => observation.availability = Availability::Unknown,
    }
    Some(observation)
}

fn edge_observations(span: &RawSpan, report: &mut DiagnosticReport) {
    let d = |suffix, name, leg| Draft {
        span,
        id_suffix: suffix,
        name,
        leg,
    };
    if let Some(o) = duration(
        d("total", catalog::EDGE_REQUEST_TOTAL, Leg::Gateway),
        "gateway.latency.total_ms",
    ) {
        report.observations.push(o);
    }
    if let Some(mut o) = duration(
        d(
            "backend_ttfb",
            catalog::EDGE_BACKEND_TIME_TO_HEADERS,
            Leg::GatewayToService,
        ),
        "gateway.latency.backend_ttfb_ms",
    ) {
        if let Some(streamed) = span.attributes.get("gateway.response.streamed") {
            o.attributes
                .insert("edge.response.streamed".to_owned(), streamed.as_string());
        }
        report.observations.push(o);
    }
    if let Some(o) = duration(
        d(
            "backend_total",
            catalog::EDGE_BACKEND_TOTAL,
            Leg::GatewayToService,
        ),
        "gateway.latency.backend_total_ms",
    ) {
        report.observations.push(o);
    }
    if let Some(o) = duration(
        d("plugins", catalog::EDGE_PLUGIN_EXECUTION, Leg::Gateway),
        "gateway.plugin_execution_ms",
    ) {
        report.observations.push(o);
    }
    let mut response = base(
        d("response", catalog::EDGE_RESPONSE, Leg::ClientToGateway),
        ObservationKind::Event,
    );
    response.availability = Availability::Measured;
    if let Some(status) = span.attributes.get("http.response.status_code") {
        response
            .attributes
            .insert("status".to_owned(), status.as_string());
    }
    if let Some(proxy) = span.attributes.get("gateway.proxy.id") {
        response
            .attributes
            .insert("proxy_id".to_owned(), proxy.as_string());
    }
    response.interval = interval(span);
    response.clock = Some(ClockDomain::WallClock);
    report.observations.push(response);
    if let Some(class) = span.attributes.get("gateway.error.class") {
        let mut error = base(
            d("error", catalog::EDGE_GATEWAY_ERROR, Leg::GatewayToService),
            ObservationKind::Event,
        );
        error.availability = Availability::Measured;
        error
            .attributes
            .insert("error_class".to_owned(), class.as_string());
        report.observations.push(error);
    }
}

/// Whether an Edge span is a backend attempt: a CLIENT span carrying the
/// attempt number Edge v0.9.9 puts on every attempt span.
fn is_edge_attempt(span: &RawSpan) -> bool {
    span.kind == SPAN_KIND_CLIENT && span.attributes.contains_key(ATTEMPT_ATTRIBUTE)
}

/// One Ferrum Edge backend attempt (an Edge v0.9.9 CLIENT span), its duration,
/// and the connection evidence Edge emitted for that attempt.
fn edge_attempt_observation(span: &RawSpan, report: &mut DiagnosticReport) {
    let mut attempt = base(
        Draft {
            span,
            id_suffix: "attempt",
            name: catalog::EDGE_BACKEND_ATTEMPT,
            leg: Leg::GatewayToService,
        },
        ObservationKind::Event,
    );
    attempt.availability = Availability::Measured;
    let number = span.attributes.get(ATTEMPT_ATTRIBUTE).and_then(|value| {
        value
            .as_string()
            .parse::<u32>()
            .ok()
            .filter(|number| *number > 0)
    });
    attempt.scope.attempt = number;
    if let Some(number) = number {
        attempt
            .attributes
            .insert("attempt".to_owned(), number.to_string());
    }
    if let Some(reason) = span.attributes.get("gateway.backend.retry_reason") {
        attempt
            .attributes
            .insert("retry_reason".to_owned(), reason.as_string());
    }
    if let Some(reused) = span.attributes.get("gateway.backend.connection.reused") {
        attempt
            .attributes
            .insert("connection.reused".to_owned(), reused.as_string());
    }
    report.observations.push(attempt);

    let mut attempt_duration = base(
        Draft {
            span,
            id_suffix: "attempt_duration",
            name: catalog::EDGE_BACKEND_ATTEMPT_DURATION,
            leg: Leg::GatewayToService,
        },
        ObservationKind::Measurement,
    );
    attempt_duration.scope.attempt = number;
    // A duration needs both timestamps, the end not before the start. Without
    // them the duration is unavailable, never zero, and the raw timestamps
    // Edge exported are kept.
    if let Some(window) = interval(span) {
        let nanos = window.end_unix_nano - window.start_unix_nano;
        attempt_duration.availability = Availability::Measured;
        attempt_duration.value = Some(nanos as f64 / 1_000_000.0);
        attempt_duration.unit = Some(Unit::Milliseconds);
        attempt_duration.clock = Some(ClockDomain::MonotonicLocal);
        attempt_duration.interval = Some(window);
    } else {
        attempt_duration.availability = Availability::Unavailable;
        for (key, nanos) in [
            ("span.start_unix_nano", span.start),
            ("span.end_unix_nano", span.end),
        ] {
            if nanos > 0 {
                attempt_duration
                    .attributes
                    .insert(key.to_owned(), nanos.to_string());
            }
        }
    }
    if let Some(number) = number {
        attempt_duration
            .attributes
            .insert("attempt".to_owned(), number.to_string());
    }
    if let Some(reason) = span.attributes.get("gateway.backend.retry_reason") {
        attempt_duration
            .attributes
            .insert("retry_reason".to_owned(), reason.as_string());
    }
    report.observations.push(attempt_duration);

    for (suffix, name, attribute) in [
        (
            "connection_setup",
            catalog::EDGE_BACKEND_CONNECTION_SETUP,
            "gateway.backend.connection.setup_ms",
        ),
        (
            "connection_dns",
            catalog::EDGE_BACKEND_CONNECTION_DNS,
            "gateway.backend.connection.dns_ms",
        ),
        (
            "connection_tcp_connect",
            catalog::EDGE_BACKEND_CONNECTION_TCP_CONNECT,
            "gateway.backend.connection.tcp_connect_ms",
        ),
        (
            "connection_tls_handshake",
            catalog::EDGE_BACKEND_CONNECTION_TLS_HANDSHAKE,
            "gateway.backend.connection.tls_handshake_ms",
        ),
    ] {
        if let Some(mut measurement) = duration(
            Draft {
                span,
                id_suffix: suffix,
                name,
                leg: Leg::GatewayToService,
            },
            attribute,
        ) {
            measurement.scope.attempt = number;
            if let Some(number) = number {
                measurement
                    .attributes
                    .insert("attempt".to_owned(), number.to_string());
            }
            report.observations.push(measurement);
        }
    }

    if span
        .attributes
        .get("gateway.backend.connection.reused")
        .is_some_and(|value| value.as_string() == "true")
        && !span
            .attributes
            .contains_key("gateway.backend.connection.setup_ms")
    {
        let mut setup = base(
            Draft {
                span,
                id_suffix: "connection_setup",
                name: catalog::EDGE_BACKEND_CONNECTION_SETUP,
                leg: Leg::GatewayToService,
            },
            ObservationKind::Measurement,
        );
        setup.availability = Availability::NotApplicable;
        setup.scope.attempt = number;
        report.observations.push(setup);
    }

    if let Some(reused) = span.attributes.get("gateway.backend.connection.reused") {
        let mut connection = base(
            Draft {
                span,
                id_suffix: "connection_reused",
                name: catalog::EDGE_BACKEND_CONNECTION_REUSED,
                leg: Leg::GatewayToService,
            },
            ObservationKind::Event,
        );
        connection.availability = Availability::Measured;
        connection.scope.attempt = number;
        connection
            .attributes
            .insert("reused".to_owned(), reused.as_string());
        if let Some(number) = number {
            connection
                .attributes
                .insert("attempt".to_owned(), number.to_string());
        }
        report.observations.push(connection);
    }
}

fn alloy_observations(span: &RawSpan, report: &mut DiagnosticReport) {
    let d = |suffix, name, leg| Draft {
        span,
        id_suffix: suffix,
        name,
        leg,
    };
    if span.kind == SPAN_KIND_SERVER
        && span
            .attributes
            .contains_key("alloy.server.time_to_headers_ms")
    {
        if let Some(mut o) = duration(
            d(
                "time_to_headers",
                catalog::ALLOY_TIME_TO_HEADERS,
                Leg::Service,
            ),
            "alloy.server.time_to_headers_ms",
        ) {
            o.interval = header_interval(span, o.duration_ms());
            report.observations.push(o);
        }
        if let Some(mut o) = duration(
            d("body", catalog::ALLOY_BODY_DURATION, Leg::Service),
            "alloy.server.body_duration_ms",
        ) {
            if let Some(outcome) = span.attributes.get("alloy.response.body.outcome") {
                o.attributes
                    .insert("body.outcome".to_owned(), outcome.as_string());
            }
            report.observations.push(o);
        }
        if let Some(o) = duration(
            d("duration", catalog::ALLOY_SERVER_DURATION, Leg::Service),
            "alloy.server.duration_ms",
        ) {
            report.observations.push(o);
        }
        if let Some(o) = duration(
            d("admission", catalog::ALLOY_ADMISSION_WAIT, Leg::Service),
            "alloy.admission.wait_ms",
        ) {
            report.observations.push(o);
        }
        let mut response = base(
            d("response", catalog::ALLOY_RESPONSE, Leg::Service),
            ObservationKind::Event,
        );
        response.availability = Availability::Measured;
        for (from, to) in [
            ("http.response.status_code", "status"),
            ("http.route", "route"),
            ("alloy.trace.parent", "trace_parent"),
            ("alloy.peer.trust", "peer_trust"),
        ] {
            if let Some(value) = span.attributes.get(from) {
                response.attributes.insert(to.to_owned(), value.as_string());
            }
        }
        report.observations.push(response);
    } else if span.attributes.contains_key("alloy.operation.duration_ms") {
        let leg = if span.kind == 3 {
            Leg::ServiceToDependency
        } else {
            Leg::Service
        };
        if let Some(mut o) = duration(
            d("operation", catalog::ALLOY_OPERATION_DURATION, leg.clone()),
            "alloy.operation.duration_ms",
        ) {
            o.interval = interval(span);
            let name = span
                .attributes
                .get("alloy.operation.name")
                .map_or_else(|| span.name.clone(), AttrValue::as_string);
            o.attributes.insert("operation.name".to_owned(), name);
            if let Some(kind) = span.attributes.get("alloy.operation.kind") {
                o.attributes
                    .insert("operation.kind".to_owned(), kind.as_string());
            }
            report.observations.push(o);
        }
        if let Some(o) = duration(
            d("pool_wait", catalog::ALLOY_DB_POOL_WAIT, leg),
            "alloy.db.pool_wait_ms",
        ) {
            report.observations.push(o);
        }
    }
}
