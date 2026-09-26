//! `ferrum-alloy diagnose`: explains supplied evidence offline.
//!
//! Deterministic rules only: no network access, no external AI service.
//! Offline input is never treated as authenticated.

use std::path::PathBuf;
use std::process::ExitCode;

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
#[command(group = clap::ArgGroup::new("source").required(true).args(["input", "otlp"]))]
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
    /// Write the assembled report (with findings) to this file.
    #[arg(long)]
    write_report: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

fn read(path: &PathBuf, max: usize) -> Result<Vec<u8>, CliError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| CliError::Invalid(format!("{}: {e}", path.display())))?;
    if metadata.len() > max as u64 {
        return Err(CliError::Invalid(format!(
            "{} is larger than {max} bytes",
            path.display()
        )));
    }
    std::fs::read(path).map_err(|e| CliError::Invalid(format!("{}: {e}", path.display())))
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
    } else {
        return Err(CliError::Invalid("pass --input or --otlp".into()));
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
        Format::Human => crate::print(&render_text(&report, &findings, &warnings))?,
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
