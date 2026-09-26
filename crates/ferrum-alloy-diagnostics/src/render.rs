//! Deterministic human-readable rendering.

use std::fmt::Write as _;

use crate::model::{DiagnosticReport, Finding};
use crate::parse::Issue;

/// Renders findings as plain text. Output depends only on the inputs.
pub fn render_text(report: &DiagnosticReport, findings: &[Finding], warnings: &[Issue]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Ferrum Alloy diagnosis");
    let _ = writeln!(
        out,
        "  report schema: {} {}",
        report.schema, report.schema_version
    );
    let _ = writeln!(
        out,
        "  collected by: {} ({}), method {}, provenance {}",
        report.collection.collector.name,
        report.collection.collector.kind,
        report.collection.method,
        report.collection.verification
    );
    if let Some(trace) = &report.subject.trace_id {
        let _ = writeln!(out, "  trace: {trace}");
    }
    if let Some(request) = &report.subject.request_id {
        let _ = writeln!(out, "  request id: {request}");
    }
    let _ = writeln!(out, "  observations: {}", report.observations.len());
    if report.collection.verification.as_str() != "verified" {
        let _ = writeln!(
            out,
            "  note: evidence provenance is unverified; findings describe the supplied evidence and do not authenticate it."
        );
    }

    if !warnings.is_empty() {
        let _ = writeln!(out, "\nWarnings ({}):", warnings.len());
        for warning in warnings {
            let path = if warning.path.is_empty() {
                "/"
            } else {
                &warning.path
            };
            let _ = writeln!(out, "  - {path}: {}", warning.message);
        }
    }

    if findings.is_empty() {
        let _ = writeln!(out, "\nNo findings.");
        return out;
    }
    let _ = writeln!(out, "\nFindings ({}):", findings.len());
    for (index, finding) in findings.iter().enumerate() {
        let _ = writeln!(
            out,
            "\n{}. [{}] {} ({})",
            index + 1,
            finding.severity,
            finding.title,
            finding.code
        );
        let _ = writeln!(
            out,
            "   confidence: {}   scope: {}   owner: {}   rule: {} v{}",
            finding.confidence, finding.scope, finding.owner, finding.rule_id, finding.rule_version
        );
        let _ = writeln!(out, "   {}", finding.explanation);
        section(
            &mut out,
            "Evidence",
            finding
                .evidence
                .iter()
                .map(|e| format!("{} {} = {}", e.source, e.key, e.value)),
        );
        section(
            &mut out,
            "Other explanations",
            finding.alternatives.iter().cloned(),
        );
        section(
            &mut out,
            "Does not prove",
            finding.does_not_prove.iter().cloned(),
        );
        section(
            &mut out,
            "Missing evidence",
            finding.missing_evidence.iter().cloned(),
        );
        section(
            &mut out,
            "Next checks",
            finding.confirm_with.iter().cloned(),
        );
        section(
            &mut out,
            "Suggested actions",
            finding
                .remediation
                .iter()
                .map(|r| format!("{} (owner: {})", r.text, r.owner)),
        );
    }
    out
}

fn section(out: &mut String, title: &str, items: impl Iterator<Item = String>) {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "   {title}:");
    for item in items {
        let _ = writeln!(out, "     - {item}");
    }
}
