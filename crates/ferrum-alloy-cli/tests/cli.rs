//! The `ferrum-alloy` binary: exit codes, formats, and safety rules.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrum-alloy"));
    // Keep the caller's FERRUM_ALLOY_* variables out of the tests.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FERRUM_ALLOY_") {
            command.env_remove(name);
        }
    }
    command
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().unwrap()
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(path: &str) -> String {
    repo()
        .join("contracts/fixtures")
        .join(path)
        .to_string_lossy()
        .into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

#[test]
fn version_reports_contract_versions() {
    let output = run(&["version", "--format", "json"]);
    assert_eq!(code(&output), 0);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["edge_contract"]["release"], "v0.9.7");
    assert!(
        json["diagnostic_report_schema"]
            .as_str()
            .unwrap()
            .contains("ferrum.diagnostic_report")
    );
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(code(&run(&["no-such-command"])), 2);
    assert_eq!(code(&run(&["diagnose"])), 2, "a source is required");
}

#[test]
fn diagnose_explains_reports_in_both_formats() {
    let human = run(&[
        "diagnose",
        "--input",
        &fixture("reports/unattributed-interval.json"),
    ]);
    assert_eq!(code(&human), 0, "{}", stderr(&human));
    assert!(stdout(&human).contains("Large unattributed interval"));
    assert!(stdout(&human).contains("provenance is unverified"));

    let json = run(&[
        "diagnose",
        "--input",
        &fixture("reports/db-operation-dominates.json"),
        "--format",
        "json",
    ]);
    assert_eq!(code(&json), 0);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(
        value["report"]["findings"][0]["code"],
        "alloy.service.operation_dominates"
    );
}

#[test]
fn diagnose_downgrades_forged_trust_claims() {
    let output = run(&[
        "diagnose",
        "--input",
        &fixture("reports/forged-verified-claim.json"),
        "--format",
        "json",
    ]);
    assert_eq!(code(&output), 0);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["claimed_verification"], "verified");
    assert_eq!(value["report"]["collection"]["verification"], "unverified");
    assert_eq!(value["report"]["findings"][0]["confidence"], "likely");
}

#[test]
fn diagnose_rejects_invalid_reports_with_exit_3() {
    let output = run(&[
        "diagnose",
        "--input",
        &fixture("reports/unsupported-major.json"),
    ]);
    assert_eq!(code(&output), 3);
    assert!(stderr(&output).contains("schema_version"));
    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("garbage.json");
    std::fs::write(&garbage, "{").unwrap();
    assert_eq!(
        code(&run(&["diagnose", "--input", garbage.to_str().unwrap()])),
        3
    );
}

#[test]
fn diagnose_reads_otlp_exports() {
    let path = fixture("otlp/edge-alloy-trace.jsonl");
    let list = run(&["diagnose", "--otlp", &path, "--list-traces"]);
    assert_eq!(code(&list), 0);
    assert_eq!(stdout(&list).lines().count(), 2);
    let ambiguous = run(&["diagnose", "--otlp", &path]);
    assert_eq!(code(&ambiguous), 3);
    let dir = tempfile::tempdir().unwrap();
    let written = dir.path().join("report.json");
    let output = run(&[
        "diagnose",
        "--otlp",
        &path,
        "--trace-id",
        "4bf92f3577b34da6a3ce929d0e0e4736",
        "--write-report",
        written.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("alloy.gateway.unattributed_interval"));
    // The written report is itself a valid input.
    let again = run(&["diagnose", "--input", written.to_str().unwrap()]);
    assert_eq!(code(&again), 0, "{}", stderr(&again));
}

fn write(dir: &Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn check_validates_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let valid = write(
        dir.path(),
        "ok.toml",
        "[server]\nbind = \"127.0.0.1:8080\"\n",
    );
    let output = run(&["check", "--config", &valid]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stdout(&output).contains("configuration is valid"));
    assert!(stdout(&output).contains("live probes"));

    let exposed = write(
        dir.path(),
        "exposed.toml",
        "[management]\nbind = \"0.0.0.0:9090\"\n",
    );
    let output = run(&["check", "--config", &exposed]);
    assert_eq!(code(&output), 3);
    assert!(stdout(&output).contains("management.token"));

    let typo = write(dir.path(), "typo.toml", "[server]\nbnd = 1\n");
    assert_eq!(code(&run(&["check", "--config", &typo])), 3);

    let otlp = write(dir.path(), "otlp.toml", "[otlp]\nenabled = true\n");
    assert_eq!(
        code(&run(&["check", "--config", &otlp])),
        0,
        "all features assumed by default"
    );
    assert_eq!(
        code(&run(&["check", "--config", &otlp, "--features", "tls"])),
        3
    );
}

#[test]
fn check_never_prints_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "secret.toml",
        "[management]\ntoken = \"0123456789abcdef0123456789abcdef-secret\"\n[database]\nurl = \"postgres://u:hunter2@db/x\"\n",
    );
    let output = run(&["check", "--config", &path, "--show-effective"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        !text.contains("hunter2") && !text.contains("-secret"),
        "{text}"
    );
    assert!(text.contains("<redacted>"));
    let output = run(&[
        "check",
        "--config",
        &path,
        "--show-effective",
        "--format",
        "json",
    ]);
    assert!(!stdout(&output).contains("hunter2"));
}

#[test]
fn check_never_prints_secrets_from_malformed_files() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text, secret) in [
        (
            "token.toml",
            "[management]\ntoken = \"SYNTHETIC-AUDIT-TOKEN-0123456789ABCDEF\" extra\n",
            "SYNTHETIC-AUDIT-TOKEN",
        ),
        (
            "database.toml",
            "[database]\nurl = \"postgres://u:SYNTHETIC-PASSWORD@db/x\" extra\n",
            "SYNTHETIC-PASSWORD",
        ),
        (
            "typed.toml",
            "[database]\nmax_connections = \"postgres://u:SYNTHETIC-TYPED@db/x\"\n",
            "SYNTHETIC-TYPED",
        ),
    ] {
        let path = write(dir.path(), name, text);
        for fmt in ["human", "json"] {
            let output = run(&["check", "--config", &path, "--format", fmt]);
            assert_eq!(code(&output), 3, "{name} {fmt}");
            let printed = format!("{}{}", stdout(&output), stderr(&output));
            assert!(!printed.contains(secret), "{name} {fmt}: {printed}");
        }
    }

    // Syntax errors keep the file and a useful location in both formats.
    let path = write(dir.path(), "bad.toml", "[server]\nbind = \"x\" extra\n");
    let message = stderr(&run(&["check", "--config", &path]));
    assert!(message.contains("bad.toml"), "{message}");
    assert!(message.contains("at line 2, column "), "{message}");
    let json = run(&["check", "--config", &path, "--format", "json"]);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["valid"], false);
    assert!(value["errors"][0].as_str().unwrap().contains("bad.toml"));
    assert_eq!(value["location"]["line"], 2);
    let column = value["location"]["column"].as_u64().unwrap();
    assert!(column > 1, "{column}");
}

#[test]
fn edge_export_writes_reviewable_artifacts_without_overwriting() {
    let manifest = fixture("manifests/orders-api.toml");
    let output = run(&["edge", "export", "--manifest", &manifest]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        std::fs::read_to_string(fixture("manifests/orders-api.edge.yaml")).unwrap()
    );

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("edge.yaml");
    let out_str = out.to_str().unwrap();
    assert_eq!(
        code(&run(&[
            "edge",
            "export",
            "--manifest",
            &manifest,
            "--output",
            out_str
        ])),
        0
    );
    assert_eq!(
        code(&run(&[
            "edge",
            "export",
            "--manifest",
            &manifest,
            "--output",
            out_str
        ])),
        3,
        "no silent overwrite"
    );
    assert_eq!(
        code(&run(&[
            "edge",
            "export",
            "--manifest",
            &manifest,
            "--output",
            out_str,
            "--force"
        ])),
        0
    );

    let tree = dir.path().join("gitops");
    let output = run(&[
        "edge",
        "export",
        "--manifest",
        &manifest,
        "--format",
        "gitforgeops",
        "--output",
        tree.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(
        tree.join("resources/ferrum/proxies/orders-api.yaml")
            .is_file()
    );
    let again = run(&[
        "edge",
        "export",
        "--manifest",
        &manifest,
        "--format",
        "gitforgeops",
        "--output",
        tree.to_str().unwrap(),
    ]);
    assert_eq!(code(&again), 3, "non-empty output directory");

    let bad = write(
        dir.path(),
        "bad.toml",
        "schema = \"ferrum.service_manifest\"\n",
    );
    assert_eq!(code(&run(&["edge", "export", "--manifest", &bad])), 3);
}

#[test]
fn openapi_export_sets_the_gateway_path_and_detects_drift() {
    let dir = tempfile::tempdir().unwrap();
    let input = write(
        dir.path(),
        "raw.json",
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{"/items":{}}}"#,
    );
    let out = dir.path().join("openapi.json");
    let out_str = out.to_str().unwrap();
    let manifest = fixture("manifests/orders-api.toml");
    let output = run(&[
        "openapi",
        "export",
        "--input",
        &input,
        "--manifest",
        &manifest,
        "--output",
        out_str,
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(written["servers"][0]["url"], "/orders");

    let check = run(&[
        "openapi",
        "export",
        "--input",
        &input,
        "--manifest",
        &manifest,
        "--output",
        out_str,
        "--check",
    ]);
    assert_eq!(code(&check), 0, "{}", stderr(&check));
    let changed = write(
        dir.path(),
        "changed.json",
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{"/items":{},"/new":{}}}"#,
    );
    let drift = run(&[
        "openapi",
        "export",
        "--input",
        &changed,
        "--manifest",
        &manifest,
        "--output",
        out_str,
        "--check",
    ]);
    assert_eq!(code(&drift), 4);

    let swagger = write(
        dir.path(),
        "swagger.json",
        r#"{"swagger":"2.0","info":{},"paths":{}}"#,
    );
    assert_eq!(
        code(&run(&[
            "openapi", "export", "--input", &swagger, "--output", out_str
        ])),
        3
    );
}

#[test]
fn new_rejects_unsafe_names_and_targets() {
    for name in [
        "Bad",
        "1abc",
        "a--b",
        "trailing-",
        "fn",
        "std",
        "ferrum-alloy",
        "has space",
        "../escape",
        "",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = bin()
            .current_dir(dir.path())
            .args(["new", name])
            .output()
            .unwrap();
        assert_eq!(code(&output), 3, "name {name:?}: {}", stderr(&output));
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.txt"), "user file").unwrap();
    let output = run(&["new", "svc", "--path", dir.path().to_str().unwrap()]);
    assert_eq!(code(&output), 3);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        "user file"
    );

    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let output = run(&["new", "svc", "--path", link.to_str().unwrap()]);
        assert_eq!(code(&output), 3, "symlink targets are refused");
        assert!(std::fs::read_dir(&real).unwrap().next().is_none());
    }
}

fn generate(dir: &Path, name: &str, with: &[&str]) -> PathBuf {
    let target = dir.join(name);
    let alloy = repo().join("crates/ferrum-alloy");
    let mut args = vec![
        "new".to_owned(),
        name.to_owned(),
        "--path".to_owned(),
        target.to_string_lossy().into_owned(),
        "--alloy-path".to_owned(),
        alloy.to_string_lossy().into_owned(),
    ];
    if !with.is_empty() {
        args.push("--with".into());
        args.push(with.join(","));
    }
    let output = bin().args(&args).output().unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    target
}

#[test]
fn new_generates_a_complete_project() {
    let dir = tempfile::tempdir().unwrap();
    let target = generate(dir.path(), "orders-api", &["openapi", "edge"]);
    for file in [
        "Cargo.toml",
        "src/main.rs",
        "src/lib.rs",
        "src/bin/openapi.rs",
        "tests/api.rs",
        "tests/openapi.rs",
        "alloy.toml",
        "ferrum-service.toml",
        "README.md",
        ".gitignore",
        ".github/workflows/ci.yml",
    ] {
        let text =
            std::fs::read_to_string(target.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(!text.contains("{{"), "{file} has an unreplaced placeholder");
    }
    let cargo = std::fs::read_to_string(target.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("features = [\"openapi\", \"edge\"]")
            || cargo.contains("features = [\"edge\", \"openapi\"]")
            || cargo.contains("\"openapi\""),
        "{cargo}"
    );
    assert!(
        std::fs::read_to_string(target.join("src/main.rs"))
            .unwrap()
            .contains(".openapi(&orders_api::openapi())")
    );
    // The generated config and manifest pass the tool's own validation.
    let check = run(&[
        "check",
        "--config",
        target.join("alloy.toml").to_str().unwrap(),
        "--no-env",
    ]);
    assert_eq!(code(&check), 0, "{}", stdout(&check));
    let export = run(&[
        "edge",
        "export",
        "--manifest",
        target.join("ferrum-service.toml").to_str().unwrap(),
    ]);
    assert_eq!(code(&export), 0, "{}", stderr(&export));
}

/// Compiles and tests generated projects. Slow; CI runs it with `--ignored`.
#[test]
#[ignore = "builds generated projects with cargo; run with --ignored"]
fn generated_projects_build_and_pass_their_tests() {
    let dir = tempfile::tempdir().unwrap();
    for (name, with) in [("plain-api", vec![]), ("documented-api", vec!["openapi"])] {
        let target = generate(dir.path(), name, &with);
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(&cargo)
            .args(["test", "--quiet"])
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", dir.path().join("target"))
            .status()
            .unwrap();
        assert!(status.success(), "{name}: cargo test failed");
        // The generated project's own CI runs clippy with -D warnings.
        let status = Command::new(&cargo)
            .args(["clippy", "--quiet", "--all-targets", "--", "-D", "warnings"])
            .current_dir(&target)
            .env("CARGO_TARGET_DIR", dir.path().join("target"))
            .status()
            .unwrap();
        assert!(status.success(), "{name}: cargo clippy -D warnings failed");
        let status = Command::new(&cargo)
            .args(["fmt", "--all", "--", "--check"])
            .current_dir(&target)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "{name}: generated code is not rustfmt-clean"
        );
        if with.contains(&"openapi") {
            let output = bin()
                .current_dir(&target)
                .env("CARGO_TARGET_DIR", dir.path().join("target"))
                .args(["openapi", "export", "--manifest", "ferrum-service.toml"])
                .output()
                .unwrap();
            assert_eq!(code(&output), 0, "{}", stderr(&output));
            let document: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(target.join("openapi.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(document["servers"][0]["url"], "/documented-api");
            assert!(document["paths"]["/items/{id}"].is_object());
            let check = bin()
                .current_dir(&target)
                .env("CARGO_TARGET_DIR", dir.path().join("target"))
                .args([
                    "openapi",
                    "export",
                    "--manifest",
                    "ferrum-service.toml",
                    "--check",
                ])
                .output()
                .unwrap();
            assert_eq!(code(&check), 0, "{}", stderr(&check));
        }
    }
}

#[test]
fn generated_code_is_rustfmt_clean_for_short_and_long_names() {
    let dir = tempfile::tempdir().unwrap();
    for (name, with) in [
        ("ab", vec![]),
        ("ab-documented", vec!["openapi"]),
        (
            "a-rather-long-service-name-that-changes-line-widths",
            vec![],
        ),
        (
            "a-rather-long-service-name-that-changes-line-widths-oa",
            vec!["openapi"],
        ),
    ] {
        let target = generate(dir.path(), name, &with);
        let mut files = Vec::new();
        for sub in ["src", "src/bin", "tests"] {
            if let Ok(entries) = std::fs::read_dir(target.join(sub)) {
                for entry in entries {
                    let path = entry.unwrap().path();
                    if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                        files.push(path);
                    }
                }
            }
        }
        let output = Command::new("rustfmt")
            .args(["--edition", "2024", "--check"])
            .args(&files)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
