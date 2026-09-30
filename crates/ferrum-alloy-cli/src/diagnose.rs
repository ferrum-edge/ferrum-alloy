//! `ferrum-alloy diagnose`: explains supplied evidence.
//!
//! Deterministic rules only, and no external AI service. The only network
//! access is `--url`, which fetches one live report from a running service
//! (see [`crate::live`]). Every input, live reports included, is read with
//! `parse_offline` and never treated as authenticated.
//!
//! An OTLP export is held to the same report limits, with or without
//! `--write-report`: rules only ever run on reports the parser accepts, so
//! a trace the parser would refuse fails instead of being analyzed.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::hash::{BuildHasher, Hasher};
use std::io::{ErrorKind, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use ferrum_alloy_diagnostics::model::{DiagnosticReport, Producer, ProducerKind};
use ferrum_alloy_diagnostics::otlp::{self, ImportLimits};
use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};

use crate::Format;
use crate::error::CliError;
use crate::input::{invalid, read_regular_file_bounded};

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
    /// Write the assembled report (with findings) to this file, pretty-printed,
    /// or compact when only compact JSON fits the report size limit. Nothing
    /// is written when `diagnose --input` would reject it.
    #[arg(long)]
    write_report: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

fn read(path: &Path, max: usize) -> Result<Vec<u8>, CliError> {
    let max = u64::try_from(max).unwrap_or(u64::MAX);
    read_regular_file_bounded(path, max).map_err(|e| invalid(path, e))
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

/// The bytes `--write-report` writes: pretty-printed JSON, or compact JSON
/// when the pretty form would not fit within `limits.max_bytes`.
fn report_bytes(report: &DiagnosticReport, limits: &Limits) -> Result<Vec<u8>, CliError> {
    let encode = |e: serde_json::Error| CliError::Io(e.to_string());
    let mut json = serde_json::to_vec_pretty(report).map_err(encode)?;
    if json.len() >= limits.max_bytes {
        json = serde_json::to_vec(report).map_err(encode)?;
    }
    json.push(b'\n');
    Ok(json)
}

/// How many temporary names `write_atomically` tries before giving up.
const TEMP_ATTEMPTS: u32 = 5;

/// A temporary-file suffix with the process id and a random part, so a
/// stale file from an earlier run, even one with the same process id, or
/// a name created in advance does not block the write.
fn temp_suffix() -> String {
    let random = RandomState::new().build_hasher().finish();
    format!(".{}.{random:016x}.tmp", std::process::id())
}

/// Writes `bytes` to `path` through a new temporary file in the same
/// directory and a rename, so `path` never holds a partial report. The
/// temporary file is removed when a later step fails. A symbolic link at
/// `path` is replaced, not written through.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let failed = |e: std::io::Error| CliError::Io(format!("write {}: {e}", path.display()));
    let Some(name) = path.file_name() else {
        let message = format!("{} names no file", path.display());
        return Err(CliError::Invalid(message));
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut attempts = 1;
    let (temp, mut file) = loop {
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(temp_suffix());
        let temp = path.with_file_name(temp_name);
        match options.open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < TEMP_ATTEMPTS => {
                attempts += 1;
            }
            Err(e) => return Err(failed(e)),
        }
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    // Closed before the rename, which Windows requires.
    drop(file);
    if let Err(e) = written.and_then(|()| std::fs::rename(&temp, path)) {
        let _ = std::fs::remove_file(&temp);
        return Err(failed(e));
    }
    Ok(())
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
        // Strict even without --write-report; see the module documentation.
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
        let json = report_bytes(&report, &limits)?;
        // Findings and pretty printing add bytes after the report was
        // checked: never write a file that `diagnose --input` would reject.
        if let Err(e) = parse_offline(&json, &limits) {
            return Err(CliError::Invalid(format!(
                "not writing {}: `diagnose --input` would reject the report: {e}",
                path.display()
            )));
        }
        write_atomically(path, &json)?;
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
#[allow(clippy::unwrap_used)]
mod tests {
    use ferrum_alloy_diagnostics::model::{Collection, CollectionMethod, Verification};

    use super::*;

    #[test]
    fn temporary_names_differ_between_attempts() {
        let (first, second) = (temp_suffix(), temp_suffix());
        assert_ne!(first, second);
        assert!(first.ends_with(".tmp"), "{first}");
    }

    #[test]
    fn written_reports_fall_back_to_compact_json_to_fit_the_limit() {
        let report = DiagnosticReport::new(Collection {
            collector: Producer {
                kind: ProducerKind::Collector,
                name: "ferrum-alloy-cli".into(),
                version: None,
                instance: None,
            },
            method: CollectionMethod::OtlpFileImport,
            verification: Verification::Unverified,
            notes: vec!["compact when pretty does not fit".into()],
        });
        let pretty = serde_json::to_vec_pretty(&report).unwrap();
        let compact = serde_json::to_vec(&report).unwrap();
        assert!(compact.len() < pretty.len());
        // The pretty form plus its newline fits exactly, then by one byte less.
        for (max_bytes, form) in [(pretty.len() + 1, &pretty), (pretty.len(), &compact)] {
            let limits = Limits {
                max_bytes,
                ..Limits::default()
            };
            let mut expected = form.clone();
            expected.push(b'\n');
            assert_eq!(report_bytes(&report, &limits).unwrap(), expected);
        }
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
