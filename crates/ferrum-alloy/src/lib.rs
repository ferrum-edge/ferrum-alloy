//! Ferrum Alloy: production defaults for ordinary Axum applications.
//!
//! [`AlloyApp`] wraps an application-owned `axum::Router` with typed
//! configuration, Problem Details errors, health endpoints, request limits,
//! graceful shutdown, truthful request telemetry, and optional integrations.
//! Handlers, extractors, state, and Tower layers remain plain Axum.
//!
//! Optional integrations are Cargo features and are off by default:
//! `otel`, `edge`, `tls`, `postgres`, `openapi`, `jwt`, `http-client`,
//! `compression`, `cors` (`full` enables all).

pub mod config;
pub mod error;
pub mod extract;
pub mod health;
pub mod lifecycle;
pub mod limits;
pub mod normalize;
pub mod problem;

mod app;
mod management;

mod server;

#[cfg(any(feature = "compression", feature = "cors"))]
mod http_layers;
#[cfg(feature = "tls")]
pub mod tls;

pub use app::{AlloyApp, AlloyParts, TelemetryGuard, TelemetryInit};
pub use config::AlloyConfig;
pub use error::AlloyError;
pub use lifecycle::Lifecycle;
pub use problem::{Problem, ProblemKind};
pub use server::ServerStats;

/// The telemetry crate, re-exported for application-owned composition.
pub use ferrum_alloy_telemetry as telemetry;

/// The Edge adapter crate (feature `edge`).
#[cfg(feature = "edge")]
pub use ferrum_alloy_edge as edge;

/// Cargo features compiled into this build.
pub fn enabled_features() -> &'static [&'static str] {
    const FEATURES: &[(&str, bool)] = &[
        ("otel", cfg!(feature = "otel")),
        ("edge", cfg!(feature = "edge")),
        ("tls", cfg!(feature = "tls")),
        ("postgres", cfg!(feature = "postgres")),
        ("openapi", cfg!(feature = "openapi")),
        ("jwt", cfg!(feature = "jwt")),
        ("http-client", cfg!(feature = "http-client")),
        ("compression", cfg!(feature = "compression")),
        ("cors", cfg!(feature = "cors")),
    ];
    static ENABLED: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ENABLED.get_or_init(|| {
        FEATURES
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect()
    })
}
