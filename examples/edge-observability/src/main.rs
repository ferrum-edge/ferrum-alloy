//! An Alloy service meant to run behind Ferrum Edge.
//!
//! Configuration comes from `FERRUM_ALLOY_CONFIG` (see `alloy.toml`): TLS
//! with client-certificate verification, `gateway_required`, the gateway's
//! SPIFFE identity as the only trusted peer, and OTLP export.
//!
//! `/items/{id}` performs an explicitly instrumented dependency call. The
//! dependency is **simulated** with a fixed delay so the end-to-end test can
//! check trace structure and timing boundaries deterministically; it is not a
//! database.
//!
//! The remaining routes exist for the end-to-end driver's attempt and
//! connection cases. Faults are produced here, inside the compose network,
//! never by changing the host's network:
//!
//! * `/flaky/{key}` answers `503` to the first request for a key and `200`
//!   afterwards, so a gateway retry reaches the service twice.
//! * `/conn` reports the gateway connection (remote socket address) the
//!   request arrived on and how many earlier requests that connection
//!   carried. A middleware over the whole application router counts every
//!   request, the main proxy's traffic and health checks included.
//! * `/gather/{n}` holds each request until `n` of them are inside the handler
//!   at once (or 5 s pass), then reports the peak and the connection.
//! * `/events/long` streams for about 4 s, long enough to cancel mid-body.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use ferrum_alloy::AlloyApp;
use ferrum_alloy::edge::GatewayContext;
use ferrum_alloy::extract::{Json, Path};
use ferrum_alloy::telemetry::operation::{Operation, OperationKind};
use serde::Serialize;

/// Simulated dependency latency.
const DEPENDENCY_DELAY: Duration = Duration::from_millis(120);

#[derive(Serialize)]
struct Item {
    id: u32,
    in_stock: u32,
    consumer: Option<String>,
}

async fn get_item(Path(id): Path<u32>, gateway: Option<GatewayContext>) -> Json<Item> {
    let in_stock = Operation::new("inventory.lookup")
        .kind(OperationKind::Client)
        .run(async move {
            tokio::time::sleep(DEPENDENCY_DELAY).await;
            id % 7
        })
        .await;
    Json(Item {
        id,
        in_stock,
        consumer: gateway.and_then(|g| g.consumer_username),
    })
}

/// Streams `count` server-sent events, one every `interval`.
fn ticks(count: u32, interval: Duration) -> Response {
    let (mut sender, body) = http_body_util::channel::Channel::<Bytes, Infallible>::new(2);
    tokio::spawn(async move {
        for i in 0..count {
            tokio::time::sleep(interval).await;
            if sender
                .send_data(Bytes::from(format!("event: tick\ndata: {i}\n\n")))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(Body::new(body))
        .unwrap_or_default()
}

async fn events() -> Response {
    ticks(5, Duration::from_millis(60))
}

async fn long_events() -> Response {
    ticks(40, Duration::from_millis(100))
}

/// Bounds the probe maps; the driver needs only a handful of entries.
const MAX_PROBE_ENTRIES: usize = 4096;

/// State behind the end-to-end probe routes.
#[derive(Clone, Default)]
struct Probes {
    /// Keys whose first request already failed.
    failed_once: Arc<Mutex<HashSet<String>>>,
    /// Requests carried so far by each gateway connection, on every route.
    per_connection: Arc<Mutex<HashMap<SocketAddr, u64>>>,
    /// `/gather` requests currently inside the handler.
    gathering: Arc<AtomicUsize>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Probes {
    /// Counts a request on `remote`'s connection and returns its 1-based
    /// position on that connection.
    fn count(&self, remote: SocketAddr) -> u64 {
        let mut map = lock(&self.per_connection);
        if map.len() >= MAX_PROBE_ENTRIES && !map.contains_key(&remote) {
            map.clear();
        }
        let seen = map.entry(remote).or_insert(0);
        *seen += 1;
        *seen
    }
}

/// This request's 1-based position among every request its connection
/// carried.
#[derive(Clone, Copy)]
struct Position(u64);

/// Counts every request per gateway connection before any route sees it, so
/// requests to other paths (main-proxy traffic, `/readyz` health checks)
/// count too.
async fn count_requests(
    State(probes): State<Probes>,
    mut request: Request,
    next: Next,
) -> Response {
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    if let Some(remote) = remote {
        let position = Position(probes.count(remote));
        request.extensions_mut().insert(position);
    }
    next.run(request).await
}

async fn flaky(State(probes): State<Probes>, Path(key): Path<String>) -> Response {
    let first = {
        let mut failed = lock(&probes.failed_once);
        if failed.len() >= MAX_PROBE_ENTRIES {
            failed.clear();
        }
        failed.insert(key)
    };
    if first {
        (StatusCode::SERVICE_UNAVAILABLE, "first attempt fails").into_response()
    } else {
        "recovered".into_response()
    }
}

#[derive(Serialize)]
struct ConnectionView {
    /// The gateway's socket address as this service saw it.
    remote: String,
    /// Requests, to any route, that connection carried before this one.
    earlier_requests: u64,
}

async fn connection(
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Extension(Position(position)): Extension<Position>,
) -> Json<ConnectionView> {
    Json(ConnectionView {
        remote: remote.to_string(),
        earlier_requests: position.saturating_sub(1),
    })
}

#[derive(Serialize)]
struct GatherView {
    /// Most `/gather` requests this one saw inside the handler at once.
    peak_in_flight: usize,
    remote: String,
    earlier_requests: u64,
}

/// Decrements the in-flight count even when the request is cancelled.
struct Gathering(Arc<AtomicUsize>);

impl Drop for Gathering {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn gather(
    State(probes): State<Probes>,
    Path(expected): Path<usize>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Extension(Position(position)): Extension<Position>,
) -> Json<GatherView> {
    let expected = expected.clamp(1, 32);
    let mut peak = probes.gathering.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = Gathering(Arc::clone(&probes.gathering));
    let deadline = Instant::now() + Duration::from_secs(5);
    while peak < expected && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        peak = peak.max(probes.gathering.load(Ordering::SeqCst));
    }
    // Stay inside long enough for every other request to observe the peak.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Json(GatherView {
        peak_in_flight: peak,
        remote: remote.to_string(),
        earlier_requests: position.saturating_sub(1),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let probes = Probes::default();
    let router = Router::new()
        .route("/hello", get(|| async { "hello from behind Ferrum Edge" }))
        .route("/items/{id}", get(get_item))
        .route("/events", get(events))
        .route("/events/long", get(long_events))
        .route("/flaky/{key}", get(flaky))
        .route("/conn", get(connection))
        .route("/gather/{n}", get(gather))
        .with_state(probes.clone());
    let mut parts = AlloyApp::new("edge-demo-api")
        .version(env!("CARGO_PKG_VERSION"))
        .router(router)
        .into_parts()?;
    // Outermost, so health checks and requests Alloy rejects count too.
    let counted = middleware::from_fn_with_state(probes, count_requests);
    parts.router = parts.router.layer(counted);
    parts.serve().await?;
    Ok(())
}
