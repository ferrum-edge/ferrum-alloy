//! `ferrum-alloy diagnose`: explains supplied evidence.
//!
//! Deterministic rules only, and no external AI service. The only network
//! access is `--url`, which fetches one live report from a running service
//! (see [`crate::live`]). Every input, live reports included, is read with
//! `parse_offline` and never treated as authenticated.

use std::io::Read;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use ferrum_alloy_diagnostics::model::{Producer, ProducerKind};
use ferrum_alloy_diagnostics::otlp::{self, ImportLimits};
use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};

use crate::Format;
use crate::error::CliError;

/// Arguments for `diagnose`.
#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("source").required(true).args(["input", "otlp", "url"]))]
pub(crate) struct DiagnoseArgs {
    /// A `ferrum.diagnostic_report` v1 JSON file.
    #[arg(long)]
    input: Option<PathBuf>,
    /// An OTLP/JSON trace export (OpenTelemetry Collector `file` exporter).
    #[arg(long)]
    otlp: Option<PathBuf>,
    /// Trace to analyze in an OTLP export with several traces.
    #[arg(long, requires = "otlp")]
    trace_id: Option<String>,
    /// List the traces in an OTLP export and exit.
    #[arg(long, requires = "otlp")]
    list_traces: bool,
    /// Fetch the live report of one request from a running service: the base
    /// URL of its management listener, for example `http://127.0.0.1:9090`.
    /// The credential comes from `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or
    /// `--token-file`, never from an argument.
    #[arg(long, requires = "request_id")]
    url: Option<String>,
    /// The request id to fetch with `--url`.
    #[arg(long, requires = "url")]
    request_id: Option<String>,
    /// A file holding the credential for `--url`, instead of
    /// `FERRUM_ALLOY_DIAGNOSTICS_TOKEN`.
    #[arg(long, requires = "url")]
    token_file: Option<PathBuf>,
    /// Whole-request timeout for `--url`, in milliseconds (1 to 120000).
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
    /// Write the assembled report (with findings) to this file.
    #[arg(long)]
    write_report: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

fn read(path: &Path, max: usize) -> Result<Vec<u8>, CliError> {
    let file = std::fs::File::open(path)
        .map_err(|e| CliError::Invalid(format!("{}: {e}", path.display())))?;
    let metadata = file
        .metadata()
        .map_err(|e| CliError::Invalid(format!("{}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(CliError::Invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > max as u64 {
        return Err(CliError::Invalid(format!(
            "{} is larger than {max} bytes",
            path.display()
        )));
    }

    // The handle metadata check rejects special files, while the bounded read
    // also covers a regular file growing after that check.
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| CliError::Invalid(format!("{}: {e}", path.display())))?;
    if bytes.len() > max {
        return Err(CliError::Invalid(format!(
            "{} is larger than {max} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Unicode format characters (general category `Cf`, Unicode 16.0), and the
/// line and paragraph separators. Bidirectional controls among them can
/// reorder the text around them, and others hide or join it.
const FORMAT_CHARACTERS: &[RangeInclusive<char>] = &[
    '\u{00AD}'..='\u{00AD}',
    '\u{0600}'..='\u{0605}',
    '\u{061C}'..='\u{061C}',
    '\u{06DD}'..='\u{06DD}',
    '\u{070F}'..='\u{070F}',
    '\u{0890}'..='\u{0891}',
    '\u{08E2}'..='\u{08E2}',
    '\u{180E}'..='\u{180E}',
    '\u{200B}'..='\u{200F}',
    '\u{2028}'..='\u{202E}',
    '\u{2060}'..='\u{206F}',
    '\u{FEFF}'..='\u{FEFF}',
    '\u{FFF9}'..='\u{FFFB}',
    '\u{110BD}'..='\u{110BD}',
    '\u{110CD}'..='\u{110CD}',
    '\u{13430}'..='\u{1343F}',
    '\u{1BCA0}'..='\u{1BCA3}',
    '\u{1D173}'..='\u{1D17A}',
    '\u{E0001}'..='\u{E0001}',
    '\u{E0020}'..='\u{E007F}',
];

/// Replaces control and format characters other than newlines. Reports may
/// come from a remote service, and none of their text may drive the
/// terminal or change how the text around it reads.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => c,
            c if c.is_control() => '?',
            c if FORMAT_CHARACTERS.iter().any(|range| range.contains(&c)) => '?',
            c => c,
        })
        .collect()
}

/// Runs `diagnose`.
pub(crate) fn run(args: DiagnoseArgs) -> Result<ExitCode, CliError> {
    let limits = Limits::default();
    let (mut report, warnings, claimed) = if let Some(path) = &args.input {
        let bytes = read(path, limits.max_bytes)?;
        let parsed = parse_offline(&bytes, &limits)
            .map_err(|e| CliError::Invalid(format!("{}: {e}", path.display())))?;
        (
            parsed.report,
            parsed.warnings,
            Some(parsed.claimed_verification),
        )
    } else if let Some(path) = &args.otlp {
        let import_limits = ImportLimits::default();
        let bytes = read(path, import_limits.max_bytes)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| CliError::Invalid(format!("{} is not UTF-8", path.display())))?;
        if args.list_traces {
            let ids = otlp::trace_ids(&text, &import_limits)
                .map_err(|e| CliError::Invalid(e.to_string()))?;
            let out = match args.format {
                Format::Json => format!("{:#}\n", serde_json::json!({ "trace_ids": ids })),
                Format::Human => ids.iter().map(|id| format!("{id}\n")).collect(),
            };
            crate::print(&out)?;
            return Ok(ExitCode::SUCCESS);
        }
        let collector = Producer {
            kind: ProducerKind::Collector,
            name: "ferrum-alloy-cli".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            instance: None,
        };
        let report = otlp::import(&text, args.trace_id.as_deref(), collector, &import_limits)
            .map_err(|e| CliError::Invalid(e.to_string()))?;
        (report, Vec::new(), None)
    } else if let Some(base) = &args.url {
        let Some(request_id) = args.request_id.as_deref() else {
            return Err(CliError::Invalid("--url requires --request-id".into()));
        };
        if !(1..=120_000).contains(&args.timeout_ms) {
            return Err(CliError::Invalid(
                "--timeout-ms must be within 1..=120000".into(),
            ));
        }
        let token = crate::live::token(args.token_file.as_deref())?;
        let url = crate::live::report_url(base, request_id, token.is_some())?;
        let timeout = Duration::from_millis(args.timeout_ms);
        let bytes = crate::live::fetch(url, token.as_deref(), timeout, limits.max_bytes)?;
        let parsed = parse_offline(&bytes, &limits)
            .map_err(|e| CliError::Invalid(format!("the live report: {e}")))?;
        (
            parsed.report,
            parsed.warnings,
            Some(parsed.claimed_verification),
        )
    } else {
        return Err(CliError::Invalid("pass --input, --otlp, or --url".into()));
    };

    let findings = analyze(&report, &Thresholds::default());
    report.findings.clone_from(&findings);
    if let Some(path) = &args.write_report {
        let json =
            serde_json::to_string_pretty(&report).map_err(|e| CliError::Io(e.to_string()))?;
        std::fs::write(path, format!("{json}\n"))
            .map_err(|e| CliError::Io(format!("write {}: {e}", path.display())))?;
    }
    match args.format {
        Format::Human => {
            let text = render_text(&report, &findings, &warnings);
            crate::print(&printable(&text))?;
        }
        Format::Json => {
            let value = serde_json::json!({
                "claimed_verification": claimed.map(|v| v.as_str().to_owned()),
                "warnings": warnings.iter().map(|w| serde_json::json!({ "path": w.path, "message": w.message })).collect::<Vec<_>>(),
                "report": report,
            });
            crate::print(&format!("{value:#}\n"))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_enforces_the_limit() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("growing.json");
        std::fs::write(&path, b"12345")?;

        let Err(error) = read(&path, 4) else {
            return Err("oversized input was accepted".into());
        };

        assert!(error.to_string().contains("is larger than 4 bytes"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_special_files() -> Result<(), Box<dyn std::error::Error>> {
        let Err(error) = read(Path::new("/dev/zero"), 4) else {
            return Err("device input was accepted".into());
        };

        assert!(error.to_string().contains("is not a regular file"));
        Ok(())
    }

    #[test]
    fn printable_text_replaces_control_and_format_characters() {
        // Bidirectional embeddings, overrides, and isolates.
        let bidi = "a\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}\u{2066}\u{2067}\u{2068}\u{2069}b";
        assert_eq!(printable(bidi), "a?????????b");
        let hidden = "\u{200B}\u{200D}\u{200E}\u{200F}\u{061C}\u{FEFF}\u{00AD}\u{E0041}";
        assert_eq!(printable(hidden), "????????");
        assert_eq!(printable("\u{2028}\u{2029}"), "??");
        assert_eq!(printable("\u{1b}[31mred\u{7}\r\u{9b}"), "?[31mred???");
        let kept = "route /orders/{id}\n\tstatus 503 · 12.5 ms, café 東京 ✓\n";
        assert_eq!(printable(kept), kept.replace('\t', "?"));
    }
}
