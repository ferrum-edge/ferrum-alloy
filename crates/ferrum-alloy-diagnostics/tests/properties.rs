//! Property tests over mutated contract fixtures: offline input never yields
//! `confirmed`, and parsing, rules, and rendering are deterministic.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::LazyLock;

use ferrum_alloy_diagnostics::model::{Confidence, Trust, Verification};
use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use proptest::prelude::*;
use proptest::sample::Index;
use serde_json::{Value, json};

/// Every report fixture the offline parser accepts unmodified.
static FIXTURES: LazyLock<Vec<Value>> = LazyLock::new(|| {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = root.join("../../contracts/fixtures/reports");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .filter(|bytes| parse_offline(bytes, &Limits::default()).is_ok())
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .collect()
});

/// What a report file may claim about its own collection.
const CLAIMS: &[&str] = &["unverified", "verified", "signed"];

/// One observation change: target, claim verified trust, scale the value
/// (possibly below zero), mark it unavailable, shift and stretch its interval.
type Mutation = (Index, bool, Option<f64>, bool, i64, u64);

fn mutate(report: &mut Value, claim: &str, mutations: &[Mutation]) {
    report["collection"]["verification"] = json!(claim);
    let Some(observations) = report["observations"].as_array_mut() else {
        return;
    };
    if observations.is_empty() {
        return;
    }
    let len = observations.len();
    for (target, verified, scale, unavailable, shift, stretch) in mutations {
        let observation = &mut observations[target.index(len)];
        if *verified {
            observation["trust"] = json!("verified");
        }
        if *unavailable {
            // Unknown is not zero: an unavailable measurement has no value.
            observation["availability"] = json!("unavailable");
            if let Some(fields) = observation.as_object_mut() {
                fields.remove("value");
            }
        } else if let (Some(scale), Some(value)) = (scale, observation["value"].as_f64()) {
            observation["value"] = json!(value * scale);
        }
        if let Some(interval) = observation.get_mut("interval") {
            let start = interval["start_unix_nano"].as_u64().unwrap_or(0);
            let end = interval["end_unix_nano"].as_u64().unwrap_or(start);
            let start = start.saturating_add_signed(*shift);
            let end = end.saturating_add_signed(*shift).saturating_add(*stretch);
            interval["start_unix_nano"] = json!(start);
            interval["end_unix_nano"] = json!(end);
        }
    }
}

#[test]
fn fixtures_are_available_for_mutation() {
    assert!(FIXTURES.len() >= 5, "{} usable fixtures", FIXTURES.len());
}

proptest! {
    #[test]
    fn offline_reports_are_never_confirmed_and_diagnosis_is_deterministic(
        fixture in any::<Index>(),
        claim in 0..CLAIMS.len(),
        mutations in prop::collection::vec(
            (
                any::<Index>(),
                any::<bool>(),
                prop::option::of(-1.5f64..3.0),
                prop::bool::weighted(0.15),
                -5_000_000_000i64..5_000_000_000,
                0u64..5_000_000_000,
            ),
            0..12,
        )
    ) {
        let mut report = FIXTURES[fixture.index(FIXTURES.len())].clone();
        mutate(&mut report, CLAIMS[claim], &mutations);
        let bytes = serde_json::to_vec(&report).unwrap();
        let Ok(parsed) = parse_offline(&bytes, &Limits::default()) else {
            // Rejected input is covered by the fuzz target; nothing to diagnose.
            return Ok(());
        };

        // Offline provenance is always downgraded.
        prop_assert_eq!(&parsed.report.collection.verification, &Verification::Unverified);
        prop_assert!(parsed.report.observations.iter().all(|o| o.trust != Trust::Verified));

        let thresholds = Thresholds::default();
        let findings = analyze(&parsed.report, &thresholds);
        let observations = &parsed.report.observations;
        let ids: BTreeSet<&str> = observations.iter().map(|o| o.id.as_str()).collect();
        for finding in &findings {
            prop_assert_ne!(&finding.confidence, &Confidence::Confirmed, "{}", finding.code);
            prop_assert!(!finding.does_not_prove.is_empty(), "{}", finding.code);
            for id in &finding.supporting_observations {
                prop_assert!(ids.contains(id.as_str()), "{} cites unknown {}", finding.code, id);
            }
        }

        // Same input, same findings, same text.
        prop_assert_eq!(&analyze(&parsed.report, &thresholds), &findings);
        let again = parse_offline(&bytes, &Limits::default()).unwrap();
        prop_assert_eq!(&analyze(&again.report, &thresholds), &findings);
        let text = render_text(&parsed.report, &findings, &parsed.warnings);
        prop_assert_eq!(render_text(&again.report, &findings, &again.warnings), text);
    }
}
