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
use ferrum_alloy_telemetry::{TelemetryConfig, TelemetryLayer, TelemetryService};
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

/// An inner telemetry-wrapped service, with `sink` when given, whose
/// handler records a pool wait and runs one operation.
fn inner_service(sink: Option<Arc<Collect>>) -> TelemetryService<Router> {
    let layer = TelemetryLayer::new(TelemetryConfig::default()).unwrap();
    let layer = match sink {
        Some(sink) => layer.with_evidence_sink(sink),
        None => layer,
    };
    layer.layer(Router::new().route(
        "/",
        get(|| async {
            record_pool_wait(Duration::from_millis(7));
            Operation::new("inner.op").run(async {}).await;
            "inner"
        }),
    ))
}

#[tokio::test]
async fn a_nested_request_keeps_its_own_evidence() {
    for inner_sunk in [true, false] {
        let inner_sink = Arc::new(Collect::default());
        let inner = inner_service(inner_sunk.then(|| inner_sink.clone()));
        let evidence = evidence_of(Router::new().route(
            "/",
            get(move || {
                let inner = inner.clone();
                async move {
                    Operation::new("outer.call")
                        .run(async move {
                            let response = inner
                                .oneshot(Request::get("/").body(Body::empty()).unwrap())
                                .await
                                .unwrap();
                            response.into_body().collect().await.unwrap();
                            // Back in the outer operation after the inner
                            // request finished.
                            Operation::new("outer.after").run(async {}).await;
                        })
                        .await;
                    "outer"
                }
            }),
        ))
        .await;

        let names: Vec<&str> = evidence.operations.iter().map(|o| o.name).collect();
        assert_eq!(
            names,
            ["outer.after", "outer.call"],
            "inner_sunk={inner_sunk}: no inner operation in the outer record"
        );
        let (after, call) = (&evidence.operations[0], &evidence.operations[1]);
        assert_eq!(call.parent_span_id, evidence.span_id);
        assert_eq!(after.parent_span_id, call.span_id, "outer scope restored");
        assert_eq!(
            call.pool_wait, None,
            "inner_sunk={inner_sunk}: the inner request's pool wait is its own"
        );

        let inner_records = inner_sink.0.lock().unwrap();
        if inner_sunk {
            assert_eq!(inner_records.len(), 1);
            let inner = &inner_records[0];
            assert_ne!(inner.span_id, evidence.span_id);
            let names: Vec<&str> = inner.operations.iter().map(|o| o.name).collect();
            assert_eq!(names, ["inner.op"]);
            assert_eq!(
                inner.operations[0].parent_span_id, inner.span_id,
                "parented on the inner server span, not outer.call"
            );
        } else {
            assert!(inner_records.is_empty());
        }
    }
}

#[tokio::test]
async fn interleaved_requests_on_one_task_keep_their_own_evidence() {
    let sink = Arc::new(Collect::default());
    let service = TelemetryLayer::new(TelemetryConfig::default())
        .unwrap()
        .with_evidence_sink(sink.clone())
        .layer(Router::new().route(
            "/{name}",
            get(
                |axum::extract::Path(name): axum::extract::Path<String>| async move {
                    let name: &'static str = if name == "a" { "op.a" } else { "op.b" };
                    for _ in 0..3 {
                        Operation::new(name)
                            .run(async {
                                // Yield so the other request is polled in
                                // between, on the same task.
                                tokio::task::yield_now().await;
                            })
                            .await;
                        tokio::task::yield_now().await;
                    }
                    "ok"
                },
            ),
        ));
    let request = |path: &str| {
        let service = service.clone();
        let request = Request::get(path).body(Body::empty()).unwrap();
        async move {
            let response = service.oneshot(request).await.unwrap();
            response.into_body().collect().await.unwrap();
        }
    };
    tokio::join!(request("/a"), request("/b"));

    let records = sink.0.lock().unwrap();
    assert_eq!(records.len(), 2);
    for record in records.iter() {
        assert_eq!(record.operations.len(), 3, "{:?}", record.operations);
        let first = record.operations[0].name;
        for operation in &record.operations {
            assert_eq!(operation.name, first, "no cross-attribution");
            assert_eq!(operation.parent_span_id, record.span_id);
        }
    }
    assert_ne!(records[0].operations[0].name, records[1].operations[0].name);
}

/// A body that ends while it still owns a pending operation: the stream
/// stopped (a `take_until`-style cutoff) before the operation finished.
struct StopsWhilePending {
    operation: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    stop: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl http_body::Body for StopsWhilePending {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        // Starts the operation on the first poll; it never completes.
        let _ = std::future::Future::poll(self.operation.as_mut(), cx);
        match std::future::Future::poll(self.stop.as_mut(), cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[tokio::test]
async fn an_operation_still_owned_by_an_ended_body_is_kept_as_cancelled() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            Body::new(StopsWhilePending {
                operation: Box::pin(
                    Operation::new("stream.pending").run(std::future::pending::<()>()),
                ),
                stop: Box::pin(tokio::time::sleep(Duration::from_millis(5))),
            })
        }),
    ))
    .await;
    let names: Vec<&str> = evidence.operations.iter().map(|o| o.name).collect();
    assert_eq!(
        names,
        ["stream.pending"],
        "recorded before the evidence closed"
    );
    let pending = &evidence.operations[0];
    assert_eq!(pending.outcome, OperationOutcome::Cancelled);
    assert_eq!(pending.parent_span_id, evidence.span_id);
}

#[tokio::test]
async fn a_recorder_scope_is_stack_ordered_and_places_its_pool_wait() {
    let evidence = evidence_of(Router::new().route(
        "/",
        get(|| async {
            let span = tracing::Span::current();
            let outer = OperationRecorder::start("outer", OperationKind::Db, &span);
            outer.in_scope(|| {
                std::thread::sleep(Duration::from_millis(2));
                record_pool_wait(Duration::from_millis(1));
                let inner = OperationRecorder::start("inner", OperationKind::Db, &span);
                inner.in_scope(|| record_pool_wait(Duration::from_millis(7)));
                inner.finish(OperationOutcome::Completed);
                // Back in `outer` once the inner scope returned.
                let sibling = OperationRecorder::start("sibling", OperationKind::Internal, &span);
                sibling.finish(OperationOutcome::Completed);
            });
            // Nothing is current after the outer scope returned.
            record_pool_wait(Duration::from_millis(99));
            let after = OperationRecorder::start("after", OperationKind::Internal, &span);
            after.finish(OperationOutcome::Completed);
            outer.finish(OperationOutcome::Completed);
            "ok"
        }),
    ))
    .await;
    let by_name = |name: &str| {
        evidence
            .operations
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("{name}: {:?}", evidence.operations))
    };
    let (outer, inner, sibling, after) = (
        by_name("outer"),
        by_name("inner"),
        by_name("sibling"),
        by_name("after"),
    );
    assert_eq!(outer.parent_span_id, evidence.span_id);
    assert_eq!(inner.parent_span_id, outer.span_id);
    assert_eq!(sibling.parent_span_id, outer.span_id, "outer restored");
    assert_eq!(after.parent_span_id, evidence.span_id, "nothing current");
    assert_eq!(inner.pool_wait, Some(Duration::from_millis(7)));
    assert_eq!(outer.pool_wait, Some(Duration::from_millis(1)), "not 99");
    // The wait was recorded inside the operation, after its 2 ms of work.
    let ended = outer.pool_wait_ended.expect("placed");
    assert!(
        ended >= outer.started + Duration::from_millis(2),
        "{outer:?}"
    );
    assert!(ended <= outer.started + outer.duration, "{outer:?}");
}

#[tokio::test]
async fn a_context_from_another_request_is_not_recorded_in_this_one() {
    let inner_sink = Arc::new(Collect::default());
    let sink = inner_sink.clone();
    let evidence = evidence_of(Router::new().route(
        "/",
        get(move |outer: ferrum_alloy_telemetry::RequestContext| {
            let sink = sink.clone();
            async move {
                let outer_span = outer.span_id;
                let inner = TelemetryLayer::new(TelemetryConfig::default())
                    .unwrap()
                    .with_evidence_sink(sink)
                    .layer(Router::new().route(
                        "/",
                        get(
                            move |own: ferrum_alloy_telemetry::RequestContext| async move {
                                let span = tracing::Span::current();
                                // Carried in from the outer request: its trace,
                                // not this one's.
                                let foreign = OperationRecorder::start_in_request(
                                    "http.client",
                                    OperationKind::Client,
                                    &span,
                                    outer_span,
                                );
                                assert_eq!(foreign.span_id(), None);
                                foreign.finish(OperationOutcome::Completed);
                                let local = OperationRecorder::start_in_request(
                                    "http.client",
                                    OperationKind::Client,
                                    &span,
                                    own.span_id,
                                );
                                assert!(local.span_id().is_some());
                                local.finish(OperationOutcome::Completed);
                                "inner"
                            },
                        ),
                    ));
                let response = inner
                    .oneshot(Request::get("/").body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                response.into_body().collect().await.unwrap();
                "outer"
            }
        }),
    ))
    .await;
    assert!(evidence.operations.is_empty(), "{:?}", evidence.operations);
    let inner_records = inner_sink.0.lock().unwrap();
    assert_eq!(inner_records.len(), 1);
    let inner = &inner_records[0];
    assert_eq!(inner.operations.len(), 1, "{:?}", inner.operations);
    assert_eq!(inner.operations[0].parent_span_id, inner.span_id);
}
