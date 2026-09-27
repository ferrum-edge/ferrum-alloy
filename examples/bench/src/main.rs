//! Overhead benchmark: plain Axum versus Ferrum Alloy configurations.
//!
//! ```text
//! alloy-bench --scenario alloy --payload small --connections 32 --seconds 10 --warmup 2 [--http2]
//! ```
//!
//! Server and client run in one process on separate multi-threaded Tokio
//! runtimes over loopback. Results are same-host measurements with the noise
//! that implies; see `docs/benchmarks.md` for how to read them. Nothing here
//! measures CPU time, memory, or allocations.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "benchmark driver"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::config::AlloyConfig;
use ferrum_alloy::telemetry::json::JsonLayer;
use ferrum_alloy::telemetry::metrics::Metrics;
use ferrum_alloy::telemetry::otel::{OtelPipeline, OtlpConfig, ServiceResource};
use ferrum_alloy::{AlloyApp, TelemetryInit};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

type Failure = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// `axum::serve` with the same router, no Alloy.
    Plain,
    /// `AlloyApp` defaults, telemetry layer active, no subscriber output.
    Alloy,
    /// As `Alloy`, plus JSON access logs written to a sink by Alloy's layer.
    AlloyLogs,
    /// As `AlloyLogs`, formatted by tracing-subscriber's JSON `fmt` layer,
    /// which Alloy used before its own layer.
    AlloyLogsFmt,
    /// OpenTelemetry bridge, every request sampled, exporter discards batches.
    OtelSampled,
    /// OpenTelemetry bridge with sampling ratio 0.
    OtelUnsampled,
    /// Sampled, exporting over OTLP/HTTP to a closed port.
    OtelUnreachable,
}

impl Scenario {
    fn parse(value: &str) -> Result<Self, Failure> {
        Ok(match value {
            "plain" => Self::Plain,
            "alloy" => Self::Alloy,
            "alloy-logs" => Self::AlloyLogs,
            "alloy-logs-fmt" => Self::AlloyLogsFmt,
            "otel-sampled" => Self::OtelSampled,
            "otel-unsampled" => Self::OtelUnsampled,
            "otel-unreachable" => Self::OtelUnreachable,
            other => return Err(format!("unknown scenario {other}").into()),
        })
    }
}

#[derive(Debug)]
struct DiscardExporter;

impl SpanExporter for DiscardExporter {
    #[allow(clippy::manual_async_fn)]
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        async move {
            drop(batch);
            Ok(())
        }
    }
}

struct Args {
    scenario: Scenario,
    payload: String,
    connections: usize,
    seconds: u64,
    warmup: u64,
    http2: bool,
}

fn args() -> Result<Args, Failure> {
    let mut args = Args {
        scenario: Scenario::Alloy,
        payload: "small".into(),
        connections: 32,
        seconds: 10,
        warmup: 2,
        http2: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        if flag == "--http2" {
            args.http2 = true;
            continue;
        }
        let value = iter.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--scenario" => args.scenario = Scenario::parse(&value)?,
            "--payload" => args.payload = value,
            "--connections" => args.connections = value.parse()?,
            "--seconds" => args.seconds = value.parse()?,
            "--warmup" => args.warmup = value.parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    Ok(args)
}

fn router() -> Router {
    let large = Bytes::from(vec![b'x'; 64 * 1024]);
    Router::new()
        .route(
            "/small",
            get(|| async { axum::Json(serde_json::json!({ "id": 42, "name": "tea", "quantity": 2, "tags": ["a", "b"] })) }),
        )
        .route("/large", get(move || {
            let large = large.clone();
            async move { large }
        }))
}

fn resource() -> ServiceResource {
    ServiceResource {
        name: "bench".into(),
        version: None,
        instance_id: None,
        environment: None,
    }
}

/// Starts the server on its own runtime and returns its address.
fn start_server(
    scenario: Scenario,
) -> Result<(std::net::SocketAddr, std::thread::JoinHandle<()>), Failure> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = tx.send(Err(error.to_string()));
                return;
            }
        };
        runtime.block_on(async move {
            let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = tx.send(Err(error.to_string()));
                    return;
                }
            };
            let addr = listener.local_addr().map_err(|e| e.to_string());
            let _ = tx.send(addr);
            let metrics = Arc::new(Metrics::default());
            let otlp = |ratio: f64| OtlpConfig {
                enabled: true,
                sampling_ratio: ratio,
                ..OtlpConfig::default()
            };
            // The subscriber is installed per scenario on this runtime's threads
            // through a global default (one scenario per process).
            let pipeline = match scenario {
                Scenario::OtelSampled => OtelPipeline::with_exporter(
                    &resource(),
                    &otlp(1.0),
                    Arc::clone(&metrics),
                    || Ok(DiscardExporter),
                )
                .ok(),
                Scenario::OtelUnsampled => OtelPipeline::with_exporter(
                    &resource(),
                    &otlp(0.0),
                    Arc::clone(&metrics),
                    || Ok(DiscardExporter),
                )
                .ok(),
                Scenario::OtelUnreachable => OtelPipeline::otlp(
                    &resource(),
                    &OtlpConfig {
                        endpoint: Some("http://127.0.0.1:9/v1/traces".into()),
                        timeout_ms: 200,
                        max_export_retries: 0,
                        ..otlp(1.0)
                    },
                    Arc::clone(&metrics),
                )
                .ok(),
                _ => None,
            };
            match (&pipeline, scenario) {
                (Some(pipeline), _) => {
                    let _ = tracing::subscriber::set_global_default(
                        tracing_subscriber::registry().with(pipeline.layer()),
                    );
                }
                (None, Scenario::AlloyLogs) => {
                    // `ferrum_alloy::telemetry::init::fmt_layer` for
                    // `LogFormat::Json`, writing to a sink.
                    let layer = JsonLayer::new()
                        .with_writer(std::io::sink)
                        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
                    let _ = tracing::subscriber::set_global_default(
                        tracing_subscriber::registry().with(layer),
                    );
                }
                (None, Scenario::AlloyLogsFmt) => {
                    // The same line layout from tracing-subscriber's formatter.
                    let layer = tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .json()
                        .with_current_span(true)
                        .with_span_list(false)
                        .flatten_event(true)
                        .with_writer(std::io::sink)
                        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
                    let _ = tracing::subscriber::set_global_default(
                        tracing_subscriber::registry().with(layer),
                    );
                }
                (None, Scenario::Alloy) => {
                    let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
                }
                _ => {}
            }
            if scenario == Scenario::Plain {
                let _ = axum::serve(listener, router()).await;
                return;
            }
            let mut config = AlloyConfig::default();
            config.management.enabled = false;
            config.server.max_connections = 100_000;
            let parts = match AlloyApp::new("bench")
                .router(router())
                .config(config)
                .telemetry(TelemetryInit::ApplicationOwned)
                .shutdown_signal(std::future::pending())
                .into_parts()
            {
                Ok(parts) => parts,
                Err(error) => {
                    eprintln!("server: {error}");
                    return;
                }
            };
            let _ = parts.serve_on(listener, None).await;
            drop(pipeline);
        });
    });
    let addr = rx.recv()?.map_err(|e| -> Failure { e.into() })?;
    Ok((addr, handle))
}

#[derive(Default)]
struct Totals {
    latencies_us: Vec<u32>,
    errors: u64,
}

async fn drive(args: &Args, addr: std::net::SocketAddr) -> Result<(Totals, Duration), Failure> {
    let mut builder = Client::builder(TokioExecutor::new());
    builder.pool_max_idle_per_host(args.connections);
    if args.http2 {
        builder.http2_only(true);
    }
    let client: Client<_, Empty<Bytes>> = builder.build_http();
    let uri: http::Uri = format!("http://{addr}/{}", args.payload).parse()?;

    // Warm up (connections, caches, JIT-free but allocator warm-up).
    let warm_until = Instant::now() + Duration::from_secs(args.warmup);
    let mut warm = Vec::new();
    for _ in 0..args.connections {
        let (client, uri) = (client.clone(), uri.clone());
        warm.push(tokio::spawn(async move {
            while Instant::now() < warm_until {
                if let Ok(response) = client.get(uri.clone()).await {
                    let _ = response.into_body().collect().await;
                }
            }
        }));
    }
    for task in warm {
        task.await?;
    }

    let started = Instant::now();
    let until = started + Duration::from_secs(args.seconds);
    let mut tasks = Vec::new();
    for _ in 0..args.connections {
        let (client, uri) = (client.clone(), uri.clone());
        tasks.push(tokio::spawn(async move {
            let mut totals = Totals::default();
            while Instant::now() < until {
                let begin = Instant::now();
                match client.get(uri.clone()).await {
                    Ok(response) if response.status().is_success() => {
                        match response.into_body().collect().await {
                            Ok(_) => totals.latencies_us.push(
                                u32::try_from(begin.elapsed().as_micros()).unwrap_or(u32::MAX),
                            ),
                            Err(_) => totals.errors += 1,
                        }
                    }
                    _ => totals.errors += 1,
                }
            }
            totals
        }));
    }
    let mut all = Totals::default();
    for task in tasks {
        let totals = task.await?;
        all.latencies_us.extend(totals.latencies_us);
        all.errors += totals.errors;
    }
    Ok((all, started.elapsed()))
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn main() -> Result<(), Failure> {
    let args = args()?;
    let (addr, _server) = start_server(args.scenario)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let (mut totals, elapsed) = runtime.block_on(drive(&args, addr))?;
    totals.latencies_us.sort_unstable();
    let requests = totals.latencies_us.len();
    let result = serde_json::json!({
        "scenario": format!("{:?}", args.scenario),
        "payload": args.payload,
        "http": if args.http2 { "h2c" } else { "http/1.1" },
        "connections": args.connections,
        "seconds": elapsed.as_secs_f64(),
        "requests": requests,
        "errors": totals.errors,
        "requests_per_second": requests as f64 / elapsed.as_secs_f64(),
        "p50_us": percentile(&totals.latencies_us, 0.50),
        "p95_us": percentile(&totals.latencies_us, 0.95),
        "p99_us": percentile(&totals.latencies_us, 0.99),
        "max_us": totals.latencies_us.last().copied().unwrap_or(0),
    });
    println!("{result}");
    std::process::exit(0);
}
