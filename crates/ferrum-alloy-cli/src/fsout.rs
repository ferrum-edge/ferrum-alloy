//! Output operations anchored to owned directory handles, never checked paths.

use std::collections::{BTreeMap, hash_map::RandomState};
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::DirExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
#[cfg(unix)]
use cap_std::fs::{OpenOptionsExt, Permissions, PermissionsExt};

use crate::error::CliError;

const TEMP_ATTEMPTS: u32 = 5;

fn temp_suffix() -> String {
    let random = RandomState::new().build_hasher().finish();
    format!(".{}.{random:016x}.tmp", std::process::id())
}

/// All handles live until the whole output operation completes. The map keys
/// and `path` are labels only: filesystem operations receive a retained `Dir`
/// and one normal component. In particular, a cached descendant is never
/// reopened through its old name, and no raw descriptor/path alias is stored.
pub(crate) struct OutputDir {
    path: PathBuf,
    root: Dir,
    descendants: BTreeMap<PathBuf, Dir>,
    // Windows cap-std locks directories against rename/delete. Retain the
    // opened parent too; ancestors above it are deliberately trusted context.
    _context: Option<Dir>,
}

impl OutputDir {
    /// Opens (or creates) the named root without following its final component.
    /// Identity is accepted at this open, before inspecting entries through the
    /// handle. A mkdir is not an identity check: any concurrent replacement is
    /// subject to the same no-follow open and empty-directory check.
    pub(crate) fn empty_tree(path: &Path, create_ancestors: bool) -> Result<Self, CliError> {
        let path = normalize_output_root(path)?;
        if create_ancestors {
            let parent = parent_context(&path);
            std::fs::create_dir_all(parent)
                .map_err(|error| CliError::Io(format!("create {}: {error}", parent.display())))?;
        }
        let output = Self::open_named(&path, true)?;
        let mut entries = output
            .root
            .entries()
            .map_err(|error| CliError::Io(format!("{}: {error}", path.display())))?;
        if entries.next().is_some() {
            return Err(CliError::Invalid(format!(
                "{} is not empty; refusing to overwrite",
                path.display()
            )));
        }
        #[cfg(test)]
        tests::checkpoint("root", &path);
        Ok(output)
    }

    fn open_named(path: &Path, create: bool) -> Result<Self, CliError> {
        let path = normalize_output_root(path)?;
        let parent = parent_context(&path);
        let context = Dir::open_ambient_dir(parent, ambient_authority()).map_err(|error| {
            CliError::Invalid(format!("parent directory {}: {error}", parent.display()))
        })?;
        let name = path.file_name().ok_or_else(|| {
            CliError::Invalid(format!("{} must name an output directory", path.display()))
        })?;
        let root = match context.open_dir_nofollow(name) {
            Ok(root) => root,
            Err(error) if create && error.kind() == ErrorKind::NotFound => {
                match context.create_dir(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(CliError::Io(format!("create {}: {error}", path.display())));
                    }
                }
                #[cfg(test)]
                tests::checkpoint("root-created", &path);
                context.open_dir_nofollow(name).map_err(|error| {
                    CliError::Invalid(format!(
                        "{} must be a real directory: {error}",
                        path.display()
                    ))
                })?
            }
            Err(error) => {
                return Err(CliError::Invalid(format!(
                    "{} must be a real directory: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            root,
            descendants: BTreeMap::new(),
            _context: Some(context),
        })
    }

    /// For a single file, its immediate parent is the selected root. Above
    /// that directory symlinks remain operator-trusted context. Unnamed roots
    /// (`.`, `..`, filesystem roots) use an ambient directory handle directly;
    /// the final component cannot itself be a directory link in those cases.
    pub(crate) fn for_file(path: &Path) -> Result<(Self, OsString), CliError> {
        let name = path.file_name().ok_or_else(|| {
            CliError::Invalid(format!("{} names no file", path.display()))
        })?;
        let parent = parent_context(path);
        let output = if parent.file_name().is_some() {
            Self::open_named(parent, false)?
        } else if matches!(
            parent.components().last(),
            Some(Component::RootDir | Component::CurDir | Component::ParentDir)
        ) {
            let root = Dir::open_ambient_dir(parent, ambient_authority())
                .map_err(|error| CliError::Io(format!("{}: {error}", parent.display())))?;
            Self {
                path: parent.to_path_buf(),
                root,
                descendants: BTreeMap::new(),
                _context: None,
            }
        } else {
            return Err(CliError::Invalid(format!(
                "{} must name an output directory",
                parent.display()
            )));
        };
        #[cfg(test)]
        tests::checkpoint("root", &output.path);
        Ok((output, name.to_os_string()))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn directory(&self, relative: &Path) -> io::Result<&Dir> {
        if relative.as_os_str().is_empty() {
            Ok(&self.root)
        } else {
            self.descendants.get(relative).ok_or_else(|| {
                io::Error::new(ErrorKind::NotFound, "output directory handle is absent")
            })
        }
    }

    fn prepare_parent(&mut self, relative: &Path) -> io::Result<(PathBuf, OsString)> {
        let mut names = Vec::new();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "output paths must contain only normal components",
                ));
            };
            names.push(name.to_os_string());
        }
        let leaf = names.pop().ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidInput, "output path names no file")
        })?;
        let mut parent = PathBuf::new();
        for name in names {
            let next = parent.join(&name);
            if self.directory(&next).is_err() {
                let base = self.directory(&parent)?;
                let child = match base.open_dir_nofollow(&name) {
                    Ok(child) => child,
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        match base.create_dir(&name) {
                            Ok(()) => {}
                            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                        #[cfg(test)]
                        tests::checkpoint("directory-created", &self.path.join(&next));
                        base.open_dir_nofollow(&name)?
                    }
                    Err(error) => return Err(error),
                };
                self.descendants.insert(next.clone(), child);
                #[cfg(test)]
                tests::checkpoint("directory", &self.path.join(&next));
            }
            parent = next;
        }
        Ok((parent, leaf))
    }

    /// Creates a leaf exclusively relative to its retained parent. Duplicate
    /// leaves, symlinks and reparse points cannot be followed or overwritten.
    pub(crate) fn write_new(&mut self, relative: &Path, bytes: &[u8]) -> Result<(), WriteNewError> {
        let path = self.path.join(relative);
        let failed = |operation, source| WriteNewError {
            operation,
            path: path.clone(),
            source,
        };
        let (parent, name) = self
            .prepare_parent(relative)
            .map_err(|source| failed("open directory for", source))?;
        let dir = self
            .directory(&parent)
            .map_err(|source| failed("open directory for", source))?;
        #[cfg(test)]
        tests::checkpoint("leaf", &path);
        let mut file = dir
            .open_with(&name, OpenOptions::new().write(true).create_new(true))
            .map_err(|source| failed("create", source))?;
        if let Err(error) = file.write_all(bytes) {
            // Do not unlink by name after a failed write: a concurrent actor
            // may have replaced that name with an unrelated entry.
            return Err(failed("write", error));
        }
        Ok(())
    }

    fn write_atomically(
        &self,
        name: &Path,
        bytes: &[u8],
        new_file_mode: NewFileMode,
    ) -> io::Result<()> {
        #[cfg(not(unix))]
        let _ = new_file_mode;
        #[cfg(unix)]
        let permissions = self
            .root
            .symlink_metadata(name)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| Permissions::from_mode(metadata.permissions().mode() & 0o777));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(match new_file_mode {
            NewFileMode::Umask => 0o666,
            NewFileMode::Private => 0o600,
        });
        let mut attempts = 1;
        let (temp, mut file) = loop {
            let mut temp = OsString::from(".");
            temp.push(name);
            temp.push(temp_suffix());
            match self.root.open_with(&temp, &options) {
                Ok(file) => break (temp, file),
                Err(error) if error.kind() == ErrorKind::AlreadyExists && attempts < TEMP_ATTEMPTS => {
                    attempts += 1;
                }
                Err(error) => return Err(error),
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
        #[cfg(test)]
        tests::checkpoint("rename", &self.path);
        let renamed = written.and_then(|()| self.root.rename(&temp, &self.root, name));
        if let Err(error) = renamed {
            let _ = self.root.remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }
}

fn parent_context(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Permissions for a newly created atomic output file.
#[derive(Clone, Copy)]
pub(crate) enum NewFileMode {
    /// Respect the process umask, as for ordinary generated artifacts.
    Umask,
    /// Restrict the file to its owner, for potentially sensitive reports.
    Private,
}

/// Holds the immediate parent throughout temporary creation, writing and
/// replacement. A final symlink is replaced rather than followed.
pub(crate) fn write_atomically(
    path: &Path,
    bytes: &[u8],
    new_file_mode: NewFileMode,
) -> Result<(), CliError> {
    let (output, name) = OutputDir::for_file(path)?;
    output
        .write_atomically(Path::new(&name), bytes, new_file_mode)
        .map_err(|error| CliError::Io(format!("write {}: {error}", path.display())))
}

/// Removes trailing separators and `.` components, refusing `..` or an
/// unnamed root. Normalization is syntax only, never a filesystem fence.
fn normalize_output_root(path: &Path) -> Result<PathBuf, CliError> {
    let Some(name) = path.file_name() else {
        return Err(CliError::Invalid(format!(
            "{} must name an output directory",
            path.display()
        )));
    };
    Ok(parent_context(path).join(name))
}

pub(crate) struct WriteNewError {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
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

#[cfg(test)]
#[path = "fsout_tests.rs"]
mod tests;
