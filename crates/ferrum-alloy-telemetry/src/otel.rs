//! OpenTelemetry bridge with bounded, asynchronous OTLP/HTTP export.
//!
//! * Spans are queued by a [`BoundedSpanProcessor`] with a span-count limit
//!   and a byte budget; spans that do not fit are dropped and counted by
//!   reason. A collector outage never blocks or fails a business request.
//! * Export runs on one dedicated OS thread with tracing disabled for that
//!   thread, so exporter internals cannot instrument themselves recursively.
//! * The OTLP exporter is built on that thread, which keeps its blocking HTTP
//!   client out of the async runtime.
//! * Spans, resource updates, flushes, and shutdowns are handled in queue
//!   order; a flush or shutdown never discards the messages queued behind it,
//!   though a shutdown handles at most one further queue's worth of them.
//! * Configuration errors (bad endpoint, invalid ratio) fail at startup.
//!
//! Component versions: `opentelemetry`/`opentelemetry_sdk`/`opentelemetry-otlp`
//! 0.33 and `tracing-opentelemetry` 0.34. The OpenTelemetry Rust tracing API
//! and SDK are documented upstream as beta; see `docs/compatibility.md`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{Array, Context, KeyValue, Value};
use opentelemetry_otlp::{Protocol, RetryPolicy, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{
    Sampler, SdkTracer, SdkTracerProvider, Span, SpanData, SpanExporter, SpanProcessor,
};
use serde::{Deserialize, Serialize};
use tracing::Subscriber;
use tracing_subscriber::registry::LookupSpan;

use crate::metrics::Metrics;

const MAX_OTLP_ENDPOINT_LENGTH: usize = 2_048;

/// OTLP trace export configuration.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct OtlpConfig {
    /// Export spans. Off by default: a service works without a collector.
    pub enabled: bool,
    /// Full OTLP/HTTP traces URL, e.g. `http://127.0.0.1:4318/v1/traces`.
    /// When unset, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, then
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` (+ `/v1/traces`), then the OTLP default
    /// `http://localhost:4318/v1/traces` apply.
    pub endpoint: Option<String>,
    /// Per-request export timeout in milliseconds. It also bounds when a
    /// retry may start and how long its backoff may be, but an in-flight
    /// retry is not cancelled when that budget ends, so one export call can
    /// take up to roughly twice this long in the worst case.
    pub timeout_ms: u64,
    /// Retries after the first attempt for retryable failures.
    pub max_export_retries: usize,
    /// Ratio of new root traces to sample, `0.0..=1.0`. Accepted remote
    /// parents keep their own sampling decision (parent-based).
    pub sampling_ratio: f64,
    /// Maximum queued spans.
    pub max_queue_spans: usize,
    /// Maximum estimated queued bytes.
    pub max_queue_bytes: usize,
    /// Maximum spans per export request.
    pub max_export_batch: usize,
    /// Maximum encoded bytes per export request.
    pub max_request_bytes: usize,
    /// Delay between scheduled exports, in milliseconds.
    pub scheduled_delay_ms: u64,
}

impl fmt::Debug for OtlpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<configured>"))
            .field("timeout_ms", &self.timeout_ms)
            .field("max_export_retries", &self.max_export_retries)
            .field("sampling_ratio", &self.sampling_ratio)
            .field("max_queue_spans", &self.max_queue_spans)
            .field("max_queue_bytes", &self.max_queue_bytes)
            .field("max_export_batch", &self.max_export_batch)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("scheduled_delay_ms", &self.scheduled_delay_ms)
            .finish()
    }
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            timeout_ms: 10_000,
            max_export_retries: 2,
            sampling_ratio: 1.0,
            max_queue_spans: 2_048,
            max_queue_bytes: 8 * 1024 * 1024,
            max_export_batch: 512,
            max_request_bytes: 4 * 1024 * 1024,
            scheduled_delay_ms: 1_000,
        }
    }
}

/// Resource identity attached to every exported span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceResource {
    /// `service.name`.
    pub name: String,
    /// `service.version`.
    pub version: Option<String>,
    /// `service.instance.id`. Generated per process when `None`.
    pub instance_id: Option<String>,
    /// `deployment.environment.name`.
    pub environment: Option<String>,
}

/// Why the OpenTelemetry pipeline could not start.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    /// Invalid configuration value.
    #[error("invalid OpenTelemetry configuration: {0}")]
    Config(String),
    /// The exporter could not be built.
    #[error("OTLP exporter could not be built: {0}")]
    Exporter(String),
}

impl OtlpConfig {
    /// Validates value ranges.
    pub fn validate(&self) -> Result<(), OtelError> {
        if !(0.0..=1.0).contains(&self.sampling_ratio) || self.sampling_ratio.is_nan() {
            return Err(OtelError::Config(format!(
                "sampling_ratio must be within 0.0..=1.0, got {}",
                self.sampling_ratio
            )));
        }
        for (name, value) in [
            ("max_queue_spans", self.max_queue_spans),
            ("max_queue_bytes", self.max_queue_bytes),
            ("max_export_batch", self.max_export_batch),
            ("max_request_bytes", self.max_request_bytes),
        ] {
            if value == 0 {
                return Err(OtelError::Config(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        if self.max_export_batch > self.max_queue_spans {
            return Err(OtelError::Config(
                "max_export_batch must not exceed max_queue_spans".into(),
            ));
        }
        if self.timeout_ms == 0 || self.scheduled_delay_ms == 0 {
            return Err(OtelError::Config(
                "timeout_ms and scheduled_delay_ms must be greater than zero".into(),
            ));
        }
        if let Some(endpoint) = &self.endpoint {
            validate_endpoint(endpoint)?;
        }
        Ok(())
    }
}

/// A running OpenTelemetry pipeline. Keep it alive for the life of the
/// service and call [`OtelPipeline::shutdown`] to flush.
pub struct OtelPipeline {
    provider: SdkTracerProvider,
    tracer: SdkTracer,
    queued_spans: Arc<AtomicUsize>,
    queued_bytes: Arc<AtomicUsize>,
}

impl fmt::Debug for OtelPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtelPipeline").finish_non_exhaustive()
    }
}

/// Instrumentation scope name for spans created through this pipeline.
pub const SCOPE_NAME: &str = "ferrum-alloy-telemetry";

impl OtelPipeline {
    /// Starts an OTLP/HTTP (protobuf) pipeline.
    pub fn otlp(
        resource: &ServiceResource,
        config: &OtlpConfig,
        metrics: Arc<Metrics>,
    ) -> Result<Self, OtelError> {
        config.validate()?;
        let endpoint = config.endpoint.clone();
        let timeout = Duration::from_millis(config.timeout_ms);
        let retries = config.max_export_retries;
        let max_request_bytes = config.max_request_bytes;
        let https = endpoint
            .as_deref()
            .is_some_and(|e| e.starts_with("https://"));
        let factory = move || {
            if endpoint.is_none() {
                validate_environment_endpoint()?;
            }
            // Build the HTTP client explicitly (on the export thread): the
            // exporter's implicit client depends on how reqwest's TLS
            // features unify across the application and can panic.
            let tls = otlp_tls(https)?;
            let client = reqwest::blocking::Client::builder()
                .tls_backend_preconfigured(tls)
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("OTLP HTTP client: {e}"))?;
            let mut builder = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_http_client(client)
                .with_protocol(Protocol::HttpBinary)
                .with_timeout(timeout)
                .with_retry_policy(RetryPolicy::recommended().with_max_retries(retries))
                .with_max_request_body_size(max_request_bytes);
            if let Some(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build().map_err(|_| {
                "the OTLP exporter could not be built from its endpoint configuration \
                 (otlp.endpoint or the OTEL_EXPORTER_OTLP_*ENDPOINT variables)"
                    .to_owned()
            })
        };
        Self::with_exporter(resource, config, metrics, factory)
    }

    /// Starts a pipeline with a custom exporter factory. The factory runs on
    /// the export thread.
    pub fn with_exporter<E, F>(
        resource: &ServiceResource,
        config: &OtlpConfig,
        metrics: Arc<Metrics>,
        factory: F,
    ) -> Result<Self, OtelError>
    where
        E: SpanExporter + 'static,
        F: FnOnce() -> Result<E, String> + Send + 'static,
    {
        config.validate()?;
        let processor = BoundedSpanProcessor::spawn(factory, config, metrics)?;
        let (queued_spans, queued_bytes) = processor.queue_counters();
        let instance_id = resource
            .instance_id
            .clone()
            .unwrap_or_else(|| crate::request_id::RequestId::generate().as_str().to_owned());
        let mut attributes = vec![KeyValue::new("service.instance.id", instance_id)];
        if let Some(version) = &resource.version {
            attributes.push(KeyValue::new("service.version", version.clone()));
        }
        if let Some(environment) = &resource.environment {
            attributes.push(KeyValue::new(
                "deployment.environment.name",
                environment.clone(),
            ));
        }
        let provider = SdkTracerProvider::builder()
            .with_span_processor(processor)
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                config.sampling_ratio,
            ))))
            .with_resource(
                Resource::builder()
                    .with_service_name(resource.name.clone())
                    .with_attributes(attributes)
                    .build(),
            )
            .build();
        let tracer = provider.tracer(SCOPE_NAME);
        Ok(Self {
            provider,
            tracer,
            queued_spans,
            queued_bytes,
        })
    }

    /// A `tracing` layer that exports spans through this pipeline. Spans and
    /// events from the telemetry stack's own dependencies are filtered out.
    pub fn layer<S>(&self) -> impl tracing_subscriber::Layer<S> + Send + Sync + 'static
    where
        S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        use tracing_subscriber::Layer as _;
        tracing_opentelemetry::layer()
            .with_tracer(self.tracer.clone())
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                let target = metadata.target();
                !(target.starts_with("opentelemetry")
                    || target.starts_with("reqwest")
                    || target.starts_with("hyper")
                    || target.starts_with("h2")
                    || target.starts_with("ferrum_alloy::access"))
            }))
    }

    /// Spans accepted by the processor and not yet exported or counted
    /// lost. Load tests and graceful-drain checks can wait for this to
    /// reach zero instead of guessing with sleeps.
    pub fn queued_spans(&self) -> usize {
        self.queued_spans.load(Ordering::Relaxed)
    }

    /// Estimated bytes of the spans currently queued for export.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Relaxed)
    }

    /// Exports everything queued so far (bounded by the processor's flush
    /// timeout).
    pub fn force_flush(&self) -> Result<(), OtelError> {
        self.provider
            .force_flush()
            .map_err(|e| OtelError::Exporter(e.to_string()))
    }

    /// Flushes queued spans and stops the export thread, waiting at most
    /// `timeout`. Spans still queued afterwards are counted as lost.
    pub fn shutdown(self, timeout: Duration) -> Result<(), OtelError> {
        self.provider
            .shutdown_with_timeout(timeout)
            .map_err(|e| OtelError::Exporter(e.to_string()))
    }
}

fn validate_environment_endpoint() -> Result<(), String> {
    for name in [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ] {
        let value = std::env::var_os(name);
        if let Some(value) = value {
            let Some(value) = value.to_str() else {
                return Err("endpoint must be a valid http(s) URL without credentials".into());
            };
            if value.is_empty() {
                continue;
            }
            validate_endpoint(value).map_err(|error| error.to_string())?;
            return Ok(());
        }
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str) -> Result<(), OtelError> {
    if endpoint.len() > MAX_OTLP_ENDPOINT_LENGTH {
        return Err(OtelError::Config(
            "endpoint must not exceed 2048 bytes".into(),
        ));
    }
    let scheme_ok = endpoint.starts_with("http://") || endpoint.starts_with("https://");
    let has_userinfo = endpoint
        .split_once("://")
        .map(|(_, rest)| {
            let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            rest[..authority_end].contains('@')
        })
        .unwrap_or(false);
    if !scheme_ok || has_userinfo || endpoint.parse::<http::Uri>().is_err() {
        return Err(OtelError::Config(
            "endpoint must be a valid http(s) URL without credentials".into(),
        ));
    }
    Ok(())
}

/// TLS for the OTLP client: the platform trust store. A configured
/// `https://` endpoint requires it; otherwise (plain HTTP, or an endpoint
/// from `OTEL_EXPORTER_OTLP_*`) a host without system roots gets an empty
/// store, so HTTPS exports fail and are counted instead of blocking startup.
fn otlp_tls(require_roots: bool) -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("OTLP TLS configuration: {e}"))?;
    match builder.clone().with_platform_verifier() {
        Ok(builder) => Ok(builder.with_no_client_auth()),
        Err(error) if require_roots => Err(format!(
            "OTLP endpoint uses https but the platform trust store is unavailable: {error}"
        )),
        Err(_) => Ok(builder
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth()),
    }
}

/// Returns `true` when spans created now are bridged to OpenTelemetry (the
/// current subscriber includes an OpenTelemetry layer).
pub fn layer_active() -> bool {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let probe =
        tracing::info_span!(target: "ferrum_alloy::probe", parent: None, "alloy.otel.probe");
    probe.context().span().span_context().is_valid()
}

/// Bound on a flush, and on waiting for queue space for a resource update.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Work for the export thread. The channel is FIFO and the worker handles
/// every message in the order it was queued, so a span queued before a
/// flush is exported by that flush and no control message is skipped.
enum Message {
    Span(Box<SpanData>, usize),
    Resource(Resource),
    /// Answered with the exporter's own `force_flush` result.
    Flush(SyncSender<OTelSdkResult>),
    /// Answered with the exporter's own `shutdown` result.
    Shutdown(SyncSender<OTelSdkResult>),
}

/// A copy of an exporter control result for one more caller
/// (`OTelSdkError` is not `Clone`).
fn copy_result(result: &OTelSdkResult) -> OTelSdkResult {
    match result {
        Ok(()) => Ok(()),
        Err(OTelSdkError::AlreadyShutdown) => Err(OTelSdkError::AlreadyShutdown),
        Err(OTelSdkError::Timeout(timeout)) => Err(OTelSdkError::Timeout(*timeout)),
        Err(OTelSdkError::InternalFailure(reason)) => {
            Err(OTelSdkError::InternalFailure(reason.clone()))
        }
    }
}

/// A span processor with bounded memory and exact loss accounting.
pub struct BoundedSpanProcessor {
    sender: SyncSender<Message>,
    queued_bytes: Arc<AtomicUsize>,
    queued_spans: Arc<AtomicUsize>,
    /// Set once the worker has ended; later spans are `shutdown` losses.
    closed: Arc<AtomicBool>,
    max_queue_bytes: usize,
    metrics: Arc<Metrics>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    controls_queued: AtomicUsize,
}

impl fmt::Debug for BoundedSpanProcessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedSpanProcessor")
            .field("queued_spans", &self.queued_spans.load(Ordering::Relaxed))
            .field("queued_bytes", &self.queued_bytes.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Rough encoded size of a span, used for the byte budget.
fn estimate_bytes(span: &SpanData) -> usize {
    // The status description is dynamically owned queued payload (the
    // tracing bridge puts `otel.status_description` there, outside the
    // attributes), so it must be charged like every other payload.
    let status = match &span.status {
        opentelemetry::trace::Status::Error { description } => description.len(),
        _ => 0,
    };
    let attributes = estimate_attributes_bytes(&span.attributes);
    let events: usize = span
        .events
        .iter()
        .map(|event| event.name.len() + estimate_attributes_bytes(&event.attributes) + 16)
        .sum();
    let links: usize = span
        .links
        .iter()
        .map(|link| estimate_attributes_bytes(&link.attributes) + 64)
        .sum();
    96 + span.name.len() + attributes + events + links + status
}

fn estimate_attributes_bytes(attributes: &[KeyValue]) -> usize {
    attributes
        .iter()
        .map(|kv| kv.key.as_str().len() + estimate_value_bytes(&kv.value) + 8)
        .sum()
}

fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::Bool(_) => std::mem::size_of::<bool>(),
        Value::I64(_) | Value::F64(_) => std::mem::size_of::<u64>(),
        Value::String(value) => value.as_str().len(),
        Value::Array(array) => match array {
            Array::Bool(values) => values.len() * std::mem::size_of::<bool>(),
            Array::I64(values) => values.len() * std::mem::size_of::<i64>(),
            Array::F64(values) => values.len() * std::mem::size_of::<f64>(),
            Array::String(values) => values.iter().map(|value| value.as_str().len() + 8).sum(),
            _ => 0,
        },
        _ => 0,
    }
}

impl BoundedSpanProcessor {
    /// Spawns the export thread and builds the exporter on it.
    pub fn spawn<E, F>(
        factory: F,
        config: &OtlpConfig,
        metrics: Arc<Metrics>,
    ) -> Result<Self, OtelError>
    where
        E: SpanExporter + 'static,
        F: FnOnce() -> Result<E, String> + Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(config.max_queue_spans);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let queued_spans = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let worker_state = Worker {
            receiver,
            queued_bytes: Arc::clone(&queued_bytes),
            queued_spans: Arc::clone(&queued_spans),
            closed: Arc::clone(&closed),
            metrics: Arc::clone(&metrics),
            max_batch: config.max_export_batch,
            max_request_bytes: config.max_request_bytes,
            delay: Duration::from_millis(config.scheduled_delay_ms),
            capacity: config.max_queue_spans,
        };
        let handle = thread::Builder::new()
            .name("ferrum-alloy-otlp".into())
            .spawn(move || {
                // Nothing the exporter does may create telemetry about itself.
                let _no_tracing = tracing::dispatcher::set_default(&tracing::Dispatch::none());
                let _suppressed = Context::enter_telemetry_suppressed_scope();
                match factory() {
                    Ok(exporter) => {
                        let _ = ready_tx.send(Ok(()));
                        worker_state.run(exporter);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|e| OtelError::Exporter(format!("could not start export thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = handle.join();
                return Err(OtelError::Exporter(error));
            }
            Err(_) => {
                let _ = handle.join();
                return Err(OtelError::Exporter(
                    "export thread exited during startup".into(),
                ));
            }
        }
        Ok(Self {
            sender,
            queued_bytes,
            queued_spans,
            closed,
            max_queue_bytes: config.max_queue_bytes,
            metrics,
            worker: Mutex::new(Some(handle)),
            controls_queued: AtomicUsize::new(0),
        })
    }

    /// Flush and shutdown requests accepted into the queue so far. A test
    /// observable, not a stable API.
    #[doc(hidden)]
    pub fn controls_queued(&self) -> usize {
        self.controls_queued.load(Ordering::SeqCst)
    }

    /// Spans accepted and not yet exported or counted lost.
    pub fn queued_spans(&self) -> usize {
        self.queued_spans.load(Ordering::Relaxed)
    }

    /// Estimated bytes of the spans currently queued.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn queue_counters(&self) -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
        (
            Arc::clone(&self.queued_spans),
            Arc::clone(&self.queued_bytes),
        )
    }

    /// Queues `message`, waiting for queue space until `deadline`.
    fn enqueue(&self, mut message: Message, deadline: Instant, timeout: Duration) -> OTelSdkResult {
        loop {
            match self.sender.try_send(message) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        return Err(OTelSdkError::Timeout(timeout));
                    }
                    message = returned;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => return Err(OTelSdkError::AlreadyShutdown),
            }
        }
    }

    /// Sends a control message and waits for the worker's answer. `Ok`
    /// carries the exporter's own result once the worker answered; `Err`
    /// means it did not answer (no queue space or no answer in time, or the
    /// worker is gone).
    fn control(
        &self,
        make: fn(SyncSender<OTelSdkResult>) -> Message,
        timeout: Duration,
    ) -> Result<OTelSdkResult, OTelSdkError> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout;
        // Control messages wait for queue space, bounded by the timeout.
        self.enqueue(make(ack_tx), deadline, timeout)?;
        self.controls_queued.fetch_add(1, Ordering::SeqCst);
        let remaining = deadline.saturating_duration_since(Instant::now());
        match ack_rx.recv_timeout(remaining) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => Err(OTelSdkError::Timeout(timeout)),
            // The worker exited without answering: the pipeline is shut down.
            Err(RecvTimeoutError::Disconnected) => Err(OTelSdkError::AlreadyShutdown),
        }
    }
}

/// Releases a byte charge against the queue budget. Saturating: the
/// exiting worker retires every outstanding charge at once
/// (`Worker::drop`), and a producer rolling back afterwards must not
/// wrap the counter.
fn release_bytes(queued_bytes: &AtomicUsize, size: usize) {
    let _ = queued_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(size))
    });
}

/// Releases one span reservation taken in `on_end` that did not reach the
/// worker. Returns `false` when the exiting worker already counted it (it
/// counts every reservation outstanding when it ends), in which case the
/// caller must not count it again. Never wraps the counter.
fn release_span_reservation(queued_spans: &AtomicUsize) -> bool {
    queued_spans
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
        .is_ok()
}

impl SpanProcessor for BoundedSpanProcessor {
    fn on_start(&self, _span: &mut Span, _cx: &Context) {}

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }
        // The worker has ended: the span cannot be queued, and the byte
        // charges it left behind must not turn this into a budget loss.
        if self.closed.load(Ordering::Acquire) {
            self.metrics.telemetry_spans_lost.inc("shutdown");
            return;
        }
        let size = estimate_bytes(&span);
        let previous = self.queued_bytes.fetch_add(size, Ordering::Relaxed);
        if previous + size > self.max_queue_bytes {
            release_bytes(&self.queued_bytes, size);
            self.metrics.telemetry_spans_lost.inc("byte_budget");
            return;
        }
        self.queued_spans.fetch_add(1, Ordering::Relaxed);
        match self.sender.try_send(Message::Span(Box::new(span), size)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                release_bytes(&self.queued_bytes, size);
                if release_span_reservation(&self.queued_spans) {
                    self.metrics.telemetry_spans_lost.inc("queue_full");
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                release_bytes(&self.queued_bytes, size);
                if release_span_reservation(&self.queued_spans) {
                    self.metrics.telemetry_spans_lost.inc("shutdown");
                }
            }
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        // An exporter that failed its own flush fails this one.
        self.control(Message::Flush, CONTROL_TIMEOUT)?
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let answer = self.control(Message::Shutdown, timeout);
        // The worker owns terminal accounting for every span it accepted:
        // each is counted exported or export-failed, and whatever is left
        // when the worker ends (normally or by unwinding from an exporter
        // panic) is counted as a shutdown loss by `Worker`'s `Drop`. Here we
        // only wait for a worker that is finished (it acknowledged the
        // shutdown) or gone (the queue or acknowledgement disconnected), so
        // its accounting is complete when this returns. On a timeout the
        // worker still owns its spans and counts them when it resumes.
        // An answer carrying the exporter's failure still means the worker
        // finished.
        if matches!(answer, Ok(_) | Err(OTelSdkError::AlreadyShutdown))
            && let Ok(mut worker) = self.worker.lock()
            && let Some(handle) = worker.take()
        {
            let _ = handle.join();
        }
        answer?
    }

    fn set_resource(&mut self, resource: &Resource) {
        // Waits for queue space (bounded) instead of dropping the update.
        let message = Message::Resource(resource.clone());
        let _ = self.enqueue(message, Instant::now() + CONTROL_TIMEOUT, CONTROL_TIMEOUT);
    }
}

struct Worker {
    receiver: Receiver<Message>,
    queued_bytes: Arc<AtomicUsize>,
    queued_spans: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    metrics: Arc<Metrics>,
    max_batch: usize,
    max_request_bytes: usize,
    delay: Duration,
    /// Queue capacity: the most messages queued behind a shutdown that are
    /// still handled, so producers that keep sending cannot hold it open.
    capacity: usize,
}

impl Drop for Worker {
    /// Runs on every worker exit, including unwinding from an exporter
    /// panic. Disconnects the queue first, so a producer can no longer hand
    /// this worker a span (later `on_end` calls count their own `shutdown`
    /// loss), then counts every span reservation still outstanding
    /// (discarded queue messages, an unexported batch, an export that
    /// unwound, or a producer about to roll back) as a shutdown loss. A
    /// producer that then rolls back finds its reservation already taken
    /// (`release_span_reservation`) and does not count it again, so every
    /// span is counted exactly once. Outstanding byte charges are
    /// retired the same way, so `queued_bytes()` reaches zero with the
    /// spans they charged; producer byte rollbacks saturate
    /// (`release_bytes`), so one racing this swap cannot wrap it.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let (_, disconnected) = mpsc::sync_channel(0);
        drop(std::mem::replace(&mut self.receiver, disconnected));
        let stranded = self.queued_spans.swap(0, Ordering::Relaxed);
        self.queued_bytes.swap(0, Ordering::Relaxed);
        if stranded > 0 {
            self.metrics
                .telemetry_spans_lost
                .add("shutdown", stranded as u64);
        }
    }
}

/// Spans accumulated for the next export request, each with the byte
/// estimate it was charged at queue time.
struct Batch {
    spans: Vec<(SpanData, usize)>,
    bytes: usize,
}

impl Worker {
    fn run<E: SpanExporter>(self, mut exporter: E) {
        let mut batch = Batch {
            spans: Vec::with_capacity(self.max_batch),
            bytes: 0,
        };
        let mut shutdown_acks = Vec::new();
        let mut next_export = Instant::now() + self.delay;
        loop {
            let wait = next_export.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(wait) {
                Ok(message) => {
                    if self.dispatch(&mut exporter, &mut batch, message, &mut shutdown_acks) {
                        next_export = Instant::now() + self.delay;
                    }
                    if !shutdown_acks.is_empty() {
                        self.finish(&mut exporter, &mut batch, shutdown_acks);
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.export(&exporter, &mut batch);
                    next_export = Instant::now() + self.delay;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.export(&exporter, &mut batch);
                    let _ = exporter.shutdown();
                    return;
                }
            }
        }
    }

    /// Handles one message in queue order. Spans, resource updates, and
    /// flushes take effect at once; shutdown acknowledgements are collected
    /// and answered only after the exporter has shut down. Returns `true`
    /// when a full batch was exported.
    fn dispatch<E: SpanExporter>(
        &self,
        exporter: &mut E,
        batch: &mut Batch,
        message: Message,
        shutdown_acks: &mut Vec<SyncSender<OTelSdkResult>>,
    ) -> bool {
        match message {
            Message::Span(span, size) => {
                batch.spans.push((*span, size));
                batch.bytes += size;
                if batch.spans.len() >= self.max_batch || batch.bytes >= self.max_request_bytes {
                    self.export(exporter, batch);
                    return true;
                }
            }
            Message::Resource(resource) => {
                // Spans queued before the update are exported under the
                // resource that was current when they were queued.
                self.export(exporter, batch);
                exporter.set_resource(&resource);
            }
            Message::Flush(ack) => {
                self.export(exporter, batch);
                let _ = ack.send(exporter.force_flush());
            }
            Message::Shutdown(ack) => shutdown_acks.push(ack),
        }
        false
    }

    /// Completes a shutdown. Messages already queued behind it are still
    /// handled in order, so a flush, resource update, or second shutdown is
    /// never discarded with its acknowledgement. At most one queue's worth
    /// is taken; anything later is dropped with the queue and its caller
    /// sees `AlreadyShutdown`.
    fn finish<E: SpanExporter>(
        &self,
        exporter: &mut E,
        batch: &mut Batch,
        mut shutdown_acks: Vec<SyncSender<OTelSdkResult>>,
    ) {
        for _ in 0..self.capacity {
            let Ok(message) = self.receiver.try_recv() else {
                break;
            };
            self.dispatch(exporter, batch, message, &mut shutdown_acks);
        }
        self.export(exporter, batch);
        let result = exporter.shutdown();
        for ack in shutdown_acks {
            let _ = ack.send(copy_result(&result));
        }
    }

    fn export<E: SpanExporter>(&self, exporter: &E, batch: &mut Batch) {
        while !batch.spans.is_empty() {
            // Chunk by count and by estimated bytes, so a chunk cannot
            // exceed the exporter's request-body cap just because the
            // spans in it are large (it would be rejected whole). The
            // estimate is the same one the queue budget uses, not the
            // exact encoded size; a single span larger than the budget
            // still goes alone and the exporter decides its fate.
            let mut take = 0usize;
            let mut chunk_bytes = 0usize;
            for (_, size) in &batch.spans {
                if take >= self.max_batch
                    || (take > 0 && chunk_bytes + size > self.max_request_bytes)
                {
                    break;
                }
                chunk_bytes += size;
                take += 1;
            }
            let chunk: Vec<SpanData> = batch.spans.drain(..take).map(|(span, _)| span).collect();
            batch.bytes -= chunk_bytes;
            let count = chunk.len();
            // An exporter panic unwinds and ends this thread: waiting
            // callers then get `AlreadyShutdown`, `Worker`'s `Drop` counts
            // this chunk and everything still queued as `shutdown` losses,
            // later spans count their own, and `exporter.shutdown()` is not
            // called.
            let result = block_on(exporter.export(chunk));
            // Saturating for safety; terminal accounting stays with this
            // worker, so each span is still counted exactly once.
            let _ = self
                .queued_spans
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(count))
                });
            // Release this chunk's bytes as soon as its export resolves,
            // so a slow later chunk does not keep completed chunks
            // charged against the queue budget.
            release_bytes(&self.queued_bytes, chunk_bytes);
            match result {
                Ok(()) => {
                    self.metrics
                        .telemetry_spans_exported
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
                Err(_) => self
                    .metrics
                    .telemetry_spans_lost
                    .add("export_failed", count as u64),
            }
        }
    }
}

/// Drives a future to completion on the current (non-async) thread.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    struct ThreadWaker(thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = TaskContext::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use opentelemetry::Key;
    use opentelemetry_sdk::testing::trace::new_test_export_span_data;

    /// Records exporter calls in the order they happen.
    #[derive(Debug, Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn push(&self, event: String) {
            self.0.lock().unwrap().push(event);
        }

        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl SpanExporter for Recorder {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            self.push(format!("export:{}", batch.len()));
            std::future::ready(Ok(()))
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            self.push("shutdown".into());
            Ok(())
        }

        fn force_flush(&self) -> OTelSdkResult {
            self.push("flush".into());
            Ok(())
        }

        fn set_resource(&mut self, resource: &Resource) {
            let name = resource
                .get(&Key::from_static_str("service.name"))
                .map(|value| value.to_string());
            self.push(format!("resource:{}", name.unwrap_or_default()));
        }
    }

    fn span() -> Message {
        let span = new_test_export_span_data();
        let size = estimate_bytes(&span);
        Message::Span(Box::new(span), size)
    }

    fn ask(
        sender: &SyncSender<Message>,
        make: fn(SyncSender<OTelSdkResult>) -> Message,
    ) -> Receiver<OTelSdkResult> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        sender.send(make(ack_tx)).unwrap();
        ack_rx
    }

    #[test]
    fn estimate_bytes_charges_the_status_description() {
        let mut span = new_test_export_span_data();
        let plain = estimate_bytes(&span);
        span.status = opentelemetry::trace::Status::Error {
            description: "x".repeat(10_000).into(),
        };
        assert!(
            estimate_bytes(&span) >= plain + 10_000,
            "a large error description is queued payload and must be charged"
        );
    }

    /// Export requests are chunked by estimated bytes as well as by span
    /// count, so large spans cannot form a request over the body cap.
    #[test]
    fn exports_are_chunked_by_estimated_bytes() {
        let size = estimate_bytes(&new_test_export_span_data());
        let (sender, receiver) = mpsc::sync_channel(16);
        let metrics = Arc::new(Metrics::default());
        let worker = Worker {
            receiver,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            queued_spans: Arc::new(AtomicUsize::new(5)),
            closed: Arc::new(AtomicBool::new(false)),
            metrics: Arc::clone(&metrics),
            max_batch: 16,
            // Room for exactly two spans per request.
            max_request_bytes: size * 2,
            delay: Duration::from_secs(60),
            capacity: 16,
        };
        for _ in 0..5 {
            sender.send(span()).unwrap();
        }
        let flush = ask(&sender, Message::Flush);
        let recorder = Recorder::default();
        let seen = recorder.clone();
        let handle = std::thread::spawn(move || worker.run(recorder));
        flush.recv().unwrap().unwrap();
        drop(sender);
        handle.join().unwrap();
        assert_eq!(
            seen.events(),
            ["export:2", "export:2", "export:1", "flush", "shutdown"]
        );
        assert_eq!(metrics.telemetry_spans_exported.load(Ordering::Relaxed), 5);
    }

    /// A single span larger than the byte budget is exported alone (it
    /// cannot be split); the exporter decides its fate.
    #[test]
    fn an_oversized_span_is_exported_alone() {
        let (sender, receiver) = mpsc::sync_channel(16);
        let metrics = Arc::new(Metrics::default());
        let worker = Worker {
            receiver,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            queued_spans: Arc::new(AtomicUsize::new(3)),
            closed: Arc::new(AtomicBool::new(false)),
            metrics: Arc::clone(&metrics),
            max_batch: 16,
            max_request_bytes: 1,
            delay: Duration::from_secs(60),
            capacity: 16,
        };
        for _ in 0..3 {
            sender.send(span()).unwrap();
        }
        let flush = ask(&sender, Message::Flush);
        let recorder = Recorder::default();
        let seen = recorder.clone();
        let handle = std::thread::spawn(move || worker.run(recorder));
        flush.recv().unwrap().unwrap();
        drop(sender);
        handle.join().unwrap();
        assert_eq!(
            seen.events(),
            ["export:1", "export:1", "export:1", "flush", "shutdown"]
        );
    }

    /// An exporter that reports each export as it begins and parks the
    /// second one until released, so byte release timing is observable.
    #[derive(Debug)]
    struct GatedExporter {
        began: SyncSender<usize>,
        release: std::sync::Mutex<Receiver<()>>,
        calls: AtomicUsize,
    }

    impl SpanExporter for GatedExporter {
        fn export(
            &self,
            _batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = self.began.send(call);
            if call == 2 {
                let _ = self.release.lock().unwrap().recv();
            }
            std::future::ready(Ok(()))
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
    }

    /// A completed chunk's bytes leave the queue budget as soon as its
    /// export resolves, not when the whole batch finishes: while the
    /// second chunk's export is still parked, only its bytes remain
    /// charged.
    #[test]
    fn chunk_bytes_are_released_as_each_export_resolves() {
        let size = estimate_bytes(&new_test_export_span_data());
        let (sender, receiver) = mpsc::sync_channel(16);
        let queued_bytes = Arc::new(AtomicUsize::new(size * 2));
        let worker = Worker {
            receiver,
            queued_bytes: Arc::clone(&queued_bytes),
            queued_spans: Arc::new(AtomicUsize::new(2)),
            closed: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(Metrics::default()),
            max_batch: 16,
            // One span per export request.
            max_request_bytes: size,
            delay: Duration::from_secs(60),
            capacity: 16,
        };
        sender.send(span()).unwrap();
        sender.send(span()).unwrap();
        let flush = ask(&sender, Message::Flush);
        let (began_tx, began_rx) = mpsc::sync_channel(4);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let exporter = GatedExporter {
            began: began_tx,
            release: std::sync::Mutex::new(release_rx),
            calls: AtomicUsize::new(0),
        };
        let handle = std::thread::spawn(move || worker.run(exporter));
        let within = Duration::from_secs(5);
        assert_eq!(began_rx.recv_timeout(within).unwrap(), 1);
        assert_eq!(began_rx.recv_timeout(within).unwrap(), 2);
        assert_eq!(
            queued_bytes.load(Ordering::Relaxed),
            size,
            "the first chunk's bytes are released before the second export runs"
        );
        release_tx.send(()).unwrap();
        flush.recv().unwrap().unwrap();
        drop(sender);
        handle.join().unwrap();
        assert_eq!(queued_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn messages_queued_behind_a_flush_or_shutdown_are_handled_in_order() {
        let (sender, receiver) = mpsc::sync_channel(16);
        let metrics = Arc::new(Metrics::default());
        let worker = Worker {
            receiver,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            queued_spans: Arc::new(AtomicUsize::new(3)),
            closed: Arc::new(AtomicBool::new(false)),
            metrics: Arc::clone(&metrics),
            max_batch: 16,
            max_request_bytes: usize::MAX,
            delay: Duration::from_secs(60),
            capacity: 16,
        };
        let resource = Resource::builder_empty()
            .with_service_name("orders-api")
            .build();

        // Everything is queued before the worker starts, so each flush and
        // shutdown finds later messages already waiting behind it.
        sender.send(span()).unwrap();
        let first_flush = ask(&sender, Message::Flush);
        sender.send(Message::Resource(resource)).unwrap();
        sender.send(span()).unwrap();
        let shutdown = ask(&sender, Message::Shutdown);
        sender.send(span()).unwrap();
        let late_flush = ask(&sender, Message::Flush);
        let second_shutdown = ask(&sender, Message::Shutdown);
        drop(sender);

        let exporter = Recorder::default();
        let recorder = exporter.clone();
        worker.run(exporter);

        assert_eq!(
            recorder.events(),
            [
                "export:1",
                "flush",
                "resource:orders-api",
                "export:2",
                "flush",
                "shutdown",
            ]
        );
        for (name, ack) in [
            ("first flush", first_flush),
            ("shutdown", shutdown),
            ("late flush", late_flush),
            ("second shutdown", second_shutdown),
        ] {
            assert!(
                matches!(ack.try_recv(), Ok(Ok(()))),
                "{name} was not acknowledged"
            );
        }
        let exported = metrics.telemetry_spans_exported.load(Ordering::Relaxed);
        assert_eq!(exported, 3);
    }

    #[test]
    fn a_rollback_after_the_worker_ended_is_not_counted_twice() {
        // Two producers hold reservations (one about to see `Full`, one
        // `Disconnected`) when the worker unwinds. The worker counts both;
        // their rollbacks then find nothing to release, never wrap the
        // counter, and add no second loss.
        let (_sender, receiver) = mpsc::sync_channel(1);
        let metrics = Arc::new(Metrics::default());
        let queued_spans = Arc::new(AtomicUsize::new(2));
        let queued_bytes = Arc::new(AtomicUsize::new(200));
        let worker = Worker {
            receiver,
            queued_bytes: Arc::clone(&queued_bytes),
            queued_spans: Arc::clone(&queued_spans),
            closed: Arc::new(AtomicBool::new(false)),
            metrics: Arc::clone(&metrics),
            max_batch: 1,
            max_request_bytes: usize::MAX,
            delay: Duration::from_secs(60),
            capacity: 1,
        };
        drop(worker);
        assert_eq!(metrics.telemetry_spans_lost.get("shutdown"), 2);
        assert!(!release_span_reservation(&queued_spans));
        assert!(!release_span_reservation(&queued_spans));
        assert_eq!(queued_spans.load(Ordering::Relaxed), 0);
        // Outstanding byte charges are retired with their spans, so the
        // reported queue depth reaches zero; each producer's own byte
        // rollback then saturates instead of wrapping.
        assert_eq!(queued_bytes.load(Ordering::Relaxed), 0);
        release_bytes(&queued_bytes, 100);
        release_bytes(&queued_bytes, 100);
        assert_eq!(queued_bytes.load(Ordering::Relaxed), 0);

        // A reservation taken after the worker ended is released normally.
        queued_spans.fetch_add(1, Ordering::Relaxed);
        assert!(release_span_reservation(&queued_spans));
        assert_eq!(queued_spans.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_shutdown_handles_at_most_one_queue_of_later_messages() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let worker = Worker {
            receiver,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            queued_spans: Arc::new(AtomicUsize::new(0)),
            closed: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(Metrics::default()),
            max_batch: 8,
            max_request_bytes: usize::MAX,
            delay: Duration::from_secs(60),
            capacity: 2,
        };
        let shutdown = ask(&sender, Message::Shutdown);
        let first = ask(&sender, Message::Flush);
        let second = ask(&sender, Message::Flush);
        let beyond = ask(&sender, Message::Flush);

        let exporter = Recorder::default();
        let recorder = exporter.clone();
        // The sender stays alive, as a busy producer's would: the worker
        // must still return.
        worker.run(exporter);

        assert_eq!(recorder.events(), ["flush", "flush", "shutdown"]);
        for ack in [shutdown, first, second] {
            assert!(matches!(ack.try_recv(), Ok(Ok(()))));
        }
        // Dropped with the queue: its caller sees the disconnect at once.
        assert!(matches!(
            beyond.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        drop(sender);
    }
}
