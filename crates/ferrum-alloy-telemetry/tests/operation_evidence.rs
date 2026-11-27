//! Instrumented operations and the admission wait reach the request's
//! evidence: linked to the request's server span, bounded per request, and
//! only for work polled as part of the request.

#![cfg(feature = "axum")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use ferrum_alloy_telemetry::evidence::{
    EvidenceSink, MAX_OPERATIONS_PER_REQUEST, OperationOutcome, RequestEvidence,
    record_admission_wait,
};
use ferrum_alloy_telemetry::operation::{
    Operation, OperationKind, OperationRecorder, record_pool_wait,
};
use ferrum_alloy_telemetry::{TelemetryConfig, TelemetryLayer};
use http_body_util::BodyExt;
use tower::{Layer, ServiceExt};

#[derive(Debug, Default)]
struct Collect(Mutex<Vec<RequestEvidence>>);

impl EvidenceSink for Collect {
    fn record(&self, evidence: RequestEvidence) {
        self.0.lock().unwrap().push(evidence);
    }
}

/// Serves `router` once at `/` with an evidence sink and returns the one
/// record.
async fn evidence_of(router: Router) -> RequestEvidence {
    let sink = Arc::new(Collect::default());
    let service = TelemetryLayer::new(TelemetryConfig::default())
        .unwrap()
        .with_evidence_sink(sink.clone())
        .layer(router);
    let response = service
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    response.into_body().collect().await.unwrap();
    let mut records = sink.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    records.pop().unwrap()
}

#[tokio::test]
async fn operations_are_kept_with_their_parent_and_pool_wait() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            Operation::new("orders.load")
                .db("postgresql", "SELECT")
                .run(async {
                    record_pool_wait(Duration::from_millis(3));
                    Operation::new("orders.decode").run(async {}).await;
                })
                .await;
            let failed = Operation::new("orders.fail")
                .run_result(async { Err::<(), _>("boom") })
                .await;
            assert!(failed.is_err());
            "ok"
        }),
    ))
    .await;

    let names: Vec<&str> = evidence.operations.iter().map(|o| o.name).collect();
    // In the order they finished: the nested operation first.
    assert_eq!(names, ["orders.decode", "orders.load", "orders.fail"]);
    assert_eq!(evidence.operations_dropped, 0);
    let (decode, load, fail) = (
        &evidence.operations[0],
        &evidence.operations[1],
        &evidence.operations[2],
    );
    assert_eq!(load.kind, OperationKind::Db);
    assert_eq!(load.outcome, OperationOutcome::Completed);
    assert_eq!(load.pool_wait, Some(Duration::from_millis(3)));
    assert_eq!(
        load.parent_span_id, evidence.span_id,
        "top level: the server span"
    );
    assert_eq!(
        decode.parent_span_id, load.span_id,
        "nested under orders.load"
    );
    assert_eq!(
        decode.pool_wait, None,
        "the pool wait belongs to orders.load"
    );
    assert_eq!(fail.outcome, OperationOutcome::Error);
    assert_eq!(fail.parent_span_id, evidence.span_id);
    assert_ne!(load.span_id, evidence.span_id);
    assert_ne!(decode.span_id, load.span_id);
    for operation in &evidence.operations {
        assert!(
            operation.started + operation.duration <= evidence.duration,
            "{operation:?} lies inside the request"
        );
    }
    assert!(load.started <= decode.started);
}

#[tokio::test]
async fn a_dropped_operation_is_kept_as_cancelled() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            let slow = Operation::new("orders.slow").run(std::future::pending::<()>());
            let timed_out = tokio::time::timeout(Duration::from_millis(5), slow).await;
            assert!(timed_out.is_err());
            "ok"
        }),
    ))
    .await;
    assert_eq!(evidence.operations.len(), 1);
    let slow = &evidence.operations[0];
    assert_eq!(slow.name, "orders.slow");
    assert_eq!(slow.outcome, OperationOutcome::Cancelled);
    assert!(slow.duration >= Duration::from_millis(5));
}

#[tokio::test]
async fn work_on_another_task_is_not_attributed() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            tokio::spawn(Operation::new("background").run(async {}))
                .await
                .unwrap();
            "ok"
        }),
    ))
    .await;
    assert!(evidence.operations.is_empty(), "{:?}", evidence.operations);
}

#[tokio::test]
async fn operations_beyond_the_limit_are_only_counted() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            for _ in 0..MAX_OPERATIONS_PER_REQUEST + 8 {
                Operation::new("orders.item").run(async {}).await;
            }
            "ok"
        }),
    ))
    .await;
    assert_eq!(evidence.operations.len(), MAX_OPERATIONS_PER_REQUEST);
    assert_eq!(evidence.operations_dropped, 8);
}

#[tokio::test]
async fn the_admission_wait_and_recorded_operations_are_kept() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            record_admission_wait(Duration::from_millis(2));
            let recorder = OperationRecorder::start(
                "http.client",
                OperationKind::Client,
                &tracing::Span::current(),
            );
            let span_id = recorder.span_id().expect("the request collects evidence");
            recorder.finish(OperationOutcome::Completed);
            span_id.to_hex()
        }),
    ))
    .await;
    assert_eq!(evidence.admission_wait, Some(Duration::from_millis(2)));
    assert_eq!(evidence.operations.len(), 1);
    let client = &evidence.operations[0];
    assert_eq!(client.name, "http.client");
    assert_eq!(client.kind, OperationKind::Client);
    assert_eq!(client.parent_span_id, evidence.span_id);
}

#[tokio::test]
async fn outside_a_request_nothing_is_collected() {
    // Neither panics nor records anywhere.
    record_admission_wait(Duration::from_millis(1));
    let recorder =
        OperationRecorder::start("idle", OperationKind::Internal, &tracing::Span::none());
    assert_eq!(recorder.span_id(), None);
    recorder.finish(OperationOutcome::Completed);
    Operation::new("idle").run(async {}).await;

    // Without a sink, operations run without collecting anything.
    let service = TelemetryLayer::new(TelemetryConfig::default())
        .unwrap()
        .layer(Router::new().route(
            "/",
            get(|| async {
                let recorder = OperationRecorder::start(
                    "unsunk",
                    OperationKind::Internal,
                    &tracing::Span::current(),
                );
                assert_eq!(recorder.span_id(), None);
                "ok"
            }),
        ));
    let response = service
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    response.into_body().collect().await.unwrap();
}
