//! `ferrum-alloy new`: generates a standalone project from embedded
//! templates.
//!
//! Safety rules: the name is validated (no shell evaluation ever happens),
//! the target must not exist or must be an empty real directory (never a
//! symlink), every file is created with `create_new` so nothing is
//! overwritten, and nothing is downloaded. Cargo fetches dependencies when
//! the user builds the project.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

use crate::error::CliError;

/// Optional integrations for a new project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum Integration {
    /// utoipa OpenAPI documentation, an `openapi` bin, and a parity test.
    Openapi,
    /// OpenTelemetry OTLP export (disabled until configured).
    Otel,
    /// Ferrum Edge adapter.
    Edge,
    /// rustls TLS termination.
    Tls,
}

impl Integration {
    fn feature(self) -> &'static str {
        match self {
            Self::Openapi => "openapi",
            Self::Otel => "otel",
            Self::Edge => "edge",
            Self::Tls => "tls",
        }
    }
}

/// Arguments for `new`.
#[derive(Debug, Args)]
pub(crate) struct NewArgs {
    /// Project (crate) name: lowercase letters, digits, and `-`.
    pub(crate) name: String,
    /// Target directory (default: `./<name>`). Must not exist or be empty.
    #[arg(long)]
    pub(crate) path: Option<PathBuf>,
    /// Optional integrations.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub(crate) with: Vec<Integration>,
    /// Depend on a local Ferrum Alloy checkout (path to `crates/ferrum-alloy`).
    #[arg(long, conflicts_with_all = ["alloy_git", "alloy_rev"])]
    pub(crate) alloy_path: Option<PathBuf>,
    /// Git repository for the Ferrum Alloy dependency.
    #[arg(long, default_value = "https://github.com/ferrum-edge/ferrum-alloy")]
    pub(crate) alloy_git: String,
    /// Git revision (commit, tag, or branch) of the dependency. Pin a commit
    /// for reproducible builds.
    #[arg(long, default_value = "main")]
    pub(crate) alloy_rev: String,
}

const BASE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../templates/base/Cargo.toml.tmpl"),
    ),
    (
        "src/main.rs",
        include_str!("../templates/base/src/main.rs.tmpl"),
    ),
    (
        "src/lib.rs",
        include_str!("../templates/base/src/lib.rs.tmpl"),
    ),
    (
        "tests/api.rs",
        include_str!("../templates/base/tests/api.rs.tmpl"),
    ),
    (
        "alloy.toml",
        include_str!("../templates/base/alloy.toml.tmpl"),
    ),
    (
        "ferrum-service.toml",
        include_str!("../templates/base/ferrum-service.toml.tmpl"),
    ),
    (
        "README.md",
        include_str!("../templates/base/README.md.tmpl"),
    ),
    (
        ".gitignore",
        include_str!("../templates/base/gitignore.tmpl"),
    ),
    (
        ".github/workflows/ci.yml",
        include_str!("../templates/base/.github/workflows/ci.yml.tmpl"),
    ),
];

const OPENAPI: &[(&str, &str)] = &[
    (
        "src/lib.rs",
        include_str!("../templates/openapi/src/lib.rs.tmpl"),
    ),
    (
        "src/bin/openapi.rs",
        include_str!("../templates/openapi/src/bin/openapi.rs.tmpl"),
    ),
    (
        "tests/openapi.rs",
        include_str!("../templates/openapi/tests/openapi.rs.tmpl"),
    ),
];

const RESERVED: &[&str] = &[
    "test",
    "core",
    "std",
    "alloc",
    "proc-macro",
    "proc_macro",
    "self",
    "super",
    "crate",
    "build",
    "ferrum-alloy",
    "axum",
    "tokio",
    "serde",
    "tower",
    "http",
    "utoipa",
    "utoipa-axum",
];

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where", "while",
    "gen", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof",
    "unsized", "virtual", "yield", "try",
];

/// Validates a project name.
pub(crate) fn validate_name(name: &str) -> Result<(), CliError> {
    let bytes = name.as_bytes();
    let well_formed = !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !well_formed {
        return Err(CliError::Invalid(format!(
            "invalid project name {name:?}: use 1-63 lowercase letters, digits, and single '-', starting with a letter"
        )));
    }
    let crate_name = name.replace('-', "_");
    if RESERVED.contains(&name) || KEYWORDS.contains(&crate_name.as_str()) {
        return Err(CliError::Invalid(format!(
            "project name {name:?} is reserved"
        )));
    }
    Ok(())
}

fn prepare_target(path: &Path) -> Result<(), CliError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(CliError::Invalid(format!(
                    "{} is a symbolic link; refusing to write through it",
                    path.display()
                )));
            }
            if !metadata.is_dir() {
                return Err(CliError::Invalid(format!(
                    "{} exists and is not a directory",
                    path.display()
                )));
            }
            let mut entries = std::fs::read_dir(path)
                .map_err(|e| CliError::Io(format!("{}: {e}", path.display())))?;
            if entries.next().is_some() {
                return Err(CliError::Invalid(format!(
                    "{} is not empty; refusing to overwrite",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !parent.is_dir() {
                return Err(CliError::Invalid(format!(
                    "parent directory {} does not exist",
                    parent.display()
                )));
            }
            std::fs::create_dir(path)
                .map_err(|e| CliError::Io(format!("create {}: {e}", path.display())))
        }
        Err(error) => Err(CliError::Io(format!("{}: {error}", path.display()))),
    }
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

/// Renders every file for `args`, returning `(relative path, content)`.
pub(crate) fn render(args: &NewArgs) -> Result<Vec<(String, String)>, CliError> {
    validate_name(&args.name)?;
    let mut with = args.with.clone();
    with.sort();
    with.dedup();
    let features: Vec<String> = with.iter().map(|i| toml_string(i.feature())).collect();
    let features = if features.is_empty() {
        String::new()
    } else {
        format!(", features = [{}]", features.join(", "))
    };
    let alloy_dependency = match &args.alloy_path {
        Some(path) => {
            let path = crate::absolute(path)?;
            if !path.join("Cargo.toml").is_file() {
                return Err(CliError::Invalid(format!(
                    "--alloy-path {} does not contain a Cargo.toml (point it at crates/ferrum-alloy)",
                    path.display()
                )));
            }
            format!(
                "ferrum-alloy = {{ path = {}{features} }}",
                toml_string(&path.to_string_lossy())
            )
        }
        None => {
            if !args.alloy_git.starts_with("https://")
                || args.alloy_git.contains(char::is_whitespace)
            {
                return Err(CliError::Invalid("--alloy-git must be an https URL".into()));
            }
            if args.alloy_rev.is_empty()
                || !args
                    .alloy_rev
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
            {
                return Err(CliError::Invalid(
                    "--alloy-rev contains unsupported characters".into(),
                ));
            }
            let key = if args.alloy_rev.len() == 40
                && args.alloy_rev.bytes().all(|b| b.is_ascii_hexdigit())
            {
                "rev"
            } else {
                "branch"
            };
            format!(
                "# Ferrum Alloy is not published to crates.io; pin a commit with --alloy-rev.\nferrum-alloy = {{ git = {}, {key} = {}{features} }}",
                toml_string(&args.alloy_git),
                toml_string(&args.alloy_rev)
            )
        }
    };
    let openapi = with.contains(&Integration::Openapi);
    let crate_name = args.name.replace('-', "_");
    let extra_dependencies = if openapi {
        "utoipa = { version = \"6\", features = [\"macros\"] }\nutoipa-axum = \"0.3\"\n"
    } else {
        ""
    };
    let replacements = [
        ("{{name}}", args.name.as_str()),
        ("{{crate_name}}", crate_name.as_str()),
        ("{{alloy_dependency}}", alloy_dependency.as_str()),
        ("{{extra_dependencies}}", extra_dependencies),
        (
            "{{openapi_registration}}",
            if openapi {
                "        .openapi(&{{crate_name}}::openapi())\n"
            } else {
                ""
            },
        ),
        (
            "{{readme_openapi}}",
            if openapi {
                "\n## OpenAPI\n\n```bash\nferrum-alloy openapi export --manifest ferrum-service.toml --output openapi.json\nferrum-alloy openapi export --manifest ferrum-service.toml --output openapi.json --check\n```\n\nThe document is served on the management listener at `/openapi.json` (token-protected).\n"
            } else {
                ""
            },
        ),
    ];
    let mut files: Vec<(String, String)> = Vec::new();
    let sources = BASE.iter().chain(if openapi { OPENAPI } else { &[] });
    for (path, template) in sources {
        let mut content = (*template).to_owned();
        // Two passes: some replacements insert other placeholders.
        for _ in 0..2 {
            for (placeholder, value) in &replacements {
                content = content.replace(placeholder, value);
            }
        }
        if let Some(existing) = files.iter_mut().find(|(p, _)| p == path) {
            existing.1 = content;
        } else {
            files.push(((*path).to_owned(), content));
        }
    }
    for (path, content) in &files {
        if content.contains("{{") {
            return Err(CliError::Io(format!(
                "internal template error: unreplaced placeholder in {path}"
            )));
        }
    }
    Ok(files)
}

/// Runs `new`.
pub(crate) fn run(args: NewArgs) -> Result<(), CliError> {
    let files = render(&args)?;
    let target = crate::absolute(
        &args
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&args.name)),
    )?;
    prepare_target(&target)?;
    for (relative, content) in &files {
        let path = target.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CliError::Io(format!("create {}: {e}", parent.display())))?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| CliError::Io(format!("create {}: {e}", path.display())))?;
        file.write_all(content.as_bytes())
            .map_err(|e| CliError::Io(format!("write {}: {e}", path.display())))?;
    }
    crate::print(&format!(
        "Created {} in {}\n\nNext:\n  cd {}\n  cargo test\n  cargo run\n",
        args.name,
        target.display(),
        target.display()
    ))
}
