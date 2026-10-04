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
//!   tenant through their [`TenantTag`], shared fairly between tenants;
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

use std::collections::btree_set;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::iter::{Peekable, Rev};
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
use ferrum_alloy_telemetry::evidence::{
    EvidenceSink, RequestEvidence, RequestIdOrigin, valid_tenant,
};
use ferrum_alloy_telemetry::trace_context::{SpanId, TraceId};
use ferrum_alloy_telemetry::{PeerInfo, RequestId, TraceDecision};
use futures_util::FutureExt as _;
use http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderValue};

pub use ferrum_alloy_telemetry::evidence::TenantTag;

use crate::config::{AlloyConfig, ConfigError, DiagnosticsSettings};
use crate::problem::{Problem, ProblemKind};

/// Route of the retrieval endpoint on the management listener.
pub const ROUTE: &str = "/diagnostics/v1/requests/{request_id}";

/// How long the authorizer may take. A slower answer denies.
pub const AUTHORIZER_TIMEOUT: Duration = Duration::from_secs(5);

/// Records kept for one locally owned server request. Cloned evidence can
/// retain backend attempts within that request. Separate frontend requests
/// never join through remote correlation ids or traces.
pub const MAX_RECORDS_PER_REQUEST_ID: usize = 16;

/// Longest route template kept. A longer one is dropped from the record.
pub const MAX_ROUTE_BYTES: usize = 512;

/// Bytes of an `Arc<str>` allocation besides its text: its two reference
/// counts.
const ARC_HEADER_BYTES: usize = 2 * size_of::<usize>();

/// Estimated fixed cost of one record besides its strings: its entries in
/// the record map and in its tenant's sequence set, with B-tree nodes at
/// their minimum occupancy of 5 of 11 entries; an index entry and a tenant
/// entry with their hash table control bytes, at the tables' 7/8 maximum
/// load, including the alias and its owner map; the smallest sequence number
/// list; the tenant's entries in both tenant orderings; and the reference-count
/// headers of its tenant and route. It is an estimate, not a measurement:
/// allocator overhead and spare hash table capacity, which never shrinks,
/// are not counted, while an index, tenant, or ordering entry shared by records is
/// counted for each of them.
const RECORD_OVERHEAD_BYTES: usize = (2 * size_of::<u64>() + size_of::<Stored>()) * 11 / 5
    + (size_of::<(Key, Filed)>() + 1) * 8 / 7
    + (size_of::<(Arc<str>, Holding)>() + 1) * 8 / 7
    + (size_of::<(Alias, Aliased)>() + 1) * 8 / 7
    + (size_of::<(RequestId, usize)>() + 1) * 8 / 7
    + 2 * size_of::<(usize, Arc<str>)>() * 11 / 5
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

/// Applies the loopback rule of [`check_config`] to `addr`, the address the
/// management listener is actually bound to, which a listener passed to
/// `AlloyParts::serve_on` need not share with `management.bind`.
pub(crate) fn check_listener(addr: SocketAddr) -> Result<(), String> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(format!(
        "diagnostic retrieval requires a loopback management listener, but it is bound to {addr}; terminate TLS in a proxy on the same host instead"
    ))
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
    diagnostic_id: RequestId,
    origin: RequestIdOrigin,
    trace_id: TraceId,
    span_id: SpanId,
    route: Option<Arc<str>>,
    status: Option<u16>,
    time_to_headers: Option<Duration>,
    body_duration: Option<Duration>,
    duration: Duration,
    outcome: &'static str,
    trace_decision: TraceDecision,
    peer_trust: &'static str,
    /// Estimated retained bytes.
    bytes: usize,
}

impl Stored {
    /// Consistent correlation and trace provenance within a local request.
    /// Remote fields never establish ownership of a different local request.
    fn same_request(&self, other: &Self) -> bool {
        self.request_id == other.request_id
            && self.origin == other.origin
            && self.trace_id == other.trace_id
            && (self.trace_decision == TraceDecision::AcceptedRemote)
                == (other.trace_decision == TraceDecision::AcceptedRemote)
    }
}

/// Local ownership is independent of every remote correlation value.
type Key = (Arc<str>, RequestId);

/// Remote ids are bounded correlation aliases, scoped by tenant and origin.
type Alias = (Arc<str>, RequestIdOrigin, RequestId);

const LOOKUP_ORDER: [RequestIdOrigin; 3] = [
    RequestIdOrigin::Generated,
    RequestIdOrigin::TrustedPeer,
    RequestIdOrigin::UntrustedCaller,
];

#[derive(Debug, Default)]
struct Filed {
    /// Attempts of one local request, oldest first.
    seqs: Vec<u64>,
}

#[derive(Debug, Default)]
struct Aliased {
    /// Local owners and their retained record counts. A reused external
    /// alias is ambiguous, even when all owners share a remote trace.
    owners: HashMap<RequestId, usize>,
    records: usize,
}

/// What one tenant holds.
#[derive(Debug, Default)]
struct Holding {
    /// Sequence numbers of the tenant's records; lower is older.
    seqs: BTreeSet<u64>,
    /// Estimated bytes of the tenant's records.
    bytes: usize,
}

/// Retained records, indexed by key and by tenant.
#[derive(Debug, Default)]
struct Ring {
    /// Every record by sequence number; lower is older.
    records: BTreeMap<u64, Stored>,
    /// The next sequence number. A `u64` does not wrap in practice.
    next_seq: u64,
    index: HashMap<Key, Filed>,
    aliases: HashMap<Alias, Aliased>,
    tenants: HashMap<Arc<str>, Holding>,
    /// Tenants by the records they hold, then by name.
    by_count: BTreeSet<(usize, Arc<str>)>,
    /// Tenants by the estimated bytes they hold, then by name.
    by_bytes: BTreeSet<(usize, Arc<str>)>,
    /// Estimated bytes of every record.
    bytes: usize,
}

impl Ring {
    /// Records present.
    fn live(&self) -> usize {
        self.records.len()
    }

    /// Records and estimated bytes `tenant` holds.
    fn usage(&self, tenant: &str) -> (usize, usize) {
        self.tenants
            .get(tenant)
            .map_or((0, 0), |holding| (holding.seqs.len(), holding.bytes))
    }

    /// Takes `tenant` out of the orderings before its holding changes.
    fn unrank(&mut self, tenant: &Arc<str>) {
        let (count, bytes) = self.usage(tenant);
        self.by_count.remove(&(count, Arc::clone(tenant)));
        self.by_bytes.remove(&(bytes, Arc::clone(tenant)));
    }

    /// Puts `tenant` back into the orderings after its holding changed, or
    /// forgets it once it holds nothing.
    fn rank(&mut self, tenant: &Arc<str>) {
        let (count, bytes) = self.usage(tenant);
        if count == 0 {
            self.tenants.remove(tenant);
            return;
        }
        self.by_count.insert((count, Arc::clone(tenant)));
        self.by_bytes.insert((bytes, Arc::clone(tenant)));
    }

    /// Files `record` under its local owner as the newest record.
    fn push(&mut self, key: Key, record: Stored) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let tenant = Arc::clone(&record.tenant);
        self.unrank(&tenant);
        let holding = self.tenants.entry(Arc::clone(&tenant)).or_default();
        holding.seqs.insert(seq);
        holding.bytes = holding.bytes.saturating_add(record.bytes);
        self.rank(&tenant);
        self.bytes = self.bytes.saturating_add(record.bytes);
        let alias = (tenant, record.origin, record.request_id.clone());
        let aliased = self.aliases.entry(alias).or_default();
        *aliased.owners.entry(record.diagnostic_id.clone()).or_default() += 1;
        aliased.records += 1;
        self.index.entry(key).or_default().seqs.push(seq);
        self.records.insert(seq, record);
    }

    /// Removes a record and every ownership, alias, and tenant index entry
    /// that becomes empty with it.
    fn remove(&mut self, seq: u64) -> bool {
        let Some(record) = self.records.remove(&seq) else {
            return false;
        };
        self.bytes = self.bytes.saturating_sub(record.bytes);
        let tenant = Arc::clone(&record.tenant);
        self.unrank(&tenant);
        if let Some(holding) = self.tenants.get_mut(&tenant) {
            holding.seqs.remove(&seq);
            holding.bytes = holding.bytes.saturating_sub(record.bytes);
        }
        self.rank(&tenant);
        let key = (Arc::clone(&tenant), record.diagnostic_id.clone());
        if let Some(filed) = self.index.get_mut(&key) {
            filed.seqs.retain(|s| *s != seq);
            if filed.seqs.is_empty() {
                self.index.remove(&key);
            }
        }
        let alias = (tenant, record.origin, record.request_id);
        if let Some(aliased) = self.aliases.get_mut(&alias) {
            aliased.records -= 1;
            if let Some(count) = aliased.owners.get_mut(&record.diagnostic_id) {
                *count -= 1;
                if *count == 0 {
                    aliased.owners.remove(&record.diagnostic_id);
                }
            }
            if aliased.records == 0 {
                self.aliases.remove(&alias);
            }
        }
        true
    }

    /// Checks a local attempt's consistency, without modifying retention.
    /// Returns the cap replacement, if any, to reserve before eviction.
    fn admit(&self, key: &Key, record: &Stored) -> Result<Option<u64>, ()> {
        let Some(filed) = self.index.get(key) else {
            return Ok(None);
        };
        let first = filed.seqs.first().and_then(|seq| self.records.get(seq));
        if first.is_some_and(|first| !first.same_request(record)) {
            return Err(());
        }
        if filed.seqs.len() >= MAX_RECORDS_PER_REQUEST_ID {
            Ok(filed.seqs.first().copied())
        } else {
            Ok(None)
        }
    }

    fn owned(&self, key: &Key) -> Found {
        let records = self.index.get(key).into_iter().flat_map(|filed| &filed.seqs);
        Found {
            records: records
                .filter_map(|seq| self.records.get(seq))
                .cloned()
                .collect(),
            shadowed: 0,
        }
    }

    /// A local id is authoritative. Otherwise the best available origin's
    /// correlation alias must name exactly one retained local owner. Reuse
    /// never silently selects the first request or combines their traces.
    fn find(&self, tenant: &Arc<str>, request_id: &RequestId) -> Found {
        let key = (Arc::clone(tenant), request_id.clone());
        if self.index.contains_key(&key) {
            return self.owned(&key);
        }
        let mut alias = (Arc::clone(tenant), LOOKUP_ORDER[0], request_id.clone());
        let mut found = Found::default();
        for origin in LOOKUP_ORDER {
            alias.1 = origin;
            let Some(aliased) = self.aliases.get(&alias) else {
                continue;
            };
            if !found.records.is_empty() {
                found.shadowed = found.shadowed.saturating_add(aliased.records);
                continue;
            }
            if aliased.owners.len() != 1 {
                return Found::default();
            }
            if let Some(owner) = aliased.owners.keys().next() {
                found = self.owned(&(Arc::clone(tenant), owner.clone()));
            }
        }
        found
    }
}

/// An immutable ordering plus updated ranks for only the reserved tenants.
/// Each original entry is visited at most once during an admission; no
/// full-store copy or repeated scan is needed for a multi-record reservation.
struct Ranking<'a> {
    original: Peekable<Rev<btree_set::Iter<'a, (usize, Arc<str>)>>>,
    updated: BTreeSet<(usize, Arc<str>)>,
}

impl<'a> Ranking<'a> {
    fn new(original: &'a BTreeSet<(usize, Arc<str>)>) -> Self {
        Self {
            original: original.iter().rev().peekable(),
            updated: BTreeSet::new(),
        }
    }

    fn heaviest(&mut self, changed: &HashMap<Arc<str>, (usize, usize)>) -> Option<Arc<str>> {
        while self
            .original
            .peek()
            .is_some_and(|(_, tenant)| changed.contains_key(tenant))
        {
            self.original.next();
        }
        self.original
            .peek()
            .copied()
            .max(self.updated.last())
            .map(|(_, tenant)| Arc::clone(tenant))
    }

    fn change(&mut self, tenant: &Arc<str>, before: usize, after: usize) {
        self.updated.remove(&(before, Arc::clone(tenant)));
        if after > 0 {
            self.updated.insert((after, Arc::clone(tenant)));
        }
    }
}

/// A preflight reservation, held under the same lock as its eventual commit.
/// Rejection discards this plan and leaves all previously retained records.
struct Reservation<'a> {
    ring: &'a Ring,
    by_count: Ranking<'a>,
    by_bytes: Ranking<'a>,
    changed: HashMap<Arc<str>, (usize, usize)>,
    cursors: HashMap<Arc<str>, u64>,
    evictions: BTreeMap<u64, Evicted>,
    count: usize,
    bytes: usize,
}

impl<'a> Reservation<'a> {
    fn new(ring: &'a Ring) -> Self {
        Self {
            ring,
            by_count: Ranking::new(&ring.by_count),
            by_bytes: Ranking::new(&ring.by_bytes),
            changed: HashMap::new(),
            cursors: HashMap::new(),
            evictions: BTreeMap::new(),
            count: ring.live(),
            bytes: ring.bytes,
        }
    }

    fn usage(&self, tenant: &str) -> (usize, usize) {
        self.changed
            .get(tenant)
            .copied()
            .unwrap_or_else(|| self.ring.usage(tenant))
    }

    fn oldest(&self, tenant: &str) -> Option<u64> {
        let seqs = &self.ring.tenants.get(tenant)?.seqs;
        let after = self
            .cursors
            .get(tenant)
            .map_or(std::ops::Bound::Unbounded, |seq| {
                std::ops::Bound::Excluded(*seq)
            });
        seqs.range((after, std::ops::Bound::Unbounded))
            .find(|seq| !self.evictions.contains_key(*seq))
            .copied()
    }

    fn reserve(&mut self, seq: u64, reason: Evicted, advance: bool) {
        let Some(record) = self.ring.records.get(&seq) else {
            return;
        };
        let tenant = &record.tenant;
        let (count, bytes) = self.usage(tenant);
        let remaining = (count.saturating_sub(1), bytes.saturating_sub(record.bytes));
        self.by_count.change(tenant, count, remaining.0);
        self.by_bytes.change(tenant, bytes, remaining.1);
        self.changed.insert(Arc::clone(tenant), remaining);
        if advance {
            self.cursors.insert(Arc::clone(tenant), seq);
        }
        self.evictions.insert(seq, reason);
        self.count = self.count.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(record.bytes);
    }

    /// Reclaim the heaviest tenant's oldest record only if its remaining
    /// holding is at least the candidate tenant's projected holding in the
    /// pressured dimension. Otherwise reserve our own oldest. If neither is
    /// possible, refuse admission; there is no global-oldest fallback.
    fn victim(&mut self, record: &Stored, reason: Evicted) -> Option<u64> {
        let (count, bytes) = self.usage(&record.tenant);
        let (will_hold, other) = match reason {
            Evicted::Bytes => (
                bytes.saturating_add(record.bytes),
                self.by_bytes.heaviest(&self.changed),
            ),
            _ => (
                count.saturating_add(1),
                self.by_count.heaviest(&self.changed),
            ),
        };
        if let Some(other) = other.filter(|other| other != &record.tenant)
            && let Some(seq) = self.oldest(&other)
            && let Some(victim) = self.ring.records.get(&seq)
        {
            let (count, bytes) = self.usage(&other);
            let remaining = match reason {
                Evicted::Bytes => bytes.saturating_sub(victim.bytes),
                _ => count.saturating_sub(1),
            };
            if remaining >= will_hold {
                return Some(seq);
            }
        }
        self.oldest(&record.tenant)
    }

    fn plan(
        mut self,
        record: &Stored,
        replacement: Option<u64>,
        max_records: usize,
        max_bytes: usize,
    ) -> Option<BTreeMap<u64, Evicted>> {
        if let Some(seq) = replacement {
            self.reserve(seq, Evicted::RequestIdLimit, false);
        }
        loop {
            let reason = if self.count >= max_records {
                Evicted::Count
            } else if self.bytes.saturating_add(record.bytes) > max_bytes {
                Evicted::Bytes
            } else {
                return Some(self.evictions);
            };
            let victim = self.victim(record, reason)?;
            self.reserve(victim, reason, true);
        }
    }
}

/// What a lookup found.
#[derive(Debug, Default)]
struct Found {
    /// The records, oldest first.
    records: Vec<Stored>,
    /// Records from less preferred correlation origins under this alias, which
    /// the lookup does not answer with.
    shadowed: usize,
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
    RequestIdConflict,
    FairShare,
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
    skipped: [AtomicU64; 4],
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
        let key = (Arc::clone(&record.tenant), record.diagnostic_id.clone());
        let mut ring = self.ring();
        let replacement = match ring.admit(&key, &record) {
            Ok(replacement) => replacement,
            Err(()) => {
                drop(ring);
                self.count_skipped(Skipped::RequestIdConflict);
                return;
            }
        };
        let plan = Reservation::new(&ring).plan(
            &record,
            replacement,
            self.max_records,
            self.max_bytes,
        );
        let Some(plan) = plan else {
            drop(ring);
            self.count_skipped(Skipped::FairShare);
            return;
        };
        let mut evicted = [0u64; 3];
        for (seq, reason) in plan {
            if ring.remove(seq) {
                evicted[reason as usize] += 1;
            }
        }
        ring.push(key, record);
        drop(ring);
        self.stored.fetch_add(1, Ordering::Relaxed);
        for (counter, count) in self.evicted.iter().zip(evicted) {
            if count > 0 {
                counter.fetch_add(count, Ordering::Relaxed);
            }
        }
    }

    /// What `tenant` has under `request_id`.
    fn find(&self, tenant: &str, request_id: RequestId) -> Found {
        self.ring().find(&Arc::from(tenant), &request_id)
    }

    /// Records and estimated bytes currently retained.
    pub(crate) fn retained(&self) -> (usize, usize) {
        let ring = self.ring();
        (ring.live(), ring.bytes)
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
                    ("request_id_conflict", &self.skipped[2]),
                    ("fair_share", &self.skipped[3]),
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

/// Charge every allocated id copy, including local ownership in the record,
/// primary index and alias owner map; remote correlation in record and alias
/// key; and the tenant/route text. Shared entries are charged to every record.
fn estimate(
    tenant: &str,
    request_id: &RequestId,
    diagnostic_id: &RequestId,
    route: Option<&str>,
) -> usize {
    RECORD_OVERHEAD_BYTES
        .saturating_add(tenant.len())
        .saturating_add(request_id.allocation_bytes().saturating_mul(2))
        .saturating_add(diagnostic_id.allocation_bytes().saturating_mul(3))
        .saturating_add(route.map_or(0, str::len))
}

impl EvidenceSink for EvidenceStore {
    fn record(&self, evidence: RequestEvidence) {
        let diagnostic_id = evidence.diagnostic_id().clone();
        let Some(tenant) = evidence.tenant else {
            self.count_skipped(Skipped::Untagged);
            return;
        };
        let route = evidence.route.filter(|r| r.len() <= MAX_ROUTE_BYTES);
        let bytes = estimate(
            &tenant,
            &evidence.request_id,
            &diagnostic_id,
            route.as_deref(),
        );
        self.insert(Stored {
            tenant,
            request_id: evidence.request_id,
            diagnostic_id,
            origin: evidence.request_id_origin,
            trace_id: evidence.trace_id,
            span_id: evidence.span_id,
            route,
            status: evidence.status,
            time_to_headers: evidence.time_to_headers,
            body_duration: evidence.body_duration,
            duration: evidence.duration,
            outcome: evidence.outcome.as_str(),
            trace_decision: evidence.trace_decision,
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
        let found = request_id
            .ok()
            .and_then(|Path(id)| RequestId::parse(&id))
            .map(|id| self.store.find(&tenant, id))
            .unwrap_or_default();
        if found.records.is_empty() {
            self.store.count_retrieval(Retrieved::NotFound);
            return not_found();
        }
        let report = report(&self.service, &found);
        match serde_json::to_vec(&report) {
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

/// Builds the report for what a lookup of one tenant and request id found.
fn report(service: &str, found: &Found) -> DiagnosticReport {
    let records = &found.records;
    let mut notes = vec![
        "assembled by the service from its own request telemetry; a reader cannot authenticate the producer".to_owned(),
        "server-level timing only; instrumented operations are not retained".to_owned(),
    ];
    if let Some(first) = records.first() {
        notes.push(format!(
            "local diagnostic lookup id: {}; remote correlation ids and traces do not establish request ownership",
            first.diagnostic_id
        ));
    }
    let shadowed = found.shadowed;
    if shadowed > 0 {
        notes.push(format!(
            "{shadowed} record(s) from less preferred correlation origins under this alias are not included"
        ));
    }
    let mut report = DiagnosticReport::new(Collection {
        collector: Producer {
            kind: ProducerKind::Alloy,
            name: "ferrum-alloy".to_owned(),
            version: None,
            instance: None,
        },
        method: CollectionMethod::LiveExport,
        verification: Verification::Unverified,
        notes,
    });
    let first = records.first();
    report.subject.request_id = first.map(|record| record.request_id.as_str().to_owned());
    report.subject.service = Some(service.to_owned());
    report.subject.trace_id = first.map(|record| record.trace_id.to_hex());
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
    let decision = record.trace_decision.as_str().to_owned();
    attributes.insert("trace_parent".to_owned(), decision);
    attributes.insert("peer_trust".to_owned(), record.peer_trust.to_owned());
    let origin = record.origin.as_str().to_owned();
    attributes.insert("request_id_origin".to_owned(), origin);
    out.push(response);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
    use ferrum_alloy_telemetry::BodyOutcome;

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

    /// A request whose id this process generated.
    fn evidence(tenant: Option<&str>, request_id: &str) -> RequestEvidence {
        let trace_id = TraceId::random();
        let mut evidence = RequestEvidence::new(id(request_id), trace_id, SpanId::random());
        evidence.tenant = tenant.map(Arc::from);
        evidence
    }

    /// A fresh frontend request with accepted gateway correlation. Clone
    /// it to model attempts within that same locally owned request.
    fn attempt(tenant: &str, request_id: &str, trace_id: TraceId) -> RequestEvidence {
        let mut evidence = evidence(Some(tenant), request_id);
        evidence.trace_id = trace_id;
        evidence.trace_decision = TraceDecision::AcceptedRemote;
        evidence.request_id_origin = RequestIdOrigin::TrustedPeer;
        evidence
    }

    /// A request whose id a trusted gateway sent without trace context, so
    /// it was rooted in a trace of its own.
    fn untraced(tenant: &str, request_id: &str) -> RequestEvidence {
        let mut evidence = evidence(Some(tenant), request_id);
        evidence.request_id_origin = RequestIdOrigin::TrustedPeer;
        evidence
    }

    /// Records of `tenant` for `request_id`.
    fn found(store: &EvidenceStore, tenant: &str, request_id: &str) -> usize {
        store.find(tenant, id(request_id)).records.len()
    }

    /// Records `tenant` holds.
    fn held(store: &EvidenceStore, tenant: &str) -> usize {
        store.ring().usage(tenant).0
    }

    /// The value of an exact Prometheus series.
    fn metric(store: &EvidenceStore, series: &str) -> u64 {
        let text = store.render_prometheus();
        text.lines()
            .find_map(|line| line.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or_else(|| panic!("{series} is missing from:\n{text}"))
    }

    /// Checks that the indexes, orderings, and totals agree with the
    /// records.
    fn check(store: &EvidenceStore) {
        let ring = store.ring();
        let bytes: usize = ring.records.values().map(|record| record.bytes).sum();
        assert_eq!(ring.bytes, bytes);
        let filed: usize = ring.index.values().map(|filed| filed.seqs.len()).sum();
        assert_eq!(filed, ring.live());
        let aliases: usize = ring.aliases.values().map(|alias| alias.records).sum();
        assert_eq!(aliases, ring.live());
        for (alias, aliased) in &ring.aliases {
            assert_eq!(aliased.records, aliased.owners.values().sum::<usize>());
            for (owner, count) in &aliased.owners {
                let key = (Arc::clone(&alias.0), owner.clone());
                assert_eq!(*count, ring.index[&key].seqs.len());
            }
        }
        let holdings: usize = ring.tenants.values().map(|held| held.seqs.len()).sum();
        assert_eq!(holdings, ring.live());
        assert_eq!(ring.by_count.len(), ring.tenants.len());
        assert_eq!(ring.by_bytes.len(), ring.tenants.len());
        for (seq, record) in &ring.records {
            assert!(ring.tenants[&record.tenant].seqs.contains(seq));
            let key = (Arc::clone(&record.tenant), record.diagnostic_id.clone());
            let filed = &ring.index[&key];
            assert!(filed.seqs.contains(seq));
            let first = &ring.records[&filed.seqs[0]];
            assert!(first.same_request(record));
            let alias = (
                Arc::clone(&record.tenant),
                record.origin,
                record.request_id.clone(),
            );
            assert!(ring.aliases[&alias].owners.contains_key(&record.diagnostic_id));
        }
        for (tenant, holding) in &ring.tenants {
            let count = (holding.seqs.len(), Arc::clone(tenant));
            assert!(ring.by_count.contains(&count));
            let size = (holding.bytes, Arc::clone(tenant));
            assert!(ring.by_bytes.contains(&size));
            let bytes: usize = holding.seqs.iter().map(|s| ring.records[s].bytes).sum();
            assert_eq!(holding.bytes, bytes);
        }
    }

    fn sized(tenant: &str, request_id: &RequestId, route: Option<&str>) -> usize {
        let diagnostic_id = id("00000000-0000-4000-8000-000000000000");
        estimate(tenant, request_id, &diagnostic_id, route)
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
        check(&store);
        let indexed = store.ring().index.len();
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
        let each = sized("acme", &id(&format!("{long}-00")), None);
        let (records, bytes) = store.retained();
        assert_eq!(records, max_bytes / each);
        assert_eq!(bytes, records * each);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        let expected = u64::try_from(50 - records).unwrap();
        assert_eq!(metric(&store, evicted), expected);
        let expected = u64::try_from(bytes).unwrap();
        assert_eq!(metric(&store, "ferrum_alloy_diagnostics_bytes"), expected);
        assert_eq!(store.ring().index.len(), records);
        check(&store);
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
    fn local_attempts_share_a_cap_but_fresh_remote_pairs_never_join() {
        let store = EvidenceStore::new(&settings(1_000, 1024 * 1024));
        let trace = TraceId::random();
        let first = attempt("acme", "shared-id", trace);
        let owner = first.diagnostic_id().clone();
        store.record(evidence(Some("acme"), "before"));
        for n in 0..40 {
            let mut retry = first.clone();
            retry.span_id = SpanId::random();
            retry.status = Some(500 + n);
            store.record(retry);
        }
        let records = store.find("acme", owner.clone()).records;
        assert_eq!(records.len(), MAX_RECORDS_PER_REQUEST_ID);
        assert_eq!(records[0].status, Some(524));
        assert_eq!(records.last().unwrap().status, Some(539));
        assert_eq!(found(&store, "acme", "shared-id"), MAX_RECORDS_PER_REQUEST_ID);
        assert_eq!(found(&store, "acme", "before"), 1);
        let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
        assert_eq!(metric(&store, limited), 24);

        // Same remote id, trace and origin; still a different local owner.
        let next = attempt("acme", "shared-id", trace);
        let next_owner = next.diagnostic_id().clone();
        store.record(next);
        assert_eq!(found(&store, "acme", "shared-id"), 0, "ambiguous alias");
        assert_eq!(
            store.find("acme", owner).records.len(),
            MAX_RECORDS_PER_REQUEST_ID
        );
        assert_eq!(store.find("acme", next_owner.clone()).records.len(), 1);
        assert!(store.find("globex", next_owner).records.is_empty());
        store.record(attempt("globex", "shared-id", trace));
        assert_eq!(found(&store, "globex", "shared-id"), 1);
        assert_eq!(metric(&store, limited), 24);
        check(&store);
    }

    #[test]
    fn a_preclaimed_remote_pair_cannot_suppress_the_next_local_request() {
        let store = EvidenceStore::new(&settings(1_000, 1024 * 1024));
        let trace = TraceId::random();
        let first = attempt("acme", "predictable", trace);
        let first_owner = first.diagnostic_id().clone();
        let first_span = first.span_id;
        store.record(first);
        let mut owners = Vec::new();
        for _ in 0..(2 * MAX_RECORDS_PER_REQUEST_ID) {
            let next = attempt("acme", "predictable", trace);
            owners.push((next.diagnostic_id().clone(), next.span_id));
            store.record(next);
        }
        assert_eq!(found(&store, "acme", "predictable"), 0);
        assert_eq!(store.find("acme", first_owner).records[0].span_id, first_span);
        for (owner, span) in owners {
            let found = store.find("acme", owner);
            assert_eq!(found.records.len(), 1);
            assert_eq!(found.records[0].span_id, span);
            assert_eq!(found.records[0].trace_id, trace);
        }
        let conflicts = r#"ferrum_alloy_diagnostics_skipped_total{reason="request_id_conflict"}"#;
        let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
        assert_eq!(metric(&store, conflicts), 0);
        assert_eq!(metric(&store, limited), 0);
        check(&store);
    }

    #[test]
    fn inconsistent_cloned_local_evidence_is_rejected_before_eviction() {
        let store = EvidenceStore::new(&settings(1, 1024 * 1024));
        let first = attempt("acme", "edge-1", TraceId::random());
        let owner = first.diagnostic_id().clone();
        store.record(first.clone());
        let mut copied_trace = first.clone();
        copied_trace.trace_decision = TraceDecision::Root;
        store.record(copied_trace);
        let mut changed_trace = first.clone();
        changed_trace.trace_id = TraceId::random();
        store.record(changed_trace);
        let mut changed_id = first.clone();
        changed_id.request_id = id("other");
        store.record(changed_id);
        let mut changed_origin = first;
        changed_origin.request_id_origin = RequestIdOrigin::UntrustedCaller;
        store.record(changed_origin);
        assert_eq!(store.find("acme", owner).records.len(), 1);
        let conflicts = r#"ferrum_alloy_diagnostics_skipped_total{reason="request_id_conflict"}"#;
        assert_eq!(metric(&store, conflicts), 4);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, evicted), 0);
        check(&store);
    }

    #[test]
    fn a_larger_local_cap_replacement_reserves_its_extra_bytes_atomically() {
        let each = sized("acme", &id("retry"), None);
        let max_bytes = (MAX_RECORDS_PER_REQUEST_ID + 1) * each;
        let store = EvidenceStore::new(&settings(100, max_bytes));
        let first = attempt("acme", "retry", TraceId::random());
        let owner = first.diagnostic_id().clone();
        for n in 0..MAX_RECORDS_PER_REQUEST_ID {
            let mut next = first.clone();
            next.span_id = SpanId::random();
            next.status = Some(u16::try_from(n).unwrap());
            store.record(next);
        }
        store.record(evidence(Some("else"), "quiet"));
        assert_eq!(store.retained().1, (MAX_RECORDS_PER_REQUEST_ID + 1) * each);
        let quiet = report("test", &store.find("else", id("quiet")));
        let mut larger = first;
        larger.route = Some(Arc::from("/".repeat(100)));
        larger.span_id = SpanId::random();
        let latest_span = larger.span_id;
        store.record(larger);
        let found = store.find("acme", owner);
        assert_eq!(found.records.len(), MAX_RECORDS_PER_REQUEST_ID - 1);
        assert_eq!(found.records[0].status, Some(2));
        assert_eq!(found.records.last().unwrap().span_id, latest_span);
        assert_eq!(report("test", &store.find("else", id("quiet"))), quiet);
        let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
        let bytes = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        assert_eq!(metric(&store, limited), 1);
        assert_eq!(metric(&store, bytes), 1);
        check(&store);
    }

    #[test]
    fn alias_ambiguity_expires_with_its_last_conflicting_owner() {
        let store = EvidenceStore::new(&settings(2, 1024 * 1024));
        let first = untraced("acme", "edge-1");
        let first_owner = first.diagnostic_id().clone();
        store.record(first);
        let next = untraced("acme", "edge-1");
        let next_owner = next.diagnostic_id().clone();
        store.record(next);
        assert_eq!(found(&store, "acme", "edge-1"), 0);
        store.record(evidence(Some("acme"), "other"));
        assert!(store.find("acme", first_owner).records.is_empty());
        assert_eq!(found(&store, "acme", "edge-1"), 1);
        assert_eq!(store.find("acme", next_owner).records.len(), 1);
        store.record(evidence(Some("acme"), "last"));
        assert_eq!(found(&store, "acme", "edge-1"), 0);
        assert_eq!(store.ring().aliases.len(), 2);
        check(&store);
    }

    #[test]
    fn alias_origin_preference_never_resolves_ambiguous_local_owners() {
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        let mut caller = attempt("acme", "external", TraceId::random());
        caller.request_id_origin = RequestIdOrigin::UntrustedCaller;
        store.record(caller);
        store.record(attempt("acme", "external", TraceId::random()));
        let found = store.find("acme", id("external"));
        assert_eq!(found.records.len(), 1);
        assert_eq!(found.records[0].origin, RequestIdOrigin::TrustedPeer);
        assert_eq!(found.shadowed, 1);
        store.record(attempt("acme", "external", TraceId::random()));
        assert!(store.find("acme", id("external")).records.is_empty());
        assert_eq!(store.retained().0, 3);
        check(&store);
    }

    #[test]
    fn caller_aliases_cannot_shadow_a_local_lookup_id() {
        let store = EvidenceStore::new(&settings(1_000, 1024 * 1024));
        let first = evidence(Some("acme"), "generated");
        let owner = first.diagnostic_id().clone();
        let first_span = first.span_id;
        store.record(first);
        for _ in 0..32 {
            let mut next = attempt("acme", owner.as_str(), TraceId::random());
            next.request_id_origin = RequestIdOrigin::UntrustedCaller;
            store.record(next);
        }
        let found = store.find("acme", owner);
        assert_eq!(found.records.len(), 1);
        assert_eq!(found.records[0].span_id, first_span);
        assert_eq!(store.retained().0, 33);
        check(&store);
    }

    #[test]
    fn a_tenant_at_its_share_evicts_only_its_own_records() {
        let store = EvidenceStore::new(&settings(10, 1024 * 1024));
        for n in 0..3 {
            store.record(evidence(Some("quiet"), &format!("quiet-{n}")));
        }
        for n in 0..100 {
            store.record(evidence(Some("noisy"), &format!("noisy-{n}")));
            assert!(store.retained().0 <= 10);
        }
        for n in 0..3 {
            assert_eq!(found(&store, "quiet", &format!("quiet-{n}")), 1, "{n}");
        }
        assert_eq!(held(&store, "noisy"), 7);
        assert_eq!(found(&store, "noisy", "noisy-92"), 0, "evicted");
        assert_eq!(found(&store, "noisy", "noisy-93"), 1);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, evicted), 93);
        check(&store);
    }

    #[test]
    fn a_tenant_above_its_share_yields_to_others_down_to_an_equal_share() {
        let store = EvidenceStore::new(&settings(10, 1024 * 1024));
        for n in 0..10 {
            store.record(evidence(Some("big"), &format!("big-{n}")));
        }
        for n in 0..10 {
            store.record(evidence(Some("small"), &format!("small-{n}")));
            check(&store);
        }
        assert_eq!(held(&store, "big"), 5);
        assert_eq!(held(&store, "small"), 5);
        // Each lost its oldest records.
        assert_eq!(found(&store, "big", "big-4"), 0);
        assert_eq!(found(&store, "big", "big-5"), 1);
        assert_eq!(found(&store, "small", "small-4"), 0);
        assert_eq!(found(&store, "small", "small-5"), 1);

        // At an equal share, each evicts only its own.
        store.record(evidence(Some("big"), "big-10"));
        assert_eq!(held(&store, "big"), 5);
        assert_eq!(held(&store, "small"), 5);
        assert_eq!(found(&store, "big", "big-5"), 0);
        assert_eq!(found(&store, "small", "small-5"), 1);
        check(&store);
    }

    #[test]
    fn a_tenant_at_its_byte_share_evicts_only_its_own_records() {
        let max_bytes = 8_192;
        let store = EvidenceStore::new(&settings(1_000, max_bytes));
        for n in 0..2 {
            store.record(evidence(Some("quiet"), &format!("quiet-{n}")));
        }
        let long = "n".repeat(200);
        for n in 0..100 {
            store.record(evidence(Some("noisy"), &format!("{long}-{n:03}")));
            let (_, bytes) = store.retained();
            assert!(bytes <= max_bytes, "{bytes} > {max_bytes}");
        }
        for n in 0..2 {
            assert_eq!(found(&store, "quiet", &format!("quiet-{n}")), 1, "{n}");
        }
        assert!(held(&store, "noisy") > 0);
        assert_eq!(found(&store, "noisy", &format!("{long}-099")), 1);
        let bytes = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        assert!(metric(&store, bytes) > 0);
        let count = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, count), 0);
        check(&store);
    }

    #[test]
    fn a_tenant_above_its_byte_share_yields_to_others() {
        let max_bytes = 8_192;
        let store = EvidenceStore::new(&settings(1_000, max_bytes));
        let long = "b".repeat(200);
        for n in 0..10 {
            store.record(evidence(Some("big"), &format!("{long}-{n}")));
        }
        let before = held(&store, "big");
        assert!(before > 2, "{before}");
        for n in 0..20 {
            store.record(evidence(Some("small"), &format!("small-{n:02}")));
            let (_, bytes) = store.retained();
            assert!(bytes <= max_bytes, "{bytes} > {max_bytes}");
            check(&store);
        }

        // The big tenant gave up its oldest records, and kept its newest.
        let after = held(&store, "big");
        assert!(after < before, "{after} >= {before}");
        let oldest_before = format!("{long}-{}", 10 - before);
        assert_eq!(found(&store, "big", &oldest_before), 0);
        assert_eq!(found(&store, "big", &format!("{long}-9")), 1);
        // A whole donor record cannot be removed if its remaining bytes
        // would fall below the small tenant's projected holding. This can
        // leave an indivisible record above equal shares.
        let small = sized("small", &id("small-00"), None);
        let big = sized("big", &id(&format!("{long}-0")), None);
        let (big_bytes, small_bytes) = {
            let ring = store.ring();
            (ring.usage("big").1, ring.usage("small").1)
        };
        assert!(big_bytes.saturating_sub(big) < small_bytes + small, "{big_bytes}");
        assert!(small_bytes <= big_bytes + big, "{small_bytes}");
        let count = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, count), 0);
    }

    #[test]
    fn records_evicted_out_of_order_free_their_room_at_once() {
        let store = EvidenceStore::new(&settings(20, 1024 * 1024));
        store.record(evidence(Some("acme"), "first"));
        let retry = attempt("acme", "retried", TraceId::random());
        for _ in 0..=MAX_RECORDS_PER_REQUEST_ID {
            store.record(retry.clone());
        }
        // The oldest attempt was evicted from behind "first", and its room
        // is free at once.
        assert_eq!(store.retained().0, MAX_RECORDS_PER_REQUEST_ID + 1);
        for n in 0..3 {
            store.record(evidence(Some("acme"), &format!("other-{n}")));
        }
        assert_eq!(store.retained().0, MAX_RECORDS_PER_REQUEST_ID + 4);
        let count = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, count), 0);
        assert_eq!(found(&store, "acme", "first"), 1);

        // The count bound evicts the oldest record left.
        store.record(evidence(Some("acme"), "other-3"));
        assert_eq!(store.retained().0, 20);
        assert_eq!(metric(&store, count), 1);
        assert_eq!(found(&store, "acme", "first"), 0);
        let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
        assert_eq!(metric(&store, limited), 1);
        check(&store);
    }

    #[test]
    fn a_new_tenant_cannot_displace_another_tenants_sole_record() {
        let store = EvidenceStore::new(&settings(2, 1024 * 1024));
        store.record(evidence(Some("a"), "a-1"));
        store.record(evidence(Some("b"), "b-1"));
        store.record(evidence(Some("c"), "c-1"));
        assert_eq!(found(&store, "a", "a-1"), 1);
        assert_eq!(found(&store, "b", "b-1"), 1);
        assert_eq!(found(&store, "c", "c-1"), 0);
        let skipped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
        assert_eq!(metric(&store, skipped), 1);
        check(&store);
    }

    #[test]
    fn variable_byte_admission_rejects_without_losing_any_tenants_records() {
        for (id_length, route_length) in [(1, 0), (80, 120), (200, 300)] {
            let short_id = "s".repeat(id_length);
            let short_route = "/%E2%82%AC".repeat(route_length / 10);
            let each = sized("t0", &id(&short_id), Some(&short_route));
            let store = EvidenceStore::new(&settings(32, 6 * each));
            let mut originals = Vec::new();
            for n in 0..6 {
                let tenant = format!("t{n}");
                let mut first = evidence(Some(&tenant), &short_id);
                first.route = Some(Arc::from(short_route.as_str()));
                originals.push((tenant, first.diagnostic_id().clone()));
                store.record(first);
            }
            assert_eq!(store.retained(), (6, 6 * each));
            let before: Vec<_> = originals
                .iter()
                .map(|(tenant, owner)| report("test", &store.find(tenant, owner.clone())))
                .collect();
            let mut larger = evidence(Some("t0"), &"l".repeat(id_length + 20));
            larger.route = Some(Arc::from(format!("{short_route}{}", "/".repeat(80))));
            let larger_bytes = sized("t0", &larger.request_id, larger.route.as_deref());
            assert!(each < larger_bytes && larger_bytes < 2 * each);
            let owner = larger.diagnostic_id().clone();
            store.record(larger);
            assert!(store.find("t0", owner).records.is_empty());
            assert_eq!(store.retained(), (6, 6 * each));
            for ((tenant, owner), expected) in originals.iter().zip(before) {
                assert_eq!(report("test", &store.find(tenant, owner.clone())), expected);
            }
            let dropped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
            let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
            assert_eq!(metric(&store, dropped), 1);
            assert_eq!(metric(&store, evicted), 0);
            assert_eq!(metric(&store, "ferrum_alloy_diagnostics_stored_total"), 6);
            assert_eq!(store.ring().next_seq, 6);
            check(&store);
        }
    }

    #[test]
    fn a_rejected_reservation_also_preserves_planned_donor_evictions() {
        let each = sized("old", &id("small"), None);
        let store = EvidenceStore::new(&settings(10, 3 * each));
        for _ in 0..3 {
            store.record(evidence(Some("old"), "small"));
        }
        let before = store.retained();
        let owners: Vec<_> = store
            .ring()
            .records
            .values()
            .map(|record| record.diagnostic_id.clone())
            .collect();
        let mut candidate = evidence(Some("new"), "large");
        candidate.route = Some(Arc::from("/".repeat(100)));
        // Reserving one donor record leaves a real overshare, but a second
        // would cross the candidate's holding. The first reservation rolls back.
        store.record(candidate);
        assert_eq!(store.retained(), before);
        for owner in owners {
            assert_eq!(store.find("old", owner).records.len(), 1);
        }
        let dropped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        assert_eq!(metric(&store, dropped), 1);
        assert_eq!(metric(&store, evicted), 0);
        check(&store);
    }

    #[test]
    fn a_large_oldest_donor_record_cannot_cross_the_reserved_share() {
        let mut big = evidence(Some("donor"), &"b".repeat(200));
        big.route = Some(Arc::from("/".repeat(400)));
        let small = evidence(Some("donor"), "small");
        let large_bytes = sized("donor", &big.request_id, big.route.as_deref());
        let small_bytes = sized("donor", &small.request_id, None);
        let store = EvidenceStore::new(&settings(10, large_bytes + small_bytes));
        store.record(big);
        store.record(small);
        let mut candidate = evidence(Some("other"), &"c".repeat(100));
        candidate.route = Some(Arc::from("/".repeat(200)));
        let bytes = sized(
            "other",
            &candidate.request_id,
            candidate.route.as_deref(),
        );
        assert!(small_bytes < bytes && bytes < large_bytes);
        store.record(candidate);
        // The heaviest tenant has two records, but removing its oldest
        // would leave less than the new tenant requests. Reject conservatively.
        assert_eq!(store.retained(), (2, large_bytes + small_bytes));
        assert_eq!(held(&store, "donor"), 2);
        assert_eq!(held(&store, "other"), 0);
        check(&store);
    }

    #[test]
    fn byte_admission_reclaims_real_overshare_for_a_new_tenant() {
        let each = sized("old", &id("req-0"), None);
        let store = EvidenceStore::new(&settings(20, 4 * each));
        for n in 0..4 {
            store.record(evidence(Some("old"), &format!("req-{n}")));
        }
        store.record(evidence(Some("new"), "req-0"));
        assert_eq!(held(&store, "old"), 3);
        assert_eq!(held(&store, "new"), 1);
        assert_eq!(found(&store, "old", "req-0"), 0);
        assert_eq!(store.retained(), (4, 4 * each));
        check(&store);
    }

    #[test]
    fn exact_byte_limit_and_single_record_replacement_account_for_all_strings() {
        let mut first = evidence(Some("solo"), &"r".repeat(256));
        first.route = Some(Arc::from("/".repeat(MAX_ROUTE_BYTES)));
        let each = sized("solo", &first.request_id, first.route.as_deref());
        let store = EvidenceStore::new(&settings(10, each));
        let first_owner = first.diagnostic_id().clone();
        store.record(first);
        assert_eq!(store.retained(), (1, each));
        let mut next = evidence(Some("solo"), &"s".repeat(256));
        next.route = Some(Arc::from("/".repeat(MAX_ROUTE_BYTES)));
        let next_owner = next.diagnostic_id().clone();
        store.record(next.clone());
        assert!(store.find("solo", first_owner).records.is_empty());
        assert_eq!(store.find("solo", next_owner.clone()).records.len(), 1);
        assert_eq!(store.retained(), (1, each));
        next.tenant = Some(Arc::from("longer-tenant"));
        store.record(next);
        assert_eq!(store.find("solo", next_owner).records.len(), 1);
        assert_eq!(store.retained(), (1, each));
        let too_large = r#"ferrum_alloy_diagnostics_skipped_total{reason="too_large"}"#;
        assert_eq!(metric(&store, too_large), 1);
        check(&store);
    }

    #[test]
    fn reservation_commits_count_and_bytes_together_and_removes_empty_tenants() {
        let each = sized("old", &id("req-0"), None);
        let store = EvidenceStore::new(&settings(4, 4 * each));
        for n in 0..4 {
            store.record(evidence(Some("old"), &format!("req-{n}")));
        }
        let mut larger = evidence(Some("new"), "req-0");
        larger.route = Some(Arc::from("/".repeat(100)));
        store.record(larger);
        assert_eq!(held(&store, "new"), 1);
        assert_eq!(held(&store, "old"), 2);
        assert_eq!(found(&store, "old", "req-0"), 0);
        assert_eq!(found(&store, "old", "req-1"), 0);
        let count = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        let bytes = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        assert_eq!(metric(&store, count), 1);
        assert_eq!(metric(&store, bytes), 1);
        check(&store);
        let seqs: Vec<_> = store.ring().tenants["old"]
            .seqs
            .iter()
            .copied()
            .collect();
        for seq in seqs {
            assert!(store.ring().remove(seq));
        }
        assert!(!store.ring().tenants.contains_key("old"));
        store.record(evidence(Some("old"), "back"));
        assert_eq!(held(&store, "old"), 1);
        check(&store);
    }

    #[test]
    fn concurrent_reservations_preserve_sole_records_and_all_indexes() {
        let each = sized("t0", &id("short"), None);
        let store = Arc::new(EvidenceStore::new(&settings(32, 6 * each)));
        for n in 0..6 {
            store.record(evidence(Some(&format!("t{n}")), "short"));
        }
        std::thread::scope(|scope| {
            for n in 0..6 {
                let store = Arc::clone(&store);
                scope.spawn(move || {
                    for _ in 0..20 {
                        let mut larger = evidence(Some(&format!("t{n}")), "longer-id");
                        larger.route = Some(Arc::from("/longer/route"));
                        store.record(larger);
                    }
                });
            }
        });
        assert_eq!(store.retained(), (6, 6 * each));
        for n in 0..6 {
            assert_eq!(found(&store, &format!("t{n}"), "short"), 1);
        }
        let dropped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
        assert_eq!(metric(&store, dropped), 120);
        check(&store);
    }

    #[test]
    fn long_route_templates_are_dropped() {
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        let mut long = evidence(Some("acme"), "req-long");
        long.route = Some(Arc::from(format!("/{}", "x".repeat(MAX_ROUTE_BYTES))));
        store.record(long);
        let records = store.find("acme", id("req-long")).records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].route, None);
    }

    #[test]
    fn reports_round_trip_through_the_offline_parser() {
        let trace = TraceId::random();
        let mut first = attempt("acme", "req-7", trace);
        first.route = Some(Arc::from("/orders/{id}"));
        first.status = Some(503);
        first.time_to_headers = Some(Duration::from_millis(12));
        first.body_duration = Some(Duration::from_millis(3));
        first.duration = Duration::from_millis(15);
        first.outcome = BodyOutcome::Completed;
        first.trace_decision = TraceDecision::AcceptedRemote;
        // A second attempt of the request that never produced headers.
        let mut second = first.clone();
        second.span_id = SpanId::random();
        second.route = None;
        second.status = None;
        second.time_to_headers = None;
        second.body_duration = None;
        second.duration = Duration::ZERO;
        second.outcome = BodyOutcome::CancelledBeforeHeaders;
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(first);
        store.record(second);
        let found_now = store.find("acme", id("req-7"));
        let records = &found_now.records;
        let json = serde_json::to_vec(&report("orders", &found_now)).unwrap();
        let parsed = parse_offline(&json, &Limits::default()).unwrap();
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(parsed.claimed_verification, Verification::Unverified);
        let report = parsed.report;
        assert_eq!(report.collection.method, CollectionMethod::LiveExport);
        let notes = &report.collection.notes;
        let noted = notes
            .iter()
            .any(|note| note.starts_with("local diagnostic lookup id:"));
        assert!(noted, "{notes:?}");
        assert_eq!(report.subject.request_id.as_deref(), Some("req-7"));
        assert_eq!(report.subject.route.as_deref(), Some("/orders/{id}"));
        let (trace_id, expected) = (report.subject.trace_id.as_deref(), trace.to_hex());
        assert_eq!(trace_id, Some(expected.as_str()), "the attempts' trace");
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
        assert_eq!(response.attr("request_id_origin"), Some("trusted_peer"));

        let head = get(1, "time_to_headers");
        assert_eq!(head.availability, Availability::Unavailable);
        assert_eq!(head.value, None, "unknown is not zero");
        let body = get(1, "body");
        assert_eq!(body.availability, Availability::NotApplicable);
        let outcome = body.attr("body.outcome");
        assert_eq!(outcome, Some("cancelled_before_headers"));
        let response = get(1, "response");
        assert_eq!(response.attr("status"), None);

        // Ownership is described without adding fields to the wire schema.
        let report = super::report("orders", &found_now);
        let notes = &report.collection.notes;
        assert_eq!(notes.len(), 3, "{notes:?}");
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
