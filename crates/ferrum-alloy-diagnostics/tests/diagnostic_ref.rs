//! Ferrum Edge diagnostic references (`X-Ferrum-Diagnostic-Ref`, Edge
//! v0.9.9): the reference grammar, and how rule `alloy.r007` records a
//! reference a client observed. The header is never gateway evidence, so no
//! reference, however it was collected, raises any finding above `likely`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use ferrum_alloy_diagnostics::catalog::{
    self, edge_diagnostic_ref_replica, is_edge_diagnostic_ref,
};
use ferrum_alloy_diagnostics::model::{
    Availability, Collection, CollectionMethod, Confidence, DiagnosticReport, Leg, Observation,
    ObservationKind, Owner, Producer, ProducerKind, Scope, Severity, Trust, Verification,
};
use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use ferrum_alloy_diagnostics::{Finding, ParsedReport};

const FD1: &str = "fd1_3f9c2a7e5b1d4c8a9e0f6b2d7c4a1e5f";
const FD2: &str = "fd2_1a2b3c4d_3f9c2a7e5b1d4c8a9e0f6b2d7c4a1e5f";
const HEADER: &str = "X-Ferrum-Diagnostic-Ref";
const RECORDED: &str = "alloy.edge.diagnostic_ref";
const MALFORMED: &str = "alloy.edge.diagnostic_ref_malformed";

fn fixture() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/reports/gateway-diagnostic-ref.json");
    let bytes = std::fs::read(&path).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The fixture with its reference header's name and value replaced.
fn parsed_with(header: &str, value: &str) -> ParsedReport {
    let mut report = fixture();
    report["observations"][1]["attributes"]["header"] = serde_json::json!(header);
    report["observations"][1]["attributes"]["value"] = serde_json::json!(value);
    let bytes = serde_json::to_vec(&report).unwrap();
    parse_offline(&bytes, &Limits::default()).unwrap()
}

fn findings_with(header: &str, value: &str) -> Vec<Finding> {
    analyze(&parsed_with(header, value).report, &Thresholds::default())
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

fn is_confirmed(finding: &Finding) -> bool {
    finding.confidence == Confidence::Confirmed
}

#[test]
fn the_grammar_accepts_only_references_edge_mints() {
    assert!(is_edge_diagnostic_ref(FD1));
    assert!(is_edge_diagnostic_ref(FD2));
    let hex = "3f9c2a7e5b1d4c8a9e0f6b2d7c4a1e5f";
    let upper = hex.to_ascii_uppercase();
    for malformed in [
        String::new(),
        "fd1_".to_owned(),
        hex.to_owned(),
        format!("fd1_{upper}"),
        format!("FD1_{hex}"),
        format!("fd1_{hex}0"),
        format!("fd1_{}", &hex[1..]),
        format!("fd1_{hex} "),
        format!(" fd1_{hex}"),
        format!("fd1_{hex}\n"),
        format!("fd3_{hex}"),
        format!("fd2_{hex}"),
        format!("fd2_1A2B3C4D_{hex}"),
        format!("fd2_1a2b3c4_{hex}"),
        format!("fd2_1a2b3c4d{hex}"),
        format!("fd2_1a2b3c4d__{hex}"),
        format!("fd2_1a2b3c4d_{hex}_"),
        format!("fd1_{}/../metrics", &hex[..20]),
        format!("fd1_{}%2f", &hex[..29]),
        "fd1_3f9c2a7e5b1d4c8a9e0f6b2d7c4a1e5g".to_owned(),
    ] {
        assert!(!is_edge_diagnostic_ref(&malformed), "{malformed:?}");
    }
}

#[test]
fn only_replica_tagged_references_name_a_replica() {
    assert_eq!(edge_diagnostic_ref_replica(FD2), Some("1a2b3c4d"));
    assert_eq!(edge_diagnostic_ref_replica(FD1), None);
    assert_eq!(edge_diagnostic_ref_replica("fd2_1a2b3c4d_short"), None);
}

#[test]
fn an_observed_reference_is_recorded_and_capped_at_likely() {
    let findings = findings_with(HEADER, FD1);
    let finding = by_code(&findings, RECORDED);
    assert_eq!(finding.rule_id, "alloy.r007");
    assert_eq!(finding.rule_version, 3);
    assert_eq!(finding.confidence, Confidence::Likely);
    assert_eq!(finding.severity, Severity::Info);
    assert_eq!(finding.owner, Owner::GatewayOperator);
    assert_eq!(finding.supporting_observations, ["client-diagnostic-ref"]);
    assert_eq!(finding.evidence.len(), 1);
    assert_eq!(finding.evidence[0].key, "header.x-ferrum-diagnostic-ref");
    assert_eq!(finding.evidence[0].value, FD1);
    assert!(finding.explanation.contains(FD1), "{}", finding.explanation);
    let lookup = format!("GET /diagnostics/v1/refs/{FD1}");
    let offered = finding
        .confirm_with
        .iter()
        .any(|c| c.contains(&lookup) && c.contains("diagnostics:read"));
    assert!(offered, "{:?}", finding.confirm_with);
    assert!(!finding.missing_evidence.is_empty());
    assert!(!finding.does_not_prove.is_empty());

    // The token finding keeps its own ceiling and missing evidence.
    const MISSING: &str = "authenticated gateway diagnostic record";
    let token = by_code(&findings, "alloy.edge.gateway_error_token");
    assert_eq!(token.confidence, Confidence::Likely);
    assert!(token.missing_evidence.iter().any(|m| m == MISSING));
    assert!(!findings.iter().any(is_confirmed));
}

#[test]
fn the_header_name_is_matched_case_insensitively() {
    for name in ["x-ferrum-diagnostic-ref", "X-FERRUM-DIAGNOSTIC-REF"] {
        let findings = findings_with(name, FD1);
        by_code(&findings, RECORDED);
    }
    let other = findings_with("X-Ferrum-Diagnostic-Owner-Replica", "1a2b3c4d");
    let other = codes(&other);
    assert!(!other.contains(&RECORDED), "{other:?}");
    assert!(!other.contains(&MALFORMED), "{other:?}");
}

#[test]
fn a_replica_tagged_reference_names_its_replica() {
    let findings = findings_with(HEADER, FD2);
    let finding = by_code(&findings, RECORDED);
    let explanation = &finding.explanation;
    assert!(explanation.contains("replica id 1a2b3c4d"), "{explanation}");
    assert_eq!(finding.confidence, Confidence::Likely);
}

#[test]
fn a_malformed_reference_is_unknown_and_never_offered_for_lookup() {
    for value in [
        "FD1_3F9C2A7E5B1D4C8A9E0F6B2D7C4A1E5F",
        "fd1_3f9c2a7e/../../admin",
        "not-a-reference",
    ] {
        let findings = findings_with(HEADER, value);
        let found = codes(&findings);
        assert!(!found.contains(&RECORDED), "{value}: {found:?}");
        let finding = by_code(&findings, MALFORMED);
        assert_eq!(finding.rule_id, "alloy.r007");
        assert_eq!(finding.confidence, Confidence::Unknown);
        let confirm = &finding.confirm_with;
        assert!(confirm.is_empty(), "{confirm:?}");
        assert!(!finding.explanation.contains("/diagnostics/v1/refs/"));
        assert!(!finding.alternatives.is_empty());
        assert!(!finding.does_not_prove.is_empty());
        assert_eq!(finding.evidence[0].value, value);
    }
}

fn client() -> Producer {
    Producer {
        kind: ProducerKind::Client,
        name: "test".into(),
        version: None,
        instance: None,
    }
}

/// The fixture's two headers as a report that claims a verified collection
/// and verified producers, bypassing the offline parser's downgrade.
fn verified_report(value: &str) -> DiagnosticReport {
    let mut report = DiagnosticReport::new(Collection {
        collector: client(),
        method: CollectionMethod::LiveExport,
        verification: Verification::Verified,
        notes: Vec::new(),
    });
    for (id, header, value) in [
        ("token", "X-Gateway-Error", "connection_failure"),
        ("reference", HEADER, value),
    ] {
        report.observations.push(Observation {
            id: id.into(),
            producer: client(),
            kind: ObservationKind::Event,
            name: catalog::CLIENT_RESPONSE_HEADER.into(),
            availability: Availability::Measured,
            value: None,
            unit: None,
            boundaries: None,
            clock: None,
            interval: None,
            scope: Scope {
                leg: Leg::ClientToGateway,
                service: None,
                gateway: None,
                attempt: None,
            },
            span: None,
            attributes: BTreeMap::from([
                ("header".to_owned(), header.to_owned()),
                ("value".to_owned(), value.to_owned()),
            ]),
            trust: Trust::Verified,
            evidence_ref: None,
            unrecognized: BTreeMap::new(),
        });
    }
    report
}

#[test]
fn a_reference_never_raises_confidence_even_from_a_verified_collection() {
    // Only the gateway's own record, bound to this response, could support
    // more than `likely`. Alloy does not resolve references, so a header is
    // the most it ever sees.
    for value in [FD1, FD2] {
        let findings = analyze(&verified_report(value), &Thresholds::default());
        let finding = by_code(&findings, RECORDED);
        assert_eq!(finding.confidence, Confidence::Likely);
        let token = by_code(&findings, "alloy.edge.gateway_error_token");
        assert_eq!(token.confidence, Confidence::Likely);
        assert!(!findings.iter().any(is_confirmed));
    }
}
