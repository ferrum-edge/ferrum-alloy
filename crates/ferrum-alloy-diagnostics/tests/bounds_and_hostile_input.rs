#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::parse::{Limits, ReportError, parse_offline};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use serde_json::{Value, json};

fn base() -> Value {
    json!({
        "schema": "ferrum.diagnostic_report",
        "schema_version": "1.0",
        "collection": {
            "collector": { "kind": "collector", "name": "test" },
            "method": "fixture",
            "verification": "unverified"
        },
        "observations": []
    })
}

fn observation(id: &str) -> Value {
    json!({
        "id": id,
        "producer": { "kind": "alloy", "name": "t" },
        "kind": "measurement",
        "name": "alloy.server.time_to_headers",
        "availability": "measured",
        "value": 1.0,
        "unit": "ms",
        "scope": { "leg": "service" },
        "trust": "unverified"
    })
}

fn parse(value: &Value) -> Result<ferrum_alloy_diagnostics::ParsedReport, ReportError> {
    parse_offline(&serde_json::to_vec(value).unwrap(), &Limits::default())
}

fn validation_messages(error: ReportError) -> Vec<String> {
    match error {
        ReportError::Invalid(issues) => issues
            .into_iter()
            .map(|i| format!("{} {}", i.path, i.message))
            .collect(),
        other => panic!("expected validation errors, got {other:?}"),
    }
}

#[test]
fn oversized_input_is_rejected_before_parsing() {
    let limits = Limits {
        max_bytes: 64,
        ..Limits::default()
    };
    let input = vec![b' '; 65];
    assert!(matches!(
        parse_offline(&input, &limits),
        Err(ReportError::TooLarge(_))
    ));
}

#[test]
fn deep_nesting_is_rejected_without_recursion() {
    let mut input = String::from("{\"schema\":");
    input.push_str(&"[".repeat(10_000));
    input.push_str(&"]".repeat(10_000));
    input.push('}');
    assert!(matches!(
        parse_offline(input.as_bytes(), &Limits::default()),
        Err(ReportError::TooLarge(message)) if message.contains("nesting")
    ));
}

#[test]
fn brackets_inside_strings_do_not_count_as_nesting() {
    let mut report = base();
    report["report_id"] = json!("[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[\\\"");
    assert!(parse(&report).is_ok());
}

#[test]
fn long_strings_are_rejected() {
    let mut report = base();
    report["report_id"] = json!("x".repeat(4_096));
    assert!(matches!(parse(&report), Err(ReportError::TooLarge(_))));
}

#[test]
fn too_many_observations_are_rejected() {
    let mut report = base();
    let limits = Limits {
        max_observations: 3,
        ..Limits::default()
    };
    report["observations"] = json!(
        (0..4)
            .map(|i| observation(&format!("o{i}")))
            .collect::<Vec<_>>()
    );
    let error = parse_offline(&serde_json::to_vec(&report).unwrap(), &limits).unwrap_err();
    assert!(
        validation_messages(error)
            .iter()
            .any(|m| m.contains("exceed the limit"))
    );
}

#[test]
fn duplicate_and_malformed_ids_are_rejected() {
    let mut report = base();
    report["observations"] = json!([
        observation("a"),
        observation("a"),
        observation("bad id with spaces")
    ]);
    let messages = validation_messages(parse(&report).unwrap_err());
    assert!(messages.iter().any(|m| m.contains("duplicate id")));
    assert!(messages.iter().any(|m| m.contains("must match")));
}

#[test]
fn span_ids_must_be_lowercase_hex_and_nonzero() {
    let mut report = base();
    let mut o = observation("o1");
    o["span"] =
        json!({ "trace_id": "00000000000000000000000000000000", "span_id": "ABCDEF0123456789" });
    report["observations"] = json!([o]);
    let messages = validation_messages(parse(&report).unwrap_err());
    assert!(messages.iter().any(|m| m.contains("/span/trace_id")));
    assert!(messages.iter().any(|m| m.contains("/span/span_id")));
}

#[test]
fn measured_values_need_value_and_unit() {
    let mut report = base();
    let mut missing_value = observation("o1");
    missing_value.as_object_mut().unwrap().remove("value");
    let mut missing_unit = observation("o2");
    missing_unit.as_object_mut().unwrap().remove("unit");
    report["observations"] = json!([missing_value, missing_unit]);
    let messages = validation_messages(parse(&report).unwrap_err());
    assert!(messages.iter().any(|m| m.contains("needs a value")));
    assert!(messages.iter().any(|m| m.contains("needs a unit")));
}

#[test]
fn inverted_intervals_and_huge_time_ranges_are_rejected() {
    let mut report = base();
    let mut inverted = observation("o1");
    inverted["interval"] = json!({ "start_unix_nano": 10, "end_unix_nano": 5 });
    let mut far = observation("o2");
    far["interval"] = json!({ "start_unix_nano": 1, "end_unix_nano": 900_000_000_000_000_000u64 });
    report["observations"] = json!([inverted, far]);
    let messages = validation_messages(parse(&report).unwrap_err());
    assert!(messages.iter().any(|m| m.contains("end precedes start")));
    assert!(messages.iter().any(|m| m.contains("time range")));
}

#[test]
fn wrong_schema_and_bad_versions_are_rejected() {
    let mut report = base();
    report["schema"] = json!("something.else");
    assert!(matches!(
        parse(&report),
        Err(ReportError::WrongSchema { .. })
    ));
    for version in ["1", "1.x", "01.0.0", "", "99999.0"] {
        let mut report = base();
        report["schema_version"] = json!(version);
        assert!(
            matches!(parse(&report), Err(ReportError::UnsupportedVersion { .. })),
            "version {version:?}"
        );
    }
}

#[test]
fn unavailable_values_are_not_treated_as_zero() {
    let mut report = base();
    let mut unavailable = observation("o1");
    unavailable["availability"] = json!("unavailable");
    report["observations"] = json!([unavailable]);
    let parsed = parse(&report).unwrap();
    assert_eq!(parsed.report.observations[0].duration_ms(), None);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.message.contains("value ignored"))
    );
}

/// Small deterministic PRNG so the mutation test needs no extra dependency.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn mutated_fixtures_never_panic() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/reports");
    let mut rng = XorShift(0x5eed_1234_abcd_ef01);
    let interesting = b"{}[]\":,0-9.eE\\ntfu";
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let original = std::fs::read(&path).unwrap();
        for _ in 0..400 {
            let mut input = original.clone();
            for _ in 0..(1 + rng.next() % 4) {
                let index = (rng.next() as usize) % input.len();
                match rng.next() % 3 {
                    0 => input[index] = interesting[(rng.next() as usize) % interesting.len()],
                    1 => {
                        input.remove(index);
                    }
                    _ => input.insert(
                        index,
                        interesting[(rng.next() as usize) % interesting.len()],
                    ),
                }
            }
            if let Ok(parsed) = parse_offline(&input, &Limits::default()) {
                // Analysis of any accepted report must also be panic-free.
                let _ = analyze(&parsed.report, &Thresholds::default());
            }
        }
    }
}
