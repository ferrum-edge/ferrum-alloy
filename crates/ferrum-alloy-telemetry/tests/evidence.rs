//! The evidence sink: one record per finalized request, the tenant the
//! application tagged, and nothing the layer does not already measure.

#![cfg(feature = "axum")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::get;
use ferrum_alloy_telemetry::evidence::{EvidenceSink, RequestEvidence, TenantTag};
use ferrum_alloy_telemetry::{BodyOutcome, RecordRouteLayer, TelemetryConfig, TelemetryLayer};
use http_body_util::BodyExt;
use tower::{Layer, ServiceExt};

/// A tagged request whose path and query carry values that must not leak.
const ORDER: &str = "/tenants/acme/orders/7?card=4111111111111111";

#[derive(Debug, Default)]
struct Collect(Mutex<Vec<RequestEvidence>>);

impl EvidenceSink for Collect {
    fn record(&self, evidence: RequestEvidence) {
        self.0.lock().unwrap().push(evidence);
    }
}

fn app() -> Router {
    Router::new()
        .route(
            "/tenants/acme/orders/{id}",
            get(|tenant: TenantTag| async move {
                assert!(tenant.set("acme"));
                assert!(!tenant.set("globex"), "the first tag wins");
                "ok"
            }),
        )
        .route("/untagged", get(|| async { "ok" }))
        .layer(RecordRouteLayer)
}

async fn send(
    service: impl tower::Service<
        Request<Body>,
        Response = Response<
            impl http_body::Body<Data = bytes::Bytes, Error = impl std::fmt::Debug>,
        >,
        Error = Infallible,
    >,
    uri: &str,
    id: &str,
) -> StatusCode {
    let request = Request::get(uri)
        .header("x-request-id", id)
        .header("authorization", "Bearer secret-credential")
        .body(Body::empty())
        .unwrap();
    let response = service.oneshot(request).await.unwrap();
    let status = response.status();
    response.into_body().collect().await.unwrap();
    status
}

#[tokio::test]
async fn every_finalized_request_is_handed_over_once_with_its_tenant() {
    let sink = Arc::new(Collect::default());
    let telemetry = TelemetryLayer::new(TelemetryConfig::default())
        .unwrap()
        .with_evidence_sink(sink.clone());
    let service = telemetry.layer(app());

    let status = send(service.clone(), ORDER, "req-a").await;
    assert_eq!(status, StatusCode::OK);
    let status = send(service.clone(), "/untagged", "req-b").await;
    assert_eq!(status, StatusCode::OK);
    let status = send(service.clone(), "/missing", "req-c").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let records = sink.0.lock().unwrap().clone();
    assert_eq!(records.len(), 3, "exactly one record per request");
    let tagged = &records[0];
    assert_eq!(tagged.request_id.as_str(), "req-a");
    assert_eq!(tagged.tenant.as_deref(), Some("acme"));
    assert_eq!(tagged.route.as_deref(), Some("/tenants/acme/orders/{id}"));
    assert_eq!(tagged.status, Some(200));
    assert_eq!(tagged.outcome, BodyOutcome::Completed);
    assert_eq!(tagged.peer_trust, "untrusted");
    let headers = tagged.time_to_headers.expect("headers were produced");
    assert!(headers <= tagged.duration);
    assert!(tagged.body_duration.is_some());

    assert_eq!(records[1].tenant, None, "untagged requests carry no tenant");
    assert_eq!(
        records[2].route, None,
        "unmatched requests have no template"
    );
    assert_eq!(records[2].status, Some(404));

    // The evidence holds labels and timings only: no query, raw path, or
    // header values.
    let text = format!("{records:?}");
    for forbidden in [
        "4111111111111111",
        "orders/7",
        "secret-credential",
        "Bearer",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} leaked into {text}");
    }
}

/// Without a sink the extractor yields a detached tag, which nothing reads.
async fn detached(tenant: TenantTag) -> &'static str {
    assert_eq!(tenant.get(), None);
    assert!(tenant.set("acme"));
    "ok"
}

#[tokio::test]
async fn without_a_sink_no_tag_is_inserted() {
    let telemetry = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let router = Router::new().route("/", get(detached));
    let service = telemetry.layer(router);
    assert_eq!(send(service, "/", "req-d").await, StatusCode::OK);
}
