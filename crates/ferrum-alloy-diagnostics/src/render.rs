//! Deterministic human-readable rendering.
//!
//! The output is line oriented, and reports read from files are untrusted,
//! so every interpolated value is written on one line: control characters
//! other than tab, plus the Unicode line and paragraph separators, are
//! written as escape sequences (`\n`, `\u{1b}`, `\u{2028}`, ...). Only the
//! renderer's own structure breaks lines, so a report value cannot forge a
//! warning, finding, or header line. This guarantee also applies to library
//! callers that print the result directly.

use std::fmt::{self, Write as _};

use crate::model::{DiagnosticReport, Finding};
use crate::parse::Issue;

/// Renders findings as plain text. Output depends only on the inputs.
///
/// Values taken from `report`, `findings`, and `warnings` have control
/// characters escaped (except tab), so library callers can print the result
/// directly without a report value forging terminal control sequences or
/// additional lines. See the module documentation.
pub fn render_text(report: &DiagnosticReport, findings: &[Finding], warnings: &[Issue]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Ferrum Alloy diagnosis");
    let _ = writeln!(
        out,
        "  report schema: {} {}",
        OneLine(&report.schema),
        OneLine(&report.schema_version)
    );
    let _ = writeln!(
        out,
        "  collected by: {} ({}), method {}, provenance {}",
        OneLine(&report.collection.collector.name),
        OneLine(&report.collection.collector.kind),
        OneLine(&report.collection.method),
        OneLine(&report.collection.verification)
    );
    if let Some(trace) = &report.subject.trace_id {
        let _ = writeln!(out, "  trace: {}", OneLine(trace));
    }
    if let Some(request) = &report.subject.request_id {
        let _ = writeln!(out, "  request id: {}", OneLine(request));
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
            let (path, message) = (OneLine(path), OneLine(&warning.message));
            let _ = writeln!(out, "  - {path}: {message}");
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
            OneLine(&finding.severity),
            OneLine(&finding.title),
            OneLine(&finding.code)
        );
        let _ = writeln!(
            out,
            "   confidence: {}   scope: {}   owner: {}   rule: {} v{}",
            OneLine(&finding.confidence),
            OneLine(&finding.scope),
            OneLine(&finding.owner),
            OneLine(&finding.rule_id),
            finding.rule_version
        );
        let _ = writeln!(out, "   {}", OneLine(&finding.explanation));
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
        let _ = writeln!(out, "     - {}", OneLine(&item));
    }
}

/// Displays a value on one line: every character that could start a new
/// line is written as its escape sequence instead.
struct OneLine<T>(T);

impl<T: fmt::Display> fmt::Display for OneLine<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(Escaped(f), "{}", self.0)
    }
}

/// Writes through to a formatter with control and line-separator characters
/// escaped.
struct Escaped<'a, 'b>(&'a mut fmt::Formatter<'b>);

impl fmt::Write for Escaped<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut rest = text;
        while let Some(index) = rest.find(should_escape) {
            let (clean, tail) = rest.split_at(index);
            self.0.write_str(clean)?;
            let mut chars = tail.chars();
            if let Some(c) = chars.next() {
                write!(self.0, "{}", c.escape_default())?;
            }
            rest = chars.as_str();
        }
        self.0.write_str(rest)
    }
}

/// Escape every control character except tab, and the Unicode line and
/// paragraph separators.
fn should_escape(c: char) -> bool {
    (c.is_control() && c != '\t') || matches!(c, '\u{2028}' | '\u{2029}')
}
