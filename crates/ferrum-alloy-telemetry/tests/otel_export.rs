//! OpenTelemetry export: real exported span relationships, streaming span
//! end times, sampling, and collector failure behavior.

#![cfg(feature = "otel")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::manual_async_fn
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::Request;
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy_telemetry::metrics::Metrics;
use ferrum_alloy_telemetry::operation::Operation;
use ferrum_alloy_telemetry::otel::{OtelPipeline, OtlpConfig, ServiceResource, layer_active};
use ferrum_alloy_telemetry::peer::{PeerInfo, TrustedPeers, TrustedPeersConfig};
use ferrum_alloy_telemetry::{RecordRouteLayer, RequestContext, TelemetryConfig, TelemetryLayer};
use http_body_util::BodyExt;
use opentelemetry::trace::{SpanId, SpanKind, TraceId};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData, SpanExporter};
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
    OtlpConfig {
        enabled: true,
        scheduled_delay_ms: 50,
        ..OtlpConfig::default()
    }
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
    let peers = TrustedPeers::new(&TrustedPeersConfig {
        identities: vec![],
        networks: vec!["10.0.0.0/8".parse().unwrap()],
    })
    .unwrap();
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
    let (pipeline, exporter, metrics) = memory_pipeline(&config());
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(pipeline.layer()));
    let router = Router::new()
        .route(
            "/events",
            get(|| async {
                let (mut sender, body) =
                    http_body_util::channel::Channel::<Bytes, std::convert::Infallible>::new(2);
                tokio::spawn(async move {
                    for _ in 0..3 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if sender
                            .send_data(Bytes::from_static(b"data: tick\n\n"))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
                axum::response::Response::new(Body::new(body))
            }),
        )
        .layer(RecordRouteLayer);
    let service = telemetry(Arc::clone(&metrics), TelemetryConfig::default()).layer(router);
    let started = Instant::now();
    let response = service
        .oneshot(request("/events", "203.0.113.1:1", None))
        .await
        .unwrap();
    let headers_after = started.elapsed();
    let text = body_text(response).await;
    assert_eq!(text.matches("tick").count(), 3);
    pipeline.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let server = spans
        .iter()
        .find(|s| s.span_kind == SpanKind::Server)
        .unwrap();
    let span_ms = server
        .end_time
        .duration_since(server.start_time)
        .unwrap()
        .as_millis();
    assert!(
        headers_after < Duration::from_millis(100),
        "headers were immediate"
    );
    assert!(
        span_ms >= 140,
        "span must cover the stream, lasted {span_ms} ms"
    );
    let body_ms = match attr(server, "alloy.server.body_duration_ms").unwrap() {
        opentelemetry::Value::F64(v) => *v,
        other => panic!("unexpected {other:?}"),
    };
    assert!(body_ms >= 140.0, "{body_ms}");
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
    let config = OtlpConfig {
        sampling_ratio: 0.0,
        ..config()
    };
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
    let config = OtlpConfig {
        max_queue_spans: 4,
        max_export_batch: 1,
        scheduled_delay_ms: 10,
        ..config()
    };
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

#[tokio::test(flavor = "current_thread")]
async fn the_byte_budget_bounds_queued_memory() {
    let config = OtlpConfig {
        max_queue_bytes: 64,
        ..config()
    };
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
fn invalid_configuration_fails_at_startup() {
    let metrics = Arc::new(Metrics::default());
    for bad in [
        OtlpConfig {
            sampling_ratio: 1.5,
            ..OtlpConfig::default()
        },
        OtlpConfig {
            endpoint: Some("ftp://collector/v1/traces".into()),
            ..OtlpConfig::default()
        },
        OtlpConfig {
            endpoint: Some("http://user:secret@collector:4318/v1/traces".into()),
            ..OtlpConfig::default()
        },
        OtlpConfig {
            max_queue_spans: 0,
            ..OtlpConfig::default()
        },
    ] {
        assert!(
            OtelPipeline::with_exporter(&resource(), &bad, Arc::clone(&metrics), || Ok(
                InMemorySpanExporter::default()
            ))
            .is_err(),
            "{bad:?}"
        );
    }
    let error = OtelPipeline::with_exporter(&resource(), &OtlpConfig::default(), metrics, || {
        Err::<InMemorySpanExporter, _>("cannot build".to_owned())
    })
    .unwrap_err();
    assert!(error.to_string().contains("cannot build"));
}

#[test]
fn the_otlp_http_exporter_builds_without_a_collector() {
    // Building must not require connectivity; export failures are runtime
    // telemetry loss, not startup errors.
    let metrics = Arc::new(Metrics::default());
    let pipeline = OtelPipeline::otlp(
        &resource(),
        &OtlpConfig {
            enabled: true,
            endpoint: Some("http://127.0.0.1:9/v1/traces".into()),
            timeout_ms: 200,
            max_export_retries: 0,
            ..OtlpConfig::default()
        },
        metrics,
    )
    .unwrap();
    pipeline.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn layer_active_is_false_without_an_opentelemetry_layer() {
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry());
    assert!(!layer_active());
}
