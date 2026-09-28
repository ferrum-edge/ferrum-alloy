//! PostgreSQL integration. Tests marked `#[ignore]` need a real database at
//! `FERRUM_ALLOY_TEST_DATABASE_URL`; CI provides one in a service container.
//!
//! The fault-injection tests hold pooled connections, sleep inside the
//! database, and terminate backends. Every fault stays inside this test
//! process or the database it is given. Span fields are captured with a
//! test-local subscriber and, where a diagnosis is asserted, converted to
//! OTLP/JSON and fed through the public offline importer.

#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use ferrum_alloy::config::{DatabaseSettings, Secret};
use ferrum_alloy::health::HealthCheck;
use ferrum_alloy::{AlloyApp, postgres};
use ferrum_alloy_diagnostics::model::{
    Confidence, DiagnosticReport, Producer, ProducerKind, Severity,
};
use ferrum_alloy_diagnostics::otlp::{ImportLimits, import};
use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
use serde_json::{Value, json};
use sqlx::{Connection, PgPool};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

fn settings(url: &str) -> DatabaseSettings {
    let mut settings = DatabaseSettings::default();
    settings.url = Some(Secret::new(url));
    settings.acquire_timeout_ms = 500;
    settings
}

fn database_url() -> String {
    std::env::var("FERRUM_ALLOY_TEST_DATABASE_URL")
        .expect("set FERRUM_ALLOY_TEST_DATABASE_URL to run database tests")
}

#[tokio::test]
async fn invalid_urls_fail_without_revealing_the_secret() {
    let error = postgres::connect(&settings("mysql://user:hunter2@db/x"), "test").unwrap_err();
    let text = error.to_string();
    assert!(text.contains("not a valid PostgreSQL URL"), "{text}");
    assert!(!text.contains("hunter2"));
    assert!(
        postgres::connect(&DatabaseSettings::default(), "test").is_err(),
        "missing url"
    );
    assert!(postgres::connect(&settings("postgres://u:hunter2@[::1:bad/x"), "test").is_err());
}

#[test]
fn connect_outside_a_runtime_is_an_error_not_a_panic() {
    let error = postgres::connect(
        &settings("postgres://alloy:alloy@127.0.0.1:1/alloy"),
        "test",
    )
    .unwrap_err();
    assert!(error.to_string().contains("Tokio runtime"));
}

#[tokio::test]
async fn an_unreachable_database_is_not_ready_within_the_acquire_timeout() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let pool = postgres::connect(
        &settings(&format!(
            "postgres://alloy:alloy@127.0.0.1:{}/alloy",
            closed.port()
        )),
        "test",
    )
    .unwrap();
    let started = Instant::now();
    let result = postgres::readiness(pool).check().await;
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded by acquire_timeout_ms"
    );
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn migrations_transactions_and_timeouts_behave() {
    let mut cfg = settings(&database_url());
    cfg.statement_timeout_ms = Some(200);
    let pool = postgres::connect(&cfg, "ferrum-alloy-test").unwrap();
    postgres::readiness(pool.clone()).check().await.unwrap();

    sqlx::query("DROP TABLE IF EXISTS alloy_test_items")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS _sqlx_migrations")
        .execute(&pool)
        .await
        .unwrap();
    let migrator = sqlx::migrate::Migrator::new(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/migrations"
    )))
    .await
    .unwrap();
    postgres::migrate(&pool, &migrator).await.unwrap();
    postgres::migrate(&pool, &migrator).await.unwrap(); // idempotent

    // Rolled-back transactions leave no rows.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO alloy_test_items (name) VALUES ($1)")
        .bind("rolled back")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM alloy_test_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Committed work through an instrumented, measured acquire.
    let mut connection = postgres::acquire(&pool).await.unwrap();
    let inserted: i64 = postgres::query("items.insert", "INSERT", "INSERT alloy_test_items")
        .run_result(
            sqlx::query_scalar("INSERT INTO alloy_test_items (name) VALUES ($1) RETURNING id")
                .bind("kept")
                .fetch_one(&mut *connection),
        )
        .await
        .unwrap();
    assert!(inserted > 0);

    // statement_timeout is enforced server-side.
    let slow = sqlx::query("SELECT pg_sleep(1)").execute(&pool).await;
    let message = slow.unwrap_err().to_string();
    assert!(message.contains("statement timeout"), "{message}");
}

/// Trace id stamped on every exported span (one request per capture).
const TRACE_ID: &str = "5f0e1d2c3b4a59687766554433221100";

/// A span as a test-local subscriber saw it.
#[derive(Debug, Clone)]
struct CapturedSpan {
    name: &'static str,
    parent: Option<usize>,
    fields: BTreeMap<&'static str, Value>,
    start: SystemTime,
    end: Option<SystemTime>,
}

impl CapturedSpan {
    fn str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    fn f64(&self, key: &str) -> f64 {
        self.fields
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("{key} not recorded on {self:?}"))
    }
}

#[derive(Default)]
struct CaptureState {
    spans: Vec<CapturedSpan>,
    // Tracing reuses ids after close, so live ids map to capture indices.
    open: HashMap<u64, usize>,
}

/// Records span fields and wall-clock start and end times. Installed with
/// `set_default` on a current-thread runtime, so it sees only this test.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<CaptureState>>);

struct Fields<'a>(&'a mut BTreeMap<&'static str, Value>);

impl Visit for Fields<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name(), json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name(), json!(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name(), Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name(), Value::from(format!("{value:?}")));
    }
}

impl<S> tracing_subscriber::Layer<S> for Capture
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .map(|parent| parent.id().into_u64());
        let mut fields = BTreeMap::new();
        attrs.record(&mut Fields(&mut fields));
        let mut state = self.0.lock().unwrap();
        let parent = parent.and_then(|parent| state.open.get(&parent).copied());
        state.spans.push(CapturedSpan {
            name: attrs.metadata().name(),
            parent,
            fields,
            start: SystemTime::now(),
            end: None,
        });
        let index = state.spans.len() - 1;
        state.open.insert(id.into_u64(), index);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut state = self.0.lock().unwrap();
        let Some(index) = state.open.get(&id.into_u64()).copied() else {
            return;
        };
        values.record(&mut Fields(&mut state.spans[index].fields));
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let mut state = self.0.lock().unwrap();
        if let Some(index) = state.open.remove(&id.into_u64()) {
            state.spans[index].end = Some(SystemTime::now());
        }
    }
}

impl Capture {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
    }

    fn spans(&self) -> Vec<CapturedSpan> {
        self.0.lock().unwrap().spans.clone()
    }

    fn operation(&self, name: &str) -> CapturedSpan {
        self.spans()
            .into_iter()
            .find(|s| s.name == "alloy.operation" && s.str("alloy.operation.name") == Some(name))
            .unwrap_or_else(|| panic!("no operation span named {name}"))
    }

    /// Waits for the request's server span to be finalized and closed.
    async fn closed_server_span(&self) -> CapturedSpan {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let closed = self
                .spans()
                .into_iter()
                .find(|s| s.name == "http.server.request" && s.end.is_some());
            if let Some(span) = closed {
                return span;
            }
            assert!(Instant::now() < deadline, "server span never closed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The captured server and operation spans as one OTLP/JSON export,
    /// shaped like the OpenTelemetry bridge's output. Spans in between are
    /// skipped and children re-parented to the nearest exported ancestor.
    fn otlp_json(&self) -> String {
        let spans = self.spans();
        let exported =
            |span: &CapturedSpan| matches!(span.name, "http.server.request" | "alloy.operation");
        let items: Vec<Value> = spans
            .iter()
            .enumerate()
            .filter(|(_, span)| exported(span))
            .map(|(index, span)| {
                let mut parent = span.parent;
                while let Some(candidate) = parent {
                    if exported(&spans[candidate]) {
                        break;
                    }
                    parent = spans[candidate].parent;
                }
                let kind = match span.str("otel.kind") {
                    Some("server") => 2,
                    Some("client") => 3,
                    _ => 1,
                };
                let attributes: Vec<Value> = span
                    .fields
                    .iter()
                    .filter(|(key, _)| !key.starts_with("otel."))
                    .map(|(key, value)| json!({ "key": key, "value": otlp_value(value) }))
                    .collect();
                let end = span.end.unwrap_or_else(SystemTime::now);
                let mut item = json!({
                    "traceId": TRACE_ID,
                    "spanId": span_id(index),
                    "name": span.str("otel.name").unwrap_or(span.name),
                    "kind": kind,
                    "startTimeUnixNano": unix_nanos(span.start).to_string(),
                    "endTimeUnixNano": unix_nanos(end).to_string(),
                    "attributes": attributes,
                });
                if let Some(parent) = parent {
                    item["parentSpanId"] = Value::from(span_id(parent));
                }
                item
            })
            .collect();
        let resource = json!([
            { "key": "service.name", "value": { "stringValue": "fault-injection" } },
            { "key": "service.instance.id", "value": { "stringValue": "fault-injection-1" } },
        ]);
        json!({
            "resourceSpans": [{
                "resource": { "attributes": resource },
                "scopeSpans": [{ "scope": { "name": "ferrum-alloy-telemetry" }, "spans": items }],
            }]
        })
        .to_string()
    }
}

fn span_id(index: usize) -> String {
    format!("{:016x}", index + 1)
}

fn unix_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

fn otlp_value(value: &Value) -> Value {
    match value {
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) if number.is_f64() => json!({ "doubleValue": number }),
        Value::Number(number) => json!({ "intValue": number.to_string() }),
        Value::String(text) => json!({ "stringValue": text }),
        other => json!({ "stringValue": other.to_string() }),
    }
}

fn diagnose(capture: &Capture) -> DiagnosticReport {
    let collector = Producer {
        kind: ProducerKind::Collector,
        name: "ferrum-alloy-test".into(),
        version: None,
        instance: None,
    };
    import(
        &capture.otlp_json(),
        Some(TRACE_ID),
        collector,
        &ImportLimits::default(),
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn pool_wait_is_measured_separately_from_the_query() {
    let capture = Capture::default();
    let _guard = capture.install();
    let mut cfg = settings(&database_url());
    cfg.max_connections = 1;
    cfg.acquire_timeout_ms = 5_000;
    let pool = postgres::connect(&cfg, "ferrum-alloy-pool-wait").unwrap();
    let held = pool.acquire().await.unwrap();

    // The only connection is released after a known delay; the operation
    // waits for it inside its own span.
    let release = async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(held);
    };
    let wait_then_query = async {
        let mut connection = postgres::acquire(&pool).await?;
        sqlx::query("SELECT 1").execute(&mut *connection).await
    };
    let operation = postgres::query("pool.wait", "SELECT", "SELECT 1").run_result(wait_then_query);
    let ((), result) = tokio::join!(release, operation);
    result.unwrap();

    let span = capture.operation("pool.wait");
    let wait = span.f64("alloy.db.pool_wait_ms");
    let duration = span.f64("alloy.operation.duration_ms");
    assert!(
        (250.0..5_000.0).contains(&wait),
        "pool wait {wait} ms reflects the held connection"
    );
    assert!(
        duration >= wait,
        "the operation ({duration} ms) includes its pool wait ({wait} ms)"
    );
    assert_eq!(span.str("alloy.operation.outcome"), Some("completed"));
    assert_eq!(span.str("db.system.name"), Some("postgresql"));

    // The importer keeps the pool wait as its own measurement rather than
    // folding it into the operation duration.
    let report = diagnose(&capture);
    let named = |name: &str| {
        report
            .observations
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("no {name} observation"))
    };
    let pool_wait = named("alloy.db.pool_wait");
    let operation = named("alloy.operation.duration");
    assert_ne!(pool_wait.id, operation.id);
    assert_eq!(pool_wait.duration_ms(), Some(wait));
    assert_eq!(operation.duration_ms(), Some(duration));
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn an_exhausted_pool_fails_within_the_acquire_timeout() {
    let capture = Capture::default();
    let _guard = capture.install();
    let mut cfg = settings(&database_url());
    cfg.max_connections = 1;
    cfg.acquire_timeout_ms = 300;
    let pool = postgres::connect(&cfg, "ferrum-alloy-exhausted").unwrap();
    let held = pool.acquire().await.unwrap();

    let started = Instant::now();
    let result = postgres::query("pool.exhausted", "SELECT", "SELECT 1")
        .run_result(async { postgres::acquire(&pool).await.map(drop) })
        .await;
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(sqlx::Error::PoolTimedOut)),
        "{result:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(3),
        "bounded by acquire_timeout_ms: {elapsed:?}"
    );

    let span = capture.operation("pool.exhausted");
    let wait = span.f64("alloy.db.pool_wait_ms");
    assert!(
        (250.0..3_000.0).contains(&wait),
        "a failed acquire still records its wait: {wait} ms"
    );
    assert_eq!(span.str("alloy.operation.outcome"), Some("error"));
    assert_eq!(span.str("error.type"), Some("operation_error"));

    // The waiter gave up without taking a permit: once the holder releases
    // its connection the pool serves the next caller.
    drop(held);
    let mut connection = postgres::acquire(&pool).await.unwrap();
    sqlx::query("SELECT 1")
        .execute(&mut *connection)
        .await
        .unwrap();
    assert!(pool.size() <= 1);
}

async fn slow(State(pool): State<PgPool>) -> Result<&'static str, StatusCode> {
    postgres::query("orders.slow", "SELECT", "SELECT pg_sleep")
        .run_result(async {
            let mut connection = postgres::acquire(&pool).await?;
            sqlx::query("SELECT pg_sleep(0.3)")
                .execute(&mut *connection)
                .await
        })
        .await
        .map(|_| "slept")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn a_slow_query_dominates_time_to_headers_without_claiming_server_time() {
    let capture = Capture::default();
    let _guard = capture.install();
    let cfg = settings(&database_url());
    let pool = postgres::connect(&cfg, "ferrum-alloy-slow").unwrap();
    // Connect before the measured request.
    postgres::readiness(pool.clone()).check().await.unwrap();

    let router = Router::new()
        .route("/slow", get(slow))
        .with_state(pool.clone());
    let server = support::start(
        AlloyApp::new("fault-injection").router(router),
        support::config(),
    )
    .await;
    let reply = support::fetch(&server.url("/slow")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let server_span = capture.closed_server_span().await;
    server.shutdown().await.unwrap();

    // Telemetry: the application-observed call time reflects pg_sleep, and
    // the time to headers contains it.
    let operation = capture.operation("orders.slow");
    let op_ms = operation.f64("alloy.operation.duration_ms");
    let ttfb_ms = server_span.f64("alloy.server.time_to_headers_ms");
    assert!((250.0..10_000.0).contains(&op_ms), "operation {op_ms} ms");
    assert!(
        ttfb_ms >= op_ms,
        "time to headers {ttfb_ms} ms contains the operation {op_ms} ms"
    );
    assert!(
        operation.f64("alloy.db.pool_wait_ms") < op_ms,
        "pool wait is recorded separately"
    );
    assert_eq!(
        server_span.str("alloy.response.body.outcome"),
        Some("completed")
    );

    // Diagnosis: r002 places the operation inside the header phase.
    let report = diagnose(&capture);
    let findings = analyze(&report, &Thresholds::default());
    let codes: Vec<&str> = findings.iter().map(|f| f.code.as_str()).collect();
    assert!(
        !codes.contains(&"alloy.evidence.operation_exceeds_enclosing"),
        "{codes:?}"
    );
    let finding = findings
        .iter()
        .find(|f| f.code == "alloy.service.operation_dominates")
        .unwrap_or_else(|| panic!("no dominance finding: {codes:?}"));
    assert_eq!(
        finding.severity,
        Severity::Warning,
        "nested in the header phase"
    );
    assert_eq!(
        finding.confidence,
        Confidence::Likely,
        "offline input never yields confirmed"
    );
    assert!(finding.missing_evidence.is_empty(), "{finding:?}");
    assert!(
        finding
            .does_not_prove
            .iter()
            .any(|text| text.starts_with("database server execution time")),
        "{:?}",
        finding.does_not_prove
    );
}

fn is_connection_reset(error: &sqlx::Error) -> bool {
    match error {
        // 57P01: admin_shutdown, sent when a backend is terminated.
        sqlx::Error::Database(database) => database.code().as_deref() == Some("57P01"),
        sqlx::Error::Io(_) | sqlx::Error::Protocol(_) => true,
        _ => false,
    }
}

async fn backend_pid(connection: &mut sqlx::PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(connection)
        .await
        .unwrap()
}

/// Terminates a backend from a separate, unpooled connection.
async fn terminate_backend(admin: &mut sqlx::PgConnection, pid: i32) -> bool {
    sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(admin)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires FERRUM_ALLOY_TEST_DATABASE_URL"]
async fn a_terminated_backend_fails_the_call_and_the_pool_recovers() {
    let capture = Capture::default();
    let _guard = capture.install();
    let mut cfg = settings(&database_url());
    cfg.max_connections = 1;
    cfg.acquire_timeout_ms = 2_000;
    let pool = postgres::connect(&cfg, "ferrum-alloy-reset").unwrap();
    let mut admin = sqlx::PgConnection::connect(&database_url()).await.unwrap();

    let mut connection = postgres::acquire(&pool).await.unwrap();
    let victim = backend_pid(&mut connection).await;
    let started = Instant::now();
    let sleep = sqlx::query("SELECT pg_sleep(30)");
    let call = postgres::query("orders.reset", "SELECT", "SELECT pg_sleep")
        .run_result(sleep.execute(&mut *connection));
    let terminate = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        terminate_backend(&mut admin, victim).await
    };
    let (result, terminated) = tokio::join!(call, terminate);
    assert!(terminated, "the backend was terminated");
    let error = result.unwrap_err();
    assert!(is_connection_reset(&error), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call failed at the reset, not at the end of pg_sleep"
    );

    let span = capture.operation("orders.reset");
    assert_eq!(span.str("alloy.operation.outcome"), Some("error"));
    assert_eq!(span.str("error.type"), Some("operation_error"));
    let duration = span.f64("alloy.operation.duration_ms");
    assert!(
        (250.0..5_000.0).contains(&duration),
        "duration {duration} ms ends at the reset"
    );

    // Returning the broken connection must discard it and release its
    // permit. With max_connections = 1 a leaked permit makes every later
    // acquire time out.
    drop(connection);
    for _ in 0..3 {
        let mut connection = postgres::acquire(&pool).await.unwrap();
        assert_ne!(
            backend_pid(&mut connection).await,
            victim,
            "a fresh backend"
        );
    }
    postgres::readiness(pool.clone()).check().await.unwrap();
    assert!(pool.size() <= 1);
    admin.close().await.unwrap();
}
