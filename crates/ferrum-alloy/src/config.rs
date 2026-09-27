//! Typed, validated configuration with predictable precedence.
//!
//! Precedence, highest first:
//!
//! 1. explicit builder overrides (e.g. [`crate::AlloyApp::bind`]);
//! 2. `FERRUM_ALLOY_*` environment variables (see [`ENV_VARS`]);
//! 3. the TOML file named by the builder or `FERRUM_ALLOY_CONFIG`;
//! 4. defaults.
//!
//! Invalid supplied configuration is rejected: unknown keys, unknown
//! `FERRUM_ALLOY_*` variables, unparsable values, and unsafe combinations are
//! errors, never silent fallbacks. Every section parses regardless of which
//! Cargo features are compiled, so enabling a section whose feature is missing
//! is reported instead of ignored.
//!
//! Secrets are held in [`Secret`] and never printed.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ferrum_alloy_telemetry::init::LoggingConfig;
use ferrum_alloy_telemetry::{TelemetryConfig, TrustedPeersConfig};
use ipnet::IpNet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum configuration file size.
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Upper bound, in milliseconds, of `auth.jwt.jwks_max_age_ms` and
/// `auth.jwt.jwks_max_stale_ms`: 24 hours.
pub const MAX_JWKS_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

/// The environment variable naming the configuration file.
pub const CONFIG_FILE_ENV: &str = "FERRUM_ALLOY_CONFIG";

/// A secret value. `Debug`, `Display`, and `Serialize` never reveal it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. Callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Secret)
    }
}

/// Complete Alloy configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AlloyConfig {
    /// Service identity.
    pub service: ServiceConfig,
    /// Application listener and request limits.
    pub server: ServerConfig,
    /// Graceful shutdown budgets.
    pub shutdown: ShutdownConfig,
    /// Protected management listener.
    pub management: ManagementConfig,
    /// Health endpoints and readiness checks.
    pub health: HealthConfig,
    /// Log output.
    pub logging: LoggingConfig,
    /// Request telemetry (request ids, trace context, Server-Timing).
    pub telemetry: TelemetryConfig,
    /// OpenTelemetry trace export (feature `otel`).
    pub otlp: OtlpSettings,
    /// Transport identities and networks trusted for propagation metadata.
    pub trust: TrustedPeersConfig,
    /// Ferrum Edge integration (feature `edge`).
    pub edge: EdgeSettings,
    /// CORS (feature `cors`). Disabled unless configured.
    pub cors: CorsSettings,
    /// Response compression (feature `compression`). Disabled by default.
    pub compression: CompressionSettings,
    /// OpenAPI document serving (feature `openapi`).
    pub openapi: OpenApiSettings,
    /// PostgreSQL (feature `postgres`).
    pub database: DatabaseSettings,
    /// Authentication (feature `jwt`).
    pub auth: AuthSettings,
    /// Outbound HTTP client (feature `http-client`).
    pub http_client: HttpClientSettings,
}

/// Service identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServiceConfig {
    /// Service name. The builder's name is an explicit override.
    pub name: Option<String>,
    /// Service version.
    pub version: Option<String>,
    /// Deployment environment label.
    pub environment: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            name: None,
            version: None,
            environment: "development".to_owned(),
        }
    }
}

/// Application listener configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Listen address. Defaults to loopback; bind `0.0.0.0` explicitly in
    /// containers.
    pub bind: SocketAddr,
    /// Maximum request body size.
    pub request_body_limit_bytes: u64,
    /// Maximum number of request headers (HTTP/1.1).
    pub max_header_count: usize,
    /// Maximum request header bytes (HTTP/1.1 read buffer and HTTP/2 header list).
    pub max_header_bytes: usize,
    /// Maximum concurrent connections; further connections are closed.
    pub max_connections: usize,
    /// Maximum concurrent HTTP/2 streams per connection.
    pub http2_max_concurrent_streams: u32,
    /// Time allowed to receive a request head (slow-header protection).
    pub header_read_timeout_ms: u64,
    /// Time a connection may stay open with no request in flight after its
    /// first request, before it is closed (HTTP/2 `GOAWAY`). Behind a load
    /// balancer that pools connections, set it above the balancer's idle
    /// timeout.
    pub idle_timeout_ms: u64,
    /// Deadline for producing response *headers*. Never applied to response
    /// body streaming (SSE) or upgraded connections. `0` disables it.
    pub request_timeout_ms: u64,
    /// Maximum requests executing handlers at once; `0` means unlimited.
    pub max_in_flight_requests: usize,
    /// How long a request may wait for an admission permit; `0` rejects
    /// immediately when the limit is reached.
    pub admission_wait_timeout_ms: u64,
    /// TLS termination (feature `tls`).
    pub tls: Option<TlsSettings>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            request_body_limit_bytes: 2 * 1024 * 1024,
            max_header_count: 100,
            max_header_bytes: 64 * 1024,
            max_connections: 10_000,
            http2_max_concurrent_streams: 256,
            header_read_timeout_ms: 10_000,
            idle_timeout_ms: 60_000,
            request_timeout_ms: 30_000,
            max_in_flight_requests: 0,
            admission_wait_timeout_ms: 0,
            tls: None,
        }
    }
}

/// Client certificate policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuth {
    /// No client certificates.
    #[default]
    None,
    /// Verify a client certificate when one is presented.
    Optional,
    /// Require a verified client certificate.
    Required,
}

/// TLS listener settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSettings {
    /// PEM certificate chain.
    pub cert_path: PathBuf,
    /// PEM private key.
    pub key_path: PathBuf,
    /// PEM CA bundle for verifying client certificates.
    #[serde(default)]
    pub client_ca_path: Option<PathBuf>,
    /// Client certificate policy.
    #[serde(default)]
    pub client_auth: ClientAuth,
    /// TLS handshake timeout.
    #[serde(default = "default_handshake_timeout_ms")]
    pub handshake_timeout_ms: u64,
}

fn default_handshake_timeout_ms() -> u64 {
    10_000
}

/// Graceful shutdown budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShutdownConfig {
    /// After a shutdown signal, keep accepting while readiness reports
    /// `draining`, so load balancers stop routing first.
    pub readiness_grace_ms: u64,
    /// Time allowed for in-flight requests and streams after accepting stops.
    pub drain_timeout_ms: u64,
    /// Time allowed to flush telemetry after draining.
    pub telemetry_flush_timeout_ms: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            readiness_grace_ms: 0,
            drain_timeout_ms: 30_000,
            telemetry_flush_timeout_ms: 5_000,
        }
    }
}

/// Management listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ManagementConfig {
    /// Serve the management listener.
    pub enabled: bool,
    /// Listen address. A non-loopback address requires `token`.
    pub bind: SocketAddr,
    /// Bearer token for detailed health, metrics, and OpenAPI.
    pub token: Option<Secret>,
    /// Request rate limits.
    pub rate_limit: ManagementRateLimit,
}

impl Default for ManagementConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: SocketAddr::from(([127, 0, 0, 1], 9090)),
            token: None,
            rate_limit: ManagementRateLimit::default(),
        }
    }
}

/// Upper bound of `management.rate_limit.max_clients`.
pub const MAX_RATE_LIMIT_CLIENTS: usize = 65_536;

/// Bounds of `management.rate_limit.ipv6_prefix_len`.
pub const RATE_LIMIT_IPV6_PREFIX_LENS: std::ops::RangeInclusive<u8> = 48..=128;

/// Request rate limits of the management listener.
///
/// Probes (`/livez` and `/readyz`) and every other path have separate
/// budgets and client tables, so traffic to one never throttles the other.
/// Endpoint requests are charged to a token bucket of their client and one
/// of the whole listener; probes only to one of their client. A client is
/// the transport peer address (an IPv6 address by its `ipv6_prefix_len`
/// prefix), never a request header. Peers in `exempt_networks` are not
/// limited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ManagementRateLimit {
    /// Enforce the limits.
    pub enabled: bool,
    /// Sustained requests per second from one client, other than probes.
    pub requests_per_second: u32,
    /// Requests one client may send at once, other than probes.
    pub burst: u32,
    /// Sustained requests per second from all clients together, other than
    /// probes.
    pub global_requests_per_second: u32,
    /// Requests all clients together may send at once, other than probes.
    pub global_burst: u32,
    /// Sustained probe requests per second from one client.
    pub probe_requests_per_second: u32,
    /// Probe requests one client may send at once.
    pub probe_burst: u32,
    /// Clients tracked individually, per budget. When a table is full of
    /// clients still spending their budget, endpoint requests from further
    /// clients share one per-client budget and probes from further clients
    /// are served untracked.
    pub max_clients: usize,
    /// Leading bits of an IPv6 peer address that identify one client.
    pub ipv6_prefix_len: u8,
    /// Peer networks that bypass the limits entirely. Empty by default.
    pub exempt_networks: Vec<IpNet>,
}

impl Default for ManagementRateLimit {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_second: 10,
            burst: 20,
            global_requests_per_second: 100,
            global_burst: 200,
            probe_requests_per_second: 20,
            probe_burst: 40,
            max_clients: 1_024,
            ipv6_prefix_len: 64,
            exempt_networks: Vec::new(),
        }
    }
}

/// Health endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    /// Also serve minimal liveness/readiness on the application listener
    /// (for gateway health checks). They take precedence over app routes.
    pub app_endpoints: bool,
    /// Liveness path.
    pub liveness_path: String,
    /// Readiness path.
    pub readiness_path: String,
    /// How long a readiness result is reused.
    pub cache_ttl_ms: u64,
    /// Timeout for each readiness check.
    pub check_timeout_ms: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            app_endpoints: true,
            liveness_path: "/livez".to_owned(),
            readiness_path: "/readyz".to_owned(),
            cache_ttl_ms: 5_000,
            check_timeout_ms: 2_000,
        }
    }
}

/// OpenTelemetry OTLP/HTTP export settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OtlpSettings {
    /// Export traces.
    pub enabled: bool,
    /// Full traces URL; see `docs/configuration.md` for `OTEL_*` fallbacks.
    pub endpoint: Option<String>,
    /// Export timeout including retries.
    pub timeout_ms: u64,
    /// Retries after the first export attempt.
    pub max_export_retries: usize,
    /// Root sampling ratio.
    pub sampling_ratio: f64,
    /// Maximum queued spans.
    pub max_queue_spans: usize,
    /// Maximum estimated queued bytes.
    pub max_queue_bytes: usize,
    /// Maximum spans per export.
    pub max_export_batch: usize,
    /// Maximum bytes per export request.
    pub max_request_bytes: usize,
    /// Delay between scheduled exports.
    pub scheduled_delay_ms: u64,
}

impl Default for OtlpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            timeout_ms: 10_000,
            max_export_retries: 2,
            sampling_ratio: 1.0,
            max_queue_spans: 2_048,
            max_queue_bytes: 8 * 1024 * 1024,
            max_export_batch: 512,
            max_request_bytes: 4 * 1024 * 1024,
            scheduled_delay_ms: 1_000,
        }
    }
}

/// How the service relates to Ferrum Edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeMode {
    /// No gateway assumptions; direct requests work normally.
    #[default]
    Standalone,
    /// Use verified gateway metadata when present; accept direct requests.
    GatewayPreferred,
    /// Reject requests that do not arrive from a verified gateway identity.
    GatewayRequired,
}

/// Ferrum Edge integration settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EdgeSettings {
    /// Deployment mode.
    pub mode: EdgeMode,
    /// Accept `X-Consumer-Username` / `X-Consumer-Custom-Id` from a verified
    /// gateway identity (mTLS). Never accepted from network-boundary trust.
    pub accept_consumer_identity: bool,
}

/// CORS settings. Nothing is permitted unless listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CorsSettings {
    /// Enable CORS handling.
    pub enabled: bool,
    /// Exact allowed origins. `*` is allowed only without credentials.
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    pub allowed_methods: Vec<String>,
    /// Allowed request headers.
    pub allowed_headers: Vec<String>,
    /// Allow credentials.
    pub allow_credentials: bool,
    /// Preflight cache duration.
    pub max_age_seconds: Option<u64>,
}

/// Compression settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CompressionSettings {
    /// Enable response compression. Compressing responses that mix secrets
    /// with attacker-reflected input enables BREACH-style attacks; enable it
    /// only for payloads where that does not apply.
    pub enabled: bool,
    /// Minimum body size to compress.
    pub min_size_bytes: u64,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_size_bytes: 1_024,
        }
    }
}

/// OpenAPI serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OpenApiSettings {
    /// Serve the registered document on the management listener.
    pub serve: bool,
    /// Path of the JSON document.
    pub path: String,
    /// Also serve it unauthenticated on the application listener.
    pub public: bool,
}

impl Default for OpenApiSettings {
    fn default() -> Self {
        Self {
            serve: true,
            path: "/openapi.json".to_owned(),
            public: false,
        }
    }
}

/// PostgreSQL pool settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DatabaseSettings {
    /// Connection URL (secret).
    pub url: Option<Secret>,
    /// Maximum pool size.
    pub max_connections: u32,
    /// Minimum idle connections.
    pub min_connections: u32,
    /// Time to wait for a pooled connection.
    pub acquire_timeout_ms: u64,
    /// Close connections idle longer than this.
    pub idle_timeout_ms: Option<u64>,
    /// Recycle connections older than this.
    pub max_lifetime_ms: Option<u64>,
    /// Server-side `statement_timeout` applied to every connection.
    pub statement_timeout_ms: Option<u64>,
    /// Run embedded migrations at startup. Off by default; production
    /// should run migrations as a deliberate, separate step.
    pub migrate_on_startup: bool,
    /// Register a cached readiness check.
    pub readiness_check: bool,
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: 10,
            min_connections: 0,
            acquire_timeout_ms: 5_000,
            idle_timeout_ms: Some(600_000),
            max_lifetime_ms: Some(1_800_000),
            statement_timeout_ms: None,
            migrate_on_startup: false,
            readiness_check: true,
        }
    }
}

/// Authentication settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSettings {
    /// JWT bearer verification.
    pub jwt: Option<JwtSettings>,
}

/// JWT verification policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JwtSettings {
    /// Required `iss`.
    pub issuer: String,
    /// Accepted `aud` values (at least one).
    pub audiences: Vec<String>,
    /// Accepted algorithms, e.g. `["RS256"]`. `none` is never accepted.
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
    /// JWKS URL (https, or http to loopback only).
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// Minimum interval between JWKS refresh attempts, whatever triggers
    /// them (expiry or unknown `kid`). Also the lower bound of the key-set
    /// lifetime.
    #[serde(default = "default_jwks_refresh_ms")]
    pub jwks_min_refresh_interval_ms: u64,
    /// Maximum time a fetched key set is trusted before it must be
    /// revalidated, even for known `kid`s. A `Cache-Control: max-age` on the
    /// JWKS response shortens it, never extends it. At most
    /// [`MAX_JWKS_LIFETIME_MS`].
    #[serde(default = "default_jwks_max_age_ms")]
    pub jwks_max_age_ms: u64,
    /// How long an expired key set keeps verifying known `kid`s while it is
    /// revalidated or while refreshes fail. After that, verification fails
    /// closed with `503 auth-unavailable`. `0` fails closed as soon as the
    /// key set expires. At most [`MAX_JWKS_LIFETIME_MS`].
    #[serde(default = "default_jwks_max_stale_ms")]
    pub jwks_max_stale_ms: u64,
    /// Maximum JWKS response size.
    #[serde(default = "default_jwks_max_bytes")]
    pub jwks_max_bytes: usize,
    /// JWKS request timeout.
    #[serde(default = "default_jwks_timeout_ms")]
    pub jwks_timeout_ms: u64,
    /// Clock skew tolerance for `exp`/`nbf`.
    #[serde(default = "default_leeway")]
    pub leeway_seconds: u64,
}

fn default_algorithms() -> Vec<String> {
    vec!["RS256".to_owned()]
}
fn default_jwks_refresh_ms() -> u64 {
    60_000
}
fn default_jwks_max_age_ms() -> u64 {
    300_000
}
fn default_jwks_max_stale_ms() -> u64 {
    300_000
}
fn default_jwks_max_bytes() -> usize {
    256 * 1024
}
fn default_jwks_timeout_ms() -> u64 {
    5_000
}
fn default_leeway() -> u64 {
    30
}

/// Outbound HTTP client settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpClientSettings {
    /// Connection establishment timeout.
    pub connect_timeout_ms: u64,
    /// Whole-request timeout (until the response body is read).
    pub request_timeout_ms: u64,
    /// Maximum same-origin redirects to follow (cross-origin redirects are
    /// never followed automatically).
    pub max_redirects: usize,
    /// Hosts (exact, or `.suffix`) that receive `traceparent`. Nothing is
    /// propagated to other hosts.
    pub propagate_trace_context_to: Vec<String>,
}

impl Default for HttpClientSettings {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 2_000,
            request_timeout_ms: 10_000,
            max_redirects: 0,
            propagate_trace_context_to: Vec::new(),
        }
    }
}

/// What kind of value an environment variable holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvKind {
    /// Free text.
    Str,
    /// Unsigned integer.
    Uint,
    /// Floating point.
    Float,
    /// `true` / `false`.
    Bool,
    /// Comma-separated list.
    List,
    /// Secret text; `<NAME>_FILE` may name a file holding it.
    Secret,
}

/// One supported `FERRUM_ALLOY_*` variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvVar {
    /// Variable name.
    pub name: &'static str,
    /// Configuration path it sets.
    pub path: &'static [&'static str],
    /// Value kind.
    pub kind: EnvKind,
}

macro_rules! env_vars {
    ($( $name:literal => [$($seg:literal),+] : $kind:ident ),+ $(,)?) => {
        /// Every supported `FERRUM_ALLOY_*` variable. Any other variable with
        /// the prefix is rejected.
        pub const ENV_VARS: &[EnvVar] = &[
            $( EnvVar { name: $name, path: &[$($seg),+], kind: EnvKind::$kind } ),+
        ];
    };
}

env_vars! {
    "FERRUM_ALLOY_SERVICE_NAME" => ["service", "name"]: Str,
    "FERRUM_ALLOY_SERVICE_VERSION" => ["service", "version"]: Str,
    "FERRUM_ALLOY_ENVIRONMENT" => ["service", "environment"]: Str,
    "FERRUM_ALLOY_BIND" => ["server", "bind"]: Str,
    "FERRUM_ALLOY_REQUEST_BODY_LIMIT_BYTES" => ["server", "request_body_limit_bytes"]: Uint,
    "FERRUM_ALLOY_REQUEST_TIMEOUT_MS" => ["server", "request_timeout_ms"]: Uint,
    "FERRUM_ALLOY_HEADER_READ_TIMEOUT_MS" => ["server", "header_read_timeout_ms"]: Uint,
    "FERRUM_ALLOY_IDLE_TIMEOUT_MS" => ["server", "idle_timeout_ms"]: Uint,
    "FERRUM_ALLOY_MAX_CONNECTIONS" => ["server", "max_connections"]: Uint,
    "FERRUM_ALLOY_MAX_IN_FLIGHT_REQUESTS" => ["server", "max_in_flight_requests"]: Uint,
    "FERRUM_ALLOY_TLS_CERT_PATH" => ["server", "tls", "cert_path"]: Str,
    "FERRUM_ALLOY_TLS_KEY_PATH" => ["server", "tls", "key_path"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_CA_PATH" => ["server", "tls", "client_ca_path"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_AUTH" => ["server", "tls", "client_auth"]: Str,
    "FERRUM_ALLOY_SHUTDOWN_READINESS_GRACE_MS" => ["shutdown", "readiness_grace_ms"]: Uint,
    "FERRUM_ALLOY_SHUTDOWN_DRAIN_TIMEOUT_MS" => ["shutdown", "drain_timeout_ms"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_ENABLED" => ["management", "enabled"]: Bool,
    "FERRUM_ALLOY_MANAGEMENT_BIND" => ["management", "bind"]: Str,
    "FERRUM_ALLOY_MANAGEMENT_TOKEN" => ["management", "token"]: Secret,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_ENABLED" => ["management", "rate_limit", "enabled"]: Bool,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_REQUESTS_PER_SECOND" => ["management", "rate_limit", "requests_per_second"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_BURST" => ["management", "rate_limit", "burst"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_GLOBAL_REQUESTS_PER_SECOND" => ["management", "rate_limit", "global_requests_per_second"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_GLOBAL_BURST" => ["management", "rate_limit", "global_burst"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_PROBE_REQUESTS_PER_SECOND" => ["management", "rate_limit", "probe_requests_per_second"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_PROBE_BURST" => ["management", "rate_limit", "probe_burst"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_MAX_CLIENTS" => ["management", "rate_limit", "max_clients"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_IPV6_PREFIX_LEN" => ["management", "rate_limit", "ipv6_prefix_len"]: Uint,
    "FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_EXEMPT_NETWORKS" => ["management", "rate_limit", "exempt_networks"]: List,
    "FERRUM_ALLOY_LOG_FORMAT" => ["logging", "format"]: Str,
    "FERRUM_ALLOY_LOG_FILTER" => ["logging", "filter"]: Str,
    "FERRUM_ALLOY_OTLP_ENABLED" => ["otlp", "enabled"]: Bool,
    "FERRUM_ALLOY_OTLP_ENDPOINT" => ["otlp", "endpoint"]: Str,
    "FERRUM_ALLOY_OTLP_SAMPLING_RATIO" => ["otlp", "sampling_ratio"]: Float,
    "FERRUM_ALLOY_OTLP_TIMEOUT_MS" => ["otlp", "timeout_ms"]: Uint,
    "FERRUM_ALLOY_TRACE_CONTEXT_ACCEPT" => ["telemetry", "trace_context", "accept_incoming"]: Str,
    "FERRUM_ALLOY_SERVER_TIMING" => ["telemetry", "server_timing"]: Str,
    "FERRUM_ALLOY_TRUSTED_IDENTITIES" => ["trust", "identities"]: List,
    "FERRUM_ALLOY_TRUSTED_NETWORKS" => ["trust", "networks"]: List,
    "FERRUM_ALLOY_EDGE_MODE" => ["edge", "mode"]: Str,
    "FERRUM_ALLOY_DATABASE_URL" => ["database", "url"]: Secret,
    "FERRUM_ALLOY_DATABASE_MAX_CONNECTIONS" => ["database", "max_connections"]: Uint,
    "FERRUM_ALLOY_DATABASE_MIGRATE_ON_STARTUP" => ["database", "migrate_on_startup"]: Bool,
    "FERRUM_ALLOY_JWT_ISSUER" => ["auth", "jwt", "issuer"]: Str,
    "FERRUM_ALLOY_JWT_AUDIENCES" => ["auth", "jwt", "audiences"]: List,
    "FERRUM_ALLOY_JWT_JWKS_URL" => ["auth", "jwt", "jwks_url"]: Str,
    "FERRUM_ALLOY_JWT_JWKS_MAX_AGE_MS" => ["auth", "jwt", "jwks_max_age_ms"]: Uint,
    "FERRUM_ALLOY_JWT_JWKS_MAX_STALE_MS" => ["auth", "jwt", "jwks_max_stale_ms"]: Uint,
    "FERRUM_ALLOY_CORS_ALLOWED_ORIGINS" => ["cors", "allowed_origins"]: List,
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read configuration file {path}: {message}")]
    Read {
        /// File path.
        path: PathBuf,
        /// Reason.
        message: String,
    },
    /// The file is not valid TOML. The message never quotes the file's
    /// contents, which may hold secrets.
    #[error(
        "configuration file {path} is not valid TOML{}: {message}",
        location_suffix(.line, .column)
    )]
    Syntax {
        /// File path.
        path: PathBuf,
        /// 1-based line of the error, when known.
        line: Option<usize>,
        /// 1-based column (in characters) of the error, when known.
        column: Option<usize>,
        /// Parser message, without source excerpts.
        message: String,
    },
    /// An environment variable is unknown or invalid.
    #[error("environment variable {name}: {message}")]
    Env {
        /// Variable name.
        name: String,
        /// Reason.
        message: String,
    },
    /// The merged configuration does not match the schema. The message
    /// names keys and expected types, never the supplied values.
    #[error("invalid configuration: {0}")]
    Schema(String),
    /// Semantic validation failed.
    #[error("invalid configuration:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

fn location_suffix(line: &Option<usize>, column: &Option<usize>) -> String {
    match (*line, *column) {
        (Some(line), Some(column)) => format!(" at line {line}, column {column}"),
        (Some(line), None) => format!(" at line {line}"),
        _ => String::new(),
    }
}

/// 1-based line and column (in characters) of byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let before = bytes.get(..offset.min(bytes.len())).unwrap_or_default();
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1);
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let current = before.get(line_start..).unwrap_or_default();
    let column = String::from_utf8_lossy(current).chars().count() + 1;
    (line, column)
}

/// A TOML syntax error without the source excerpt that
/// `toml::de::Error`'s `Display` prints: the offending line may hold a
/// secret. Only the parser's own message and the location are kept.
fn syntax_error(path: &Path, text: &str, error: &toml::de::Error) -> ConfigError {
    let (line, column) = match error.span() {
        Some(span) => {
            let (line, column) = line_column(text, span.start);
            (Some(line), Some(column))
        }
        None => (None, None),
    };
    let message = error.message().lines().next().unwrap_or_default();
    ConfigError::Syntax {
        path: path.to_owned(),
        line,
        column,
        message: message.to_owned(),
    }
}

/// `true` for text that is safe to echo as a key name: a short bare TOML
/// key. Anything else (quoted keys with URLs, long tokens) is redacted,
/// including a bare key over 32 bytes that mixes letters and digits, which
/// looks more like a token than a key name.
fn is_plain_key(key: &str) -> bool {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let token_like = key.len() > 32
        && key.bytes().any(|b| b.is_ascii_alphabetic())
        && key.bytes().any(|b| b.is_ascii_digit());
    !key.is_empty() && key.len() <= 64 && !token_like && key.bytes().all(plain)
}

fn redact_segment(segment: &str) -> &str {
    if is_plain_key(segment) {
        segment
    } else {
        "<redacted>"
    }
}

/// Rewrites a serde/toml schema error so it names keys and expected types
/// but never a supplied value. serde's standard messages quote the value
/// (`invalid type: string "postgres://u:pw@db", expected ...`), and
/// `toml::de::Error`'s `Display` appends the key path.
fn schema_error(error: &toml::de::Error) -> ConfigError {
    let first = error.message().lines().next().unwrap_or_default();
    let mut message = redact_schema_message(first);
    // With no source input, `Display` ends with "in `a.b.c`" when the
    // failing key is known.
    let rendered = error.to_string();
    let path = rendered
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("in `"))
        .and_then(|rest| rest.strip_suffix('`'));
    if let Some(path) = path {
        let segments: Vec<&str> = path.split('.').map(redact_segment).collect();
        message.push_str(&format!(" (at `{}`)", segments.join(".")));
    }
    ConfigError::Schema(message)
}

/// The `", expected ..."` part of a serde message, or nothing. The value
/// comes first and the schema's expectation last, so the last separator is
/// the real one even when the value contains the same text.
fn expectation(message: &str) -> &str {
    match message.rfind(", expected ") {
        Some(at) => message.get(at..).unwrap_or_default(),
        None => "",
    }
}

fn redact_schema_message(message: &str) -> String {
    for kind in ["invalid type", "invalid value", "unknown variant"] {
        if message.starts_with(kind) {
            return format!("{kind} (value redacted){}", expectation(message));
        }
    }
    if let Some(rest) = message.strip_prefix("unknown field `") {
        let end = rest
            .rfind("`, expected ")
            .or_else(|| rest.rfind("`, there are no fields"));
        let (field, tail) = match end.and_then(|end| rest.split_at_checked(end)) {
            Some((field, tail)) => (field, tail.strip_prefix('`').unwrap_or(tail)),
            None => ("", ""),
        };
        return if is_plain_key(field) {
            format!("unknown field `{field}`{tail}")
        } else {
            format!("unknown field (key redacted){tail}")
        };
    }
    // Field names in these come from the schema, not from the input.
    for kind in ["missing field `", "duplicate field `", "invalid length "] {
        if message.starts_with(kind) {
            return message.to_owned();
        }
    }
    "a value does not match the expected type or format".to_owned()
}

/// Where configuration values came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigSources {
    /// The file that was read, if any.
    pub file: Option<PathBuf>,
    /// Environment variables that were applied.
    pub env: Vec<&'static str>,
    /// Builder override paths that were applied.
    pub overrides: Vec<String>,
}

/// Builder overrides (highest precedence).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    values: toml::Table,
    applied: Vec<String>,
}

impl Overrides {
    /// Sets `path` to `value`.
    pub fn set(&mut self, path: &[&str], value: toml::Value) {
        insert(&mut self.values, path, value);
        self.applied.push(path.join("."));
    }
}

fn insert(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = table;
    for segment in parents {
        let entry = current
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        let toml::Value::Table(next) = entry else {
            return;
        };
        current = next;
    }
    current.insert((*last).to_owned(), value);
}

fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Reads and parses a configuration file.
pub fn read_file(path: &Path) -> Result<toml::Table, ConfigError> {
    let read_error = |message: String| ConfigError::Read {
        path: path.to_owned(),
        message,
    };
    let metadata = std::fs::metadata(path).map_err(|e| read_error(e.to_string()))?;
    if !metadata.is_file() {
        return Err(read_error("not a regular file".into()));
    }
    if metadata.len() > MAX_CONFIG_FILE_BYTES {
        return Err(read_error(format!(
            "larger than {MAX_CONFIG_FILE_BYTES} bytes"
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|e| read_error(e.to_string()))?;
    toml::from_str(&text).map_err(|e| syntax_error(path, &text, &e))
}

/// Converts `FERRUM_ALLOY_*` variables into a TOML table.
pub fn env_table<I>(vars: I) -> Result<(toml::Table, Vec<&'static str>), ConfigError>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    let mut table = toml::Table::new();
    let mut applied = Vec::new();
    let mut seen: BTreeMap<&'static str, String> = BTreeMap::new();
    for (name, value) in vars {
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("FERRUM_ALLOY_") || name == CONFIG_FILE_ENV {
            continue;
        }
        let env_error = |message: String| ConfigError::Env {
            name: name.to_owned(),
            message,
        };
        let (spec, from_file) = match ENV_VARS.iter().find(|v| v.name == name) {
            Some(spec) => (spec, false),
            None => match name
                .strip_suffix("_FILE")
                .and_then(|base| ENV_VARS.iter().find(|v| v.name == base))
            {
                Some(spec) if spec.kind == EnvKind::Secret => (spec, true),
                _ => {
                    return Err(env_error(
                        "unknown FERRUM_ALLOY_* variable (see docs/configuration.md)".into(),
                    ));
                }
            },
        };
        if let Some(previous) = seen.insert(spec.name, name.to_owned()) {
            return Err(env_error(format!(
                "conflicts with {previous}; set only one"
            )));
        }
        let value = value
            .into_string()
            .map_err(|_| env_error("value is not valid UTF-8".into()))?;
        let value = if from_file {
            let text = std::fs::read_to_string(&value)
                .map_err(|e| env_error(format!("cannot read secret file: {e}")))?;
            text.trim_end_matches(['\n', '\r']).to_owned()
        } else {
            value
        };
        let parsed = match spec.kind {
            EnvKind::Str | EnvKind::Secret => toml::Value::String(value),
            EnvKind::Uint => toml::Value::Integer(
                value
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .and_then(|v| i64::try_from(v).ok())
                    .ok_or_else(|| env_error("expected a non-negative integer".into()))?,
            ),
            EnvKind::Float => toml::Value::Float(
                value
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| env_error("expected a finite number".into()))?,
            ),
            EnvKind::Bool => toml::Value::Boolean(match value.trim() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err(env_error("expected true or false".into())),
            }),
            EnvKind::List => toml::Value::Array(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| toml::Value::String(s.to_owned()))
                    .collect(),
            ),
        };
        insert(&mut table, spec.path, parsed);
        applied.push(spec.name);
    }
    applied.sort_unstable();
    Ok((table, applied))
}

/// Loads configuration from an optional file, an environment snapshot, and
/// overrides. Tests pass an explicit environment; applications use
/// [`AlloyConfig::load`].
pub fn load_from<I>(
    file: Option<&Path>,
    env: I,
    overrides: &Overrides,
) -> Result<(AlloyConfig, ConfigSources), ConfigError>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    let env: Vec<_> = env.into_iter().collect();
    let file_path = match file {
        Some(path) => Some(path.to_owned()),
        None => env
            .iter()
            .find(|(k, _)| k == CONFIG_FILE_ENV)
            .map(|(_, v)| {
                v.clone()
                    .into_string()
                    .map(PathBuf::from)
                    .map_err(|_| ConfigError::Env {
                        name: CONFIG_FILE_ENV.into(),
                        message: "value is not valid UTF-8".into(),
                    })
            })
            .transpose()?,
    };
    let mut merged = match &file_path {
        Some(path) => read_file(path)?,
        None => toml::Table::new(),
    };
    let (env_values, env_applied) = env_table(env)?;
    merge(&mut merged, env_values);
    merge(&mut merged, overrides.values.clone());
    let config: AlloyConfig = toml::Value::Table(merged)
        .try_into()
        .map_err(|e: toml::de::Error| schema_error(&e))?;
    let sources = ConfigSources {
        file: file_path,
        env: env_applied,
        overrides: overrides.applied.clone(),
    };
    Ok((config, sources))
}

/// A validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// `true` for errors that prevent startup.
    pub error: bool,
    /// Message.
    pub message: String,
}

impl AlloyConfig {
    /// Loads from the process environment.
    pub fn load(
        file: Option<&Path>,
        overrides: &Overrides,
    ) -> Result<(Self, ConfigSources), ConfigError> {
        load_from(file, std::env::vars_os(), overrides)
    }

    /// The effective configuration as TOML, with secrets redacted.
    pub fn redacted_toml(&self) -> String {
        toml::to_string_pretty(self)
            .unwrap_or_else(|e| format!("# cannot render configuration: {e}\n"))
    }

    /// Semantic validation. `features` names the Cargo features compiled
    /// into the running application (see [`crate::enabled_features`]).
    pub fn check(&self, features: &[&str]) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        let mut error = |message: String| {
            issues.push(ConfigIssue {
                error: true,
                message,
            })
        };
        let has = |feature: &str| features.contains(&feature);

        let server = &self.server;
        for (name, value) in [
            (
                "server.request_body_limit_bytes",
                server.request_body_limit_bytes,
            ),
            ("server.max_header_count", server.max_header_count as u64),
            ("server.max_connections", server.max_connections as u64),
            (
                "server.http2_max_concurrent_streams",
                u64::from(server.http2_max_concurrent_streams),
            ),
            (
                "server.header_read_timeout_ms",
                server.header_read_timeout_ms,
            ),
            ("server.idle_timeout_ms", server.idle_timeout_ms),
            ("shutdown.drain_timeout_ms", self.shutdown.drain_timeout_ms),
            ("health.cache_ttl_ms", self.health.cache_ttl_ms),
            ("health.check_timeout_ms", self.health.check_timeout_ms),
        ] {
            if value == 0 {
                error(format!("{name} must be greater than zero"));
            }
        }
        if server.max_header_bytes < 8_192 {
            error("server.max_header_bytes must be at least 8192".into());
        }
        if server.max_header_bytes > u32::MAX as usize {
            error("server.max_header_bytes is too large".into());
        }
        for (name, path) in [
            ("health.liveness_path", &self.health.liveness_path),
            ("health.readiness_path", &self.health.readiness_path),
            ("openapi.path", &self.openapi.path),
        ] {
            if !path.starts_with('/') || path.contains(['{', '}', '*', ' ']) {
                error(format!("{name} must be a literal path starting with '/'"));
            }
        }
        if self.health.liveness_path == self.health.readiness_path {
            error("health.liveness_path and health.readiness_path must differ".into());
        }

        if let Some(tls) = &server.tls {
            if !has("tls") {
                error("server.tls is configured but the `tls` feature is not enabled".into());
            }
            if tls.client_auth != ClientAuth::None && tls.client_ca_path.is_none() {
                error("server.tls.client_auth requires server.tls.client_ca_path".into());
            }
            if tls.client_auth == ClientAuth::None && tls.client_ca_path.is_some() {
                error("server.tls.client_ca_path is set but client_auth is none".into());
            }
        }

        let management = &self.management;
        if management.enabled {
            if !management.bind.ip().is_loopback() && management.token.is_none() {
                error(format!(
                    "management.bind {} is not loopback; set management.token (FERRUM_ALLOY_MANAGEMENT_TOKEN) or bind to loopback",
                    management.bind
                ));
            }
            if let Some(token) = &management.token
                && token.expose().len() < 32
            {
                error("management.token must be at least 32 characters".into());
            }
            if management.bind == server.bind {
                error("management.bind must differ from server.bind".into());
            }
            let rate = &management.rate_limit;
            if rate.enabled {
                for (name, value) in [
                    (
                        "management.rate_limit.requests_per_second",
                        rate.requests_per_second,
                    ),
                    ("management.rate_limit.burst", rate.burst),
                    (
                        "management.rate_limit.global_requests_per_second",
                        rate.global_requests_per_second,
                    ),
                    ("management.rate_limit.global_burst", rate.global_burst),
                    (
                        "management.rate_limit.probe_requests_per_second",
                        rate.probe_requests_per_second,
                    ),
                    ("management.rate_limit.probe_burst", rate.probe_burst),
                ] {
                    if value == 0 {
                        error(format!("{name} must be greater than zero"));
                    }
                }
                if rate.max_clients == 0 || rate.max_clients > MAX_RATE_LIMIT_CLIENTS {
                    error(format!(
                        "management.rate_limit.max_clients must be within 1..={MAX_RATE_LIMIT_CLIENTS}"
                    ));
                }
                // Clients admitted at once must fit, or a burst of newcomers
                // would push returning clients into the shared budget.
                let admitted_at_once = usize::try_from(rate.global_burst).unwrap_or(usize::MAX);
                if rate.max_clients < admitted_at_once {
                    error("management.rate_limit.max_clients must be at least management.rate_limit.global_burst".into());
                }
                if !RATE_LIMIT_IPV6_PREFIX_LENS.contains(&rate.ipv6_prefix_len) {
                    error(format!(
                        "management.rate_limit.ipv6_prefix_len must be within {}..={}",
                        RATE_LIMIT_IPV6_PREFIX_LENS.start(),
                        RATE_LIMIT_IPV6_PREFIX_LENS.end()
                    ));
                }
                if rate.exempt_networks.iter().any(|n| n.prefix_len() == 0) {
                    error("management.rate_limit.exempt_networks must not contain a network of every address; set management.rate_limit.enabled = false instead".into());
                }
                if rate.exempt_networks.iter().any(|network| {
                    matches!(
                        network,
                        IpNet::V6(v6)
                            if v6.prefix_len() >= 96 && v6.addr().to_ipv4_mapped().is_some()
                    )
                }) {
                    error(
                        "management.rate_limit.exempt_networks must use IPv4 CIDRs instead of IPv4-mapped IPv6 CIDRs"
                            .into(),
                    );
                }
            }
        }

        if self.otlp.enabled && !has("otel") {
            error("otlp.enabled is true but the `otel` feature is not enabled".into());
        }
        if !(0.0..=1.0).contains(&self.otlp.sampling_ratio) {
            error("otlp.sampling_ratio must be within 0.0..=1.0".into());
        }
        if self.edge.mode != EdgeMode::Standalone && !has("edge") {
            error("edge.mode requires the `edge` feature".into());
        }
        if self.edge.mode == EdgeMode::GatewayRequired && self.trust.identities.is_empty() {
            error("edge.mode = gateway_required needs at least one trust.identities entry (verified mTLS identity)".into());
        }
        if self.edge.accept_consumer_identity && self.trust.identities.is_empty() {
            error("edge.accept_consumer_identity requires trust.identities; network trust never authorizes identity headers".into());
        }
        if let Err(message) = ferrum_alloy_telemetry::TrustedPeers::new(&self.trust) {
            error(format!("trust: {message}"));
        }
        if self.cors.enabled {
            if !has("cors") {
                error("cors.enabled is true but the `cors` feature is not enabled".into());
            }
            if self.cors.allowed_origins.is_empty() {
                error("cors.allowed_origins must list origins when CORS is enabled".into());
            }
            if self.cors.allow_credentials && self.cors.allowed_origins.iter().any(|o| o == "*") {
                error("cors.allow_credentials cannot be combined with origin '*'".into());
            }
        }
        if self.compression.enabled && !has("compression") {
            error(
                "compression.enabled is true but the `compression` feature is not enabled".into(),
            );
        }
        if self.database.url.is_some() && !has("postgres") {
            error("database.url is set but the `postgres` feature is not enabled".into());
        }
        if self.database.max_connections == 0
            || self.database.min_connections > self.database.max_connections
        {
            error("database pool sizes are invalid (need 0 <= min_connections <= max_connections, max > 0)".into());
        }
        if let Some(jwt) = &self.auth.jwt {
            if !has("jwt") {
                error("auth.jwt is configured but the `jwt` feature is not enabled".into());
            }
            if jwt.issuer.is_empty() || jwt.audiences.is_empty() {
                error("auth.jwt requires issuer and at least one audience".into());
            }
            if jwt.algorithms.is_empty() {
                error("auth.jwt.algorithms must not be empty".into());
            }
            for algorithm in &jwt.algorithms {
                if ![
                    "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA",
                ]
                .contains(&algorithm.as_str())
                {
                    error(format!(
                        "auth.jwt.algorithms: {algorithm:?} is not an accepted asymmetric algorithm"
                    ));
                }
            }
            if jwt.jwks_max_age_ms == 0 {
                error("auth.jwt.jwks_max_age_ms must be greater than zero".into());
            }
            if jwt.jwks_max_age_ms > MAX_JWKS_LIFETIME_MS {
                error("auth.jwt.jwks_max_age_ms must not exceed 24 hours (86400000)".into());
            }
            if jwt.jwks_max_stale_ms > MAX_JWKS_LIFETIME_MS {
                error("auth.jwt.jwks_max_stale_ms must not exceed 24 hours (86400000)".into());
            }
            if jwt.jwks_min_refresh_interval_ms > jwt.jwks_max_age_ms {
                error(
                    "auth.jwt.jwks_min_refresh_interval_ms must not exceed auth.jwt.jwks_max_age_ms"
                        .into(),
                );
            }
            match &jwt.jwks_url {
                None => error("auth.jwt.jwks_url is required".into()),
                Some(url) => {
                    let loopback_http = url.starts_with("http://127.0.0.1")
                        || url.starts_with("http://localhost")
                        || url.starts_with("http://[::1]");
                    if !url.starts_with("https://") && !loopback_http {
                        error(
                            "auth.jwt.jwks_url must use https (http is allowed only for loopback)"
                                .into(),
                        );
                    }
                }
            }
        }

        let mut warn = |message: String| {
            issues.push(ConfigIssue {
                error: false,
                message,
            })
        };
        if self.telemetry.trace_context.accept_incoming == ferrum_alloy_telemetry::AcceptPolicy::Any
        {
            warn("telemetry.trace_context.accept_incoming = any lets every caller choose trace ids and force sampling".into());
        }
        if !server.bind.ip().is_loopback() {
            warn(format!(
                "server.bind {} accepts non-loopback connections",
                server.bind
            ));
        }
        if self.trust.identities.is_empty() && !self.trust.networks.is_empty() {
            warn("trust relies on network boundaries only; prefer verified mTLS identities for gateway metadata".into());
        }
        if self.database.migrate_on_startup {
            warn("database.migrate_on_startup runs migrations from every replica at startup; prefer a separate migration step in production".into());
        }
        issues
    }

    /// Returns an error when [`AlloyConfig::check`] reports errors.
    pub fn validate(&self, features: &[&str]) -> Result<Vec<ConfigIssue>, ConfigError> {
        let issues = self.check(features);
        let errors: Vec<String> = issues
            .iter()
            .filter(|i| i.error)
            .map(|i| i.message.clone())
            .collect();
        if errors.is_empty() {
            Ok(issues.into_iter().filter(|i| !i.error).collect())
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }
}
