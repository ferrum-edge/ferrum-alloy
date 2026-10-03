//! Diagnostic report files can come from anyone.

#![no_main]

use ferrum_alloy_diagnostics::model::Confidence;
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::{Limits, Thresholds, analyze, parse_offline};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(parsed) = parse_offline(data, &Limits::default()) else {
        return;
    };
    assert!(parsed.report.findings.is_empty());
    let thresholds = Thresholds::default();
    let findings = analyze(&parsed.report, &thresholds);
    for finding in &findings {
        // Offline input cannot be authenticated.
        assert_ne!(finding.confidence, Confidence::Confirmed, "{}", finding.code);
        assert!(!finding.does_not_prove.is_empty(), "{}", finding.code);
    }
    // Rules and rendering depend only on their inputs.
    assert_eq!(analyze(&parsed.report, &thresholds), findings);
    let text = render_text(&parsed.report, &findings, &parsed.warnings);
    assert_eq!(render_text(&parsed.report, &findings, &parsed.warnings), text);
});
