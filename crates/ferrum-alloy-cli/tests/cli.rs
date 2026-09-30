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
    assert_eq!(json["edge_contract"]["release"], "v0.9.8");
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
fn diagnose_sanitizes_errors_from_hostile_report_keys_in_both_formats() {
    let dir = tempfile::tempdir().unwrap();
    let key = "\\u001b[2J";
    let report = format!(
        "{{\"schema\":\"ferrum.diagnostic_report\",\"schema_version\":\"1.0\",\"{key}\":\"{}\"}}",
        "x".repeat(2_049)
    );
    let path = write(dir.path(), "hostile-report.json", &report);

    for format in ["human", "json"] {
        let output = run(&["diagnose", "--input", &path, "--format", format]);
        assert_eq!(code(&output), 3);
        let error = stderr(&output);
        assert!(!error.contains('\u{1b}'));
        assert!(error.contains("?[2J"));
    }
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

#[test]
fn diagnose_writes_reports_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture("otlp/edge-alloy-trace.jsonl");
    let target = dir.path().join("report.json");
    let args = [
        "diagnose",
        "--otlp",
        &path,
        "--trace-id",
        "4bf92f3577b34da6a3ce929d0e0e4736",
        "--write-report",
        target.to_str().unwrap(),
    ];
    let entries = || -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    };
    // A directory cannot be replaced by the report: the rename fails and
    // the temporary file is removed.
    std::fs::create_dir(&target).unwrap();
    let output = run(&args);
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(target.is_dir());
    assert_eq!(entries(), vec![std::ffi::OsString::from("report.json")]);

    std::fs::remove_dir(&target).unwrap();
    let output = run(&args);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(target.is_file());
    assert_eq!(entries(), vec![std::ffi::OsString::from("report.json")]);
}

/// One trace of `count` Alloy SERVER spans, five observations each.
fn many_server_spans(count: u64) -> String {
    let attributes: Vec<serde_json::Value> = [
        ("alloy.server.time_to_headers_ms", 100.0),
        ("alloy.server.body_duration_ms", 100.0),
        ("alloy.server.duration_ms", 200.0),
        ("alloy.admission.wait_ms", 0.0),
    ]
    .iter()
    .map(|(key, value)| serde_json::json!({ "key": key, "value": { "doubleValue": value } }))
    .collect();
    let spans: Vec<serde_json::Value> = (1..=count)
        .map(|i| {
            let start = 1_790_000_000_000_000_000u64 + i * 1_000_000;
            serde_json::json!({
                "traceId": "3c4d5e6f708192a3b4c5d6e7f8091a2b",
                "spanId": format!("{i:016x}"),
                "kind": 2,
                "startTimeUnixNano": start.to_string(),
                "endTimeUnixNano": (start + 200_000_000).to_string(),
                "attributes": attributes,
            })
        })
        .collect();
    serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [{
                    "key": "service.name",
                    "value": { "stringValue": "orders-api" },
                }],
            },
            "scopeSpans": [{ "scope": { "name": "ferrum-alloy-telemetry" }, "spans": spans }],
        }],
    })
    .to_string()
}

#[test]
fn diagnose_refuses_otlp_reports_that_input_could_not_read_back() {
    // Within the OTLP input bounds, but 5,005 observations are more than a
    // report may hold.
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "traces.jsonl", &many_server_spans(1_001));
    let written = dir.path().join("report.json");
    let output = run(&[
        "diagnose",
        "--otlp",
        &path,
        "--write-report",
        written.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 3, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("more than 5000 observations"),
        "{}",
        stderr(&output)
    );
    assert!(!written.exists(), "no report is written");
}

/// Devices and FIFOs report a length of `0`; reading one would never end.
#[cfg(unix)]
#[test]
fn diagnose_refuses_special_files_with_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("report.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    for path in ["/dev/zero", fifo.to_str().unwrap()] {
        for flag in ["--input", "--otlp"] {
            let output = run(&["diagnose", flag, path]);
            assert_eq!(code(&output), 3, "{flag} {path}: {}", stderr(&output));
            assert!(stderr(&output).contains("is not a regular file"));
        }
        let args = [
            "diagnose",
            "--url",
            "http://127.0.0.1:9/",
            "--request-id",
            "req-7",
            "--token-file",
            path,
        ];
        let output = bin().env_remove(TOKEN_ENV).args(args).output().unwrap();
        assert_eq!(code(&output), 3, "token {path}: {}", stderr(&output));
        assert!(stderr(&output).contains("is not a regular file"));
    }
}

/// The credential variable of `diagnose --url`.
const TOKEN_ENV: &str = "FERRUM_ALLOY_DIAGNOSTICS_TOKEN";

/// A live report as a service serves it, with a forged `verified` claim
/// and a request id carrying a terminal escape sequence and a bidirectional
/// override.
fn live_report() -> String {
    serde_json::json!({
        "schema": "ferrum.diagnostic_report",
        "schema_version": "1.0",
        "collection": {
            "collector": { "kind": "alloy", "name": "ferrum-alloy" },
            "method": "live_export",
            "verification": "verified"
        },
        "subject": { "request_id": "req-7\u{1b}[2J\u{202E}", "service": "orders" },
        "observations": [{
            "id": "alloy:00f067aa0ba902b7:time_to_headers",
            "producer": { "kind": "alloy", "name": "ferrum-alloy-telemetry" },
            "kind": "measurement",
            "name": "alloy.server.time_to_headers",
            "availability": "measured",
            "value": 12.5,
            "unit": "ms",
            "clock": "monotonic_local",
            "scope": { "leg": "service", "service": "orders" },
            "span": {
                "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
                "span_id": "00f067aa0ba902b7"
            },
            "trust": "verified"
        }]
    })
    .to_string()
}

/// Answers one HTTP/1.1 request on a loopback port with `status` and
/// `body`. The handle yields the request head it received, and fails when
/// no request arrives within 20 seconds.
fn serve_once(status: &'static str, body: String) -> (u16, std::thread::JoinHandle<String>) {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::time::{Duration, Instant};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("no request arrived: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            head.push(byte[0]);
        }
        let length = body.len();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{body}"
        );
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&head).into_owned()
    });
    (port, handle)
}

#[test]
fn diagnose_url_fetches_a_live_report_with_the_credential_from_the_environment() {
    let (port, server) = serve_once("200 OK", live_report());
    let url = format!("http://127.0.0.1:{port}");
    let args = [
        "diagnose",
        "--url",
        &url,
        "--request-id",
        "req-7",
        "--format",
        "json",
    ];
    let output = bin()
        .env(TOKEN_ENV, "live-secret-credential")
        .args(args)
        .output()
        .unwrap();
    let head = server.join().unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let expected = "GET /diagnostics/v1/requests/req-7 HTTP/1.1";
    assert_eq!(head.lines().next(), Some(expected));
    let head = head.to_ascii_lowercase();
    assert!(head.contains("authorization: bearer live-secret-credential"));

    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(!text.contains("live-secret-credential"), "{text}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["claimed_verification"], "verified");
    let collection = &value["report"]["collection"];
    assert_eq!(collection["verification"], "unverified");
    assert_eq!(collection["method"], "live_export");
    for finding in value["report"]["findings"].as_array().into_iter().flatten() {
        assert_ne!(finding["confidence"], "confirmed", "{finding}");
    }
}

#[test]
fn diagnose_url_reads_a_token_file_and_keeps_escapes_off_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let token = write(dir.path(), "token", "file-secret-credential\n");
    let (port, server) = serve_once("200 OK", live_report());
    let url = format!("http://127.0.0.1:{port}/");
    let args = [
        "diagnose",
        "--url",
        &url,
        "--request-id",
        "req-7",
        "--token-file",
        &token,
    ];
    let output = bin().env_remove(TOKEN_ENV).args(args).output().unwrap();
    let head = server.join().unwrap().to_ascii_lowercase();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(head.contains("authorization: bearer file-secret-credential"));
    let text = stdout(&output);
    assert!(text.contains("provenance is unverified"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(!text.contains('\u{202E}'), "{text:?}");
    assert!(!text.contains("file-secret-credential"));
}

#[test]
fn diagnose_url_reports_a_refusal_without_details() {
    let problem = r#"{"title":"Not found","status":404}"#;
    let (port, server) = serve_once("404 Not Found", problem.to_owned());
    let url = format!("http://127.0.0.1:{port}");
    let args = ["diagnose", "--url", &url, "--request-id", "req-7"];
    let output = bin()
        .env(TOKEN_ENV, "a-credential")
        .args(args)
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(code(&output), 1);
    let error = stderr(&output);
    assert!(error.contains("no report"), "{error}");
}

#[test]
fn diagnose_url_never_takes_a_credential_from_arguments() {
    let url = "http://127.0.0.1:9";
    for flag in [["--token", "argv-secret"], ["--bearer", "argv-secret"]] {
        let mut args = vec!["diagnose", "--url", url, "--request-id", "req-7"];
        args.extend(flag);
        let output = bin().env_remove(TOKEN_ENV).args(&args).output().unwrap();
        assert_eq!(code(&output), 2, "{}", stderr(&output));
    }
    let output = run(&[
        "diagnose",
        "--url",
        url,
        "--request-id",
        "req-7",
        "--token=argv-secret",
    ]);
    assert_eq!(code(&output), 2);
    // Credentials in the URL are refused, and never repeated.
    let output = run(&[
        "diagnose",
        "--url",
        "http://user:argv-secret@127.0.0.1:9",
        "--request-id",
        "req-7",
    ]);
    assert_eq!(code(&output), 3);
    let error = stderr(&output);
    assert!(!error.contains("argv-secret"), "{error}");
}

#[test]
fn diagnose_url_sends_credentials_over_plain_http_only_to_loopback() {
    let args = [
        "diagnose",
        "--url",
        "http://192.0.2.1:9090",
        "--request-id",
        "req-7",
    ];
    let output = bin()
        .env(TOKEN_ENV, "a-credential")
        .args(args)
        .output()
        .unwrap();
    assert_eq!(code(&output), 3);
    let error = stderr(&output);
    assert!(error.contains("loopback"), "{error}");
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
fn manifest_errors_are_terminal_safe_for_both_manifest_consumers() {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = std::fs::read_to_string(fixture("manifests/orders-api.toml")).unwrap();
    manifest.push_str("\n\"\\u001b[2J\" = \"value\"\n");
    let manifest_path = write(dir.path(), "hostile-manifest.toml", &manifest);
    let edge = run(&["edge", "export", "--manifest", &manifest_path]);
    let openapi_input = write(
        dir.path(),
        "openapi.json",
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{}}"#,
    );
    let output_path = dir.path().join("out.json");
    let openapi = run(&[
        "openapi",
        "export",
        "--input",
        &openapi_input,
        "--manifest",
        &manifest_path,
        "--output",
        output_path.to_str().unwrap(),
    ]);

    for result in [edge, openapi] {
        assert_eq!(code(&result), 3);
        let output = stderr(&result);
        assert!(!output.contains('\u{1b}'));
        assert!(output.contains("?[2J"));
    }
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
fn openapi_export_maps_service_base_path_when_the_gateway_strips_it() {
    let dir = tempfile::tempdir().unwrap();
    let input = write(
        dir.path(),
        "raw.json",
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{"/internal/items":{},"/internal":{},"/items-already-public":{},"/internalized":{}}}"#,
    );

    for (name, service_base_path, strip_public_path, expected_paths) in [
        (
            "stripped",
            "/internal/",
            true,
            vec!["/", "/items", "/items-already-public", "/internalized"],
        ),
        (
            "not-stripped",
            "/internal/",
            false,
            vec![
                "/internal",
                "/internal/items",
                "/items-already-public",
                "/internalized",
            ],
        ),
        (
            "root-base",
            "/",
            true,
            vec![
                "/internal",
                "/internal/items",
                "/items-already-public",
                "/internalized",
            ],
        ),
    ] {
        let manifest = write(
            dir.path(),
            &format!("{name}.toml"),
            &format!(
                "schema = \"ferrum.service_manifest\"\nschema_version = \"1.0\"\n[service]\nname = \"review-api\"\n[api]\npublic_path = \"/public\"\nservice_base_path = \"{service_base_path}\"\nstrip_public_path = {strip_public_path}\n[upstream]\nhost = \"127.0.0.1\"\nport = 8080\nscheme = \"http\"\n"
            ),
        );
        let out = dir.path().join(format!("{name}.json"));
        let output = run(&[
            "openapi",
            "export",
            "--input",
            &input,
            "--manifest",
            &manifest,
            "--output",
            out.to_str().unwrap(),
        ]);
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out).unwrap()).unwrap();
        assert_eq!(written["servers"][0]["url"], "/public");
        let paths = written["paths"].as_object().unwrap();
        let mut actual_paths: Vec<_> = paths.keys().map(String::as_str).collect();
        actual_paths.sort_unstable();
        let mut expected_paths = expected_paths;
        expected_paths.sort_unstable();
        assert_eq!(actual_paths, expected_paths, "{name}");
    }
}

/// `openapi export --check` reads the existing output only up to the
/// document limit; a larger file is drift.
#[test]
fn openapi_check_refuses_oversized_outputs_with_exit_4() {
    let dir = tempfile::tempdir().unwrap();
    let input = write(
        dir.path(),
        "raw.json",
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{}}"#,
    );
    let oversized = dir.path().join("openapi.json");
    std::fs::write(&oversized, vec![b' '; 16 * 1024 * 1024 + 1]).unwrap();
    let output = run(&[
        "openapi",
        "export",
        "--input",
        &input,
        "--output",
        oversized.to_str().unwrap(),
        "--check",
    ]);
    assert_eq!(code(&output), 4, "{}", stderr(&output));
    assert!(stderr(&output).contains("is larger than 16777216 bytes"));
}

/// Devices and FIFOs report a length of `0`; reading one would never end.
#[cfg(unix)]
#[test]
fn openapi_check_refuses_special_outputs_with_exit_4() {
    let dir = tempfile::tempdir().unwrap();
    let input = write(
        dir.path(),
        "raw.json",
        r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{}}"#,
    );
    let fifo = dir.path().join("openapi.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    for output_path in [PathBuf::from("/dev/zero"), fifo] {
        let output = run(&[
            "openapi",
            "export",
            "--input",
            &input,
            "--output",
            output_path.to_str().unwrap(),
            "--check",
        ]);
        assert_eq!(code(&output), 4, "{}", stderr(&output));
        assert!(stderr(&output).contains("is not a regular file"));
    }
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
fn new_alloy_rev_resolves_branches_tags_and_commit_ids() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("alloy-fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&fixture)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let initialized = Command::new("git")
        .args(["init", "--quiet", "--initial-branch=main"])
        .arg(&fixture)
        .status()
        .unwrap();
    assert!(initialized.success());
    git(&["config", "user.name", "Alloy CLI test"]);
    git(&["config", "user.email", "alloy-cli-test@example.invalid"]);
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"ferrum-alloy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(fixture.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "fixture"]);
    git(&["tag", "v0.1.0"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let abbreviated = &commit[..7];
    let selectors = [
        ("--alloy-branch", "main", "branch"),
        ("--alloy-tag", "v0.1.0", "tag"),
        ("--alloy-rev", commit.as_str(), "rev"),
        ("--alloy-rev", abbreviated, "rev"),
    ];
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // A file URL with forward slashes: a Windows path's backslashes would be read as
    // TOML escapes inside the generated dependency string.
    let fixture_path = fixture.to_string_lossy().replace('\\', "/");
    let fixture_url = if fixture_path.starts_with('/') {
        format!("file://{fixture_path}")
    } else {
        format!("file:///{fixture_path}")
    };
    let default_target = dir.path().join("generated-default");
    let default_output = bin()
        .args([
            "new",
            "fixture-default",
            "--path",
            default_target.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(code(&default_output), 0, "{}", stderr(&default_output));
    let default_dependency = read(&default_target, "Cargo.toml");
    let default_dependency = default_dependency
        .lines()
        .find(|line| line.starts_with("ferrum-alloy = "))
        .unwrap();
    assert!(default_dependency.contains("branch = \"main\""));

    for (index, (option, value, selector)) in selectors.iter().enumerate() {
        let target = dir.path().join(format!("generated-{index}"));
        let output = bin()
            .args([
                "new",
                &format!("fixture-{index}"),
                "--path",
                target.to_str().unwrap(),
                option,
                value,
            ])
            .output()
            .unwrap();
        assert_eq!(code(&output), 0, "{}", stderr(&output));

        let generated = read(&target, "Cargo.toml");
        let dependency = generated
            .lines()
            .find(|line| line.starts_with("ferrum-alloy = "))
            .unwrap();
        assert!(
            dependency.contains(&format!("{selector} = ")),
            "{dependency}"
        );
        assert_eq!(
            ["branch", "tag", "rev"]
                .iter()
                .filter(|key| dependency.contains(&format!("{key} = ")))
                .count(),
            1,
            "{dependency}"
        );
        let dependency =
            dependency.replace("https://github.com/ferrum-edge/ferrum-alloy", &fixture_url);
        let manifest = format!(
            "[package]\nname = \"fixture-{index}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependency}\n"
        );
        std::fs::write(target.join("Cargo.toml"), manifest).unwrap();

        let resolved = Command::new(&cargo)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--manifest-path",
                target.join("Cargo.toml").to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            resolved.status.success(),
            "{selector} {value:?} did not resolve: {}",
            String::from_utf8_lossy(&resolved.stderr)
        );
    }
}

#[test]
fn new_alloy_rev_rejects_named_refs_and_conflicting_selectors() {
    let named_ref = run(&["new", "fixture", "--alloy-rev", "main"]);
    assert_eq!(code(&named_ref), 3, "{}", stderr(&named_ref));
    assert!(stderr(&named_ref).contains("--alloy-branch or --alloy-tag"));

    let conflicting = run(&[
        "new",
        "fixture",
        "--alloy-branch",
        "main",
        "--alloy-tag",
        "v0.1.0",
    ]);
    assert_eq!(code(&conflicting), 2, "{}", stderr(&conflicting));
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

fn read(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

#[test]
fn new_generates_database_auth_and_client_starters() {
    let dir = tempfile::tempdir().unwrap();
    let with = ["openapi", "postgres", "jwt", "http-client"];
    let target = generate(dir.path(), "notes-api", &with);
    for file in [
        "src/db.rs",
        "src/auth.rs",
        "src/upstream.rs",
        "tests/db.rs",
        "tests/auth.rs",
        "tests/upstream.rs",
        "migrations/0001_create_notes.sql",
    ] {
        assert!(!read(&target, file).contains("{{"), "{file}");
    }
    let cargo = read(&target, "Cargo.toml");
    let features = r#"features = ["openapi", "postgres", "jwt", "http-client"]"#;
    assert!(cargo.contains(features), "{cargo}");
    assert!(cargo.contains(r#"default-run = "notes-api""#), "{cargo}");
    let lib = read(&target, "src/lib.rs");
    let modules = "pub mod auth;\npub mod db;\npub mod upstream;\n";
    assert!(lib.contains(modules), "{lib}");
    let main = read(&target, "src/main.rs");
    for expected in [
        "use notes_api::{auth, db, upstream};",
        "postgres::migrate(&pool, &db::MIGRATOR)",
        "Some(\"migrate\") => true,",
        "app = app.router(routes).openapi(&document);",
    ] {
        assert!(main.contains(expected), "{expected}: {main}");
    }
    let ci = read(&target, ".github/workflows/ci.yml");
    assert!(ci.contains("TEST_DATABASE_URL"), "{ci}");
    assert!(ci.contains("cargo test -- --include-ignored"), "{ci}");
    let readme = read(&target, "README.md");
    assert!(readme.contains("cargo run -- migrate"), "{readme}");
    // The generated configuration passes the tool's own validation.
    let config = target.join("alloy.toml").to_string_lossy().into_owned();
    let check = run(&["check", "--config", &config, "--no-env"]);
    assert_eq!(code(&check), 0, "{}", stdout(&check));

    // Each starter also stands alone.
    for (with, module) in [
        ("postgres", "db"),
        ("jwt", "auth"),
        ("http-client", "upstream"),
    ] {
        let target = generate(dir.path(), &format!("only-{module}"), &[with]);
        let main = read(&target, "src/main.rs");
        let import = format!("use only_{module}::{module};");
        assert!(main.contains(&import), "{main}");
        assert!(target.join(format!("src/{module}.rs")).is_file());
    }
}

/// Compiles and tests generated projects. Slow; CI runs it with `--ignored`.
#[test]
#[ignore = "builds generated projects with cargo; run with --ignored"]
fn generated_projects_build_and_pass_their_tests() {
    let dir = tempfile::tempdir().unwrap();
    for (name, with) in [
        ("plain-api", vec![]),
        ("documented-api", vec!["openapi"]),
        ("postgres-api", vec!["postgres"]),
        ("jwt-api", vec!["jwt"]),
        ("client-api", vec!["http-client"]),
        (
            "starters-api",
            vec!["openapi", "postgres", "jwt", "http-client"],
        ),
    ] {
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
        // With a database (the CI `generator` job has its own service
        // container), also run the generated tests that need one.
        let database = std::env::var_os("FERRUM_ALLOY_TEST_DATABASE_URL");
        if let Some(url) = database.filter(|_| with.contains(&"postgres")) {
            let status = Command::new(&cargo)
                .args(["test", "--quiet", "--", "--include-ignored"])
                .current_dir(&target)
                .env("CARGO_TARGET_DIR", dir.path().join("target"))
                .env("TEST_DATABASE_URL", url)
                .status()
                .unwrap();
            assert!(status.success(), "{name}: database tests failed");
        }
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
            assert_eq!(document["servers"][0]["url"], format!("/{name}"));
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
        // The JWT tests pin the algorithm they sign with, so switching
        // alloy.toml to an issuer's algorithm does not break them.
        if with.contains(&"jwt") {
            let config = read(&target, "alloy.toml");
            let config = config.replace("\"ES256\"", "\"RS256\"");
            assert!(config.contains(r#"algorithms = ["RS256"]"#), "{config}");
            std::fs::write(target.join("alloy.toml"), config).unwrap();
            let status = Command::new(&cargo)
                .args(["test", "--quiet", "--test", "auth"])
                .current_dir(&target)
                .env("CARGO_TARGET_DIR", dir.path().join("target"))
                .status()
                .unwrap();
            assert!(status.success(), "{name}: tests broke on RS256");
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
        ("ab-starters", vec!["postgres", "jwt", "http-client"]),
        ("ab-oa-db", vec!["openapi", "postgres"]),
        (
            "a-rather-long-service-name-that-changes-line-widths-st",
            vec!["openapi", "postgres", "jwt", "http-client"],
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
