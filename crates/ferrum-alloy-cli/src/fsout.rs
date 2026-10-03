//! Safe output-file operations shared by CLI commands.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::hash::{BuildHasher, Hasher};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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

/// Atomically writes a file in its parent directory. A symlink at the final
/// path is replaced rather than followed. New files use mode 0600 on Unix;
/// replacements preserve only the permission bits of an existing regular file.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let failed = |error: std::io::Error| CliError::Io(format!("write {}: {error}", path.display()));
    let Some(name) = path.file_name() else {
        return Err(CliError::Invalid(format!("{} names no file", path.display())));
    };
    #[cfg(unix)]
    let permissions = preserved_permissions(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut attempts = 1;
    let (temp, mut file) = loop {
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(temp_suffix());
        let temp = path.with_file_name(temp_name);
        match options.open(&temp) {
            Ok(file) => break (temp, file),
            Err(error)
                if error.kind() == ErrorKind::AlreadyExists && attempts < TEMP_ATTEMPTS =>
            {
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

/// Creates each missing directory separately and rejects symlinks and
/// non-directory components. This avoids create_dir_all traversing a link that
/// was already present when the component is checked.
pub(crate) fn create_dirs(path: &Path) -> Result<(), CliError> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {
                current.push(component.as_os_str());
            }
            Component::ParentDir | Component::Normal(_) => current.push(component.as_os_str()),
        }
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
                let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
                    CliError::Io(format!("{}: {error}", current.display()))
                })?;
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

/// Creates a new output file exclusively, so a pre-existing leaf (including
/// a symlink or Windows reparse point) cannot be followed or overwritten.
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| CliError::Io(format!("create {}: {error}", path.display())))?;
    file.write_all(bytes)
        .map_err(|error| CliError::Io(format!("write {}: {error}", path.display())))
}
