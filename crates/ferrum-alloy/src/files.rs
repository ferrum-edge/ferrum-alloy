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
//!
//! [`secret_file_permission_warning`] reports a secret file that others
//! could replace: on Unix, one writable by its group or by everyone, or one
//! owned by a user other than this process's (effective) user and root.
//! World-readable files are not reported, so the default `0644` of a
//! Kubernetes secret volume stays quiet.

use std::io::Read as _;
use std::path::Path;

use zeroize::Zeroizing;

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

/// Why the secret file at `path` (symbolic links followed) could be replaced
/// by someone other than this service, or `None` when it could not: on Unix
/// it is writable by its group or by everyone, or its owner is neither this
/// process's effective user nor root. Readable-by-everyone is not reported.
/// A file that cannot be inspected yields `None`; reading it reports the
/// error. Always `None` on other platforms.
pub fn secret_file_permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::metadata(path).ok()?;
        let mode = metadata.permissions().mode();
        let mut problems = Vec::new();
        if mode & 0o022 != 0 {
            problems.push(format!(
                "is writable by its group or by everyone (mode {:03o})",
                mode & 0o777
            ));
        }
        let owner = metadata.uid();
        if owner != 0
            && let Some(user) = effective_uid()
            && owner != user
        {
            problems.push(format!(
                "is owned by uid {owner}, neither this process's user (uid {user}) nor root"
            ));
        }
        if problems.is_empty() {
            return None;
        }
        Some(format!(
            "secret file {} {}; restrict it so only the service account (or root) can change it",
            path.display(),
            problems.join(" and ")
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// This process's effective user id: the owner of a pipe it creates, which
/// is the creating process's effective user. `None` if that fails.
#[cfg(unix)]
fn effective_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    let (reader, _writer) = std::io::pipe().ok()?;
    let file = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
    file.metadata().ok().map(|metadata| metadata.uid())
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

/// [`read_regular_file_bounded`] for secrets and private keys: the buffer is
/// [`Zeroizing`] before the first byte is read and holds `max + 1` bytes from
/// the start, so it is never reallocated (which would leave a plaintext copy
/// behind) and is zeroized on every path, a refusal or an I/O error included.
pub fn read_sensitive_file_bounded(path: &Path, max: u64) -> Result<Zeroizing<Vec<u8>>, ReadError> {
    check(&std::fs::metadata(path).map_err(ReadError::Io)?, max)?;
    let capacity =
        usize::try_from(max.saturating_add(1)).map_err(|_| ReadError::TooLarge { limit: max })?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
    read_opened_into(path, max, &mut bytes)?;
    Ok(bytes)
}

/// Opens `path` and reads it, checking the type on the opened handle.
fn read_opened(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    let mut bytes = Vec::new();
    read_opened_into(path, max, &mut bytes)?;
    Ok(bytes)
}

/// Opens `path` and reads at most `max` bytes of it into `bytes`, checking
/// the type on the opened handle. Reserves the reported length when `bytes`
/// has less spare capacity.
fn read_opened_into(path: &Path, max: u64, bytes: &mut Vec<u8>) -> Result<(), ReadError> {
    let file = open(path).map_err(ReadError::Io)?;
    let metadata = file.metadata().map_err(ReadError::Io)?;
    check(&metadata, max)?;

    bytes.reserve(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max.saturating_add(1))
        .read_to_end(bytes)
        .map_err(ReadError::Io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
        return Err(ReadError::TooLarge { limit: max });
    }
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn only_secret_files_others_could_replace_are_reported()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("token");
        std::fs::write(&path, b"secret")?;
        // Owned by this process's user; readable by everyone stays quiet.
        for mode in [0o600, 0o400, 0o640, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
            let warning = secret_file_permission_warning(&path);
            assert_eq!(warning, None, "mode {mode:o}");
        }
        for mode in [0o620, 0o602, 0o666, 0o664] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
            let Some(warning) = secret_file_permission_warning(&path) else {
                return Err(format!("mode {mode:o} was not reported").into());
            };
            assert!(
                warning.contains("writable by its group or by everyone"),
                "{warning}"
            );
            assert!(warning.contains(&format!("{:03o}", mode)), "{warning}");
            assert!(warning.contains("token"), "{warning}");
        }
        // A file this process cannot inspect is left to the read to report.
        assert_eq!(
            secret_file_permission_warning(&dir.path().join("missing")),
            None
        );
        // A root-owned, world-readable file is not reported: root may own a
        // secret volume, and readable-by-everyone is not a replacement risk.
        assert_eq!(
            secret_file_permission_warning(Path::new("/etc/passwd")),
            None
        );
        // The pipe probe finds this process's user: the owner of a file it
        // just created.
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(effective_uid(), Some(std::fs::metadata(&path)?.uid()));
        Ok(())
    }

    #[test]
    fn sensitive_reads_are_bounded_and_never_reallocate() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("secret");
        std::fs::write(&path, b"1234")?;
        let Ok(bytes) = read_sensitive_file_bounded(&path, 4) else {
            return Err("input at the limit was refused".into());
        };
        assert_eq!(bytes.as_slice(), b"1234");
        assert!(bytes.capacity() >= 5, "max + 1 reserved up front");
        std::fs::write(&path, b"12345")?;
        assert!(matches!(
            read_sensitive_file_bounded(&path, 4),
            Err(ReadError::TooLarge { limit: 4 })
        ));
        Ok(())
    }

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
