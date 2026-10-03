//! Safe output-file operations shared by CLI commands.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::hash::{BuildHasher, Hasher};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use crate::error::CliError;

const TEMP_ATTEMPTS: u32 = 5;

fn temp_suffix() -> String {
    let random = RandomState::new().build_hasher().finish();
    format!(".{}.{random:016x}.tmp", std::process::id())
}

#[cfg(unix)]
fn preserved_permissions(path: &Path) -> Option<std::fs::Permissions> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| std::fs::Permissions::from_mode(metadata.permissions().mode() & 0o777))
}

/// Permissions for a newly created atomic output file.
#[derive(Clone, Copy)]
pub(crate) enum NewFileMode {
    /// Respect the process umask, as for ordinary generated artifacts.
    Umask,
    /// Restrict the file to its owner, for potentially sensitive reports.
    Private,
}

/// Atomically writes a file in its parent directory. A symlink at the final
/// path is replaced rather than followed. New files use the selected mode on
/// Unix; replacements preserve only the permission bits of an existing regular file.
pub(crate) fn write_atomically(
    path: &Path,
    bytes: &[u8],
    new_file_mode: NewFileMode,
) -> Result<(), CliError> {
    #[cfg(not(unix))]
    let _ = new_file_mode;
    let failed = |error: std::io::Error| CliError::Io(format!("write {}: {error}", path.display()));
    let Some(name) = path.file_name() else {
        return Err(CliError::Invalid(format!(
            "{} names no file",
            path.display()
        )));
    };
    #[cfg(unix)]
    let permissions = preserved_permissions(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(match new_file_mode {
        NewFileMode::Umask => 0o666,
        NewFileMode::Private => 0o600,
    });
    let mut attempts = 1;
    let (temp, mut file) = loop {
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(temp_suffix());
        let temp = path.with_file_name(temp_name);
        match options.open(&temp) {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists && attempts < TEMP_ATTEMPTS => {
                attempts += 1;
            }
            Err(error) => return Err(failed(error)),
        }
    };
    let written = file.write_all(bytes);
    #[cfg(unix)]
    let written = written.and_then(|()| match permissions {
        Some(permissions) => file.set_permissions(permissions),
        None => Ok(()),
    });
    let written = written.and_then(|()| file.sync_all());
    drop(file);
    let renamed = written.and_then(|()| replace_by_rename(&temp, path));
    if let Err(error) = renamed {
        let _ = std::fs::remove_file(&temp);
        return Err(failed(error));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_by_rename(temp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, path)
}

#[cfg(windows)]
fn replace_by_rename(temp: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::rename(temp, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    return Err(error);
                }
                Ok(_) => std::fs::remove_file(path)?,
                Err(check_error) if check_error.kind() == ErrorKind::NotFound => {}
                Err(check_error) => return Err(check_error),
            }
            std::fs::rename(temp, path)
        }
        Err(error) => Err(error),
    }
}

/// Creates an output root and its descendants. Ancestors of `root` are user
/// path context; only `root` and components beneath it are checked for links.
pub(crate) fn create_dirs(root: &Path, path: &Path) -> Result<(), CliError> {
    let root = normalize_output_root(root)?;
    let relative = path.strip_prefix(&root).map_err(|_| {
        CliError::Invalid(format!(
            "{} is outside output root {}",
            path.display(),
            root.display()
        ))
    })?;
    let parent = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| CliError::Io(format!("create {}: {error}", parent.display())))?;
    let mut directories = vec![root.to_path_buf()];
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(CliError::Invalid(format!(
                "{} contains an invalid output path component",
                path.display()
            )));
        };
        current.push(name);
        directories.push(current.clone());
    }
    for current in directories {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CliError::Invalid(format!(
                    "{} must be a real directory",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|error| {
                    CliError::Io(format!("create {}: {error}", current.display()))
                })?;
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|error| CliError::Io(format!("{}: {error}", current.display())))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(CliError::Invalid(format!(
                        "{} must be a real directory",
                        current.display()
                    )));
                }
            }
            Err(error) => {
                return Err(CliError::Io(format!("{}: {error}", current.display())));
            }
        }
    }
    Ok(())
}

/// Removes trailing separators and `.` components from a directory output
/// root while refusing roots whose final component is `..` or absent.
pub(crate) fn normalize_output_root(path: &Path) -> Result<std::path::PathBuf, CliError> {
    let Some(name) = path.file_name() else {
        return Err(CliError::Invalid(format!(
            "{} must name an output directory",
            path.display()
        )));
    };
    if name == ".." {
        return Err(CliError::Invalid(format!(
            "{} must not end in ..",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.join(name))
}

/// Creates a new output file exclusively, so a pre-existing leaf (including
/// a symlink or Windows reparse point) cannot be followed or overwritten.
pub(crate) struct WriteNewError {
    operation: &'static str,
    path: std::path::PathBuf,
    source: std::io::Error,
}

impl WriteNewError {
    pub(crate) fn kind(&self) -> ErrorKind {
        self.source.kind()
    }
}

impl std::fmt::Display for WriteNewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), WriteNewError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| WriteNewError {
            operation: "create",
            path: path.to_path_buf(),
            source,
        })?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(WriteNewError {
            operation: "write",
            path: path.to_path_buf(),
            source: error,
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn temporary_names_differ_between_attempts() {
        let (first, second) = (temp_suffix(), temp_suffix());
        assert_ne!(first, second);
        assert!(first.ends_with(".tmp"), "{first}");
    }

    #[cfg(unix)]
    #[test]
    fn create_dirs_checks_symlinks_beneath_the_output_root_only() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let linked_parent = dir.path().join("linked-parent");
        symlink(&outside, &linked_parent).unwrap();
        let root = linked_parent.join("not-yet-created-root");
        create_dirs(&root, &root).unwrap();
        assert!(root.is_dir());

        let output_root = dir.path().join("output-root");
        std::fs::create_dir(&output_root).unwrap();
        let linked_child = output_root.join("linked-child");
        symlink(&outside, &linked_child).unwrap();
        assert!(create_dirs(&output_root, &linked_child.join("nested")).is_err());
        assert!(!outside.join("nested").exists());
    }
}
