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
//!
//! Edge v0.9.8 hands Alloy its SERVER span as the `traceparent` parent. Edge
//! v0.9.9 exports a CLIENT span per backend attempt and hands Alloy that span
//! instead, so Alloy's SERVER span nests under the attempt and the attempt
//! under the Edge SERVER span. Both releases are in the support window, so the
//! parentage checks accept either shape and nothing deeper.
//!
//! Besides the happy path it drives the attempt and connection cases: a
//! gateway retry, a cold and a reused gateway connection, concurrent HTTP/2
//! streams, a client cancelling mid-body, and a refused backend connection.
//! Their faults come from inside the compose network (Alloy routes in
//! `src/main.rs`, proxies from `src/bin/gen_e2e_edge_config.rs`), never from
//! changes to the host's network.

#![allow(clippy::print_stdout, reason = "command-line test driver")]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ferrum_alloy_diagnostics::catalog;
use ferrum_alloy_diagnostics::model::{
    Availability, DiagnosticReport, Finding, Leg, Observation, ObservationKind, Producer,
    ProducerKind, Scope, Trust,
};
use ferrum_alloy_diagnostics::otlp::{self, ImportLimits};
use ferrum_alloy_diagnostics::render::render_text;
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

type Failure = Box<dyn std::error::Error + Send + Sync>;

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

/// Starts a streamed request, reads its first body frame, then closes the
/// connection: a client cancelling mid-body. Returns the response head and
/// the number of body bytes read.
async fn cancel_mid_body(addr: SocketAddr, path: &str) -> Result<(Reply, usize), Failure> {
    let stream = tokio::net::TcpStream::connect(addr).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    let connection = tokio::spawn(connection);
    let request = Request::get(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())?;
    let response = sender.send_request(request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let mut body = response.into_body();
    let first = body.frame().await.transpose()?;
    let read = first
        .and_then(|frame| frame.into_data().ok())
        .map_or(0, |data| data.len());
    // Dropping the connection task closes the socket mid-body.
    connection.abort();
    Ok((
        Reply {
            status,
            headers,
            body: String::new(),
        },
        read,
    ))
}

/// Concurrent requests in the HTTP/2 multiplexing case.
const STREAMS: usize = 6;

/// Responses from the attempt and connection cases.
struct Cases {
    /// `/retry/flaky/{key}`: Alloy fails the first attempt with a 503.
    retry: Reply,
    /// The first request through `e2e-reuse`, on a new gateway connection.
    cold: Reply,
    /// Sequential requests after `cold`, through the same proxy.
    later: Vec<Reply>,
    /// Concurrent requests through `e2e-reuse`.
    concurrent: Vec<Reply>,
    /// A long event stream the client abandoned after the first frame.
    cancelled: Reply,
    cancelled_bytes: usize,
    /// `e2e-refused`: nothing listens on the backend port.
    refused: Reply,
}

async fn attempt_cases(edge: SocketAddr) -> Result<Cases, Failure> {
    // A fresh key per run, so a rerun against the same stack fails once again.
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let retry = plain_get(edge, &format!("/retry/flaky/run-{run}")).await?;

    let cold = plain_get(edge, "/reuse/conn").await?;
    let mut later = Vec::new();
    for _ in 0..3 {
        later.push(plain_get(edge, "/reuse/conn").await?);
    }

    let path = format!("/reuse/gather/{STREAMS}");
    let mut tasks = Vec::new();
    for _ in 0..STREAMS {
        let path = path.clone();
        tasks.push(tokio::spawn(async move { plain_get(edge, &path).await }));
    }
    let mut concurrent = Vec::new();
    for task in tasks {
        concurrent.push(task.await??);
    }

    let (cancelled, cancelled_bytes) = cancel_mid_body(edge, "/orders/events/long").await?;
    let refused = plain_get(edge, "/refused/items/7").await?;
    Ok(Cases {
        retry,
        cold,
        later,
        concurrent,
        cancelled,
        cancelled_bytes,
        refused,
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

/// Every span in the raw OTLP/JSON, tagged with its service and scope.
fn all_spans(text: &str) -> Vec<Value> {
    let mut spans = Vec::new();
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
                    let mut span = span.clone();
                    span["x-service"] = json!(service);
                    span["x-scope"] = scope["scope"]["name"].clone();
                    spans.push(span);
                }
            }
        }
    }
    spans
}

fn lower(span: &Value, key: &str) -> String {
    span[key].as_str().unwrap_or_default().to_ascii_lowercase()
}

/// Spans of one trace from the raw OTLP/JSON, keyed by span id.
fn raw_spans(text: &str, trace_id: &str) -> BTreeMap<String, Value> {
    all_spans(text)
        .into_iter()
        .filter(|span| lower(span, "traceId") == trace_id)
        .map(|span| (lower(&span, "spanId"), span))
        .filter(|(id, _)| !id.is_empty())
        .collect()
}

/// The trace of the newest Edge SERVER span for `proxy`, for a response that
/// carried no `traceparent`. Only used for proxies that receive a single
/// request per run; the newest span keeps a rerun against the same stack from
/// matching an earlier run's trace.
fn proxy_trace(text: &str, proxy: &str) -> Option<String> {
    all_spans(text)
        .iter()
        .filter(|span| {
            span["x-scope"] == EDGE_SCOPE
                && span["kind"] == 2
                && attr_str(span, "gateway.proxy.id") == proxy
        })
        .max_by_key(|span| nanos(span, "startTimeUnixNano"))
        .map(|span| lower(span, "traceId"))
}

const EDGE_SCOPE: &str = "ferrum-edge";
const ALLOY_SCOPE: &str = "ferrum-alloy-telemetry";

/// SERVER spans of one producer scope, oldest first. Start times are only
/// compared within one producer.
fn servers<'a>(spans: &'a BTreeMap<String, Value>, scope: &str) -> Vec<&'a Value> {
    let mut found: Vec<&Value> = spans
        .values()
        .filter(|s| s["x-scope"] == scope && s["kind"] == 2)
        .collect();
    found.sort_by_key(|s| nanos(s, "startTimeUnixNano"));
    found
}

fn nanos(span: &Value, key: &str) -> u64 {
    match &span[key] {
        Value::String(s) => s.parse().unwrap_or(0),
        other => other.as_u64().unwrap_or(0),
    }
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

    // 4. Attempt and connection cases. Every fault comes from inside the
    //    compose network: an Alloy route that fails its first attempt, a
    //    backend alias only one proxy uses, and a port where nothing listens.
    let cases = attempt_cases(args.edge).await?;

    // 5. Collect exported spans.
    let traces_path = args.out.join("traces.jsonl");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut settling = false;
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
        let base = has_edge && has_alloy && has_events;
        let waiting = pending(&cases, &text);
        if base && waiting.is_empty() {
            if settling {
                break text;
            }
            // Exact-count checks need any late duplicate too: allow one more
            // export and flush interval before the final read.
            settling = true;
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }
        if Instant::now() > deadline {
            if base {
                // The affected checks fail with the details below.
                println!("spans still missing for: {}", waiting.join(", "));
                break text;
            }
            return Err(fail(format!(
                "spans did not arrive (edge={has_edge}, alloy={has_alloy}, events={has_events})"
            )));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    // 6. Verify relationships for the item request.
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
    let alloy_parent = parent_id(alloy);
    let alloy_gateway = gateway_parent(&spans, alloy);
    check.that(
        "Alloy SERVER span is under the Edge SERVER span, directly or through its attempt span",
        alloy_gateway == edge_id,
        format!("alloy parent {alloy_parent}, its Edge span {alloy_gateway}, edge span {edge_id}"),
    );
    check.that(
        "Edge echoed its SERVER span to the client",
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

    // 7. Streaming: the Alloy span covers the body, not just headers.
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

    // 8. Offline diagnosis of the exported evidence.
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

    // 9. Attempt and connection cases: exported spans and their diagnosis.
    let evidence = Evidence {
        text: &text,
        out: &args.out,
    };
    check_retry(&mut check, &evidence, &cases.retry);
    check_connections(&mut check, &evidence, &cases);
    check_concurrency(&mut check, &evidence, &cases.concurrent);
    check_cancellation(&mut check, &evidence, &cases);
    check_refused(&mut check, &evidence, &cases.refused);
    report(&args, &check)
}

fn collector() -> Producer {
    Producer {
        kind: ProducerKind::Collector,
        name: "edge-e2e".into(),
        version: None,
        instance: None,
    }
}

/// Exported spans, and where per-case diagnoses are kept.
struct Evidence<'a> {
    text: &'a str,
    out: &'a Path,
}

impl Evidence<'_> {
    /// Imports one trace, adds `extra` client observations, runs the rules,
    /// and saves the report as `diagnosis-<label>.json`.
    fn diagnose(
        &self,
        label: &str,
        trace: &str,
        extra: Vec<Observation>,
    ) -> Result<(DiagnosticReport, Vec<Finding>), String> {
        let mut diagnosis = otlp::import(
            self.text,
            Some(trace),
            collector(),
            &ImportLimits::default(),
        )
        .map_err(|e| e.to_string())?;
        diagnosis.observations.extend(extra);
        let findings = analyze(&diagnosis, &Thresholds::default());
        diagnosis.findings.clone_from(&findings);
        let json = serde_json::to_string_pretty(&diagnosis).map_err(|e| e.to_string())?;
        std::fs::write(self.out.join(format!("diagnosis-{label}.json")), json)
            .map_err(|e| e.to_string())?;
        Ok((diagnosis, findings))
    }
}

/// A response header the client observed, as diagnosis input.
fn client_header(name: &str, value: &str) -> Observation {
    Observation {
        id: "client:response-header".into(),
        producer: Producer {
            kind: ProducerKind::Client,
            name: "edge-e2e".into(),
            version: None,
            instance: None,
        },
        kind: ObservationKind::Event,
        name: catalog::CLIENT_RESPONSE_HEADER.into(),
        availability: Availability::Measured,
        value: None,
        unit: None,
        boundaries: None,
        clock: None,
        interval: None,
        scope: Scope {
            leg: Leg::ClientToGateway,
            service: None,
            gateway: None,
            attempt: None,
        },
        span: None,
        attributes: BTreeMap::from([
            ("header".to_owned(), name.to_owned()),
            ("value".to_owned(), value.to_owned()),
        ]),
        trust: Trust::Unverified,
        evidence_ref: None,
        unrecognized: BTreeMap::new(),
    }
}

fn codes(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("{} ({})", f.code, f.confidence.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn confirmed(findings: &[Finding]) -> Vec<&str> {
    findings
        .iter()
        .filter(|f| f.confidence.as_str() == "confirmed")
        .map(|f| f.code.as_str())
        .collect()
}

/// The trace id and parent span id Edge echoed in `traceparent`.
fn echoed(reply: &Reply) -> Option<(String, String)> {
    let value = header(reply, "traceparent").ok()?;
    parse_traceparent(&value).ok()
}

fn body_json(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap_or(Value::Null)
}

fn span_id(span: &Value) -> String {
    lower(span, "spanId")
}

fn parent_id(span: &Value) -> String {
    lower(span, "parentSpanId")
}

/// OTLP `SPAN_KIND_CLIENT`.
const CLIENT_KIND: u64 = 3;

/// The Edge CLIENT span of one backend attempt that parents `service`, when
/// its parent is one. Edge v0.9.9 exports a span per attempt and hands it to
/// the backend as the `traceparent` parent; v0.9.8 hands over its SERVER span.
fn attempt_of<'a>(spans: &'a BTreeMap<String, Value>, service: &Value) -> Option<&'a Value> {
    spans
        .get(&parent_id(service))
        .filter(|span| span["x-scope"] == EDGE_SCOPE && span["kind"] == CLIENT_KIND)
}

/// The Edge span `service` hangs under: its parent, or the parent of the Edge
/// attempt span that is its parent. One attempt hop at most.
fn gateway_parent(spans: &BTreeMap<String, Value>, service: &Value) -> String {
    attempt_of(spans, service)
        .map_or_else(|| parent_id(service), parent_id)
}

/// The `gateway.backend.attempt` number of the Edge attempt span that parents
/// `service`, or an empty string when its parent is not an attempt span.
fn attempt_number(spans: &BTreeMap<String, Value>, service: &Value) -> String {
    attempt_of(spans, service)
        .map(|attempt| attr_str(attempt, "gateway.backend.attempt"))
        .unwrap_or_default()
}

/// Cases whose spans have not all arrived yet.
fn pending(cases: &Cases, text: &str) -> Vec<String> {
    let trace_of = |reply: &Reply| echoed(reply).map(|(trace, _)| trace);
    let refused = trace_of(&cases.refused).or_else(|| proxy_trace(text, "e2e-refused"));
    let mut wanted = vec![
        ("retry".to_owned(), trace_of(&cases.retry), 2),
        ("cold".to_owned(), trace_of(&cases.cold), 1),
        ("cancel".to_owned(), trace_of(&cases.cancelled), 1),
        ("refused".to_owned(), refused, 0),
    ];
    for (index, reply) in cases.later.iter().chain(&cases.concurrent).enumerate() {
        wanted.push((format!("request {index}"), trace_of(reply), 1));
    }
    wanted
        .into_iter()
        .filter(|(_, trace, alloy)| {
            let Some(trace) = trace else {
                return true;
            };
            let spans = raw_spans(text, trace);
            let services = servers(&spans, ALLOY_SCOPE);
            // Behind Edge v0.9.9 a service span's parent is an attempt span,
            // which may be exported apart from the SERVER span.
            let orphaned = services
                .iter()
                .any(|service| !spans.contains_key(&parent_id(service)));
            servers(&spans, EDGE_SCOPE).is_empty() || services.len() < *alloy || orphaned
        })
        .map(|(label, _, _)| label)
        .collect()
}

/// Every timing attribute the tested Edge and Alloy releases put on a SERVER
/// span. Each is request-scoped; neither producer measures connection setup
/// (`docs/measurement-semantics.md`).
const REQUEST_TIMINGS: &[&str] = &[
    "gateway.latency.total_ms",
    "gateway.latency.backend_ttfb_ms",
    "gateway.latency.backend_total_ms",
    "gateway.latency.processing_ms",
    "gateway.overhead_ms",
    "gateway.plugin_execution_ms",
    "alloy.server.time_to_headers_ms",
    "alloy.server.body_duration_ms",
    "alloy.server.duration_ms",
    "alloy.admission.wait_ms",
];

/// What diagnosis lists as missing when it cannot rule out connection setup.
const SETUP_MISSING: &str = "gateway connection setup timing";

/// Evidence that would charge a connection-setup phase to one request: a
/// SERVER-span timing outside the request-scoped set, an observation the
/// catalog does not define, a confirmed finding, or an unattributed interval
/// that does not list setup timing as missing evidence. Unknown is not zero:
/// setup must stay unmeasured, never appear as a value.
fn setup_charges(
    spans: &[&Value],
    diagnosis: &DiagnosticReport,
    findings: &[Finding],
) -> Vec<String> {
    let mut problems = Vec::new();
    for span in spans {
        for attribute in span["attributes"].as_array().into_iter().flatten() {
            let key = attribute["key"].as_str().unwrap_or_default();
            if key.ends_with("_ms") && !REQUEST_TIMINGS.contains(&key) {
                problems.push(format!("span timing {key}"));
            }
        }
    }
    for observation in &diagnosis.observations {
        if !catalog::is_known(&observation.name) {
            problems.push(format!("observation {}", observation.name));
        }
    }
    for finding in findings {
        if finding.confidence.as_str() == "confirmed" {
            problems.push(format!("{} is confirmed", finding.code));
        }
        let lists_setup = finding.missing_evidence.iter().any(|m| m == SETUP_MISSING);
        if finding.code == "alloy.gateway.unattributed_interval" && !lists_setup {
            problems.push(format!("{} does not list {SETUP_MISSING}", finding.code));
        }
    }
    problems
}

/// Edge's default fixed retry backoff (`docs/retry.md` in Ferrum Edge).
const RETRY_BACKOFF_MS: f64 = 100.0;

/// Diagnosis codes that compare gateway and service timings.
const COMPARISONS: &[&str] = &[
    "alloy.gateway.unattributed_interval",
    "alloy.gateway.timings_not_comparable",
    "alloy.evidence.service_exceeds_gateway",
];

/// A retried request: two Alloy SERVER spans under one Edge SERVER span
/// (behind Edge v0.9.9, each through its own numbered attempt span), and a
/// diagnosis that reports several attempts without splitting the gateway's
/// measurement between them.
fn check_retry(check: &mut Check, evidence: &Evidence<'_>, reply: &Reply) {
    check.that(
        "retry: the client received the recovered response",
        reply.status == StatusCode::OK && reply.body == "recovered",
        format!("{} {}", reply.status, reply.body),
    );
    let Some((trace, parent)) = echoed(reply) else {
        check.that("retry: Edge echoed a traceparent", false, "missing");
        return;
    };
    let spans = raw_spans(evidence.text, &trace);
    let edges = servers(&spans, EDGE_SCOPE);
    let attempts = servers(&spans, ALLOY_SCOPE);
    check.that(
        "retry: one Edge SERVER span and two Alloy SERVER spans",
        edges.len() == 1 && attempts.len() == 2,
        format!("edge {}, alloy {}", edges.len(), attempts.len()),
    );
    let parents: Vec<String> = attempts.iter().copied().map(parent_id).collect();
    let gateways: Vec<String> = attempts
        .iter()
        .map(|attempt| gateway_parent(&spans, attempt))
        .collect();
    check.that(
        "retry: both attempts are under the same Edge SERVER span",
        attempts.len() == 2
            && edges.iter().all(|s| span_id(s) == parent)
            && gateways.iter().all(|p| *p == parent),
        format!(
            "parents {}; their Edge spans {}; echoed {parent}",
            parents.join(", "),
            gateways.join(", ")
        ),
    );
    // Edge v0.9.9 gives every attempt its own CLIENT span; v0.9.8 gives none.
    let numbers: Vec<String> = attempts
        .iter()
        .map(|attempt| attempt_number(&spans, attempt))
        .collect();
    check.that(
        "retry: each attempt has its own numbered Edge attempt span, or none has one",
        numbers == ["1", "2"] || numbers == ["", ""],
        format!("attempt numbers [{}]", numbers.join(", ")),
    );
    let statuses: Vec<String> = attempts
        .iter()
        .map(|s| attr_str(s, "http.response.status_code"))
        .collect();
    check.that(
        "retry: the first attempt answered 503 and the second 200",
        statuses == ["503", "200"],
        statuses.join(", "),
    );
    let request_id = header(reply, "x-request-id").unwrap_or_default();
    let carried = attempts.iter().all(|s| {
        attr_str(s, "alloy.trace.parent") == "accepted_remote"
            && attr_str(s, "alloy.peer.trust") == "verified_identity"
            && attr_str(s, "alloy.request_id") == request_id
    });
    check.that(
        "retry: every attempt carried the gateway's trace context and request id",
        attempts.len() == 2 && carried,
        format!("request id {request_id}"),
    );
    let edge_ttfb = edges
        .first()
        .and_then(|s| attr_f64(s, "gateway.latency.backend_ttfb_ms"))
        .unwrap_or(-1.0);
    // The attempts ran one after the other, separated by the backoff, so
    // Edge's single measurement is at least their sum plus the backoff.
    let headers: Vec<f64> = attempts
        .iter()
        .filter_map(|s| attr_f64(s, "alloy.server.time_to_headers_ms"))
        .collect();
    let floor = headers.iter().sum::<f64>() + RETRY_BACKOFF_MS;
    check.that(
        "retry: Edge backend time-to-headers covers both attempts and the backoff",
        headers.len() == 2 && edge_ttfb >= floor,
        format!("Edge {edge_ttfb:.1} ms; attempts {headers:?} ms; floor {floor:.1} ms"),
    );
    match evidence.diagnose("retry", &trace, Vec::new()) {
        Ok((diagnosis, findings)) => {
            let compared: Vec<&str> = findings
                .iter()
                .map(|f| f.code.as_str())
                .filter(|code| COMPARISONS.contains(code))
                .collect();
            let multiple = findings
                .iter()
                .any(|f| f.code == "alloy.gateway.multiple_service_attempts");
            check.that(
                "retry: diagnosis reports multiple attempts and compares no timings",
                multiple && compared.is_empty(),
                codes(&findings),
            );
            let scoped = diagnosis
                .observations
                .iter()
                .filter(|o| o.scope.attempt.is_some())
                .count();
            let cited = findings
                .iter()
                .flat_map(|f| &f.evidence)
                .filter(|e| e.attempt.is_some())
                .count();
            let per_attempt = scoped + cited;
            check.that(
                "retry: diagnosis invents no per-attempt breakdown or confirmed finding",
                per_attempt == 0 && confirmed(&findings).is_empty(),
                format!(
                    "{per_attempt} attempt-scoped values; confirmed: {}",
                    confirmed(&findings).join(", ")
                ),
            );
        }
        Err(error) => check.that("retry: diagnosis imported", false, error),
    }
}

/// A cold and a reused gateway connection: the service's own view of its
/// connection tells them apart, and neither request's evidence carries a
/// connection-setup value. The service counts every request per connection,
/// on any route, so a connection that already carried main-proxy traffic or a
/// health check does not pass as cold.
fn check_connections(check: &mut Check, evidence: &Evidence<'_>, cases: &Cases) {
    let earlier = |reply: &Reply| body_json(reply)["earlier_requests"].as_u64();
    check.that(
        "reuse: the first request's gateway connection carried no earlier request",
        cases.cold.status == StatusCode::OK && earlier(&cases.cold) == Some(0),
        format!("{} {}", cases.cold.status, cases.cold.body),
    );
    let reused = cases
        .later
        .iter()
        .find(|&reply| earlier(reply).is_some_and(|n| n > 0));
    check.that(
        "reuse: a later request reused a gateway connection",
        reused.is_some(),
        cases
            .later
            .iter()
            .map(|reply| reply.body.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    for (label, reply) in [("cold", Some(&cases.cold)), ("reused", reused)] {
        let Some(reply) = reply else {
            continue;
        };
        let Some((trace, parent)) = echoed(reply) else {
            check.that(&format!("reuse: {label} traceparent"), false, "missing");
            continue;
        };
        let spans = raw_spans(evidence.text, &trace);
        let edges = servers(&spans, EDGE_SCOPE);
        let services = servers(&spans, ALLOY_SCOPE);
        let linked = services
            .iter()
            .all(|service| gateway_parent(&spans, service) == parent);
        check.that(
            &format!("reuse: the {label} request has one Edge and one Alloy SERVER span under it"),
            edges.len() == 1
                && services.len() == 1
                && edges.iter().all(|s| span_id(s) == parent)
                && linked,
            format!("edge {}, alloy {}", edges.len(), services.len()),
        );
        let name = if label == "cold" {
            "reuse: the cold request's connection setup stays unmeasured, not zero"
        } else {
            "reuse: no connection setup is charged to the reused request"
        };
        match evidence.diagnose(&format!("reuse-{label}"), &trace, Vec::new()) {
            Ok((diagnosis, findings)) => {
                let both: Vec<&Value> = edges.iter().chain(&services).copied().collect();
                let problems = setup_charges(&both, &diagnosis, &findings);
                let detail = if problems.is_empty() {
                    codes(&findings)
                } else {
                    problems.join("; ")
                };
                check.that(name, problems.is_empty(), detail);
            }
            Err(error) => check.that(name, false, error),
        }
    }
}

/// Concurrent requests that Edge multiplexed as HTTP/2 streams to Alloy.
fn check_concurrency(check: &mut Check, evidence: &Evidence<'_>, replies: &[Reply]) {
    let bodies: Vec<Value> = replies.iter().map(body_json).collect();
    let peaks: Vec<u64> = bodies
        .iter()
        .map(|b| b["peak_in_flight"].as_u64().unwrap_or(0))
        .collect();
    let full = u64::try_from(STREAMS).unwrap_or(u64::MAX);
    check.that(
        "http2: all concurrent requests were inside Alloy at the same time",
        replies.len() == STREAMS
            && replies.iter().all(|r| r.status == StatusCode::OK)
            && peaks.iter().all(|&p| p == full),
        format!("peaks {peaks:?}"),
    );
    let connections: BTreeSet<&str> = bodies.iter().filter_map(|b| b["remote"].as_str()).collect();
    check.that(
        "http2: concurrent requests shared gateway connections",
        bodies.iter().all(|b| b["remote"].is_string())
            && !connections.is_empty()
            && connections.len() < STREAMS,
        format!("{} connection(s) for {STREAMS} requests", connections.len()),
    );

    let mut problems = Vec::new();
    let mut streams: Vec<Value> = Vec::new();
    let mut setup = Vec::new();
    for (index, reply) in replies.iter().enumerate() {
        let Some((trace, parent)) = echoed(reply) else {
            problems.push(format!("request {index}: no traceparent"));
            continue;
        };
        let spans = raw_spans(evidence.text, &trace);
        let edges = servers(&spans, EDGE_SCOPE);
        let services = servers(&spans, ALLOY_SCOPE);
        let counts = (edges.len(), services.len());
        let linked = |service: &Value| gateway_parent(&spans, service) == parent;
        match (edges.as_slice(), services.as_slice()) {
            ([edge], [service]) if span_id(edge) == parent && linked(service) => {
                streams.push((*service).clone());
            }
            _ => problems.push(format!("request {index}: edge and alloy spans {counts:?}")),
        }
        match evidence.diagnose(&format!("http2-{index}"), &trace, Vec::new()) {
            Ok((diagnosis, findings)) => {
                let both: Vec<&Value> = edges.iter().chain(&services).copied().collect();
                setup.extend(setup_charges(&both, &diagnosis, &findings));
            }
            Err(error) => setup.push(error),
        }
    }
    check.that(
        "http2: each stream has one Edge and one Alloy SERVER span under it",
        problems.is_empty() && streams.len() == STREAMS,
        problems.join("; "),
    );
    let versions: Vec<String> = streams
        .iter()
        .map(|s| attr_str(s, "network.protocol.version"))
        .collect();
    check.that(
        "http2: Edge reached Alloy over HTTP/2 for every stream",
        streams.len() == STREAMS && versions.iter().all(|v| v == "2"),
        versions.join(", "),
    );
    // Start and end times come from one Alloy process, so they share a clock.
    let latest_start = streams
        .iter()
        .map(|s| nanos(s, "startTimeUnixNano"))
        .max()
        .unwrap_or(0);
    let earliest_end = streams
        .iter()
        .map(|s| nanos(s, "endTimeUnixNano"))
        .min()
        .unwrap_or(0);
    check.that(
        "http2: the Alloy SERVER spans overlap in time",
        streams.len() == STREAMS && latest_start > 0 && latest_start < earliest_end,
        format!("latest start {latest_start}, earliest end {earliest_end}"),
    );
    check.that(
        "http2: no connection setup is charged to a multiplexed request",
        setup.is_empty(),
        setup.join("; "),
    );
}

/// Length of `/events/long`: 40 events, one every 100 ms.
const LONG_STREAM_MS: f64 = 4_000.0;

/// A client that leaves mid-body: Alloy finalizes the request once, as
/// `cancelled`, well before the stream would have ended.
fn check_cancellation(check: &mut Check, evidence: &Evidence<'_>, cases: &Cases) {
    let reply = &cases.cancelled;
    check.that(
        "cancel: the stream had started when the client left",
        reply.status == StatusCode::OK && cases.cancelled_bytes > 0,
        format!("{}, {} bytes read", reply.status, cases.cancelled_bytes),
    );
    let Some((trace, parent)) = echoed(reply) else {
        check.that("cancel: Edge echoed a traceparent", false, "missing");
        return;
    };
    let spans = raw_spans(evidence.text, &trace);
    let edges = servers(&spans, EDGE_SCOPE);
    let services = servers(&spans, ALLOY_SCOPE);
    let outcomes: Vec<String> = services
        .iter()
        .map(|s| attr_str(s, "alloy.response.body.outcome"))
        .collect();
    let edge_attr = |key: &str| edges.first().map(|s| attr_str(s, key)).unwrap_or_default();
    let detail = format!(
        "outcomes [{}]; Edge client.disconnected={}, body.completed={}",
        outcomes.join(", "),
        edge_attr("gateway.client.disconnected"),
        edge_attr("gateway.body.completed")
    );
    check.that(
        "cancel: Edge recorded that the client disconnected",
        edge_attr("gateway.client.disconnected") == "true",
        detail.clone(),
    );
    let linked = services
        .iter()
        .all(|service| gateway_parent(&spans, service) == parent);
    check.that(
        "cancel: Alloy recorded the request exactly once, as cancelled",
        outcomes == ["cancelled"] && linked,
        detail,
    );
    let body_ms = services
        .first()
        .and_then(|s| attr_f64(s, "alloy.server.body_duration_ms"))
        .unwrap_or(-1.0);
    check.that(
        "cancel: the body ended mid-stream",
        (0.0..LONG_STREAM_MS * 0.75).contains(&body_ms),
        format!("{body_ms:.1} ms of a {LONG_STREAM_MS:.0} ms stream"),
    );
    match evidence.diagnose("cancel", &trace, Vec::new()) {
        Ok((_, findings)) => {
            let incomplete = findings
                .iter()
                .filter(|f| f.code == "alloy.response.body_incomplete")
                .count();
            check.that(
                "cancel: diagnosis reports one incomplete body and nothing confirmed",
                incomplete == 1 && confirmed(&findings).is_empty(),
                codes(&findings),
            );
        }
        Err(error) => check.that("cancel: diagnosis imported", false, error),
    }
}

/// What the `connection_failure` token must not be read as (rule `alloy.r007`).
const NOT_DOWN: &str = "that the service is down";

/// A refused backend connection: Edge's gateway error token and error class,
/// no service span, and a diagnosis that does not blame the service.
fn check_refused(check: &mut Check, evidence: &Evidence<'_>, reply: &Reply) {
    let token = header(reply, "x-gateway-error").unwrap_or_default();
    check.that(
        "refused: Edge answered with the connection_failure gateway error",
        reply.status.is_server_error() && token == "connection_failure",
        format!("{} X-Gateway-Error: {token}", reply.status),
    );
    let trace = echoed(reply)
        .map(|(trace, _)| trace)
        .or_else(|| proxy_trace(evidence.text, "e2e-refused"));
    let Some(trace) = trace else {
        check.that("refused: Edge exported a SERVER span", false, "missing");
        return;
    };
    let spans = raw_spans(evidence.text, &trace);
    let edges = servers(&spans, EDGE_SCOPE);
    let services = servers(&spans, ALLOY_SCOPE);
    let class = edges
        .first()
        .map(|s| attr_str(s, "gateway.error.class"))
        .unwrap_or_default();
    check.that(
        "refused: one Edge SERVER span with an error class and no Alloy span",
        edges.len() == 1 && services.is_empty() && !class.is_empty(),
        format!(
            "edge {}, alloy {}, gateway.error.class {class}",
            edges.len(),
            services.len()
        ),
    );
    let observed = client_header("X-Gateway-Error", &token);
    match evidence.diagnose("refused", &trace, vec![observed]) {
        Ok((_, findings)) => {
            let explained = findings.iter().any(|f| {
                f.code == "alloy.edge.gateway_error_token"
                    && f.confidence.as_str() == "likely"
                    && f.does_not_prove.iter().any(|d| d == NOT_DOWN)
            });
            let classified = findings
                .iter()
                .any(|f| f.code == "alloy.edge.gateway_error_class");
            check.that(
                "refused: diagnosis explains the failure without blaming the service",
                explained && classified && confirmed(&findings).is_empty(),
                codes(&findings),
            );
        }
        Err(error) => check.that("refused: diagnosis imported", false, error),
    }
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
