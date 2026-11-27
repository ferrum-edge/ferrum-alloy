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
//!
//! When the telemetry layer has an evidence sink, each operation is also
//! kept in its request's [`RequestEvidence`](crate::RequestEvidence) (see
//! [`crate::evidence`] for which request an operation belongs to). Code
//! that times an operation itself, without [`Operation::run`], records it
//! with an [`OperationRecorder`].

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pin_project_lite::pin_project;
use tracing::Span;
use tracing::field::Empty;

use crate::evidence::{Collector, OperationEvidence, OperationOutcome, current_request};
use crate::trace_context::SpanId;

/// Kind of operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperationKind {
    /// In-process work.
    Internal,
    /// A database call (exported as an OpenTelemetry CLIENT span).
    Db,
    /// An outbound call to another service (CLIENT span).
    Client,
}

impl OperationKind {
    /// Stable label (`alloy.operation.kind`).
    pub fn label(self) -> &'static str {
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

    /// Runs `future` inside the operation span. The span is created at
    /// the future's first poll, as a child of the then-current span, so
    /// its interval starts when work starts — a future stored and only
    /// later awaited, or dropped unpolled, leaves no inflated or phantom
    /// span.
    pub fn run<F: Future>(self, future: F) -> OperationFuture<F> {
        fn never<O>(_: &O) -> bool {
            false
        }
        OperationFuture {
            inner: future,
            operation: self,
            span: None,
            recorder: None,
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
            operation: self,
            span: None,
            recorder: None,
            done: false,
            is_error: is_err::<T, E>,
        }
    }
}

/// Records the time spent waiting for a pooled connection on the current
/// operation span (no-op outside an operation span). When one operation
/// acquires more than once, each wait is recorded; export may carry the
/// repeated values, and Alloy's import and diagnosis use the **last**
/// value present — waits are never summed (nested or sequential waits
/// are not added together, like every other Alloy duration). Under an
/// exporter's span attribute limit, later values can be dropped, so with
/// very many acquires the last value present may not be the last wait.
pub fn record_pool_wait(duration: std::time::Duration) {
    Span::current().record("alloy.db.pool_wait_ms", duration.as_secs_f64() * 1_000.0);
    if let Ok(Some(slot)) = ACTIVE_OPERATION.try_with(|active| active.borrow().clone()) {
        let mut last = slot
            .pool_wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *last = Some((duration, Instant::now()));
    }
}

/// What a running operation gathers for its request's evidence.
#[derive(Debug)]
pub(crate) struct OperationSlot {
    span_id: SpanId,
    /// The last recorded pool wait and when it was recorded.
    pool_wait: Mutex<Option<(Duration, Instant)>>,
}

thread_local! {
    /// The operation whose future this thread is polling.
    static ACTIVE_OPERATION: RefCell<Option<Arc<OperationSlot>>> = const { RefCell::new(None) };
}

/// What [`replace_active_operation`] returns, to be put back later.
pub(crate) type ActiveOperation = Option<Arc<OperationSlot>>;

/// Makes `operation` the current operation on this thread and returns the
/// previous one. A request scope clears it, so a nested request's
/// operations and pool waits never attach to the outer request's operation.
pub(crate) fn replace_active_operation(operation: ActiveOperation) -> ActiveOperation {
    ACTIVE_OPERATION
        .try_with(|active| active.replace(operation))
        .ok()
        .flatten()
}

/// Makes an operation current on this thread until dropped, so pool waits
/// and nested operations attach to it. Private, and only ever held for a
/// synchronous stretch ([`OperationRecorder::in_scope`], one poll of an
/// [`OperationFuture`]), so guards drop in reverse order of entry on the
/// thread that entered them and each restores exactly what it replaced.
#[derive(Debug)]
#[must_use = "the operation is current only while the guard is alive"]
pub(crate) struct OperationGuard {
    previous: Option<Option<Arc<OperationSlot>>>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = ACTIVE_OPERATION.try_with(|active| active.replace(previous));
        }
    }
}

/// Times one operation for its request's evidence, for code that measures
/// an operation without [`Operation::run`] (Alloy's outbound HTTP client
/// does). [`Operation::run`] uses it internally.
///
/// Start it when the operation starts, on the request's task, and call
/// [`OperationRecorder::finish`] when it ends. A recorder dropped unfinished
/// records the operation as [`OperationOutcome::Cancelled`]. Without an
/// evidence sink on the current request it only measures the duration.
#[derive(Debug)]
pub struct OperationRecorder {
    name: &'static str,
    kind: OperationKind,
    start: Instant,
    /// Present while the operation belongs to a request that collects
    /// evidence.
    evidence: Option<RecorderEvidence>,
    finished: bool,
}

#[derive(Debug)]
struct RecorderEvidence {
    request: Arc<Collector>,
    slot: Arc<OperationSlot>,
    parent_span_id: SpanId,
}

impl OperationRecorder {
    /// Starts timing operation `name` of `kind`, whose span is `span`. Its
    /// parent is the operation current on this thread, or else the
    /// request's server span, as for a span created as a child of the
    /// current span.
    pub fn start(name: &'static str, kind: OperationKind, span: &Span) -> Self {
        Self::start_inner(name, kind, span, None)
    }

    /// Like [`OperationRecorder::start`], for a span created with the
    /// server span of a request as its explicit parent
    /// (`parent: &context.span`), where `server_span_id` is
    /// `RequestContext::span_id`. The operation is kept, with that parent,
    /// only when the request whose future or body this thread is polling is
    /// that same request. Otherwise (a context carried into another
    /// instrumented request) it is not kept in any evidence, since it
    /// belongs to the other request's trace; it is still timed.
    pub fn start_in_request(
        name: &'static str,
        kind: OperationKind,
        span: &Span,
        server_span_id: SpanId,
    ) -> Self {
        Self::start_inner(name, kind, span, Some(server_span_id))
    }

    fn start_inner(
        name: &'static str,
        kind: OperationKind,
        span: &Span,
        server_span_id: Option<SpanId>,
    ) -> Self {
        let start = Instant::now();
        let evidence = current_request()
            .filter(|request| server_span_id.is_none_or(|id| id == request.span_id()))
            .map(|request| {
                let span_id =
                    crate::otel_bridge::exported_ids(span).map_or_else(SpanId::random, |ids| ids.1);
                let parent_span_id = server_span_id.unwrap_or_else(|| {
                    ACTIVE_OPERATION
                        .try_with(|active| active.borrow().as_ref().map(|slot| slot.span_id))
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| request.span_id())
                });
                RecorderEvidence {
                    request,
                    slot: Arc::new(OperationSlot {
                        span_id,
                        pool_wait: Mutex::new(None),
                    }),
                    parent_span_id,
                }
            });
        Self {
            name,
            kind,
            start,
            evidence,
            finished: false,
        }
    }

    /// Runs `f` with this operation current on this thread, so
    /// [`record_pool_wait`] calls and operations started inside `f` attach
    /// to it. The previous operation is current again when `f` returns or
    /// unwinds. `f` is synchronous: for asynchronous work use
    /// [`Operation::run`], which makes its operation current on every poll.
    pub fn in_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        let _current = self.enter();
        f()
    }

    /// Makes this operation current on this thread until the guard drops.
    pub(crate) fn enter(&self) -> OperationGuard {
        let previous = self.evidence.as_ref().and_then(|evidence| {
            ACTIVE_OPERATION
                .try_with(|active| active.replace(Some(Arc::clone(&evidence.slot))))
                .ok()
        });
        OperationGuard { previous }
    }

    /// Time since the operation started.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// The span id this operation is recorded under in its request's
    /// evidence, when the request collects evidence. Use it as the parent
    /// id of an outbound `traceparent`, so a downstream service's spans link
    /// to this operation.
    pub fn span_id(&self) -> Option<SpanId> {
        self.evidence.as_ref().map(|evidence| evidence.slot.span_id)
    }

    /// Ends the operation with `outcome` and returns its duration.
    pub fn finish(mut self, outcome: OperationOutcome) -> Duration {
        self.record(outcome)
    }

    fn record(&mut self, outcome: OperationOutcome) -> Duration {
        let duration = self.start.elapsed();
        if self.finished {
            return duration;
        }
        self.finished = true;
        if let Some(evidence) = self.evidence.take() {
            let last_wait = *evidence
                .slot
                .pool_wait
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut operation = OperationEvidence::new(
                self.name,
                self.kind,
                outcome,
                evidence.slot.span_id,
                evidence.parent_span_id,
            );
            operation.started = self
                .start
                .saturating_duration_since(evidence.request.start());
            operation.duration = duration;
            if let Some((wait, at)) = last_wait {
                operation.pool_wait = Some(wait);
                operation.pool_wait_ended =
                    Some(at.saturating_duration_since(evidence.request.start()));
            }
            evidence.request.push(operation);
        }
        duration
    }
}

impl Drop for OperationRecorder {
    fn drop(&mut self) {
        if !self.finished {
            self.record(OperationOutcome::Cancelled);
        }
    }
}

pin_project! {
    /// Future returned by [`Operation::run`].
    pub struct OperationFuture<F>
    where
        F: Future,
    {
        #[pin]
        inner: F,
        operation: Operation,
        span: Option<Span>,
        recorder: Option<OperationRecorder>,
        done: bool,
        is_error: fn(&F::Output) -> bool,
    }

    impl<F> PinnedDrop for OperationFuture<F>
    where
        F: Future,
    {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if !*this.done
                && let Some(span) = this.span
                && let Some(recorder) = this.recorder.take()
            {
                let _entered = span.enter();
                let duration = recorder.finish(OperationOutcome::Cancelled);
                span.record("alloy.operation.duration_ms", duration.as_secs_f64() * 1_000.0);
                span.record("alloy.operation.outcome", OperationOutcome::Cancelled.as_str());
            }
        }
    }
}

impl<F: Future> Future for OperationFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        // Create the span at first poll, when work actually starts, so
        // the span interval and the monotonic duration share a start. A
        // future dropped before this exports no span at all.
        let span = match this.span.as_ref() {
            Some(span) => span,
            None => {
                let span = this.operation.span();
                this.span.insert(span)
            }
        };
        let _entered = span.enter();
        let recorder = this.recorder.get_or_insert_with(|| {
            OperationRecorder::start(this.operation.name, this.operation.kind, span)
        });
        let output = {
            let _current = recorder.enter();
            std::task::ready!(this.inner.poll(cx))
        };
        *this.done = true;
        let failed = (this.is_error)(&output);
        let outcome = if failed {
            OperationOutcome::Error
        } else {
            OperationOutcome::Completed
        };
        let duration = match this.recorder.take() {
            Some(recorder) => recorder.finish(outcome),
            None => Duration::ZERO,
        };
        span.record(
            "alloy.operation.duration_ms",
            duration.as_secs_f64() * 1_000.0,
        );
        span.record("alloy.operation.outcome", outcome.as_str());
        if failed {
            span.record("otel.status_code", "error");
            span.record("error.type", "operation_error");
        }
        Poll::Ready(output)
    }
}
