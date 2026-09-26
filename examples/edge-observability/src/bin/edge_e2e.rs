//! Drives the Edge → Alloy stack in `compose.yaml` and verifies the
//! resulting telemetry exported through the OpenTelemetry Collector.
//!
//! ```text
//! edge-e2e --compose examples/edge-observability/compose.yaml --out target/e2e
//! ```
//!
//! Every assertion is about evidence the components actually exported:
//! span parentage, identity, and timing attributes read from the
//! collector's OTLP/JSON file. Nothing is inferred from a mock.

#![allow(clippy::print_stdout, reason = "command-line test driver")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ferrum_alloy_diagnostics::model::{DiagnosticReport, Producer, ProducerKind};
use ferrum_alloy_diagnostics::otlp::{self, ImportLimits};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

type Failure = Box<dyn std::error::Error>;

fn fail(message: impl Into<String>) -> Failure {
    message.into().into()
}

struct Args {
    compose: PathBuf,
    out: PathBuf,
    edge: SocketAddr,
    alloy: SocketAddr,
}

fn args() -> Result<Args, Failure> {
    let mut compose = None;
    let mut out = PathBuf::from("target/e2e");
    let mut edge: SocketAddr = "127.0.0.1:18000".parse()?;
    let mut alloy: SocketAddr = "127.0.0.1:18443".parse()?;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| fail(format!("{flag} needs a value")))?;
        match flag.as_str() {
            "--compose" => compose = Some(PathBuf::from(value)),
            "--out" => out = PathBuf::from(value),
            "--edge" => edge = value.parse()?,
            "--alloy" => alloy = value.parse()?,
            other => return Err(fail(format!("unknown flag {other}"))),
        }
    }
    Ok(Args {
        compose: compose.ok_or_else(|| fail("--compose is required"))?,
        out,
        edge,
        alloy,
    })
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn plain_get(addr: SocketAddr, path: &str) -> Result<Reply, Failure> {
    let stream = tokio::net::TcpStream::connect(addr).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);
    let request = Request::get(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())?;
    let response = sender.send_request(request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await?.to_bytes();
    Ok(Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

async fn tls_get(
    addr: SocketAddr,
    ca_pem: &Path,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<Reply, Failure> {
    use rustls_pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pki_types::CertificateDer::pem_file_iter(ca_pem)? {
        roots.add(cert?)?;
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let name = rustls_pki_types::ServerName::try_from("localhost")?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);
    let mut request = Request::get(path).header("host", "localhost");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(request.body(Empty::<Bytes>::new())?)
        .await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await?.to_bytes();
    Ok(Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn compose(args: &Args, command: &[&str]) -> Result<(), Failure> {
    // `DOCKER_COMPOSE` may name a standalone binary (e.g. `docker-compose`).
    let program = std::env::var("DOCKER_COMPOSE").unwrap_or_else(|_| "docker compose".to_owned());
    let mut parts = program.split_whitespace();
    let mut command_line = std::process::Command::new(parts.next().unwrap_or("docker"));
    command_line.args(parts);
    let status = command_line
        .arg("-f")
        .arg(&args.compose)
        .args(command)
        .stdout(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(fail(format!("docker compose {} failed", command.join(" "))))
    }
}

fn header(reply: &Reply, name: &str) -> Result<String, Failure> {
    reply
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| {
            fail(format!(
                "response has no {name} header (status {})",
                reply.status
            ))
        })
}

/// Spans of one trace from the raw OTLP/JSON, keyed by span id.
fn raw_spans(text: &str, trace_id: &str) -> BTreeMap<String, Value> {
    let mut spans = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for resource in value["resourceSpans"].as_array().into_iter().flatten() {
            let service = resource["resource"]["attributes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["key"] == "service.name")
                .and_then(|a| a["value"]["stringValue"].as_str())
                .unwrap_or_default()
                .to_owned();
            for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                for span in scope["spans"].as_array().into_iter().flatten() {
                    if span["traceId"]
                        .as_str()
                        .map(str::to_ascii_lowercase)
                        .as_deref()
                        == Some(trace_id)
                    {
                        let mut span = span.clone();
                        span["x-service"] = json!(service);
                        span["x-scope"] = scope["scope"]["name"].clone();
                        if let Some(id) = span["spanId"].as_str() {
                            spans.insert(id.to_ascii_lowercase(), span);
                        }
                    }
                }
            }
        }
    }
    spans
}

fn attr(span: &Value, key: &str) -> Option<Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| {
            let v = &a["value"];
            v.get("stringValue")
                .or_else(|| v.get("intValue"))
                .or_else(|| v.get("doubleValue"))
                .or_else(|| v.get("boolValue"))
                .cloned()
                .unwrap_or(Value::Null)
        })
}

fn attr_str(span: &Value, key: &str) -> String {
    match attr(span, key) {
        Some(Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn attr_f64(span: &Value, key: &str) -> Option<f64> {
    match attr(span, key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

struct Check {
    results: Vec<(String, bool, String)>,
}

impl Check {
    fn that(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.results.push((name.to_owned(), ok, detail.into()));
    }
}

fn parse_traceparent(value: &str) -> Result<(String, String), Failure> {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 4 || parts[1].len() != 32 || parts[2].len() != 16 {
        return Err(fail(format!(
            "malformed traceparent echoed by Edge: {value}"
        )));
    }
    Ok((parts[1].to_owned(), parts[2].to_owned()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Failure> {
    let args = args()?;
    std::fs::create_dir_all(&args.out)?;
    let ca = args.out.join("ca.pem");
    compose(&args, &["cp", "alloy:/certs/ca.pem", &ca.to_string_lossy()])?;

    // 1. Wait until Edge routes to a ready Alloy.
    let started = Instant::now();
    loop {
        match plain_get(args.edge, "/orders/hello").await {
            Ok(reply) if reply.status == StatusCode::OK => break,
            Ok(reply) if started.elapsed() > Duration::from_secs(90) => {
                return Err(fail(format!(
                    "Edge never routed successfully: {} {}",
                    reply.status, reply.body
                )));
            }
            Err(error) if started.elapsed() > Duration::from_secs(90) => {
                return Err(fail(format!("Edge never became reachable: {error}")));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }

    let mut check = Check {
        results: Vec::new(),
    };

    // 2. Requests through the gateway.
    let item = plain_get(args.edge, "/orders/items/7").await?;
    check.that(
        "item via Edge returns 200",
        item.status == StatusCode::OK,
        format!("{} {}", item.status, item.body),
    );
    let (item_trace, item_edge_parent) = parse_traceparent(&header(&item, "traceparent")?)?;
    let item_request_id = header(&item, "x-request-id")?;

    let events = plain_get(args.edge, "/orders/events").await?;
    check.that(
        "event stream via Edge delivers every event",
        events.status == StatusCode::OK && events.body.matches("event: tick").count() == 5,
        format!(
            "{} ({} events)",
            events.status,
            events.body.matches("event: tick").count()
        ),
    );
    let (events_trace, _) = parse_traceparent(&header(&events, "traceparent")?)?;

    let missing = plain_get(args.edge, "/orders/no-such-route").await?;
    check.that(
        "unknown route is an Alloy problem through Edge",
        missing.status == StatusCode::NOT_FOUND && missing.body.contains("route-not-found"),
        format!("{} {}", missing.status, missing.body),
    );

    // 3. Bypass prevention: direct callers are not the gateway.
    let direct = tls_get(
        args.alloy,
        &ca,
        "/items/7",
        &[
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
            ("x-consumer-username", "mallory"),
        ],
    )
    .await?;
    check.that(
        "direct request without the gateway identity is rejected",
        direct.status == StatusCode::FORBIDDEN && direct.body.contains("gateway-required"),
        format!("{} {}", direct.status, direct.body),
    );
    let probe = tls_get(args.alloy, &ca, "/readyz", &[]).await?;
    check.that(
        "direct readiness probe is allowed",
        probe.status == StatusCode::OK,
        format!("{}", probe.status),
    );

    // 4. Collect exported spans.
    let traces_path = args.out.join("traces.jsonl");
    let deadline = Instant::now() + Duration::from_secs(60);
    let text = loop {
        compose(
            &args,
            &[
                "cp",
                "otel-collector:/out/traces.jsonl",
                &traces_path.to_string_lossy(),
            ],
        )?;
        let text = std::fs::read_to_string(&traces_path).unwrap_or_default();
        let item_spans = raw_spans(&text, &item_trace);
        let has_edge = item_spans.values().any(|s| s["x-scope"] == "ferrum-edge");
        let has_alloy = item_spans
            .values()
            .any(|s| s["x-scope"] == "ferrum-alloy-telemetry" && s["kind"] == 2);
        let has_events = raw_spans(&text, &events_trace)
            .values()
            .filter(|s| s["kind"] == 2)
            .count()
            >= 2;
        if has_edge && has_alloy && has_events {
            break text;
        }
        if Instant::now() > deadline {
            return Err(fail(format!(
                "spans did not arrive (edge={has_edge}, alloy={has_alloy}, events={has_events})"
            )));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    // 5. Verify relationships for the item request.
    let spans = raw_spans(&text, &item_trace);
    let edge_servers: Vec<&Value> = spans
        .values()
        .filter(|s| s["x-scope"] == "ferrum-edge" && s["kind"] == 2)
        .collect();
    let alloy_servers: Vec<&Value> = spans
        .values()
        .filter(|s| s["x-scope"] == "ferrum-alloy-telemetry" && s["kind"] == 2)
        .collect();
    check.that(
        "exactly one Edge SERVER span",
        edge_servers.len() == 1,
        format!("{}", edge_servers.len()),
    );
    check.that(
        "exactly one Alloy SERVER span",
        alloy_servers.len() == 1,
        format!("{}", alloy_servers.len()),
    );
    let (Some(edge), Some(alloy)) = (edge_servers.first(), alloy_servers.first()) else {
        return report(&args, &check);
    };
    let edge_id = edge["spanId"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let alloy_id = alloy["spanId"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let alloy_parent = alloy["parentSpanId"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    check.that(
        "Alloy SERVER span is a child of the Edge SERVER span",
        alloy_parent == edge_id,
        format!("alloy parent {alloy_parent}, edge span {edge_id}"),
    );
    check.that(
        "Edge echoed the same parent it injected upstream",
        item_edge_parent == edge_id,
        format!("echoed {item_edge_parent}, edge span {edge_id}"),
    );
    check.that(
        "Alloy accepted the trace context from a verified identity",
        attr_str(alloy, "alloy.trace.parent") == "accepted_remote"
            && attr_str(alloy, "alloy.peer.trust") == "verified_identity",
        format!(
            "trace.parent={}, peer.trust={}",
            attr_str(alloy, "alloy.trace.parent"),
            attr_str(alloy, "alloy.peer.trust")
        ),
    );
    check.that(
        "Alloy used the gateway's request id",
        attr_str(alloy, "alloy.request_id") == item_request_id,
        format!(
            "span {} vs response {item_request_id}",
            attr_str(alloy, "alloy.request_id")
        ),
    );
    check.that(
        "Alloy recorded the route template",
        attr_str(alloy, "http.route") == "/items/{id}",
        attr_str(alloy, "http.route"),
    );
    let operation = spans.values().find(|s| s["name"] == "inventory.lookup");
    let op_ms = operation
        .and_then(|op| attr_f64(op, "alloy.operation.duration_ms"))
        .unwrap_or(-1.0);
    check.that(
        "the instrumented operation is a child of the Alloy SERVER span",
        operation.is_some_and(|op| {
            op["parentSpanId"].as_str().map(str::to_ascii_lowercase) == Some(alloy_id.clone())
        }),
        format!("{operation:?}")
            .chars()
            .take(160)
            .collect::<String>(),
    );
    check.that(
        "operation duration covers the simulated dependency",
        op_ms >= 115.0,
        format!("{op_ms:.1} ms"),
    );
    let ttfh = attr_f64(alloy, "alloy.server.time_to_headers_ms").unwrap_or(-1.0);
    check.that(
        "Alloy time-to-headers encloses the operation",
        ttfh >= op_ms,
        format!("{ttfh:.1} ms"),
    );
    let edge_ttfb = attr_f64(edge, "gateway.latency.backend_ttfb_ms").unwrap_or(-1.0);
    check.that(
        "Edge measured backend time",
        edge_ttfb >= 0.0,
        format!("{edge_ttfb:.1} ms"),
    );

    // 6. Streaming: the Alloy span covers the body, not just headers.
    let event_spans = raw_spans(&text, &events_trace);
    if let Some(stream) = event_spans
        .values()
        .find(|s| s["x-scope"] == "ferrum-alloy-telemetry" && s["kind"] == 2)
    {
        let body_ms = attr_f64(stream, "alloy.server.body_duration_ms").unwrap_or(-1.0);
        let head_ms = attr_f64(stream, "alloy.server.time_to_headers_ms").unwrap_or(-1.0);
        check.that(
            "stream: headers were early and the body lasted until completion",
            head_ms < body_ms
                && body_ms >= 250.0
                && attr_str(stream, "alloy.response.body.outcome") == "completed",
            format!(
                "time_to_headers {head_ms:.1} ms, body {body_ms:.1} ms, outcome {}",
                attr_str(stream, "alloy.response.body.outcome")
            ),
        );
    } else {
        check.that("stream: Alloy span exported", false, "missing");
    }

    // 7. Offline diagnosis of the exported evidence.
    let collector = Producer {
        kind: ProducerKind::Collector,
        name: "edge-e2e".into(),
        version: None,
        instance: None,
    };
    let mut diagnosis: DiagnosticReport = otlp::import(
        &text,
        Some(&item_trace),
        collector,
        &ImportLimits::default(),
    )?;
    let findings = analyze(&diagnosis, &Thresholds::default());
    check.that(
        "diagnosis attributes the delay to the instrumented operation",
        findings
            .iter()
            .any(|f| f.code == "alloy.service.operation_dominates"),
        findings
            .iter()
            .map(|f| f.code.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
    check.that(
        "diagnosis never claims confirmed evidence from a file import",
        findings
            .iter()
            .all(|f| f.confidence.as_str() != "confirmed"),
        String::new(),
    );
    diagnosis.findings.clone_from(&findings);
    std::fs::write(
        args.out.join("diagnosis.json"),
        serde_json::to_string_pretty(&diagnosis)?,
    )?;
    std::fs::write(
        args.out.join("diagnosis.txt"),
        render_text(&diagnosis, &findings, &[]),
    )?;
    println!("{}", render_text(&diagnosis, &findings, &[]));
    report(&args, &check)
}

fn report(args: &Args, check: &Check) -> Result<(), Failure> {
    let summary: Vec<Value> = check
        .results
        .iter()
        .map(|(name, ok, detail)| json!({ "check": name, "passed": ok, "detail": detail }))
        .collect();
    std::fs::write(
        args.out.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    let mut failed = 0;
    for (name, ok, detail) in &check.results {
        println!(
            "{} {name}{}",
            if *ok { "PASS" } else { "FAIL" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            }
        );
        if !ok {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(fail(format!("{failed} end-to-end check(s) failed")));
    }
    println!("all {} end-to-end checks passed", check.results.len());
    Ok(())
}
