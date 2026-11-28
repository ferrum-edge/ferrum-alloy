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
//! [`read_secret_file_bounded`] (from the file it read) and
//! [`secret_file_permission_warning`] (from a path, for offline checks)
//! report a secret file that others could replace: on Unix, one writable by its group or by everyone, or one
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
///
/// This inspects the path, for offline checks such as `ferrum-alloy check`.
/// Code that reads the secret uses [`read_secret_file_bounded`], whose
/// warning describes the file it actually read.
pub fn secret_file_permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        let metadata = std::fs::metadata(path).ok()?;
        permission_warning(path, &metadata)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// A secret read by [`read_secret_file_bounded`].
#[derive(Debug)]
#[non_exhaustive]
pub struct SecretFile {
    /// The file's bytes, zeroized when dropped.
    pub bytes: Zeroizing<Vec<u8>>,
    /// [`secret_file_permission_warning`] for the file that was read, from
    /// the metadata of the opened handle, so a file swapped in after the
    /// read cannot change it.
    pub permission_warning: Option<String>,
    /// The same permission warning with the file path omitted, for callers
    /// that must drop the path when redacting a reflected secret would still
    /// recreate it.
    pub(crate) permission_warning_without_path: Option<String>,
}

/// [`read_sensitive_file_bounded`], with the permission warning of the file
/// it read, taken from the opened handle.
///
/// # Errors
///
/// As [`read_sensitive_file_bounded`].
pub fn read_secret_file_bounded(path: &Path, max: u64) -> Result<SecretFile, ReadError> {
    check(&std::fs::metadata(path).map_err(ReadError::Io)?, max)?;
    let mut bytes = sensitive_buffer(max)?;
    let metadata = read_opened_into(path, max, &mut bytes)?;
    let (permission_warning, permission_warning_without_path) =
        permission_warnings(path, &metadata);
    Ok(SecretFile {
        bytes,
        permission_warning,
        permission_warning_without_path,
    })
}

/// The warning for the file at `path` whose metadata is `metadata`.
#[cfg(unix)]
fn permission_warning(path: &Path, metadata: &std::fs::Metadata) -> Option<String> {
    permission_warnings(path, metadata).0
}

/// The warning with the formatted path and the same diagnostic with the path
/// omitted.
fn permission_warnings(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> (Option<String>, Option<String>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let problems = permission_problems(
            metadata.permissions().mode(),
            metadata.uid(),
            effective_uid(),
        );
        if problems.is_empty() {
            return (None, None);
        }
        let reasons = problems.join(" and ");
        // Debug formatting quotes the path and escapes control and format
        // characters, so the warning stays one line in text output.
        (
            Some(format!(
                "secret file {:?} {reasons}; restrict it so only the service account (or root) can change it",
                path.to_string_lossy(),
            )),
            Some(format!(
                "secret file (path redacted) {reasons}; restrict it so only the service account (or root) can change it",
            )),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (path, metadata);
        (None, None)
    }
}

/// What lets someone other than `user` (the effective user, when known) or
/// root replace a file with `mode` owned by `owner`.
#[cfg(unix)]
fn permission_problems(mode: u32, owner: u32, user: Option<u32>) -> Vec<String> {
    let mut problems = Vec::new();
    if mode & 0o022 != 0 {
        problems.push(format!(
            "is writable by its group or by everyone (mode {:03o})",
            mode & 0o777
        ));
    }
    if owner != 0
        && let Some(user) = user
        && owner != user
    {
        problems.push(format!(
            "is owned by uid {owner}, neither this process's user (uid {user}) nor root"
        ));
    }
    problems
}

/// This process's effective user id, or `None` when it cannot be read.
///
/// On Linux it is the second `Uid:` field of `/proc/thread-self/status`
/// (falling back to `/proc/self/status` on older kernels): the owner of a new
/// file or pipe there is the filesystem user, which can differ, and reading as
/// raw bytes avoids failing on a non-UTF-8 task name. When procfs is
/// unavailable, `None` disables the ownership half while keeping the
/// write-mode check. Elsewhere on Unix, where there is no separate filesystem
/// user, it is the owner of a pipe this process creates.
#[cfg(unix)]
fn effective_uid() -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let status = std::fs::read("/proc/thread-self/status")
            .or_else(|_| std::fs::read("/proc/self/status"))
            .ok()?;
        effective_uid_from_status_bytes(&status)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        use std::os::unix::fs::MetadataExt as _;
        let (reader, _writer) = std::io::pipe().ok()?;
        let file = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
        file.metadata().ok().map(|metadata| metadata.uid())
    }
}

/// The effective uid in a Linux `/proc/<pid>/status` byte stream: the second of
/// the real, effective, saved, and filesystem uids on its `Uid:` line.
#[cfg(unix)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn effective_uid_from_status_bytes(status: &[u8]) -> Option<u32> {
    let line = status
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))?;
    let line = std::str::from_utf8(line).ok()?;
    line.split_ascii_whitespace().nth(1)?.parse().ok()
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
    let mut bytes = sensitive_buffer(max)?;
    read_opened_into(path, max, &mut bytes)?;
    Ok(bytes)
}

/// A zeroizing buffer of `max + 1` bytes, so reading at most that many never
/// reallocates it.
fn sensitive_buffer(max: u64) -> Result<Zeroizing<Vec<u8>>, ReadError> {
    let capacity =
        usize::try_from(max.saturating_add(1)).map_err(|_| ReadError::TooLarge { limit: max })?;
    Ok(Zeroizing::new(Vec::with_capacity(capacity)))
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
fn read_opened_into(
    path: &Path,
    max: u64,
    bytes: &mut Vec<u8>,
) -> Result<std::fs::Metadata, ReadError> {
    let file = open(path).map_err(ReadError::Io)?;
    read_from_open_file(file, max, bytes)
}

/// Checks `file`'s metadata and reads at most `max` bytes from that same open
/// handle into `bytes`.
fn read_from_open_file(
    file: std::fs::File,
    max: u64,
    bytes: &mut Vec<u8>,
) -> Result<std::fs::Metadata, ReadError> {
    let metadata = file.metadata().map_err(ReadError::Io)?;
    check(&metadata, max)?;

    bytes.reserve(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max.saturating_add(1))
        .read_to_end(bytes)
        .map_err(ReadError::Io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
        return Err(ReadError::TooLarge { limit: max });
    }
    Ok(metadata)
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
        // The read reports the file it read, from the opened handle.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        let read = read_secret_file_bounded(&path, 64)?;
        assert_eq!(read.bytes.as_slice(), b"secret");
        let warning = read.permission_warning.unwrap_or_default();
        assert!(warning.contains("(mode 666)"), "{warning}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        assert!(
            read_secret_file_bounded(&path, 64)?
                .permission_warning
                .is_none()
        );
        // Swapping the pathname after the handle is opened does not change
        // either the bytes read or the warning derived from that handle.
        let replaced = dir.path().join("swapped-secret");
        std::fs::write(&replaced, b"permissive-bytes")?;
        std::fs::set_permissions(&replaced, std::fs::Permissions::from_mode(0o666))?;
        let opened = open(&replaced)?;
        let replacement = dir.path().join("replacement-secret");
        std::fs::write(&replacement, b"restricted-bytes")?;
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&replacement, &replaced)?;
        let mut buf = sensitive_buffer(64)?;
        let metadata = read_from_open_file(opened, 64, &mut buf)?;
        assert_eq!(buf.as_slice(), b"permissive-bytes");
        let handle_warning = permission_warning(&replaced, &metadata).unwrap_or_default();
        assert!(handle_warning.contains("(mode 666)"), "{handle_warning}");
        assert_eq!(secret_file_permission_warning(&replaced), None);
        Ok(())
    }

    /// The ownership rule: a file owned by another user is reported even
    /// when only its owner can write it; root and this process's user are
    /// not; an unknown effective user disables the ownership half.
    #[cfg(unix)]
    #[test]
    fn files_owned_by_a_third_user_are_reported() {
        let problems = permission_problems(0o644, 4242, Some(1000));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("owned by uid 4242"), "{problems:?}");
        assert!(problems[0].contains("(uid 1000)"), "{problems:?}");
        assert!(permission_problems(0o644, 0, Some(1000)).is_empty(), "root");
        assert!(permission_problems(0o600, 1000, Some(1000)).is_empty());
        assert!(permission_problems(0o644, 4242, None).is_empty());
        assert_eq!(permission_problems(0o666, 4242, Some(1000)).len(), 2);
    }

    /// The effective uid is the second `Uid:` field, not the filesystem uid
    /// (the fourth) that owns new files and pipes on Linux, and non-UTF-8
    /// task names on other lines do not prevent reading `Uid:`.
    #[cfg(unix)]
    #[test]
    fn the_effective_uid_is_read_from_the_status_uid_line() -> Result<(), Box<dyn std::error::Error>>
    {
        let status = b"Name:\tservice\nUmask:\t0022\nUid:\t1000\t1001\t1001\t2000\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(effective_uid_from_status_bytes(status), Some(1001));
        let non_utf8_name = b"Name:\t\xff\xfe\nUid:\t1000\t1002\t1002\t2000\n";
        assert_eq!(effective_uid_from_status_bytes(non_utf8_name), Some(1002));
        assert_eq!(effective_uid_from_status_bytes(b"Name:\tx\n"), None);
        assert_eq!(effective_uid_from_status_bytes(b"Uid:\t1000\n"), None);
        // This process: on Linux, a file it creates is owned by its
        // filesystem uid, which equals the effective uid unless changed.
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("owned");
        std::fs::write(&path, b"x")?;
        let owner = std::fs::metadata(&path)?.uid();
        assert_eq!(effective_uid(), Some(owner));
        Ok(())
    }

    /// A control character in a path is escaped in the warning, which stays
    /// one line.
    #[cfg(unix)]
    #[test]
    fn warnings_escape_control_characters_in_paths() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("to\nken\u{1b}[31m");
        std::fs::write(&path, b"secret")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        let warning = secret_file_permission_warning(&path).unwrap_or_default();
        assert!(!warning.contains('\n'), "{warning:?}");
        assert!(!warning.contains('\u{1b}'), "{warning:?}");
        assert!(warning.contains("to\\nken\\u{1b}[31m"), "{warning:?}");
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
