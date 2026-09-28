#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::{
    Availability, Collection, CollectionMethod, Confidence, DiagnosticReport, Leg, Observation,
    ObservationKind, Owner, Producer, ProducerKind, Scope, SourceScope, SpanRef, Trust,
    Verification,
};
use ferrum_alloy_diagnostics::parse::{Limits, ReportError, parse_offline};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use ferrum_alloy_diagnostics::{Finding, ParsedReport};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/reports")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn parsed(name: &str) -> ParsedReport {
    parse_offline(&fixture(name), &Limits::default()).unwrap()
}

fn findings(name: &str) -> Vec<Finding> {
    analyze(&parsed(name).report, &Thresholds::default())
}

#[test]
fn operation_exceeding_its_enclosing_measurement_lists_unproven_claims() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("db-operation-dominates.json")).unwrap();
    report["observations"][1]["value"] = serde_json::json!(300.0);
    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();

    let findings = analyze(&parsed.report, &Thresholds::default());
    let finding = by_code(&findings, "alloy.evidence.operation_exceeds_enclosing");
    assert_eq!(finding.rule_id, "alloy.r006");
    assert_eq!(finding.rule_version, 2);
    assert!(!finding.does_not_prove.is_empty());
}

#[test]
fn every_fixture_report_finding_lists_unproven_claims() {
    let reports_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/reports");
    for entry in std::fs::read_dir(&reports_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let parsed = match parse_offline(&bytes, &Limits::default()) {
            Ok(parsed) => parsed,
            Err(_) => {
                // This parser rejection fixture cannot produce findings.
                assert_eq!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("unsupported-major.json")
                );
                continue;
            }
        };
        for finding in analyze(&parsed.report, &Thresholds::default()) {
            assert!(
                !finding.does_not_prove.is_empty(),
                "{} produced finding {} without does_not_prove entries",
                path.display(),
                finding.code
            );
        }
    }
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

/// Claims the evidence in these fixtures can never support. They may appear
/// only inside `does_not_prove`.
const FORBIDDEN_CLAIMS: &[&str] = &[
    "packet loss",
    "backend crashed",
    "service crashed",
    "dns failure",
    "slow handler",
    "network latency",
];

fn assert_no_forbidden_claims(findings: &[Finding]) {
    for finding in findings {
        let claim = format!("{} {}", finding.title, finding.explanation).to_lowercase();
        for forbidden in FORBIDDEN_CLAIMS {
            assert!(
                !claim.contains(forbidden),
                "finding {} makes a forbidden claim {forbidden:?}: {claim}",
                finding.code
            );
        }
    }
}

#[test]
fn every_fixture_avoids_invalid_explanations() {
    for name in [
        "edge-rejected-before-upstream.json",
        "db-operation-dominates.json",
        "db-operation-after-headers.json",
        "unattributed-interval.json",
        "service-span-missing.json",
        "service-exceeds-gateway.json",
        "forged-verified-claim.json",
        "gateway-error-token.json",
    ] {
        assert_no_forbidden_claims(&findings(name));
    }
}

#[test]
fn offline_edge_rejection_is_likely_not_confirmed() {
    let findings = findings("edge-rejected-before-upstream.json");
    let finding = by_code(&findings, "alloy.edge.rejected_before_upstream");
    assert_eq!(finding.confidence, Confidence::Likely);
    assert_eq!(finding.scope, SourceScope::GatewayAdmission);
    assert_eq!(
        finding.owner,
        Owner::Caller,
        "authentication rejections belong to the caller"
    );
    assert_eq!(
        finding.supporting_observations,
        vec!["edge-rejected".to_owned()]
    );
    assert!(
        finding
            .missing_evidence
            .iter()
            .any(|m| m.contains("verified gateway provenance"))
    );
}

fn verified_rejection_report(with_service_span: bool) -> DiagnosticReport {
    let mut report = DiagnosticReport::new(Collection {
        collector: Producer {
            kind: ProducerKind::Collector,
            name: "in-process".into(),
            version: None,
            instance: None,
        },
        method: CollectionMethod::LiveExport,
        verification: Verification::Verified,
        notes: vec![],
    });
    let edge_span = "00f067aa0ba902b7";
    report.observations.push(Observation {
        id: "edge-rejected".into(),
        producer: Producer {
            kind: ProducerKind::Edge,
            name: "ferrum-edge".into(),
            version: Some("0.9.7".into()),
            instance: None,
        },
        kind: ObservationKind::Event,
        name: "edge.request.rejected".into(),
        availability: Availability::Measured,
        value: None,
        unit: None,
        boundaries: None,
        clock: None,
        interval: None,
        scope: Scope {
            leg: Leg::Gateway,
            service: None,
            gateway: None,
            attempt: None,
        },
        span: Some(SpanRef {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: edge_span.into(),
            parent_span_id: None,
        }),
        attributes: [("phase".to_owned(), "before_proxy".to_owned())].into(),
        trust: Trust::Verified,
        evidence_ref: None,
        unrecognized: Default::default(),
    });
    if with_service_span {
        let mut service = report.observations[0].clone();
        service.id = "alloy-response".into();
        service.producer = Producer {
            kind: ProducerKind::Alloy,
            name: "ferrum-alloy-telemetry".into(),
            version: None,
            instance: None,
        };
        service.name = "alloy.response".into();
        service.attributes.clear();
        service.span = Some(SpanRef {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "b7ad6b7169203331".into(),
            parent_span_id: Some(edge_span.into()),
        });
        report.observations.push(service);
    }
    report
}

#[test]
fn verified_edge_rejection_is_confirmed() {
    let findings = analyze(&verified_rejection_report(false), &Thresholds::default());
    let finding = by_code(&findings, "alloy.edge.rejected_before_upstream");
    assert_eq!(finding.confidence, Confidence::Confirmed);
    assert_eq!(finding.owner, Owner::GatewayOperator);
}

#[test]
fn rejection_with_linked_service_span_is_conflicting() {
    let findings = analyze(&verified_rejection_report(true), &Thresholds::default());
    let finding = by_code(&findings, "alloy.edge.rejected_before_upstream");
    assert_eq!(finding.confidence, Confidence::ConflictingEvidence);
    assert!(
        finding
            .supporting_observations
            .contains(&"alloy-response".to_owned())
    );
}

#[test]
fn forged_verified_claim_is_downgraded() {
    let parsed = parsed("forged-verified-claim.json");
    assert_eq!(parsed.claimed_verification, Verification::Verified);
    assert_eq!(
        parsed.report.collection.verification,
        Verification::Unverified
    );
    assert!(
        parsed
            .report
            .observations
            .iter()
            .all(|o| o.trust == Trust::Unverified)
    );
    let messages: Vec<&str> = parsed.warnings.iter().map(|w| w.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("treated as unverified")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("trusted")),
        "the unknown `trusted` field must be reported, not interpreted: {messages:?}"
    );
    let findings = analyze(&parsed.report, &Thresholds::default());
    let finding = by_code(&findings, "alloy.edge.rejected_before_upstream");
    assert_eq!(finding.confidence, Confidence::Likely);
}

#[test]
fn dominance_compares_the_largest_single_operation_and_never_sums() {
    let findings = findings("db-operation-dominates.json");
    let finding = by_code(&findings, "alloy.service.operation_dominates");
    assert_eq!(
        finding.confidence,
        Confidence::Likely,
        "offline evidence caps at likely"
    );
    assert!(finding.explanation.contains("orders.load"));
    assert!(finding.explanation.contains("200.0 ms"));
    assert!(
        !finding.explanation.contains("350"),
        "overlapping operations must not be added"
    );
    assert!(
        finding
            .does_not_prove
            .iter()
            .any(|d| d.contains("database server execution time"))
    );
    assert!(
        finding.missing_evidence.is_empty(),
        "nesting is proven by same-instance intervals"
    );
}

#[test]
fn operations_outside_the_header_phase_are_not_blamed_on_time_to_headers() {
    // The database call runs while the body streams and the cache fill
    // crosses the headers boundary; the only operation inside the header
    // phase is too small to dominate.
    let findings = findings("db-operation-after-headers.json");
    let found = codes(&findings);
    assert!(
        !found.contains(&"alloy.service.operation_dominates"),
        "{found:?}"
    );
    assert!(
        !found.contains(&"alloy.evidence.operation_exceeds_enclosing"),
        "{found:?}"
    );
    let streaming = by_code(&findings, "alloy.response.streaming_dominates");
    assert_eq!(streaming.confidence, Confidence::Likely);
}

#[test]
fn unattributed_interval_is_not_called_network_latency() {
    let findings = findings("unattributed-interval.json");
    let finding = by_code(&findings, "alloy.gateway.unattributed_interval");
    assert_eq!(finding.confidence, Confidence::Likely);
    assert!(finding.explanation.contains("780.0 ms"));
    assert!(
        finding
            .does_not_prove
            .iter()
            .any(|d| d == "network latency")
    );
    assert!(
        finding
            .missing_evidence
            .iter()
            .any(|m| m.contains("attempt identity"))
    );
    assert!(
        finding
            .alternatives
            .iter()
            .any(|a| a.contains("retries and backoff"))
    );
}

#[test]
fn missing_service_span_is_unknown_and_not_a_network_fault() {
    let findings = findings("service-span-missing.json");
    let finding = by_code(&findings, "alloy.telemetry.service_span_missing");
    assert_eq!(finding.confidence, Confidence::Unknown);
    assert!(
        finding
            .does_not_prove
            .iter()
            .any(|d| d.contains("never reached"))
    );
    assert!(
        finding
            .supporting_observations
            .contains(&"alloy-not-sampled".to_owned()),
        "the not-sampled observation explains the gap and must be cited"
    );
}

#[test]
fn missing_service_findings_only_cite_degraded_evidence_for_their_request() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("service-span-missing.json")).unwrap();
    let observations = report["observations"].as_array_mut().unwrap();

    let mut second_edge = observations[0].clone();
    second_edge["id"] = serde_json::json!("edge-ttfb-2");
    second_edge["span"]["span_id"] = serde_json::json!("5555666677778888");
    observations.push(second_edge);

    let mut linked_degraded = observations[1].clone();
    linked_degraded["id"] = serde_json::json!("alloy-not-sampled-linked");
    linked_degraded["producer"]["kind"] = serde_json::json!("user");
    linked_degraded["span"] = serde_json::json!({
        "trace_id": "6c9f0a1b2c3d4e5f60718293a4b5c6d7",
        "span_id": "9999000011112222",
        "parent_span_id": "1111222233334444"
    });
    observations.push(linked_degraded);

    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let missing: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.code == "alloy.telemetry.service_span_missing")
        .collect();

    assert_eq!(missing.len(), 2);
    assert_eq!(
        missing
            .iter()
            .map(|finding| finding.evidence.len())
            .sum::<usize>(),
        3,
        "unlinked degraded evidence must not be copied into every request finding"
    );
    assert_eq!(
        missing
            .iter()
            .filter(|finding| {
                finding
                    .supporting_observations
                    .contains(&"alloy-not-sampled-linked".to_owned())
            })
            .count(),
        1,
        "span-linked degraded evidence belongs to only its gateway request"
    );
    let unlinked = by_code(&findings, "alloy.telemetry.degraded_evidence_unlinked");
    assert_eq!(unlinked.rule_version, 2);
    assert_eq!(
        unlinked.supporting_observations,
        ["alloy-not-sampled"],
        "span-less degraded evidence is cited once instead of being dropped"
    );
    assert!(!unlinked.does_not_prove.is_empty());
}

/// Adds a not-sampled service observation whose span links to no gateway
/// span, as when the service's trust policy re-rooted the trace.
fn push_rerooted_degraded(report: &mut serde_json::Value) {
    let observations = report["observations"].as_array_mut().unwrap();
    let mut rerooted = observations[1].clone();
    rerooted["id"] = serde_json::json!("alloy-not-sampled-rerooted");
    rerooted["span"] = serde_json::json!({
        "trace_id": "6c9f0a1b2c3d4e5f60718293a4b5c6d7",
        "span_id": "aaaabbbbccccdddd",
        "parent_span_id": "eeeeffff00001111"
    });
    observations.push(rerooted);
}

#[test]
fn rerooted_degraded_evidence_is_cited_on_the_only_missing_request() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("service-span-missing.json")).unwrap();
    push_rerooted_degraded(&mut report);

    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let finding = by_code(&findings, "alloy.telemetry.service_span_missing");

    assert_eq!(finding.rule_id, "alloy.r004");
    assert_eq!(finding.rule_version, 2);
    assert_eq!(
        finding.supporting_observations,
        [
            "alloy-not-sampled",
            "alloy-not-sampled-rerooted",
            "edge-ttfb"
        ],
        "with one missing request, unlinked evidence can describe only that request"
    );
    assert!(
        !codes(&findings).contains(&"alloy.telemetry.degraded_evidence_unlinked"),
        "{:?}",
        codes(&findings)
    );
}

#[test]
fn rerooted_degraded_evidence_is_cited_once_when_the_request_is_ambiguous() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("service-span-missing.json")).unwrap();
    push_rerooted_degraded(&mut report);
    let observations = report["observations"].as_array_mut().unwrap();
    let mut second_edge = observations[0].clone();
    second_edge["id"] = serde_json::json!("edge-ttfb-2");
    second_edge["span"]["span_id"] = serde_json::json!("5555666677778888");
    observations.push(second_edge);

    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let unlinked = by_code(&findings, "alloy.telemetry.degraded_evidence_unlinked");

    assert_eq!(
        unlinked.supporting_observations,
        ["alloy-not-sampled", "alloy-not-sampled-rerooted"]
    );
    for finding in &findings {
        if finding.code == "alloy.telemetry.service_span_missing" {
            assert_eq!(finding.evidence.len(), 1, "{:?}", finding.evidence);
        }
    }
}

/// Adds a gateway request on `gateway_span` whose service span
/// `service_span` has a measured time-to-headers.
fn push_served_gateway(report: &mut serde_json::Value, gateway_span: &str, service_span: &str) {
    let observations = report["observations"].as_array_mut().unwrap();
    let mut edge = observations[0].clone();
    edge["id"] = serde_json::json!("edge-ttfb-served");
    edge["span"]["span_id"] = serde_json::json!(gateway_span);
    observations.push(edge);

    let mut served = observations[1].clone();
    served["id"] = serde_json::json!("alloy-served");
    served["availability"] = serde_json::json!("measured");
    served["value"] = serde_json::json!(1700.0);
    served["unit"] = serde_json::json!("ms");
    served["span"] = serde_json::json!({
        "trace_id": "6c9f0a1b2c3d4e5f60718293a4b5c6d7",
        "span_id": service_span,
        "parent_span_id": gateway_span
    });
    observations.push(served);
}

/// Adds a not-sampled observation from `kind` on `span` under `parent`.
fn push_degraded_child(
    report: &mut serde_json::Value,
    id: &str,
    kind: &str,
    span: &str,
    parent: &str,
) {
    let observations = report["observations"].as_array_mut().unwrap();
    let mut degraded = observations[1].clone();
    degraded["id"] = serde_json::json!(id);
    degraded["producer"]["kind"] = serde_json::json!(kind);
    degraded["span"] = serde_json::json!({
        "trace_id": "6c9f0a1b2c3d4e5f60718293a4b5c6d7",
        "span_id": span,
        "parent_span_id": parent
    });
    observations.push(degraded);
}

#[test]
fn degraded_child_of_a_served_request_is_not_cited_on_the_only_missing_request() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("service-span-missing.json")).unwrap();
    push_served_gateway(&mut report, "5555666677778888", "7777000011112222");
    // Not an Alloy span, so the lineage walk must start at its Alloy parent.
    push_degraded_child(
        &mut report,
        "user-not-sampled-served",
        "user",
        "9999000011112222",
        "7777000011112222",
    );

    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let missing: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.code == "alloy.telemetry.service_span_missing")
        .collect();

    assert_eq!(missing.len(), 1);
    assert_eq!(
        missing[0].supporting_observations,
        ["alloy-not-sampled", "edge-ttfb"],
        "evidence under a request that has service telemetry is not attributed to another request"
    );
    assert!(
        !codes(&findings).contains(&"alloy.telemetry.degraded_evidence_unlinked"),
        "{:?}",
        codes(&findings)
    );
}

#[test]
fn degraded_evidence_under_a_served_or_untimed_gateway_is_not_cited() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("service-span-missing.json")).unwrap();
    // A second missing request, so span-less evidence is aggregated.
    let observations = report["observations"].as_array_mut().unwrap();
    let mut second_edge = observations[0].clone();
    second_edge["id"] = serde_json::json!("edge-ttfb-2");
    second_edge["span"]["span_id"] = serde_json::json!("2222333344445555");
    observations.push(second_edge);
    // A gateway request with no measured backend timing.
    let mut untimed = observations[0].clone();
    untimed["id"] = serde_json::json!("edge-untimed");
    untimed["availability"] = serde_json::json!("not_sampled");
    untimed["span"]["span_id"] = serde_json::json!("3333444455556666");
    untimed.as_object_mut().unwrap().remove("value");
    observations.push(untimed);
    push_served_gateway(&mut report, "5555666677778888", "7777000011112222");
    push_degraded_child(
        &mut report,
        "user-not-sampled-served",
        "user",
        "8888000011112222",
        "7777000011112222",
    );
    push_degraded_child(
        &mut report,
        "user-not-sampled-untimed",
        "user",
        "9999000011112222",
        "3333444455556666",
    );

    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let r004: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.rule_id == "alloy.r004")
        .collect();

    // Accepted gap: this evidence belongs to a request R004 does not report,
    // so it is neither cited nor counted in the unlinked aggregate.
    for id in [
        "user-not-sampled-served",
        "user-not-sampled-untimed",
        "edge-untimed",
    ] {
        assert!(
            r004.iter()
                .all(|finding| !finding.supporting_observations.contains(&id.to_owned())),
            "{id} is not cited by R004"
        );
    }
    assert_eq!(
        r004.iter()
            .filter(|finding| finding.code == "alloy.telemetry.service_span_missing")
            .count(),
        2
    );
    let unlinked = by_code(&findings, "alloy.telemetry.degraded_evidence_unlinked");
    assert_eq!(unlinked.supporting_observations, ["alloy-not-sampled"]);
    assert!(
        !unlinked.explanation.contains("more degraded observations"),
        "{}",
        unlinked.explanation
    );
}

#[test]
fn negative_residual_is_preserved_as_conflicting_evidence() {
    let findings = findings("service-exceeds-gateway.json");
    let finding = by_code(&findings, "alloy.evidence.service_exceeds_gateway");
    assert_eq!(finding.confidence, Confidence::ConflictingEvidence);
    assert!(
        finding.explanation.contains("-15.0 ms"),
        "{}",
        finding.explanation
    );
    assert!(
        !codes(&findings).contains(&"alloy.gateway.unattributed_interval"),
        "an invalid comparison must be suppressed"
    );
}

#[test]
fn unsupported_major_version_is_rejected() {
    let error = parse_offline(&fixture("unsupported-major.json"), &Limits::default()).unwrap_err();
    assert_eq!(
        error,
        ReportError::UnsupportedVersion {
            found: "2.0".into()
        }
    );
}

#[test]
fn newer_minor_version_preserves_unknown_fields_without_interpreting_them() {
    let parsed = parsed("gateway-error-token.json");
    let messages: Vec<&str> = parsed.warnings.iter().map(|w| w.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("minor version 1")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("x-future-field")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("client.total_time")),
        "{messages:?}"
    );
    let header = parsed
        .report
        .observations
        .iter()
        .find(|o| o.id == "client-header")
        .unwrap();
    assert!(header.unrecognized.contains_key("x-future-field"));

    let findings = analyze(&parsed.report, &Thresholds::default());
    let finding = by_code(&findings, "alloy.edge.gateway_error_token");
    assert_eq!(
        finding.confidence,
        Confidence::Likely,
        "X-Gateway-Error is spoofable"
    );
    for claim in [
        "that DNS resolution failed",
        "that TLS failed",
        "that the service is down",
    ] {
        assert!(
            finding.does_not_prove.iter().any(|d| d == claim),
            "missing {claim:?}"
        );
    }
    assert_eq!(finding.scope, SourceScope::GatewayToUpstream);
}

#[test]
fn request_timeout_token_is_recognized_and_capped_at_likely() {
    let mut report: serde_json::Value =
        serde_json::from_slice(&fixture("gateway-error-token.json")).unwrap();
    report["observations"][0]["attributes"]["value"] = serde_json::json!("request_timeout");
    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();

    let findings = analyze(&parsed.report, &Thresholds::default());
    let finding = by_code(&findings, "alloy.edge.gateway_error_token");
    assert_eq!(finding.confidence, Confidence::Likely);
    assert!(finding.explanation.contains("before any backend held"));
    for claim in [
        "that the service received the request",
        "which gateway phase used up the deadline",
    ] {
        assert!(
            finding.does_not_prove.iter().any(|d| d == claim),
            "missing {claim:?}"
        );
    }
}

#[test]
fn analysis_and_rendering_are_deterministic() {
    for name in [
        "db-operation-dominates.json",
        "db-operation-after-headers.json",
        "unattributed-interval.json",
        "gateway-error-token.json",
    ] {
        let parsed = parsed(name);
        let first = analyze(&parsed.report, &Thresholds::default());
        let second = analyze(&parsed.report, &Thresholds::default());
        assert_eq!(first, second);
        assert_eq!(
            render_text(&parsed.report, &first, &parsed.warnings),
            render_text(&parsed.report, &second, &parsed.warnings)
        );
    }
}

#[test]
fn rendered_text_matches_snapshot() {
    let parsed = parsed("unattributed-interval.json");
    let findings = analyze(&parsed.report, &Thresholds::default());
    let rendered = render_text(&parsed.report, &findings, &parsed.warnings);
    let snapshot_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/reports/unattributed-interval.expected.txt");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&snapshot_path, &rendered).unwrap();
    }
    let snapshot = std::fs::read_to_string(&snapshot_path).unwrap();
    assert_eq!(rendered, snapshot);
}

#[test]
fn findings_serialize_as_an_anvil_compatible_superset() {
    let findings = findings("edge-rejected-before-upstream.json");
    let value = serde_json::to_value(&findings[0]).unwrap();
    for field in [
        "code",
        "rule_id",
        "rule_version",
        "title",
        "explanation",
        "scope",
        "confidence",
        "severity",
        "evidence",
        "alternatives",
        "does_not_prove",
        "remediation",
        "owner",
        "confirm_with",
    ] {
        assert!(value.get(field).is_some(), "missing Anvil field {field}");
    }
    assert!(value.get("supporting_observations").is_some());
    assert!(value.get("missing_evidence").is_some());
}
