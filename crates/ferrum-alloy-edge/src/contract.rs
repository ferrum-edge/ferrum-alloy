//! Ferrum Edge wire contracts this adapter relies on.
//!
//! Every item is an EXISTING contract of the Ferrum Edge release named in
//! [`EDGE_RELEASE`], verified against its source. `docs/edge-contract-inventory.md`
//! records producers, consumers, trust rules, and tests for each. Nothing
//! here invents gateway headers: neither Edge v0.9.9 nor v0.9.8 sends a route
//! id, attempt number, or diagnostics header to backends.

/// The Ferrum Edge release these contracts were verified against.
pub const EDGE_RELEASE: &str = "v0.9.21";
/// Source commit of [`EDGE_RELEASE`].
pub const EDGE_SOURCE_COMMIT: &str = "f9915358c8032214f4034a17c28cdf72d14006c3";

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
/// rule `alloy.r007`), and its rules do not resolve it; the CLI's explicit
/// `diagnose --edge-admin-url` lookup does (ADR 0009).
pub const DIAGNOSTIC_REF: &str = "x-ferrum-diagnostic-ref";

/// The closed `X-Gateway-Error` vocabulary (`src/retry.rs`): nine tokens in
/// Edge v0.9.17, eight in v0.9.15 (the same eight as v0.9.9 and v0.9.8).
/// v0.9.8 added `request_timeout`, and older releases send only the first
/// seven. v0.9.17 added `loop_detected`, the pre-routing `X-Ferrum-Hops`
/// hop-limit refusal (HTTP 508, no backend contacted); an outer gateway
/// relaying that 508 reports `backend_error`, so the token alone does not
/// prove which hop refused.
pub const GATEWAY_ERROR_TOKENS: &[&str] = &[
    "connection_failure",
    "backend_timeout",
    "backend_error",
    "circuit_breaker_open",
    "overload",
    "config_stale",
    "concurrency_limit",
    "request_timeout",
    "loop_detected",
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

/// External authenticated identity (Edge v0.9.15 and later): the identity
/// or display claim of a caller that an external auth flow (for example
/// `jwks_auth`/OIDC) authenticated without mapping a Consumer. Edge sends it
/// instead of [`CONSUMER_USERNAME`], never with it, and strips client copies
/// (`src/proxy/headers.rs` `is_consumer_assertion_header`). Trust it only
/// from a verified gateway identity.
pub const AUTHENTICATED_IDENTITY: &str = "x-authenticated-identity";

/// The Consumer identity headers Edge asserts about a mapped Consumer.
/// [`GatewayContext`](crate::GatewayContext) exposes them only from a
/// verified gateway identity.
pub const IDENTITY_HEADERS: &[&str] = &[CONSUMER_USERNAME, CONSUMER_CUSTOM_ID];

/// Every exact request header name the pinned `gateway-headers` vocabulary
/// (`contracts/ferrum-contracts/vocabularies/gateway-headers.json`) gives the
/// role `gateway_assertion`, lowercase: values only Edge may write, which it
/// strips from clients before writing its own. Direct callers can send them
/// too, so Alloy removes every one unless the peer is a verified gateway
/// identity and `accept_consumer_identity` is on. The pairing test
/// `gateway_assertion_headers_match_the_pinned_vocabulary` fails when the
/// vocabulary gains, drops, or renames one.
pub const GATEWAY_ASSERTION_HEADERS: &[&str] = &[
    CONSUMER_USERNAME,
    CONSUMER_CUSTOM_ID,
    AUTHENTICATED_IDENTITY,
    "x-geo-country",
    "x-ferrum-hops",
    "x-ferrum-peer-spiffe-id",
    "x-ferrum-source-principal",
    "x-ferrum-source-namespace",
    "x-ferrum-source-service-account",
    "x-ferrum-request-principal",
];

/// The `gateway_assertion` name prefixes of the same vocabulary: Edge owns
/// the whole `x-consumer-*` namespace (`X-Consumer-Role`, `X-Consumer-Groups`,
/// and so on), not just the two names it writes today.
pub const GATEWAY_ASSERTION_HEADER_PREFIXES: &[&str] = &["x-consumer-"];

/// Request header prefixes the pinned vocabulary gives the role
/// `ferrum_owned` with direction `gateway_to_backend`: names Edge writes to
/// the backend itself (route path-parameter captures, `x-path-param-*`) and
/// strips from clients (`is_path_param_assertion_header`). Not assertions
/// about the caller, but just as forgeable by a direct caller, so the Edge
/// policy removes them with the assertions.
pub const GATEWAY_WRITTEN_HEADER_PREFIXES: &[&str] = &["x-path-param-"];

/// Whether `name` is a request header only Edge may write: a gateway
/// assertion ([`is_gateway_assertion_header`]) or under
/// [`GATEWAY_WRITTEN_HEADER_PREFIXES`], matched the same way.
pub fn is_gateway_written_header(name: &str) -> bool {
    is_gateway_assertion_header(name)
        || GATEWAY_WRITTEN_HEADER_PREFIXES
            .iter()
            .any(|prefix| folded_starts_with(name, prefix))
}

/// Whether `name` is a gateway-asserted request header
/// ([`GATEWAY_ASSERTION_HEADERS`] or under
/// [`GATEWAY_ASSERTION_HEADER_PREFIXES`]), matched the way Edge matches
/// them: ASCII case-insensitive, with `_` read as `-`, because CGI-style
/// frameworks fold `X_Geo_Country` and `X-Geo-Country` to the same variable.
pub fn is_gateway_assertion_header(name: &str) -> bool {
    GATEWAY_ASSERTION_HEADERS
        .iter()
        .any(|header| folded_eq(name, header))
        || GATEWAY_ASSERTION_HEADER_PREFIXES
            .iter()
            .any(|prefix| folded_starts_with(name, prefix))
}

/// One header-name byte, lowercased, with `_` read as `-`.
fn fold(byte: u8) -> u8 {
    match byte {
        b'_' => b'-',
        other => other.to_ascii_lowercase(),
    }
}

/// `name` equals the lowercase, dash-spelled `expected` once folded.
fn folded_eq(name: &str, expected: &str) -> bool {
    name.len() == expected.len() && folded_eq_bytes(name.as_bytes(), expected.as_bytes())
}

/// `name` starts with the lowercase, dash-spelled `prefix` once folded.
fn folded_starts_with(name: &str, prefix: &str) -> bool {
    name.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| folded_eq_bytes(head, prefix.as_bytes()))
}

/// Byte-wise [`folded_eq`]; callers ensure equal lengths.
fn folded_eq_bytes(bytes: &[u8], expected: &[u8]) -> bool {
    bytes
        .iter()
        .zip(expected)
        .all(|(&byte, &expected)| fold(byte) == expected)
}

/// Upper bound Alloy accepts for an identity header value.
pub const MAX_IDENTITY_VALUE_BYTES: usize = 512;
