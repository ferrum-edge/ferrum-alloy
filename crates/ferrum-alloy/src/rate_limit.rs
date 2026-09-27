//! Token-bucket rate limits for the management listener.
//!
//! Every request is charged to two buckets of its budget: one for the whole
//! listener and one for its client. Probes (`/livez` and `/readyz`) and all
//! other paths have separate budgets, so a flood of one never throttles the
//! other. A request is admitted only when both buckets hold a token, and a
//! rejected request consumes nothing, so a client over its own limit cannot
//! drain the listener budget of everyone else.
//!
//! A client is the transport peer address ([`PeerInfo`], or axum's
//! `ConnectInfo`), never a request header. IPv6 clients are keyed by their
//! /64 prefix, which a single host usually controls entirely. The client
//! table holds at most `max_clients` entries. A client whose buckets have
//! refilled completely is indistinguishable from a new one, so it is dropped
//! when room is needed and at least once a minute. While the table is full
//! of active clients, further clients, and requests without a transport
//! address, share one client budget.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ferrum_alloy_telemetry::PeerInfo;
use http::header::{CACHE_CONTROL, HeaderValue, RETRY_AFTER};

use crate::config::ManagementRateLimit;
use crate::problem::{Problem, ProblemKind};

/// Buckets count billionths of a request, so refills are exact integers.
const TOKEN: u64 = 1_000_000_000;

/// Shortest interval between sweeps for idle clients while the table is full.
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Longest interval between sweeps for idle clients.
const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Keeps the network part of an IPv6 address.
const IPV6_PREFIX_64: u128 = !0 << 64;

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
}

/// Which bucket rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The client's own bucket.
    Client,
    /// The bucket shared by clients the table has no room for.
    Shared,
    /// The listener bucket.
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

#[derive(Debug, Clone, Copy)]
struct Rates {
    client: Rate,
    global: Rate,
}

#[derive(Debug, Clone, Copy)]
struct Client {
    probes: Bucket,
    endpoints: Bucket,
}

impl Client {
    fn full(limits: &Limits, now: Instant) -> Self {
        Self {
            probes: Bucket::full(limits.probes.client, now),
            endpoints: Bucket::full(limits.endpoints.client, now),
        }
    }

    /// Refills both buckets and reports whether they are full, which makes
    /// the entry equivalent to a new one.
    fn refill_is_idle(&mut self, limits: &Limits, now: Instant) -> bool {
        self.probes.refill(limits.probes.client, now);
        self.endpoints.refill(limits.endpoints.client, now);
        let probes = self.probes.is_full(limits.probes.client);
        probes && self.endpoints.is_full(limits.endpoints.client)
    }
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    probes: Rates,
    endpoints: Rates,
    max_clients: usize,
}

#[derive(Debug)]
struct Table {
    clients: HashMap<IpAddr, Client>,
    shared: Client,
    global_probes: Bucket,
    global_endpoints: Bucket,
    last_sweep: Instant,
}

impl Table {
    fn sweep(&mut self, limits: &Limits, now: Instant) {
        self.clients
            .retain(|_, client| !client.refill_is_idle(limits, now));
        self.last_sweep = now;
    }

    /// The key under which `client` has an entry, creating one when there is
    /// room. `None` means the shared budget applies.
    fn admit(&mut self, client: Option<IpAddr>, limits: &Limits, now: Instant) -> Option<IpAddr> {
        let since_sweep = now.saturating_duration_since(self.last_sweep);
        let full = self.clients.len() >= limits.max_clients;
        if since_sweep >= IDLE_SWEEP_INTERVAL || (full && since_sweep >= FULL_SWEEP_INTERVAL) {
            self.sweep(limits, now);
        }
        let key = client?;
        if self.clients.contains_key(&key) {
            return Some(key);
        }
        if self.clients.len() >= limits.max_clients {
            return None;
        }
        self.clients.insert(key, Client::full(limits, now));
        Some(key)
    }
}

/// The management listener's rate limiter.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    limits: Limits,
    table: Mutex<Table>,
    /// Rejections by budget (probes, endpoints) and [`Scope`].
    rejected: [[AtomicU64; 3]; 2],
}

impl RateLimiter {
    pub(crate) fn new(config: &ManagementRateLimit) -> Self {
        Self::starting_at(config, Instant::now())
    }

    fn starting_at(config: &ManagementRateLimit, now: Instant) -> Self {
        let limits = Limits {
            probes: Rates {
                client: Rate::new(config.probe_requests_per_second, config.probe_burst),
                global: Rate::new(
                    config.probe_global_requests_per_second,
                    config.probe_global_burst,
                ),
            },
            endpoints: Rates {
                client: Rate::new(config.requests_per_second, config.burst),
                global: Rate::new(config.global_requests_per_second, config.global_burst),
            },
            max_clients: config.max_clients.max(1),
        };
        Self {
            table: Mutex::new(Table {
                clients: HashMap::new(),
                shared: Client::full(&limits, now),
                global_probes: Bucket::full(limits.probes.global, now),
                global_endpoints: Bucket::full(limits.endpoints.global, now),
                last_sweep: now,
            }),
            limits,
            rejected: Default::default(),
        }
    }

    /// Charges one request from `client` to `budget`.
    pub(crate) fn check(&self, client: Option<IpAddr>, budget: Budget, now: Instant) -> Decision {
        let limits = &self.limits;
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let key = table.admit(client, limits, now);
        let Table {
            clients,
            shared,
            global_probes,
            global_endpoints,
            ..
        } = &mut *table;
        let tracked = match key {
            Some(key) => clients.get_mut(&key),
            None => None,
        };
        let (entry, own_scope) = match tracked {
            Some(entry) => (entry, Scope::Client),
            None => (shared, Scope::Shared),
        };
        let (own, global, rates) = match budget {
            Budget::Probes => (&mut entry.probes, global_probes, limits.probes),
            Budget::Endpoints => (&mut entry.endpoints, global_endpoints, limits.endpoints),
        };
        own.refill(rates.client, now);
        global.refill(rates.global, now);
        if own.has_token() && global.has_token() {
            own.take();
            global.take();
            return Decision::Allow;
        }
        let scope = if own.has_token() {
            Scope::Global
        } else {
            own_scope
        };
        let retry_after = own.wait(rates.client).max(global.wait(rates.global));
        drop(table);
        let row = match budget {
            Budget::Probes => 0,
            Budget::Endpoints => 1,
        };
        if let Some(counter) = self.rejected.get(row).and_then(|r| r.get(scope.index())) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        Decision::Reject { scope, retry_after }
    }

    /// Clients with their own entry.
    pub(crate) fn tracked_clients(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clients
            .len()
    }

    /// Prometheus text for the rejection counters and the table size.
    pub(crate) fn render_prometheus(&self) -> String {
        let name = "ferrum_alloy_management_rate_limited_total";
        let mut out = format!(
            "# HELP {name} Management requests rejected by a rate limit.\n# TYPE {name} counter\n"
        );
        for (budget, row) in [Budget::Probes, Budget::Endpoints]
            .into_iter()
            .zip(&self.rejected)
        {
            for scope in Scope::ALL {
                let value = row
                    .get(scope.index())
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
            "# HELP {name} Clients tracked individually by the management rate limit.\n# TYPE {name} gauge\n{name} {}\n",
            self.tracked_clients()
        ));
        out
    }
}

/// The key of a transport peer address: IPv4 (including IPv4-mapped IPv6)
/// as is, other IPv6 addresses by their /64 prefix.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & IPV6_PREFIX_64)),
        },
    }
}

/// The transport peer of a request. Headers are never consulted.
fn peer(request: &Request) -> Option<IpAddr> {
    let extensions = request.extensions();
    if let Some(peer) = extensions.get::<PeerInfo>() {
        return peer.remote_addr.map(|addr| client_key(addr.ip()));
    }
    let ConnectInfo(addr) = extensions.get::<ConnectInfo<SocketAddr>>()?;
    Some(client_key(addr.ip()))
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
            probe_global_requests_per_second: 100,
            probe_global_burst: 100,
            max_clients: 4,
        }
    }

    fn ip(text: &str) -> Option<IpAddr> {
        Some(text.parse().unwrap())
    }

    /// One request to a non-probe path.
    fn call(limiter: &RateLimiter, client: &str, at: Instant) -> Decision {
        limiter.check(ip(client), Budget::Endpoints, at)
    }

    fn rejected(decision: Decision) -> (Scope, Duration) {
        match decision {
            Decision::Reject { scope, retry_after } => (scope, retry_after),
            Decision::Allow => panic!("expected a rejection"),
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
        let probe = limiter.check(ip("192.0.2.1"), Budget::Probes, start);
        assert_eq!(probe, Decision::Allow);
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
        let probe = limiter.check(ip("192.0.2.1"), Budget::Probes, start);
        assert_eq!(probe, Decision::Allow, "probes have their own budget");
    }

    #[test]
    fn the_client_table_is_bounded_and_overflow_shares_one_budget() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for n in 0..=255u8 {
            let client = Some(IpAddr::from([198, 51, 100, n]));
            limiter.check(client, Budget::Endpoints, start);
            assert!(limiter.tracked_clients() <= 4);
        }
        assert_eq!(limiter.tracked_clients(), 4);
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
    fn idle_clients_are_evicted_to_make_room() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for n in 0..4u8 {
            let client = Some(IpAddr::from([198, 51, 100, n]));
            limiter.check(client, Budget::Endpoints, start);
        }
        for _ in 0..3 {
            call(&limiter, "203.0.113.1", start);
        }
        let (scope, _) = rejected(call(&limiter, "203.0.113.1", start));
        assert_eq!(scope, Scope::Shared);
        // The table sweeps at most once a second while full; by then the
        // first four clients have refilled and are dropped.
        let later = start + Duration::from_secs(1);
        assert_eq!(call(&limiter, "203.0.113.1", later), Decision::Allow);
        assert_eq!(limiter.tracked_clients(), 1, "idle clients were dropped");
        // Idle clients also leave a table that is not full.
        let much_later = start + Duration::from_secs(120);
        limiter.check(None, Budget::Probes, much_later);
        assert_eq!(limiter.tracked_clients(), 0);
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_64_prefix() {
        let start = Instant::now();
        let limiter = RateLimiter::starting_at(&limits(), start);
        for client in ["2001:db8:1:2::1", "2001:db8:1:2::2", "2001:db8:1:2:f::"] {
            assert_eq!(call(&limiter, client, start), Decision::Allow);
        }
        assert_eq!(limiter.tracked_clients(), 1);
        let (scope, _) = rejected(call(&limiter, "2001:db8:1:2::9", start));
        assert_eq!(scope, Scope::Client);
        assert_eq!(call(&limiter, "2001:db8:1:3::1", start), Decision::Allow);
        let mapped = client_key("::ffff:192.0.2.1".parse().unwrap());
        assert_eq!(mapped, "192.0.2.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_seconds(Duration::ZERO), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_secs(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1_001)), 2);
    }
}
