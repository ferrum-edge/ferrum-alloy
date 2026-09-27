//! Startup and serving errors.

use std::fmt;
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
    /// Application routes match paths Alloy serves on the application
    /// listener, ahead of the application's router, so requests for those
    /// paths would never reach them. Every conflict is listed.
    #[error(
        "Alloy serves paths on the application listener ahead of the application's router: {}; change the settings named or the routes",
        conflict_list(.conflicts)
    )]
    ShadowedRoute {
        /// Every conflict, in the order Alloy's paths are served.
        conflicts: Vec<RouteConflict>,
    },
    /// An integration (database, auth) failed to start.
    #[error("{0}")]
    Integration(String),
    /// An invariant was violated.
    #[error("internal error: {0}")]
    Internal(String),
}

/// An application route that a path Alloy serves on the application listener
/// would shadow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteConflict {
    /// The application route's pattern, or `None` for a `nest_service` whose
    /// prefix contains `path` (axum reports no pattern for it).
    pub route: Option<String>,
    /// The path Alloy serves.
    pub path: String,
    /// The setting that places the path on the application listener.
    pub setting: &'static str,
}

impl fmt::Display for RouteConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            route,
            path,
            setting,
        } = self;
        match route {
            Some(route) => write!(
                f,
                "requests for {path} would never reach application route {route} ({setting})"
            ),
            None => write!(
                f,
                "requests for {path} would never reach a nest_service whose prefix contains {path} ({setting})"
            ),
        }
    }
}

fn conflict_list(conflicts: &[RouteConflict]) -> String {
    let conflicts: Vec<String> = conflicts.iter().map(ToString::to_string).collect();
    conflicts.join("; ")
}
