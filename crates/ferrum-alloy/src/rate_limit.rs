//! Token-bucket rate limits for the management listener.
//!
//! Probes (`/livez` and `/readyz`) and all other paths ("endpoints") have
//! separate budgets, each with its own client table behind its own lock, so
//! a flood of one neither throttles nor contends with the other.
//!
//! An endpoint request is charged to a bucket of its client and to one of
//! the whole listener, and is admitted only when both hold a token. A
//! rejected request consumes nothing, so a client over its own limit cannot
//! drain the listener budget of everyone else. A probe is charged to its
//! client's bucket only: a listener-wide probe budget would let a few
//! sources starve kubelet of `/livez`.
//!
//! A client is the transport peer address ([`PeerInfo`], or axum's
//! `ConnectInfo`), never a request header. IPv4-mapped IPv6 addresses count
//! as IPv4; other IPv6 addresses are keyed by their `ipv6_prefix_len`
//! prefix, which a single host usually controls entirely. Peers in
//! `exempt_networks` (loopback by default) are not limited at all.
//!
//! Each client table holds at most `max_clients` entries. A client gets an
//! entry only when a request of it is admitted, so rejected requests never
//! take room. An entry whose bucket has refilled completely is equivalent to
//! none and is forgotten: every request examines the longest-unexamined
//! entry, and a newcomer that finds the table full examines a few more. The
//! work per request is bounded; a table is never swept as a whole. While a
//! table is full of clients still spending their budget, endpoint requests
//! from further clients share one client budget, and probes from further
//! clients are served untracked, because refusing them restarts pods.
//! Requests without a transport address use the shared budget.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ferrum_alloy_telemetry::PeerInfo;
use http::header::{CACHE_CONTROL, HeaderValue, RETRY_AFTER};
use ipnet::IpNet;

use crate::config::ManagementRateLimit;
use crate::problem::{Problem, ProblemKind};

/// Buckets count billionths of a request, so refills are exact integers.
const TOKEN: u64 = 1_000_000_000;

/// Entries every request examines for a refilled bucket.
const EXAMINED_PER_REQUEST: usize = 1;

/// Further entries a newcomer examines when the table is full.
const EXAMINED_WHEN_FULL: usize = 8;

/// Sustained rate and capacity of one bucket.
#[derive(Debug, Clone, Copy)]
struct Rate {
    per_second: u64,
    capacity: u64,
}

impl Rate {
    fn new(per_second: u32, burst: u32) -> Self {
        Self {
            per_second: u64::from(per_second.max(1)),
            capacity: u64::from(burst.max(1)).saturating_mul(TOKEN),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u64,
    updated: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Self {
        Self {
            tokens: rate.capacity,
            updated: now,
        }
    }

    fn refill(&mut self, rate: Rate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated);
        let added = elapsed
            .as_nanos()
            .saturating_mul(u128::from(rate.per_second));
        let tokens = u128::from(self.tokens)
            .saturating_add(added)
            .min(u128::from(rate.capacity));
        self.tokens = u64::try_from(tokens).unwrap_or(rate.capacity);
        self.updated = self.updated.max(now);
    }

    fn is_full(&self, rate: Rate) -> bool {
        self.tokens >= rate.capacity
    }

    fn has_token(&self) -> bool {
        self.tokens >= TOKEN
    }

    fn take(&mut self) {
        self.tokens = self.tokens.saturating_sub(TOKEN);
    }

    /// Time until the bucket holds a token again.
    fn wait(&self, rate: Rate) -> Duration {
        let missing = TOKEN.saturating_sub(self.tokens);
        Duration::from_nanos(missing.div_ceil(rate.per_second))
    }
}

/// Which budget a request is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Budget {
    /// `/livez` and `/readyz`.
    Probes,
    /// Every other path.
    Endpoints,
}

impl Budget {
    const ALL: [Self; 2] = [Self::Probes, Self::Endpoints];

    fn of(path: &str) -> Self {
        if crate::management::PROBE_PATHS.contains(&path) {
            Self::Probes
        } else {
            Self::Endpoints
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Probes => "probes",
            Self::Endpoints => "endpoints",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Probes => 0,
            Self::Endpoints => 1,
        }
    }

    /// The buckets that can reject a request of this budget.
    fn scopes(self) -> &'static [Scope] {
        match self {
            Self::Probes => &[Scope::Client, Scope::Shared],
            Self::Endpoints => &Scope::ALL,
        }
    }
}

/// Which bucket rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The client's own bucket.
    Client,
    /// The bucket shared by requests without a transport address and, for
    /// endpoints, by clients the table has no room for.
    Shared,
    /// The listener bucket (endpoints only).
    Global,
}

impl Scope {
    const ALL: [Self; 3] = [Self::Client, Self::Shared, Self::Global];

    fn label(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Shared => "shared",
            Self::Global => "global",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Client => 0,
            Self::Shared => 1,
            Self::Global => 2,
        }
    }
}

/// The outcome of [`RateLimiter::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Serve the request.
    Allow,
    /// Reject it; a token is available again after `retry_after`.
    Reject {
        /// The bucket that was empty (the client's when both were).
        scope: Scope,
        /// Time until both buckets hold a token.
        retry_after: Duration,
    },
}

/// What a full table does with a newcomer.
#[derive(Debug, Clone, Copy)]
enum Overflow {
    /// Charge it to the shared bucket.
    Shared,
    /// Serve it without an entry.
    Untracked,
}

/// The limits of one budget.
#[derive(Debug, Clone, Copy)]
struct Policy {
    client: Rate,
    global: Option<Rate>,
    max_clients: usize,
    overflow: Overflow,
}

/// Where a request is charged.
#[derive(Debug, Clone, Copy)]
enum Slot {
    /// The entry of a tracked client.
    Tracked(IpAddr),
    /// A fresh bucket, which becomes the client's entry if it is admitted.
    New(IpAddr),
    /// A fresh bucket that is not kept.
    Untracked,
    /// The shared bucket.
    Shared,
}

/// The client table of one budget.
#[derive(Debug)]
struct Table {
    clients: HashMap<IpAddr, Bucket>,
    /// Every key of `clients` exactly once, longest-unexamined first.
    queue: VecDeque<IpAddr>,
    shared: Bucket,
    /// The listener bucket, for endpoints.
    global: Option<Bucket>,
    /// Newcomers served without an entry because the table was full.
    untracked: u64,
}

impl Table {
    fn new(policy: &Policy, now: Instant) -> Self {
        Self {
            clients: HashMap::new(),
            queue: VecDeque::new(),
            shared: Bucket::full(policy.client, now),
            global: policy.global.map(|rate| Bucket::full(rate, now)),
            untracked: 0,
        }
    }

    /// Examines up to `count` entries, longest-unexamined first. An entry
    /// whose bucket has refilled completely is forgotten; any other moves to
    /// the back of the queue.
    fn forget_refilled(&mut self, rate: Rate, now: Instant, count: usize) {
        for _ in 0..count {
            let Some(key) = self.queue.pop_front() else {
                return;
            };
            let Some(bucket) = self.clients.get_mut(&key) else {
                continue;
            };
            bucket.refill(rate, now);
            if bucket.is_full(rate) {
                self.clients.remove(&key);
            } else {
                self.queue.push_back(key);
            }
        }
    }

    fn slot(&mut self, policy: &Policy, client: Option<IpAddr>, now: Instant) -> Slot {
        let Some(key) = client else {
            return Slot::Shared;
        };
        if self.clients.contains_key(&key) {
            return Slot::Tracked(key);
        }
        if self.clients.len() >= policy.max_clients {
            self.forget_refilled(policy.client, now, EXAMINED_WHEN_FULL);
        }
        if self.clients.len() < policy.max_clients {
            return Slot::New(key);
        }
        match policy.overflow {
            Overflow::Shared => Slot::Shared,
            Overflow::Untracked => Slot::Untracked,
        }
    }

    /// Charges one request from the client keyed `client`.
    fn check(&mut self, policy: &Policy, client: Option<IpAddr>, now: Instant) -> Decision {
        self.forget_refilled(policy.client, now, EXAMINED_PER_REQUEST);
        let slot = self.slot(policy, client, now);
        let Self {
            clients,
            queue,
            shared,
            global,
            untracked,
        } = self;
        let mut fresh = Bucket::full(policy.client, now);
        let (own, own_scope) = match slot {
            Slot::Tracked(key) => (clients.get_mut(&key).unwrap_or(&mut fresh), Scope::Client),
            Slot::New(_) | Slot::Untracked => (&mut fresh, Scope::Client),
            Slot::Shared => (shared, Scope::Shared),
        };
        own.refill(policy.client, now);
        if let (Some(bucket), Some(rate)) = (global.as_mut(), policy.global) {
            bucket.refill(rate, now);
        }
        let global_ready = global.as_ref().is_none_or(Bucket::has_token);
        if own.has_token() && global_ready {
            own.take();
            if let Some(bucket) = global.as_mut() {
                bucket.take();
            }
            match slot {
                Slot::New(key) => {
                    clients.insert(key, fresh);
                    queue.push_back(key);
                }
                Slot::Untracked => *untracked = untracked.saturating_add(1),
                Slot::Tracked(_) | Slot::Shared => {}
            }
            return Decision::Allow;
        }
        let scope = if own.has_token() {
            Scope::Global
        } else {
            own_scope
        };
        let global_wait = match (global.as_ref(), policy.global) {
            (Some(bucket), Some(rate)) => bucket.wait(rate),
            _ => Duration::ZERO,
        };
        let retry_after = own.wait(policy.client).max(global_wait);
        Decision::Reject { scope, retry_after }
    }
}

/// The limits and the client table of one budget.
#[derive(Debug)]
struct Limiter {
    policy: Policy,
    table: Mutex<Table>,
}

impl Limiter {
    fn new(policy: Policy, now: Instant) -> Self {
        Self {
            table: Mutex::new(Table::new(&policy, now)),
            policy,
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The management listener's rate limiter.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    probes: Limiter,
    endpoints: Limiter,
    /// Peers that bypass the limits.
    exempt: Vec<IpNet>,
    /// Keeps the network part of an IPv6 address.
    ipv6_mask: u128,
    /// Rejections by [`Budget`] and [`Scope`].
    rejected: [[AtomicU64; 3]; 2],
}

impl RateLimiter {
    pub(crate) fn new(config: &ManagementRateLimit) -> Self {
        Self::starting_at(config, Instant::now())
    }

    fn starting_at(config: &ManagementRateLimit, now: Instant) -> Self {
        let max_clients = config.max_clients.max(1);
        let probes = Policy {
            client: Rate::new(config.probe_requests_per_second, config.probe_burst),
            global: None,
            max_clients,
            overflow: Overflow::Untracked,
        };
        let global = Rate::new(config.global_requests_per_second, config.global_burst);
        let endpoints = Policy {
            client: Rate::new(config.requests_per_second, config.burst),
            global: Some(global),
            max_clients,
            overflow: Overflow::Shared,
        };
        let prefix_len = u32::from(config.ipv6_prefix_len.min(128));
        Self {
            probes: Limiter::new(probes, now),
            endpoints: Limiter::new(endpoints, now),
            exempt: config.exempt_networks.iter().map(IpNet::trunc).collect(),
            ipv6_mask: u128::MAX.checked_shl(128 - prefix_len).unwrap_or(0),
            rejected: Default::default(),
        }
    }

    fn limiter(&self, budget: Budget) -> &Limiter {
        match budget {
            Budget::Probes => &self.probes,
            Budget::Endpoints => &self.endpoints,
        }
    }

    /// Whether requests from the canonical peer address `ip` bypass the
    /// limits.
    fn is_exempt(&self, ip: IpAddr) -> bool {
        self.exempt.iter().any(|network| network.contains(&ip))
    }

    /// The table key of a canonical peer address: IPv4 as is, IPv6 by its
    /// prefix.
    fn client_key(&self, ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V4(_) => ip,
            IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & self.ipv6_mask)),
        }
    }

    /// Charges one request from the transport peer `peer` to `budget`.
    pub(crate) fn check(&self, peer: Option<IpAddr>, budget: Budget, now: Instant) -> Decision {
        // IPv4-mapped IPv6 addresses are IPv4 clients.
        let peer = peer.map(|ip| ip.to_canonical());
        if peer.is_some_and(|ip| self.is_exempt(ip)) {
            return Decision::Allow;
        }
        let key = peer.map(|ip| self.client_key(ip));
        let limiter = self.limiter(budget);
        let decision = limiter.table().check(&limiter.policy, key, now);
        if let Decision::Reject { scope, .. } = decision {
            let row = self.rejected.get(budget.index());
            if let Some(counter) = row.and_then(|row| row.get(scope.index())) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
        decision
    }

    /// Clients of `budget` with their own entry.
    pub(crate) fn tracked_clients(&self, budget: Budget) -> usize {
        self.limiter(budget).table().clients.len()
    }

    /// Prometheus text for the rejection counters and the table sizes.
    pub(crate) fn render_prometheus(&self) -> String {
        let name = "ferrum_alloy_management_rate_limited_total";
        let mut out = format!(
            "# HELP {name} Management requests rejected by a rate limit.\n# TYPE {name} counter\n"
        );
        for budget in Budget::ALL {
            let row = self.rejected.get(budget.index());
            for scope in budget.scopes() {
                let value = row
                    .and_then(|row| row.get(scope.index()))
                    .map_or(0, |counter| counter.load(Ordering::Relaxed));
                out.push_str(&format!(
                    "{name}{{budget=\"{}\",scope=\"{}\"}} {value}\n",
                    budget.label(),
                    scope.label()
                ));
            }
        }
        let name = "ferrum_alloy_management_rate_limit_clients";
        out.push_str(&format!(
            "# HELP {name} Clients tracked individually by the management rate limit.\n# TYPE {name} gauge\n"
        ));
        for budget in Budget::ALL {
            out.push_str(&format!(
                "{name}{{budget=\"{}\"}} {}\n",
                budget.label(),
                self.tracked_clients(budget)
            ));
        }
        let name = "ferrum_alloy_management_rate_limit_untracked_probes_total";
        out.push_str(&format!(
            "# HELP {name} Probes served untracked because the probe client table was full.\n# TYPE {name} counter\n{name} {}\n",
            self.probes.table().untracked
        ));
        out
    }
}

/// The transport peer of a request. Headers are never consulted.
fn peer(request: &Request) -> Option<IpAddr> {
    let extensions = request.extensions();
    if let Some(peer) = extensions.get::<PeerInfo>() {
        return peer.remote_addr.map(|addr| addr.ip());
    }
    let ConnectInfo(addr) = extensions.get::<ConnectInfo<SocketAddr>>()?;
    Some(addr.ip())
}

/// Seconds for `Retry-After`: rounded up, at least one.
fn retry_after_seconds(wait: Duration) -> u64 {
    let partial = u64::from(wait.subsec_nanos() > 0);
    wait.as_secs().saturating_add(partial).max(1)
}

/// `429` Problem Details with `Retry-After`.
fn too_many_requests(wait: Duration) -> Response {
    Problem::new(ProblemKind::RateLimited)
        .with_detail("The management request rate limit was exceeded.")
        .with_header(RETRY_AFTER, HeaderValue::from(retry_after_seconds(wait)))
        .with_header(CACHE_CONTROL, HeaderValue::from_static("no-store"))
        .into_response()
}

/// Middleware that enforces `limiter` before any management handler runs,
/// including the bearer token check.
pub(crate) async fn enforce(
    State(limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    let budget = Budget::of(request.uri().path());
    match limiter.check(peer(&request), budget, Instant::now()) {
        Decision::Allow => next.run(request).await,
        Decision::Reject { scope, retry_after } => {
            tracing::debug!(
                target: "ferrum_alloy::management",
                budget = budget.label(),
                scope = scope.label(),
                "request rate limit exceeded"
            );
            too_many_requests(retry_after)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn limits() -> ManagementRateLimit {
        ManagementRateLimit {
            enabled: true,
            requests_per_second: 2,
            burst: 3,
            global_requests_per_second: 100,
            global_burst: 100,
            probe_requests_per_second: 2,
            probe_burst: 3,
            max_clients: 4,
            ipv6_prefix_len: 64,
            exempt_networks: vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()],
        }
    }

    fn ip(text: &str) -> Option<IpAddr> {
        Some(text.parse().unwrap())
    }

    /// One request to a non-probe path.
    fn call(limiter: &RateLimiter, client: &str, at: Instant) -> Decision {
        limiter.check(ip(client), Budget::Endpoints, at)
    }

    /// One probe request.
    fn probe(limiter: &RateLimiter, client: &str, at: Instant) -> Decision {
        limiter.check(ip(client), Budget::Probes, at)
    }

    fn rejected(decision: Decision) -> (Scope, Duration) {
        match decision {
            Decision::Reject { scope, retry_after } => (scope, retry_after),
            Decision::Allow => panic!("expected a rejection"),
        }
    }

    /// Every tracked key is queued exactly once.
    fn assert_queue_matches(limiter: &RateLimiter) {
        for budget in Budget::ALL {
            let table = limiter.limiter(budget).table();
            assert_eq!(table.queue.len(), table.clients.len());
            for key in &table.queue {
                assert!(table.clients.contains_key(key));
            }
        }
    }

    #[test]
    fn a_burst_is_admitted_then_refilled_at_the_sustained_rate() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for _ in 0..3 {
            assert_eq!(call(&limiter, "192.0.2.1", start), Decision::Allow);
        }
        let (scope, wait) = rejected(call(&limiter, "192.0.2.1", start));
        assert_eq!(scope, Scope::Client);
        assert_eq!(wait, Duration::from_millis(500), "one token at 2/s");
        let early = start + Duration::from_millis(499);
        assert_ne!(call(&limiter, "192.0.2.1", early), Decision::Allow);
        let later = start + Duration::from_millis(500);
        assert_eq!(call(&limiter, "192.0.2.1", later), Decision::Allow);
        assert_ne!(call(&limiter, "192.0.2.1", later), Decision::Allow);
    }

    #[test]
    fn clients_and_budgets_are_independent() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for _ in 0..3 {
            assert_eq!(call(&limiter, "192.0.2.1", start), Decision::Allow);
        }
        assert_ne!(call(&limiter, "192.0.2.1", start), Decision::Allow);
        assert_eq!(call(&limiter, "192.0.2.2", start), Decision::Allow);
        assert_eq!(probe(&limiter, "192.0.2.1", start), Decision::Allow);
    }

    #[test]
    fn rejected_requests_do_not_drain_the_global_budget() {
        let start = Instant::now();
        let mut config = limits();
        config.global_burst = 4;
        config.global_requests_per_second = 1;
        let limiter = RateLimiter::starting_at(&config, start);
        for _ in 0..50 {
            call(&limiter, "192.0.2.1", start);
        }
        assert_eq!(call(&limiter, "192.0.2.2", start), Decision::Allow);
        let (scope, wait) = rejected(call(&limiter, "192.0.2.2", start));
        assert_eq!(scope, Scope::Global);
        assert_eq!(wait, Duration::from_secs(1));
        let decision = probe(&limiter, "192.0.2.1", start);
        assert_eq!(decision, Decision::Allow, "probes have their own budget");
    }

    #[test]
    fn the_client_table_is_bounded_and_overflow_shares_one_budget() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for n in 0..=255u8 {
            let client = Some(IpAddr::from([198, 51, 100, n]));
            limiter.check(client, Budget::Endpoints, start);
            assert!(limiter.tracked_clients(Budget::Endpoints) <= 4);
        }
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 4);
        assert_queue_matches(&limiter);
        // The overflow clients used up the shared budget.
        let (scope, _) = rejected(call(&limiter, "203.0.113.9", start));
        assert_eq!(scope, Scope::Shared);
        // Tracked clients keep their own budget.
        assert_eq!(call(&limiter, "198.51.100.0", start), Decision::Allow);
        // Requests without a transport address use the shared budget.
        let (scope, _) = rejected(limiter.check(None, Budget::Endpoints, start));
        assert_eq!(scope, Scope::Shared);
    }

    #[test]
    fn refilled_clients_are_forgotten_a_few_at_a_time() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for n in 0..4u8 {
            let client = Some(IpAddr::from([198, 51, 100, n]));
            limiter.check(client, Budget::Endpoints, start);
        }
        for _ in 0..3 {
            assert_eq!(call(&limiter, "203.0.113.1", start), Decision::Allow);
        }
        let (scope, _) = rejected(call(&limiter, "203.0.113.1", start));
        assert_eq!(scope, Scope::Shared, "the table is full of active clients");
        // After 500 ms the first four clients have refilled. The newcomer
        // finds one of them to forget and gets its own entry.
        let later = start + Duration::from_millis(500);
        for _ in 0..3 {
            assert_eq!(call(&limiter, "203.0.113.1", later), Decision::Allow);
        }
        let (scope, _) = rejected(call(&limiter, "203.0.113.1", later));
        assert_eq!(scope, Scope::Client);
        // Every request examined one more entry and forgot it.
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 1);
        let much_later = start + Duration::from_secs(120);
        limiter.check(None, Budget::Endpoints, much_later);
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 0);
        assert_queue_matches(&limiter);
    }

    #[test]
    fn rejected_newcomers_never_take_room_from_a_returning_client() {
        let start = Instant::now();
        let mut config = limits();
        config.global_requests_per_second = 1;
        config.global_burst = 3;
        let limiter = RateLimiter::starting_at(&config, start);
        // A returning client and an attacker use up the listener budget.
        assert_eq!(call(&limiter, "192.0.2.1", start), Decision::Allow);
        for _ in 0..2 {
            assert_eq!(call(&limiter, "192.0.2.66", start), Decision::Allow);
        }
        // Newcomers find no listener token: none of them gets an entry.
        let flood = start + Duration::from_millis(500);
        for n in 0..=255u8 {
            let client = Some(IpAddr::from([198, 51, 100, n]));
            let (scope, _) = rejected(limiter.check(client, Budget::Endpoints, flood));
            assert_eq!(scope, Scope::Global);
        }
        assert!(limiter.tracked_clients(Budget::Endpoints) <= 2);
        assert_queue_matches(&limiter);
        // The returning client has refilled and was forgotten, but there is
        // room, so it gets its own budget again instead of the shared one.
        let back = start + Duration::from_secs(1);
        assert_eq!(call(&limiter, "192.0.2.1", back), Decision::Allow);
        let returning: IpAddr = "192.0.2.1".parse().unwrap();
        let table = limiter.endpoints.table();
        assert!(table.clients.contains_key(&returning));
    }

    #[test]
    fn probes_are_limited_per_client_only() {
        let start = Instant::now();
        let mut config = limits();
        config.probe_requests_per_second = 1;
        let limiter = RateLimiter::starting_at(&config, start);
        // Many clients spending their probe budget never limit another one.
        for n in 0..=255u8 {
            let client = Some(IpAddr::from([198, 51, 100, n]));
            for _ in 0..3 {
                let decision = limiter.check(client, Budget::Probes, start);
                assert_eq!(decision, Decision::Allow);
            }
        }
        assert_eq!(limiter.tracked_clients(Budget::Probes), 4);
        // A tracked client is limited by its own bucket.
        let (scope, wait) = rejected(probe(&limiter, "198.51.100.0", start));
        assert_eq!(scope, Scope::Client);
        assert_eq!(wait, Duration::from_secs(1));
        // Clients beyond the full table are served untracked.
        assert_eq!(probe(&limiter, "203.0.113.1", start), Decision::Allow);
        assert!(limiter.probes.table().untracked > 0);
        // Probes without a transport address share one bucket.
        for _ in 0..3 {
            assert_eq!(limiter.check(None, Budget::Probes, start), Decision::Allow);
        }
        let (scope, _) = rejected(limiter.check(None, Budget::Probes, start));
        assert_eq!(scope, Scope::Shared);
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 0);
        assert_queue_matches(&limiter);
    }

    #[test]
    fn exempt_networks_bypass_the_limits() {
        let start = Instant::now();
        let mut config = limits();
        config.exempt_networks.push("10.1.0.0/16".parse().unwrap());
        let limiter = RateLimiter::starting_at(&config, start);
        // 127.0.0.6 is where the Istio sidecar connects from.
        for client in ["127.0.0.6", "::1", "::ffff:127.0.0.1", "10.1.2.3"] {
            for _ in 0..50 {
                assert_eq!(call(&limiter, client, start), Decision::Allow);
                assert_eq!(probe(&limiter, client, start), Decision::Allow);
            }
        }
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 0);
        assert_eq!(limiter.tracked_clients(Budget::Probes), 0);
        for _ in 0..3 {
            assert_eq!(call(&limiter, "10.2.0.1", start), Decision::Allow);
        }
        assert_ne!(call(&limiter, "10.2.0.1", start), Decision::Allow);
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_prefix() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for client in ["2001:db8:1:2::1", "2001:db8:1:2::2", "2001:db8:1:2:f::"] {
            assert_eq!(call(&limiter, client, start), Decision::Allow);
        }
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 1);
        let (scope, _) = rejected(call(&limiter, "2001:db8:1:2::9", start));
        assert_eq!(scope, Scope::Client);
        assert_eq!(call(&limiter, "2001:db8:1:3::1", start), Decision::Allow);

        // IPv4-mapped IPv6 addresses are the IPv4 client.
        for _ in 0..3 {
            assert_eq!(call(&limiter, "::ffff:192.0.2.1", start), Decision::Allow);
        }
        let (scope, _) = rejected(call(&limiter, "192.0.2.1", start));
        assert_eq!(scope, Scope::Client);

        // A shorter prefix groups more addresses, a longer one fewer.
        let mut config = limits();
        config.ipv6_prefix_len = 56;
        let limiter = RateLimiter::starting_at(&config, start);
        for client in ["2001:db8:1:2::1", "2001:db8:1:3::1", "2001:db8:1:ff::1"] {
            assert_eq!(call(&limiter, client, start), Decision::Allow);
        }
        assert_ne!(call(&limiter, "2001:db8:1:4::1", start), Decision::Allow);
        assert_eq!(call(&limiter, "2001:db8:1:100::1", start), Decision::Allow);
        config.ipv6_prefix_len = 128;
        let limiter = RateLimiter::starting_at(&config, start);
        for n in 0..4u16 {
            let client = Some(IpAddr::from([0x2001, 0xdb8, 1, 2, 0, 0, 0, n]));
            limiter.check(client, Budget::Endpoints, start);
        }
        assert_eq!(limiter.tracked_clients(Budget::Endpoints), 4);
    }

    #[test]
    fn metrics_name_every_budget_and_scope() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for _ in 0..4 {
            call(&limiter, "192.0.2.1", start);
        }
        let text = limiter.render_prometheus();
        assert!(
            text.contains("budget=\"endpoints\",scope=\"client\"} 1"),
            "{text}"
        );
        assert!(!text.contains("budget=\"probes\",scope=\"global\""));
        let clients = "ferrum_alloy_management_rate_limit_clients{budget=\"endpoints\"} 1";
        assert!(text.contains(clients), "{text}");
        let untracked = "ferrum_alloy_management_rate_limit_untracked_probes_total 0";
        assert!(text.contains(untracked), "{text}");
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_seconds(Duration::ZERO), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_secs(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1_001)), 2);
    }
}
