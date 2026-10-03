//! Untrusted report values cannot inject controls into the human rendering.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use ferrum_alloy_diagnostics::model::{Finding, Owner, Remediation};
use ferrum_alloy_diagnostics::parse::{Issue, Limits, parse_offline};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use serde_json::{Value, json};

/// C0, DEL, C1, and Unicode line separators, placed so line-breaking values
/// would start forged warning or finding lines.
const INJECTED: &str = concat!(
    "x\0\u{1}\u{8}\t\u{0B}\u{0C}\n\u{0E}\u{1F}",
    "\r\u{7F}\u{80}\u{85}\u{9F}\u{2028}\u{2029}end"
);

/// How [`INJECTED`] must appear in the rendering: on one line, escaped.
const ESCAPED: &str = concat!(
    "x\\u{0}\\u{1}\\u{8}\t\\u{b}\\u{c}\\n\\u{e}\\u{1f}",
    "\\r\\u{7f}\\u{80}\\u{85}\\u{9f}\\u{2028}\\u{2029}end"
);

fn is_break(c: char) -> bool {
    (c.is_control() && c != '\t') || matches!(c, '\u{2028}' | '\u{2029}')
}

fn fixture() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/reports/unattributed-interval.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Appends `text` to every string a finding renders.
fn taint(finding: &mut Finding, text: &str) {
    finding.title.push_str(text);
    finding.explanation.push_str(text);
    for evidence in &mut finding.evidence {
        evidence.key.push_str(text);
        evidence.value.push_str(text);
    }
    for list in [
        &mut finding.alternatives,
        &mut finding.does_not_prove,
        &mut finding.missing_evidence,
        &mut finding.confirm_with,
    ] {
        list.push(format!("item{text}"));
    }
    finding.remediation.push(Remediation {
        text: format!("action{text}"),
        owner: Owner::Unknown,
    });
}

/// Renders the unattributed-interval fixture with `text` in every report
/// field, warning, and finding string the renderer prints.
fn render_with(text: &str) -> String {
    let mut report = fixture();
    report["collection"]["collector"]["name"] = json!(format!("fixture{text}"));
    report["collection"]["collector"]["kind"] = json!(format!("kind{text}"));
    report["collection"]["method"] = json!(format!("method{text}"));
    report["subject"]["request_id"] = json!(format!("request{text}"));
    report[format!("x-field{text}")] = json!(true);
    let bytes = serde_json::to_vec(&report).unwrap();
    let parsed = parse_offline(&bytes, &Limits::default()).unwrap();

    let mut findings = analyze(&parsed.report, &Thresholds::default());
    assert!(!findings.is_empty());
    for finding in &mut findings {
        taint(finding, text);
    }
    let mut warnings = parsed.warnings;
    warnings.push(Issue {
        path: format!("/path{text}"),
        message: format!("message{text}"),
    });
    render_text(&parsed.report, &findings, &warnings)
}

#[test]
fn untrusted_values_stay_on_one_escaped_line() {
    let plain = render_with("-plain");
    let hostile = render_with(INJECTED);

    let plain_lines: Vec<&str> = plain.split('\n').collect();
    let hostile_lines: Vec<&str> = hostile.split(is_break).collect();
    assert_eq!(
        hostile_lines.len(),
        plain_lines.len(),
        "a value added lines:\n{hostile}"
    );
    for (line, expected) in hostile_lines.iter().zip(&plain_lines) {
        assert_eq!(
            line.replace(ESCAPED, "-plain"),
            *expected,
            "only the injected value differs, escaped in place"
        );
    }
    assert_eq!(
        hostile.matches(ESCAPED).count(),
        plain.matches("-plain").count()
    );
    for forged in ["  - /forged", "  confidence: confirmed"] {
        assert!(
            !hostile_lines.iter().any(|line| line.starts_with(forged)),
            "{forged:?} starts a line:\n{hostile}"
        );
    }
}

#[test]
fn renderer_structure_and_other_text_are_unchanged() {
    let text = "\t café 東京 ✓ \\n \u{1b}[31m";
    let rendered = render_with(text);
    let collected = "collected by: fixture\t café 東京 ✓ \\n \\u{1b}[31m (kind";
    assert!(rendered.contains(collected), "{rendered}");
    assert!(rendered.contains(r"\u{1b}[31m"), "{rendered}");
    let header = "Ferrum Alloy diagnosis\n  report schema: ";
    assert!(rendered.starts_with(header), "{rendered}");
    for section in [
        "\n\nWarnings (",
        "\n\nFindings (",
        "\n   Evidence:\n     - ",
    ] {
        assert!(rendered.contains(section), "{section:?}");
    }
    assert!(rendered.ends_with('\n'));
    assert!(!rendered.contains('\r'));
}
