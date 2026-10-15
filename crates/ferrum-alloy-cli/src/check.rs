//! `ferrum-alloy check`: validates configuration exactly as startup does.
//!
//! Read-only: it reads the file and environment and never contacts a
//! gateway, database, or collector.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use ferrum_alloy::config::{AlloyConfig, ClientAuth, ConfigError, EdgeMode, Overrides, load_from};

use crate::Format;
use crate::error::{CliError, INVALID};

/// Arguments for `check`.
#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    /// Configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Cargo features compiled into the application (comma-separated), as
    /// in Cargo: `full` means every integration, and a feature brings the
    /// ones it enables. Defaults to every feature, so only
    /// feature-independent problems fail.
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
    "openapi-ui",
    "jwt",
    "http-client",
    "compression",
    "cors",
    "diagnostics",
];

/// The umbrella crate's `default` features (`crates/ferrum-alloy/Cargo.toml`).
const DEFAULT_FEATURES: &[&str] = &[];

/// Features another feature turns on (`crates/ferrum-alloy/Cargo.toml`).
/// `full` turns on every one of [`ALL_FEATURES`], and `default` the ones in
/// [`DEFAULT_FEATURES`].
const IMPLIED_FEATURES: &[(&str, &str)] = &[("openapi-ui", "openapi"), ("jwt", "http-client")];

/// The features a `--features` list compiles in, the way Cargo resolves
/// them, in [`ALL_FEATURES`] order.
fn assumed_features(requested: &[String]) -> Result<Vec<&'static str>, CliError> {
    let mut enabled = std::collections::BTreeSet::new();
    for feature in requested {
        if feature == "full" {
            enabled.extend(ALL_FEATURES.iter().copied());
            continue;
        }
        if feature == "default" {
            enabled.extend(DEFAULT_FEATURES.iter().copied());
            continue;
        }
        let Some(known) = ALL_FEATURES.iter().copied().find(|known| known == feature) else {
            return Err(CliError::Invalid(format!("unknown feature {feature:?}")));
        };
        enabled.insert(known);
        for (feature, implied) in IMPLIED_FEATURES {
            if *feature == known {
                enabled.insert(*implied);
            }
        }
    }
    Ok(ALL_FEATURES
        .iter()
        .copied()
        .filter(|feature| enabled.contains(feature))
        .collect())
}

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
        ("openapi-ui", config.openapi.ui, "openapi-ui"),
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
            // `ConfigError` names files, keys, and locations, but never
            // configuration values or source excerpts, which may be secrets.
            return match args.format {
                Format::Json => {
                    let mut value =
                        serde_json::json!({ "valid": false, "errors": [error.to_string()] });
                    if let ConfigError::Syntax { line, column, .. } = &error
                        && let Some(map) = value.as_object_mut()
                    {
                        map.insert(
                            "location".into(),
                            serde_json::json!({ "line": line, "column": column }),
                        );
                    }
                    crate::print(&format!("{value:#}\n"))?;
                    Ok(ExitCode::from(INVALID))
                }
                Format::Human => Err(CliError::Invalid(error.to_string())),
            };
        }
    };
    let features: Vec<&str> = match &args.features {
        Some(list) => assumed_features(list)?,
        None => ALL_FEATURES.to_vec(),
    };
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
                    serde_json::to_value(config.redacted())
                        .map_err(|e| CliError::Io(e.to_string()))?,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn requested(features: &[&str]) -> Vec<String> {
        features.iter().map(|f| (*f).to_owned()).collect()
    }

    #[test]
    fn features_resolve_like_cargo() {
        assert_eq!(
            assumed_features(&requested(&["full"])).unwrap(),
            ALL_FEATURES
        );
        assert_eq!(
            assumed_features(&requested(&["jwt", "openapi-ui"])).unwrap(),
            ["openapi", "openapi-ui", "jwt", "http-client"]
        );
        assert_eq!(assumed_features(&requested(&["otel"])).unwrap(), ["otel"]);
        assert!(
            assumed_features(&requested(&["default"]))
                .unwrap()
                .is_empty()
        );
        assert!(assumed_features(&requested(&["fulll"])).is_err());
    }

    /// The lists here follow the umbrella crate's `[features]`.
    #[test]
    fn features_match_the_umbrella_crate() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ferrum-alloy/Cargo.toml");
        let manifest: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
        let features = manifest["features"].as_table().unwrap();
        let list = |name: &str| -> Vec<&str> {
            features[name]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect()
        };
        let mut full = list("full");
        full.sort_unstable();
        let mut all = ALL_FEATURES.to_vec();
        all.sort_unstable();
        assert_eq!(full, all, "`full` and ALL_FEATURES differ");
        assert_eq!(list("default"), DEFAULT_FEATURES, "`default` differs");
        for (feature, implied) in IMPLIED_FEATURES {
            assert!(list(feature).contains(implied), "{feature} -> {implied}");
        }
        // Every feature that turns on another integration is listed.
        for feature in ALL_FEATURES {
            for enabled in list(feature) {
                if ALL_FEATURES.contains(&enabled) {
                    assert!(
                        IMPLIED_FEATURES.contains(&(*feature, enabled)),
                        "{feature} enables {enabled}"
                    );
                }
            }
        }
    }
}
