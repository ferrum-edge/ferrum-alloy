//! OpenTelemetry export: real exported span relationships, streaming span
//! end times, sampling, and collector failure behavior.

#![cfg(all(feature = "otel", feature = "axum"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::manual_async_fn
)]

use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::Request;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy_telemetry::metrics::Metrics;
use ferrum_alloy_telemetry::operation::Operation;
use ferrum_alloy_telemetry::otel::{
    BoundedSpanProcessor, OtelPipeline, OtlpConfig, ServiceResource, layer_active,
};
use ferrum_alloy_telemetry::peer::{PeerInfo, TrustedPeers, TrustedPeersConfig};
use ferrum_alloy_telemetry::{RecordRouteLayer, RequestContext, TelemetryConfig, TelemetryLayer};
use http_body_util::BodyExt;
use opentelemetry::trace::{SpanId, SpanKind, TraceId};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::testing::trace::new_test_export_span_data;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData, SpanExporter, SpanProcessor};
use tower::{Layer, ServiceExt};
use tracing_subscriber::layer::SubscriberExt;

const INCOMING_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const INCOMING_PARENT: &str = "00f067aa0ba902b7";

fn resource() -> ServiceResource {
    ServiceResource {
        name: "orders-api".into(),
        version: Some("1.2.3".into()),
        instance_id: Some("orders-1".into()),
        environment: Some("test".into()),
    }
}

fn config() -> OtlpConfig {
    let mut config = OtlpConfig::default();
    config.enabled = true;
    config.scheduled_delay_ms = 50;
    config
}

fn memory_pipeline(config: &OtlpConfig) -> (OtelPipeline, InMemorySpanExporter, Arc<Metrics>) {
    let exporter = InMemorySpanExporter::default();
    let shared = exporter.clone();
    let metrics = Arc::new(Metrics::default());
    let pipeline =
        OtelPipeline::with_exporter(&resource(), config, Arc::clone(&metrics), move || {
            Ok(shared)
        })
        .unwrap();
    (pipeline, exporter, metrics)
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn telemetry(metrics: Arc<Metrics>, config: TelemetryConfig) -> TelemetryLayer {
    let mut trust = TrustedPeersConfig::default();
    trust.networks = vec!["10.0.0.0/8".parse().unwrap()];
    let peers = TrustedPeers::new(&trust).unwrap();
    TelemetryLayer::new(config)
        .unwrap()
        .with_classifier(Arc::new(peers))
        .with_metrics(metrics)
}

fn request(uri: &str, peer: &str, traceparent: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(value) = traceparent {
        builder = builder.header("traceparent", value);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    request.extensions_mut().insert(PeerInfo {
        remote_addr: Some(peer.parse::<SocketAddr>().unwrap()),
        tls: None,
    });
    request
}

fn orders_router() -> Router {
    Router::new()
        .route(
            "/orders/{id}",
            get(|Path(id): Path<u32>, context: RequestContext| async move {
                Operation::new("orders.load")
                    .db("postgresql", "SELECT")
                    .summary("SELECT orders")
                    .run(tokio::time::sleep(Duration::from_millis(20)))
                    .await;
                format!("{id}|{}|{}", context.trace_id, context.span_id)
            }),
        )
        .layer(RecordRouteLayer)
}

async fn body_text<B>(response: http::Response<B>) -> String
where
    B: http_body::Body<Data = Bytes>,
    B::Error: std::fmt::Debug,
{
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn trusted_parent_links_exported_spans_and_ids_agree_with_logs() {
    let (pipeline, exporter, metrics) = memory_pipeline(&config());
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    assert!(layer_active());

    let service =
        telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(orders_router());
    let traceparent = format!("00-{INCOMING_TRACE}-{INCOMING_PARENT}-01");
    let response = service
        .oneshot(request("/orders/7", "10.1.1.1:5000", Some(&traceparent)))
        .await
        .unwrap();
    let body = body_text(response).await;
    let parts: Vec<&str> = body.split('|').collect();
    pipeline.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let server = spans
        .iter()
        .find(|s| s.span_kind == SpanKind::Server)
        .unwrap();
    assert_eq!(server.name, "GET /orders/{id}");
    assert_eq!(
        server.span_context.trace_id(),
        TraceId::from_hex(INCOMING_TRACE).unwrap()
    );
    assert_eq!(
        server.parent_span_id,
        SpanId::from_hex(INCOMING_PARENT).unwrap()
    );
    assert!(server.parent_span_is_remote);
    assert_eq!(
        parts[1], INCOMING_TRACE,
        "handler context carries the exported trace id"
    );
    assert_eq!(
        parts[2],
        server.span_context.span_id().to_string(),
        "RequestContext.span_id equals the exported span id"
    );
    assert_eq!(
        server.instrumentation_scope.name(),
        "ferrum-alloy-telemetry"
    );
    assert_eq!(attr(server, "http.route").unwrap().as_str(), "/orders/{id}");
    assert_eq!(
        attr(server, "alloy.trace.parent").unwrap().as_str(),
        "accepted_remote"
    );
    assert_eq!(
        attr(server, "alloy.peer.trust").unwrap().as_str(),
        "network_boundary"
    );
    assert!(attr(server, "alloy.server.time_to_headers_ms").is_some());
    assert_eq!(
        attr(server, "alloy.response.body.outcome")
            .unwrap()
            .as_str(),
        "completed"
    );

    let operation = spans
        .iter()
        .find(|s| s.name == "orders.load")
        .expect("operation span exported");
    assert_eq!(operation.span_kind, SpanKind::Client);
    assert_eq!(operation.parent_span_id, server.span_context.span_id());
    assert_eq!(
        attr(operation, "db.system.name").unwrap().as_str(),
        "postgresql"
    );
    let op_ms = match attr(operation, "alloy.operation.duration_ms").unwrap() {
        opentelemetry::Value::F64(v) => *v,
        other => panic!("unexpected {other:?}"),
    };
    assert!(op_ms >= 20.0, "{op_ms}");
    assert_eq!(
        metrics
            .telemetry_spans_exported
            .load(std::sync::atomic::Ordering::Relaxed),
        spans.len() as u64
    );
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn server_span_ends_after_the_body_not_at_headers() {
    const TICK: &[u8] = b"data: tick\n\n";
    let (pipeline, exporter, metrics) = memory_pipeline(&config());
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    // The test itself produces the body, so every instant below is taken at a
    // known point of the response lifecycle. Runner or scheduler delays can
    // only lengthen the measured intervals, never reorder them.
    let (mut sender, channel) =
        http_body_util::channel::Channel::<Bytes, std::convert::Infallible>::new(2);
    let slot = Arc::new(Mutex::new(Some(channel)));
    let router = Router::new()
        .route(
            "/events",
            get(move || {
                let channel = slot.lock().unwrap().take().unwrap();
                async move { axum::response::Response::new(Body::new(channel)) }
            }),
        )
        .layer(RecordRouteLayer);
    let service = telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(router);
    let response = service
        .oneshot(request("/events", "203.0.113.1:1", None))
        .await
        .unwrap();
    // Headers exist from here on; the server span was created before them.
    let headers_seen = Instant::now();
    let headers_seen_wall = SystemTime::now();
    let mut body = response.into_body();

    sender.send_data(Bytes::from_static(TICK)).await.unwrap();
    let first = body.frame().await.unwrap().unwrap();
    assert_eq!(first.into_data().unwrap(), Bytes::from_static(TICK));
    // Neither the headers nor a delivered frame may end the span.
    pipeline.force_flush().unwrap();
    assert!(
        exporter
            .get_finished_spans()
            .unwrap()
            .iter()
            .all(|s| s.span_kind != SpanKind::Server),
        "server span ended while the body was still streaming"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let last_frame_sent = Instant::now();
    let last_frame_sent_wall = SystemTime::now();
    sender.send_data(Bytes::from_static(TICK)).await.unwrap();
    drop(sender);
    let mut rest = Vec::new();
    while let Some(frame) = body.frame().await {
        rest.push(frame.unwrap().into_data().unwrap());
    }
    assert_eq!(rest, [Bytes::from_static(TICK)]);
    pipeline.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let server = spans
        .iter()
        .find(|s| s.span_kind == SpanKind::Server)
        .unwrap();
    assert!(
        server.start_time <= headers_seen_wall,
        "span started after the headers were returned"
    );
    // Same clock as the exported timestamps: the span's end is stamped only
    // once the body ends, which is after the test sent the final frame.
    assert!(
        server.end_time >= last_frame_sent_wall,
        "span ended before the final body frame was sent ({:?} vs {:?} after headers)",
        server.end_time.duration_since(headers_seen_wall),
        last_frame_sent_wall.duration_since(headers_seen_wall)
    );
    assert_eq!(
        attr(server, "alloy.response.body.outcome")
            .unwrap()
            .as_str(),
        "completed"
    );
    // The body duration runs from the layer seeing headers to the body's end,
    // an interval that encloses the one the test kept the stream open for.
    let streamed = last_frame_sent.duration_since(headers_seen);
    let body_ms = match attr(server, "alloy.server.body_duration_ms").unwrap() {
        opentelemetry::Value::F64(v) => *v,
        other => panic!("unexpected {other:?}"),
    };
    assert!(
        body_ms >= streamed.as_secs_f64() * 1_000.0,
        "body lasted {body_ms} ms but the test streamed it for {streamed:?}"
    );
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn untrusted_context_is_rerooted_and_optionally_linked() {
    let (pipeline, exporter, metrics) = memory_pipeline(&config());
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    let mut telemetry_config = TelemetryConfig::default();
    telemetry_config.trace_context.link_untrusted_parent = true;
    let service = telemetry(Arc::clone(&metrics), telemetry_config).layer(orders_router());
    let traceparent = format!("00-{INCOMING_TRACE}-{INCOMING_PARENT}-01");
    let response = service
        .oneshot(request("/orders/1", "203.0.113.1:1", Some(&traceparent)))
        .await
        .unwrap();
    body_text(response).await;
    pipeline.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let server = spans
        .iter()
        .find(|s| s.span_kind == SpanKind::Server)
        .unwrap();
    assert_ne!(
        server.span_context.trace_id(),
        TraceId::from_hex(INCOMING_TRACE).unwrap()
    );
    assert_eq!(
        server.parent_span_id,
        SpanId::INVALID,
        "re-rooted: no remote parent"
    );
    let link = server.links.links.first().expect("untrusted parent linked");
    assert_eq!(
        link.span_context.trace_id(),
        TraceId::from_hex(INCOMING_TRACE).unwrap()
    );
    assert!(
        link.attributes
            .iter()
            .any(|kv| kv.value.as_str() == "untrusted_parent")
    );
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn untrusted_callers_cannot_force_sampling() {
    let mut config = config();
    config.sampling_ratio = 0.0;
    let (pipeline, exporter, metrics) = memory_pipeline(&config);
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    let service =
        telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(orders_router());
    let sampled = format!("00-{INCOMING_TRACE}-{INCOMING_PARENT}-01");

    // Untrusted caller asks for sampling: ignored (root ratio is 0).
    let response = service
        .clone()
        .oneshot(request("/orders/1", "203.0.113.1:1", Some(&sampled)))
        .await
        .unwrap();
    body_text(response).await;
    pipeline.force_flush().unwrap();
    assert!(exporter.get_finished_spans().unwrap().is_empty());

    // Trusted gateway sampled the trace: parent-based sampling keeps it.
    let response = service
        .oneshot(request("/orders/1", "10.0.0.9:1", Some(&sampled)))
        .await
        .unwrap();
    body_text(response).await;
    pipeline.force_flush().unwrap();
    assert!(!exporter.get_finished_spans().unwrap().is_empty());
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[derive(Debug)]
struct FailingExporter;
impl SpanExporter for FailingExporter {
    fn export(
        &self,
        _batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        async {
            Err(OTelSdkError::InternalFailure(
                "collector unavailable".into(),
            ))
        }
    }
}

#[derive(Debug)]
struct SlowExporter(Duration);
impl SpanExporter for SlowExporter {
    fn export(
        &self,
        _batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let delay = self.0;
        async move {
            std::thread::sleep(delay);
            Ok(())
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn collector_failures_never_fail_requests_and_are_counted() {
    let metrics = Arc::new(Metrics::default());
    let pipeline =
        OtelPipeline::with_exporter(&resource(), &config(), Arc::clone(&metrics), || {
            Ok(FailingExporter)
        })
        .unwrap();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    let service =
        telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(orders_router());
    for _ in 0..5 {
        let response = service
            .clone()
            .oneshot(request("/orders/1", "203.0.113.1:1", None))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        body_text(response).await;
    }
    pipeline.force_flush().unwrap();
    assert_eq!(
        metrics.telemetry_spans_lost.get("export_failed"),
        10,
        "5 server + 5 operation spans"
    );
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_full_queue_drops_spans_instead_of_blocking_requests() {
    let mut config = config();
    config.max_queue_spans = 4;
    config.max_export_batch = 1;
    config.scheduled_delay_ms = 10;
    let metrics = Arc::new(Metrics::default());
    let pipeline = OtelPipeline::with_exporter(&resource(), &config, Arc::clone(&metrics), || {
        Ok(SlowExporter(Duration::from_millis(200)))
    })
    .unwrap();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    let service = telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(
        Router::new()
            .route("/fast", get(|| async { "ok" }))
            .layer(RecordRouteLayer),
    );
    let started = Instant::now();
    for _ in 0..40 {
        let response = service
            .clone()
            .oneshot(request("/fast", "203.0.113.1:1", None))
            .await
            .unwrap();
        body_text(response).await;
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "requests were not blocked by export"
    );
    assert!(metrics.telemetry_spans_lost.get("queue_full") > 0);
    // Bounded shutdown: stop waiting even though the exporter is slow.
    let shutdown_started = Instant::now();
    let _ = pipeline.shutdown(Duration::from_millis(300));
    assert!(shutdown_started.elapsed() < Duration::from_secs(2));
}

#[derive(Debug, Default)]
struct GateState {
    exporting: bool,
    open: bool,
    exported: usize,
    shutdowns: usize,
    resources: usize,
}

/// Blocks every export until the gate opens and counts lifecycle calls.
#[derive(Debug, Clone, Default)]
struct GatedExporter(Arc<(Mutex<GateState>, Condvar)>);

impl GatedExporter {
    fn wait_until_exporting(&self) {
        let (state, changed) = &*self.0;
        let guard = state.lock().unwrap();
        let (_guard, wait) = changed
            .wait_timeout_while(guard, Duration::from_secs(5), |s| !s.exporting)
            .unwrap();
        assert!(!wait.timed_out(), "the export never started");
    }

    fn open(&self) {
        let (state, changed) = &*self.0;
        state.lock().unwrap().open = true;
        changed.notify_all();
    }

    fn counts(&self) -> (usize, usize, usize) {
        let state = self.0.0.lock().unwrap();
        (state.exported, state.shutdowns, state.resources)
    }
}

impl SpanExporter for GatedExporter {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let (state, changed) = &*self.0;
        let mut guard = state.lock().unwrap();
        guard.exporting = true;
        changed.notify_all();
        guard = changed.wait_while(guard, |s| !s.open).unwrap();
        guard.exported += batch.len();
        std::future::ready(Ok(()))
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        self.0.0.lock().unwrap().shutdowns += 1;
        Ok(())
    }

    fn set_resource(&mut self, _resource: &Resource) {
        self.0.0.lock().unwrap().resources += 1;
    }
}

#[test]
fn a_flush_and_a_shutdown_queued_behind_a_blocked_export_both_complete() {
    let exporter = GatedExporter::default();
    let shared = exporter.clone();
    let mut config = config();
    config.max_queue_spans = 8;
    config.max_export_batch = 1;
    let metrics = Arc::new(Metrics::default());
    let factory = move || Ok(shared);
    let spawned = BoundedSpanProcessor::spawn(factory, &config, Arc::clone(&metrics));
    let mut processor = spawned.unwrap();

    processor.on_end(new_test_export_span_data());
    exporter.wait_until_exporting();
    // Queued behind the blocked export, ahead of the controls.
    let resource = Resource::builder_empty()
        .with_service_name("orders-api")
        .build();
    processor.set_resource(&resource);

    let processor = &processor;
    let timeout = Duration::from_secs(5);
    let (flush, shutdown) = std::thread::scope(|scope| {
        let flush = scope.spawn(|| processor.force_flush());
        let shutdown = scope.spawn(|| processor.shutdown_with_timeout(timeout));
        // Release the export only once both controls are queued behind it.
        // Whichever is handled first must not swallow the other.
        let deadline = Instant::now() + Duration::from_secs(5);
        while processor.controls_queued() < 2 {
            assert!(Instant::now() < deadline, "the controls were never queued");
            std::thread::sleep(Duration::from_millis(1));
        }
        exporter.open();
        (flush.join().unwrap(), shutdown.join().unwrap())
    });

    assert!(flush.is_ok(), "flush: {flush:?}");
    assert!(shutdown.is_ok(), "shutdown: {shutdown:?}");
    let (exported, shutdowns, resources) = exporter.counts();
    assert_eq!(exported, 1);
    assert_eq!(shutdowns, 1, "the exporter is shut down exactly once");
    assert_eq!(resources, 1, "the resource update was applied");
    assert_eq!(
        metrics
            .telemetry_spans_exported
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    let after = processor.force_flush();
    assert!(matches!(after, Err(OTelSdkError::AlreadyShutdown)));
}

#[tokio::test(flavor = "current_thread")]
async fn the_byte_budget_bounds_queued_memory() {
    let mut config = config();
    config.max_queue_bytes = 64;
    let (pipeline, exporter, metrics) = memory_pipeline(&config);
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    let service =
        telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(orders_router());
    let response = service
        .oneshot(request("/orders/1", "203.0.113.1:1", None))
        .await
        .unwrap();
    body_text(response).await;
    pipeline.force_flush().unwrap();
    assert!(metrics.telemetry_spans_lost.get("byte_budget") >= 1);
    assert!(exporter.get_finished_spans().unwrap().is_empty());
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn the_byte_budget_counts_large_event_attributes() {
    let mut config = config();
    config.max_queue_bytes = 2_048;
    let (pipeline, exporter, metrics) = memory_pipeline(&config);
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));

    let small_span = tracing::info_span!("small_budget_test");
    {
        let _entered = small_span.enter();
        tracing::info!(payload = "small", "small_event");
    }
    drop(small_span);

    let large_payload = "x".repeat(65_536);
    let large_span = tracing::info_span!("large_budget_test");
    {
        let _entered = large_span.enter();
        tracing::info!(payload = large_payload.as_str(), "large_event");
    }
    drop(large_span);

    pipeline.force_flush().unwrap();

    assert!(metrics.telemetry_spans_lost.get("byte_budget") >= 1);
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "small_budget_test");
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn invalid_configuration_fails_at_startup() {
    let metrics = Arc::new(Metrics::default());
    let invalid: [fn(&mut OtlpConfig); 4] = [
        |c| c.sampling_ratio = 1.5,
        |c| c.endpoint = Some("ftp://collector/v1/traces".into()),
        |c| c.endpoint = Some("http://user:secret@collector:4318/v1/traces".into()),
        |c| c.max_queue_spans = 0,
    ];
    for change in invalid {
        let mut bad = OtlpConfig::default();
        change(&mut bad);
        assert!(
            OtelPipeline::with_exporter(&resource(), &bad, Arc::clone(&metrics), || Ok(
                InMemorySpanExporter::default()
            ))
            .is_err()
        );
    }
    let error = OtelPipeline::with_exporter(&resource(), &OtlpConfig::default(), metrics, || {
        Err::<InMemorySpanExporter, _>("cannot build".to_owned())
    })
    .unwrap_err();
    assert!(error.to_string().contains("cannot build"));
}

#[test]
fn rejected_otlp_endpoints_do_not_appear_in_startup_errors() {
    let credential_endpoint = "http://sentinel-user:sentinel-password@collector:4318/v1/traces";
    let mut config = OtlpConfig::default();
    config.enabled = true;
    config.endpoint = Some(credential_endpoint.to_owned());
    let config_debug = format!("{config:?}");
    assert!(!config_debug.contains("sentinel-user"));
    assert!(!config_debug.contains("sentinel-password"));
    let metrics = Arc::new(Metrics::default());
    let error = OtelPipeline::otlp(&resource(), &config, Arc::clone(&metrics)).unwrap_err();
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(display.contains("without credentials"));
    assert!(!display.contains("sentinel-user"));
    assert!(!display.contains("sentinel-password"));
    assert!(!debug.contains("sentinel-user"));
    assert!(!debug.contains("sentinel-password"));

    let init_error = ferrum_alloy_telemetry::init::init_logging_and_otel(
        &ferrum_alloy_telemetry::init::LoggingConfig::default(),
        &resource(),
        &config,
        Arc::clone(&metrics),
    )
    .unwrap_err();
    let display = init_error.to_string();
    let debug = format!("{init_error:?}");
    assert!(display.contains("without credentials"));
    assert!(!display.contains("sentinel-user"));
    assert!(!display.contains("sentinel-password"));
    assert!(!debug.contains("sentinel-user"));
    assert!(!debug.contains("sentinel-password"));

    for endpoint in ["ftp://collector/v1/traces", "http://[::1"] {
        config.endpoint = Some(endpoint.to_owned());
        let error = OtelPipeline::otlp(&resource(), &config, Arc::clone(&metrics)).unwrap_err();
        let display = error.to_string();
        assert!(display.contains("valid http(s) URL"));
        assert!(!display.contains(endpoint));
    }

    for endpoint in [
        "http://collector:4318/v1/traces",
        "https://collector.example/v1/traces",
        "http://collector:4318/v1/traces?x=a@b",
    ] {
        config.endpoint = Some(endpoint.to_owned());
        assert!(config.validate().is_ok());
    }

    config.endpoint = Some(format!("http://collector/{}", "x".repeat(2_031)));
    assert_eq!(config.endpoint.as_ref().unwrap().len(), 2_048);
    assert!(config.validate().is_ok());
    config.endpoint = Some(format!("http://collector/{}", "x".repeat(2_032)));
    let error = config.validate().unwrap_err();
    assert!(error.to_string().contains("2048 bytes"));
}

#[test]
fn the_otlp_http_exporter_builds_without_a_collector() {
    // Building must not require connectivity; export failures are runtime
    // telemetry loss, not startup errors.
    let metrics = Arc::new(Metrics::default());
    let mut config = OtlpConfig::default();
    config.enabled = true;
    config.endpoint = Some("http://127.0.0.1:9/v1/traces".into());
    config.timeout_ms = 200;
    config.max_export_retries = 0;
    let pipeline = OtelPipeline::otlp(&resource(), &config, metrics).unwrap();
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn layer_active_is_false_without_an_opentelemetry_layer() {
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry());
    assert!(!layer_active());
}
