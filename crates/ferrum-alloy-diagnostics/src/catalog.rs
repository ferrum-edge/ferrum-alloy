//! Known observation names and their lifecycle boundaries.
//!
//! This catalog mirrors `docs/measurement-semantics.md`. Rules only interpret
//! names listed here; an unknown name is preserved and reported but never used
//! as evidence.

/// A catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Observation name.
    pub name: &'static str,
    /// Start boundary.
    pub start: &'static str,
    /// End boundary.
    pub end: &'static str,
    /// One-line meaning, including what the measurement does not cover.
    pub meaning: &'static str,
}

/// Alloy: middleware entry until the service produced response headers.
pub const ALLOY_TIME_TO_HEADERS: &str = "alloy.server.time_to_headers";
/// Alloy: response headers produced until the body ended, failed, or was dropped.
pub const ALLOY_BODY_DURATION: &str = "alloy.server.body_duration";
/// Alloy: middleware entry until body finalization.
pub const ALLOY_SERVER_DURATION: &str = "alloy.server.duration";
/// Alloy: an explicitly instrumented application operation.
pub const ALLOY_OPERATION_DURATION: &str = "alloy.operation.duration";
/// Alloy: time waiting for a database pool connection.
pub const ALLOY_DB_POOL_WAIT: &str = "alloy.db.pool_wait";
/// Alloy: time waiting for an admission permit.
pub const ALLOY_ADMISSION_WAIT: &str = "alloy.admission.wait";
/// Edge: request received until the transaction summary (body completion for streamed responses).
pub const EDGE_REQUEST_TOTAL: &str = "edge.request.total";
/// Edge: first backend dispatch until response headers, across all attempts and backoff.
pub const EDGE_BACKEND_TIME_TO_HEADERS: &str = "edge.backend.time_to_headers";
/// Edge: backend exchange including the full body (buffered responses only).
pub const EDGE_BACKEND_TOTAL: &str = "edge.backend.total";
/// Edge: time spent executing plugins.
pub const EDGE_PLUGIN_EXECUTION: &str = "edge.plugin_execution";
/// Edge event: the gateway rejected the request (with `phase` when known).
pub const EDGE_REQUEST_REJECTED: &str = "edge.request.rejected";
/// Edge event: the gateway classified a failure (`error_class`, `token`).
pub const EDGE_GATEWAY_ERROR: &str = "edge.gateway_error";
/// Edge event: the final response status the gateway returned.
pub const EDGE_RESPONSE: &str = "edge.response";
/// Alloy event: the service received and answered a request.
pub const ALLOY_RESPONSE: &str = "alloy.response";
/// Client event: a response header value the client observed (e.g. `X-Gateway-Error`).
pub const CLIENT_RESPONSE_HEADER: &str = "client.response_header";

/// Every catalog entry.
pub const ENTRIES: &[CatalogEntry] = &[
    CatalogEntry {
        name: ALLOY_TIME_TO_HEADERS,
        start: "alloy.middleware_entry",
        end: "alloy.response_headers_produced",
        meaning: "Alloy telemetry middleware entry until the inner service returned response headers to Hyper. Excludes accept, TLS handshake, and request-head parsing before the service was called. Its interval ends at the server span start plus this duration, never at the span end, which is body finalization.",
    },
    CatalogEntry {
        name: ALLOY_BODY_DURATION,
        start: "alloy.response_headers_produced",
        end: "alloy.response_body_finalized",
        meaning: "Response headers produced until the body yielded its last frame to Hyper, failed, or was dropped. Not proof that the client received the bytes.",
    },
    CatalogEntry {
        name: ALLOY_SERVER_DURATION,
        start: "alloy.middleware_entry",
        end: "alloy.response_body_finalized",
        meaning: "Whole service-observed request lifetime inside Alloy middleware.",
    },
    CatalogEntry {
        name: ALLOY_OPERATION_DURATION,
        start: "alloy.operation_start",
        end: "alloy.operation_end",
        meaning: "Application-observed duration of an explicitly instrumented operation. For database calls this is client-observed call time, not database server execution time.",
    },
    CatalogEntry {
        name: ALLOY_DB_POOL_WAIT,
        start: "alloy.pool_acquire_start",
        end: "alloy.pool_acquire_end",
        meaning: "Time waiting to acquire a pooled database connection.",
    },
    CatalogEntry {
        name: ALLOY_ADMISSION_WAIT,
        start: "alloy.admission_start",
        end: "alloy.admission_granted",
        meaning: "Time waiting for an Alloy admission permit when the admission limiter is enabled.",
    },
    CatalogEntry {
        name: EDGE_REQUEST_TOTAL,
        start: "edge.request_received",
        end: "edge.transaction_summary",
        meaning: "Ferrum Edge latency_total_ms: handler entry until the transaction summary; refreshed at body completion for streamed responses.",
    },
    CatalogEntry {
        name: EDGE_BACKEND_TIME_TO_HEADERS,
        start: "edge.backend_dispatch_start",
        end: "edge.backend_response_headers",
        meaning: "Ferrum Edge latency_backend_ttfb_ms: spans every retry attempt and backoff; for buffered responses it equals the full backend exchange including the body.",
    },
    CatalogEntry {
        name: EDGE_BACKEND_TOTAL,
        start: "edge.backend_dispatch_start",
        end: "edge.backend_body_buffered",
        meaning: "Ferrum Edge latency_backend_total_ms: only reported for buffered responses.",
    },
    CatalogEntry {
        name: EDGE_PLUGIN_EXECUTION,
        start: "edge.plugin_phases",
        end: "edge.plugin_phases",
        meaning: "Ferrum Edge latency_plugin_execution_ms: cumulative plugin execution time.",
    },
];

/// Event names rules understand.
pub const EVENTS: &[&str] = &[
    EDGE_REQUEST_REJECTED,
    EDGE_GATEWAY_ERROR,
    EDGE_RESPONSE,
    ALLOY_RESPONSE,
    CLIENT_RESPONSE_HEADER,
];

/// Looks up a measurement entry.
pub fn entry(name: &str) -> Option<&'static CatalogEntry> {
    ENTRIES.iter().find(|entry| entry.name == name)
}

/// Returns `true` when `name` is a known measurement or event.
pub fn is_known(name: &str) -> bool {
    entry(name).is_some() || EVENTS.contains(&name)
}

/// Ferrum Edge `rejection_phase` values that run before any upstream attempt
/// (Edge `docs/plugin_execution_order.md`; the same set in v0.9.7 and v0.9.8, except the
/// HTTP/3-only `route_request_timeout_h3_upload` added in v0.9.8, which is not yet mapped).
pub const EDGE_PRE_UPSTREAM_PHASES: &[&str] = &[
    "allowed_methods",
    "on_request_received",
    "authenticate",
    "authorize",
    "before_proxy",
    "on_backend_path_resolved",
    "on_final_request_body",
    "finalized_request_egress",
    "circuit_breaker_open",
    "max_forwards",
    "grpc_deadline_preflight",
    "backend_max_connections",
    "websocket_connection_limit",
    "adaptive_concurrency",
];

/// The closed `X-Gateway-Error` token vocabulary of Ferrum Edge v0.9.8
/// (`src/retry.rs`), each token with Alloy's user-facing explanation, which
/// rule `alloy.r007` renders into findings. v0.9.8 added `request_timeout`.
///
/// A header names no Edge version, so each explanation holds for every
/// supported release and is never narrower than an earlier release's meaning:
/// in v0.9.7, `backend_timeout` also covered route deadlines that expired
/// before any backend held the request. The release-specific meanings in the
/// pinned `contracts/ferrum-contracts/vocabularies/gateway-errors.json` are
/// narrower and use Edge-internal terms, so they are never rendered. The
/// pairing tests in `ferrum-alloy-edge` check that both token sets agree.
pub const EDGE_GATEWAY_ERROR_TOKENS: &[(&str, &str)] = &[
    (
        "connection_failure",
        "The gateway could not set up a connection to the configured backend (DNS, TCP, TLS, or pool).",
    ),
    (
        "backend_timeout",
        "A gateway backend or route deadline elapsed.",
    ),
    (
        "backend_error",
        "The backend exchange failed, or the gateway refused locally after dispatch was considered.",
    ),
    (
        "circuit_breaker_open",
        "The gateway's circuit breaker for this backend was open.",
    ),
    (
        "overload",
        "The gateway shed load, was draining, or hit an output ceiling.",
    ),
    (
        "config_stale",
        "The gateway data plane's configuration fence was stale.",
    ),
    (
        "concurrency_limit",
        "An adaptive or static gateway concurrency limit rejected the request.",
    ),
    (
        "request_timeout",
        "A gateway route's total request deadline expired before any backend held the request.",
    ),
];
