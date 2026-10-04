//! Recording HTTP fixtures executed by hosted CI only. These exercise the
//! actual binary and real request/response boundaries, not a mocked client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN_ENV: &str = "FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN";
const TOKEN: &str = "private-edge-lookup-credential";
const SERVICE_TOKEN_ENV: &str = "FERRUM_ALLOY_DIAGNOSTICS_TOKEN";

fn record() -> Value {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root
        .join("contracts/ferrum-contracts/fixtures/diagnostic-ref/valid/connection-failure.json");
    let bytes = std::fs::read(path).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn capture(record: &Value) -> Value {
    json!({
        "reference": record["ref"], "namespace": record["namespace"],
        "status": record["status"], "gateway_error": record["gateway_error"],
        "protocol": record["protocol"],
        "request_started_at": "2026-09-27T10:15:00Z",
        "response_received_at": "2026-09-27T10:15:05Z",
    })
}

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

fn assert_client_disconnect(error: &std::io::Error, write: &str) {
    assert!(
        matches!(
            error.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
        ) || matches!(error.raw_os_error(), Some(10053 | 10054)),
        "unexpected {write} write error: kind={:?}, raw_os_error={:?}",
        error.kind(),
        error.raw_os_error()
    );
}

/// Retry nonblocking fixture I/O within one absolute lifetime, checking
/// cancellation even when the peer makes progress or interrupts a syscall.
fn trickle_io<T>(
    cancelled: &AtomicBool,
    deadline: Instant,
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                ErrorKind::Interrupted,
                "trickle fixture cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "trickle fixture exceeded its lifetime",
            ));
        }
        match operation() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn trickle_write(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Instant,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = trickle_io(cancelled, deadline, || stream.write(bytes))?;
        if written == 0 {
            return Err(std::io::Error::new(
                ErrorKind::WriteZero,
                "trickle fixture could not write the response",
            ));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn diagnose(url: &str, observation: &Value, extra: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.json");
    std::fs::write(&path, serde_json::to_vec(observation).unwrap()).unwrap();
    bin()
        .args([
            "diagnose",
            "--edge-admin-url",
            url,
            "--edge-observation",
            path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .args(extra)
        .output()
        .unwrap()
}

/// Records one request, with bounded accept/read waits. Response writes can
/// fail when the client correctly stops reading an oversized or slow body.
fn serve(
    status: &str,
    headers: &str,
    body: String,
    delay: Duration,
    client_may_stop_reading: bool,
) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n",
        body.len(),
    );
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("request did not arrive: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
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
        stream.write_all(head.as_bytes()).unwrap();
        std::thread::sleep(delay);
        if let Err(error) = stream.write_all(body.as_bytes()) {
            assert!(client_may_stop_reading);
            assert!(
                matches!(
                    error.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                ),
                "unexpected fixture body write error: {error}"
            );
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
fn an_authenticated_bound_record_confirms_only_the_new_record_finding() {
    let record = record();
    let observation = capture(&record);
    let (url, server) = serve("200 OK", "", record.to_string(), Duration::ZERO, false);
    let dir = tempfile::tempdir().unwrap();
    let saved = dir.path().join("report.json");
    let report_fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/reports/forged-verified-claim.json");
    let mut report_value: Value =
        serde_json::from_slice(&std::fs::read(report_fixture).unwrap()).unwrap();
    report_value["extensions"]["x-reflected-credential"] = json!(TOKEN);
    report_value["extensions"][format!("x-{TOKEN}")] = json!("discard this key");
    report_value["extensions"]["x-[REDACTED]"] = json!("keep this safe value");
    report_value["extensions"]["x-nested"] = json!([{
        "schema": "keep this safe field",
        (format!("x-{TOKEN}")): "discard this nested key",
        "x-[REDACTED]": "keep this nested safe value",
        "deeper": [{ (TOKEN): TOKEN, "safe": "keep this deepest value" }],
    }]);
    let report = dir.path().join("input.json");
    std::fs::write(&report, report_value.to_string()).unwrap();
    let output = diagnose(
        &url,
        &observation,
        &[
            "--input",
            report.to_str().unwrap(),
            "--write-report",
            saved.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = server.join().unwrap();
    let expected = format!(
        "GET /diagnostics/v1/refs/{} HTTP/1.1",
        record["ref"].as_str().unwrap()
    );
    assert_eq!(request.lines().next().unwrap(), expected);
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}"))
    );
    assert_private(&output);
    let saved_text = std::fs::read_to_string(&saved).unwrap();
    assert!(!saved_text.contains(TOKEN));
    let saved_value: Value = serde_json::from_str(&saved_text).unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    for report in [&value["report"], &saved_value] {
        assert_eq!(report["schema"], "ferrum.diagnostic_report");
        assert_eq!(report["extensions"]["x-[REDACTED]"], "keep this safe value");
        assert_eq!(
            report["extensions"]["x-nested"],
            json!([{
                "schema": "keep this safe field",
                "x-[REDACTED]": "keep this nested safe value",
                "deeper": [{ "safe": "keep this deepest value" }],
            }])
        );
    }
    assert_eq!(value["report"]["collection"]["verification"], "unverified");
    let confirmed = value["report"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["confidence"] == "confirmed")
        .collect::<Vec<_>>();
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0]["code"], "alloy.edge.bound_diagnostic_record");
    let offline = bin()
        .args([
            "diagnose",
            "--input",
            saved.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(offline.status.success());
    let value: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert!(
        value["report"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["confidence"] != "confirmed")
    );
}

#[test]
fn plaintext_lookup_bypasses_all_environment_proxies() {
    let record = record();
    let (url, server) = serve("200 OK", "", record.to_string(), Duration::ZERO, false);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.json");
    std::fs::write(&path, capture(&record).to_string()).unwrap();
    let output = bin()
        .env(SERVICE_TOKEN_ENV, "separate-service-credential")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .args([
            "diagnose",
            "--edge-admin-url",
            &url,
            "--edge-observation",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = server.join().unwrap();
    assert!(request.contains(TOKEN));
    assert!(!request.contains("separate-service-credential"));
    assert_private(&output);
}

#[test]
fn redirects_never_forward_the_credential_and_refusals_disclose_no_server_text() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let headers = format!("location: http://{}/secret\r\n", trap.local_addr().unwrap());
    for status in [
        "302 Found",
        "307 Temporary Redirect",
        "401 Unauthorized",
        "403 Forbidden",
        "404 Not Found",
        "429 Too Many Requests",
    ] {
        let (url, server) = serve(status, &headers, TOKEN.into(), Duration::ZERO, false);
        let output = diagnose(&url, &capture(&record()), &[]);
        server.join().unwrap();
        assert!(!output.status.success(), "{status}");
        assert_private(&output);
        assert_eq!(trap.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    }
}

#[test]
fn response_bounds_and_timeout_are_enforced() {
    for (body, delay, extra, expected_error) in [
        (
            format!("{TOKEN}{}", " ".repeat(64 * 1024 + 1)),
            Duration::ZERO,
            vec![],
            "Edge input exceeds 64 KiB",
        ),
        (
            record().to_string(),
            Duration::from_millis(600),
            vec!["--timeout-ms", "100"],
            "Edge record read failed or timed out",
        ),
    ] {
        let (url, server) = serve("200 OK", "", body, delay, true);
        let output = diagnose(&url, &capture(&record()), &extra);
        server.join().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_private(&output);
    }
}

#[test]
fn a_trickling_body_cannot_extend_the_whole_operation_deadline() {
    let record = record();
    let observation = capture(&record);
    let body = record.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let expected_request = format!(
        "GET /diagnostics/v1/refs/{} HTTP/1.1",
        record["ref"].as_str().unwrap()
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&cancelled);
    let server = std::thread::spawn(move || {
        let result = (|| -> std::io::Result<_> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut stream, _) = trickle_io(&stop, deadline, || listener.accept())?;
            stream.set_nonblocking(true)?;
            stream.set_nodelay(true)?;
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                let read = trickle_io(&stop, deadline, || stream.read(&mut byte))?;
                if read == 0 {
                    return Err(std::io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "trickling request ended before its headers",
                    ));
                }
                request.push(byte[0]);
                if request.len() >= 32 * 1024 {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "trickling request headers exceeded the fixture limit",
                    ));
                }
            }
            let request = String::from_utf8(request)
                .map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error))?;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len() + 40,
            );
            trickle_write(&mut stream, head.as_bytes(), &stop, deadline)?;
            let _ = started_tx.send(Instant::now());
            let mut write_times = Vec::new();
            let mut disconnect = None;
            // Every gap is below the 600 ms read timeout, but a complete response
            // takes at least four seconds. Without a total deadline it succeeds.
            for _ in 0..40 {
                if let Err(error) = trickle_write(&mut stream, b" ", &stop, deadline) {
                    disconnect = Some((error, "trickle"));
                    break;
                }
                write_times.push(Instant::now());
                std::thread::sleep(Duration::from_millis(100));
            }
            if disconnect.is_none() {
                disconnect = trickle_write(&mut stream, body.as_bytes(), &stop, deadline)
                    .err()
                    .map(|error| (error, "final body"));
            }
            Ok((request, write_times, disconnect))
        })();
        // The socket has been dropped before completion is recorded, including
        // every I/O error path. A panic closes the channel and is reported at join.
        let _ = completed_tx.send(Instant::now());
        result
    });
    // Keep ownership of the fixture outside all fallible client/proof work so
    // even a helper panic or a channel failure cannot bypass cancellation/join.
    let proof = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let output = diagnose(&url, &observation, &["--timeout-ms", "600"]);
        // Measure before cleanup; joining must not inflate the CLI return time.
        let returned_at = Instant::now();
        let completion_deadline = returned_at + Duration::from_secs(1);
        let started = started_rx.recv_timeout(Duration::from_secs(1));
        let completion_wait = completion_deadline.saturating_duration_since(Instant::now());
        let completed = completed_rx.recv_timeout(completion_wait);
        (output, returned_at, started, completed)
    }));
    cancelled.store(true, Ordering::Relaxed);
    let server_result = match server.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    let (output, returned_at, started, completed) = match proof {
        Ok(proof) => proof,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    let started = started.unwrap();
    let elapsed = returned_at.duration_since(started);
    // The request deadline starts before connect/send, so time spent reaching
    // this server has already consumed it. There is no minimum body-read time.
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    // Require bounded cleanup after the CLI returns, including an observed
    // disconnect rather than successful completion of the four-second body.
    let completed_at = completed.unwrap();
    assert!(
        completed_at.duration_since(returned_at) < Duration::from_secs(1),
        "fixture did not complete within one second of CLI return"
    );
    let (request, write_times, disconnect) = server_result.unwrap();
    assert_eq!(request.lines().next().unwrap(), expected_request);
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}"))
    );
    let (error, write) = disconnect.expect("client read the complete trickling response");
    assert_client_disconnect(&error, write);
    let writes: Vec<_> = write_times
        .into_iter()
        .filter(|written_at| *written_at <= returned_at)
        .collect();
    assert!(
        writes.len() >= 3,
        "fixture did not actively trickle before CLI returned: {}",
        writes.len()
    );
    assert!(writes.len() < 40, "client outlasted the trickle");
    assert!(
        writes
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0]) < Duration::from_millis(600)),
        "fixture stalled between body writes"
    );
    assert!(
        returned_at.duration_since(*writes.last().unwrap()) < Duration::from_millis(600),
        "fixture stalled before CLI returned"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(error, "error: Edge record read failed or timed out\n");
    assert!(!error.contains(&url));
    assert_private(&output);
}

#[test]
fn malformed_inputs_are_redacted_before_any_lookup_can_succeed() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let url = format!("http://{}", trap.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("input-{TOKEN}.json"));
    let saved = dir.path().join("report.json");
    let capture_path = dir.path().join("capture.json");
    std::fs::write(&capture_path, capture(&record()).to_string()).unwrap();
    for input in [
        json!({ "schema": TOKEN }).to_string(),
        json!({ "schema": "ferrum.diagnostic_report", "schema_version": TOKEN }).to_string(),
        format!("{{\"schema\":\"{TOKEN}\", broken"),
    ] {
        std::fs::write(&path, input).unwrap();
        for format in ["json", "human"] {
            let output = bin()
                .args([
                    "diagnose",
                    "--edge-admin-url",
                    &url,
                    "--edge-observation",
                    capture_path.to_str().unwrap(),
                    "--input",
                    path.to_str().unwrap(),
                    "--write-report",
                    saved.to_str().unwrap(),
                    "--format",
                    format,
                ])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert_private(&output);
            assert!(!saved.exists());
        }
    }
    // Credential validation can fail too; it must not disable input scrubbing.
    let invalid_token = format!("{TOKEN}\"invalid");
    let rejected = dir.path().join("rejected.json");
    std::fs::write(&rejected, json!({ "schema": invalid_token }).to_string()).unwrap();
    let output = bin()
        .env(TOKEN_ENV, &invalid_token)
        .args([
            "diagnose",
            "--edge-admin-url",
            &url,
            "--edge-observation",
            capture_path.to_str().unwrap(),
            "--input",
            rejected.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_private(&output);

    let missing = dir.path().join(format!("missing-{TOKEN}.json"));
    let output = bin()
        .args([
            "diagnose",
            "--edge-admin-url",
            &url,
            "--edge-observation",
            missing.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_private(&output);
    assert_eq!(trap.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

#[test]
fn output_errors_redact_the_credential_and_keep_the_failure_code() {
    let record = record();
    let (url, server) = serve("200 OK", "", record.to_string(), Duration::ZERO, false);
    let dir = tempfile::tempdir().unwrap();
    let saved = dir.path().join(TOKEN).join("missing-parent.json");
    let output = diagnose(
        &url,
        &capture(&record),
        &["--write-report", saved.to_str().unwrap()],
    );
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("write "));
    assert_private(&output);
    assert!(!saved.exists());
}

#[test]
fn redaction_refuses_to_save_a_report_if_required_structure_contains_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let saved = dir.path().join("report.json");
    let input = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/reports/forged-verified-claim.json");
    let output = bin()
        .env(TOKEN_ENV, "schema")
        .args([
            "diagnose",
            "--input",
            input.to_str().unwrap(),
            "--write-report",
            saved.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("not writing"));
    assert!(!error.contains("schema"));
    assert!(output.stdout.is_empty());
    assert!(!saved.exists());
}

#[test]
fn lookup_failures_disclose_no_url_or_transport_details() {
    for (url, code) in [
        (format!("http://127.0.0.1:9/?secret={TOKEN}"), 3),
        (format!("https://user:{TOKEN}@admin.example"), 3),
        ("http://127.0.0.1:0".to_owned(), 1),
    ] {
        let output = diagnose(&url, &capture(&record()), &[]);
        assert_eq!(output.status.code(), Some(code));
        assert_private(&output);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains(&url));
        assert!(!error.contains("secret="));
        assert!(!error.contains("Caused by"));
        if code == 1 {
            assert_eq!(error, "error: Edge lookup failed or timed out\n");
        }
    }
}

#[test]
fn https_never_sends_the_credential_to_an_unverified_server() {
    use std::sync::Arc;

    use rcgen::{CertificateParams, KeyPair};
    use rustls::pki_types::PrivatePkcs8KeyDer;

    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.der().clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("TLS request did not arrive: {e}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0; 1];
        // A failed TLS handshake must deliver no application request bytes.
        assert!(!matches!(stream.read(&mut byte), Ok(1)));
    });
    let output = diagnose(&url, &capture(&record()), &[]);
    server.join().unwrap();
    assert!(!output.status.success());
    assert_private(&output);
}

#[test]
fn mismatches_and_forged_success_claims_cannot_confirm() {
    let original = record();
    for (key, value) in [
        ("namespace", json!("other")),
        ("status", json!(503)),
        ("gateway_error", json!("backend_timeout")),
        ("protocol", json!("http3")),
        ("ref", json!("fd1_00000000000000000000000000000000")),
        ("replica_id", json!("1a2b3c4d")),
        ("created_at", json!("2026-09-27T10:15:06Z")),
        ("gateway_error", json!("future_token")),
    ] {
        let mut record = original.clone();
        record[key] = value;
        record["authenticated"] = json!(true);
        let (url, server) = serve("200 OK", "", record.to_string(), Duration::ZERO, false);
        let output = diagnose(&url, &capture(&original), &[]);
        server.join().unwrap();
        assert!(!output.status.success(), "{key}");
        assert_private(&output);
    }
    let mut unknown = original.clone();
    unknown["detail"]["error_class"] = json!("future_sensitive_class");
    unknown["detail"]["proxy_id"] = json!(TOKEN);
    let (url, server) = serve("200 OK", "", unknown.to_string(), Duration::ZERO, false);
    let output = diagnose(&url, &capture(&original), &[]);
    server.join().unwrap();
    assert!(output.status.success());
    assert_private(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let findings = value["report"]["findings"].as_array().unwrap();
    assert!(findings.iter().all(|v| v["confidence"] != "confirmed"));
}

#[test]
fn explicit_observation_and_environment_credential_are_required_before_io() {
    let record = record();
    let mut forged = capture(&record);
    forged["authenticated"] = json!(true);
    assert!(
        !diagnose("http://127.0.0.1:9", &forged, &[])
            .status
            .success()
    );
    for url in [
        "http://localhost:9",
        "http://192.0.2.1:9",
        "https://user:secret@admin.example",
    ] {
        let output = diagnose(url, &capture(&record), &[]);
        assert_eq!(output.status.code(), Some(3));
        assert_private(&output);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("user:secret"));
    }
    let output = bin()
        .args(["diagnose", "--edge-admin-url", "http://127.0.0.1:9"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.json");
    std::fs::write(&path, capture(&record).to_string()).unwrap();
    let output = bin()
        .env_remove(TOKEN_ENV)
        .env(SERVICE_TOKEN_ENV, TOKEN)
        .args([
            "diagnose",
            "--edge-admin-url",
            "http://127.0.0.1:9",
            "--edge-observation",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let output = bin()
        .args([
            "diagnose",
            "--edge-admin-url",
            "http://127.0.0.1:9",
            "--edge-observation",
            path.to_str().unwrap(),
            "--edge-token",
            TOKEN,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
