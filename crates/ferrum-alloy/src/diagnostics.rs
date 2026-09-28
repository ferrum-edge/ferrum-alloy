//! Authorized, tenant-scoped retrieval of one request's diagnostic evidence
//! (feature `diagnostics`; ADR 0008).
//!
//! Nothing here runs until the application installs a
//! [`DiagnosticsAuthorizer`] with
//! [`AlloyApp::diagnostics_authorizer`](crate::AlloyApp::diagnostics_authorizer).
//! Then:
//!
//! * the telemetry layer hands every finalized request to a bounded
//!   in-memory store, which keeps the requests the application tagged with a
//!   tenant through their [`TenantTag`];
//! * `GET /diagnostics/v1/requests/{request_id}` on the management listener
//!   asks the authorizer which tenant the caller may read, and answers with
//!   that tenant's evidence for the id as a `ferrum.diagnostic_report` v1
//!   document marked `Cache-Control: no-store`.
//!
//! A denied caller, a malformed, unknown, or evicted id, and another
//! tenant's id all get byte-identical `404` responses, and so does an
//! authorizer that panics or times out. A request id is a lookup key, never
//! a credential. The management token is neither required nor sufficient:
//! the authorizer decides.
//!
//! ```no_run
//! use axum::{Router, routing::get};
//! use ferrum_alloy::AlloyApp;
//! use ferrum_alloy::diagnostics::{DiagnosticsAccess, DiagnosticsRequest, TenantTag};
//!
//! # async fn doc() -> Result<(), Box<dyn std::error::Error>> {
//! let router = Router::new().route(
//!     "/orders",
//!     get(|tenant: TenantTag| async move {
//!         // After authenticating the caller, attribute the request.
//!         tenant.set("acme");
//!         "ok"
//!     }),
//! );
//! AlloyApp::new("orders")
//!     .router(router)
//!     .diagnostics_authorizer(|request: DiagnosticsRequest| async move {
//!         // Verify a tenant-scoped credential here; never trust a header
//!         // that merely names a tenant. Compare secrets in constant time.
//!         match request.bearer_token() {
//!             Some(token) if same(token.as_bytes(), b"a-verified-acme-credential") => {
//!                 DiagnosticsAccess::tenant("acme")
//!             }
//!             _ => DiagnosticsAccess::Deny,
//!         }
//!     })
//!     .run()
//!     .await?;
//! # Ok(()) }
//!
//! // Compares two secrets in time that depends only on their lengths.
//! fn same(a: &[u8], b: &[u8]) -> bool {
//!     a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
//! }
//! ```

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::extract::rejection::PathRejection;
use axum::extract::{ConnectInfo, Path, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ferrum_alloy_diagnostics::catalog;
use ferrum_alloy_diagnostics::model::{
    Availability, Boundaries, ClockDomain, Collection, CollectionMethod, DiagnosticReport, Leg,
    Observation, ObservationKind, Producer, ProducerKind, Scope, SpanRef, Trust, Unit,
    Verification,
};
use ferrum_alloy_telemetry::evidence::{EvidenceSink, RequestEvidence, valid_tenant};
use ferrum_alloy_telemetry::trace_context::{SpanId, TraceId};
use ferrum_alloy_telemetry::{PeerInfo, RequestId};
use futures_util::FutureExt as _;
use http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderValue};

pub use ferrum_alloy_telemetry::evidence::TenantTag;

use crate::config::{AlloyConfig, ConfigError, DiagnosticsSettings};
use crate::problem::{Problem, ProblemKind};

/// Route of the retrieval endpoint on the management listener.
pub const ROUTE: &str = "/diagnostics/v1/requests/{request_id}";

/// How long the authorizer may take. A slower answer denies.
pub const AUTHORIZER_TIMEOUT: Duration = Duration::from_secs(5);

/// Records kept for one tenant and request id, such as the attempts of a
/// retried request. A further one evicts the oldest of them.
pub const MAX_RECORDS_PER_REQUEST_ID: usize = 16;

/// Longest route template kept. A longer one is dropped from the record.
pub const MAX_ROUTE_BYTES: usize = 512;

/// Bytes of an `Arc<str>` allocation besides its text: its two reference
/// counts.
const ARC_HEADER_BYTES: usize = 2 * size_of::<usize>();

/// Estimated fixed cost of one record besides its strings: its queue slot;
/// an index entry and its hash table control byte, at the table's 7/8
/// maximum load; the smallest sequence number list; and the reference-count
/// headers of its tenant and route. It is an estimate, not a measurement:
/// allocator overhead and the spare capacity of the queue and the index,
/// which never shrink, are not counted, while an index entry that several
/// records share is counted for each of them.
const RECORD_OVERHEAD_BYTES: usize = size_of::<Option<Stored>>()
    + (size_of::<(Key, Vec<u64>)>() + 1) * 8 / 7
    + 4 * size_of::<u64>()
    + 2 * ARC_HEADER_BYTES;

/// Checks that retrieval can be served as ADR 0008 requires: on a loopback
/// management listener, behind its rate limit.
pub(crate) fn check_config(config: &AlloyConfig) -> Result<(), ConfigError> {
    let management = &config.management;
    let mut errors: Vec<String> = Vec::new();
    if !management.enabled {
        errors.push("diagnostic retrieval requires management.enabled".into());
    }
    // The management listener has no TLS, and the authorizer's credentials
    // must not cross a network in cleartext.
    if !management.bind.ip().is_loopback() {
        errors.push(format!(
            "diagnostic retrieval requires a loopback management.bind, not {}; terminate TLS in a proxy on the same host instead",
            management.bind
        ));
    }
    if !management.rate_limit.enabled {
        errors.push("diagnostic retrieval requires management.rate_limit.enabled".into());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(errors))
    }
}

/// What the authorizer sees of a retrieval request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DiagnosticsRequest {
    /// The transport peer, as the management listener recorded it.
    pub peer: Option<PeerInfo>,
    /// The request headers, including `Authorization`.
    pub headers: HeaderMap,
}

impl DiagnosticsRequest {
    /// A request from `peer` with `headers`, for testing an authorizer.
    pub fn new(peer: Option<PeerInfo>, headers: HeaderMap) -> Self {
        Self { peer, headers }
    }

    /// The credential of the `Authorization: Bearer` header, when the
    /// request has exactly one `Authorization` header and it is a non-empty
    /// bearer credential.
    pub fn bearer_token(&self) -> Option<&str> {
        let mut values = self.headers.get_all(AUTHORIZATION).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return None;
        };
        let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
        let token = token.trim();
        (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
    }
}

/// The authorizer's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticsAccess {
    /// The caller may read the evidence of requests tagged with this tenant,
    /// and of no other. A value that is not a valid tag
    /// ([`ferrum_alloy_telemetry::evidence::valid_tenant`]) denies.
    Tenant(String),
    /// The caller may read nothing. Answered exactly like an unknown id.
    Deny,
}

impl DiagnosticsAccess {
    /// Access to the evidence of `tenant`.
    pub fn tenant(tenant: impl Into<String>) -> Self {
        Self::Tenant(tenant.into())
    }
}

/// Boxed authorizer future.
pub type AuthorizeFuture = Pin<Box<dyn Future<Output = DiagnosticsAccess> + Send>>;

/// Decides which tenant's evidence a retrieval request may read.
///
/// Verify a tenant-scoped credential (for example a JWT whose claims name
/// the tenant) or a verified transport identity. A header that merely names
/// a tenant proves nothing, and neither does knowing a request id or a trace
/// id.
pub trait DiagnosticsAuthorizer: Send + Sync + 'static {
    /// Authorizes one retrieval request. It has [`AUTHORIZER_TIMEOUT`]. A
    /// panic, like a timeout, denies (unless panics abort the process).
    fn authorize(&self, request: DiagnosticsRequest) -> AuthorizeFuture;
}

impl<F, Fut> DiagnosticsAuthorizer for F
where
    F: Fn(DiagnosticsRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DiagnosticsAccess> + Send + 'static,
{
    fn authorize(&self, request: DiagnosticsRequest) -> AuthorizeFuture {
        Box::pin(self(request))
    }
}

/// One retained request.
#[derive(Debug, Clone)]
struct Stored {
    tenant: Arc<str>,
    request_id: RequestId,
    trace_id: TraceId,
    span_id: SpanId,
    route: Option<Arc<str>>,
    status: Option<u16>,
    time_to_headers: Option<Duration>,
    body_duration: Option<Duration>,
    duration: Duration,
    outcome: &'static str,
    trace_decision: &'static str,
    peer_trust: &'static str,
    /// Estimated retained bytes.
    bytes: usize,
}

type Key = (Arc<str>, RequestId);

/// Retained records, oldest first, with an index by tenant and request id.
#[derive(Debug, Default)]
struct Ring {
    /// `records[i]` has sequence number `first_seq + i`. `None` is the slot
    /// of a record evicted out of order; it is never first, and counts
    /// toward the count bound until it is.
    records: VecDeque<Option<Stored>>,
    first_seq: u64,
    /// Sequence numbers of each key's records, oldest first.
    index: HashMap<Key, Vec<u64>>,
    /// Records present.
    live: usize,
    bytes: usize,
}

impl Ring {
    /// Removes the empty slots at the front.
    fn trim(&mut self) {
        while matches!(self.records.front(), Some(None)) {
            self.records.pop_front();
            self.first_seq = self.first_seq.wrapping_add(1);
        }
    }

    /// Removes the oldest record. Returns `false` when there is none.
    fn evict_oldest(&mut self) -> bool {
        // An empty slot is never first, but skipping any keeps `first_seq`
        // in step with the queue if that ever stops holding.
        self.trim();
        let Some(Some(oldest)) = self.records.pop_front() else {
            return false;
        };
        let seq = self.first_seq;
        self.first_seq = self.first_seq.wrapping_add(1);
        self.live = self.live.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(oldest.bytes);
        let key = (oldest.tenant, oldest.request_id);
        if let Some(seqs) = self.index.get_mut(&key) {
            seqs.retain(|s| *s != seq);
            if seqs.is_empty() {
                self.index.remove(&key);
            }
        }
        self.trim();
        true
    }

    /// Removes the oldest record of `key` when it has the most a key may
    /// have. Returns whether it did.
    fn evict_oldest_of(&mut self, key: &Key) -> bool {
        let Some(seqs) = self.index.get_mut(key) else {
            return false;
        };
        if seqs.len() < MAX_RECORDS_PER_REQUEST_ID {
            return false;
        }
        let seq = seqs.remove(0);
        let offset = usize::try_from(seq.wrapping_sub(self.first_seq)).ok();
        let slot = offset.and_then(|offset| self.records.get_mut(offset));
        if let Some(oldest) = slot.and_then(Option::take) {
            self.live = self.live.saturating_sub(1);
            self.bytes = self.bytes.saturating_sub(oldest.bytes);
        }
        self.trim();
        true
    }

    fn find(&self, key: &Key) -> Vec<Stored> {
        let Some(seqs) = self.index.get(key) else {
            return Vec::new();
        };
        seqs.iter()
            .filter_map(|seq| {
                let offset = usize::try_from(seq.wrapping_sub(self.first_seq)).ok()?;
                self.records.get(offset)?.clone()
            })
            .collect()
    }
}

/// Why an eviction happened.
#[derive(Debug, Clone, Copy)]
enum Evicted {
    Count,
    Bytes,
    RequestIdLimit,
}

/// Why a finalized request was not retained.
#[derive(Debug, Clone, Copy)]
enum Skipped {
    Untagged,
    TooLarge,
}

/// How a retrieval ended. A denial is counted as not found, like the
/// response it gets.
#[derive(Debug, Clone, Copy)]
enum Retrieved {
    Served,
    NotFound,
}

/// Why the authorizer gave no answer. Each denies.
#[derive(Debug, Clone, Copy)]
enum AuthorizerFailure {
    Timeout,
    Panic,
}

/// What the authorizer did.
enum Answer {
    Access(DiagnosticsAccess),
    Failed(AuthorizerFailure),
}

/// The bounded evidence store: the telemetry layer's sink.
#[derive(Debug)]
pub(crate) struct EvidenceStore {
    max_records: usize,
    max_bytes: usize,
    ring: Mutex<Ring>,
    stored: AtomicU64,
    evicted: [AtomicU64; 3],
    skipped: [AtomicU64; 2],
    retrievals: [AtomicU64; 2],
    authorizer_failures: [AtomicU64; 2],
}

impl EvidenceStore {
    pub(crate) fn new(settings: &DiagnosticsSettings) -> Self {
        Self {
            max_records: settings.max_records.max(1),
            max_bytes: settings.max_bytes,
            ring: Mutex::new(Ring::default()),
            stored: AtomicU64::new(0),
            evicted: Default::default(),
            skipped: Default::default(),
            retrievals: Default::default(),
            authorizer_failures: Default::default(),
        }
    }

    fn ring(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn count_skipped(&self, reason: Skipped) {
        let counter = &self.skipped[reason as usize];
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn count_retrieval(&self, outcome: Retrieved) {
        let counter = &self.retrievals[outcome as usize];
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn count_authorizer_failure(&self, reason: AuthorizerFailure) {
        let counter = &self.authorizer_failures[reason as usize];
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn insert(&self, record: Stored) {
        if record.bytes > self.max_bytes {
            self.count_skipped(Skipped::TooLarge);
            return;
        }
        let key = (Arc::clone(&record.tenant), record.request_id.clone());
        let mut ring = self.ring();
        let mut evicted = [0u64; 3];
        // The newest records of a request id are kept, so records added
        // under it earlier cannot keep a later one out.
        if ring.evict_oldest_of(&key) {
            evicted[Evicted::RequestIdLimit as usize] += 1;
        }
        loop {
            let reason = if ring.records.len() >= self.max_records {
                Evicted::Count
            } else if ring.bytes.saturating_add(record.bytes) > self.max_bytes {
                Evicted::Bytes
            } else {
                break;
            };
            if !ring.evict_oldest() {
                break;
            }
            evicted[reason as usize] += 1;
        }
        let offset = u64::try_from(ring.records.len()).unwrap_or(u64::MAX);
        let seq = ring.first_seq.wrapping_add(offset);
        ring.live = ring.live.saturating_add(1);
        ring.bytes = ring.bytes.saturating_add(record.bytes);
        ring.index.entry(key).or_default().push(seq);
        ring.records.push_back(Some(record));
        drop(ring);
        self.stored.fetch_add(1, Ordering::Relaxed);
        for (counter, count) in self.evicted.iter().zip(evicted) {
            if count > 0 {
                counter.fetch_add(count, Ordering::Relaxed);
            }
        }
    }

    /// The records of `tenant` for `request_id`, oldest first.
    fn find(&self, tenant: &str, request_id: RequestId) -> Vec<Stored> {
        self.ring().find(&(Arc::from(tenant), request_id))
    }

    /// Records and estimated bytes currently retained.
    pub(crate) fn retained(&self) -> (usize, usize) {
        let ring = self.ring();
        (ring.live, ring.bytes)
    }

    /// Prometheus text for retention and retrievals.
    pub(crate) fn render_prometheus(&self) -> String {
        let (records, bytes) = self.retained();
        let mut out = String::new();
        for (name, kind, help, value) in [
            (
                "ferrum_alloy_diagnostics_records",
                "gauge",
                "Request evidence records retained for diagnostic retrieval.",
                records as u64,
            ),
            (
                "ferrum_alloy_diagnostics_bytes",
                "gauge",
                "Estimated bytes of retained request evidence.",
                bytes as u64,
            ),
            (
                "ferrum_alloy_diagnostics_stored_total",
                "counter",
                "Request evidence records retained.",
                self.stored.load(Ordering::Relaxed),
            ),
        ] {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
            ));
        }
        for (name, help, label, values) in [
            (
                "ferrum_alloy_diagnostics_evicted_total",
                "Retained records evicted to stay within a bound.",
                "reason",
                &[
                    ("count", &self.evicted[0]),
                    ("bytes", &self.evicted[1]),
                    ("request_id_limit", &self.evicted[2]),
                ][..],
            ),
            (
                "ferrum_alloy_diagnostics_skipped_total",
                "Finalized requests whose evidence was not retained.",
                "reason",
                &[
                    ("untagged", &self.skipped[0]),
                    ("too_large", &self.skipped[1]),
                ][..],
            ),
            (
                "ferrum_alloy_diagnostics_retrievals_total",
                "Diagnostic retrieval requests by outcome; denials count as not_found.",
                "outcome",
                &[
                    ("served", &self.retrievals[0]),
                    ("not_found", &self.retrievals[1]),
                ][..],
            ),
            (
                "ferrum_alloy_diagnostics_authorizer_failures_total",
                "Diagnostic retrievals denied because the authorizer timed out or panicked.",
                "reason",
                &[
                    ("timeout", &self.authorizer_failures[0]),
                    ("panic", &self.authorizer_failures[1]),
                ][..],
            ),
        ] {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
            for (value_label, counter) in values {
                out.push_str(&format!(
                    "{name}{{{label}=\"{value_label}\"}} {}\n",
                    counter.load(Ordering::Relaxed)
                ));
            }
        }
        out
    }
}

/// Estimated retained bytes of a record: the fixed overhead, its tenant,
/// its request id twice (record and index key), and its route template.
fn estimate(tenant: &str, request_id: &RequestId, route: Option<&str>) -> usize {
    RECORD_OVERHEAD_BYTES
        .saturating_add(tenant.len())
        .saturating_add(request_id.as_str().len().saturating_mul(2))
        .saturating_add(route.map_or(0, str::len))
}

impl EvidenceSink for EvidenceStore {
    fn record(&self, evidence: RequestEvidence) {
        let Some(tenant) = evidence.tenant else {
            self.count_skipped(Skipped::Untagged);
            return;
        };
        let route = evidence.route.filter(|r| r.len() <= MAX_ROUTE_BYTES);
        let bytes = estimate(&tenant, &evidence.request_id, route.as_deref());
        self.insert(Stored {
            tenant,
            request_id: evidence.request_id,
            trace_id: evidence.trace_id,
            span_id: evidence.span_id,
            route,
            status: evidence.status,
            time_to_headers: evidence.time_to_headers,
            body_duration: evidence.body_duration,
            duration: evidence.duration,
            outcome: evidence.outcome.as_str(),
            trace_decision: evidence.trace_decision.as_str(),
            peer_trust: evidence.peer_trust,
            bytes,
        });
    }
}

/// The retrieval endpoint's state.
#[derive(Clone)]
pub(crate) struct Retrieval {
    pub(crate) authorizer: Arc<dyn DiagnosticsAuthorizer>,
    pub(crate) store: Arc<EvidenceStore>,
    pub(crate) service: String,
}

/// The one answer for every request that gets no report.
fn not_found() -> Response {
    Problem::new(ProblemKind::RouteNotFound)
        .with_header(CACHE_CONTROL, HeaderValue::from_static("no-store"))
        .into_response()
}

fn no_store_json(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

/// The transport peer of a request. Headers are never consulted.
fn peer(request: &Request) -> Option<PeerInfo> {
    let extensions = request.extensions();
    if let Some(peer) = extensions.get::<PeerInfo>() {
        return Some(peer.clone());
    }
    let ConnectInfo(addr) = extensions.get::<ConnectInfo<SocketAddr>>()?;
    Some(PeerInfo {
        remote_addr: Some(*addr),
        ..PeerInfo::default()
    })
}

/// Asks `authorizer` about `request`, within [`AUTHORIZER_TIMEOUT`]. A panic,
/// in the call or in the future it returns, is a failure like a timeout, so
/// it neither drops the connection nor gets a response of its own.
async fn ask(authorizer: &dyn DiagnosticsAuthorizer, request: DiagnosticsRequest) -> Answer {
    let Ok(future) = catch_unwind(AssertUnwindSafe(|| authorizer.authorize(request))) else {
        return Answer::Failed(AuthorizerFailure::Panic);
    };
    let future = AssertUnwindSafe(future).catch_unwind();
    match tokio::time::timeout(AUTHORIZER_TIMEOUT, future).await {
        Ok(Ok(access)) => Answer::Access(access),
        Ok(Err(_)) => Answer::Failed(AuthorizerFailure::Panic),
        Err(_) => Answer::Failed(AuthorizerFailure::Timeout),
    }
}

impl Retrieval {
    /// Serves `GET /diagnostics/v1/requests/{request_id}`.
    pub(crate) async fn retrieve(
        &self,
        request_id: Result<Path<String>, PathRejection>,
        request: Request,
    ) -> Response {
        let peer = peer(&request);
        let (parts, _body) = request.into_parts();
        let asked = DiagnosticsRequest::new(peer, parts.headers);
        let tenant = match ask(self.authorizer.as_ref(), asked).await {
            Answer::Access(DiagnosticsAccess::Tenant(tenant)) if valid_tenant(&tenant) => tenant,
            Answer::Access(_) => {
                self.store.count_retrieval(Retrieved::NotFound);
                return not_found();
            }
            Answer::Failed(failure) => {
                let what = match failure {
                    AuthorizerFailure::Timeout => "timed out",
                    AuthorizerFailure::Panic => "panicked",
                };
                // Never the request, its credential, or the panic message.
                tracing::warn!(
                    target: "ferrum_alloy::diagnostics",
                    timeout_ms = AUTHORIZER_TIMEOUT.as_millis() as u64,
                    "diagnostics authorizer {what}; the request was denied"
                );
                self.store.count_authorizer_failure(failure);
                self.store.count_retrieval(Retrieved::NotFound);
                return not_found();
            }
        };
        let records = request_id
            .ok()
            .and_then(|Path(id)| RequestId::parse(&id))
            .map(|id| self.store.find(&tenant, id))
            .unwrap_or_default();
        if records.is_empty() {
            self.store.count_retrieval(Retrieved::NotFound);
            return not_found();
        }
        match serde_json::to_vec(&report(&self.service, &records)) {
            Ok(body) => {
                self.store.count_retrieval(Retrieved::Served);
                no_store_json(body)
            }
            Err(_) => Problem::new(ProblemKind::Internal)
                .with_header(CACHE_CONTROL, HeaderValue::from_static("no-store"))
                .into_response(),
        }
    }
}

/// Builds the report for the records of one tenant and request id.
fn report(service: &str, records: &[Stored]) -> DiagnosticReport {
    let mut report = DiagnosticReport::new(Collection {
        collector: Producer {
            kind: ProducerKind::Alloy,
            name: "ferrum-alloy".to_owned(),
            version: None,
            instance: None,
        },
        method: CollectionMethod::LiveExport,
        verification: Verification::Unverified,
        notes: vec![
            "assembled by the service from its own request telemetry; a reader cannot authenticate the producer".to_owned(),
            "server-level timing only; instrumented operations are not retained".to_owned(),
        ],
    });
    let first = records.first();
    report.subject.request_id = first.map(|record| record.request_id.as_str().to_owned());
    report.subject.service = Some(service.to_owned());
    if let Some(first) = first {
        let trace_id = first.trace_id;
        // Attempts that share a request id may belong to different traces.
        if records.iter().all(|record| record.trace_id == trace_id) {
            report.subject.trace_id = Some(trace_id.to_hex());
        }
    }
    report.subject.route = records
        .iter()
        .find_map(|record| record.route.as_deref().map(str::to_owned));
    for record in records {
        observations(service, record, &mut report.observations);
    }
    report
}

/// An observation of `record` with everything but its value.
fn base(
    service: &str,
    record: &Stored,
    suffix: &str,
    name: &str,
    kind: ObservationKind,
) -> Observation {
    let span_id = record.span_id.to_hex();
    Observation {
        id: format!("alloy:{span_id}:{suffix}"),
        producer: Producer {
            kind: ProducerKind::Alloy,
            name: "ferrum-alloy-telemetry".to_owned(),
            version: None,
            instance: None,
        },
        kind,
        name: name.to_owned(),
        availability: Availability::Unknown,
        value: None,
        unit: None,
        boundaries: catalog::entry(name).map(|entry| Boundaries {
            start: entry.start.to_owned(),
            end: entry.end.to_owned(),
        }),
        clock: None,
        interval: None,
        scope: Scope {
            leg: Leg::Service,
            service: Some(service.to_owned()),
            gateway: None,
            attempt: None,
        },
        span: Some(SpanRef {
            trace_id: record.trace_id.to_hex(),
            span_id: span_id.clone(),
            parent_span_id: None,
        }),
        attributes: BTreeMap::new(),
        trust: Trust::Unverified,
        evidence_ref: Some(format!("alloy:request/{span_id}")),
        unrecognized: BTreeMap::new(),
    }
}

/// A duration measurement, or `missing` when there is no value.
fn duration(
    mut observation: Observation,
    value: Option<Duration>,
    missing: Availability,
) -> Observation {
    match value {
        Some(value) => {
            observation.availability = Availability::Measured;
            observation.value = Some(value.as_secs_f64() * 1_000.0);
            observation.unit = Some(Unit::Milliseconds);
            observation.clock = Some(ClockDomain::MonotonicLocal);
        }
        None => observation.availability = missing,
    }
    observation
}

fn observations(service: &str, record: &Stored, out: &mut Vec<Observation>) {
    use ObservationKind::{Event, Measurement};
    let head = base(
        service,
        record,
        "time_to_headers",
        catalog::ALLOY_TIME_TO_HEADERS,
        Measurement,
    );
    // Headers that were never produced have no time: unavailable, not zero.
    let head = duration(head, record.time_to_headers, Availability::Unavailable);
    out.push(head);
    let body = base(
        service,
        record,
        "body",
        catalog::ALLOY_BODY_DURATION,
        Measurement,
    );
    // Without headers there is no body phase.
    let mut body = duration(body, record.body_duration, Availability::NotApplicable);
    body.attributes
        .insert("body.outcome".to_owned(), record.outcome.to_owned());
    out.push(body);
    let whole = base(
        service,
        record,
        "duration",
        catalog::ALLOY_SERVER_DURATION,
        Measurement,
    );
    let whole = duration(whole, Some(record.duration), Availability::Unknown);
    out.push(whole);
    let mut response = base(service, record, "response", catalog::ALLOY_RESPONSE, Event);
    response.availability = Availability::Measured;
    let attributes = &mut response.attributes;
    if let Some(status) = record.status {
        attributes.insert("status".to_owned(), status.to_string());
    }
    if let Some(route) = &record.route {
        attributes.insert("route".to_owned(), route.to_string());
    }
    attributes.insert("trace_parent".to_owned(), record.trace_decision.to_owned());
    attributes.insert("peer_trust".to_owned(), record.peer_trust.to_owned());
    out.push(response);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
    use ferrum_alloy_telemetry::{BodyOutcome, TraceDecision};

    use super::*;

    fn settings(max_records: usize, max_bytes: usize) -> DiagnosticsSettings {
        DiagnosticsSettings {
            max_records,
            max_bytes,
        }
    }

    fn id(text: &str) -> RequestId {
        RequestId::parse(text).unwrap()
    }

    fn evidence(tenant: Option<&str>, request_id: &str) -> RequestEvidence {
        let trace_id = TraceId::random();
        let mut evidence = RequestEvidence::new(id(request_id), trace_id, SpanId::random());
        evidence.tenant = tenant.map(Arc::from);
        evidence
    }

    /// Records of `tenant` for `request_id`.
    fn found(store: &EvidenceStore, tenant: &str, request_id: &str) -> usize {
        store.find(tenant, id(request_id)).len()
    }

    /// The value of an exact Prometheus series.
    fn metric(store: &EvidenceStore, series: &str) -> u64 {
        let text = store.render_prometheus();
        text.lines()
            .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or_else(|| panic!("{series} is missing from:\n{text}"))
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        let request = DiagnosticsRequest::new(None, headers.clone());
        request.bearer_token().map(str::to_owned)
    }

    #[test]
    fn the_ring_is_bounded_by_count() {
        let store = EvidenceStore::new(&settings(3, 1024 * 1024));
        for n in 0..10 {
            store.record(evidence(Some("acme"), &format!("req-{n}")));
            assert!(store.retained().0 <= 3);
        }
        assert_eq!(store.retained().0, 3);
        assert_eq!(found(&store, "acme", "req-6"), 0, "evicted");
        assert_eq!(found(&store, "acme", "req-7"), 1);
        assert_eq!(found(&store, "acme", "req-9"), 1);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, evicted), 7);
        assert_eq!(metric(&store, "ferrum_alloy_diagnostics_records"), 3);
        assert_eq!(metric(&store, "ferrum_alloy_diagnostics_stored_total"), 10);
        let ring = store.ring();
        let indexed: usize = ring.index.values().map(Vec::len).sum();
        assert_eq!(indexed, 3, "evicted records leave the index");
    }

    #[test]
    fn the_ring_is_bounded_by_bytes() {
        let max_bytes = 4_096;
        let store = EvidenceStore::new(&settings(1_000, max_bytes));
        let long = "r".repeat(200);
        for n in 0..50 {
            store.record(evidence(Some("acme"), &format!("{long}-{n:02}")));
            let (_, bytes) = store.retained();
            assert!(bytes <= max_bytes, "{bytes} > {max_bytes}");
        }
        let each = estimate("acme", &id(&format!("{long}-00")), None);
        let (records, bytes) = store.retained();
        assert_eq!(records, max_bytes / each);
        assert_eq!(bytes, records * each);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        let expected = u64::try_from(50 - records).unwrap();
        assert_eq!(metric(&store, evicted), expected);
        let expected = u64::try_from(bytes).unwrap();
        assert_eq!(metric(&store, "ferrum_alloy_diagnostics_bytes"), expected);
        assert_eq!(store.ring().index.len(), records);
    }

    #[test]
    fn untagged_requests_are_not_retained() {
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(evidence(None, "req-1"));
        assert_eq!(store.retained(), (0, 0));
        let untagged = r#"ferrum_alloy_diagnostics_skipped_total{reason="untagged"}"#;
        assert_eq!(metric(&store, untagged), 1);
    }

    #[test]
    fn records_are_scoped_to_their_tenant_and_capped_per_request_id() {
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(evidence(Some("acme"), "shared-id"));
        store.record(evidence(Some("globex"), "shared-id"));
        assert_eq!(found(&store, "acme", "shared-id"), 1);
        assert_eq!(found(&store, "globex", "shared-id"), 1);
        assert_eq!(found(&store, "initech", "shared-id"), 0);

        for _ in 0..40 {
            store.record(evidence(Some("acme"), "retried"));
        }
        assert_eq!(found(&store, "acme", "retried"), MAX_RECORDS_PER_REQUEST_ID);
        let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
        assert_eq!(metric(&store, limited), 24);
        assert_eq!(store.retained().0, 2 + MAX_RECORDS_PER_REQUEST_ID);
    }

    #[test]
    fn records_already_under_a_request_id_cannot_keep_a_later_one_out() {
        let store = EvidenceStore::new(&settings(1_000, 1024 * 1024));
        store.record(evidence(Some("acme"), "before"));
        // Records added first under a request id someone else will use.
        for _ in 0..MAX_RECORDS_PER_REQUEST_ID {
            store.record(evidence(Some("acme"), "predicted"));
        }
        store.record(evidence(Some("acme"), "after"));
        let genuine = evidence(Some("acme"), "predicted");
        let span_id = genuine.span_id;
        store.record(genuine);

        let records = store.find("acme", id("predicted"));
        assert_eq!(records.len(), MAX_RECORDS_PER_REQUEST_ID);
        let newest = records.last().unwrap();
        assert_eq!(newest.span_id, span_id, "the newest record is kept");
        assert_eq!(found(&store, "acme", "before"), 1);
        assert_eq!(found(&store, "acme", "after"), 1);
        let (records, bytes) = store.retained();
        assert_eq!(records, 2 + MAX_RECORDS_PER_REQUEST_ID);
        let each = estimate("acme", &id("predicted"), None);
        let before = estimate("acme", &id("before"), None);
        let after = estimate("acme", &id("after"), None);
        assert_eq!(bytes, each * MAX_RECORDS_PER_REQUEST_ID + before + after);
    }

    #[test]
    fn slots_of_records_evicted_out_of_order_count_until_they_are_oldest() {
        let store = EvidenceStore::new(&settings(20, 1024 * 1024));
        store.record(evidence(Some("acme"), "first"));
        for _ in 0..=MAX_RECORDS_PER_REQUEST_ID {
            store.record(evidence(Some("acme"), "retried"));
        }
        // The oldest "retried" record left its slot behind "first", and the
        // slot counts toward the count bound.
        assert_eq!(store.ring().records.len(), MAX_RECORDS_PER_REQUEST_ID + 2);
        assert_eq!(store.retained().0, MAX_RECORDS_PER_REQUEST_ID + 1);

        // Evicting "first" frees the slot after it too.
        for n in 0..3 {
            store.record(evidence(Some("acme"), &format!("other-{n}")));
        }
        assert_eq!(store.ring().records.len(), MAX_RECORDS_PER_REQUEST_ID + 3);
        assert_eq!(store.retained().0, MAX_RECORDS_PER_REQUEST_ID + 3);
        assert_eq!(found(&store, "acme", "first"), 0);
        assert_eq!(found(&store, "acme", "other-2"), 1);
        let count = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, count), 1);
        let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
        assert_eq!(metric(&store, limited), 1);
        let ring = store.ring();
        assert!(matches!(ring.records.front(), Some(Some(_))));
        let indexed: usize = ring.index.values().map(Vec::len).sum();
        assert_eq!(indexed, ring.live);
        let present = ring.records.iter().filter(|slot| slot.is_some()).count();
        assert_eq!(present, ring.live);
    }

    #[test]
    fn eviction_skips_empty_slots_at_the_front() {
        let store = EvidenceStore::new(&settings(20, 1024 * 1024));
        store.record(evidence(Some("acme"), "first"));
        store.record(evidence(Some("acme"), "second"));
        let mut ring = store.ring();
        // An empty slot at the front, which `trim` normally prevents.
        ring.records.push_front(None);
        ring.first_seq = ring.first_seq.wrapping_sub(1);

        assert!(ring.evict_oldest());
        assert_eq!(ring.live, 1);
        assert_eq!(ring.records.len(), 1);
        let second = (Arc::from("acme"), id("second"));
        assert_eq!(ring.find(&second).len(), 1);
        assert!(ring.find(&(Arc::from("acme"), id("first"))).is_empty());
        assert!(ring.evict_oldest());
        assert!(!ring.evict_oldest());
        assert!(ring.index.is_empty());
    }

    #[test]
    fn long_route_templates_are_dropped() {
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        let mut long = evidence(Some("acme"), "req-long");
        long.route = Some(Arc::from(format!("/{}", "x".repeat(MAX_ROUTE_BYTES))));
        store.record(long);
        let records = store.find("acme", id("req-long"));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].route, None);
    }

    #[test]
    fn reports_round_trip_through_the_offline_parser() {
        let mut first = evidence(Some("acme"), "req-7");
        first.route = Some(Arc::from("/orders/{id}"));
        first.status = Some(503);
        first.time_to_headers = Some(Duration::from_millis(12));
        first.body_duration = Some(Duration::from_millis(3));
        first.duration = Duration::from_millis(15);
        first.outcome = BodyOutcome::Completed;
        first.trace_decision = TraceDecision::AcceptedRemote;
        // A second attempt with the same id that never produced headers.
        let second = evidence(Some("acme"), "req-7");
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(first);
        store.record(second);
        let records = store.find("acme", id("req-7"));
        let json = serde_json::to_vec(&report("orders", &records)).unwrap();
        let parsed = parse_offline(&json, &Limits::default()).unwrap();
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(parsed.claimed_verification, Verification::Unverified);
        let report = parsed.report;
        assert_eq!(report.collection.method, CollectionMethod::LiveExport);
        assert_eq!(report.subject.request_id.as_deref(), Some("req-7"));
        assert_eq!(report.subject.route.as_deref(), Some("/orders/{id}"));
        assert_eq!(report.subject.trace_id, None, "two traces share the id");
        assert_eq!(report.observations.len(), 8);
        for observation in &report.observations {
            assert!(catalog::is_known(&observation.name), "{}", observation.name);
            assert_eq!(observation.trust, Trust::Unverified);
            assert_eq!(observation.producer.kind, ProducerKind::Alloy);
        }

        // Observations of the `attempt`th record.
        let get = |attempt: usize, suffix: &str| {
            let span = records[attempt].span_id.to_hex();
            let id = format!("alloy:{span}:{suffix}");
            report.observation(&id).unwrap().clone()
        };
        assert_eq!(get(0, "time_to_headers").duration_ms(), Some(12.0));
        let response = get(0, "response");
        assert_eq!(response.attr("status"), Some("503"));
        assert_eq!(response.attr("route"), Some("/orders/{id}"));
        assert_eq!(response.attr("trace_parent"), Some("accepted_remote"));
        assert_eq!(response.attr("peer_trust"), Some("untrusted"));

        let head = get(1, "time_to_headers");
        assert_eq!(head.availability, Availability::Unavailable);
        assert_eq!(head.value, None, "unknown is not zero");
        let body = get(1, "body");
        assert_eq!(body.availability, Availability::NotApplicable);
        let outcome = body.attr("body.outcome");
        assert_eq!(outcome, Some("cancelled_before_headers"));
        let response = get(1, "response");
        assert_eq!(response.attr("status"), None);
    }

    #[test]
    fn bearer_tokens_need_exactly_one_bearer_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer abc "));
        assert_eq!(bearer(&headers).as_deref(), Some("abc"));
        headers.append(AUTHORIZATION, HeaderValue::from_static("Bearer def"));
        assert_eq!(bearer(&headers), None, "two credentials are ambiguous");
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&headers), None);
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer  "));
        assert_eq!(bearer(&headers), None);
    }
}
