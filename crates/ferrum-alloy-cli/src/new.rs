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
    /// PostgreSQL pool, readiness check, and a separate migration step.
    Postgres,
    /// JWT bearer verification from `alloy.toml` with an `Authorize` policy.
    Jwt,
    /// Instrumented outbound HTTP client with explicit timeouts.
    HttpClient,
}

impl Integration {
    fn feature(self) -> &'static str {
        match self {
            Self::Openapi => "openapi",
            Self::Otel => "otel",
            Self::Edge => "edge",
            Self::Tls => "tls",
            Self::Postgres => "postgres",
            Self::Jwt => "jwt",
            Self::HttpClient => "http-client",
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
    #[arg(
        long,
        conflicts_with_all = ["alloy_git", "alloy_branch", "alloy_tag", "alloy_rev"]
    )]
    pub(crate) alloy_path: Option<PathBuf>,
    /// Git repository for the Ferrum Alloy dependency.
    #[arg(long, default_value = "https://github.com/ferrum-edge/ferrum-alloy")]
    pub(crate) alloy_git: String,
    /// Git branch of the dependency (defaults to `main`).
    #[arg(long, conflicts_with_all = ["alloy_path", "alloy_tag", "alloy_rev"])]
    pub(crate) alloy_branch: Option<String>,
    /// Git tag of the dependency.
    #[arg(long, conflicts_with_all = ["alloy_path", "alloy_branch", "alloy_rev"])]
    pub(crate) alloy_tag: Option<String>,
    /// Git commit ID (7–40 hexadecimal characters) of the dependency.
    #[arg(long, conflicts_with_all = ["alloy_path", "alloy_branch", "alloy_tag"])]
    pub(crate) alloy_rev: Option<String>,
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

const POSTGRES: &[(&str, &str)] = &[
    (
        "src/db.rs",
        include_str!("../templates/postgres/src/db.rs.tmpl"),
    ),
    (
        "tests/db.rs",
        include_str!("../templates/postgres/tests/db.rs.tmpl"),
    ),
    (
        "migrations/0001_create_notes.sql",
        include_str!("../templates/postgres/migrations/0001_create_notes.sql.tmpl"),
    ),
];

const JWT: &[(&str, &str)] = &[
    (
        "src/auth.rs",
        include_str!("../templates/jwt/src/auth.rs.tmpl"),
    ),
    (
        "tests/auth.rs",
        include_str!("../templates/jwt/tests/auth.rs.tmpl"),
    ),
];

const HTTP_CLIENT: &[(&str, &str)] = &[
    (
        "src/upstream.rs",
        include_str!("../templates/http-client/src/upstream.rs.tmpl"),
    ),
    (
        "tests/upstream.rs",
        include_str!("../templates/http-client/tests/upstream.rs.tmpl"),
    ),
];

/// A `postgres`, `jwt`, or `http-client` starter: the module and files it
/// adds, and the fragments it inserts into shared files.
struct Starter {
    integration: Integration,
    /// Module in the generated crate. `STARTERS` is sorted by it, which is
    /// the order rustfmt keeps `mod` declarations and `use` lists in.
    module: &'static str,
    files: &'static [(&'static str, &'static str)],
    dependencies: &'static str,
    dev_dependencies: &'static str,
    main_doc: &'static str,
    main_use: &'static str,
    main_resources: &'static str,
    main_route: &'static str,
    main_readiness: &'static str,
    alloy: &'static str,
    readme: &'static str,
    ci_services: &'static str,
}

const JWT_STARTER: Starter = Starter {
    integration: Integration::Jwt,
    module: "auth",
    files: JWT,
    dependencies: "",
    dev_dependencies: include_str!("../templates/jwt/fragments/dev-dependencies.toml.tmpl"),
    main_doc: "",
    main_use: "",
    main_resources: include_str!("../templates/jwt/fragments/main_resources.rs.tmpl"),
    main_route: include_str!("../templates/jwt/fragments/main_route.rs.tmpl"),
    main_readiness: "",
    alloy: include_str!("../templates/jwt/fragments/alloy.toml.tmpl"),
    readme: include_str!("../templates/jwt/fragments/README.md.tmpl"),
    ci_services: "",
};

const POSTGRES_STARTER: Starter = Starter {
    integration: Integration::Postgres,
    module: "db",
    files: POSTGRES,
    dependencies: include_str!("../templates/postgres/fragments/dependencies.toml.tmpl"),
    dev_dependencies: "",
    main_doc: include_str!("../templates/postgres/fragments/main_doc.rs.tmpl"),
    main_use: "use ferrum_alloy::postgres;\n",
    main_resources: include_str!("../templates/postgres/fragments/main_resources.rs.tmpl"),
    main_route: include_str!("../templates/postgres/fragments/main_route.rs.tmpl"),
    main_readiness: include_str!("../templates/postgres/fragments/main_readiness.rs.tmpl"),
    alloy: include_str!("../templates/postgres/fragments/alloy.toml.tmpl"),
    readme: include_str!("../templates/postgres/fragments/README.md.tmpl"),
    ci_services: include_str!("../templates/postgres/fragments/ci_services.yml.tmpl"),
};

const HTTP_CLIENT_STARTER: Starter = Starter {
    integration: Integration::HttpClient,
    module: "upstream",
    files: HTTP_CLIENT,
    dependencies: include_str!("../templates/http-client/fragments/dependencies.toml.tmpl"),
    dev_dependencies: "",
    main_doc: "",
    main_use: "use ferrum_alloy::http_client::AlloyClient;\n",
    main_resources: include_str!("../templates/http-client/fragments/main_resources.rs.tmpl"),
    main_route: include_str!("../templates/http-client/fragments/main_route.rs.tmpl"),
    main_readiness: "",
    alloy: include_str!("../templates/http-client/fragments/alloy.toml.tmpl"),
    readme: include_str!("../templates/http-client/fragments/README.md.tmpl"),
    ci_services: "",
};

const STARTERS: &[Starter] = &[JWT_STARTER, POSTGRES_STARTER, HTTP_CLIENT_STARTER];

/// `src/main.rs` when any starter is chosen.
const STARTER_MAIN: &str = include_str!("../templates/starters/src/main.rs.tmpl");
const STARTER_DEPENDENCIES: &str =
    include_str!("../templates/starters/fragments/dependencies.toml.tmpl");
const STARTER_DEV_DEPENDENCIES: &str =
    include_str!("../templates/starters/fragments/dev-dependencies.toml.tmpl");
const STARTER_OPENAPI_DOCUMENT: &str =
    include_str!("../templates/starters/fragments/openapi_document.rs.tmpl");
const STARTER_OPENAPI_README: &str =
    include_str!("../templates/starters/fragments/README-openapi.md.tmpl");
const OPENAPI_README: &str = include_str!("../templates/openapi/fragments/README.md.tmpl");
const OPENAPI_AGENTS: &str =
    include_str!("../templates/openapi/fragments/ferrum-service.toml.tmpl");

/// Replacement text for the chosen starters; empty when there are none.
#[derive(Default)]
struct StarterText {
    dependencies: String,
    dev_dependencies: String,
    modules: String,
    main_doc: String,
    main_uses: String,
    main_modules: String,
    main_resources: String,
    main_routes: String,
    main_readiness: String,
    alloy: String,
    readme: String,
    ci_services: String,
    ci_test_args: &'static str,
}

fn starter_text(chosen: &[&Starter]) -> StarterText {
    let mut text = StarterText::default();
    if chosen.is_empty() {
        return text;
    }
    text.dependencies.push_str(STARTER_DEPENDENCIES);
    text.dev_dependencies.push_str(STARTER_DEV_DEPENDENCIES);
    let modules: Vec<&str> = chosen.iter().map(|s| s.module).collect();
    text.main_modules = match modules.as_slice() {
        [single] => (*single).to_owned(),
        _ => format!("{{{}}}", modules.join(", ")),
    };
    text.main_uses.push_str("use ferrum_alloy::AlloyApp;\n");
    // rustfmt order: `http_client` before `postgres`.
    let mut uses: Vec<&str> = chosen.iter().map(|s| s.main_use).collect();
    uses.sort_unstable();
    text.main_uses.extend(uses);
    // The database comes first, so that `cargo run -- migrate` needs no
    // other resource.
    let mut resources: Vec<&Starter> = chosen.to_vec();
    resources.sort_by_key(|s| s.integration != Integration::Postgres);
    let resources: Vec<&str> = resources.iter().map(|s| s.main_resources).collect();
    text.main_resources = resources.join("\n");
    for starter in chosen {
        text.dependencies.push_str(starter.dependencies);
        text.dev_dependencies.push_str(starter.dev_dependencies);
        text.modules.push_str("pub mod ");
        text.modules.push_str(starter.module);
        text.modules.push_str(";\n");
        text.main_doc.push_str(starter.main_doc.trim_end());
        text.main_routes.push_str(starter.main_route);
        text.main_readiness.push_str(starter.main_readiness);
        text.alloy.push_str(starter.alloy);
        text.readme.push_str(starter.readme);
        text.ci_services.push_str(starter.ci_services);
    }
    text.modules.push('\n');
    if !text.ci_services.is_empty() {
        text.ci_test_args = " -- --include-ignored";
    }
    text
}

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
    "sqlx",
    "tracing",
    "jsonwebtoken",
    "rcgen",
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

fn validate_git_selector(option: &str, value: &str) -> Result<(), CliError> {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(CliError::Invalid(format!(
            "{option} must be a non-empty Git ref without whitespace"
        )));
    }
    Ok(())
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
            let (key, value) = if let Some(branch) = &args.alloy_branch {
                validate_git_selector("--alloy-branch", branch)?;
                ("branch", branch.as_str())
            } else if let Some(tag) = &args.alloy_tag {
                validate_git_selector("--alloy-tag", tag)?;
                ("tag", tag.as_str())
            } else if let Some(rev) = &args.alloy_rev {
                if !(7..=40).contains(&rev.len())
                    || !rev.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(CliError::Invalid(
                        concat!(
                            "--alloy-rev must be a 7–40 character hexadecimal commit ID; ",
                            "use --alloy-branch or --alloy-tag for named refs"
                        )
                        .into(),
                    ));
                }
                ("rev", rev.as_str())
            } else {
                ("branch", "main")
            };
            format!(
                "# Ferrum Alloy is not published to crates.io; pin a commit with --alloy-rev.\nferrum-alloy = {{ git = {}, {key} = {}{features} }}",
                toml_string(&args.alloy_git),
                toml_string(value)
            )
        }
    };
    let openapi = with.contains(&Integration::Openapi);
    let crate_name = args.name.replace('-', "_");
    let chosen: Vec<&Starter> = STARTERS
        .iter()
        .filter(|s| with.contains(&s.integration))
        .collect();
    let starters = starter_text(&chosen);
    let openapi_dependencies = if openapi {
        "utoipa = { version = \"6\", features = [\"macros\"] }\nutoipa-axum = \"0.3\"\n"
    } else {
        ""
    };
    let extra_dependencies = format!("{openapi_dependencies}{}", starters.dependencies);
    // `src/bin/openapi.rs` is a second binary, so `cargo run` needs a default.
    let default_run = if openapi {
        "default-run = \"{{name}}\"\n"
    } else {
        ""
    };
    let (openapi_document, openapi_registration, openapi_readme) = if openapi {
        (
            STARTER_OPENAPI_DOCUMENT,
            ".openapi(&document)",
            STARTER_OPENAPI_README,
        )
    } else {
        ("", "", "")
    };
    let readme_starters = if chosen.is_empty() {
        String::new()
    } else {
        format!("{}{openapi_readme}", starters.readme)
    };
    let replacements = [
        ("{{name}}", args.name.as_str()),
        ("{{crate_name}}", crate_name.as_str()),
        ("{{alloy_dependency}}", alloy_dependency.as_str()),
        ("{{default_run}}", default_run),
        ("{{extra_dependencies}}", extra_dependencies.as_str()),
        (
            "{{extra_dev_dependencies}}",
            starters.dev_dependencies.as_str(),
        ),
        ("{{starter_modules}}", starters.modules.as_str()),
        ("{{alloy_starters}}", starters.alloy.as_str()),
        ("{{readme_starters}}", readme_starters.as_str()),
        ("{{ci_services}}", starters.ci_services.as_str()),
        ("{{ci_test_args}}", starters.ci_test_args),
        ("{{main_doc}}", starters.main_doc.as_str()),
        ("{{main_uses}}", starters.main_uses.as_str()),
        ("{{main_modules}}", starters.main_modules.as_str()),
        ("{{main_resources}}", starters.main_resources.as_str()),
        ("{{main_routes}}", starters.main_routes.as_str()),
        ("{{main_openapi_document}}", openapi_document),
        ("{{main_openapi_registration}}", openapi_registration),
        ("{{main_readiness}}", starters.main_readiness.as_str()),
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
            if openapi { OPENAPI_README } else { "" },
        ),
        (
            "{{manifest_agents}}",
            if openapi { OPENAPI_AGENTS } else { "" },
        ),
    ];
    let mut sources: Vec<(&str, &str)> = BASE.to_vec();
    if openapi {
        sources.extend_from_slice(OPENAPI);
    }
    if !chosen.is_empty() {
        sources.push(("src/main.rs", STARTER_MAIN));
    }
    for starter in &chosen {
        sources.extend_from_slice(starter.files);
    }
    let mut files: Vec<(String, String)> = Vec::new();
    for (path, template) in &sources {
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
