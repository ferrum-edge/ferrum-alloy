//! Hand-off of finished-request evidence to an application-owned store.
//!
//! With [`TelemetryLayer::with_evidence_sink`](crate::TelemetryLayer::with_evidence_sink),
//! the layer passes one [`RequestEvidence`] per instrumented request to an
//! [`EvidenceSink`] when the request finalizes: at body completion, error,
//! or drop, exactly once. The evidence holds only values the layer already
//! measures and labels. It never holds header values, the raw path or query,
//! bodies, peer addresses, or certificate identities.
//!
//! A request is attributed to a tenant only when the application says so
//! through the request's [`TenantTag`]. The layer inserts the tag into
//! request extensions only while a sink is installed.
//!
//! While a sink is installed, the evidence also carries the request's
//! explicitly instrumented operations ([`crate::operation`]) and its
//! admission wait ([`record_admission_wait`]). An operation belongs to the
//! request whose instrumented future or response body was being polled when
//! the operation first ran; work moved to another task (for example with
//! `tokio::spawn`) is not attributed to the request. At most
//! [`MAX_OPERATIONS_PER_REQUEST`] operations are kept per request; later
//! ones are only counted ([`RequestEvidence::operations_dropped`]).
//!
//! ```
//! use ferrum_alloy_telemetry::evidence::TenantTag;
//!
//! // In a handler or middleware that has authenticated the caller:
//! let tag = TenantTag::default();
//! assert!(tag.set("acme"));
//! assert!(!tag.set("other"), "the first tag wins");
//! assert_eq!(tag.get(), Some("acme"));
//! ```

use std::cell::RefCell;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::context::TraceDecision;
use crate::layer::BodyOutcome;
use crate::operation::OperationKind;
use crate::peer::PeerTrust;
use crate::request_id::{RequestId, RequestIdSource};
use crate::trace_context::{SpanId, TraceId};

/// Maximum length of a tenant tag in bytes.
pub const MAX_TENANT_BYTES: usize = 128;

/// Most operations kept in one request's evidence. Later operations of the
/// request are counted in [`RequestEvidence::operations_dropped`], never
/// stored.
pub const MAX_OPERATIONS_PER_REQUEST: usize = 32;

/// Returns `true` when `tenant` is a valid tenant tag: 1 to
/// [`MAX_TENANT_BYTES`] bytes of `[A-Za-z0-9._:@-]`.
pub fn valid_tenant(tenant: &str) -> bool {
    !tenant.is_empty()
        && tenant.len() <= MAX_TENANT_BYTES
        && tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

/// The tenant a request belongs to, set by application code once it has
/// authenticated the caller.
///
/// Clones share one slot. The first valid [`TenantTag::set`] wins, so code
/// that runs later cannot move a request to another tenant.
#[derive(Clone, Default)]
pub struct TenantTag(Arc<OnceLock<Arc<str>>>);

impl TenantTag {
    /// Attributes the request to `tenant`. Returns `false`, and changes
    /// nothing, when `tenant` is not a valid tag (see [`valid_tenant`]) or
    /// the request already has a tenant.
    pub fn set(&self, tenant: &str) -> bool {
        valid_tenant(tenant) && self.0.set(Arc::from(tenant)).is_ok()
    }

    /// The tenant, once set.
    pub fn get(&self) -> Option<&str> {
        self.0.get().map(|tenant| tenant.as_ref())
    }

    pub(crate) fn shared(&self) -> Option<Arc<str>> {
        self.0.get().cloned()
    }
}

impl fmt::Debug for TenantTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TenantTag").field(&self.get()).finish()
    }
}

/// Reads the request's tag. Without an evidence sink the layer inserts none,
/// and the extractor returns a detached tag whose value is never read.
#[cfg(feature = "axum")]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for TenantTag {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let tag = parts.extensions.get::<TenantTag>().cloned();
        Ok(tag.unwrap_or_default())
    }
}

/// How a correlation id entered this process, in alias lookup preference
/// order. This does not authenticate who originally chose an external value.
///
/// This is correlation provenance, not ownership of retained evidence.
/// A trusted peer can forward values its own caller chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum RequestIdOrigin {
    /// This process generated it, so no other request shares it. Includes
    /// ids that replaced an invalid or untrusted incoming one.
    Generated,
    /// A peer the trust classifier trusts supplied it, such as Ferrum Edge.
    /// The peer may have kept a value its own client chose.
    TrustedPeer,
    /// A caller the trust classifier does not trust supplied it, which only
    /// `request_id.accept_incoming = any` allows.
    UntrustedCaller,
}

impl RequestIdOrigin {
    /// The origin of an id chosen as `source` for a request from a peer
    /// classified as `trust`.
    pub fn new(source: RequestIdSource, trust: &PeerTrust) -> Self {
        match source {
            RequestIdSource::Accepted if trust.is_trusted() => Self::TrustedPeer,
            RequestIdSource::Accepted => Self::UntrustedCaller,
            RequestIdSource::Generated
            | RequestIdSource::ReplacedInvalid
            | RequestIdSource::ReplacedUntrusted => Self::Generated,
        }
    }

    /// Stable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::TrustedPeer => "trusted_peer",
            Self::UntrustedCaller => "untrusted_caller",
        }
    }
}

/// What the telemetry layer measured about one finalized request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RequestEvidence {
    pub(crate) diagnostic_id: RequestId,
    /// The request id the layer chose.
    pub request_id: RequestId,
    /// How the correlation id entered this process.
    pub request_id_origin: RequestIdOrigin,
    /// Trace id of the request's trace.
    pub trace_id: TraceId,
    /// Span id of Alloy's server span.
    pub span_id: SpanId,
    /// The tenant the application tagged the request with, if any.
    pub tenant: Option<Arc<str>>,
    /// The matched route template, when the router matched one.
    pub route: Option<Arc<str>>,
    /// The response status, when response headers were produced.
    pub status: Option<u16>,
    /// Middleware entry until response headers were produced
    /// (`alloy.server.time_to_headers`), when they were.
    pub time_to_headers: Option<Duration>,
    /// Response headers produced until the body ended, failed, or was
    /// dropped (`alloy.server.body_duration`), when headers were produced.
    pub body_duration: Option<Duration>,
    /// Middleware entry until finalization (`alloy.server.duration`).
    pub duration: Duration,
    /// How the response ended.
    pub outcome: BodyOutcome,
    /// How the trace was rooted.
    pub trace_decision: TraceDecision,
    /// Transport trust label of the direct peer (`PeerTrust::label`).
    pub peer_trust: &'static str,
    /// Wall-clock time of middleware entry, the instant every duration and
    /// operation offset here is measured from. It only places this
    /// request's measurements on a common timeline; durations come from the
    /// monotonic clock.
    pub started_at: SystemTime,
    /// Time spent waiting for an admission permit
    /// (`alloy.admission.wait`), when admission control ran.
    pub admission_wait: Option<Duration>,
    /// The request's explicitly instrumented operations, in the order they
    /// finished, at most [`MAX_OPERATIONS_PER_REQUEST`].
    pub operations: Vec<OperationEvidence>,
    /// Operations of the request beyond [`MAX_OPERATIONS_PER_REQUEST`],
    /// counted but not kept.
    pub operations_dropped: u32,
}

/// How an instrumented operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperationOutcome {
    /// The operation's future completed (with a value that is not an error).
    Completed,
    /// The operation's future resolved to an error.
    Error,
    /// The operation was dropped before it completed.
    Cancelled,
}

impl OperationOutcome {
    /// Stable label (`alloy.operation.outcome`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One explicitly instrumented operation of a request
/// (`alloy.operation.duration`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OperationEvidence {
    /// The operation's static name (`alloy.operation.name`).
    pub name: &'static str,
    /// The operation kind (`alloy.operation.kind`).
    pub kind: OperationKind,
    /// How it ended.
    pub outcome: OperationOutcome,
    /// The operation span's id: its exported OpenTelemetry span id when
    /// export is active, otherwise an id generated for this evidence.
    pub span_id: SpanId,
    /// The enclosing operation's span id, or the request's server span id.
    pub parent_span_id: SpanId,
    /// First poll of the operation, measured from middleware entry
    /// ([`RequestEvidence::started_at`]).
    pub started: Duration,
    /// First poll until completion or drop.
    pub duration: Duration,
    /// The last pool wait recorded inside the operation
    /// (`alloy.db.pool_wait`, see [`crate::operation::record_pool_wait`]).
    pub pool_wait: Option<Duration>,
}

impl OperationEvidence {
    /// An operation that started at middleware entry and took no time, with
    /// no pool wait. Intended for testing a sink; set any other field.
    pub fn new(
        name: &'static str,
        kind: OperationKind,
        outcome: OperationOutcome,
        span_id: SpanId,
        parent_span_id: SpanId,
    ) -> Self {
        Self {
            name,
            kind,
            outcome,
            span_id,
            parent_span_id,
            started: Duration::ZERO,
            duration: Duration::ZERO,
            pool_wait: None,
        }
    }
}

impl RequestEvidence {
    /// Evidence of a request that finalized before it produced response
    /// headers: a generated request id, no tenant, route, or status, zero
    /// duration, outcome [`BodyOutcome::CancelledBeforeHeaders`], a root
    /// trace, an untrusted peer, a start at the Unix epoch, and no admission
    /// wait or operations.
    ///
    /// Intended for testing a sink. Fields may be added in any release, so
    /// set any other field on the returned value.
    pub fn new(request_id: RequestId, trace_id: TraceId, span_id: SpanId) -> Self {
        Self {
            diagnostic_id: RequestId::generate(),
            request_id,
            request_id_origin: RequestIdOrigin::Generated,
            trace_id,
            span_id,
            tenant: None,
            route: None,
            status: None,
            time_to_headers: None,
            body_duration: None,
            duration: Duration::ZERO,
            outcome: BodyOutcome::CancelledBeforeHeaders,
            trace_decision: TraceDecision::Root,
            peer_trust: PeerTrust::Untrusted.label(),
            started_at: SystemTime::UNIX_EPOCH,
            admission_wait: None,
            operations: Vec::new(),
            operations_dropped: 0,
        }
    }

    /// Immutable local ownership and lookup id. Cloned evidence represents
    /// another attempt within the same local request; constructing new
    /// evidence creates a fresh owner, even for the same remote id and trace.
    pub fn diagnostic_id(&self) -> &RequestId {
        &self.diagnostic_id
    }
}

/// Receives the evidence of every finalized request.
pub trait EvidenceSink: Send + Sync + fmt::Debug + 'static {
    /// Called once per request, on the task or thread that finalized it.
    /// It must not block.
    fn record(&self, evidence: RequestEvidence);
}

/// Records the time the current request waited for an admission permit.
///
/// Call it from middleware that runs inside the telemetry layer, on the
/// request's own task (Alloy's admission control does). Outside an
/// instrumented request with an evidence sink it does nothing. A later call
/// for the same request replaces the earlier value.
pub fn record_admission_wait(wait: Duration) {
    if let Some(collector) = current_request() {
        collector.lock().admission_wait = Some(wait);
    }
}

/// What a request's operations and admission control left for its
/// evidence, gathered while the request runs.
#[derive(Debug)]
pub(crate) struct Collector {
    /// Middleware entry, on the monotonic clock.
    start: Instant,
    /// The request's server span id: the parent of a top-level operation.
    span_id: SpanId,
    state: Mutex<Collected>,
}

#[derive(Debug, Default)]
pub(crate) struct Collected {
    pub(crate) admission_wait: Option<Duration>,
    pub(crate) operations: Vec<OperationEvidence>,
    pub(crate) operations_dropped: u32,
    /// Set at finalization; later operations are ignored.
    closed: bool,
}

impl Collector {
    pub(crate) fn new(start: Instant, span_id: SpanId) -> Self {
        Self {
            start,
            span_id,
            state: Mutex::new(Collected::default()),
        }
    }

    pub(crate) fn start(&self) -> Instant {
        self.start
    }

    pub(crate) fn span_id(&self) -> SpanId {
        self.span_id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Collected> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps `operation`, or counts it once the request already holds
    /// [`MAX_OPERATIONS_PER_REQUEST`]. Ignored after finalization.
    pub(crate) fn push(&self, operation: OperationEvidence) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        if state.operations.len() < MAX_OPERATIONS_PER_REQUEST {
            state.operations.push(operation);
        } else {
            state.operations_dropped = state.operations_dropped.saturating_add(1);
        }
    }

    /// Everything gathered, once; later records are ignored.
    pub(crate) fn close(&self) -> Collected {
        let mut state = self.lock();
        state.closed = true;
        Collected {
            admission_wait: state.admission_wait.take(),
            operations: std::mem::take(&mut state.operations),
            operations_dropped: std::mem::take(&mut state.operations_dropped),
            closed: true,
        }
    }
}

thread_local! {
    /// The request whose future or body this thread is polling.
    static ACTIVE_REQUEST: RefCell<Option<Arc<Collector>>> = const { RefCell::new(None) };
}

/// Makes one instrumented request current on this thread until dropped,
/// then restores whatever was current before (polls can nest: a handler
/// may drive another instrumented service on its own task).
///
/// Every request the layer instruments enters its own scope, also when it
/// collects nothing (`collector` is `None`): a nested request then never
/// writes into the request that is driving it, and its operations never
/// take that request's operation as their parent. The current operation is
/// cleared for the scope's duration for the same reason.
pub(crate) struct RequestScope {
    previous: Option<Arc<Collector>>,
    previous_operation: crate::operation::ActiveOperation,
}

impl RequestScope {
    pub(crate) fn enter(collector: Option<&Arc<Collector>>) -> Self {
        let previous = ACTIVE_REQUEST
            .try_with(|active| active.replace(collector.cloned()))
            .ok()
            .flatten();
        let previous_operation = crate::operation::replace_active_operation(None);
        Self {
            previous,
            previous_operation,
        }
    }
}

impl Drop for RequestScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = ACTIVE_REQUEST.try_with(|active| active.replace(previous));
        crate::operation::replace_active_operation(self.previous_operation.take());
    }
}

/// The request being polled on this thread, if it collects evidence.
pub(crate) fn current_request() -> Option<Arc<Collector>> {
    ACTIVE_REQUEST
        .try_with(|active| active.borrow().clone())
        .ok()
        .flatten()
}

/// Per-request state kept until finalization.
#[derive(Debug)]
pub(crate) struct Pending {
    pub(crate) sink: Arc<dyn EvidenceSink>,
    pub(crate) diagnostic_id: RequestId,
    pub(crate) request_id: RequestId,
    pub(crate) request_id_origin: RequestIdOrigin,
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: SpanId,
    pub(crate) tenant: TenantTag,
    pub(crate) trace_decision: TraceDecision,
    pub(crate) peer_trust: &'static str,
    pub(crate) started_at: SystemTime,
    pub(crate) collector: Arc<Collector>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_tags_are_bounded_and_set_once() {
        assert!(valid_tenant("acme"));
        assert!(valid_tenant("org:acme@eu-1.prod_2"));
        assert!(!valid_tenant(""));
        assert!(!valid_tenant("a b"));
        assert!(!valid_tenant("a/b"));
        assert!(valid_tenant(&"a".repeat(MAX_TENANT_BYTES)));
        assert!(!valid_tenant(&"a".repeat(MAX_TENANT_BYTES + 1)));

        let tag = TenantTag::default();
        let clone = tag.clone();
        assert!(!tag.set("not valid"));
        assert_eq!(tag.get(), None);
        assert!(clone.set("acme"));
        assert!(!tag.set("other"));
        assert_eq!(tag.get(), Some("acme"));
        assert_eq!(format!("{tag:?}"), "TenantTag(Some(\"acme\"))");
    }

    #[test]
    fn only_an_accepted_id_has_its_peer_as_its_origin() {
        let trusted = PeerTrust::NetworkBoundary([10, 0, 0, 5].into());
        let untrusted = PeerTrust::Untrusted;
        for trust in [&trusted, &untrusted] {
            for source in [
                RequestIdSource::Generated,
                RequestIdSource::ReplacedInvalid,
                RequestIdSource::ReplacedUntrusted,
            ] {
                let origin = RequestIdOrigin::new(source, trust);
                assert_eq!(origin, RequestIdOrigin::Generated, "{source:?}");
            }
        }
        let origin = RequestIdOrigin::new(RequestIdSource::Accepted, &trusted);
        assert_eq!(origin, RequestIdOrigin::TrustedPeer);
        assert_eq!(origin.as_str(), "trusted_peer");
        let origin = RequestIdOrigin::new(RequestIdSource::Accepted, &untrusted);
        assert_eq!(origin, RequestIdOrigin::UntrustedCaller);
        assert_eq!(origin.as_str(), "untrusted_caller");
        assert_eq!(RequestIdOrigin::Generated.as_str(), "generated");
    }
}
