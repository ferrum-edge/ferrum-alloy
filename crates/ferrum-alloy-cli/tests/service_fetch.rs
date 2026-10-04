//! Hosted HTTP fixtures for the service `diagnose --url` retrieval path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN_ENV: &str = "FERRUM_ALLOY_DIAGNOSTICS_TOKEN";
const TOKEN: &str = "private-service-lookup-credential";

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrum-alloy"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FERRUM_ALLOY_") {
            command.env_remove(name);
        }
    }
    command.env(TOKEN_ENV, TOKEN)
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

/// Serve a report, optionally trickling a valid whitespace prefix before it.
/// The channel marks when response headers are sent, for deadline measurement.
fn serve(
    body: String,
    trickle: bool,
) -> (String, std::sync::mpsc::Receiver<Instant>, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
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
        let prefix_bytes = if trickle { 40 } else { 0 };
        let response_body_bytes = body.len() + prefix_bytes;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {response_body_bytes}\r\nconnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).unwrap();
        started_tx.send(Instant::now()).unwrap();
        if trickle {
            for _ in 0..prefix_bytes {
                if let Err(error) = stream.write_all(b" ") {
                    assert_client_disconnect(&error);
                    return String::from_utf8(request).unwrap();
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if let Err(error) = stream.write_all(body.as_bytes()) {
            assert_client_disconnect(&error);
        }
        String::from_utf8(request).unwrap()
    });
    (url, started_rx, server)
}

fn assert_client_disconnect(error: &std::io::Error) {
    assert!(
        matches!(
            error.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
        ) || matches!(error.raw_os_error(), Some(10053 | 10054)),
        "unexpected client disconnect: kind={:?}, raw_os_error={:?}",
        error.kind(),
        error.raw_os_error()
    );
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
    let (url, _, server) = serve(body, false);
    let output = diagnose(&url, "1000");
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("alloy.service.operation_dominates")
    );
    assert!(request.starts_with("GET /diagnostics/v1/requests/req-1 HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}"))
    );
    assert_private(&output);
}

#[test]
fn a_trickling_service_body_cannot_extend_the_whole_operation_deadline() {
    let (url, started_rx, server) = serve(report(), true);
    let output = diagnose(&url, "600");
    let elapsed = started_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .elapsed();
    let request = server.join().unwrap();
    assert!(request.starts_with("GET /diagnostics/v1/requests/req-1 HTTP/1.1\r\n"));
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reading the report failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_private(&output);
}
