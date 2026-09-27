//! Explicit, application-invoked subscriber initialization.
//!
//! Nothing in this crate installs a global subscriber implicitly. These
//! helpers are called by the application (or by `AlloyApp::run`, which is an
//! application entry point) and return [`InitError::AlreadyInitialized`]
//! rather than silently discarding telemetry when a global subscriber exists.

use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// One JSON object per line (production default).
    #[default]
    Json,
    /// Human-readable multi-line output.
    Pretty,
    /// Human-readable single-line output.
    Compact,
}

/// Logging configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LoggingConfig {
    /// Output format.
    pub format: LogFormat,
    /// `EnvFilter` directives, e.g. `info,ferrum_alloy=debug`. When unset,
    /// `RUST_LOG` is used; when that is unset too, `info`.
    pub filter: Option<String>,
    /// Emit ANSI colors (text formats only).
    pub ansi: bool,
}

/// Why initialization did not happen.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// A global subscriber is already installed by the application or a
    /// library. Alloy will not replace it.
    #[error(
        "a global tracing subscriber is already installed; Ferrum Alloy will not replace it. \
         Compose Alloy's layers into your subscriber instead (see docs/getting-started.md)"
    )]
    AlreadyInitialized,
    /// The filter directives are invalid.
    #[error("invalid log filter {filter:?}: {message}")]
    Filter {
        /// The directives.
        filter: String,
        /// Parser message.
        message: String,
    },
    /// OpenTelemetry could not start.
    #[cfg(feature = "otel")]
    #[error(transparent)]
    Otel(#[from] crate::otel::OtelError),
}

/// Returns `true` when a global subscriber is already installed.
pub fn global_subscriber_installed() -> bool {
    tracing::dispatcher::has_been_set()
}

/// Builds the filter: explicit directives, else `RUST_LOG`, else `info`.
/// Invalid directives are an error, never silently replaced.
pub fn build_filter(config: &LoggingConfig) -> Result<EnvFilter, InitError> {
    let directives = match &config.filter {
        Some(filter) => filter.clone(),
        None => std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()),
    };
    EnvFilter::builder()
        .parse(&directives)
        .map_err(|e| InitError::Filter {
            filter: directives.clone(),
            message: e.to_string(),
        })
}

/// A formatting layer for `config`, writing to standard output.
///
/// `json` uses [`crate::json::JsonLayer`], whose lines match
/// tracing-subscriber's JSON formatter with flattened event fields, the
/// current span, and no span list.
pub fn fmt_layer<S>(config: &LoggingConfig) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let base = tracing_subscriber::fmt::layer().with_target(true);
    match config.format {
        LogFormat::Json => Box::new(crate::json::JsonLayer::new()),
        LogFormat::Pretty => Box::new(base.pretty().with_ansi(config.ansi)),
        LogFormat::Compact => Box::new(base.compact().with_ansi(config.ansi)),
    }
}

/// Installs a global subscriber with logging only.
pub fn init_logging(config: &LoggingConfig) -> Result<(), InitError> {
    if global_subscriber_installed() {
        return Err(InitError::AlreadyInitialized);
    }
    let filter = build_filter(config)?;
    tracing_subscriber::registry()
        .with(fmt_layer(config).with_filter(filter))
        .try_init()
        .map_err(|_| InitError::AlreadyInitialized)
}

/// Installs a global subscriber with logging and OpenTelemetry export.
/// Keep the returned pipeline and shut it down on exit.
#[cfg(feature = "otel")]
pub fn init_logging_and_otel(
    config: &LoggingConfig,
    resource: &crate::otel::ServiceResource,
    otlp: &crate::otel::OtlpConfig,
    metrics: std::sync::Arc<crate::metrics::Metrics>,
) -> Result<crate::otel::OtelPipeline, InitError> {
    if global_subscriber_installed() {
        return Err(InitError::AlreadyInitialized);
    }
    let filter = build_filter(config)?;
    let pipeline = crate::otel::OtelPipeline::otlp(resource, otlp, metrics)?;
    let installed = tracing_subscriber::registry()
        .with(fmt_layer(config).with_filter(filter))
        .with(pipeline.layer())
        .try_init();
    match installed {
        Ok(()) => Ok(pipeline),
        Err(_) => {
            let _ = pipeline.shutdown(std::time::Duration::from_secs(1));
            Err(InitError::AlreadyInitialized)
        }
    }
}

use tracing_subscriber::Layer as _;
