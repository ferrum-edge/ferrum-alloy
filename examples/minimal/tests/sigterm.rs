//! The built binary shuts down gracefully on a real SIGTERM.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn sigterm_drains_and_exits_cleanly() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_example-minimal"))
        .env("FERRUM_ALLOY_BIND", "127.0.0.1:0")
        .env("FERRUM_ALLOY_MANAGEMENT_ENABLED", "false")
        .env("FERRUM_ALLOY_LOG_FORMAT", "json")
        .env("FERRUM_ALLOY_LOG_FILTER", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();

    // Find the bound address in the "serving" event.
    let addr = loop {
        let line = lines
            .next()
            .expect("process exited before serving")
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        if event["message"] == "serving" {
            let listen = event["listen"].as_str().unwrap();
            break listen
                .trim_start_matches("Some(")
                .trim_end_matches(')')
                .to_owned();
        }
    };

    let mut stream = TcpStream::connect(&addr).unwrap();
    stream
        .write_all(b"GET /hello HTTP/1.1\r\nhost: t\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("Hello from Ferrum Alloy"));

    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let started = Instant::now();
    let mut saw_complete = false;
    for line in lines.map_while(Result::ok) {
        if line.contains("shutdown complete") {
            saw_complete = true;
        }
    }
    let exit = child.wait().unwrap();
    assert!(exit.success(), "exit status {exit}");
    assert!(saw_complete, "the shutdown sequence ran to completion");
    assert!(started.elapsed() < Duration::from_secs(10));
}
