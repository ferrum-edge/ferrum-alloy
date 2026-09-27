#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::{
    Availability, CollectionMethod, Confidence, Producer, ProducerKind, Trust, Verification,
};
use ferrum_alloy_diagnostics::otlp::{ImportError, ImportLimits, import, trace_ids};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};

const OK_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const REJECTED_TRACE: &str = "0af7651916cd43dd8448eb211c80319c";

fn input() -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/fixtures/otlp/edge-alloy-trace.jsonl"),
    )
    .unwrap()
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
    assert!(
        codes.contains(&"alloy.service.operation_dominates"),
        "{codes:?}"
    );
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
