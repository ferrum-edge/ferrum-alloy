//! Bounded reads of operator-supplied files, with the command's messages.
//!
//! The reader is [`ferrum_alloy::files::read_regular_file_bounded`], which
//! the service's configuration and TLS files share: regular files only
//! (symbolic links followed), and the read stops one byte past the limit.

use std::path::Path;

pub(crate) use ferrum_alloy::files::{ReadError, read_regular_file_bounded};

use crate::error::CliError;

/// The usual invalid-input error for `path` refused with `error`.
pub(crate) fn invalid(path: &Path, error: ReadError) -> CliError {
    CliError::Invalid(describe(path, error))
}

/// Why `path` was refused, naming the path and never quoting the file.
pub(crate) fn describe(path: &Path, error: ReadError) -> String {
    let path = path.display();
    match error {
        ReadError::Io(e) => format!("{path}: {e}"),
        ReadError::NotRegular => format!("{path} is not a regular file"),
        ReadError::TooLarge { limit } => format!("{path} is larger than {limit} bytes"),
        other => format!("{path}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_path_and_the_limit() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("report.json");
        std::fs::write(&path, b"12345")?;

        let Err(error) = read_regular_file_bounded(&path, 4) else {
            return Err("oversized input was accepted".into());
        };

        let message = invalid(&path, error).to_string();
        assert!(message.starts_with(&path.display().to_string()), "{message}");
        assert!(message.ends_with("is larger than 4 bytes"), "{message}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_devices() -> Result<(), Box<dyn std::error::Error>> {
        let path = Path::new("/dev/zero");
        let Err(error) = read_regular_file_bounded(path, 4) else {
            return Err("device input was accepted".into());
        };

        let message = invalid(path, error).to_string();
        assert_eq!(message, "/dev/zero is not a regular file");
        Ok(())
    }
}
