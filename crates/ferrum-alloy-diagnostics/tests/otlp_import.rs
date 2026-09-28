#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::{
    Availability, CollectionMethod, Confidence, Finding, Interval, Producer, ProducerKind,
    Severity, Trust, Verification,
};
use ferrum_alloy_diagnostics::otlp::{ImportError, ImportLimits, import, trace_ids};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
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
    let limits = ImportLimits {
        max_spans: 2,
        ..ImportLimits::default()
    };
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
