//! Explicitly instrumented application operations.
//!
//! Alloy never claims to know how long "the handler" or "the database" took
//! unless that boundary is instrumented. [`Operation`] marks such a boundary:
//! it creates a child span and records the application-observed duration on
//! a monotonic clock, including when the future is dropped before completion.
//!
//! ```no_run
//! # async fn load() -> Result<u32, std::io::Error> { Ok(1) }
//! use ferrum_alloy_telemetry::operation::{Operation, OperationKind};
//!
//! # async fn handler() -> Result<u32, std::io::Error> {
//! let order = Operation::new("orders.load")
//!     .kind(OperationKind::Db)
//!     .db("postgresql", "SELECT")
//!     .run_result(load())
//!     .await?;
//! # Ok(order) }
//! ```
//!
//! A database operation's duration is the time the application waited for
//! the call. It includes pool wait, network transfer, and driver work, and is
//! not database server execution time.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use pin_project_lite::pin_project;
use tracing::Span;
use tracing::field::Empty;

/// Kind of operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    /// In-process work.
    Internal,
    /// A database call (exported as an OpenTelemetry CLIENT span).
    Db,
    /// An outbound call to another service (CLIENT span).
    Client,
}

impl OperationKind {
    fn label(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Db => "db",
            Self::Client => "client",
        }
    }

    fn otel_kind(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Db | Self::Client => "client",
        }
    }
}

/// Builder for an instrumented operation.
#[derive(Debug, Clone)]
#[must_use]
pub struct Operation {
    name: &'static str,
    kind: OperationKind,
    db_system: Option<&'static str>,
    db_operation: Option<&'static str>,
    summary: Option<&'static str>,
}

impl Operation {
    /// A named operation. Use a low-cardinality, static name.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            kind: OperationKind::Internal,
            db_system: None,
            db_operation: None,
            summary: None,
        }
    }

    /// Sets the operation kind.
    pub fn kind(mut self, kind: OperationKind) -> Self {
        self.kind = kind;
        self
    }

    /// Database system (`db.system.name`) and operation (`db.operation.name`).
    pub fn db(mut self, system: &'static str, operation: &'static str) -> Self {
        self.kind = OperationKind::Db;
        self.db_system = Some(system);
        self.db_operation = Some(operation);
        self
    }

    /// A static, parameter-free summary (`db.query.summary`). Never pass
    /// SQL with values or user input.
    pub fn summary(mut self, summary: &'static str) -> Self {
        self.summary = Some(summary);
        self
    }

    /// Creates the span (child of the current span).
    pub fn span(&self) -> Span {
        let span = tracing::info_span!(
            target: "ferrum_alloy::operation",
            "alloy.operation",
            otel.name = self.name,
            otel.kind = self.kind.otel_kind(),
            otel.status_code = Empty,
            alloy.operation.name = self.name,
            alloy.operation.kind = self.kind.label(),
            alloy.operation.duration_ms = Empty,
            alloy.operation.outcome = Empty,
            alloy.db.pool_wait_ms = Empty,
            db.system.name = Empty,
            db.operation.name = Empty,
            db.query.summary = Empty,
            error.type = Empty,
        );
        if let Some(system) = self.db_system {
            span.record("db.system.name", system);
        }
        if let Some(operation) = self.db_operation {
            span.record("db.operation.name", operation);
        }
        if let Some(summary) = self.summary {
            span.record("db.query.summary", summary);
        }
        span
    }

    /// Runs `future` inside the operation span.
    pub fn run<F: Future>(self, future: F) -> OperationFuture<F> {
        fn never<O>(_: &O) -> bool {
            false
        }
        OperationFuture {
            inner: future,
            span: self.span(),
            start: None,
            done: false,
            is_error: never::<F::Output>,
        }
    }

    /// Like [`Operation::run`], and marks the span as an error when the
    /// future resolves to `Err`.
    pub fn run_result<F, T, E>(self, future: F) -> OperationFuture<F>
    where
        F: Future<Output = Result<T, E>>,
    {
        fn is_err<T, E>(result: &Result<T, E>) -> bool {
            result.is_err()
        }
        OperationFuture {
            inner: future,
            span: self.span(),
            start: None,
            done: false,
            is_error: is_err::<T, E>,
        }
    }
}

/// Records the time spent waiting for a pooled connection on the current
/// operation span (no-op outside an operation span).
pub fn record_pool_wait(duration: std::time::Duration) {
    Span::current().record("alloy.db.pool_wait_ms", duration.as_secs_f64() * 1_000.0);
}

pin_project! {
    /// Future returned by [`Operation::run`].
    pub struct OperationFuture<F>
    where
        F: Future,
    {
        #[pin]
        inner: F,
        span: Span,
        start: Option<Instant>,
        done: bool,
        is_error: fn(&F::Output) -> bool,
    }

    impl<F> PinnedDrop for OperationFuture<F>
    where
        F: Future,
    {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if !*this.done && let Some(start) = this.start {
                let _entered = this.span.enter();
                this.span.record("alloy.operation.duration_ms", start.elapsed().as_secs_f64() * 1_000.0);
                this.span.record("alloy.operation.outcome", "cancelled");
            }
        }
    }
}

impl<F: Future> Future for OperationFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let start = *this.start.get_or_insert_with(Instant::now);
        let _entered = this.span.enter();
        let output = std::task::ready!(this.inner.poll(cx));
        *this.done = true;
        this.span.record(
            "alloy.operation.duration_ms",
            start.elapsed().as_secs_f64() * 1_000.0,
        );
        if (this.is_error)(&output) {
            this.span.record("alloy.operation.outcome", "error");
            this.span.record("otel.status_code", "error");
            this.span.record("error.type", "operation_error");
        } else {
            this.span.record("alloy.operation.outcome", "completed");
        }
        Poll::Ready(output)
    }
}
