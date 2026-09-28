#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::Finding;
use ferrum_alloy_diagnostics::parse::{Limits, ReportError, parse_offline};
use ferrum_alloy_diagnostics::rules::{
    MAX_DEGRADED_CITATIONS_PER_FINDING, MAX_DEGRADED_CITATIONS_PER_RUN, Thresholds, analyze,
};
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

const TRACE: &str = "6c9f0a1b2c3d4e5f60718293a4b5c6d7";

/// Span id number `index` in the id range `range`, one hex digit.
fn span_id(range: char, index: usize) -> String {
    format!("{range}{index:015x}")
}

/// A measured gateway backend exchange on span `index` of range `1`.
fn gateway_ttfb(index: usize) -> Value {
    json!({
        "id": format!("edge-{index}"),
        "producer": { "kind": "edge", "name": "ferrum-edge" },
        "kind": "measurement",
        "name": "edge.backend.time_to_headers",
        "availability": "measured",
        "value": 100.0,
        "unit": "ms",
        "scope": { "leg": "gateway_to_service" },
        "span": { "trace_id": TRACE, "span_id": span_id('1', index) },
        "trust": "unverified"
    })
}

/// An Alloy operation with the given availability and no span.
fn alloy_spanless(id: &str, availability: &str) -> Value {
    json!({
        "id": id,
        "producer": { "kind": "alloy", "name": "t" },
        "kind": "measurement",
        "name": "alloy.operation.duration",
        "availability": availability,
        "scope": { "leg": "service" },
        "trust": "unverified"
    })
}

/// An Alloy operation with the given availability on `span` under `parent`.
fn alloy_span(id: &str, availability: &str, span: &str, parent: &str) -> Value {
    let mut observation = alloy_spanless(id, availability);
    observation["span"] = json!({ "trace_id": TRACE, "span_id": span, "parent_span_id": parent });
    observation
}

/// Evidence rows that cite a not-sampled observation.
fn degraded_citations(finding: &Finding) -> usize {
    finding
        .evidence
        .iter()
        .filter(|evidence| evidence.value == "not_sampled")
        .count()
}

/// The count in a finding's "N more degraded observations are not cited" note.
fn omitted_citations(finding: &Finding) -> usize {
    let words: Vec<&str> = finding.explanation.split_whitespace().collect();
    words
        .windows(2)
        .find(|pair| pair[1] == "more")
        .map_or(0, |pair| pair[0].parse().unwrap())
}

fn cites(finding: &Finding, id: &str) -> bool {
    finding
        .supporting_observations
        .iter()
        .any(|cited| cited == id)
}

#[test]
fn degraded_citations_stay_within_explicit_caps_at_hostile_scale() {
    const GATEWAYS: usize = 1_500;
    const CHAIN: usize = 200;
    const EXTRA: usize = 100;
    const SPANLESS: usize = 200;
    let first_gateway = span_id('1', 0);
    let mut observations: Vec<Value> = (0..GATEWAYS).map(gateway_ttfb).collect();

    // A parent chain far deeper than the lineage walk, rooted at gateway 0.
    for index in 0..CHAIN {
        let parent = if index == 0 {
            first_gateway.clone()
        } else {
            span_id('2', index - 1)
        };
        let id = format!("chain-{index}");
        let node = alloy_span(&id, "unavailable", &span_id('2', index), &parent);
        observations.push(node);
    }
    // Two spans that are each other's parent.
    let (cycle_a, cycle_b) = (span_id('3', 1), span_id('3', 2));
    observations.push(alloy_span("cycle-a", "unavailable", &cycle_a, &cycle_b));
    observations.push(alloy_span("cycle-b", "unavailable", &cycle_b, &cycle_a));

    // Degraded evidence reaching gateway 0 through the chain, beyond the
    // walk's reach, inside the cycle, without a span, piled on gateway 0,
    // and one under every gateway.
    let near = alloy_span("near", "not_sampled", &span_id('4', 0), &span_id('2', 9));
    observations.push(near);
    let deepest = span_id('2', CHAIN - 1);
    let far = alloy_span("far", "not_sampled", &span_id('4', 1), &deepest);
    observations.push(far);
    let looping = alloy_span("looping", "not_sampled", &span_id('4', 2), &cycle_a);
    observations.push(looping);
    for index in 0..SPANLESS {
        let id = format!("spanless-{index}");
        observations.push(alloy_spanless(&id, "not_sampled"));
    }
    for index in 0..EXTRA {
        let id = format!("extra-{index}");
        let node = alloy_span(&id, "not_sampled", &span_id('5', index), &first_gateway);
        observations.push(node);
    }
    for index in 0..GATEWAYS {
        let id = format!("degraded-{index}");
        let gateway = span_id('1', index);
        let node = alloy_span(&id, "not_sampled", &span_id('6', index), &gateway);
        observations.push(node);
    }
    let degraded = 3 + SPANLESS + EXTRA + GATEWAYS;

    let mut report = base();
    report["observations"] = Value::Array(observations);
    let parsed = parse(&report).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let r004: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.rule_id == "alloy.r004")
        .collect();

    assert_eq!(r004.len(), GATEWAYS + 1);
    for finding in &r004 {
        assert!(degraded_citations(finding) <= MAX_DEGRADED_CITATIONS_PER_FINDING);
        assert!(finding.evidence.len() <= MAX_DEGRADED_CITATIONS_PER_FINDING + 1);
    }
    let cited: usize = r004.iter().copied().map(degraded_citations).sum();
    let omitted: usize = r004.iter().copied().map(omitted_citations).sum();
    assert_eq!(cited, MAX_DEGRADED_CITATIONS_PER_RUN);
    assert_eq!(
        cited + omitted,
        degraded,
        "every degraded observation is either cited or counted as not cited"
    );

    let first = r004.iter().find(|f| cites(f, "edge-0")).unwrap();
    assert!(
        cites(first, "near"),
        "the bounded walk still follows the chain"
    );
    assert_eq!(
        degraded_citations(first),
        MAX_DEGRADED_CITATIONS_PER_FINDING
    );
    assert!(
        first
            .explanation
            .contains("more degraded observations are not cited"),
        "{}",
        first.explanation
    );

    let unlinked = r004
        .iter()
        .find(|finding| finding.code == "alloy.telemetry.degraded_evidence_unlinked")
        .unwrap();
    assert_eq!(
        degraded_citations(unlinked) + omitted_citations(unlinked),
        SPANLESS + 2,
        "span-less, too-deep, and cyclic evidence is reported, not dropped"
    );
}

#[test]
fn insufficient_telemetry_cites_degraded_evidence_within_the_cap() {
    let extra = 5;
    let observations: Vec<Value> = (0..MAX_DEGRADED_CITATIONS_PER_FINDING + extra)
        .map(|index| alloy_spanless(&format!("d{index}"), "not_sampled"))
        .collect();
    let mut report = base();
    report["observations"] = Value::Array(observations);
    let parsed = parse(&report).unwrap();
    let findings = analyze(&parsed.report, &Thresholds::default());
    let finding = findings
        .iter()
        .find(|finding| finding.code == "alloy.telemetry.insufficient")
        .unwrap();

    assert_eq!(finding.rule_version, 2);
    assert_eq!(
        degraded_citations(finding),
        MAX_DEGRADED_CITATIONS_PER_FINDING
    );
    assert_eq!(omitted_citations(finding), extra);
    assert!(
        finding
            .explanation
            .contains("5 more degraded observations are not cited;"),
        "{}",
        finding.explanation
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
