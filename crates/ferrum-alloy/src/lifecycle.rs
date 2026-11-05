//! Shutdown coordination shared by listeners, health, and applications.

use std::sync::Arc;
use std::time::Instant;

use ferrum_alloy_telemetry::Metrics;
use tokio_util::sync::CancellationToken;

/// A handle to the service lifecycle. Cheap to clone.
///
/// Applications that own long-lived work (WebSocket sessions, background
/// tasks) should watch [`Lifecycle::shutdown_token`]. HTTP draining waits for
/// requests, response streams, and upgraded connections, and at the drain
/// budget fails every read and write on upgraded connections still open, but
/// only the application can end a session gracefully or stop detached tasks.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    started: Instant,
    /// Cancelled when shutdown begins: the one record of whether it has,
    /// whoever cancels it.
    shutdown: CancellationToken,
    stop_accepting: CancellationToken,
    metrics: Arc<Metrics>,
}

impl Lifecycle {
    /// A new lifecycle recording into `metrics`.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            inner: Arc::new(Inner {
                started: Instant::now(),
                shutdown: CancellationToken::new(),
                stop_accepting: CancellationToken::new(),
                metrics,
            }),
        }
    }

    /// `true` once shutdown has begun; readiness then reports `draining`.
    pub fn is_draining(&self) -> bool {
        self.inner.shutdown.is_cancelled()
    }

    /// Cancelled when shutdown begins. Cancelling it begins shutdown, as
    /// [`Lifecycle::trigger_shutdown`] does.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.inner.shutdown.clone()
    }

    /// Begins shutdown programmatically (as if a signal arrived).
    pub fn trigger_shutdown(&self) {
        self.inner.shutdown.cancel();
    }

    /// The request metrics registry.
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.inner.metrics)
    }

    /// Time since the lifecycle was created.
    pub fn uptime(&self) -> std::time::Duration {
        self.inner.started.elapsed()
    }

    pub(crate) fn stop_accepting(&self) -> &CancellationToken {
        &self.inner.stop_accepting
    }
}

/// Resolves on SIGTERM or SIGINT (Unix) or Ctrl-C (all platforms).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // No signal handler available: never resolve rather than exit.
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
