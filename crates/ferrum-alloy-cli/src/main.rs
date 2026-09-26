//! `ferrum-alloy`: project starters, configuration checks, artifact export,
//! and offline diagnosis.
//!
//! Exit codes: `0` success, `1` unexpected failure, `2` usage error,
//! `3` invalid input (configuration, manifest, report), `4` a `--check`
//! found drift.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod check;
mod diagnose;
mod edge;
mod error;
mod new;
mod openapi;

use error::CliError;

#[derive(Debug, Parser)]
#[command(name = "ferrum-alloy", version, about = "Ferrum Alloy tooling", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// Human-readable text.
    #[default]
    Human,
    /// Machine-readable JSON.
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new Alloy project in an empty directory.
    New(new::NewArgs),
    /// Validate an Alloy configuration file (and the current environment).
    Check(check::CheckArgs),
    /// OpenAPI artifacts.
    Openapi {
        #[command(subcommand)]
        command: openapi::OpenapiCommand,
    },
    /// Ferrum Edge artifacts.
    Edge {
        #[command(subcommand)]
        command: edge::EdgeCommand,
    },
    /// Explain a diagnostic report or an OTLP/JSON trace export offline.
    Diagnose(diagnose::DiagnoseArgs),
    /// Print tool and contract versions.
    Version {
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
}

pub(crate) fn print(text: &str) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| CliError::Io(format!("write stdout: {e}")))
}

pub(crate) fn eprint(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

fn version(format: Format) -> Result<(), CliError> {
    let info = serde_json::json!({
        "ferrum_alloy_cli": env!("CARGO_PKG_VERSION"),
        "diagnostic_report_schema": format!(
            "{} {}.{}",
            ferrum_alloy_diagnostics::model::SCHEMA_NAME,
            ferrum_alloy_diagnostics::model::SCHEMA_MAJOR,
            ferrum_alloy_diagnostics::model::SCHEMA_MINOR
        ),
        "service_manifest_schema": format!("{} {}.x (PROPOSED)", ferrum_alloy_edge::manifest::MANIFEST_SCHEMA, ferrum_alloy_edge::manifest::MANIFEST_MAJOR),
        "edge_contract": {
            "release": ferrum_alloy_edge::contract::EDGE_RELEASE,
            "source_commit": ferrum_alloy_edge::contract::EDGE_SOURCE_COMMIT,
        },
    });
    match format {
        Format::Json => print(&format!("{info:#}\n")),
        Format::Human => print(&format!(
            "ferrum-alloy {}\ndiagnostic report schema: {}\nservice manifest schema: {}\nEdge contract: {} ({})\n",
            env!("CARGO_PKG_VERSION"),
            info["diagnostic_report_schema"]
                .as_str()
                .unwrap_or_default(),
            info["service_manifest_schema"].as_str().unwrap_or_default(),
            ferrum_alloy_edge::contract::EDGE_RELEASE,
            ferrum_alloy_edge::contract::EDGE_SOURCE_COMMIT,
        )),
    }
}

fn run(cli: Cli) -> Result<ExitCode, CliError> {
    match cli.command {
        Command::New(args) => new::run(args).map(|()| ExitCode::SUCCESS),
        Command::Check(args) => check::run(args),
        Command::Openapi { command } => openapi::run(command),
        Command::Edge { command } => edge::run(command).map(|()| ExitCode::SUCCESS),
        Command::Diagnose(args) => diagnose::run(args),
        Command::Version { format } => version(format).map(|()| ExitCode::SUCCESS),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprint(&format!("error: {error}\n"));
            error.exit_code()
        }
    }
}

/// Resolves `path` relative to the current directory.
pub(crate) fn absolute(path: &std::path::Path) -> Result<PathBuf, CliError> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .map_err(|e| CliError::Io(format!("current directory: {e}")))
}
