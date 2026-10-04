//! Static contract, binding, and forged-provenance regression fixtures.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::edge_record::{self, bind_record, parse_observation};
use ferrum_alloy_diagnostics::model::Confidence;
use serde_json::{Value, json};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/ferrum-contracts")
}

fn fixture(name: &str) -> Value {
    let bytes = std::fs::read(root().join("fixtures/diagnostic-ref").join(name)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn observation(record: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "reference": record["ref"],
        "namespace": record["namespace"],
        "status": record["status"],
        "gateway_error": record["gateway_error"],
        "protocol": record["protocol"],
        "request_started_at": record["created_at"],
        "response_received_at": record["created_at"],
    }))
    .unwrap()
}

#[test]
fn released_valid_records_bind_but_cannot_authenticate_themselves() {
    for name in [
        "connection-failure",
        "plugin-rejection",
        "replica-tagged-reference",
        "tls-retry",
    ] {
        let mut record = fixture(&format!("valid/{name}.json"));
        let observation = parse_observation(&observation(&record)).unwrap();
        // Forging these fields changes no authority, even in a valid record.
        record["authenticated"] = json!(true);
        record["channel_authenticated"] = json!(true);
        record["trust"] = json!("verified");
        let bytes = serde_json::to_vec(&record).unwrap();
        let bound = bind_record(&bytes, &observation).unwrap();
        assert_eq!(bound.finding().confidence, Confidence::Likely);
        assert!(!bound.finding().does_not_prove.is_empty());
    }
}

#[test]
fn every_released_invalid_fixture_is_refused() {
    let valid = fixture("valid/connection-failure.json");
    let observation = parse_observation(&observation(&valid)).unwrap();
    for entry in std::fs::read_dir(root().join("fixtures/diagnostic-ref/invalid")).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bind_record(&bytes, &observation).is_err(),
            "{}",
            path.display()
        );
    }
}

#[test]
fn binding_checks_all_response_facts_and_retention() {
    let original = fixture("valid/connection-failure.json");
    let observation = parse_observation(&observation(&original)).unwrap();
    for (key, value) in [
        ("ref", json!("fd1_00000000000000000000000000000000")),
        ("namespace", json!("another-tenant")),
        ("status", json!(504)),
        ("gateway_error", json!("backend_error")),
        ("protocol", json!("http1")),
        ("created_at", json!("2026-09-27T10:15:02.113Z")),
        ("created_at", json!("2026-09-27T10:15:02.115Z")),
        ("expires_at", json!("2026-09-27T10:15:02.113Z")),
        ("detail_available", json!(false)),
        ("replica_id", json!("1a2b3c4d")),
    ] {
        let mut record = original.clone();
        record[key] = value;
        let bytes = serde_json::to_vec(&record).unwrap();
        let result = bind_record(&bytes, &observation);
        assert!(result.is_err(), "{key}");
    }
    let original = fixture("valid/replica-tagged-reference.json");
    let observation = parse_observation(&crate::observation(&original)).unwrap();
    for replica in [Value::Null, json!("aaaaaaaa"), json!("1A2B3C4D")] {
        let mut record = original.clone();
        record["replica_id"] = replica;
        let bytes = serde_json::to_vec(&record).unwrap();
        let result = bind_record(&bytes, &observation);
        assert!(result.is_err());
    }
}

#[test]
fn bounded_observations_require_explicit_absence_and_no_trust_flags() {
    let record = fixture("valid/connection-failure.json");
    let original: Value = serde_json::from_slice(&observation(&record)).unwrap();
    for (key, value) in [
        ("reference", json!("../metrics")),
        ("status", json!(99)),
        ("protocol", json!("http4")),
        ("request_started_at", json!("2026-09-27T10:00:00Z")),
        ("response_received_at", json!("2026-09-27T10:15:01Z")),
        ("authenticated", json!(true)),
    ] {
        let mut value_map = original.clone();
        value_map[key] = value;
        assert!(
            parse_observation(&serde_json::to_vec(&value_map).unwrap()).is_err(),
            "{key}"
        );
    }
    let mut missing = original;
    missing.as_object_mut().unwrap().remove("gateway_error");
    assert!(parse_observation(&serde_json::to_vec(&missing).unwrap()).is_err());
    assert!(parse_observation(&vec![b' '; edge_record::MAX_RECORD_BYTES + 1]).is_err());
}

#[test]
fn unknown_vocabulary_never_supports_confirmation_and_is_not_echoed() {
    let mut record = fixture("valid/connection-failure.json");
    let observation = parse_observation(&observation(&record)).unwrap();
    record["detail"]["error_class"] = json!("future_sensitive_class");
    let bytes = serde_json::to_vec(&record).unwrap();
    let bound = bind_record(&bytes, &observation).unwrap();
    assert!(!bound.known_vocabulary());
    let finding = bound.finding();
    assert!(
        !serde_json::to_string(&finding)
            .unwrap()
            .contains("future_sensitive_class")
    );
    assert_eq!(finding.confidence, Confidence::Likely);
    record["gateway_error"] = json!("future_token");
    let bytes = serde_json::to_vec(&record).unwrap();
    let result = bind_record(&bytes, &observation);
    assert!(result.is_err());
}

#[test]
fn detail_shape_and_attempt_bounds_are_enforced_before_binding() {
    let original = fixture("valid/connection-failure.json");
    let observation = parse_observation(&observation(&original)).unwrap();
    for (key, value) in [
        ("backend_dispatch", json!("future_dispatch")),
        ("duration_bucket", json!("future_bucket")),
        ("rejection_phase", json!("future_phase")),
        ("route_timeout_phase", json!("future_phase")),
        ("attempts_omitted", json!(0)),
        ("proxy_id", json!(["invalid"])),
        ("attempts", json!(null)),
        (
            "attempts",
            json!([{"attempt": 0, "backend_dispatch": "pre_wire_failure"}]),
        ),
        (
            "attempts",
            json!([{"attempt": 1, "backend_dispatch": "not_dispatched"}]),
        ),
        (
            "attempts",
            json!([{"attempt": 1, "backend_dispatch": "backend_response", "status": 600}]),
        ),
    ] {
        let mut record = original.clone();
        record["detail"][key] = value;
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(bind_record(&bytes, &observation).is_err(), "{key}");
    }
    let mut record = original;
    let attempts = (1..=9)
        .map(|n| json!({ "attempt": n, "backend_dispatch": "pre_wire_failure" }))
        .collect::<Vec<_>>();
    record["detail"]["attempts"] = json!(attempts);
    let bytes = serde_json::to_vec(&record).unwrap();
    assert!(bind_record(&bytes, &observation).is_err());
    // Null detail is explicitly allowed by the released API and does not
    // establish any dispatch or timing fact.
    record["detail_available"] = json!(false);
    record["detail"] = Value::Null;
    let bytes = serde_json::to_vec(&record).unwrap();
    let bound = bind_record(&bytes, &observation).unwrap();
    assert!(
        bound
            .finding()
            .evidence
            .iter()
            .all(|e| !e.key.contains("detail"))
    );
}

#[test]
fn required_fields_and_interpreted_enums_track_the_released_schema() {
    let schema: Value = serde_json::from_slice(
        &std::fs::read(root().join("schemas/diagnostic-ref/v1.schema.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(schema["required"], json!(edge_record::REQUIRED_KEYS));
    assert_eq!(
        schema["$defs"]["Detail"]["required"],
        json!(edge_record::DETAIL_REQUIRED_KEYS)
    );
    for (path, values) in [
        ("/properties/protocol/enum", edge_record::PROTOCOLS),
        (
            "/$defs/Detail/properties/backend_dispatch/enum",
            edge_record::DISPATCH,
        ),
        (
            "/$defs/Detail/properties/duration_bucket/enum",
            edge_record::DURATION_BUCKETS,
        ),
        (
            "/$defs/Rejection/properties/source/enum",
            edge_record::REJECTION_SOURCES,
        ),
        (
            "/$defs/TlsDetail/properties/failure/enum",
            edge_record::TLS_FAILURES,
        ),
        (
            "/$defs/Attempt/properties/backend_dispatch/enum",
            &edge_record::DISPATCH[1..],
        ),
    ] {
        assert_eq!(schema.pointer(path).unwrap(), &json!(values), "{path}");
    }
    for (path, values) in [
        (
            "/$defs/Detail/properties/rejection_phase/enum",
            edge_record::REJECTION_PHASES,
        ),
        (
            "/$defs/Detail/properties/route_timeout_phase/enum",
            edge_record::ROUTE_TIMEOUT_PHASES,
        ),
    ] {
        let mut expected = serde_json::to_value(values).unwrap();
        expected.as_array_mut().unwrap().push(Value::Null);
        assert_eq!(schema.pointer(path).unwrap(), &expected, "{path}");
    }
    let vocabulary: Value = serde_json::from_slice(
        &std::fs::read(root().join("vocabularies/gateway-errors.json")).unwrap(),
    )
    .unwrap();
    let classes = vocabulary["error_classes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["value"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(classes, edge_record::ERROR_CLASSES);
}

#[test]
fn rfc3339_binding_handles_offsets_calendar_bounds_and_nanoseconds() {
    let record = fixture("valid/connection-failure.json");
    let mut value: Value = serde_json::from_slice(&observation(&record)).unwrap();
    value["request_started_at"] = json!("2026-09-27T12:15:02.114000000+02:00");
    let observation = parse_observation(&serde_json::to_vec(&value).unwrap()).unwrap();
    let bytes = serde_json::to_vec(&record).unwrap();
    let result = bind_record(&bytes, &observation);
    assert!(result.is_ok());
    for date in [
        "2026-02-29T10:15:02Z",
        "2026-09-31T10:15:02Z",
        "2026-09-27T24:15:02Z",
        "2026-09-27T10:15:60Z",
        "2026-09-27T10:15:02-00:00",
        "2026-09-27T10:15:02+24:00",
        "2026-09-27T10:15:02.1234567890Z",
        "2026-09-27T10:15:02.Z",
    ] {
        value["request_started_at"] = json!(date);
        assert!(
            parse_observation(&serde_json::to_vec(&value).unwrap()).is_err(),
            "{date}"
        );
    }
}
