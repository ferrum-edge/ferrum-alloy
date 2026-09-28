//! Bounded reads of operator-supplied files.
//!
//! A path may name a device, a FIFO, or a file that reports a length of `0`
//! but holds more (`/proc`), and a file may grow after it is inspected. The
//! size a path reports is therefore only an early refusal; the read itself
//! stops one byte past the limit.

use std::io::Read as _;
use std::path::Path;

use crate::error::CliError;

/// Why [`read_regular_file_bounded`] refused a path.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// Inspecting, opening, or reading the file failed.
    Io(std::io::Error),
    /// The path names a device, FIFO, socket, or directory.
    NotRegular,
    /// The file holds more than the limit.
    TooLarge,
}

impl ReadError {
    /// The usual invalid-input error for `path` read with limit `max`.
    pub(crate) fn invalid(self, path: &Path, max: u64) -> CliError {
        CliError::Invalid(match self {
            Self::Io(e) => format!("{}: {e}", path.display()),
            Self::NotRegular => format!("{} is not a regular file", path.display()),
            Self::TooLarge => format!("{} is larger than {max} bytes", path.display()),
        })
    }
}

/// Reads the regular file at `path` (symbolic links followed), holding at
/// most `max` bytes.
///
/// The type is checked on the path before opening, so a FIFO or device is
/// never opened, and again on the opened handle, so a path swapped for one
/// in between is still refused before any read. Opening a FIFO swapped in
/// between the two checks blocks until a writer appears; the read that
/// follows is still refused.
pub(crate) fn read_regular_file_bounded(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    check(&std::fs::metadata(path).map_err(ReadError::Io)?, max)?;
    let file = std::fs::File::open(path).map_err(ReadError::Io)?;
    let metadata = file.metadata().map_err(ReadError::Io)?;
    check(&metadata, max)?;

    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
        return Err(ReadError::TooLarge);
    }
    Ok(bytes)
}

fn check(metadata: &std::fs::Metadata, max: u64) -> Result<(), ReadError> {
    if !metadata.is_file() {
        return Err(ReadError::NotRegular);
    }
    if metadata.len() > max {
        return Err(ReadError::TooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_file_within_the_limit() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("report.json");
        std::fs::write(&path, b"1234")?;

        let Ok(bytes) = read_regular_file_bounded(&path, 4) else {
            return Err("input at the limit was refused".into());
        };

        assert_eq!(bytes, b"1234");
        Ok(())
    }

    #[test]
    fn refuses_a_file_reporting_more_than_the_limit() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("report.json");
        std::fs::write(&path, b"12345")?;

        let Err(ReadError::TooLarge) = read_regular_file_bounded(&path, 4) else {
            return Err("oversized input was accepted".into());
        };
        Ok(())
    }

    #[test]
    fn refuses_directories() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;

        let Err(ReadError::NotRegular) = read_regular_file_bounded(dir.path(), 4) else {
            return Err("a directory was accepted".into());
        };
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_devices() -> Result<(), Box<dyn std::error::Error>> {
        let Err(error) = read_regular_file_bounded(Path::new("/dev/zero"), 4) else {
            return Err("device input was accepted".into());
        };

        let message = error.invalid(Path::new("/dev/zero"), 4).to_string();
        assert!(message.contains("is not a regular file"), "{message}");
        Ok(())
    }

    /// `/proc/self/status` is a regular file that reports a length of `0`
    /// but holds far more than 16 bytes, so only the bounded read catches it.
    #[cfg(target_os = "linux")]
    #[test]
    fn bounds_the_read_of_a_file_reporting_no_length() -> Result<(), Box<dyn std::error::Error>> {
        let path = Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(path)?.len(), 0);

        let Err(error) = read_regular_file_bounded(path, 16) else {
            return Err("a file larger than the limit was accepted".into());
        };

        let message = error.invalid(path, 16).to_string();
        assert!(message.contains("is larger than 16 bytes"), "{message}");
        Ok(())
    }
}
