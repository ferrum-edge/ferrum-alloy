//! CLI errors and exit codes.

use std::process::ExitCode;

/// A CLI failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// Filesystem or process I/O failed.
    #[error("{0}")]
    Io(String),
    /// Input is invalid (configuration, manifest, report, name, path).
    #[error("{0}")]
    Invalid(String),
    /// A `--check` found drift.
    #[error("{0}")]
    Drift(String),
}

impl CliError {
    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Io(_) => ExitCode::from(1),
            Self::Invalid(_) => ExitCode::from(3),
            Self::Drift(_) => ExitCode::from(4),
        }
    }
}

/// Exit code for invalid input.
pub(crate) const INVALID: u8 = 3;
