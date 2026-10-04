//! One matrix cell: start the server, drive load, and report one result.

use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ferrum_alloy::telemetry::json::JsonLayer;
use ferrum_alloy::telemetry::metrics::{Metrics, TELEMETRY_LOSS_REASONS};
use ferrum_alloy::telemetry::otel::{OtelPipeline, OtlpConfig, ServiceResource};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use serde_json::{Map, Value, json};
use tracing::Dispatch;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry, registry};

use crate::Failure;
use crate::alloc::{self, Role};
use crate::client::{self, Load, Measured, Target};
use crate::dims::{Cell, Dimension, Scenario};
use crate::pki::Pki;
use crate::probe::{self, CLIENT_THREAD, CpuSnapshot};
use crate::server::{self, CollectorStats};

/// Version of the result format. Bump it when a field changes meaning or is
/// removed; adding fields is compatible.
pub(crate) const SCHEMA: &str = "alloy-bench/1";

/// Options shared by every cell of a run.
#[derive(Debug, Clone)]
pub(crate) struct RunOptions {
    pub(crate) load: Load,
    pub(crate) alloc_counting: bool,
    pub(crate) label: Option<String>,
    /// Identifies one invocation: every result of a `matrix` shares it.
    pub(crate) run_id: String,
    /// An external OTLP/HTTP traces endpoint for `otel-collector`, instead of
    /// the in-process stub.
    pub(crate) collector_endpoint: Option<String>,
    /// Repetition index, set by `matrix`.
    pub(crate) rep: Option<u32>,
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

fn resource() -> ServiceResource {
    ServiceResource {
        name: "bench".into(),
        version: None,
        instance_id: None,
        environment: None,
    }
}

fn otlp(sampling_ratio: f64) -> OtlpConfig {
    let mut config = OtlpConfig::default();
    config.enabled = true;
    config.sampling_ratio = sampling_ratio;
    config
}

/// JSON access logs formatted by tracing-subscriber's JSON `fmt` layer, written to a sink.
fn json_logs() -> impl Layer<Registry> + Send + Sync {
    tracing_subscriber::fmt::layer()
        .with_target(true)
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .flatten_event(true)
        .with_writer(std::io::sink)
        .with_filter(LevelFilter::INFO)
}

/// Starts the scenario's OpenTelemetry pipeline, if any, and installs its
/// global subscriber. A process runs exactly one scenario.
pub(crate) fn install_telemetry(
    scenario: Scenario,
    endpoint: Option<String>,
    metrics: &Arc<Metrics>,
) -> Result<Option<OtelPipeline>, Failure> {
    let metrics = Arc::clone(metrics);
    let pipeline = match scenario {
        Scenario::OtelSampled => Some(OtelPipeline::with_exporter(
            &resource(),
            &otlp(1.0),
            metrics,
            || Ok(DiscardExporter),
        )?),
        Scenario::OtelUnsampled => Some(OtelPipeline::with_exporter(
            &resource(),
            &otlp(0.0),
            metrics,
            || Ok(DiscardExporter),
        )?),
        Scenario::OtelUnreachable => {
            let mut config = otlp(1.0);
            config.endpoint = Some("http://127.0.0.1:9/v1/traces".into());
            config.timeout_ms = 200;
            config.max_export_retries = 0;
            Some(OtelPipeline::otlp(&resource(), &config, metrics)?)
        }
        Scenario::OtelCollector => {
            let mut config = otlp(1.0);
            config.endpoint = endpoint;
            Some(OtelPipeline::otlp(&resource(), &config, metrics)?)
        }
        Scenario::Plain
        | Scenario::Alloy
        | Scenario::AlloyLogs
        | Scenario::AlloyLogsFmt
        | Scenario::AlloyDiagnostics => None,
    };
    let dispatch = match (&pipeline, scenario) {
        (Some(pipeline), _) => Some(Dispatch::new(registry().with(pipeline.layer()))),
        (None, Scenario::AlloyLogs) => Some(Dispatch::new(
            registry().with(
                JsonLayer::new()
                    .with_writer(std::io::sink)
                    .with_filter(LevelFilter::INFO),
            ),
        )),
        (None, Scenario::AlloyLogsFmt) => Some(Dispatch::new(registry().with(json_logs()))),
        (None, Scenario::Alloy | Scenario::AlloyDiagnostics) => Some(Dispatch::new(registry())),
        (None, _) => None,
    };
    let installed = match dispatch {
        Some(dispatch) => tracing::dispatcher::set_global_default(dispatch),
        None => Ok(()),
    };
    if let Err(error) = installed {
        let message = format!("cannot install the scenario's subscriber: {error}");
        return Err(message.into());
    }
    Ok(pipeline)
}

/// Runs one cell in this process and returns its result line.
pub(crate) fn run_cell(cell: Cell, options: &RunOptions) -> Result<Value, Failure> {
    if options.alloc_counting {
        alloc::enable();
    }
    let environment = probe::environment(options.label.as_deref());
    let metrics = Arc::new(Metrics::default());
    let collector = match (cell.scenario, &options.collector_endpoint) {
        (Scenario::OtelCollector, None) => Some(server::start_collector()?),
        _ => None,
    };
    let endpoint = match &collector {
        Some(collector) => Some(collector.endpoint.clone()),
        None => options.collector_endpoint.clone(),
    };
    let pipeline = install_telemetry(cell.scenario, endpoint, &metrics)?;
    let stats = collector.as_ref().map(|c| Arc::clone(&c.stats));
    let result = measure(cell, options, pipeline, &metrics, stats, environment);
    drop(collector);
    result
}

/// Counters read at the start and end of the measurement window.
struct Sample {
    cpu: Option<CpuSnapshot>,
    allocations: [alloc::Counts; Role::ALL.len()],
    spans_exported: u64,
    spans_lost: Vec<u64>,
    collector_requests: u64,
    collector_bytes: u64,
}

impl Sample {
    fn take(metrics: &Metrics, collector: Option<&CollectorStats>) -> Self {
        Self {
            cpu: CpuSnapshot::take(),
            allocations: alloc::snapshot(),
            spans_exported: metrics.telemetry_spans_exported.load(Ordering::Relaxed),
            spans_lost: TELEMETRY_LOSS_REASONS
                .iter()
                .map(|reason| metrics.telemetry_spans_lost.get(reason))
                .collect(),
            collector_requests: collector.map_or(0, |c| c.requests.load(Ordering::Relaxed)),
            collector_bytes: collector.map_or(0, |c| c.bytes.load(Ordering::Relaxed)),
        }
    }
}

/// Starts the server (with `pipeline` kept alive by it), drives the load,
/// and assembles the result. Separate from [`run_cell`] so tests can run
/// cells without installing a global subscriber.
pub(crate) fn measure(
    cell: Cell,
    options: &RunOptions,
    pipeline: Option<OtelPipeline>,
    metrics: &Metrics,
    collector: Option<Arc<CollectorStats>>,
    environment: Value,
) -> Result<Value, Failure> {
    options.load.validate(cell.transport)?;
    let pki = if cell.transport.tls() {
        Some(Pki::generate()?)
    } else {
        None
    };
    let server_tls = match &pki {
        Some(pki) => Some(pki.server(cell.transport.mtls())?),
        None => None,
    };
    let server = server::start(cell.scenario, server_tls, pipeline)?;
    let target = Target {
        addr: server.addr,
        transport: cell.transport,
        workload: cell.workload,
        tls: match &pki {
            Some(pki) => Some(pki.client(cell.transport)?),
            None => None,
        },
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(server::THREADS)
        .thread_name(CLIENT_THREAD)
        .on_thread_start(|| alloc::set_role(Role::Client))
        .enable_all()
        .build()?;
    let collector = collector.as_deref();
    let probe = || Sample::take(metrics, collector);
    alloc::set_role(Role::Client);
    let measured = runtime.block_on(client::drive(target, options.load, probe));
    alloc::set_role(Role::Service);
    // Close the client's connections before stopping the server.
    drop(runtime);
    drop(server);
    drop(pki);
    Ok(report(cell, options, environment, measured?))
}

/// A random identifier for one invocation, formatted as a version 4 UUID.
pub(crate) fn new_run_id() -> Result<String, Failure> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("cannot generate a run id: {error}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut id = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

/// The commit under test, when GitHub Actions names it.
fn commit() -> Option<String> {
    std::env::var("GITHUB_SHA")
        .ok()
        .filter(|sha| !sha.is_empty())
}

/// Nearest-rank percentile: the value at 1-based rank `ceil(p * n)`.
fn percentile(sorted: &[u32], p: f64) -> Option<u32> {
    let last = sorted.len().checked_sub(1)?;
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1).min(last)).copied()
}

/// Growth of a monotonic counter between two samples.
fn delta(start: u64, end: u64) -> u64 {
    end.saturating_sub(start)
}

fn per_request(total: u64, requests: u64, scale: f64) -> Option<f64> {
    if requests == 0 {
        return None;
    }
    Some(total as f64 / scale / requests as f64)
}

fn report(
    cell: Cell,
    options: &RunOptions,
    environment: Value,
    measured: Measured<Sample>,
) -> Value {
    let Measured {
        mut totals,
        window,
        start,
        end,
    } = measured;
    totals.latencies_us.sort_unstable();
    let latencies = &totals.latencies_us;
    let requests = latencies.len() as u64;
    let load = options.load;
    let transport = cell.transport;

    let cpu = match (&start.cpu, &end.cpu) {
        (Some(start), Some(end)) => {
            let [service_ns, client_ns, collector_ns] = end.since(start);
            json!({
                "scope": "threads",
                "service_ns": service_ns,
                "client_ns": client_ns,
                "collector_ns": collector_ns,
                "service_us_per_request": per_request(service_ns, requests, 1_000.0),
                "client_us_per_request": per_request(client_ns, requests, 1_000.0),
            })
        }
        _ => Value::Null,
    };

    let allocations = if alloc::enabled() {
        let mut by_role = Map::new();
        for role in Role::ALL {
            let index = role as usize;
            let delta = end.allocations[index].since(start.allocations[index]);
            by_role.insert(
                role.name().to_owned(),
                json!({
                    "calls": delta.calls,
                    "bytes": delta.bytes,
                    "calls_per_request": per_request(delta.calls, requests, 1.0),
                    "bytes_per_request": per_request(delta.bytes, requests, 1.0),
                }),
            );
        }
        Value::Object(by_role)
    } else {
        Value::Null
    };

    let otel = if cell.scenario.otel() {
        let mut lost = Map::new();
        for (index, reason) in TELEMETRY_LOSS_REASONS.iter().enumerate() {
            let before = start.spans_lost.get(index).copied().unwrap_or(0);
            let after = end.spans_lost.get(index).copied().unwrap_or(0);
            lost.insert((*reason).to_owned(), json!(delta(before, after)));
        }
        let stub = cell.scenario == Scenario::OtelCollector && options.collector_endpoint.is_none();
        let collector_requests = delta(start.collector_requests, end.collector_requests);
        let collector_bytes = delta(start.collector_bytes, end.collector_bytes);
        json!({
            "spans_exported": delta(start.spans_exported, end.spans_exported),
            "spans_lost": lost,
            "collector_requests": stub.then_some(collector_requests),
            "collector_bytes": stub.then_some(collector_bytes),
        })
    } else {
        Value::Null
    };

    json!({
        "schema": SCHEMA,
        "run_id": options.run_id,
        "commit": commit(),
        "rep": options.rep,
        "scenario": cell.scenario.name(),
        "workload": cell.workload.name(),
        "transport": transport.name(),
        "protocol": transport.protocol(),
        "tls": transport.tls(),
        "mtls": transport.mtls(),
        "concurrency": load.concurrency,
        "connections": load.connections(transport),
        "streams_per_connection": load.streams_per_connection(transport),
        "server_threads": server::THREADS,
        "client_threads": server::THREADS,
        "warmup_seconds": load.warmup.as_secs_f64(),
        "seconds": window.as_secs_f64(),
        "alloc_counting": options.alloc_counting,
        "requests": requests,
        "errors": totals.errors,
        "error_samples": totals.error_samples,
        "connects": totals.connects,
        "body_bytes": totals.body_bytes,
        "requests_per_second": requests as f64 / window.as_secs_f64().max(f64::EPSILON),
        "latency_us": {
            "p50": percentile(latencies, 0.50),
            "p90": percentile(latencies, 0.90),
            "p99": percentile(latencies, 0.99),
            "p999": percentile(latencies, 0.999),
            "max": latencies.last().copied(),
        },
        "cpu": cpu,
        "memory": probe::memory(),
        "allocations": allocations,
        "otel": otel,
        "environment": environment,
    })
}

/// A duration from a command-line number of seconds.
pub(crate) fn seconds(value: &str) -> Result<Duration, Failure> {
    let seconds: f64 = value.parse()?;
    Duration::try_from_secs_f64(seconds).map_err(|_| format!("invalid seconds {value:?}").into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use crate::dims::{
        CANCEL_FRAMES, FRAME_BYTES, LARGE_BYTES, STREAM_FRAMES, Transport, Workload,
    };

    fn options(seconds: f64) -> RunOptions {
        RunOptions {
            load: Load {
                concurrency: 4,
                streams: 2,
                warmup: Duration::ZERO,
                duration: Duration::from_secs_f64(seconds),
            },
            alloc_counting: false,
            label: Some("test".into()),
            run_id: "test-run".into(),
            collector_endpoint: None,
            rep: Some(1),
        }
    }

    fn run(scenario: Scenario, workload: Workload, transport: Transport) -> Value {
        let cell = Cell {
            scenario,
            workload,
            transport,
        };
        let metrics = Metrics::default();
        let environment = probe::environment(None);
        measure(cell, &options(0.2), None, &metrics, None, environment).unwrap()
    }

    fn assert_healthy(result: &Value) {
        assert_work_completed(result);
        assert_eq!(result["seconds"], 0.2, "{result}");
    }

    fn assert_work_completed(result: &Value) {
        assert_eq!(result["schema"], SCHEMA, "{result}");
        assert!(result["requests"].as_u64().unwrap() > 0, "{result}");
        assert_eq!(result["errors"], 0, "{result}");
        assert_eq!(result["error_samples"], json!([]), "{result}");
        for percentile in ["p50", "p90", "p99", "p999", "max"] {
            assert!(
                result["latency_us"][percentile].as_u64().unwrap() > 0,
                "{result}"
            );
        }
    }

    #[test]
    fn plain_serves_every_workload_over_every_transport() {
        for transport in Transport::ALL {
            for workload in Workload::ALL {
                let result = run(Scenario::Plain, *workload, *transport);
                assert_healthy(&result);
                assert_eq!(result["transport"], transport.name());
                assert_eq!(result["workload"], workload.name());
            }
        }
    }

    #[test]
    fn alloy_serves_every_workload_over_every_transport() {
        for transport in Transport::ALL {
            for workload in Workload::ALL {
                assert_healthy(&run(Scenario::Alloy, *workload, *transport));
            }
        }
    }

    #[test]
    fn http1_cancellation_reconnects_and_http2_does_not() {
        for transport in Transport::ALL {
            let metrics = Metrics::default();
            let probe = || Sample::take(&metrics, None);
            let measured = client::tests::cancellation_rounds(*transport, probe);
            let mut options = options(0.2);
            options.load.duration = measured.window;
            let cell = Cell {
                scenario: Scenario::Plain,
                workload: Workload::Cancel,
                transport: *transport,
            };
            let result = report(cell, &options, probe::environment(None), measured);
            assert_work_completed(&result);
            assert_eq!(result["requests"], options.load.concurrency, "{result}");
            assert_eq!(result["seconds"], options.load.duration.as_secs_f64());
            assert_cancellation(&result, *transport);
        }
    }

    fn assert_cancellation(result: &Value, transport: Transport) {
        let requests = result["requests"].as_u64().unwrap();
        let bytes = result["body_bytes"].as_u64().unwrap();
        assert!(bytes >= requests * FRAME_BYTES as u64, "{result}");
        assert!(
            bytes < requests * (CANCEL_FRAMES * FRAME_BYTES) as u64,
            "{result}"
        );
        if transport.http2() {
            assert_eq!(result["connects"], result["connections"], "{result}");
        } else {
            let connections = result["connections"].as_u64().unwrap();
            let connects = result["connects"].as_u64().unwrap();
            assert!(connects >= connections + requests, "{result}");
        }
    }

    #[test]
    fn cancellation_remains_healthy_after_normal_warmup() {
        for transport in [Transport::H2c, Transport::H2Mtls, Transport::H1Mtls] {
            let cell = Cell {
                scenario: Scenario::Plain,
                workload: Workload::Cancel,
                transport,
            };
            let mut options = options(0.2);
            options.load.warmup = Duration::from_millis(100);
            let metrics = Metrics::default();
            let environment = probe::environment(None);
            let result = measure(cell, &options, None, &metrics, None, environment).unwrap();
            assert_healthy(&result);
            assert_cancellation(&result, transport);
            assert_eq!(result["warmup_seconds"], 0.1, "{result}");
        }
    }

    #[test]
    fn streamed_and_large_bodies_are_read_in_full() {
        let cases = [
            (Workload::Stream, Transport::H1, STREAM_FRAMES * FRAME_BYTES),
            (Workload::Large, Transport::H2c, LARGE_BYTES),
        ];
        for (workload, transport, size) in cases {
            let result = run(Scenario::Plain, workload, transport);
            let requests = result["requests"].as_u64().unwrap();
            let body_bytes = result["body_bytes"].as_u64().unwrap();
            assert_eq!(body_bytes, requests * size as u64, "{result}");
        }
    }

    /// The result's keys, pinned: renaming or removing one changes what
    /// `alloy-bench/1` means, so it needs a schema bump, not just this list.
    #[test]
    fn result_keys_are_pinned() {
        let result = run(Scenario::Plain, Workload::Small, Transport::H1);
        let mut keys: Vec<&str> = result
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let expected = [
            "alloc_counting",
            "allocations",
            "body_bytes",
            "client_threads",
            "commit",
            "concurrency",
            "connections",
            "connects",
            "cpu",
            "environment",
            "error_samples",
            "errors",
            "latency_us",
            "memory",
            "mtls",
            "otel",
            "protocol",
            "rep",
            "requests",
            "requests_per_second",
            "run_id",
            "scenario",
            "schema",
            "seconds",
            "server_threads",
            "streams_per_connection",
            "tls",
            "transport",
            "warmup_seconds",
            "workload",
        ];
        assert_eq!(keys, expected, "{result}");
        let latency = result["latency_us"].as_object().unwrap();
        let mut latency: Vec<&str> = latency.keys().map(String::as_str).collect();
        latency.sort_unstable();
        assert_eq!(latency, ["max", "p50", "p90", "p99", "p999"], "{result}");
        assert_eq!(result["run_id"], "test-run", "{result}");
        assert_eq!(result["alloc_counting"], false, "{result}");
        assert_eq!(result["error_samples"], json!([]), "{result}");
    }

    #[test]
    fn run_ids_are_random_version_4_uuids() {
        let id = new_run_id().unwrap();
        assert_eq!(id.len(), 36, "{id}");
        let groups: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12], "{id}");
        assert_eq!(id.as_bytes()[14], b'4', "{id}");
        assert!(
            id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
            "{id}"
        );
        assert_ne!(id, new_run_id().unwrap());
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[7], 0.99), Some(7));
        let sorted: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 0.0), Some(1));
        assert_eq!(percentile(&sorted, 0.5), Some(50));
        assert_eq!(percentile(&sorted, 0.99), Some(99));
        assert_eq!(percentile(&sorted, 1.0), Some(100));
        let ten: Vec<u32> = (1..=10).collect();
        assert_eq!(percentile(&ten, 0.9), Some(9));
        assert_eq!(percentile(&ten, 0.95), Some(10));
        assert_eq!(percentile(&ten, 0.999), Some(10));
    }

    #[test]
    fn seconds_reject_negative_and_non_numeric_values() {
        assert_eq!(seconds("1.5").unwrap(), Duration::from_millis(1500));
        assert!(seconds("-1").is_err());
        assert!(seconds("soon").is_err());
    }
}
