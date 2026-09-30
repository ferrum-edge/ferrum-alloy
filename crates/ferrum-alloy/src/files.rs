//! Bounded reads of operator-supplied files.
//!
//! A path may name a device, a FIFO, or a file that reports a length of `0`
//! but holds more (`/proc`), and a file may grow after it is inspected. The
//! size a path reports is therefore only an early refusal; the read itself
//! stops one byte past the limit.
//!
//! The configuration file, `FERRUM_ALLOY_*_FILE` secrets, and TLS
//! certificates, keys, and CRLs are read this way, as are the files the
//! `ferrum-alloy` command reads.

use std::io::Read as _;
use std::path::Path;

/// Why [`read_regular_file_bounded`] refused a path. The message never
/// quotes the file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// Inspecting, opening, or reading the file failed.
    #[error("{0}")]
    Io(std::io::Error),
    /// The path names a device, FIFO, socket, or directory.
    #[error("not a regular file")]
    NotRegular,
    /// The file holds more than the limit.
    #[error("larger than {limit} bytes")]
    #[non_exhaustive]
    TooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
}

/// Reads the regular file at `path` (symbolic links followed), holding at
/// most `max` bytes.
///
/// The type is checked on the path before opening, so a FIFO or device is
/// never opened, and again on the opened handle, so a path swapped for one
/// in between is still refused before any read. On Unix the open does not
/// wait for a writer, so a FIFO swapped in between never blocks it.
pub fn read_regular_file_bounded(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    check(&std::fs::metadata(path).map_err(ReadError::Io)?, max)?;
    read_opened(path, max)
}

/// Opens `path` and reads it, checking the type on the opened handle.
fn read_opened(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    let file = open(path).map_err(ReadError::Io)?;
    let metadata = file.metadata().map_err(ReadError::Io)?;
    check(&metadata, max)?;

    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
        return Err(ReadError::TooLarge { limit: max });
    }
    Ok(bytes)
}

/// Opens `path` for reading. `O_NONBLOCK` makes opening a FIFO return at
/// once instead of waiting for a writer; it has no effect on reads of
/// regular files, the only files read after the check on the handle.
#[cfg(unix)]
fn open(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn check(metadata: &std::fs::Metadata, max: u64) -> Result<(), ReadError> {
    if !metadata.is_file() {
        return Err(ReadError::NotRegular);
    }
    if metadata.len() > max {
        return Err(ReadError::TooLarge { limit: max });
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

        let Err(error) = read_regular_file_bounded(&path, 4) else {
            return Err("oversized input was accepted".into());
        };

        assert!(
            matches!(error, ReadError::TooLarge { limit: 4 }),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "larger than 4 bytes");
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

        assert_eq!(error.to_string(), "not a regular file");
        Ok(())
    }

    /// A FIFO swapped in after the check on the path reaches the open, which
    /// returns at once; the check on the handle then refuses it.
    #[cfg(unix)]
    #[test]
    fn refuses_a_fifo_at_the_open_without_blocking() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("key.pem");
        let status = std::process::Command::new("mkfifo").arg(&path).status()?;
        assert!(status.success(), "mkfifo failed: {status}");

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read_opened(&path, 4));
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(10))?;

        let Err(ReadError::NotRegular) = result else {
            return Err(format!("a FIFO was not refused: {result:?}").into());
        };
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

        assert_eq!(error.to_string(), "larger than 16 bytes");
        Ok(())
    }
}
