//! Hosted HTTP fixtures for the service `diagnose --url` retrieval path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

#[path = "support/trickle.rs"]
mod trickle;

const TOKEN_ENV: &str = "FERRUM_ALLOY_DIAGNOSTICS_TOKEN";
const TOKEN: &str = "private-service-lookup-credential";

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrum-alloy"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FERRUM_ALLOY_") {
            command.env_remove(name);
        }
    }
    command.env(TOKEN_ENV, TOKEN);
    command
}

fn report() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("contracts/fixtures/reports/db-operation-dominates.json");
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

fn diagnose(url: &str, timeout_ms: &str) -> Output {
    bin()
        .args([
            "diagnose",
            "--url",
            url,
            "--request-id",
            "req-1",
            "--timeout-ms",
            timeout_ms,
            "--format",
            "json",
        ])
        .output()
        .unwrap()
}

/// Serve a report and record the request.
fn serve(body: String) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("service request did not arrive: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            request.push(byte[0]);
            assert!(request.len() < 32 * 1024);
        }
        assert!(request.ends_with(b"\r\n\r\n"));
        let response_body_bytes = body.len();
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {response_body_bytes}\r\nconnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).unwrap();
        if let Err(error) = stream.write_all(body.as_bytes()) {
            trickle::assert_client_disconnect(&error, "body write");
        }
        String::from_utf8(request).unwrap()
    });
    (url, server)
}

fn assert_private(output: &Output) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains(TOKEN), "{text}");
}

#[test]
fn an_ordinary_service_report_is_fetched_successfully() {
    let body = report();
    let (url, server) = serve(body);
    let output = diagnose(&url, "1000");
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("alloy.service.operation_dominates"));
    assert!(request.starts_with("GET /diagnostics/v1/requests/req-1 HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}"))
    );
    assert_private(&output);
}

/// A hostile or wrong service receives the credential and can reflect it
/// into the report; no output may repeat it.
#[test]
fn a_reflected_service_credential_is_redacted_from_output() {
    let body = report().replace("orders-1", TOKEN);
    let (url, server) = serve(body);
    let output = diagnose(&url, "1000");
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_private(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("[REDACTED]"));
}

/// A `--token-file` credential never appears in the environment, so the
/// redactor must learn it from the resolved credential itself — for the
/// terminal and for `--write-report`, whose purpose is sharing evidence.
#[test]
fn a_reflected_token_file_credential_is_redacted_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let saved = dir.path().join("report.json");
    let body = report().replace("orders-1", TOKEN);
    let (url, server) = serve(body);
    let output = bin()
        .env_remove(TOKEN_ENV)
        .args([
            "diagnose",
            "--url",
            &url,
            "--request-id",
            "req-1",
            "--token-file",
            token_file.to_str().unwrap(),
            "--write-report",
            saved.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_private(&output);
    let written = std::fs::read_to_string(&saved).unwrap();
    assert!(!written.contains(TOKEN), "{written}");
    assert!(written.contains("[REDACTED]"), "{written}");
}

/// Parser bounds must never turn a complete reflected token into a
/// fragment that exact credential redaction cannot recognize.
#[test]
fn bounded_warnings_do_not_disclose_reflected_credential_fragments() {
    let token = "Q7w8E9r0T1y2U3i4O5p6A7s8D9f0G1h2";
    for case in [
        "long-key",
        "unicode-key",
        "short-keys",
        "name",
        "verification",
        "availability",
        "short-key",
    ] {
        let mut value: serde_json::Value = serde_json::from_str(&report()).unwrap();
        let long_value = format!("{}{token}{}", "n".repeat(450), "z".repeat(100));
        match case {
            "long-key" => value[format!("x{token}{}", "z".repeat(32))] = 0.into(),
            "unicode-key" => value[format!("😀{token}{}", "z".repeat(32))] = 0.into(),
            "short-keys" => {
                for index in 0..8 {
                    value[format!("{index}{}{token}", "x".repeat(31))] = 0.into();
                }
            }
            "name" => value["observations"][0]["name"] = long_value.into(),
            "verification" => value["collection"]["verification"] = long_value.into(),
            "availability" => value["observations"][0]["availability"] = long_value.into(),
            "short-key" => value[format!("x{token}")] = 0.into(),
            _ => unreachable!(),
        }
        value["x-safe"] = 1.into();
        for format in ["human", "json"] {
            for live in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let input = dir.path().join("input.json");
                let token_file = dir.path().join("token");
                let saved = dir.path().join("saved.json");
                std::fs::write(&input, value.to_string()).unwrap();
                std::fs::write(&token_file, format!("{token}\n")).unwrap();
                let mut command = bin();
                command.args(["diagnose", "--format", format, "--write-report"]);
                command.arg(&saved);
                let server = if live {
                    let (url, server) = serve(value.to_string());
                    command.env_remove(TOKEN_ENV).args([
                        "--url",
                        &url,
                        "--request-id",
                        "req-1",
                        "--token-file",
                    ]);
                    command.arg(&token_file);
                    Some(server)
                } else {
                    command.env(TOKEN_ENV, token).arg("--input").arg(&input);
                    None
                };
                let output = command.output().unwrap();
                if let Some(server) = server {
                    let request = server.join().unwrap();
                    assert!(request.to_ascii_lowercase().contains(&format!(
                        "authorization: bearer {}",
                        token.to_ascii_lowercase()
                    )));
                }
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    output.status.success(),
                    "{case}/{format}/live={live}: {text}"
                );
                assert!(
                    !text.contains(&token[..8]),
                    "{case}/{format}/live={live}: {text}"
                );
                let written = std::fs::read_to_string(saved).unwrap();
                assert!(!written.contains(&token[..8]), "{case}: {written}");
                assert!(written.contains("x-safe"));
                if case == "short-key" {
                    assert!(text.contains("[REDACTED]"), "{text}");
                    assert!(text.contains("x-safe"), "{text}");
                }
            }
        }
    }
}

#[test]
fn a_trickling_service_body_cannot_extend_the_whole_operation_deadline() {
    let proof = trickle::prove_trickle_deadline(&report(), diagnose);
    assert!(
        proof
            .request
            .starts_with("GET /diagnostics/v1/requests/req-1 HTTP/1.1\r\n")
    );
    assert!(
        proof
            .request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}"))
    );
    let output = proof.output;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("reading the report failed"), "{error}");
    assert!(!error.contains(&proof.url), "{error}");
    assert_private(&output);
}
