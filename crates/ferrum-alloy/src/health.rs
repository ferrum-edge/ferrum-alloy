//! Liveness, cached readiness, and detailed health.
//!
//! * Liveness answers `{"status":"ok"}` and nothing else.
//! * Readiness answers `ready`, `not_ready`, or `draining` with no check
//!   names or errors. Results are cached for `cache_ttl_ms` and refreshed by a
//!   single caller at a time, so a flood of health requests cannot trigger a
//!   flood of dependency probes.
//! * Detailed health (check names, latencies, errors) is served only on the
//!   protected management listener.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::header::{CACHE_CONTROL, HeaderValue};
use serde::Serialize;

use crate::lifecycle::Lifecycle;

/// A readiness check failure. The message is shown only on the protected
/// management listener; keep it free of secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckError(pub String);

impl CheckError {
    /// A failure with a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl<E: std::error::Error> From<E> for CheckError {
    fn from(error: E) -> Self {
        Self(error.to_string())
    }
}

/// Boxed check future.
pub type CheckFuture = Pin<Box<dyn Future<Output = Result<(), CheckError>> + Send>>;

/// A readiness dependency check.
pub trait HealthCheck: Send + Sync + 'static {
    /// Runs the check once.
    fn check(&self) -> CheckFuture;
}

impl<F, Fut> HealthCheck for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), CheckError>> + Send + 'static,
{
    fn check(&self) -> CheckFuture {
        Box::pin(self())
    }
}

/// Result of one check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Check name.
    pub name: String,
    /// Whether it passed.
    pub ok: bool,
    /// Failure message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Check duration in milliseconds.
    pub latency_ms: f64,
}

/// A readiness evaluation.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// All checks passed.
    pub ready: bool,
    /// Individual results.
    pub checks: Vec<CheckResult>,
    /// Age of the evaluation in milliseconds when served.
    pub age_ms: u64,
    #[serde(skip)]
    at: Option<Instant>,
}

type Checks = Vec<(String, Arc<dyn HealthCheck>)>;

/// Readiness registry with caching and single-flight refresh.
pub struct Readiness {
    checks: Checks,
    ttl: Duration,
    timeout: Duration,
    cache: Mutex<Option<Snapshot>>,
    refresh: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Readiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Readiness")
            .field(
                "checks",
                &self.checks.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl Readiness {
    /// Creates a registry.
    pub fn new(checks: Checks, ttl: Duration, timeout: Duration) -> Self {
        Self {
            checks,
            ttl,
            timeout,
            cache: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    /// Number of registered checks.
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    /// `true` when no checks are registered.
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    fn cached(&self) -> Option<Snapshot> {
        self.cache.lock().ok().and_then(|c| c.clone())
    }

    fn fresh(&self) -> Option<Snapshot> {
        self.cached()
            .filter(|s| s.at.is_some_and(|at| at.elapsed() < self.ttl))
    }

    /// Returns the current readiness, running checks at most once per TTL.
    pub async fn snapshot(&self) -> Snapshot {
        if let Some(snapshot) = self.fresh() {
            return with_age(snapshot);
        }
        let _guard = match self.refresh.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                // Another caller is refreshing: serve the previous result if
                // there is one, otherwise wait for the refresh.
                if let Some(snapshot) = self.cached() {
                    return with_age(snapshot);
                }
                let guard = self.refresh.lock().await;
                if let Some(snapshot) = self.fresh() {
                    return with_age(snapshot);
                }
                guard
            }
        };
        let results = futures_util::future::join_all(self.checks.iter().map(|(name, check)| {
            let name = name.clone();
            let future = check.check();
            let timeout = self.timeout;
            async move {
                let started = Instant::now();
                let outcome = tokio::time::timeout(timeout, future).await;
                let latency_ms = started.elapsed().as_secs_f64() * 1_000.0;
                let error = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.0),
                    Err(_) => Some(format!("timed out after {} ms", timeout.as_millis())),
                };
                CheckResult {
                    name,
                    ok: error.is_none(),
                    error,
                    latency_ms,
                }
            }
        }))
        .await;
        let snapshot = Snapshot {
            ready: results.iter().all(|r| r.ok),
            checks: results,
            age_ms: 0,
            at: Some(Instant::now()),
        };
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(snapshot.clone());
        }
        snapshot
    }
}

fn with_age(mut snapshot: Snapshot) -> Snapshot {
    snapshot.age_ms = snapshot.at.map_or(0, |at| {
        u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
    });
    snapshot
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Minimal liveness response.
pub fn liveness() -> Response {
    no_store(Json(serde_json::json!({ "status": "ok" })).into_response())
}

/// Minimal readiness response: status only.
pub async fn readiness(readiness: &Readiness, lifecycle: &Lifecycle) -> Response {
    if lifecycle.is_draining() {
        return no_store(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "draining" })),
            )
                .into_response(),
        );
    }
    let snapshot = readiness.snapshot().await;
    let (status, label) = if snapshot.ready {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    };
    no_store((status, Json(serde_json::json!({ "status": label }))).into_response())
}

/// Detailed health for the protected management listener.
pub async fn detailed(
    readiness: &Readiness,
    lifecycle: &Lifecycle,
    service: &str,
    version: Option<&str>,
) -> Response {
    let snapshot = readiness.snapshot().await;
    let draining = lifecycle.is_draining();
    let status = match (draining, snapshot.ready) {
        (true, _) => "draining",
        (false, true) => "ready",
        (false, false) => "not_ready",
    };
    let code = if status == "ready" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    no_store(
        (
            code,
            Json(serde_json::json!({
                "status": status,
                "draining": draining,
                "service": { "name": service, "version": version },
                "uptime_seconds": lifecycle.uptime().as_secs(),
                "in_flight_requests": lifecycle.metrics().in_flight(),
                "readiness": snapshot,
            })),
        )
            .into_response(),
    )
}
