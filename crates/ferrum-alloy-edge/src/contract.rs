//! Ferrum Edge wire contracts this adapter relies on.
//!
//! Every item is an EXISTING contract of the Ferrum Edge release named in
//! [`EDGE_RELEASE`], verified against its source. `docs/edge-contract-inventory.md`
//! records producers, consumers, trust rules, and tests for each. Nothing
//! here invents gateway headers: neither Edge v0.9.9 nor v0.9.8 sends a route
//! id, attempt number, or diagnostics header to backends.

/// The Ferrum Edge release these contracts were verified against.
pub const EDGE_RELEASE: &str = "v0.9.10";
/// Source commit of [`EDGE_RELEASE`].
pub const EDGE_SOURCE_COMMIT: &str = "ee040d5e3281fde424aa65f5b18004852c5b53b0";

/// Authenticated consumer username injected by Edge after authentication.
/// Edge strips client-supplied copies at admission: v0.9.9 drops the whole
/// `x-consumer-*` namespace (`src/proxy/headers.rs`
/// `is_consumer_assertion_header`), v0.9.8 only this name and
/// [`CONSUMER_CUSTOM_ID`]. Trust it only from a verified gateway identity.
pub const CONSUMER_USERNAME: &str = "x-consumer-username";
/// Mapped consumer `custom_id`, when the consumer has one.
pub const CONSUMER_CUSTOM_ID: &str = "x-consumer-custom-id";
/// Default header of Edge's `correlation_id` plugin (`header_name`).
pub const CORRELATION_ID: &str = "x-request-id";
/// Coarse gateway error token on gateway-generated 5xx responses.
pub const GATEWAY_ERROR: &str = "x-gateway-error";
/// `degraded` when Edge used its all-unhealthy fallback target.
pub const GATEWAY_UPSTREAM_STATUS: &str = "x-gateway-upstream-status";
/// Opaque gateway diagnostic reference (Edge v0.9.9, off by default with
/// `FERRUM_DIAGNOSTIC_REFS`), resolved through Edge's admin API. Edge strips a
/// backend copy. Alloy never emits it; diagnosis records one a client
/// observed (`ferrum_alloy_diagnostics::catalog::EDGE_DIAGNOSTIC_REF_HEADER`,
/// rule `alloy.r007`) but does not resolve it.
pub const DIAGNOSTIC_REF: &str = "x-ferrum-diagnostic-ref";

/// The closed `X-Gateway-Error` vocabulary (`src/retry.rs`), the same in Edge
/// v0.9.9 and v0.9.8. v0.9.8 added `request_timeout`; older releases send
/// only the first seven.
pub const GATEWAY_ERROR_TOKENS: &[&str] = &[
    "connection_failure",
    "backend_timeout",
    "backend_error",
    "circuit_breaker_open",
    "overload",
    "config_stale",
    "concurrency_limit",
    "request_timeout",
];

/// Edge `otel_tracing` span attributes Alloy's diagnostics interpret.
pub mod span_attributes {
    /// Handler entry until the transaction summary (body completion when streamed).
    pub const LATENCY_TOTAL_MS: &str = "gateway.latency.total_ms";
    /// Backend dispatch until response headers, across all attempts and
    /// backoff; always exported, `-1` when unknown.
    pub const LATENCY_BACKEND_TTFB_MS: &str = "gateway.latency.backend_ttfb_ms";
    /// Backend exchange including body; buffered responses only.
    pub const LATENCY_BACKEND_TOTAL_MS: &str = "gateway.latency.backend_total_ms";
    /// Plugin execution time.
    pub const PLUGIN_EXECUTION_MS: &str = "gateway.plugin_execution_ms";
    /// Typed gateway failure class.
    pub const ERROR_CLASS: &str = "gateway.error.class";
    /// Proxy id.
    pub const PROXY_ID: &str = "gateway.proxy.id";
}

/// Headers whose values Edge asserts about the authenticated caller. Direct
/// callers can send them too; they are meaningful only from a verified gateway.
pub const IDENTITY_HEADERS: &[&str] = &[CONSUMER_USERNAME, CONSUMER_CUSTOM_ID];

/// Upper bound Alloy accepts for an identity header value.
pub const MAX_IDENTITY_VALUE_BYTES: usize = 512;
