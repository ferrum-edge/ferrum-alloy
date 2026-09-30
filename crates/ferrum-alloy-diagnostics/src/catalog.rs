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
/// (`src/retry.rs`), with the only meaning each token supports. v0.9.8 added
/// `request_timeout`. A header carries no Edge version. Edge v0.9.7 also used
/// `backend_timeout` for route deadlines that expired before a backend held
/// the request, so readers keep that older meaning in mind.
pub const EDGE_GATEWAY_ERROR_TOKENS: &[(&str, &str)] = &[
    (
        "connection_failure",
        "Pre-wire connect, DNS or TLS failure: the gateway could not set up a connection to the backend. Also the token for every ErrorClass whose request_reached_wire is false.",
    ),
    (
        "backend_timeout",
        "A backend held the request (it accepted the connection and was sent the request) but did not answer in time. Never used for a timeout no backend held.",
    ),
    (
        "backend_error",
        "The backend returned a 5xx, or a post-wire 5xx had no more specific token. Also the metric label for an unclassified backend 5xx. Never used for a response that did not reach a backend.",
    ),
    (
        "circuit_breaker_open",
        "The circuit breaker for the backend was open; the request never reached a backend.",
    ),
    (
        "overload",
        "Gateway resource refusal: overload or drain reject_new_requests (503), or response-transformer output above the configured response ceiling (502).",
    ),
    (
        "config_stale",
        "Data-plane stale-config fence.",
    ),
    (
        "concurrency_limit",
        "adaptive_concurrency admission refused the request.",
    ),
    (
        "request_timeout",
        "A matched route rule's total request deadline (mesh_route_dispatch request_timeout_ms, Gateway API timeouts.request) expired before any backend held the request: during the client upload, a gateway-local phase, admission, or retry backoff. New in v0.9.8 (#5762).",
    ),
];
