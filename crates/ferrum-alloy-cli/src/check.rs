//! `ferrum-alloy check`: validates configuration exactly as startup does.
//!
//! Read-only: it reads the file and environment and never contacts a
//! gateway, database, or collector.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use ferrum_alloy::config::{AlloyConfig, ClientAuth, EdgeMode, Overrides, load_from};

use crate::Format;
use crate::error::{CliError, INVALID};

/// Arguments for `check`.
#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    /// Configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Cargo features compiled into the application (comma-separated).
    /// Defaults to every feature, so only feature-independent problems fail.
    #[arg(long, value_delimiter = ',')]
    features: Option<Vec<String>>,
    /// Ignore `FERRUM_ALLOY_*` variables in the current environment.
    #[arg(long)]
    no_env: bool,
    /// Print the merged configuration (secrets redacted).
    #[arg(long)]
    show_effective: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

const ALL_FEATURES: &[&str] = &[
    "otel",
    "edge",
    "tls",
    "postgres",
    "openapi",
    "jwt",
    "http-client",
    "compression",
    "cors",
];

fn capabilities(config: &AlloyConfig) -> Vec<(&'static str, bool, &'static str)> {
    vec![
        ("tls", config.server.tls.is_some(), "tls"),
        (
            "mtls-client-verification",
            config
                .server
                .tls
                .as_ref()
                .is_some_and(|t| t.client_auth != ClientAuth::None),
            "tls",
        ),
        ("otlp-export", config.otlp.enabled, "otel"),
        (
            "edge-integration",
            config.edge.mode != EdgeMode::Standalone || config.edge.accept_consumer_identity,
            "edge",
        ),
        ("cors", config.cors.enabled, "cors"),
        ("compression", config.compression.enabled, "compression"),
        ("postgres", config.database.url.is_some(), "postgres"),
        ("jwt", config.auth.jwt.is_some(), "jwt"),
        ("management-listener", config.management.enabled, "-"),
        ("app-health-endpoints", config.health.app_endpoints, "-"),
        (
            "server-timing",
            config.telemetry.server_timing != ferrum_alloy::telemetry::ServerTimingPolicy::Disabled,
            "-",
        ),
        (
            "admission-limit",
            config.server.max_in_flight_requests > 0,
            "-",
        ),
    ]
}

/// Runs `check`.
pub(crate) fn run(args: CheckArgs) -> Result<ExitCode, CliError> {
    let env: Vec<_> = if args.no_env {
        Vec::new()
    } else {
        std::env::vars_os().collect()
    };
    let (config, sources) = match load_from(args.config.as_deref(), env, &Overrides::default()) {
        Ok(loaded) => loaded,
        Err(error) => {
            return match args.format {
                Format::Json => {
                    crate::print(&format!(
                        "{:#}\n",
                        serde_json::json!({ "valid": false, "errors": [error.to_string()] })
                    ))?;
                    Ok(ExitCode::from(INVALID))
                }
                Format::Human => Err(CliError::Invalid(error.to_string())),
            };
        }
    };
    let features: Vec<&str> = match &args.features {
        Some(list) => list.iter().map(String::as_str).collect(),
        None => ALL_FEATURES.to_vec(),
    };
    for feature in &features {
        if !ALL_FEATURES.contains(feature) {
            return Err(CliError::Invalid(format!("unknown feature {feature:?}")));
        }
    }
    let issues = config.check(&features);
    let errors: Vec<&str> = issues
        .iter()
        .filter(|i| i.error)
        .map(|i| i.message.as_str())
        .collect();
    let warnings: Vec<&str> = issues
        .iter()
        .filter(|i| !i.error)
        .map(|i| i.message.as_str())
        .collect();
    let capabilities = capabilities(&config);

    match args.format {
        Format::Json => {
            let mut value = serde_json::json!({
                "valid": errors.is_empty(),
                "errors": errors,
                "warnings": warnings,
                "sources": {
                    "file": sources.file.as_ref().map(|p| p.display().to_string()),
                    "env": sources.env,
                },
                "capabilities": capabilities.iter().map(|(name, enabled, feature)| serde_json::json!({
                    "name": name, "enabled": enabled, "feature": feature,
                })).collect::<Vec<_>>(),
            });
            if args.show_effective
                && let Some(map) = value.as_object_mut()
            {
                map.insert(
                    "effective".into(),
                    serde_json::to_value(&config).map_err(|e| CliError::Io(e.to_string()))?,
                );
            }
            crate::print(&format!("{value:#}\n"))?;
        }
        Format::Human => {
            let mut out = String::new();
            out.push_str(&format!(
                "configuration: {}\n",
                sources
                    .file
                    .as_ref()
                    .map_or("(no file)".to_owned(), |p| p.display().to_string())
            ));
            if !sources.env.is_empty() {
                out.push_str(&format!("environment: {}\n", sources.env.join(", ")));
            }
            out.push_str(&format!("features assumed: {}\n", features.join(", ")));
            out.push_str("\ncapabilities:\n");
            for (name, enabled, feature) in &capabilities {
                let requires = if *feature == "-" {
                    String::new()
                } else {
                    format!(" (feature `{feature}`)")
                };
                out.push_str(&format!(
                    "  {:<26} {}{requires}\n",
                    name,
                    if *enabled { "enabled" } else { "disabled" }
                ));
            }
            out.push_str("  live probes                not run (check is offline and read-only)\n");
            for warning in &warnings {
                out.push_str(&format!("\nwarning: {warning}"));
            }
            for error in &errors {
                out.push_str(&format!("\nerror: {error}"));
            }
            out.push_str(if errors.is_empty() {
                "\n\nconfiguration is valid\n"
            } else {
                "\n\nconfiguration is invalid\n"
            });
            if args.show_effective {
                out.push_str("\n# effective configuration (secrets redacted)\n");
                out.push_str(&config.redacted_toml());
            }
            crate::print(&out)?;
        }
    }
    Ok(if errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(INVALID)
    })
}
