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
//! A denied caller, a malformed, unknown, evicted, or expired id, and
//! another tenant's id all get byte-identical `404` responses, and so does an
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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::rejection::PathRejection;
use axum::extract::{ConnectInfo, Path, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ferrum_alloy_diagnostics::catalog;
use ferrum_alloy_diagnostics::model::{
    Availability, Boundaries, ClockDomain, Collection, CollectionMethod, DiagnosticReport,
    Interval, Leg, Observation, ObservationKind, Producer, ProducerKind, Scope, SpanRef, Trust,
    Unit, Verification,
};
use ferrum_alloy_telemetry::evidence::{
    EvidenceSink, MAX_OPERATIONS_PER_REQUEST, OperationEvidence, RequestEvidence, RequestIdOrigin,
    valid_tenant,
};
use ferrum_alloy_telemetry::operation::OperationKind;
use ferrum_alloy_telemetry::trace_context::{SpanId, TraceId};
use ferrum_alloy_telemetry::{PeerInfo, RequestId, TraceDecision};
use futures_util::FutureExt as _;
use http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};

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

/// How long a record is retained before a tenant holding nothing may take
/// it over while every tenant holds at most one record. A stream of new
/// tenants cannot displace a sole record sooner.
pub const TAKEOVER_AGE: Duration = Duration::from_secs(1);

/// Bytes of an `Arc<str>` allocation besides its text: its two reference
/// counts.
const ARC_HEADER_BYTES: usize = 2 * size_of::<usize>();

/// Compact an alias's owner table once its allocation exceeds this many
/// capacity slots per live owner. Rebuild at half-load to avoid alternating
/// growth and compaction on single-owner insertions/removals.
const OWNER_CAPACITY_FACTOR: usize = 4;

/// Per-record allowance for owner buckets and control bytes, rounded up.
const OWNER_INDEX_BYTES: usize =
    ((size_of::<(RequestId, usize)>() + 1) * OWNER_CAPACITY_FACTOR * 8).div_ceil(7);

/// Estimated fixed cost of one record besides its strings: its entries in
/// the record map and in its tenant's sequence set, with B-tree nodes at
/// their minimum occupancy of 5 of 11 entries; an index entry and a tenant
/// entry with their hash table control bytes, at the tables' 7/8 maximum
/// load, including the alias; the owner table's bounded spare capacity;
/// the smallest sequence number list; the tenant's entries in both tenant
/// orderings; and the reference-count headers of its tenant and route.
/// It is an estimate, not a measurement: allocator overhead, control-group
/// padding and spare capacity of the other collections are not counted,
/// while an index, tenant, or ordering entry shared by records is counted
/// for each of them.
const RECORD_OVERHEAD_BYTES: usize = (2 * size_of::<u64>() + size_of::<Stored>()) * 11 / 5
    + (size_of::<(Key, Filed)>() + 1) * 8 / 7
    + (size_of::<(Arc<str>, Holding)>() + 1) * 8 / 7
    + (size_of::<(Alias, Aliased)>() + 1) * 8 / 7
    + OWNER_INDEX_BYTES
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
        crate::bearer::sole(&self.headers).ok().flatten()
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
    /// Middleware entry on the wall clock, which anchors the intervals.
    started_at: SystemTime,
    admission_wait: Option<Duration>,
    /// At most `MAX_OPERATIONS_PER_REQUEST`, enforced by the telemetry layer.
    operations: Vec<OperationEvidence>,
    operations_dropped: u32,
    /// Estimated retained bytes.
    bytes: usize,
    /// When the record was admitted, on the monotonic clock. It never
    /// decreases with the sequence number.
    inserted: Instant,
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
    /// Earlier attempts evicted or expired while later ones are retained.
    /// Eviction always takes a request's oldest attempt first.
    dropped: usize,
}

#[derive(Debug, Default)]
struct Aliased {
    /// Local owners and their retained record counts. A reused external
    /// alias is ambiguous, even when all owners share a remote trace.
    owners: HashMap<RequestId, usize>,
    /// Capacity observed when allocating/growing the table, retained until
    /// rebuilding it. Deletions can lower `HashMap::capacity()` through
    /// tombstones without releasing buckets, so that value alone cannot
    /// decide when to compact. This bounds the allocation, not just its
    /// current insertion capacity.
    owner_capacity: usize,
    records: usize,
}

impl Aliased {
    fn add(&mut self, owner: RequestId) {
        *self.owners.entry(owner).or_default() += 1;
        self.owner_capacity = self.owner_capacity.max(self.owners.capacity());
        self.records += 1;
    }

    fn remove(&mut self, owner: &RequestId) {
        self.records -= 1;
        let Some(count) = self.owners.get_mut(owner) else {
            return;
        };
        *count -= 1;
        if *count != 0 {
            return;
        }
        self.owners.remove(owner);
        let live = self.owners.len();
        if live == 0 || self.owner_capacity <= live.saturating_mul(OWNER_CAPACITY_FACTOR) {
            return;
        }
        // Move the keys instead of cloning their strings. Each rebuild
        // scans the old allocation only after geometric depletion; its
        // work is amortized over the intervening owner removals, including
        // when this runs under the global retention mutex.
        let mut owners = HashMap::with_capacity(live.saturating_mul(2));
        owners.extend(self.owners.drain());
        self.owners = owners;
        self.owner_capacity = self.owners.capacity();
    }
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

    /// Files `record` under its local owner as the newest record. Every
    /// index shares one tenant allocation, the one already retained.
    fn push(&mut self, mut key: Key, mut record: Stored) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if let Some((interned, _)) = self.tenants.get_key_value(&record.tenant) {
            record.tenant = Arc::clone(interned);
        }
        let tenant = Arc::clone(&record.tenant);
        key.0 = Arc::clone(&tenant);
        self.unrank(&tenant);
        let holding = self.tenants.entry(Arc::clone(&tenant)).or_default();
        holding.seqs.insert(seq);
        holding.bytes = holding.bytes.saturating_add(record.bytes);
        self.rank(&tenant);
        self.bytes = self.bytes.saturating_add(record.bytes);
        let alias = (tenant, record.origin, record.request_id.clone());
        let aliased = self.aliases.entry(alias).or_default();
        aliased.add(record.diagnostic_id.clone());
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
            } else {
                filed.dropped = filed.dropped.saturating_add(1);
            }
        }
        let alias = (tenant, record.origin, record.request_id);
        if let Some(aliased) = self.aliases.get_mut(&alias) {
            aliased.remove(&record.diagnostic_id);
            if aliased.records == 0 {
                self.aliases.remove(&alias);
            }
        }
        true
    }

    /// Removes the records retained for `max_age` or longer at `now`, oldest
    /// first, and returns how many. Admission times never decrease with the
    /// sequence number, so expired records are always the oldest.
    fn expire(&mut self, now: Instant, max_age: Option<Duration>) -> u64 {
        let Some(max_age) = max_age else {
            return 0;
        };
        let mut expired = 0;
        while let Some((&seq, oldest)) = self.records.first_key_value()
            && now.saturating_duration_since(oldest.inserted) >= max_age
        {
            self.remove(seq);
            expired += 1;
        }
        expired
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
        let Some(filed) = self.index.get(key) else {
            return Found::default();
        };
        Found {
            records: filed
                .seqs
                .iter()
                .filter_map(|seq| self.records.get(seq))
                .cloned()
                .collect(),
            dropped: filed.dropped,
            ..Found::default()
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
                found.alias = Some(origin);
            }
        }
        found
    }
}

type OriginalTenantRanking<'a> = Peekable<Rev<btree_set::Iter<'a, (usize, Arc<str>)>>>;

/// An immutable ordering plus updated ranks for only the reserved tenants.
/// Each original entry is visited at most once during an admission; no
/// full-store copy or repeated scan is needed for a multi-record reservation.
struct Ranking<'a> {
    original: OriginalTenantRanking<'a>,
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
    /// The latest admission time of a sole record that may be taken over,
    /// if any is old enough.
    settled: Option<Instant>,
}

impl<'a> Reservation<'a> {
    fn new(ring: &'a Ring, settled: Option<Instant>) -> Self {
        Self {
            ring,
            by_count: Ranking::new(&ring.by_count),
            by_bytes: Ranking::new(&ring.by_bytes),
            changed: HashMap::new(),
            cursors: HashMap::new(),
            evictions: BTreeMap::new(),
            count: ring.live(),
            bytes: ring.bytes,
            settled,
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

    /// Reserves the eviction of `seq`. Returns `false`, reserving nothing,
    /// when no such record exists.
    fn reserve(&mut self, seq: u64, reason: Evicted, advance: bool) -> bool {
        let Some(record) = self.ring.records.get(&seq) else {
            return false;
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
        true
    }

    /// Reclaim the heaviest tenant's oldest record only if its remaining
    /// holding is at least the candidate tenant's projected holding in the
    /// pressured dimension. Otherwise reserve our own oldest, or, once we
    /// hold nothing, take over a record (see `takeover`). If none is
    /// possible, refuse admission.
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
            .or_else(|| self.takeover(&record.tenant))
    }

    /// A tenant that holds nothing, counting reservations, takes a record
    /// when no donor can keep its share in the pressured dimension. While
    /// another tenant holds two or more records, it takes the oldest record
    /// of the tenant holding the most, which keeps at least the one record
    /// the candidate will hold. Once every tenant holds at most one record,
    /// it takes the oldest record, whose owner is the tenant whose newest
    /// record is oldest, but only once that record is `TAKEOVER_AGE` old.
    /// Without this, a store full of sole records, or of sole records and a
    /// tenant holding a few small ones, would refuse every newcomer until
    /// records expire. One admission takes over sole records only until the
    /// new record fits: at most its estimate divided by the smallest taken
    /// record's, rounded up.
    fn takeover(&mut self, tenant: &str) -> Option<u64> {
        if self.usage(tenant).0 != 0 {
            return None;
        }
        let heaviest = self.by_count.heaviest(&self.changed)?;
        if self.usage(&heaviest).0 > 1 {
            return self.oldest(&heaviest);
        }
        let (&seq, oldest) = self
            .ring
            .records
            .iter()
            .find(|(seq, _)| !self.evictions.contains_key(*seq))?;
        let settled = self.settled.is_some_and(|at| oldest.inserted <= at);
        settled.then_some(seq)
    }

    fn plan(
        mut self,
        record: &Stored,
        replacement: Option<u64>,
        max_records: usize,
        max_bytes: usize,
    ) -> Option<BTreeMap<u64, Evicted>> {
        if let Some(seq) = replacement
            && !self.reserve(seq, Evicted::RequestIdLimit, false)
        {
            return None;
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
            // A victim always exists; if the indexes ever disagreed, give
            // up rather than loop under the lock.
            if !self.reserve(victim, reason, true) {
                return None;
            }
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
    /// The correlation origin the lookup resolved through, when the id was
    /// not a retained local id.
    alias: Option<RequestIdOrigin>,
    /// Earlier attempts of the request that are no longer retained.
    dropped: usize,
}

/// Why an eviction happened.
#[derive(Debug, Clone, Copy)]
enum Evicted {
    Count,
    Bytes,
    RequestIdLimit,
    Age,
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
    max_age: Option<Duration>,
    ring: Mutex<Ring>,
    stored: AtomicU64,
    /// Times the store was emptied after a panic while it was locked.
    resets: AtomicU64,
    evicted: [AtomicU64; 4],
    skipped: [AtomicU64; 4],
    retrievals: [AtomicU64; 2],
    authorizer_failures: [AtomicU64; 2],
    /// This process's producer instance in reports, so readers compare its
    /// intervals only with each other (they share one clock).
    instance: String,
}

impl EvidenceStore {
    pub(crate) fn new(settings: &DiagnosticsSettings) -> Self {
        let max_age = Duration::from_millis(settings.max_age_ms);
        Self {
            max_records: settings.max_records.max(1),
            max_bytes: settings.max_bytes,
            max_age: (!max_age.is_zero()).then_some(max_age),
            ring: Mutex::new(Ring::default()),
            stored: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            evicted: Default::default(),
            skipped: Default::default(),
            retrievals: Default::default(),
            authorizer_failures: Default::default(),
            instance: format!("alloy-{}", RequestId::generate()),
        }
    }

    /// Locks the store. A panic while it was locked may have left its
    /// indexes half-updated, so a poisoned store starts again empty.
    fn ring(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(|poisoned| {
            let mut ring = poisoned.into_inner();
            *ring = Ring::default();
            self.ring.clear_poison();
            self.resets.fetch_add(1, Ordering::Relaxed);
            ring
        })
    }

    /// Locks the store after removing the records that are too old at
    /// `now`.
    fn fresh(&self, now: Instant) -> MutexGuard<'_, Ring> {
        let mut ring = self.ring();
        let expired = ring.expire(now, self.max_age);
        self.count_evicted(Evicted::Age, expired);
        ring
    }

    fn count_evicted(&self, reason: Evicted, count: u64) {
        if count > 0 {
            self.evicted[reason as usize].fetch_add(count, Ordering::Relaxed);
        }
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

    fn insert(&self, mut record: Stored) {
        if record.bytes > self.max_bytes {
            self.count_skipped(Skipped::TooLarge);
            return;
        }
        let key = (Arc::clone(&record.tenant), record.diagnostic_id.clone());
        let mut ring = self.ring();
        // Another thread may have read the clock later but locked first.
        // Keeping admission times in sequence order keeps expiry a prefix.
        if let Some((_, newest)) = ring.records.last_key_value() {
            record.inserted = record.inserted.max(newest.inserted);
        }
        let expired = ring.expire(record.inserted, self.max_age);
        self.count_evicted(Evicted::Age, expired);
        let replacement = match ring.admit(&key, &record) {
            Ok(replacement) => replacement,
            Err(()) => {
                drop(ring);
                self.count_skipped(Skipped::RequestIdConflict);
                return;
            }
        };
        let settled = record.inserted.checked_sub(TAKEOVER_AGE);
        let reservation = Reservation::new(&ring, settled);
        let plan = reservation.plan(&record, replacement, self.max_records, self.max_bytes);
        let Some(plan) = plan else {
            drop(ring);
            self.count_skipped(Skipped::FairShare);
            return;
        };
        let mut evicted = [0u64; 4];
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

    /// What `tenant` has under `request_id`. An expired record is removed
    /// first, so it is missing exactly like an evicted one.
    fn find(&self, tenant: &str, request_id: RequestId) -> Found {
        let tenant: Arc<str> = Arc::from(tenant);
        self.fresh(Instant::now()).find(&tenant, &request_id)
    }

    /// Records and estimated bytes currently retained.
    pub(crate) fn retained(&self) -> (usize, usize) {
        let ring = self.fresh(Instant::now());
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
            (
                "ferrum_alloy_diagnostics_resets_total",
                "counter",
                "Times the evidence store was emptied after a panic while it was locked.",
                self.resets.load(Ordering::Relaxed),
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
                    ("age", &self.evicted[3]),
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
    operations: usize,
) -> usize {
    // Operation names are `&'static str`: they live in the binary, so only
    // each operation's fixed-size entry is retained per record.
    RECORD_OVERHEAD_BYTES
        .saturating_add(tenant.len())
        .saturating_add(request_id.allocation_bytes().saturating_mul(2))
        .saturating_add(diagnostic_id.allocation_bytes().saturating_mul(3))
        .saturating_add(route.map_or(0, str::len))
        .saturating_add(operations.saturating_mul(size_of::<OperationEvidence>()))
}

impl EvidenceSink for EvidenceStore {
    fn record(&self, evidence: RequestEvidence) {
        let diagnostic_id = evidence.diagnostic_id().clone();
        let Some(tenant) = evidence.tenant else {
            self.count_skipped(Skipped::Untagged);
            return;
        };
        let route = evidence.route.filter(|r| r.len() <= MAX_ROUTE_BYTES);
        // The telemetry layer keeps at most this many; the bound is applied
        // again so the byte estimate holds whatever sink fed the store.
        let mut operations = evidence.operations;
        let excess = operations.len().saturating_sub(MAX_OPERATIONS_PER_REQUEST);
        operations.truncate(MAX_OPERATIONS_PER_REQUEST);
        operations.shrink_to_fit();
        let operations_dropped = evidence
            .operations_dropped
            .saturating_add(u32::try_from(excess).unwrap_or(u32::MAX));
        let bytes = estimate(
            &tenant,
            &evidence.request_id,
            &diagnostic_id,
            route.as_deref(),
            operations.len(),
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
            started_at: evidence.started_at,
            admission_wait: evidence.admission_wait,
            operations,
            operations_dropped,
            bytes,
            inserted: Instant::now(),
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
    Some(PeerInfo::new(*addr))
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
        let report = report(&self.service, &self.store.instance, &found);
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
fn report(service: &str, instance: &str, found: &Found) -> DiagnosticReport {
    let records = &found.records;
    let mut notes = vec![
        "assembled by the service from its own request telemetry; a reader cannot authenticate the producer".to_owned(),
        format!(
            "operations are those explicitly instrumented on the request's own task, at most {MAX_OPERATIONS_PER_REQUEST} per request; work moved to other tasks is not attributed"
        ),
    ];
    let operations_dropped: u64 = records
        .iter()
        .map(|record| u64::from(record.operations_dropped))
        .sum();
    if operations_dropped > 0 {
        notes.push(format!(
            "{operations_dropped} operation(s) beyond the per-request limit are not retained"
        ));
    }
    if let Some(first) = records.first() {
        notes.push(format!(
            "local diagnostic lookup id: {}; remote correlation ids and traces do not establish request ownership",
            first.diagnostic_id
        ));
    }
    if let Some(origin) = found.alias {
        let origin = origin.as_str();
        notes.push(format!(
            "resolved as a correlation alias (request_id_origin {origin}); no retained request has the requested id as its local diagnostic id, and a request that had it may have been evicted or expired"
        ));
    }
    let shadowed = found.shadowed;
    if shadowed > 0 {
        notes.push(format!(
            "{shadowed} record(s) from less preferred correlation origins under this alias are not included"
        ));
    }
    let dropped = found.dropped;
    if dropped > 0 {
        notes.push(format!(
            "{dropped} earlier attempt(s) of this request are no longer retained"
        ));
    }
    let mut clock = ReportClock::new(records);
    let mut observed = Vec::new();
    for record in records {
        observations(service, instance, record, &mut clock, &mut observed);
    }
    let omitted = clock.omitted;
    if omitted > 0 {
        let hours = clock.range / 3_600_000_000_000;
        notes.push(format!(
            "{omitted} interval(s) ending more than {hours} h after the earliest request start are omitted; their durations are kept"
        ));
    }
    let mut collection = Collection::new(
        Producer::new(ProducerKind::Alloy, "ferrum-alloy"),
        CollectionMethod::LiveExport,
        Verification::Unverified,
    );
    collection.notes = notes;
    let mut report = DiagnosticReport::new(collection);
    let first = records.first();
    report.subject.request_id = first.map(|record| record.request_id.as_str().to_owned());
    report.subject.service = Some(service.to_owned());
    report.subject.trace_id = first.map(|record| record.trace_id.to_hex());
    report.subject.route = records
        .iter()
        .find_map(|record| record.route.as_deref().map(str::to_owned));
    report.observations = observed;
    report
}

/// Places the intervals of one report on the wall clock within the range
/// the offline reader accepts, so a late streaming operation cannot make
/// the whole report unreadable.
struct ReportClock {
    /// Unix nanoseconds of the earliest record's middleware entry.
    earliest: Option<u64>,
    /// Widest accepted range from `earliest`, in nanoseconds.
    range: u64,
    /// Intervals left out because they end beyond the range.
    omitted: usize,
}

impl ReportClock {
    fn new(records: &[Stored]) -> Self {
        Self {
            earliest: records
                .iter()
                .filter_map(|record| unix_nanos(record.started_at, Duration::ZERO))
                .filter(|&start| start > 0)
                .min(),
            range: ferrum_alloy_diagnostics::parse::Limits::default().max_time_range_nanos,
            omitted: 0,
        }
    }

    /// The wall-clock interval from `offset` to `offset + length` after the
    /// middleware entry of `record`. Every interval of one record shares that
    /// anchor and this process's clock (the producer instance says so). An
    /// interval ending beyond the accepted range is omitted and counted.
    fn interval(
        &mut self,
        record: &Stored,
        offset: Duration,
        length: Duration,
    ) -> Option<Interval> {
        let start = unix_nanos(record.started_at, offset)?;
        let end = unix_nanos(record.started_at, offset.checked_add(length)?)?;
        let earliest = self.earliest?;
        if start == 0 || start < earliest {
            return None;
        }
        if end.saturating_sub(earliest) > self.range {
            self.omitted = self.omitted.saturating_add(1);
            return None;
        }
        Some(Interval::new(start, end))
    }
}

/// Unix nanoseconds of `offset` after `start`, when representable.
fn unix_nanos(start: SystemTime, offset: Duration) -> Option<u64> {
    let since = start
        .checked_add(offset)?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    u64::try_from(since.as_nanos()).ok()
}

/// Alloy's telemetry as the producer, on this process's `instance`.
fn producer(instance: &str) -> Producer {
    let mut producer = Producer::new(ProducerKind::Alloy, "ferrum-alloy-telemetry");
    producer.instance = Some(instance.to_owned());
    producer
}

/// An observation of `record` with everything but its value.
fn base(
    service: &str,
    instance: &str,
    record: &Stored,
    suffix: &str,
    name: &str,
    kind: ObservationKind,
) -> Observation {
    let span_id = record.span_id.to_hex();
    let mut scope = Scope::new(Leg::Service);
    scope.service = Some(service.to_owned());
    let mut observation = Observation::new(
        format!("alloy:{span_id}:{suffix}"),
        producer(instance),
        kind,
        name,
        Availability::Unknown,
        scope,
        Trust::Unverified,
    );
    observation.boundaries =
        catalog::entry(name).map(|entry| Boundaries::new(entry.start, entry.end));
    observation.span = Some(SpanRef::new(record.trace_id.to_hex(), span_id.clone()));
    observation.evidence_ref = Some(format!("alloy:request/{span_id}"));
    observation
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

fn observations(
    service: &str,
    instance: &str,
    record: &Stored,
    clock: &mut ReportClock,
    out: &mut Vec<Observation>,
) {
    use ObservationKind::{Event, Measurement};
    let head = base(
        service,
        instance,
        record,
        "time_to_headers",
        catalog::ALLOY_TIME_TO_HEADERS,
        Measurement,
    );
    // Headers that were never produced have no time: unavailable, not zero.
    let mut head = duration(head, record.time_to_headers, Availability::Unavailable);
    // The header phase on this process's clock, so readers can place the
    // request's operations inside or after it.
    head.interval = record
        .time_to_headers
        .and_then(|time_to_headers| clock.interval(record, Duration::ZERO, time_to_headers));
    out.push(head);
    let body = base(
        service,
        instance,
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
        instance,
        record,
        "duration",
        catalog::ALLOY_SERVER_DURATION,
        Measurement,
    );
    let whole = duration(whole, Some(record.duration), Availability::Unknown);
    out.push(whole);
    let mut response = base(
        service,
        instance,
        record,
        "response",
        catalog::ALLOY_RESPONSE,
        Event,
    );
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
    if let Some(wait) = record.admission_wait {
        let admission = base(
            service,
            instance,
            record,
            "admission",
            catalog::ALLOY_ADMISSION_WAIT,
            Measurement,
        );
        out.push(duration(admission, Some(wait), Availability::Unknown));
    }
    for operation in &record.operations {
        operation_observations(service, instance, record, operation, clock, out);
    }
}

/// `alloy.operation.duration` for `operation`, and `alloy.db.pool_wait` when
/// it recorded one, linked to the operation's span under the request's trace.
fn operation_observations(
    service: &str,
    instance: &str,
    record: &Stored,
    operation: &OperationEvidence,
    clock: &mut ReportClock,
    out: &mut Vec<Observation>,
) {
    let span_id = operation.span_id.to_hex();
    // As the OTLP importer reads operation spans: database and outbound
    // calls (CLIENT spans) are on the dependency leg.
    let leg = match operation.kind {
        OperationKind::Internal => Leg::Service,
        _ => Leg::ServiceToDependency,
    };
    let mut span = SpanRef::new(record.trace_id.to_hex(), span_id.clone());
    span.parent_span_id = Some(operation.parent_span_id.to_hex());
    // The per-request cap keeps operations in completion order, so a parent
    // operation can be dropped while its children are kept. The server span
    // is then the nearest ancestor the report contains, so rules can still
    // walk from the operation to the request.
    let parent = operation.parent_span_id;
    let parent_retained = parent == record.span_id
        || record
            .operations
            .iter()
            .any(|other| other.span_id == parent);
    let lineage = (!parent_retained).then(|| record.span_id.to_hex());
    let observation = |suffix: &str, name: &str, value: Duration| {
        let mut scope = Scope::new(leg.clone());
        scope.service = Some(service.to_owned());
        let mut observation = Observation::new(
            format!("alloy:{span_id}:{suffix}"),
            producer(instance),
            ObservationKind::Measurement,
            name,
            Availability::Measured,
            scope,
            Trust::Unverified,
        );
        observation.value = Some(value.as_secs_f64() * 1_000.0);
        observation.unit = Some(Unit::Milliseconds);
        observation.clock = Some(ClockDomain::MonotonicLocal);
        observation.boundaries =
            catalog::entry(name).map(|entry| Boundaries::new(entry.start, entry.end));
        observation.span = Some(span.clone());
        observation.evidence_ref = Some(format!("alloy:operation/{span_id}"));
        if let Some(ancestor) = &lineage {
            observation
                .attributes
                .insert(catalog::LINEAGE_PARENT_SPAN_ID.to_owned(), ancestor.clone());
        }
        observation
    };
    let mut measured = observation(
        "operation",
        catalog::ALLOY_OPERATION_DURATION,
        operation.duration,
    );
    measured.interval = clock.interval(record, operation.started, operation.duration);
    let attributes = &mut measured.attributes;
    // Names are static but unbounded: long ones become the OTLP importer's
    // digest, so the report stays within the reader's string limit.
    attributes.insert(
        "operation.name".to_owned(),
        ferrum_alloy_diagnostics::otlp::bounded_text(operation.name),
    );

    attributes.insert(
        "operation.kind".to_owned(),
        operation.kind.label().to_owned(),
    );
    attributes.insert(
        "operation.outcome".to_owned(),
        operation.outcome.as_str().to_owned(),
    );
    out.push(measured);
    if let Some(wait) = operation.pool_wait {
        let mut pool = observation("pool_wait", catalog::ALLOY_DB_POOL_WAIT, wait);
        // The wait ended when it was recorded, so it spans `wait` before.
        pool.interval = operation
            .pool_wait_ended
            .and_then(|ended| clock.interval(record, ended.saturating_sub(wait), wait));
        out.push(pool);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use ferrum_alloy_diagnostics::model::Confidence;
    use ferrum_alloy_diagnostics::parse::{Limits, parse_offline};
    use ferrum_alloy_diagnostics::rules::{Thresholds, analyze};
    use ferrum_alloy_telemetry::BodyOutcome;
    use ferrum_alloy_telemetry::evidence::OperationOutcome;
    use http::header::AUTHORIZATION;

    use super::*;

    fn settings(max_records: usize, max_bytes: usize) -> DiagnosticsSettings {
        DiagnosticsSettings {
            max_records,
            max_bytes,
            ..DiagnosticsSettings::default()
        }
    }

    /// Moves every retained record's admission `age` into the past.
    fn backdate(store: &EvidenceStore, age: Duration) {
        let mut ring = store.ring();
        for record in ring.records.values_mut() {
            record.inserted = record.inserted.checked_sub(age).unwrap();
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
        let allocated: usize = ring
            .aliases
            .values()
            .map(|alias| alias.owner_capacity)
            .sum();
        assert!(allocated <= OWNER_CAPACITY_FACTOR * ring.live());
        let owner_bytes = (allocated * (size_of::<(RequestId, usize)>() + 1) * 8).div_ceil(7);
        assert!(owner_bytes <= OWNER_INDEX_BYTES * ring.live());
        for (alias, aliased) in &ring.aliases {
            assert_eq!(aliased.records, aliased.owners.values().sum::<usize>());
            assert!(aliased.owners.capacity() <= aliased.owner_capacity);
            assert!(aliased.owner_capacity <= OWNER_CAPACITY_FACTOR * aliased.owners.len());
            for (owner, count) in &aliased.owners {
                let key = (Arc::clone(&alias.0), owner.clone());
                assert_eq!(*count, ring.index[&key].seqs.len());
            }
        }
        let holdings: usize = ring.tenants.values().map(|held| held.seqs.len()).sum();
        assert_eq!(holdings, ring.live());
        assert_eq!(ring.by_count.len(), ring.tenants.len());
        assert_eq!(ring.by_bytes.len(), ring.tenants.len());
        // Every index shares the tenant's one retained allocation.
        let interned = |tenant: &Arc<str>| {
            let (key, _) = ring.tenants.get_key_value(tenant).unwrap();
            assert!(Arc::ptr_eq(key, tenant), "{tenant} is not interned");
        };
        for key in ring.index.keys() {
            interned(&key.0);
        }
        for alias in ring.aliases.keys() {
            interned(&alias.0);
        }
        for (_, tenant) in ring.by_count.iter().chain(&ring.by_bytes) {
            interned(tenant);
        }
        let mut inserted = None;
        for (seq, record) in &ring.records {
            interned(&record.tenant);
            assert!(
                inserted <= Some(record.inserted),
                "admission times are ordered"
            );
            inserted = Some(record.inserted);
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
            assert!(
                ring.aliases[&alias]
                    .owners
                    .contains_key(&record.diagnostic_id)
            );
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
        estimate(tenant, request_id, &diagnostic_id, route, 0)
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
        let found_now = store.find("acme", owner.clone());
        assert_eq!(found_now.dropped, 24);
        let records = found_now.records;
        assert_eq!(records.len(), MAX_RECORDS_PER_REQUEST_ID);
        assert_eq!(records[0].status, Some(524));
        assert_eq!(records.last().unwrap().status, Some(539));
        assert_eq!(
            found(&store, "acme", "shared-id"),
            MAX_RECORDS_PER_REQUEST_ID
        );
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
        assert_eq!(
            store.find("acme", first_owner).records[0].span_id,
            first_span
        );
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
        let quiet = report("test", "alloy-test", &store.find("else", id("quiet")));
        let mut larger = first;
        larger.route = Some(Arc::from("/".repeat(100)));
        larger.span_id = SpanId::random();
        let latest_span = larger.span_id;
        store.record(larger);
        let found = store.find("acme", owner);
        assert_eq!(found.records.len(), MAX_RECORDS_PER_REQUEST_ID - 1);
        assert_eq!(found.records[0].status, Some(2));
        assert_eq!(found.records.last().unwrap().span_id, latest_span);
        assert_eq!(
            report("test", "alloy-test", &store.find("else", id("quiet"))),
            quiet
        );
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
    fn alias_owner_compaction_is_geometric_and_preserves_attempt_counts() {
        let mut alias = Aliased::default();
        // Exercise variable-size index keys directly. Frontend ownership
        // still comes exclusively from fresh locally generated UUIDs.
        let owners: Vec<_> = (0..512)
            .map(|n| {
                let padding = [0, 30, 122, 250][n % 4];
                id(&format!("o-{n:03}-{}", "x".repeat(padding)))
            })
            .collect();
        for owner in &owners {
            alias.add(owner.clone());
            assert!(alias.owner_capacity <= OWNER_CAPACITY_FACTOR * alias.owners.len());
        }
        for _ in 1..MAX_RECORDS_PER_REQUEST_ID {
            alias.add(owners.last().unwrap().clone());
        }
        let initial_capacity = alias.owner_capacity;
        let mut scanned_capacity = 0;
        let mut rebuilds = 0;
        for owner in &owners[..owners.len() - 1] {
            let before = alias.owner_capacity;
            alias.remove(owner);
            if alias.owner_capacity < before {
                scanned_capacity += before;
                rebuilds += 1;
                assert!(alias.owner_capacity <= before / 2);
            }
            assert!(alias.owners.capacity() <= alias.owner_capacity);
            assert!(alias.owner_capacity <= OWNER_CAPACITY_FACTOR * alias.owners.len());
            assert_eq!(
                alias.owners[owners.last().unwrap()],
                MAX_RECORDS_PER_REQUEST_ID
            );
        }
        assert!(rebuilds > 1, "exercise multiple allocation sizes");
        assert!(scanned_capacity < 2 * initial_capacity);
        assert_eq!(alias.records, MAX_RECORDS_PER_REQUEST_ID);
        let capacity = alias.owner_capacity;
        for _ in 1..MAX_RECORDS_PER_REQUEST_ID {
            alias.remove(owners.last().unwrap());
            assert_eq!(alias.owner_capacity, capacity, "owner remains live");
        }
        assert_eq!(alias.records, 1);
        assert_eq!(alias.owners.len(), 1);
    }

    /// Replay the former no-compaction index alongside sink admission. All
    /// requests here have fresh local ownership, accepted caller aliases,
    /// and one attempt, so count pressure removes exactly the oldest owner.
    fn churn_alias(
        store: &EvidenceStore,
        alias: &RequestId,
        legacy: &mut HashMap<RequestId, HashMap<RequestId, usize>>,
    ) {
        let oldest = {
            let ring = store.ring();
            (ring.live() == store.max_records).then(|| {
                let record = ring.records.first_key_value().unwrap().1;
                (record.request_id.clone(), record.diagnostic_id.clone())
            })
        };
        if let Some((alias, owner)) = oldest {
            let owners = legacy.get_mut(&alias).unwrap();
            assert_eq!(owners.remove(&owner), Some(1));
            if owners.is_empty() {
                legacy.remove(&alias);
            }
        }
        let mut next = evidence(Some("churn"), alias.as_str());
        next.request_id_origin = RequestIdOrigin::UntrustedCaller;
        legacy
            .entry(alias.clone())
            .or_default()
            .insert(next.diagnostic_id().clone(), 1);
        store.record(next);
        check(store);
        let ring = store.ring();
        for (alias, aliased) in &ring.aliases {
            assert_eq!(aliased.owners, legacy[&alias.2]);
        }
    }

    #[test]
    fn grow_drain_and_pair_refresh_bound_aggregate_allocated_alias_capacity() {
        for max_records in [64usize, 128] {
            let store = EvidenceStore::new(&settings(max_records, 1024 * 1024));
            let mut legacy = HashMap::new();
            for group in 0..max_records / 2 {
                // After each drain, every older alias has a pair of owners.
                // Refresh those pairs in insertion order so every large old
                // allocation survives while the next alias grows and drains.
                for _ in 0..group.saturating_sub(1) * 2 {
                    let alias = store
                        .ring()
                        .records
                        .first_key_value()
                        .unwrap()
                        .1
                        .request_id
                        .clone();
                    churn_alias(&store, &alias, &mut legacy);
                }
                let padding = [0, 32, 128, 252][group % 4];
                let alias = id(&format!("a{group:03}{}", "x".repeat(padding)));
                for _ in 0..max_records - 2 * group {
                    churn_alias(&store, &alias, &mut legacy);
                }
                assert_eq!(store.retained().0, max_records);
                assert_eq!(store.ring().aliases.len(), group + 1);
            }
            let allocated: usize = store
                .ring()
                .aliases
                .values()
                .map(|alias| alias.owner_capacity)
                .sum();
            assert!(allocated <= OWNER_CAPACITY_FACTOR * max_records);
            // This is measured table capacity, not owner/record counts or
            // the byte estimate. Deletion may understate the legacy bucket
            // allocation through tombstones, so even this lower bound must
            // exceed the fixed index's linear allocation allowance.
            let legacy_capacity: usize = legacy.values().map(HashMap::capacity).sum();
            assert!(legacy_capacity > OWNER_CAPACITY_FACTOR * max_records);
            for alias in legacy.keys() {
                assert_eq!(found(&store, "churn", alias.as_str()), 0, "ambiguous");
            }
            let limited = r#"ferrum_alloy_diagnostics_evicted_total{reason="request_id_limit"}"#;
            assert_eq!(metric(&store, limited), 0, "all owners were fresh");
            let seqs: Vec<_> = store.ring().records.keys().copied().collect();
            for seq in seqs {
                assert!(store.ring().remove(seq));
                check(&store);
            }
            assert_eq!(store.retained(), (0, 0));
            assert!(store.ring().aliases.is_empty());
            assert!(store.ring().index.is_empty());
            assert!(store.ring().tenants.is_empty());
        }
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
        assert!(
            big_bytes.saturating_sub(big) < small_bytes + small,
            "{big_bytes}"
        );
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
    fn a_new_tenant_takes_the_sole_record_of_the_least_recently_active_tenant() {
        let store = EvidenceStore::new(&settings(2, 1024 * 1024));
        store.record(evidence(Some("a"), "a-1"));
        store.record(evidence(Some("b"), "b-1"));
        backdate(&store, TAKEOVER_AGE);
        store.record(evidence(Some("c"), "c-1"));
        // Every tenant held one record, and "a" was inactive longest.
        assert_eq!(found(&store, "a", "a-1"), 0);
        assert_eq!(found(&store, "b", "b-1"), 1);
        assert_eq!(found(&store, "c", "c-1"), 1);

        // A present tenant at its share still replaces only its own record.
        store.record(evidence(Some("b"), "b-2"));
        assert_eq!(found(&store, "b", "b-1"), 0);
        assert_eq!(found(&store, "b", "b-2"), 1);
        assert_eq!(found(&store, "c", "c-1"), 1);

        // Now "c" is the least recently active tenant.
        backdate(&store, TAKEOVER_AGE);
        store.record(evidence(Some("d"), "d-1"));
        assert_eq!(found(&store, "c", "c-1"), 0);
        assert_eq!(found(&store, "b", "b-2"), 1);
        assert_eq!(found(&store, "d", "d-1"), 1);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, evicted), 3);
        let skipped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
        assert_eq!(metric(&store, skipped), 0);
        check(&store);
    }

    #[test]
    fn admission_never_freezes_with_more_tenants_than_room() {
        // About 800 default-sized records fill the default byte budget. Small
        // budgets reach the same state, every retained tenant holding one
        // record, by count and by bytes, with expiry disabled.
        let each = sized("t000", &id("req"), None);
        for (max_records, max_bytes) in [(16, 1024 * 1024), (1_000, 16 * each)] {
            let mut config = settings(max_records, max_bytes);
            config.max_age_ms = 0;
            let store = EvidenceStore::new(&config);
            for n in 0..800 {
                let tenant = format!("t{n:03}");
                backdate(&store, TAKEOVER_AGE);
                store.record(evidence(Some(&tenant), "req"));
                assert_eq!(found(&store, &tenant, "req"), 1, "{tenant}");
            }
            assert_eq!(store.retained().0, 16);
            for n in 784..800 {
                assert_eq!(found(&store, &format!("t{n:03}"), "req"), 1, "{n}");
            }
            // A tenant whose record was taken over comes back.
            backdate(&store, TAKEOVER_AGE);
            store.record(evidence(Some("t000"), "back"));
            assert_eq!(found(&store, "t000", "back"), 1);
            assert_eq!(found(&store, "t784", "req"), 0);
            let skipped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
            assert_eq!(metric(&store, skipped), 0);
            check(&store);
        }
    }

    #[test]
    fn admission_never_freezes_under_byte_pressure_with_mixed_record_sizes() {
        // A quiet tenant holds two small records and every other tenant one
        // large record, with expiry disabled. No tenant can give up bytes
        // and keep a newcomer's holding, which once refused every newcomer
        // for as long as the quiet tenant stayed idle.
        let small = sized("x", &id("r0"), None);
        let long = "l".repeat(256);
        let route = "/".repeat(MAX_ROUTE_BYTES);
        let large = sized("t0", &id(&long), Some(&route));
        let typical = "/".repeat(100);
        let each = sized("n0", &id("req"), Some(&typical));
        assert!(small < each && each < large, "{small} {each} {large}");
        let mut config = settings(1_000, small + 4 * large + each);
        config.max_age_ms = 0;
        let store = EvidenceStore::new(&config);
        for n in 0..2 {
            store.record(evidence(Some("x"), &format!("r{n}")));
        }
        for n in 0..4 {
            let mut record = evidence(Some(&format!("t{n}")), &long);
            record.route = Some(Arc::from(route.as_str()));
            store.record(record);
        }
        let newcomer = |n: usize| {
            let mut record = evidence(Some(&format!("n{n}")), "req");
            record.route = Some(Arc::from(typical.as_str()));
            record
        };

        // The quiet tenant gives up one of its two records at once.
        store.record(newcomer(0));
        assert_eq!(found(&store, "n0", "req"), 1);
        assert_eq!(found(&store, "x", "r0"), 0);
        assert_eq!(found(&store, "x", "r1"), 1);
        check(&store);

        // Then every tenant holds one record, and each newcomer takes over
        // the least recently active tenants' records once they are old
        // enough.
        for n in 1..4 {
            backdate(&store, TAKEOVER_AGE);
            store.record(newcomer(n));
            assert_eq!(found(&store, &format!("n{n}"), "req"), 1, "{n}");
            check(&store);
        }
        assert_eq!(found(&store, "x", "r1"), 0);
        assert_eq!(found(&store, "n0", "req"), 1);
        let skipped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
        assert_eq!(metric(&store, skipped), 0);
    }

    #[test]
    fn a_present_tenant_with_a_larger_record_is_admitted_like_a_new_tenant() {
        let each = sized("t0", &id("short"), None);
        let mut config = settings(32, 6 * each);
        config.max_age_ms = 0;
        let store = EvidenceStore::new(&config);
        for n in 0..6 {
            store.record(evidence(Some(&format!("t{n}")), "short"));
        }
        let mut larger = evidence(Some("t5"), "longer-id");
        larger.route = Some(Arc::from("/longer/route"));

        // Room needs another tenant's sole record, younger than the
        // takeover age: nothing is lost.
        store.record(larger.clone());
        assert_eq!(found(&store, "t5", "longer-id"), 0);
        assert_eq!(store.retained(), (6, 6 * each));
        let skipped = r#"ferrum_alloy_diagnostics_skipped_total{reason="fair_share"}"#;
        assert_eq!(metric(&store, skipped), 1);
        check(&store);

        // Once it is old enough, the tenant gives up its own record and
        // takes over the least recently active tenant's.
        backdate(&store, TAKEOVER_AGE);
        store.record(larger);
        assert_eq!(found(&store, "t5", "longer-id"), 1);
        assert_eq!(found(&store, "t5", "short"), 0);
        assert_eq!(found(&store, "t0", "short"), 0);
        for n in 1..5 {
            assert_eq!(found(&store, &format!("t{n}"), "short"), 1, "{n}");
        }
        assert_eq!(metric(&store, skipped), 1);
        check(&store);
    }

    #[test]
    fn one_admission_takes_over_at_most_six_sole_records() {
        // The smallest record a tenant can hold, and the largest: longest
        // tenant, accepted request id and route, and every operation.
        let small = sized("a", &id("r"), None);
        let tenant = "n".repeat(128);
        let mut large = evidence(Some(&tenant), &"l".repeat(256));
        large.route = Some(Arc::from("/".repeat(MAX_ROUTE_BYTES)));
        large.operations = (0..MAX_OPERATIONS_PER_REQUEST)
            .map(|_| {
                OperationEvidence::new(
                    "orders.item",
                    OperationKind::Db,
                    OperationOutcome::Completed,
                    SpanId::random(),
                    large.span_id,
                )
            })
            .collect();
        let large_bytes = estimate(
            &tenant,
            &large.request_id,
            large.diagnostic_id(),
            large.route.as_deref(),
            MAX_OPERATIONS_PER_REQUEST,
        );
        let taken = large_bytes.div_ceil(small);
        assert!((2..=6).contains(&taken), "{large_bytes} / {small}");
        let owners = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let mut config = settings(1_000, owners.len() * small);
        config.max_age_ms = 0;
        let store = EvidenceStore::new(&config);
        for owner in owners {
            store.record(evidence(Some(owner), "r"));
        }
        backdate(&store, TAKEOVER_AGE);
        store.record(large);
        assert_eq!(held(&store, &tenant), 1);
        // The least recently active tenants lost theirs, and no more.
        for (n, owner) in owners.into_iter().enumerate() {
            assert_eq!(held(&store, owner), usize::from(n >= taken), "{owner}");
        }
        let bytes = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        assert_eq!(metric(&store, bytes), u64::try_from(taken).unwrap());
        check(&store);
    }

    #[test]
    fn records_expire_by_age_before_admission_and_lookup() {
        let mut config = settings(10, 1024 * 1024);
        config.max_age_ms = 1_000;
        let store = EvidenceStore::new(&config);
        store.record(evidence(Some("acme"), "old-0"));
        store.record(evidence(Some("other"), "old-1"));
        backdate(&store, Duration::from_millis(1_500));
        store.record(evidence(Some("acme"), "young"));
        let aged = r#"ferrum_alloy_diagnostics_evicted_total{reason="age"}"#;
        assert_eq!(metric(&store, aged), 2, "expired before admission");
        assert_eq!(found(&store, "acme", "old-0"), 0);
        assert_eq!(found(&store, "other", "old-1"), 0);
        assert_eq!(found(&store, "acme", "young"), 1);
        assert!(!store.ring().tenants.contains_key("other"));
        check(&store);

        // A lookup expires a record that no admission has removed yet.
        backdate(&store, Duration::from_millis(500));
        assert_eq!(found(&store, "acme", "young"), 1);
        backdate(&store, Duration::from_millis(600));
        assert_eq!(found(&store, "acme", "young"), 0);
        assert_eq!(metric(&store, aged), 3);
        assert_eq!(store.retained(), (0, 0));
        let count = r#"ferrum_alloy_diagnostics_evicted_total{reason="count"}"#;
        assert_eq!(metric(&store, count), 0);
        check(&store);
    }

    #[tokio::test]
    async fn an_expired_id_gets_the_same_404_as_an_unknown_id() {
        let mut config = settings(10, 1024 * 1024);
        config.max_age_ms = 1_000;
        let store = Arc::new(EvidenceStore::new(&config));
        let authorizer = |_: DiagnosticsRequest| async { DiagnosticsAccess::tenant("acme") };
        let retrieval = Retrieval {
            authorizer: Arc::new(authorizer),
            store: Arc::clone(&store),
            service: "test".to_owned(),
        };
        let answer = |request_id: &str| {
            let path = Ok(Path(request_id.to_owned()));
            let request = Request::new(axum::body::Body::empty());
            let retrieval = retrieval.clone();
            async move {
                let response = retrieval.retrieve(path, request).await;
                let (parts, body) = response.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                (parts.status, parts.headers, body)
            }
        };
        store.record(evidence(Some("acme"), "expiring"));
        assert_eq!(answer("expiring").await.0, StatusCode::OK);
        backdate(&store, Duration::from_millis(1_000));
        let expired = answer("expiring").await;
        assert_eq!(expired.0, StatusCode::NOT_FOUND);
        assert_eq!(expired, answer("unknown").await);
        let aged = r#"ferrum_alloy_diagnostics_evicted_total{reason="age"}"#;
        assert_eq!(metric(&store, aged), 1);
    }

    #[test]
    fn reports_disclose_alias_resolution_and_dropped_attempts() {
        let store = EvidenceStore::new(&settings(3, 1024 * 1024));
        let first = attempt("acme", "edge-1", TraceId::random());
        let owner = first.diagnostic_id().clone();
        for _ in 0..(MAX_RECORDS_PER_REQUEST_ID + 2) {
            let mut next = first.clone();
            next.span_id = SpanId::random();
            store.record(next);
        }
        let by_owner = store.find("acme", owner.clone());
        assert_eq!(by_owner.records.len(), 3);
        assert_eq!(by_owner.alias, None);
        let notes = report("test", "alloy-test", &by_owner).collection.notes;
        let dropped = format!(
            "{} earlier attempt(s) of this request are no longer retained",
            MAX_RECORDS_PER_REQUEST_ID - 1
        );
        assert!(notes.contains(&dropped), "{notes:?}");
        let resolved = "resolved as a correlation alias";
        assert!(!notes.iter().any(|note| note.starts_with(resolved)));

        let by_alias = store.find("acme", id("edge-1"));
        assert_eq!(by_alias.alias, Some(RequestIdOrigin::TrustedPeer));
        let notes = report("test", "alloy-test", &by_alias).collection.notes;
        let resolved = "resolved as a correlation alias (request_id_origin trusted_peer)";
        let noted = notes.iter().any(|note| note.starts_with(resolved));
        assert!(noted, "{notes:?}");

        // Once the local owner is evicted, its id can resolve as another
        // request's alias. The report says so.
        let mut reuse = attempt("acme", owner.as_str(), TraceId::random());
        reuse.request_id_origin = RequestIdOrigin::UntrustedCaller;
        let reuse_owner = reuse.diagnostic_id().clone();
        store.record(reuse);
        for n in 0..2 {
            store.record(evidence(Some("acme"), &format!("later-{n}")));
        }
        let found_now = store.find("acme", owner);
        assert_eq!(found_now.records.len(), 1);
        assert_eq!(found_now.records[0].diagnostic_id, reuse_owner);
        assert_eq!(found_now.alias, Some(RequestIdOrigin::UntrustedCaller));
        let notes = report("test", "alloy-test", &found_now).collection.notes;
        let resolved = "resolved as a correlation alias (request_id_origin untrusted_caller)";
        let noted = notes.iter().any(|note| note.starts_with(resolved));
        assert!(noted, "{notes:?}");
        check(&store);
    }

    #[test]
    fn a_poisoned_store_starts_again_empty() {
        let store = EvidenceStore::new(&settings(10, 1024 * 1024));
        store.record(evidence(Some("acme"), "before"));
        let poisoned = catch_unwind::<_, ()>(AssertUnwindSafe(|| {
            let _ring = store.ring.lock().unwrap();
            panic!("poison the store");
        }));
        assert!(poisoned.is_err());
        assert!(store.ring.is_poisoned());
        assert_eq!(store.retained(), (0, 0));
        assert!(!store.ring.is_poisoned());
        let resets = "ferrum_alloy_diagnostics_resets_total";
        assert_eq!(metric(&store, resets), 1);
        store.record(evidence(Some("acme"), "after"));
        assert_eq!(found(&store, "acme", "after"), 1);
        assert_eq!(found(&store, "acme", "before"), 0);
        assert_eq!(metric(&store, resets), 1);
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
                .map(|(tenant, owner)| {
                    report("test", "alloy-test", &store.find(tenant, owner.clone()))
                })
                .collect();
            let mut larger = evidence(Some("t0"), &"l".repeat(id_length + 20));
            larger.route = Some(Arc::from(format!("{short_route}{}", "/".repeat(80))));
            let larger_bytes = sized("t0", &larger.request_id, larger.route.as_deref());
            assert!(each < larger_bytes && larger_bytes < 2 * each);
            let owner = larger.diagnostic_id().clone();
            // Room needs another tenant's sole record, which is younger than
            // the takeover age.
            store.record(larger);
            assert!(store.find("t0", owner).records.is_empty());
            assert_eq!(store.retained(), (6, 6 * each));
            for ((tenant, owner), expected) in originals.iter().zip(before) {
                assert_eq!(
                    report("test", "alloy-test", &store.find(tenant, owner.clone())),
                    expected
                );
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
        let small = evidence(Some("old"), "small");
        let mut big = evidence(Some("old"), &"b".repeat(200));
        big.route = Some(Arc::from("/".repeat(400)));
        let small_bytes = sized("old", &small.request_id, None);
        let big_bytes = sized("old", &big.request_id, big.route.as_deref());
        let store = EvidenceStore::new(&settings(10, small_bytes + big_bytes));
        store.record(small);
        store.record(big);
        let before = store.retained();
        let owners: Vec<_> = store
            .ring()
            .records
            .values()
            .map(|record| record.diagnostic_id.clone())
            .collect();
        let mut candidate = evidence(Some("new"), "large");
        candidate.route = Some(Arc::from("/".repeat(100)));
        let bytes = sized("new", &candidate.request_id, candidate.route.as_deref());
        assert!(small_bytes < bytes && bytes <= big_bytes);
        // The donor can give up its small record and keep more than the
        // candidate requests, but more room needs its remaining sole record,
        // younger than the takeover age. The donor reservation rolls back.
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
    fn a_tenant_holding_nothing_takes_a_large_record_from_a_tenant_holding_two() {
        let mut big = evidence(Some("donor"), &"b".repeat(200));
        big.route = Some(Arc::from("/".repeat(400)));
        let small = evidence(Some("donor"), "small");
        let large_bytes = sized("donor", &big.request_id, big.route.as_deref());
        let small_bytes = sized("donor", &small.request_id, None);
        let mut config = settings(10, large_bytes + small_bytes);
        config.max_age_ms = 0;
        let store = EvidenceStore::new(&config);
        store.record(big);
        store.record(small);
        let mut candidate = evidence(Some("other"), &"c".repeat(100));
        candidate.route = Some(Arc::from("/".repeat(200)));
        let bytes = sized("other", &candidate.request_id, candidate.route.as_deref());
        assert!(small_bytes < bytes && bytes < large_bytes);
        store.record(candidate);
        // Removing the donor's large oldest record would leave it fewer
        // bytes than the new tenant requests, but it still keeps as many
        // records as the new tenant then holds: one each.
        assert_eq!(store.retained(), (2, small_bytes + bytes));
        assert_eq!(held(&store, "donor"), 1);
        assert_eq!(found(&store, "donor", "small"), 1);
        assert_eq!(held(&store, "other"), 1);
        let evicted = r#"ferrum_alloy_diagnostics_evicted_total{reason="bytes"}"#;
        assert_eq!(metric(&store, evicted), 1);
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
        let seqs: Vec<_> = store.ring().tenants["old"].seqs.iter().copied().collect();
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
        // Each larger record needs another tenant's sole record, which is
        // younger than the takeover age.
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

    /// A request's operations, pool wait, and admission wait reach the live
    /// report linked to its server span and placed on one clock, so the rules
    /// can attribute the time to headers to the dominant operation.
    #[test]
    fn live_reports_carry_operation_pool_and_admission_evidence() {
        let mut request = evidence(Some("acme"), "req-ops");
        let server_span = request.span_id;
        request.started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        request.status = Some(200);
        request.time_to_headers = Some(Duration::from_millis(100));
        request.body_duration = Some(Duration::from_millis(5));
        request.duration = Duration::from_millis(105);
        request.outcome = BodyOutcome::Completed;
        request.admission_wait = Some(Duration::from_millis(4));
        let load_span = SpanId::random();
        let mut load = OperationEvidence::new(
            "orders.load",
            OperationKind::Db,
            OperationOutcome::Completed,
            load_span,
            server_span,
        );
        load.started = Duration::from_millis(10);
        load.duration = Duration::from_millis(80);
        load.pool_wait = Some(Duration::from_millis(30));
        load.pool_wait_ended = Some(Duration::from_millis(45));
        let mut decode = OperationEvidence::new(
            "orders.decode",
            OperationKind::Internal,
            OperationOutcome::Error,
            SpanId::random(),
            load_span,
        );
        decode.started = Duration::from_millis(60);
        decode.duration = Duration::from_millis(20);
        request.operations = vec![decode, load];
        request.operations_dropped = 3;
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(request);
        let found = store.find("acme", id("req-ops"));
        let json = serde_json::to_vec(&report("orders", "alloy-test", &found)).unwrap();
        let parsed = parse_offline(&json, &Limits::default()).unwrap();
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        let report = parsed.report;
        // Four server observations, the admission wait, two operations and
        // one pool wait.
        assert_eq!(report.observations.len(), 8);
        let notes = &report.collection.notes;
        let noted = notes
            .iter()
            .any(|note| note.starts_with("3 operation(s) beyond the per-request limit"));
        assert!(noted, "{notes:?}");
        for observation in &report.observations {
            assert!(catalog::is_known(&observation.name), "{}", observation.name);
            assert_eq!(observation.producer.instance.as_deref(), Some("alloy-test"));
        }
        let server = server_span.to_hex();
        let head = report
            .observation(&format!("alloy:{server}:time_to_headers"))
            .unwrap();
        let start = 1_790_000_000_000_000_000;
        assert_eq!(
            head.interval,
            Some(Interval::new(start, start + 100_000_000))
        );
        let admission = report
            .observation(&format!("alloy:{server}:admission"))
            .unwrap();
        assert_eq!(admission.name, catalog::ALLOY_ADMISSION_WAIT);
        assert_eq!(admission.duration_ms(), Some(4.0));

        let load_id = load_span.to_hex();
        let operation = report
            .observation(&format!("alloy:{load_id}:operation"))
            .unwrap();
        assert_eq!(operation.name, catalog::ALLOY_OPERATION_DURATION);
        assert_eq!(operation.duration_ms(), Some(80.0));
        assert_eq!(operation.scope.leg, Leg::ServiceToDependency);
        assert_eq!(operation.attr("operation.name"), Some("orders.load"));
        assert_eq!(operation.attr("operation.kind"), Some("db"));
        assert_eq!(operation.attr("operation.outcome"), Some("completed"));
        let span = operation.span.as_ref().unwrap();
        assert_eq!(span.span_id, load_id);
        assert_eq!(span.parent_span_id.as_deref(), Some(server.as_str()));
        assert_eq!(
            operation.interval,
            Some(Interval::new(start + 10_000_000, start + 90_000_000))
        );
        let pool = report
            .observation(&format!("alloy:{load_id}:pool_wait"))
            .unwrap();
        assert_eq!(pool.name, catalog::ALLOY_DB_POOL_WAIT);
        assert_eq!(pool.duration_ms(), Some(30.0));
        assert_eq!(pool.span, operation.span);
        assert_eq!(
            pool.interval,
            Some(Interval::new(start + 15_000_000, start + 45_000_000)),
            "the acquire, placed inside orders.load"
        );
        let decode = report
            .observations
            .iter()
            .find(|o| o.attr("operation.name") == Some("orders.decode"))
            .unwrap();
        assert_eq!(decode.scope.leg, Leg::Service);
        assert_eq!(decode.attr("operation.outcome"), Some("error"));
        let parent = decode.span.as_ref().unwrap().parent_span_id.as_deref();
        assert_eq!(parent, Some(load_id.as_str()), "nested under orders.load");

        // The 80 ms database call inside the 100 ms header phase dominates.
        let findings = analyze(&report, &Thresholds::default());
        let dominates = findings
            .iter()
            .find(|f| f.code == "alloy.service.operation_dominates")
            .unwrap_or_else(|| panic!("{findings:#?}"));
        assert_eq!(dominates.confidence, Confidence::Likely);
        assert!(
            dominates.explanation.contains("orders.load"),
            "{}",
            dominates.explanation
        );
    }

    /// Operation entries count toward the store's byte bound.
    #[test]
    fn operations_count_toward_the_byte_estimate() {
        let mut request = evidence(Some("acme"), "req-sized");
        let bare = estimate(
            "acme",
            &request.request_id,
            request.diagnostic_id(),
            None,
            0,
        );
        request.operations = (0..40)
            .map(|_| {
                OperationEvidence::new(
                    "orders.load",
                    OperationKind::Db,
                    OperationOutcome::Completed,
                    SpanId::random(),
                    request.span_id,
                )
            })
            .collect();
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(request);
        let ring = store.ring();
        let record = ring.records.values().next().unwrap();
        assert_eq!(record.operations.len(), MAX_OPERATIONS_PER_REQUEST);
        assert_eq!(record.operations_dropped, 8);
        assert_eq!(
            record.bytes,
            bare + MAX_OPERATIONS_PER_REQUEST * size_of::<OperationEvidence>()
        );
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
        let json = serde_json::to_vec(&report("orders", "alloy-test", &found_now)).unwrap();
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
        let report = super::report("orders", "alloy-test", &found_now);
        let notes = &report.collection.notes;
        assert_eq!(notes.len(), 4, "{notes:?}");
        let resolved = "resolved as a correlation alias (request_id_origin trusted_peer)";
        let noted = notes.iter().any(|note| note.starts_with(resolved));
        assert!(noted, "{notes:?}");
    }

    /// The vendored `ferrum.diagnostic_report` v1 schema.
    fn v1_schema() -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../contracts/diagnostics/diagnostic-report.v1.schema.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Whether `value` has JSON Schema type `name`.
    fn has_type(value: &serde_json::Value, name: &str) -> bool {
        match name {
            "string" => value.is_string(),
            "integer" => value.is_u64() || value.is_i64(),
            "number" => value.is_number(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            other => panic!("type {other:?} is not implemented by this validator"),
        }
    }

    /// Whether `text` matches one of the schema's anchored patterns.
    fn matches_pattern(pattern: &str, text: &str) -> bool {
        let hex = |len: usize| {
            text.len() == len
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        match pattern {
            "^[0-9a-f]{16}$" => hex(16),
            "^[0-9a-f]{32}$" => hex(32),
            "^[A-Za-z0-9._:-]{1,64}$" => {
                (1..=64).contains(&text.len())
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            }
            r"^1\.[0-9]{1,4}$" => text.strip_prefix("1.").is_some_and(|minor| {
                (1..=4).contains(&minor.len()) && minor.bytes().all(|b| b.is_ascii_digit())
            }),
            other => panic!("pattern {other:?} is not implemented by this validator"),
        }
    }

    /// Collects every way `value` at `path` breaks `schema`, implementing
    /// the JSON Schema keywords the v1 schema uses. Any other keyword or
    /// pattern fails the test, so a schema update cannot go unchecked.
    fn violations(
        root: &serde_json::Value,
        schema: &serde_json::Value,
        value: &serde_json::Value,
        path: &str,
        out: &mut Vec<String>,
    ) {
        use serde_json::Value;
        let schema = match schema {
            Value::Object(schema) => schema,
            Value::Bool(true) => return,
            Value::Bool(false) => {
                out.push(format!("{path}: no value is allowed"));
                return;
            }
            other => panic!("{path}: schema {other}"),
        };
        let max = |rule: &Value| usize::try_from(rule.as_u64().unwrap()).unwrap();
        for (keyword, rule) in schema {
            match keyword.as_str() {
                "$schema" | "$id" | "$defs" | "title" | "description" | "x-contract" => {}
                "$ref" => {
                    let pointer = rule.as_str().unwrap().strip_prefix('#').unwrap();
                    let target = root.pointer(pointer).unwrap();
                    violations(root, target, value, path, out);
                }
                "type" => {
                    let allowed: Vec<&str> = match rule {
                        Value::String(name) => vec![name.as_str()],
                        Value::Array(names) => names.iter().map(|n| n.as_str().unwrap()).collect(),
                        other => panic!("{path}: type {other}"),
                    };
                    if !allowed.iter().any(|name| has_type(value, name)) {
                        out.push(format!("{path}: {value} is not of type {allowed:?}"));
                    }
                }
                "anyOf" => {
                    let matched = rule.as_array().unwrap().iter().any(|alternative| {
                        let mut inner = Vec::new();
                        violations(root, alternative, value, path, &mut inner);
                        inner.is_empty()
                    });
                    if !matched {
                        out.push(format!("{path}: {value} matches no anyOf alternative"));
                    }
                }
                "enum" => {
                    if !rule.as_array().unwrap().contains(value) {
                        out.push(format!("{path}: {value} is not in {rule}"));
                    }
                }
                "const" => {
                    if rule != value {
                        out.push(format!("{path}: {value} is not {rule}"));
                    }
                }
                "required" => {
                    if let Some(object) = value.as_object() {
                        for name in rule.as_array().unwrap() {
                            let name = name.as_str().unwrap();
                            if !object.contains_key(name) {
                                out.push(format!("{path}: {name} is required"));
                            }
                        }
                    }
                }
                "properties" => {
                    if let Some(object) = value.as_object() {
                        for (name, property) in rule.as_object().unwrap() {
                            if let Some(member) = object.get(name) {
                                let at = format!("{path}/{name}");
                                violations(root, property, member, &at, out);
                            }
                        }
                    }
                }
                "additionalProperties" => {
                    if let Some(object) = value.as_object() {
                        let declared = schema.get("properties").and_then(Value::as_object);
                        for (name, member) in object {
                            if declared.is_some_and(|declared| declared.contains_key(name)) {
                                continue;
                            }
                            violations(root, rule, member, &format!("{path}/{name}"), out);
                        }
                    }
                }
                "maxProperties" => {
                    if let Some(object) = value.as_object()
                        && object.len() > max(rule)
                    {
                        out.push(format!("{path}: {} properties", object.len()));
                    }
                }
                "items" => {
                    if let Some(items) = value.as_array() {
                        for (n, item) in items.iter().enumerate() {
                            violations(root, rule, item, &format!("{path}/{n}"), out);
                        }
                    }
                }
                "maxItems" => {
                    if let Some(items) = value.as_array()
                        && items.len() > max(rule)
                    {
                        out.push(format!("{path}: {} items", items.len()));
                    }
                }
                "maxLength" => {
                    if let Some(text) = value.as_str()
                        && text.chars().count() > max(rule)
                    {
                        out.push(format!("{path}: {} characters", text.chars().count()));
                    }
                }
                "pattern" => {
                    if let Some(text) = value.as_str()
                        && !matches_pattern(rule.as_str().unwrap(), text)
                    {
                        out.push(format!("{path}: {text:?} does not match {rule}"));
                    }
                }
                "minimum" => {
                    if let Some(number) = value.as_f64()
                        && number < rule.as_f64().unwrap()
                    {
                        out.push(format!("{path}: {value} is below {rule}"));
                    }
                }
                other => panic!("{path}: keyword {other:?} is not implemented by this validator"),
            }
        }
    }

    /// Checks `report` against the vendored v1 schema and the offline reader,
    /// and returns what the reader accepted.
    fn conforming(report: &DiagnosticReport) -> DiagnosticReport {
        let schema = v1_schema();
        let value = serde_json::to_value(report).unwrap();
        let mut out = Vec::new();
        violations(&schema, &schema, &value, "", &mut out);
        assert!(out.is_empty(), "{out:#?}");
        let json = serde_json::to_vec(&value).unwrap();
        let parsed = parse_offline(&json, &Limits::default()).unwrap();
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        parsed.report
    }

    /// The validator rejects what the schema forbids, so a clean result
    /// means something.
    #[test]
    fn the_schema_validator_rejects_nonconforming_reports() {
        let schema = v1_schema();
        let mut request = evidence(Some("acme"), "req-bad");
        request.started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        request.operations = vec![OperationEvidence::new(
            "orders.load",
            OperationKind::Db,
            OperationOutcome::Completed,
            SpanId::random(),
            request.span_id,
        )];
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        store.record(request);
        let good = serde_json::to_value(report(
            "orders",
            "alloy-test",
            &store.find("acme", id("req-bad")),
        ))
        .unwrap();
        let mut out = Vec::new();
        violations(&schema, &schema, &good, "", &mut out);
        assert!(out.is_empty(), "{out:#?}");
        let mutations: [(&str, serde_json::Value); 6] = [
            ("/schema", serde_json::json!("other")),
            ("/schema_version", serde_json::json!("2.0")),
            ("/observations/0/span/span_id", serde_json::json!("XYZ")),
            ("/observations/0/scope/leg", serde_json::json!(7)),
            ("/observations/0/id", serde_json::json!("has space")),
            ("/collection/notes/0", serde_json::json!("n".repeat(2049))),
        ];
        for (pointer, replacement) in mutations {
            let mut bad = good.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            let mut out = Vec::new();
            violations(&schema, &schema, &bad, "", &mut out);
            assert!(!out.is_empty(), "{pointer} accepted");
        }
        // The body phase, which has a `body.outcome` attribute.
        let mut bad = good.clone();
        let attributes = bad
            .pointer_mut("/observations/1/attributes")
            .unwrap()
            .as_object_mut()
            .unwrap();
        for n in 0..33 {
            attributes.insert(format!("extra.{n}"), serde_json::json!("x"));
        }
        let mut out = Vec::new();
        violations(&schema, &schema, &bad, "", &mut out);
        assert!(!out.is_empty(), "33 attributes accepted");
    }

    /// A request started at a fixed wall-clock time with a 100 ms header
    /// phase.
    fn timed(request_id: &str) -> RequestEvidence {
        let mut request = evidence(Some("acme"), request_id);
        request.started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        request.status = Some(200);
        request.time_to_headers = Some(Duration::from_millis(100));
        request.body_duration = Some(Duration::from_millis(5));
        request.duration = Duration::from_millis(105);
        request.outcome = BodyOutcome::Completed;
        request
    }

    /// A completed database operation of `parent`, `started` and lasting
    /// `length` milliseconds into the request.
    fn op(name: &'static str, parent: SpanId, started: u64, length: u64) -> OperationEvidence {
        let mut operation = OperationEvidence::new(
            name,
            OperationKind::Db,
            OperationOutcome::Completed,
            SpanId::random(),
            parent,
        );
        operation.started = Duration::from_millis(started);
        operation.duration = Duration::from_millis(length);
        operation
    }

    /// Every report Alloy serves conforms to the vendored v1 schema and the
    /// offline reader: an empty lookup, attempts without operations,
    /// operations with pool and admission waits, a full operation cap whose
    /// parent was dropped, an overlong operation name, and an operation
    /// that ends long after the request started.
    #[test]
    fn every_live_report_validates_against_the_v1_schema() {
        let store = EvidenceStore::new(&DiagnosticsSettings::default());
        let lookup =
            |request_id: &str| report("orders", "alloy-test", &store.find("acme", id(request_id)));

        // Nothing found.
        let empty = conforming(&lookup("req-missing"));
        assert!(empty.observations.is_empty());

        // Attempts without operations, one without headers.
        let mut first = attempt("acme", "req-attempts", TraceId::random());
        first.started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        first.time_to_headers = Some(Duration::from_millis(12));
        let mut second = first.clone();
        second.span_id = SpanId::random();
        second.time_to_headers = None;
        second.outcome = BodyOutcome::CancelledBeforeHeaders;
        store.record(first);
        store.record(second);
        assert_eq!(conforming(&lookup("req-attempts")).observations.len(), 8);

        // Operations, a pool wait, and the admission wait.
        let mut request = timed("req-ops");
        request.admission_wait = Some(Duration::from_millis(4));
        let mut load = op("orders.load", request.span_id, 10, 80);
        load.pool_wait = Some(Duration::from_millis(30));
        load.pool_wait_ended = Some(Duration::from_millis(45));
        let decode = op("orders.decode", load.span_id, 60, 20);
        request.operations = vec![decode, load];
        store.record(request);
        let ops = conforming(&lookup("req-ops"));
        assert_eq!(ops.observations.len(), 8);
        assert!(
            ops.observations
                .iter()
                .all(|o| o.attr(catalog::LINEAGE_PARENT_SPAN_ID).is_none()),
            "every parent is retained"
        );

        // A full cap: 32 children kept, the outer operation that finished
        // last dropped with 7 more, and a name too long for the reader.
        let mut request = timed("req-cap");
        let server = request.span_id;
        let outer = SpanId::random();
        let long: &'static str = Box::leak("n".repeat(4_096).into_boxed_str());
        let mut operations = vec![op(long, outer, 1, 80)];
        operations.extend((1..MAX_OPERATIONS_PER_REQUEST).map(|n| {
            let n = u64::try_from(n).unwrap();
            op("orders.item", outer, 81 + n / 4, 1)
        }));
        let mut outer_op = op("orders.batch", server, 0, 99);
        outer_op.span_id = outer;
        operations.push(outer_op);
        operations.extend((0..7).map(|_| op("orders.late", server, 99, 1)));
        request.operations = operations;
        store.record(request);
        let cap = conforming(&lookup("req-cap"));
        let measured: Vec<&Observation> = cap
            .observations
            .iter()
            .filter(|o| o.name == catalog::ALLOY_OPERATION_DURATION)
            .collect();
        assert_eq!(measured.len(), MAX_OPERATIONS_PER_REQUEST);
        let notes = &cap.collection.notes;
        let noted = notes
            .iter()
            .any(|note| note.starts_with("8 operation(s) beyond the per-request limit"));
        assert!(noted, "{notes:?}");
        let server_hex = server.to_hex();
        for operation in &measured {
            assert_eq!(
                operation.attr(catalog::LINEAGE_PARENT_SPAN_ID),
                Some(server_hex.as_str()),
                "the dropped parent is bridged to the server span"
            );
            let parent = operation.span.as_ref().unwrap().parent_span_id.as_deref();
            assert_eq!(
                parent,
                Some(outer.to_hex().as_str()),
                "the real parent is kept"
            );
        }
        let named = measured[0].attr("operation.name").unwrap();
        assert!(
            named.starts_with("digest:") && named.len() <= 512,
            "{named}"
        );
        let other = ferrum_alloy_diagnostics::otlp::bounded_text(&"n".repeat(4_096));
        assert_eq!(named, other, "the OTLP importer's digest");
        // The rules still reach the dominant child through the lineage.
        let findings = analyze(&cap, &Thresholds::default());
        let dominates = findings
            .iter()
            .find(|f| f.code == "alloy.service.operation_dominates")
            .unwrap_or_else(|| panic!("{findings:#?}"));
        assert!(
            dominates.explanation.contains(named),
            "{}",
            dominates.explanation
        );

        // A streaming response whose operation ends 25 hours after the
        // request started: the interval is left out, its duration kept.
        let mut request = timed("req-late");
        let hours = |n: u64| Duration::from_secs(n * 3_600);
        request.body_duration = Some(hours(26));
        request.duration = hours(26);
        let mut late = op("stream.flush", request.span_id, 0, 2);
        late.started = hours(25);
        let early = op("stream.open", request.span_id, 10, 20);
        request.operations = vec![early, late];
        store.record(request);
        let streamed = conforming(&lookup("req-late"));
        let operation = |name: &str| {
            streamed
                .observations
                .iter()
                .find(|o| o.attr("operation.name") == Some(name))
                .unwrap()
        };
        assert_eq!(operation("stream.flush").interval, None);
        assert_eq!(operation("stream.flush").duration_ms(), Some(2.0));
        assert!(operation("stream.open").interval.is_some());
        let head = streamed
            .observations
            .iter()
            .find(|o| o.name == catalog::ALLOY_TIME_TO_HEADERS)
            .unwrap();
        assert!(head.interval.is_some(), "the header phase keeps its place");
        let notes = &streamed.collection.notes;
        let noted = notes
            .iter()
            .any(|note| note.starts_with("1 interval(s) ending more than 24 h"));
        assert!(noted, "{notes:?}");
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
