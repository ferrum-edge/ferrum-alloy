//! Startup and serving errors.

use std::net::SocketAddr;

use crate::config::ConfigError;

/// Errors from building or running an Alloy application.
#[derive(Debug, thiserror::Error)]
pub enum AlloyError {
    /// Configuration is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// No router was supplied.
    #[error("no router supplied; call AlloyApp::router")]
    MissingRouter,
    /// Telemetry could not be initialized.
    #[error("telemetry: {0}")]
    Telemetry(String),
    /// A listener could not bind.
    #[error("cannot bind {listener} listener to {addr}: {source}")]
    Bind {
        /// Which listener.
        listener: &'static str,
        /// Address.
        addr: SocketAddr,
        /// Cause.
        source: std::io::Error,
    },
    /// TLS configuration failed.
    #[error("tls: {0}")]
    Tls(String),
    /// Serving failed.
    #[error("serve: {0}")]
    Serve(std::io::Error),
    /// An application route matches a path Alloy serves on the application
    /// listener, ahead of the application's router, so the route would
    /// never be reached.
    #[error(
        "application route {route} matches {path}, which Alloy serves on the application listener ({setting}), so the route would never be reached; change {setting} or the route"
    )]
    ShadowedRoute {
        /// The application route's pattern.
        route: String,
        /// The path Alloy serves.
        path: String,
        /// The setting that places the path on the application listener.
        setting: &'static str,
    },
    /// An integration (database, auth) failed to start.
    #[error("{0}")]
    Integration(String),
    /// An invariant was violated.
    #[error("internal error: {0}")]
    Internal(String),
}
