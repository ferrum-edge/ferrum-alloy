//! OTLP/JSON trace exports are read from files.

#![no_main]

use ferrum_alloy_diagnostics::model::{Confidence, Producer, ProducerKind};
use ferrum_alloy_diagnostics::otlp::{ImportLimits, import, trace_ids};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::{Thresholds, analyze};
use libfuzzer_sys::fuzz_target;

fn collector() -> Producer {
    Producer {
        kind: ProducerKind::Collector,
        name: "ferrum-alloy-fuzz".into(),
        version: None,
        instance: None,
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let limits = ImportLimits::default();
    let mut reports = Vec::new();
    if let Ok(report) = import(text, None, collector(), &limits) {
        reports.push(report);
    }
    if let Ok(ids) = trace_ids(text, &limits) {
        // Bounded: selecting every trace of a large input adds no coverage.
        for id in ids.iter().take(4) {
            if let Ok(report) = import(text, Some(id), collector(), &limits) {
                reports.push(report);
            }
        }
    }
    let thresholds = Thresholds::default();
    for report in &reports {
        let findings = analyze(report, &thresholds);
        for finding in &findings {
            // An imported file is unverified, like any offline input.
            assert_ne!(finding.confidence, Confidence::Confirmed, "{}", finding.code);
            assert!(!finding.does_not_prove.is_empty(), "{}", finding.code);
        }
        assert_eq!(analyze(report, &thresholds), findings);
        let rendered = render_text(report, &findings, &[]);
        assert_eq!(render_text(report, &findings, &[]), rendered);
    }
});
