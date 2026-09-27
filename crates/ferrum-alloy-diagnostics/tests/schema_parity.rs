//! The JSON Schema and the Rust model must describe the same contract.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::*;
use serde_json::Value;

fn schema() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/diagnostics/diagnostic-report.v1.schema.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn schema_enum(schema: &Value, name: &str) -> Vec<String> {
    schema["$defs"][name]["anyOf"][0]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{name} has no enum"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn enums_match_the_rust_model() {
    let schema = schema();
    let pairs: &[(&str, &[&str])] = &[
        ("CollectionMethod", CollectionMethod::KNOWN),
        ("Verification", Verification::KNOWN),
        ("ProducerKind", ProducerKind::KNOWN),
        ("ObservationKind", ObservationKind::KNOWN),
        ("Availability", Availability::KNOWN),
        ("Unit", Unit::KNOWN),
        ("ClockDomain", ClockDomain::KNOWN),
        ("Leg", Leg::KNOWN),
        ("Trust", Trust::KNOWN),
        ("SourceScope", SourceScope::KNOWN),
        ("Confidence", Confidence::KNOWN),
        ("Severity", Severity::KNOWN),
        ("Owner", Owner::KNOWN),
        ("EvidenceSource", EvidenceSource::KNOWN),
    ];
    for (name, rust) in pairs {
        let mut from_schema = schema_enum(&schema, name);
        let mut from_rust: Vec<String> = rust.iter().map(|s| (*s).to_owned()).collect();
        from_schema.sort();
        from_rust.sort();
        assert_eq!(from_schema, from_rust, "{name}");
    }
}

#[test]
fn findings_require_every_anvil_field() {
    let schema = schema();
    let required: Vec<&str> = schema["$defs"]["Finding"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
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
        assert!(required.contains(&field), "{field}");
    }
}

#[test]
fn schema_names_the_current_version() {
    let schema = schema();
    assert_eq!(schema["properties"]["schema"]["const"], SCHEMA_NAME);
    assert_eq!(SCHEMA_MAJOR, 1);
}
