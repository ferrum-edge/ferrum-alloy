#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::{
    Availability, CollectionMethod, Confidence, DiagnosticReport, Finding, Interval, Producer,
    ProducerKind, Severity, Trust, Verification,
};
use ferrum_alloy_diagnostics::otlp::{ImportError, ImportLimits, import, trace_ids};
use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use proptest::prelude::*;
use serde_json::{Value, json};

const OK_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const REJECTED_TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const PHASE_TRACE: &str = "3c4d5e6f708192a3b4c5d6e7f8091a2b";
const T0: u64 = 1_790_000_000_000_000_000;
const MS: u64 = 1_000_000;

fn otlp_fixture(name: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/fixtures/otlp")
            .join(name),
    )
    .unwrap()
}

fn input() -> String {
    otlp_fixture("edge-alloy-trace.jsonl")
}

fn collector() -> Producer {
    Producer {
        kind: ProducerKind::Collector,
        name: "ferrum-alloy-cli".into(),
        version: None,
        instance: None,
    }
}

#[test]
fn file_with_several_traces_requires_a_selection() {
    let error = import(&input(), None, collector(), &ImportLimits::default()).unwrap_err();
    assert_eq!(error, ImportError::AmbiguousTrace(2));
    let ids = trace_ids(&input(), &ImportLimits::default()).unwrap();
    assert_eq!(ids, vec![REJECTED_TRACE.to_owned(), OK_TRACE.to_owned()]);
}

#[test]
fn imported_evidence_is_unverified_and_linked_by_parent_ids() {
    let report = import(
        &input(),
        Some(OK_TRACE),
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    assert_eq!(report.collection.method, CollectionMethod::OtlpFileImport);
    assert_eq!(report.collection.verification, Verification::Unverified);
    assert!(
        report
            .observations
            .iter()
            .all(|o| o.trust == Trust::Unverified)
    );
    assert_eq!(report.subject.request_id.as_deref(), Some("req-42"));
    assert_eq!(report.subject.route.as_deref(), Some("/orders/{id}"));

    let ttfb = report
        .observation("edge:00f067aa0ba902b7:backend_ttfb")
        .unwrap();
    assert_eq!(ttfb.duration_ms(), Some(910.0));
    assert_eq!(ttfb.attr("edge.response.streamed"), Some("true"));
    let server = report
        .observation("alloy:b7ad6b7169203331:time_to_headers")
        .unwrap();
    assert_eq!(
        server.span.as_ref().unwrap().parent_span_id.as_deref(),
        Some("00f067aa0ba902b7"),
        "the Alloy server span must be parented by the Edge SERVER span"
    );
    assert_eq!(
        server.interval,
        Some(Interval {
            start_unix_nano: 1_790_000_000_700_000_000,
            end_unix_nano: 1_790_000_000_830_000_000,
        }),
        "the header phase ends at span start plus time to headers, not at the span end"
    );
    let op = report
        .observation("alloy:c1d2e3f405060708:operation")
        .unwrap();
    assert_eq!(op.attr("operation.kind"), Some("db"));

    let findings = analyze(&report, &Thresholds::default());
    let codes: Vec<&str> = findings.iter().map(|f| f.code.as_str()).collect();
    assert!(
        codes.contains(&"alloy.gateway.unattributed_interval"),
        "{codes:?}"
    );
    let dominance = by_code(&findings, "alloy.service.operation_dominates");
    assert_eq!(dominance.confidence, Confidence::Likely);
    assert!(dominance.missing_evidence.is_empty(), "{dominance:?}");
    for finding in &findings {
        assert_ne!(
            finding.confidence,
            Confidence::Confirmed,
            "file imports can never produce confirmed findings ({})",
            finding.code
        );
    }
}

#[test]
fn edge_unknown_sentinel_becomes_unavailable_not_zero() {
    let report = import(
        &input(),
        Some(REJECTED_TRACE),
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let ttfb = report
        .observation("edge:1234567890abcdef:backend_ttfb")
        .unwrap();
    assert_eq!(ttfb.availability, Availability::Unavailable);
    assert_eq!(ttfb.value, None);
    assert_eq!(ttfb.duration_ms(), None);
    assert_eq!(ttfb.attr("producer.sentinel"), Some("-1"));

    let findings = analyze(&report, &Thresholds::default());
    let finding = findings
        .iter()
        .find(|f| f.code == "alloy.edge.no_backend_response_recorded")
        .unwrap();
    assert_eq!(finding.confidence, Confidence::Likely);
    assert!(
        finding
            .does_not_prove
            .iter()
            .any(|d| d.contains("never reached"))
    );
}

#[test]
fn malformed_ids_and_selection_are_rejected() {
    assert_eq!(
        import(
            &input(),
            Some("NOT-HEX"),
            collector(),
            &ImportLimits::default()
        )
        .unwrap_err(),
        ImportError::InvalidTraceId
    );
    let base64_ids = r#"{"resourceSpans":[{"resource":{},"scopeSpans":[{"scope":{"name":"ferrum-edge"},"spans":[{"traceId":"S/kvNXezTaajzpKdDg5HNg==","spanId":"APBnqgupArc=","kind":2}]}]}]}"#;
    assert_eq!(
        import(base64_ids, None, collector(), &ImportLimits::default()).unwrap_err(),
        ImportError::NoSpans(None)
    );
    assert!(matches!(
        import("{not json", None, collector(), &ImportLimits::default()).unwrap_err(),
        ImportError::InvalidJson { line: 1, .. }
    ));
}

#[test]
fn span_limit_is_enforced() {
    let mut limits = ImportLimits::default();
    limits.max_spans = 2;
    assert!(matches!(
        import(&input(), Some(OK_TRACE), collector(), &limits).unwrap_err(),
        ImportError::TooLarge(_)
    ));
}

fn by_code<'a>(findings: &'a [Finding], code: &str) -> &'a Finding {
    findings
        .iter()
        .find(|f| f.code == code)
        .unwrap_or_else(|| panic!("no finding {code}; got {:?}", codes(findings)))
}

fn codes(findings: &[Finding]) -> Vec<&str> {
    findings.iter().map(|f| f.code.as_str()).collect()
}

fn str_attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn f64_attr(key: &str, value: f64) -> Value {
    json!({ "key": key, "value": { "doubleValue": value } })
}

/// An Alloy SERVER span that produced headers after `time_to_headers_ms` and
/// finalized its body at 1,000 ms, with one child database operation.
struct PhaseCase {
    time_to_headers_ms: f64,
    op_start_ms: u64,
    op_len_ms: u64,
    op_recorded_ms: f64,
    instance: Option<&'static str>,
}

impl PhaseCase {
    /// Headers at 100 ms and an operation over `[op_start_ms, op_start_ms + op_len_ms]`.
    fn op(op_start_ms: u64, op_len_ms: u64) -> Self {
        Self {
            time_to_headers_ms: 100.0,
            op_start_ms,
            op_len_ms,
            op_recorded_ms: op_len_ms as f64,
            instance: Some("orders-1"),
        }
    }

    fn otlp(&self) -> String {
        let mut resource = vec![str_attr("service.name", "orders-api")];
        if let Some(instance) = self.instance {
            resource.push(str_attr("service.instance.id", instance));
        }
        let server = json!({
            "traceId": PHASE_TRACE,
            "spanId": "b7ad6b7169203331",
            "name": "GET /orders/export",
            "kind": 2,
            "startTimeUnixNano": T0.to_string(),
            "endTimeUnixNano": (T0 + 1_000 * MS).to_string(),
            "attributes": [
                f64_attr("alloy.server.time_to_headers_ms", self.time_to_headers_ms),
                f64_attr("alloy.server.body_duration_ms", 900.0),
                f64_attr("alloy.server.duration_ms", 1_000.0),
                str_attr("alloy.response.body.outcome", "completed"),
            ],
        });
        let op_start = T0 + self.op_start_ms * MS;
        let operation = json!({
            "traceId": PHASE_TRACE,
            "spanId": "c1d2e3f405060708",
            "parentSpanId": "b7ad6b7169203331",
            "name": "orders.rows",
            "kind": 3,
            "startTimeUnixNano": op_start.to_string(),
            "endTimeUnixNano": (op_start + self.op_len_ms * MS).to_string(),
            "attributes": [
                f64_attr("alloy.operation.duration_ms", self.op_recorded_ms),
                str_attr("alloy.operation.name", "orders.rows"),
                str_attr("alloy.operation.kind", "db"),
            ],
        });
        json!({
            "resourceSpans": [{
                "resource": { "attributes": resource },
                "scopeSpans": [{
                    "scope": { "name": "ferrum-alloy-telemetry" },
                    "spans": [server, operation],
                }],
            }],
        })
        .to_string()
    }

    fn findings(&self) -> Vec<Finding> {
        let report = import(&self.otlp(), None, collector(), &ImportLimits::default()).unwrap();
        analyze(&report, &Thresholds::default())
    }
}

const HEADER_PHASE_CODES: &[&str] = &[
    "alloy.service.operation_dominates",
    "alloy.evidence.operation_exceeds_enclosing",
];

fn assert_not_blamed_on_headers(case: &PhaseCase) {
    let findings = case.findings();
    for code in HEADER_PHASE_CODES {
        assert!(
            !codes(&findings).contains(code),
            "operation at {} ms for {} ms must not be compared with time to headers: {:?}",
            case.op_start_ms,
            case.op_len_ms,
            codes(&findings)
        );
    }
}

#[test]
fn header_interval_ends_at_time_to_headers_not_at_span_end() {
    let report = import(
        &PhaseCase::op(200, 200).otlp(),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let server = report
        .observation("alloy:b7ad6b7169203331:time_to_headers")
        .unwrap();
    assert_eq!(
        server.interval,
        Some(Interval {
            start_unix_nano: T0,
            end_unix_nano: T0 + 100 * MS,
        })
    );
}

#[test]
fn time_to_headers_that_does_not_fit_its_span_leaves_the_interval_unknown() {
    let case = PhaseCase {
        time_to_headers_ms: 2_000.0,
        ..PhaseCase::op(10, 80)
    };
    let report = import(&case.otlp(), None, collector(), &ImportLimits::default()).unwrap();
    let server = report
        .observation("alloy:b7ad6b7169203331:time_to_headers")
        .unwrap();
    assert_eq!(server.duration_ms(), Some(2_000.0));
    assert_eq!(server.interval, None, "never manufacture a header phase");
}

#[test]
fn operations_after_headers_are_not_blamed_on_time_to_headers() {
    // Longer than time to headers, shorter than it, and starting exactly at
    // the headers boundary.
    for (start, len) in [(200, 200), (200, 80), (100, 60), (500, 400)] {
        assert_not_blamed_on_headers(&PhaseCase::op(start, len));
    }
}

#[test]
fn body_phase_fixture_does_not_blame_the_database_on_time_to_headers() {
    let report = import(
        &otlp_fixture("body-phase-operation.jsonl"),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let server = report
        .observation("alloy:b7ad6b7169203331:time_to_headers")
        .unwrap();
    assert_eq!(
        server.interval,
        Some(Interval {
            start_unix_nano: T0,
            end_unix_nano: T0 + 100 * MS,
        })
    );
    let findings = analyze(&report, &Thresholds::default());
    for code in HEADER_PHASE_CODES {
        assert!(!codes(&findings).contains(code), "{:?}", codes(&findings));
    }
    let streaming = by_code(&findings, "alloy.response.streaming_dominates");
    assert_eq!(streaming.confidence, Confidence::Likely);
}

#[test]
fn operations_crossing_the_headers_boundary_are_not_compared() {
    // Each starts before headers were produced and ends after them.
    for (start, len) in [(50, 100), (50, 80), (0, 150), (90, 30)] {
        assert_not_blamed_on_headers(&PhaseCase::op(start, len));
    }
}

#[test]
fn header_phase_operation_still_dominates() {
    for (start, len) in [(10, 80), (0, 100), (5, 60)] {
        let findings = PhaseCase::op(start, len).findings();
        let finding = by_code(&findings, "alloy.service.operation_dominates");
        assert_eq!(finding.confidence, Confidence::Likely, "{finding:?}");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(finding.missing_evidence.is_empty(), "{finding:?}");
        assert!(finding.explanation.contains("orders.rows"));
    }
}

#[test]
fn nested_operation_longer_than_the_header_phase_is_conflicting() {
    let case = PhaseCase {
        op_recorded_ms: 150.0,
        ..PhaseCase::op(10, 80)
    };
    let findings = case.findings();
    let finding = by_code(&findings, "alloy.evidence.operation_exceeds_enclosing");
    assert_eq!(finding.confidence, Confidence::ConflictingEvidence);
    let found = codes(&findings);
    assert!(
        !found.contains(&"alloy.service.operation_dominates"),
        "{found:?}"
    );
}

#[test]
fn unplaced_operation_is_unknown_rather_than_likely() {
    let unnamed_instance = PhaseCase {
        instance: None,
        ..PhaseCase::op(200, 80)
    };
    // Headers claimed after the span ended: the header phase is unknown.
    let unknown_phase = PhaseCase {
        time_to_headers_ms: 1_100.0,
        ..PhaseCase::op(10, 900)
    };
    for case in [unnamed_instance, unknown_phase] {
        let findings = case.findings();
        let finding = by_code(&findings, "alloy.service.operation_dominates");
        assert_eq!(finding.confidence, Confidence::Unknown, "{finding:?}");
        assert_eq!(finding.severity, Severity::Info);
        assert!(
            finding
                .missing_evidence
                .iter()
                .any(|m| m.contains("inside the header phase")),
            "{finding:?}"
        );
        assert!(
            finding
                .does_not_prove
                .iter()
                .any(|d| d.contains("ran before response headers were produced")),
            "{finding:?}"
        );
    }
}

const ORDERS_SPAN: &str = "b7ad6b7169203331";
const BILLING_SPAN: &str = "d4e5f60718293a4b";

/// One resource of Alloy spans, named by its resource `service.name` only,
/// as Alloy's own OTLP export names it.
fn alloy_resource(service: &str, spans: Vec<Value>) -> Value {
    json!({
        "resource": { "attributes": [str_attr("service.name", service)] },
        "scopeSpans": [{
            "scope": { "name": "ferrum-alloy-telemetry" },
            "spans": spans,
        }],
    })
}

/// An Alloy SERVER span recording every server phase: five observations.
fn server_span(span_id: &str, parent: Option<&str>, start_ms: u64) -> Value {
    let start = T0 + start_ms * MS;
    let mut span = json!({
        "traceId": PHASE_TRACE,
        "spanId": span_id,
        "name": "GET /orders",
        "kind": 2,
        "startTimeUnixNano": start.to_string(),
        "endTimeUnixNano": (start + 200 * MS).to_string(),
        "attributes": [
            f64_attr("alloy.server.time_to_headers_ms", 100.0),
            f64_attr("alloy.server.body_duration_ms", 100.0),
            f64_attr("alloy.server.duration_ms", 200.0),
            f64_attr("alloy.admission.wait_ms", 0.0),
        ],
    });
    if let Some(parent) = parent {
        span["parentSpanId"] = json!(parent);
    }
    span
}

fn otlp(resources: Vec<Value>) -> String {
    json!({ "resourceSpans": resources }).to_string()
}

/// One trace of `count` Alloy SERVER spans with distinct span ids.
fn many_server_spans(count: u64) -> String {
    let spans = (1..=count)
        .map(|i| server_span(&format!("{i:016x}"), None, i))
        .collect();
    otlp(vec![alloy_resource("orders-api", spans)])
}

/// Serializes an imported report and reads it back with the default limits.
fn round_trip(report: &DiagnosticReport) -> DiagnosticReport {
    let bytes = serde_json::to_vec(report).unwrap();
    parse_offline(&bytes, &Limits::default()).unwrap().report
}

#[test]
fn resource_service_name_names_the_service() {
    let spans = vec![server_span(ORDERS_SPAN, None, 0)];
    let input = otlp(vec![alloy_resource("orders-api", spans)]);
    let imported = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    for report in [round_trip(&imported), imported] {
        assert_eq!(report.subject.service.as_deref(), Some("orders-api"));
        assert_eq!(report.observations.len(), 5);
        for observation in &report.observations {
            let (scope, id) = (&observation.scope, &observation.id);
            assert_eq!(scope.service.as_deref(), Some("orders-api"), "{id}");
            assert_eq!(scope.gateway, None, "{id}");
            // The producer names the telemetry library, not the service.
            assert_eq!(observation.producer.name, "ferrum-alloy-telemetry");
        }
    }
}

#[test]
fn edge_resource_service_name_names_the_gateway_not_the_service() {
    let imported = import(
        &input(),
        Some(OK_TRACE),
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let report = round_trip(&imported);
    assert_eq!(report.subject.service.as_deref(), Some("orders-api"));
    let edge = report.observation("edge:00f067aa0ba902b7:total").unwrap();
    assert_eq!(edge.scope.gateway.as_deref(), Some("edge-public"));
    assert_eq!(edge.scope.service, None);
    let op = report
        .observation("alloy:c1d2e3f405060708:operation")
        .unwrap();
    assert_eq!(op.scope.service.as_deref(), Some("orders-api"));
    assert_eq!(op.scope.gateway, None);
}

#[test]
fn trace_across_services_keeps_each_name_on_its_observations() {
    let orders = alloy_resource("orders-api", vec![server_span(ORDERS_SPAN, None, 0)]);
    let billing_span = server_span(BILLING_SPAN, Some(ORDERS_SPAN), 20);
    let billing = alloy_resource("billing-api", vec![billing_span]);
    let input = otlp(vec![orders, billing]);
    let imported = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    for report in [round_trip(&imported), imported] {
        assert_eq!(report.subject.service, None, "several services");
        for (span, service) in [(ORDERS_SPAN, "orders-api"), (BILLING_SPAN, "billing-api")] {
            let names: Vec<Option<&str>> = report
                .observations
                .iter()
                .filter(|o| o.span.as_ref().is_some_and(|s| s.span_id == span))
                .map(|o| o.scope.service.as_deref())
                .collect();
            assert_eq!(names, vec![Some(service); 5], "{span}");
        }
    }
}

#[test]
fn trace_at_the_observation_limit_reads_back() {
    // 1,000 spans yield exactly the default limit of 5,000 observations.
    let report = import(
        &many_server_spans(1_000),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    assert_eq!(report.observations.len(), 5_000);
    assert_eq!(round_trip(&report).observations.len(), 5_000);
}

#[test]
fn analysis_handles_maximum_cardinality_otlp_with_bounded_ancestry() {
    let server_count = 999;
    let mut spans: Vec<Value> = (1..=server_count)
        .map(|number| {
            let span_id = format!("{number:016x}");
            let parent = (number > 1).then(|| format!("{:016x}", number - 1));
            server_span(&span_id, parent.as_deref(), number)
        })
        .collect();
    let deepest_server = format!("{server_count:016x}");
    for number in 1..=5 {
        spans.push(json!({
            "traceId": PHASE_TRACE,
            "spanId": format!("{:016x}", server_count + number),
            "parentSpanId": deepest_server,
            "name": "orders.rows",
            "kind": 3,
            "startTimeUnixNano": T0.to_string(),
            "endTimeUnixNano": (T0 + MS).to_string(),
            "attributes": [f64_attr("alloy.operation.duration_ms", 1.0)],
        }));
    }
    let report = import(
        &otlp(vec![alloy_resource("orders-api", spans)]),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();

    assert_eq!(report.observations.len(), 5_000);
    let _findings = analyze(&report, &Thresholds::default());
}

#[test]
fn trace_beyond_the_observation_limit_is_rejected_not_truncated() {
    // Well within the input bounds, but 5,005 observations are more than
    // `parse_offline` accepts by default.
    let input = many_server_spans(1_001);
    let error = import(&input, None, collector(), &ImportLimits::default()).unwrap_err();
    let ImportError::ReportRejected(message) = &error else {
        panic!("{error:?}");
    };
    assert!(message.contains("more than 5000 observations"), "{message}");
}

#[test]
fn report_beyond_the_byte_limit_is_rejected() {
    let mut limits = ImportLimits::default();
    limits.report.max_bytes = 2_000;
    let error = import(&many_server_spans(10), None, collector(), &limits).unwrap_err();
    assert!(
        matches!(&error, ImportError::ReportRejected(m) if m.contains("2000 bytes")),
        "{error}"
    );
}

#[test]
fn exact_duplicate_spans_are_ignored_with_a_note() {
    // A collector retry can write the same span record twice.
    let span = server_span(ORDERS_SPAN, None, 0);
    let spans = vec![span.clone(), span.clone(), span];
    let input = otlp(vec![alloy_resource("orders-api", spans)]);
    let imported = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    for report in [round_trip(&imported), imported] {
        assert_eq!(report.observations.len(), 5);
        let notes = &report.collection.notes;
        let note = "2 duplicate span record(s) ignored";
        assert!(notes.iter().any(|n| n == note), "{notes:?}");
    }
}

#[test]
fn conflicting_records_of_one_span_are_rejected() {
    let spans = vec![
        server_span(ORDERS_SPAN, None, 0),
        server_span(ORDERS_SPAN, None, 5),
    ];
    let input = otlp(vec![alloy_resource("orders-api", spans)]);
    let error = import(&input, None, collector(), &ImportLimits::default()).unwrap_err();
    let expected = ImportError::ConflictingSpans(ORDERS_SPAN.to_owned());
    assert_eq!(error, expected);
    assert_eq!(
        error.to_string(),
        format!("span {ORDERS_SPAN} appears twice with different content")
    );
}

#[test]
fn spans_too_far_apart_are_rejected_with_their_times() {
    let later_ms = 25 * 60 * 60 * 1_000;
    let spans = vec![
        server_span(ORDERS_SPAN, None, 0),
        server_span(BILLING_SPAN, None, later_ms),
    ];
    let input = otlp(vec![alloy_resource("orders-api", spans)]);
    let error = import(&input, None, collector(), &ImportLimits::default()).unwrap_err();
    let ImportError::ReportRejected(message) = &error else {
        panic!("{error:?}");
    };
    assert!(message.contains("time range too wide"), "{message}");
    assert!(message.contains(&format!("from {T0} to ")), "{message}");
    let range = "25.0 h apart; the limit is 24 h";
    assert!(message.contains(range), "{message}");
    assert!(!message.contains("/observations"), "{message}");
}

#[test]
fn empty_resource_service_name_names_nothing() {
    let mut named = server_span(ORDERS_SPAN, None, 0);
    named["attributes"]
        .as_array_mut()
        .unwrap()
        .push(str_attr("service.name", "orders-api"));
    let cases = [
        (named, Some("orders-api")),
        (server_span(ORDERS_SPAN, None, 0), None),
    ];
    for (span, expected) in cases {
        // The empty resource name neither hides the span attribute nor
        // becomes the subject's service.
        let input = otlp(vec![alloy_resource("", vec![span])]);
        let report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
        assert_eq!(report.subject.service.as_deref(), expected);
        for observation in &report.observations {
            assert_eq!(observation.scope.service.as_deref(), expected);
        }
    }
}

const GATEWAY_SPAN: &str = "00f067aa0ba902b7";
const FIRST_ATTEMPT: &str = "a1a1a1a1a1a1a1a1";
const SECOND_ATTEMPT: &str = "a2a2a2a2a2a2a2a2";
const RETRIED_SPAN: &str = "c3c3c3c3c3c3c3c3";

/// One resource of Ferrum Edge spans.
fn edge_resource(spans: Vec<Value>) -> Value {
    json!({
        "resource": { "attributes": [
            str_attr("service.name", "edge-public"),
            str_attr("telemetry.sdk.name", "ferrum-edge"),
        ] },
        "scopeSpans": [{
            "scope": { "name": "ferrum-edge" },
            "spans": spans,
        }],
    })
}

/// An Edge SERVER span for a streamed response.
fn gateway_span(backend_ttfb_ms: f64) -> Value {
    json!({
        "traceId": PHASE_TRACE,
        "spanId": GATEWAY_SPAN,
        "name": "GET orders",
        "kind": 2,
        "startTimeUnixNano": T0.to_string(),
        "endTimeUnixNano": (T0 + 1_000 * MS).to_string(),
        "attributes": [
            f64_attr("gateway.latency.total_ms", 1_000.0),
            f64_attr("gateway.latency.backend_ttfb_ms", backend_ttfb_ms),
            { "key": "gateway.response.streamed", "value": { "boolValue": true } },
        ],
    })
}

/// An Edge v0.9.9 CLIENT span for backend attempt `number`, a child of the
/// gateway span, with the connection evidence emitted by Edge.
fn attempt_span(span_id: &str, number: u32, start_ms: u64) -> Value {
    let start = T0 + start_ms * MS;
    let mut attributes = vec![json!({
        "key": "gateway.backend.attempt",
        "value": { "intValue": number.to_string() }
    })];
    if number == 1 {
        attributes.push(f64_attr("gateway.backend.connection.setup_ms", 12.5));
    }
    attributes.push(json!({
        "key": "gateway.backend.connection.reused",
        "value": { "boolValue": number > 1 }
    }));
    if number > 1 {
        attributes.push(json!({
            "key": "gateway.backend.retry_reason",
            "value": { "stringValue": "connection_failure" }
        }));
    }
    json!({
        "traceId": PHASE_TRACE,
        "spanId": span_id,
        "parentSpanId": GATEWAY_SPAN,
        "name": "GET",
        "kind": 3,
        "startTimeUnixNano": start.to_string(),
        "endTimeUnixNano": (start + 250 * MS).to_string(),
        "attributes": attributes,
    })
}

#[test]
fn edge_attempt_span_links_the_service_to_the_gateway_request() {
    let edge = vec![gateway_span(300.0), attempt_span(FIRST_ATTEMPT, 1, 10)];
    let alloy = vec![server_span(ORDERS_SPAN, Some(FIRST_ATTEMPT), 20)];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let imported = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    let report = round_trip(&imported);
    let attempt = report
        .observation(&format!("edge:{FIRST_ATTEMPT}:attempt"))
        .unwrap();
    assert_eq!(attempt.name, "edge.backend.attempt");
    assert_eq!(attempt.attr("attempt"), Some("1"));
    assert_eq!(attempt.scope.gateway.as_deref(), Some("edge-public"));
    assert_eq!(
        attempt.span.as_ref().unwrap().parent_span_id.as_deref(),
        Some(GATEWAY_SPAN)
    );
    // The link, attempt duration, setup timing, and connection reuse are imported.
    let from_attempt = report
        .observations
        .iter()
        .filter(|o| o.span.as_ref().is_some_and(|s| s.span_id == FIRST_ATTEMPT))
        .count();
    assert_eq!(from_attempt, 4);
    assert_eq!(attempt.value, None);
    let timing = report
        .observation(&format!("edge:{FIRST_ATTEMPT}:attempt_duration"))
        .unwrap();
    assert_eq!(timing.name, "edge.backend.attempt.duration");
    assert_eq!(timing.scope.attempt, Some(1));
    assert_eq!(timing.duration_ms(), Some(250.0));
    let setup = report
        .observation(&format!("edge:{FIRST_ATTEMPT}:connection_setup"))
        .unwrap();
    assert_eq!(setup.name, "edge.backend.connection.setup");
    assert_eq!(setup.duration_ms(), Some(12.5));
    let reused = report
        .observation(&format!("edge:{FIRST_ATTEMPT}:connection_reused"))
        .unwrap();
    assert_eq!(reused.attr("reused"), Some("false"));

    let findings = analyze(&report, &Thresholds::default());
    let found = codes(&findings);
    assert!(
        !found.contains(&"alloy.telemetry.service_span_missing"),
        "the service span is linked through the attempt: {found:?}"
    );
    let residual = by_code(&findings, "alloy.gateway.unattributed_interval");
    assert_eq!(residual.confidence, Confidence::Likely);
}

#[test]
fn edge_retry_attempt_spans_report_several_service_attempts() {
    let edge = vec![
        gateway_span(450.0),
        attempt_span(FIRST_ATTEMPT, 1, 10),
        attempt_span(SECOND_ATTEMPT, 2, 360),
    ];
    let alloy = vec![
        server_span(ORDERS_SPAN, Some(FIRST_ATTEMPT), 20),
        server_span(RETRIED_SPAN, Some(SECOND_ATTEMPT), 370),
    ];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    let reused_setup = report
        .observation(&format!("edge:{SECOND_ATTEMPT}:connection_setup"))
        .unwrap();
    assert_eq!(reused_setup.availability, Availability::NotApplicable);
    let findings = analyze(&report, &Thresholds::default());
    let found = codes(&findings);
    let multiple = by_code(&findings, "alloy.gateway.multiple_service_attempts");
    assert_eq!(multiple.confidence, Confidence::Likely);
    assert!(multiple.explanation.contains("attempt 1"));
    assert!(multiple.explanation.contains("attempt 2"));
    assert!(
        multiple
            .evidence
            .iter()
            .any(|e| e.key == "gateway.backend.connection.setup")
    );
    assert!(
        multiple
            .evidence
            .iter()
            .any(|e| e.key == "gateway.backend.connection.reused")
    );
    assert!(
        multiple
            .evidence
            .iter()
            .any(|e| e.key == "gateway.backend.retry_reason" && e.attempt == Some(2))
    );
    for code in [
        "alloy.gateway.timings_not_comparable",
        "alloy.evidence.service_exceeds_gateway",
        "alloy.telemetry.service_span_missing",
    ] {
        assert!(!found.contains(&code), "{code}: {found:?}");
    }
    assert!(multiple.evidence.iter().any(|e| e.attempt == Some(1)));
    assert!(multiple.evidence.iter().any(|e| e.attempt == Some(2)));
}

#[test]
fn multiple_service_spans_without_attempt_spans_keep_the_legacy_r003_output() {
    let edge = vec![gateway_span(450.0)];
    let alloy = vec![
        server_span(ORDERS_SPAN, Some(GATEWAY_SPAN), 20),
        server_span(RETRIED_SPAN, Some(GATEWAY_SPAN), 370),
    ];
    let report = import(
        &otlp(vec![
            edge_resource(edge),
            alloy_resource("orders-api", alloy),
        ]),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let multiple = by_code(
        &analyze(&report, &Thresholds::default()),
        "alloy.gateway.multiple_service_attempts",
    );
    assert_eq!(multiple.rule_version, 3);
    assert_eq!(
        multiple.explanation,
        "2 service server spans are linked to the same gateway span, directly or through gateway backend attempt spans, so these are probably separate attempts. Ferrum Edge v0.9.8 reuses one traceparent for every retry attempt; v0.9.9 exports a span per attempt, but Alloy does not interpret per-attempt timing. Gateway and service timings are not compared."
    );
    assert_eq!(
        multiple.missing_evidence,
        ["per-attempt gateway spans or attempt identifiers"]
    );
}

#[test]
fn committed_attempt_span_fixture_imports_per_attempt_timing() {
    let report = import(
        include_str!("../../../contracts/fixtures/otlp/edge-attempt-spans.jsonl"),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let findings = analyze(&report, &Thresholds::default());
    let residual = by_code(&findings, "alloy.gateway.unattributed_interval");
    assert!(
        residual
            .evidence
            .iter()
            .any(|e| e.key == "edge.backend.attempt.duration")
    );
    assert!(
        residual
            .evidence
            .iter()
            .any(|e| e.key == "edge.backend.connection.setup")
    );
}

#[test]
fn committed_v098_fixture_preserves_multiple_attempt_refusal() {
    let report = import(
        include_str!("../../../contracts/fixtures/otlp/edge-no-attempt-spans.jsonl"),
        None,
        collector(),
        &ImportLimits::default(),
    )
    .unwrap();
    let findings = analyze(&report, &Thresholds::default());
    let multiple = by_code(&findings, "alloy.gateway.multiple_service_attempts");
    assert_eq!(multiple.rule_version, 3);
    assert!(
        multiple
            .explanation
            .contains("Gateway and service timings are not compared.")
    );
    assert_eq!(
        multiple.missing_evidence,
        ["per-attempt gateway spans or attempt identifiers"]
    );
}

#[test]
fn edge_attempt_span_without_a_service_child_leaves_one_request_unlinked() {
    let edge = vec![gateway_span(5.0), attempt_span(FIRST_ATTEMPT, 1, 10)];
    let input = otlp(vec![edge_resource(edge)]);
    let report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    let findings = analyze(&report, &Thresholds::default());
    let missing = findings
        .iter()
        .filter(|f| f.code == "alloy.telemetry.service_span_missing")
        .count();
    assert_eq!(missing, 1, "{:?}", codes(&findings));
}

/// Rule `alloy.r003` codes, which compare gateway and service timings only
/// for a service linked to the gateway request.
const GATEWAY_COMPARISONS: &[&str] = &[
    "alloy.gateway.unattributed_interval",
    "alloy.gateway.timings_not_comparable",
    "alloy.evidence.service_exceeds_gateway",
    "alloy.gateway.multiple_service_attempts",
];

/// The gateway request has no linked service: `alloy.r004` reports it at
/// `unknown`, and `alloy.r003` compares nothing.
fn assert_service_unlinked(report: &DiagnosticReport) {
    let findings = analyze(report, &Thresholds::default());
    let missing = by_code(&findings, "alloy.telemetry.service_span_missing");
    assert_eq!(missing.confidence, Confidence::Unknown);
    let found = codes(&findings);
    for code in GATEWAY_COMPARISONS {
        assert!(!found.contains(code), "{code}: {found:?}");
    }
}

#[test]
fn a_chain_of_two_attempt_spans_does_not_link_the_service() {
    let mut second = attempt_span(SECOND_ATTEMPT, 2, 20);
    second["parentSpanId"] = json!(FIRST_ATTEMPT);
    let edge = vec![
        gateway_span(300.0),
        attempt_span(FIRST_ATTEMPT, 1, 10),
        second,
    ];
    let alloy = vec![server_span(ORDERS_SPAN, Some(SECOND_ATTEMPT), 30)];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    assert_service_unlinked(&report);
}

#[test]
fn a_service_under_an_unexported_attempt_span_stays_unlinked() {
    // Edge drops an attempt span when its export buffer is full, or the
    // collector may never receive it.
    let edge = vec![gateway_span(300.0)];
    let alloy = vec![server_span(ORDERS_SPAN, Some(FIRST_ATTEMPT), 20)];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    assert_service_unlinked(&report);
}

#[test]
fn an_attempt_span_in_another_trace_does_not_link_the_service() {
    let mut elsewhere = attempt_span(FIRST_ATTEMPT, 1, 10);
    elsewhere["traceId"] = json!(OK_TRACE);
    let edge = vec![gateway_span(300.0), elsewhere];
    let alloy = vec![server_span(ORDERS_SPAN, Some(FIRST_ATTEMPT), 20)];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let limits = ImportLimits::default();
    let mut report = import(&input, Some(PHASE_TRACE), collector(), &limits).unwrap();
    // Merge the other trace's attempt, whose span id and parent id match this
    // trace's: linkage must still compare trace ids.
    let other = import(&input, Some(OK_TRACE), collector(), &limits).unwrap();
    let attempt = format!("edge:{FIRST_ATTEMPT}:attempt");
    assert!(report.observation(&attempt).is_none());
    report.observations.extend(other.observations);
    assert!(report.observation(&attempt).is_some());
    assert_service_unlinked(&report);
}

#[test]
fn an_edge_client_span_without_an_attempt_number_is_not_an_attempt() {
    // For example, a mesh workload-metrics span on outbound traffic.
    let mut client = attempt_span(FIRST_ATTEMPT, 1, 10);
    client["attributes"] = json!([]);
    let edge = vec![gateway_span(300.0), client];
    let alloy = vec![server_span(ORDERS_SPAN, Some(FIRST_ATTEMPT), 20)];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    let from_client = report
        .observations
        .iter()
        .filter(|o| o.span.as_ref().is_some_and(|s| s.span_id == FIRST_ATTEMPT))
        .count();
    assert_eq!(from_client, 0);
    assert_service_unlinked(&report);
}

const OTHER_GATEWAY_SPAN: &str = "00f067aa0ba902b8";

#[test]
fn degraded_evidence_follows_at_most_one_attempt_span() {
    let mut second = attempt_span(SECOND_ATTEMPT, 2, 20);
    second["parentSpanId"] = json!(FIRST_ATTEMPT);
    let mut other_gateway = gateway_span(300.0);
    other_gateway["spanId"] = json!(OTHER_GATEWAY_SPAN);
    let edge = vec![
        gateway_span(300.0),
        other_gateway,
        attempt_span(FIRST_ATTEMPT, 1, 10),
        second,
    ];
    let alloy = vec![server_span(ORDERS_SPAN, Some(SECOND_ATTEMPT), 30)];
    let resources = vec![edge_resource(edge), alloy_resource("orders-api", alloy)];
    let input = otlp(resources);
    let mut report = import(&input, None, collector(), &ImportLimits::default()).unwrap();
    for o in &mut report.observations {
        if o.span.as_ref().is_some_and(|s| s.span_id == ORDERS_SPAN) {
            o.availability = Availability::NotSampled;
        }
    }
    // Two gateway requests lack a service, and the unsampled service span is
    // two attempt hops from either, so it is attributed to neither of them.
    let findings = analyze(&report, &Thresholds::default());
    by_code(&findings, "alloy.telemetry.degraded_evidence_unlinked");
}

const SERVICES: &[&str] = &["orders-api", "billing-api", "search-api"];
const EDGE_KEYS: [&str; 2] = [
    "gateway.latency.total_ms",
    "gateway.latency.backend_ttfb_ms",
];
const ALLOY_KEYS: [&str; 2] = [
    "alloy.server.time_to_headers_ms",
    "alloy.server.duration_ms",
];

/// One generated SERVER span: Edge or Alloy, its service, two durations
/// (absent, negative, or measured), start and length in milliseconds, a span
/// id that may repeat, and whether its clock is far from the others.
type GeneratedSpan = (bool, usize, Option<f64>, Option<f64>, u64, u64, u64, bool);

/// An OTLP export of `spans`. A `clean` export gives every span its own id
/// and puts every span on one clock.
fn generated_otlp(spans: &[GeneratedSpan], clean: bool) -> String {
    let mut resources = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        let &(edge, service, first, second, start_ms, len_ms, id, skewed) = span;
        let id = if clean { index as u64 + 1 } else { id };
        let skewed = skewed && !clean;
        let start = if skewed { 1_000 } else { T0 + start_ms * MS };
        let (scope, name, keys) = if edge {
            ("ferrum-edge", "edge-public", EDGE_KEYS)
        } else {
            ("ferrum-alloy-telemetry", SERVICES[service], ALLOY_KEYS)
        };
        let attributes: Vec<Value> = keys
            .iter()
            .zip([first, second])
            .filter_map(|(key, value)| value.map(|v| f64_attr(key, v)))
            .collect();
        resources.push(json!({
            "resource": { "attributes": [str_attr("service.name", name)] },
            "scopeSpans": [{
                "scope": { "name": scope },
                "spans": [{
                    "traceId": PHASE_TRACE,
                    "spanId": format!("{id:016x}"),
                    "kind": 2,
                    "startTimeUnixNano": start.to_string(),
                    "endTimeUnixNano": (start + len_ms * MS).to_string(),
                    "attributes": attributes,
                }],
            }],
        }));
    }
    otlp(resources)
}

proptest! {
    /// Whatever an import accepts reads back with `parse_offline` under the
    /// same report limits, and the subject names a service only when every
    /// service-scoped observation names the same one. Conversely, a clean
    /// export well inside the default limits always imports.
    #[test]
    fn successful_imports_read_back_under_the_report_limits(
        spans in prop::collection::vec(
            (
                any::<bool>(),
                0..SERVICES.len(),
                prop::option::of(-5.0f64..5_000.0),
                prop::option::of(-5.0f64..5_000.0),
                0u64..60_000,
                0u64..5_000,
                1u64..48,
                prop::bool::weighted(0.05),
            ),
            1..40,
        ),
        max_observations in 1usize..120,
        max_bytes in 500usize..60_000,
    ) {
        let input = generated_otlp(&spans, false);
        let mut tight = ImportLimits::default();
        tight.report.max_observations = max_observations;
        tight.report.max_bytes = max_bytes;
        for limits in [ImportLimits::default(), tight] {
            let result = import(&input, None, collector(), &limits);
            prop_assert!(
                matches!(
                    &result,
                    Ok(_) | Err(ImportError::ReportRejected(_) | ImportError::ConflictingSpans(_))
                ),
                "{:?}",
                result.as_ref().err()
            );
            let Ok(report) = result else {
                continue;
            };
            let bytes = serde_json::to_vec(&report).unwrap();
            let parsed = parse_offline(&bytes, &limits.report);
            prop_assert!(parsed.is_ok(), "{:?}", parsed.err());
            let services: BTreeSet<&str> = report
                .observations
                .iter()
                .filter_map(|o| o.scope.service.as_deref())
                .collect();
            let single = if services.len() == 1 { services.first().copied() } else { None };
            prop_assert_eq!(report.subject.service.as_deref(), single);
        }

        // At most 39 spans of at most five observations each, within one
        // minute: far inside 5,000 observations, 4 MiB, and 24 hours.
        let clean = generated_otlp(&spans, true);
        let result = import(&clean, None, collector(), &ImportLimits::default());
        prop_assert!(result.is_ok(), "{:?}", result.err());
    }
}
