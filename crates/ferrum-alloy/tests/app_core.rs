//! End-to-end behavior of a standalone Alloy application over real TCP.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::extract::{Json, ValidJson, Validate, ValidationErrors};
use ferrum_alloy::health::CheckError;
use ferrum_alloy::telemetry::RequestContext;
use http::Request;
use http_body_util::{BodyExt, Full};
use serde::Deserialize;
use support::{TOKEN, config, fetch, fetch_with, raw, send, start};
use tokio::sync::Notify;
use tower::ServiceExt;

#[derive(Deserialize)]
struct NewOrder {
    item: String,
    quantity: u32,
}

impl Validate for NewOrder {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::default();
        if self.quantity == 0 {
            errors.add("/quantity", "must be at least 1");
        }
        if self.item.is_empty() {
            errors.add("/item", "must not be empty");
        }
        errors.into_result()
    }
}

#[derive(Clone)]
struct AppState {
    calls: Arc<AtomicUsize>,
}

fn router() -> Router {
    let state = AppState {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    Router::new()
        .route("/hello", get(|| async { "Hello from Ferrum Alloy" }))
        .route(
            "/state",
            get(|State(state): State<AppState>| async move {
                state.calls.fetch_add(1, Ordering::Relaxed).to_string()
            }),
        )
        .route(
            "/orders",
            post(|Json(order): Json<NewOrder>| async move {
                format!("{}x{}", order.quantity, order.item)
            }),
        )
        .route(
            "/validated",
            post(|ValidJson(order): ValidJson<NewOrder>| async move { order.item }),
        )
        .route(
            "/context",
            get(|context: RequestContext| async move {
                format!(
                    "{}|{}|{}",
                    context.request_id,
                    context.trace_id,
                    context.trace_decision.as_str()
                )
            }),
        )
        .route(
            "/panic",
            get(|| async {
                if std::hint::black_box(true) {
                    panic!("secret-internal-detail");
                }
                "unreachable"
            }),
        )
        .route(
            "/custom-404",
            get(|| async { (StatusCode::NOT_FOUND, "application says no").into_response() }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                "slow"
            }),
        )
        .route(
            "/stream",
            get(|| async {
                let (mut sender, body) =
                    http_body_util::channel::Channel::<Bytes, Infallible>::new(4);
                tokio::spawn(async move {
                    for i in 0..6 {
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        if sender
                            .send_data(Bytes::from(format!("data: {i}\n\n")))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::new(body))
                    .unwrap()
            }),
        )
        .route(
            "/forever",
            get(|| async {
                let (mut sender, body) =
                    http_body_util::channel::Channel::<Bytes, Infallible>::new(1);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if sender
                            .send_data(Bytes::from_static(b"tick\n"))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
                Body::new(body)
            }),
        )
        .with_state(state)
}

fn json_post(url: &str, body: &'static str, content_type: &str) -> Request<Full<Bytes>> {
    Request::post(url)
        .header("content-type", content_type)
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .unwrap()
}

#[tokio::test]
async fn routes_state_and_minimal_health_work() {
    let server = start(AlloyApp::new("core-test").router(router()), config()).await;

    let reply = fetch(&server.url("/hello")).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.text(), "Hello from Ferrum Alloy");
    assert!(
        !reply.headers.contains_key("x-request-id"),
        "a shared cache may store a bare GET 200, so the id is not echoed"
    );

    assert_eq!(fetch(&server.url("/state")).await.text(), "0");
    assert_eq!(fetch(&server.url("/state")).await.text(), "1");

    let live = fetch(&server.url("/livez")).await;
    assert_eq!(live.json(), serde_json::json!({ "status": "ok" }));
    assert_eq!(live.headers["cache-control"], "no-store");
    let ready = fetch(&server.url("/readyz")).await;
    assert_eq!(ready.status, 200);
    assert_eq!(ready.json(), serde_json::json!({ "status": "ready" }));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn framework_errors_are_problem_details_and_app_bodies_are_untouched() {
    let mut cfg = config();
    cfg.server.request_body_limit_bytes = 64;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;

    let not_found = fetch(&server.url("/missing")).await;
    assert_eq!(not_found.status, 404);
    assert_eq!(
        not_found.headers["content-type"],
        "application/problem+json"
    );
    assert_eq!(
        not_found.json()["type"],
        "tag:ferrumedge.com,2026:alloy/problem/route-not-found"
    );

    let method = send(
        Request::delete(server.url("/hello"))
            .body(Full::default())
            .unwrap(),
    )
    .await;
    assert_eq!(method.status, 405);
    assert_eq!(method.json()["status"], 405);
    assert!(method.headers.contains_key("allow"));
    assert!(
        method.headers.contains_key("x-request-id"),
        "DELETE is not storable"
    );

    let malformed = send(json_post(
        &server.url("/orders"),
        "{not json",
        "application/json",
    ))
    .await;
    assert_eq!(malformed.status, 400);
    assert!(
        malformed.json()["type"]
            .as_str()
            .unwrap()
            .ends_with("malformed-json")
    );

    let shape = send(json_post(
        &server.url("/orders"),
        r#"{"item":"tea"}"#,
        "application/json",
    ))
    .await;
    assert_eq!(shape.status, 422);
    assert!(
        shape.json()["detail"]
            .as_str()
            .unwrap()
            .contains("quantity")
    );

    let media = send(json_post(&server.url("/orders"), r#"{}"#, "text/plain")).await;
    assert_eq!(media.status, 415);

    let invalid = send(json_post(
        &server.url("/validated"),
        r#"{"item":"","quantity":0}"#,
        "application/json",
    ))
    .await;
    assert_eq!(invalid.status, 422);
    assert_eq!(invalid.json()["errors"].as_array().unwrap().len(), 2);

    let too_large = send(json_post(
        &server.url("/orders"),
        r#"{"item":"this body is definitely longer than sixty-four bytes","quantity":1}"#,
        "application/json",
    ))
    .await;
    assert_eq!(too_large.status, 413);
    assert_eq!(too_large.json()["limit_bytes"], 64);

    let panic = fetch(&server.url("/panic")).await;
    assert_eq!(panic.status, 500);
    assert!(
        !panic.text().contains("secret-internal-detail"),
        "panic text must not leak"
    );
    assert!(
        panic.json()["type"]
            .as_str()
            .unwrap()
            .ends_with("/internal")
    );

    let custom = fetch(&server.url("/custom-404")).await;
    assert_eq!(custom.status, 404);
    assert_eq!(
        custom.text(),
        "application says no",
        "application bodies are never rewritten"
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn chunked_bodies_without_content_length_are_still_limited() {
    let mut cfg = config();
    cfg.server.request_body_limit_bytes = 32;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let body = format!("{{\"item\":\"{}\",\"quantity\":1}}", "x".repeat(100));
    let request = format!(
        "POST /orders HTTP/1.1\r\nhost: t\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        body.len()
    );
    let response = raw(server.addr, request.as_bytes(), Duration::from_secs(2)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 413"), "{text}");
    assert!(text.contains("payload-too-large"), "{text}");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn request_context_and_trace_policy_reach_handlers() {
    let server = start(AlloyApp::new("core-test").router(router()), config()).await;
    let reply = fetch_with(
        &server.url("/context"),
        &[
            ("x-request-id", "req-abc"),
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
        ],
    )
    .await;
    let text = reply.text();
    let parts: Vec<&str> = text.split('|').collect();
    assert_eq!(parts[0], "req-abc");
    assert_ne!(
        parts[1], "4bf92f3577b34da6a3ce929d0e0e4736",
        "untrusted peer is re-rooted"
    );
    assert_eq!(parts[2], "rerooted_untrusted");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn trusted_network_peers_propagate_trace_context() {
    let mut cfg = config();
    cfg.trust.networks = vec!["127.0.0.0/8".parse().unwrap()];
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let reply = fetch_with(
        &server.url("/context"),
        &[(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )],
    )
    .await;
    let text = reply.text();
    assert!(
        text.contains("|4bf92f3577b34da6a3ce929d0e0e4736|accepted_remote"),
        "{text}"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_requires_the_token_for_detail_and_metrics() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let app = AlloyApp::new("core-test")
        .router(router())
        .readiness_check("database", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(CheckError::new("connection refused by db.internal:5432"))
            }
        });
    let server = start(app, config()).await;
    fetch(&server.url("/hello")).await;

    let live = fetch(&server.management_url("/livez")).await;
    assert_eq!(live.status, 200);
    let ready = fetch(&server.url("/readyz")).await;
    assert_eq!(ready.status, 503);
    assert_eq!(
        ready.json(),
        serde_json::json!({ "status": "not_ready" }),
        "no check names or errors in public readiness"
    );

    let metrics = fetch(&server.management_url("/metrics")).await;
    assert_eq!(metrics.status, 401);
    assert_eq!(metrics.headers["www-authenticate"], "Bearer");
    let wrong = fetch_with(
        &server.management_url("/metrics"),
        &[("authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(wrong.status, 401);

    let bearer = format!("Bearer {TOKEN}");
    let metrics = fetch_with(
        &server.management_url("/metrics"),
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(metrics.status, 200);
    let text = metrics.text();
    assert!(text.contains("http_route=\"/hello\""), "{text}");
    assert!(text.contains("ferrum_alloy_active_connections{listener=\"app\"}"));

    let health = fetch_with(
        &server.management_url("/health"),
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(health.status, 503);
    let body = health.json();
    assert_eq!(body["readiness"]["checks"][0]["name"], "database");
    assert!(
        body["readiness"]["checks"][0]["error"]
            .as_str()
            .unwrap()
            .contains("refused")
    );
    assert_eq!(body["service"]["name"], "core-test");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn readiness_checks_are_cached_and_single_flight() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let app = AlloyApp::new("core-test")
        .router(router())
        .readiness_check("slow-db", move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok::<(), CheckError>(())
            }
        });
    let mut cfg = config();
    cfg.health.cache_ttl_ms = 60_000;
    let server = start(app, cfg).await;
    let url = server.url("/readyz");
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let url = url.clone();
        tasks.push(tokio::spawn(async move { fetch(&url).await.status }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), 200);
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "a flood of readiness requests runs the check once"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failing_checks_time_out_instead_of_hanging() {
    let app = AlloyApp::new("core-test")
        .router(router())
        .readiness_check("hang", || async {
            std::future::pending::<()>().await;
            Ok::<(), CheckError>(())
        });
    let mut cfg = config();
    cfg.health.check_timeout_ms = 100;
    let server = start(app, cfg).await;
    let started = Instant::now();
    let ready = fetch(&server.url("/readyz")).await;
    assert_eq!(ready.status, 503);
    assert!(started.elapsed() < Duration::from_secs(2));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_headers_deadline_does_not_cut_streams() {
    let mut cfg = config();
    cfg.server.request_timeout_ms = 150;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;

    let slow = fetch(&server.url("/slow")).await;
    assert_eq!(slow.status, 503);
    assert!(
        slow.json()["type"]
            .as_str()
            .unwrap()
            .ends_with("request-timeout")
    );

    // Headers arrive immediately; the body streams for ~360 ms, beyond the
    // 150 ms deadline, and must complete.
    let stream = fetch(&server.url("/stream")).await;
    assert_eq!(stream.status, 200);
    assert_eq!(stream.text().matches("data:").count(), 6);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn admission_limits_reject_excess_concurrency() {
    let mut cfg = config();
    cfg.server.max_in_flight_requests = 1;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let first = tokio::spawn({
        let url = server.url("/slow");
        async move { fetch(&url).await.status }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = fetch(&server.url("/slow")).await;
    assert_eq!(second.status, 503);
    assert!(
        second.json()["type"]
            .as_str()
            .unwrap()
            .ends_with("overloaded")
    );
    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(fetch(&server.url("/slow")).await.status, 200);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn liveness_takes_no_admission_permit() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let held = {
        let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
        move || async move {
            entered.notify_one();
            release.notified().await;
            "released"
        }
    };
    let router = Router::new()
        .route("/held", get(held))
        .route("/hello", get(|| async { "hello" }));
    let mut cfg = config();
    cfg.server.max_in_flight_requests = 1;
    cfg.server.admission_wait_timeout_ms = 0;
    let parts = AlloyApp::new("core-test")
        .router(router)
        .config(cfg)
        .telemetry(ferrum_alloy::TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap();
    let call = |method: &str, path: &str| {
        let request = Request::builder().method(method).uri(path);
        let request = request.body(Body::empty()).unwrap();
        parts.router.clone().oneshot(request)
    };
    assert_eq!(call("GET", "/livez").await.unwrap().status(), 200);

    // The only permit is held until `release`.
    let slow = tokio::spawn(call("GET", "/held"));
    entered.notified().await;

    let live = call("GET", "/livez").await.unwrap();
    assert_eq!(live.status(), StatusCode::OK, "liveness is not refused");
    assert_eq!(live.headers()["cache-control"], "no-store");
    // It still has the other layers: axum's `405` becomes a problem.
    let post = call("POST", "/livez").await.unwrap();
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(post.headers()["content-type"], "application/problem+json");
    // Business requests and readiness still get the overload response.
    for path in ["/hello", "/readyz"] {
        let overloaded = call("GET", path).await.unwrap();
        assert_eq!(
            overloaded.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{path}"
        );
        let body = overloaded.into_body().collect().await.unwrap().to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let kind = problem["type"].as_str().unwrap();
        assert!(kind.ends_with("overloaded"), "{path}: {problem}");
    }

    release.notify_one();
    let slow = slow.await.unwrap().unwrap();
    assert_eq!(slow.status(), StatusCode::OK);
    assert_eq!(call("GET", "/hello").await.unwrap().status(), 200);
    drop(parts);
}

#[tokio::test]
async fn oversized_request_heads_are_rejected() {
    let mut cfg = config();
    cfg.server.max_header_count = 20;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let mut request = String::from("GET /hello HTTP/1.1\r\nhost: t\r\n");
    for i in 0..40 {
        request.push_str(&format!("x-filler-{i}: v\r\n"));
    }
    request.push_str("connection: close\r\n\r\n");
    let response = raw(server.addr, request.as_bytes(), Duration::from_secs(2)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 431"), "{text}");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_request_heads_are_cut_off() {
    let mut cfg = config();
    cfg.server.header_read_timeout_ms = 200;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let started = Instant::now();
    let response = raw(
        server.addr,
        b"GET /hello HTTP/1.1\r\nhost: t\r\n",
        Duration::from_secs(3),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "connection closed by the header read timeout"
    );
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.is_empty() || text.starts_with("HTTP/1.1 408"),
        "{text}"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn connections_beyond_the_limit_are_closed() {
    let mut cfg = config();
    cfg.server.max_connections = 1;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let held = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let response = raw(
        server.addr,
        b"GET /hello HTTP/1.1\r\nhost: t\r\n\r\n",
        Duration::from_secs(1),
    )
    .await;
    assert!(
        response.is_empty(),
        "second connection is closed without a response"
    );
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fetch(&server.url("/hello")).await.status, 200);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn http2_prior_knowledge_is_served() {
    let server = start(AlloyApp::new("core-test").router(router()), config()).await;
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(true)
        .build_http::<Full<Bytes>>();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let client = client.clone();
        let url = server.url("/hello");
        tasks.push(tokio::spawn(async move {
            let response = client.get(url.parse().unwrap()).await.unwrap();
            (response.version(), response.status())
        }));
    }
    for task in tasks {
        let (version, status) = task.await.unwrap();
        assert_eq!(version, http::Version::HTTP_2);
        assert_eq!(status, 200);
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn graceful_shutdown_drains_in_flight_streams() {
    let mut cfg = config();
    cfg.shutdown.readiness_grace_ms = 300;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let stream = tokio::spawn({
        let url = server.url("/stream");
        async move { fetch(&url).await }
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    server.lifecycle.trigger_shutdown();
    // During the readiness grace period the listener still answers, reporting draining.
    let ready = fetch(&server.url("/readyz")).await;
    assert_eq!(ready.status, 503);
    assert_eq!(ready.json(), serde_json::json!({ "status": "draining" }));
    let reply = stream.await.unwrap();
    assert_eq!(
        reply.text().matches("data:").count(),
        6,
        "the in-flight stream completed"
    );
    tokio::time::timeout(Duration::from_secs(5), server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn the_drain_budget_force_closes_endless_streams() {
    let mut cfg = config();
    cfg.shutdown.drain_timeout_ms = 300;
    let server = start(AlloyApp::new("core-test").router(router()), cfg).await;
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /forever HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf).await.unwrap();
    let started = Instant::now();
    server.lifecycle.trigger_shutdown();
    tokio::time::timeout(Duration::from_secs(5), server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250),
        "waited for the drain budget ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "then force-closed ({elapsed:?})"
    );
}

#[tokio::test]
async fn websocket_upgrades_pass_through() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let router = Router::new().route(
        "/ws",
        get(|upgrade: WebSocketUpgrade| async move {
            upgrade.on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if let Message::Text(text) = message {
                        let _ = socket
                            .send(Message::Text(format!("echo:{text}").into()))
                            .await;
                    }
                }
            })
        }),
    );
    let server = start(AlloyApp::new("ws-test").router(router), config()).await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(
            b"GET /ws HTTP/1.1\r\nhost: t\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\nsec-websocket-version: 13\r\n\r\n",
        )
        .await
        .unwrap();
    let mut head = vec![0u8; 1024];
    let n = stream.read(&mut head).await.unwrap();
    let text = String::from_utf8_lossy(&head[..n]);
    assert!(text.starts_with("HTTP/1.1 101"), "{text}");
    assert!(text.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{text}");
    // Masked text frame "hi".
    let mask = [1u8, 2, 3, 4];
    let payload: Vec<u8> = b"hi"
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % 4])
        .collect();
    let mut frame = vec![0x81, 0x80 | 2];
    frame.extend_from_slice(&mask);
    frame.extend_from_slice(&payload);
    stream.write_all(&frame).await.unwrap();
    let mut reply = [0u8; 16];
    let n = stream.read(&mut reply).await.unwrap();
    assert_eq!(&reply[2..n], b"echo:hi");
    drop(stream);

    let bearer = format!("Bearer {TOKEN}");
    let metrics = fetch_with(
        &server.management_url("/metrics"),
        &[("authorization", &bearer)],
    )
    .await;
    assert!(
        metrics
            .text()
            .contains("ferrum_alloy_response_body_outcomes_total{outcome=\"upgraded\"} 1"),
        "{}",
        metrics.text()
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn into_parts_router_can_be_served_by_plain_axum() {
    let mut cfg = config();
    cfg.management.enabled = false;
    let parts = AlloyApp::new("parts-test")
        .router(router())
        .config(cfg)
        .telemetry(ferrum_alloy::TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap();
    assert!(parts.management_router.is_none());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = parts.router.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let reply = fetch(&format!("http://{addr}/hello")).await;
    assert_eq!(reply.status, 200);
    let missing = fetch(&format!("http://{addr}/missing")).await;
    assert_eq!(missing.headers["content-type"], "application/problem+json");
    server.abort();
    drop(parts);
}

#[tokio::test]
async fn missing_router_is_an_error() {
    let error = AlloyApp::new("x")
        .config(config())
        .telemetry(ferrum_alloy::TelemetryInit::ApplicationOwned)
        .into_parts()
        .unwrap_err();
    assert!(matches!(error, ferrum_alloy::AlloyError::MissingRouter));
}
