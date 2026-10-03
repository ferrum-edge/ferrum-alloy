//! `ferrum-alloy edge export`: reviewable gateway configuration.
//!
//! Produces files only. It never contacts a gateway or admin API.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use ferrum_alloy_edge::export;
use ferrum_alloy_edge::manifest::ServiceManifest;

use crate::error::CliError;
use crate::input::{ReadError, read_regular_file_bounded};

/// Output kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum EdgeFormat {
    /// One Ferrum Edge file-mode document (`FERRUM_MODE=file`).
    #[default]
    EdgeFile,
    /// A GitForgeOps `resources/<namespace>/...` tree.
    Gitforgeops,
}

/// `edge` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum EdgeCommand {
    /// Generate gateway configuration from a service manifest.
    Export(ExportArgs),
}

/// Arguments for `edge export`.
#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    /// Service manifest (PROPOSED `ferrum.service_manifest` v1).
    #[arg(long, default_value = "ferrum-service.toml")]
    manifest: PathBuf,
    /// Output kind.
    #[arg(long, value_enum, default_value_t)]
    format: EdgeFormat,
    /// Output file (edge-file; default stdout) or directory (gitforgeops,
    /// must not exist or be empty).
    #[arg(long)]
    output: Option<PathBuf>,
    /// Replace an existing edge-file output.
    #[arg(long)]
    force: bool,
}

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

pub(crate) fn read_manifest(path: &Path) -> Result<ServiceManifest, CliError> {
    let bytes = match read_regular_file_bounded(path, MAX_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(ReadError::Io(e)) => {
            return Err(CliError::Invalid(format!("{}: {e}", path.display())));
        }
        Err(_) => {
            return Err(CliError::Invalid(format!(
                "{} must be a regular file of at most {MAX_MANIFEST_BYTES} bytes",
                path.display()
            )));
        }
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| CliError::Invalid(format!("{} is not UTF-8", path.display())))?;
    ServiceManifest::from_toml(&text).map_err(|e| CliError::Invalid(e.to_string()))
}

/// Runs an `edge` subcommand.
pub(crate) fn run(command: EdgeCommand) -> Result<(), CliError> {
    let EdgeCommand::Export(args) = command;
    let manifest = read_manifest(&args.manifest)?;
    let resources = export::resources(&manifest);
    match args.format {
        EdgeFormat::EdgeFile => {
            let yaml = export::file_mode_yaml(&resources);
            match &args.output {
                Some(path) => {
                    if args.force {
                        crate::fsout::write_atomically(path, yaml.as_bytes())?;
                    } else {
                        crate::fsout::write_new(path, yaml.as_bytes()).map_err(|error| {
                            match std::fs::symlink_metadata(path) {
                                Ok(_) => CliError::Invalid(format!(
                                    "{} exists; pass --force to replace it",
                                    path.display()
                                )),
                                Err(_) => error,
                            }
                        })?;
                    }
                    crate::eprint(&format!(
                        "wrote {} (validate with: ferrum-edge validate -m file -c {})\n",
                        path.display(),
                        path.display()
                    ));
                    Ok(())
                }
                None => crate::print(&yaml),
            }
        }
        EdgeFormat::Gitforgeops => {
            let root = args.output.ok_or_else(|| {
                CliError::Invalid("--output DIR is required for --format gitforgeops".into())
            })?;
            if let Ok(metadata) = std::fs::symlink_metadata(&root) {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(CliError::Invalid(format!(
                        "{} must be a real directory",
                        root.display()
                    )));
                }
                let mut entries =
                    std::fs::read_dir(&root).map_err(|e| CliError::Io(e.to_string()))?;
                if entries.next().is_some() {
                    return Err(CliError::Invalid(format!(
                        "{} is not empty; refusing to overwrite",
                        root.display()
                    )));
                }
            }
            for file in export::gitforgeops_files(&resources, &manifest.gateway.namespace) {
                let path = root.join(&file.path);
                if let Some(parent) = path.parent() {
                    crate::fsout::create_dirs(parent)?;
                }
                crate::fsout::write_new(&path, file.content.as_bytes())?;
                crate::eprint(&format!("wrote {}\n", path.display()));
            }
            Ok(())
        }
    }
}
