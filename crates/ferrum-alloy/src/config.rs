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
//! `FERRUM_ALLOY_*` variables (other than the command's own,
//! [`CLI_ENV_VARS`]), unparsable values, and unsafe combinations are
//! errors, never silent fallbacks. Every section parses regardless of which
//! Cargo features are compiled, so enabling a section whose feature is missing
//! is reported instead of ignored.
//!
//! Secrets are held in [`Secret`] and never printed.
//!
//! The configuration structs are `#[non_exhaustive]`, so adding a setting is
//! not a breaking change. Outside this crate, start from `Default` (or
//! [`TlsSettings::new`] and [`JwtSettings::new`], whose sections have
//! required fields) and assign the fields to change. The enums are
//! `#[non_exhaustive]` too, so a `match` on them outside this crate needs a
//! wildcard arm.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ferrum_alloy_telemetry::init::LoggingConfig;
use ferrum_alloy_telemetry::{TelemetryConfig, TrustedPeersConfig};
use ipnet::IpNet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::files::read_regular_file_bounded;

/// Maximum configuration file size.
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Maximum size of a file named by a `FERRUM_ALLOY_*_FILE` variable. Secrets
/// are tokens and connection strings; 64 KiB leaves ample room.
pub const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

/// Upper bound, in milliseconds, of `auth.jwt.jwks_max_age_ms` and
/// `auth.jwt.jwks_max_stale_ms`: 24 hours.
pub const MAX_JWKS_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

/// The environment variable naming the configuration file.
pub const CONFIG_FILE_ENV: &str = "FERRUM_ALLOY_CONFIG";

/// The credential `ferrum-alloy diagnose --url` sends to a service's
/// diagnostic retrieval endpoint. A variable of the command, not of service
/// configuration (see [`CLI_ENV_VARS`]).
pub const DIAGNOSTICS_TOKEN_ENV: &str = "FERRUM_ALLOY_DIAGNOSTICS_TOKEN";

/// The CLI's Edge admin credential, authorized for `diagnostics:read` and
/// a namespace (`ns`). Service configuration never reads it.
pub const EDGE_DIAGNOSTICS_TOKEN_ENV: &str = "FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN";

/// `FERRUM_ALLOY_*` variables that belong to the `ferrum-alloy` command
/// rather than to service configuration. Loading configuration ignores them
/// instead of rejecting them as unknown, and never reads their values.
pub const CLI_ENV_VARS: &[&str] = &[DIAGNOSTICS_TOKEN_ENV, EDGE_DIAGNOSTICS_TOKEN_ENV];

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
#[non_exhaustive]
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
    /// Evidence retained for authorized diagnostic retrieval (feature
    /// `diagnostics`).
    pub diagnostics: DiagnosticsSettings,
}

/// Service identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// Time a connection may stay open with no request in flight and no
    /// response data written after its first request, before it is closed
    /// (HTTP/2 `GOAWAY`). Behind a load balancer that pools connections, set
    /// it above the balancer's idle timeout.
    pub idle_timeout_ms: u64,
    /// Time a connection may go without writing any response data while
    /// response data waits to be written (the peer withholds HTTP/2
    /// `WINDOW_UPDATE` or keeps a zero TCP receive window), before it is
    /// closed (HTTP/2 `GOAWAY`).
    pub write_stall_timeout_ms: u64,
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
            write_stall_timeout_ms: 60_000,
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
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// PEM or DER certificate revocation lists (CRLs) checked against
    /// client certificates. Empty disables revocation checking.
    #[serde(default)]
    pub client_crl_paths: Vec<PathBuf>,
    /// Which client certificates have their revocation status checked.
    #[serde(default)]
    pub client_crl_depth: CrlDepth,
    /// How a certificate is treated when no configured CRL covers it.
    #[serde(default)]
    pub client_crl_unknown_status: CrlUnknownStatus,
    /// Whether a CRL past its `nextUpdate` time is rejected.
    #[serde(default)]
    pub client_crl_expiration: CrlExpiration,
    /// How often the certificate chain, private key, client CA bundle, and
    /// CRLs are read again and, when they changed and validate, swapped in
    /// for new handshakes. `0` disables reloading.
    #[serde(default = "default_reload_interval_ms")]
    pub reload_interval_ms: u64,
}

impl TlsSettings {
    /// Settings for the given certificate chain and private key, with every
    /// other setting at its default, as when only `cert_path` and `key_path`
    /// are configured.
    pub fn new(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            cert_path: cert_path.into(),
            key_path: key_path.into(),
            client_ca_path: None,
            client_auth: ClientAuth::default(),
            handshake_timeout_ms: default_handshake_timeout_ms(),
            client_crl_paths: Vec::new(),
            client_crl_depth: CrlDepth::default(),
            client_crl_unknown_status: CrlUnknownStatus::default(),
            client_crl_expiration: CrlExpiration::default(),
            reload_interval_ms: default_reload_interval_ms(),
        }
    }
}

/// Which certificates of a client chain are checked against the CRLs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CrlDepth {
    /// The leaf and every intermediate. Trust anchors are never checked.
    #[default]
    Chain,
    /// Only the leaf.
    EndEntity,
}

/// Policy for a certificate whose revocation status no configured CRL
/// determines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CrlUnknownStatus {
    /// Refuse the handshake.
    #[default]
    Deny,
    /// Accept the certificate.
    Allow,
}

/// Policy for a CRL whose `nextUpdate` time has passed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CrlExpiration {
    /// Fail startup, and refuse handshakes once a loaded CRL expires.
    #[default]
    Enforce,
    /// Keep using the CRL.
    Ignore,
}

fn default_handshake_timeout_ms() -> u64 {
    10_000
}

fn default_reload_interval_ms() -> u64 {
    60_000
}

/// Accepted nonzero `server.tls.reload_interval_ms` values: 100 ms to one
/// day.
const TLS_RELOAD_INTERVAL_MS: std::ops::RangeInclusive<u64> = 100..=24 * 60 * 60 * 1000;

/// Graceful shutdown budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct ManagementConfig {
    /// Serve the management listener.
    pub enabled: bool,
    /// Listen address. A non-loopback address requires `token`.
    pub bind: SocketAddr,
    /// Bearer token required for detailed health, metrics, and management
    /// OpenAPI, even on loopback. When absent, these routes deny access;
    /// minimal liveness/readiness probes remain available.
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
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

impl fmt::Debug for OtlpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpSettings")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<configured>"))
            .field("timeout_ms", &self.timeout_ms)
            .field("max_export_retries", &self.max_export_retries)
            .field("sampling_ratio", &self.sampling_ratio)
            .field("max_queue_spans", &self.max_queue_spans)
            .field("max_queue_bytes", &self.max_queue_bytes)
            .field("max_export_batch", &self.max_export_batch)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("scheduled_delay_ms", &self.scheduled_delay_ms)
            .finish()
    }
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct OpenApiSettings {
    /// Serve the registered document on the management listener.
    pub serve: bool,
    /// Path of the JSON document.
    pub path: String,
    /// Also serve it unauthenticated on the application listener.
    pub public: bool,
    /// Serve the documentation UI (feature `openapi-ui`) wherever the
    /// document is served, under the same access policy.
    pub ui: bool,
    /// Path of the documentation UI page. Its assets are served beneath it.
    pub ui_path: String,
}

impl Default for OpenApiSettings {
    fn default() -> Self {
        Self {
            serve: true,
            path: "/openapi.json".to_owned(),
            public: false,
            ui: false,
            ui_path: "/docs".to_owned(),
        }
    }
}

/// PostgreSQL pool settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct AuthSettings {
    /// JWT bearer verification.
    pub jwt: Option<JwtSettings>,
}

/// JWT verification policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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

impl JwtSettings {
    /// A policy requiring `issuer` and accepting `audiences`, with every
    /// other setting at its default, as when only `issuer` and `audiences`
    /// are configured. `jwks_url` must still be set.
    pub fn new(issuer: impl Into<String>, audiences: Vec<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audiences,
            algorithms: default_algorithms(),
            jwks_url: None,
            jwks_min_refresh_interval_ms: default_jwks_refresh_ms(),
            jwks_max_age_ms: default_jwks_max_age_ms(),
            jwks_max_stale_ms: default_jwks_max_stale_ms(),
            jwks_max_bytes: default_jwks_max_bytes(),
            jwks_timeout_ms: default_jwks_timeout_ms(),
            leeway_seconds: default_leeway(),
        }
    }
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
#[non_exhaustive]
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

/// Upper bound of `diagnostics.max_records`.
pub const MAX_DIAGNOSTICS_RECORDS: usize = 65_536;

/// Bounds of `diagnostics.max_bytes`: 4 KiB to 64 MiB.
pub const DIAGNOSTICS_BYTES: std::ops::RangeInclusive<usize> = 4_096..=64 * 1024 * 1024;

/// Retention of request evidence for authorized diagnostic retrieval
/// (feature `diagnostics`).
///
/// It applies only when the application installs a
/// `diagnostics::DiagnosticsAuthorizer`; otherwise nothing is retained. The
/// evidence lives in memory in this process. When either bound would be
/// exceeded, a tenant evicts another tenant's oldest record only while that
/// tenant holds more than it, and otherwise its own oldest record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct DiagnosticsSettings {
    /// Most requests retained.
    pub max_records: usize,
    /// Most estimated bytes retained.
    pub max_bytes: usize,
}

impl Default for DiagnosticsSettings {
    fn default() -> Self {
        Self {
            max_records: 1_024,
            max_bytes: 1024 * 1024,
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
    "FERRUM_ALLOY_WRITE_STALL_TIMEOUT_MS" => ["server", "write_stall_timeout_ms"]: Uint,
    "FERRUM_ALLOY_MAX_CONNECTIONS" => ["server", "max_connections"]: Uint,
    "FERRUM_ALLOY_MAX_IN_FLIGHT_REQUESTS" => ["server", "max_in_flight_requests"]: Uint,
    "FERRUM_ALLOY_TLS_CERT_PATH" => ["server", "tls", "cert_path"]: Str,
    "FERRUM_ALLOY_TLS_KEY_PATH" => ["server", "tls", "key_path"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_CA_PATH" => ["server", "tls", "client_ca_path"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_AUTH" => ["server", "tls", "client_auth"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_CRL_PATHS" => ["server", "tls", "client_crl_paths"]: List,
    "FERRUM_ALLOY_TLS_CLIENT_CRL_DEPTH" => ["server", "tls", "client_crl_depth"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_CRL_UNKNOWN_STATUS" => ["server", "tls", "client_crl_unknown_status"]: Str,
    "FERRUM_ALLOY_TLS_CLIENT_CRL_EXPIRATION" => ["server", "tls", "client_crl_expiration"]: Str,
    "FERRUM_ALLOY_TLS_RELOAD_INTERVAL_MS" => ["server", "tls", "reload_interval_ms"]: Uint,
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
    "FERRUM_ALLOY_DIAGNOSTICS_MAX_RECORDS" => ["diagnostics", "max_records"]: Uint,
    "FERRUM_ALLOY_DIAGNOSTICS_MAX_BYTES" => ["diagnostics", "max_bytes"]: Uint,
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
/// including keys with at least 16 bytes mixing letters and digits, or any
/// key at least 33 bytes long, which look more like tokens than key names.
fn is_plain_key(key: &str) -> bool {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let token_like = key.len() >= 33
        || (key.len() >= 16
            && key.bytes().any(|b| b.is_ascii_alphabetic())
            && key.bytes().any(|b| b.is_ascii_digit()));
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
    let message = error.message();
    // With no source input, `Display` prints the message and then, when the
    // failing key is known, "in `a.b.c`". The message is stripped whole
    // rather than split into lines, because a supplied value or key may
    // itself hold a newline.
    let rendered = error.to_string();
    let path: Vec<&str> = rendered
        .strip_prefix(message)
        .and_then(|rest| rest.strip_prefix("\nin `"))
        .and_then(|rest| rest.strip_suffix("`\n"))
        .map(|path| path.split('.').collect())
        .unwrap_or_default();
    let mut redacted = redact_schema_message(message, &path);
    if !path.is_empty() {
        let segments: Vec<&str> = path.iter().copied().map(redact_segment).collect();
        redacted.push_str(&format!(" (at `{}`)", segments.join(".")));
    }
    ConfigError::Schema(redacted)
}

/// The `", expected ..."` part of an `invalid type` or `invalid value`
/// message, or nothing. serde quotes the value with escapes and puts the
/// schema's expectation last, so the last separator is the real one even
/// when the value contains the same text.
fn expectation(message: &str) -> &str {
    message
        .rfind(", expected ")
        .and_then(|at| message.get(at..))
        .filter(|tail| !tail.chars().any(char::is_control))
        .unwrap_or_default()
}

/// Stands in for a supplied variant or key when asking the schema what it
/// expects. It holds no backtick and no `, expected `.
const PROBE: &str = "?";

/// The schema's own text after an unknown variant at `path` (`field` false),
/// or after an unknown key in the table at `path` (`field` true), such as
/// `, expected one of ...` or `, there are no fields`. It comes from
/// deserializing a table that holds only [`PROBE`], so it never contains
/// supplied text.
fn schema_expectation(path: &[&str], field: bool) -> Option<String> {
    let mut table = toml::Table::new();
    let prefix = if field {
        let mut at = path.to_vec();
        at.push(PROBE);
        insert(&mut table, &at, toml::Value::Integer(0));
        format!("unknown field `{PROBE}`")
    } else {
        insert(&mut table, path, toml::Value::String(PROBE.to_owned()));
        format!("unknown variant `{PROBE}`")
    };
    let probed: Result<AlloyConfig, _> = toml::Value::Table(table).try_into();
    let error = probed.err()?;
    let tail = error.message().strip_prefix(&prefix)?;
    if tail.chars().any(char::is_control) {
        return None;
    }
    Some(tail.to_owned())
}

fn redact_schema_message(message: &str, path: &[&str]) -> String {
    for kind in ["invalid type", "invalid value"] {
        if message.starts_with(kind) {
            return format!("{kind} (value redacted){}", expectation(message));
        }
    }
    // serde repeats an unknown variant or key verbatim, and it may hold a
    // newline or `, expected `. Nothing after the prefix is kept; the valid
    // variants or keys are asked of the schema instead.
    if message.starts_with("unknown variant") {
        let tail = schema_expectation(path, false).unwrap_or_default();
        return format!("unknown variant (value redacted){tail}");
    }
    if let Some(rest) = message.strip_prefix("unknown field `") {
        let tail = schema_expectation(path, true);
        // With the schema's text known exactly, the key is what precedes it.
        let key = tail
            .as_deref()
            .and_then(|tail| rest.strip_suffix(tail)?.strip_suffix('`'));
        let tail = tail.as_deref().unwrap_or_default();
        return match key {
            Some(key) if is_plain_key(key) => format!("unknown field `{key}`{tail}"),
            _ => format!("unknown field (key redacted){tail}"),
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
    let bytes = read_regular_file_bounded(path, MAX_CONFIG_FILE_BYTES)
        .map_err(|e| read_error(e.to_string()))?;
    let text = String::from_utf8(bytes).map_err(|_| read_error("not valid UTF-8".into()))?;
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
        if !name.starts_with("FERRUM_ALLOY_")
            || name == CONFIG_FILE_ENV
            || CLI_ENV_VARS.contains(&name)
        {
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
            let bytes = read_regular_file_bounded(Path::new(&value), MAX_SECRET_FILE_BYTES)
                .map_err(|e| env_error(format!("cannot read secret file: {e}")))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| env_error("cannot read secret file: not valid UTF-8".into()))?;
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
        toml::to_string_pretty(&self.redacted())
            .unwrap_or_else(|e| format!("# cannot render configuration: {e}\n"))
    }

    /// Returns a copy suitable for display, with endpoint userinfo and
    /// query parameters redacted.
    pub fn redacted(&self) -> Self {
        let mut config = self.clone();
        if let Some(endpoint) = &mut config.otlp.endpoint {
            *endpoint = redact_endpoint(endpoint);
        }
        config
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
            (
                "server.write_stall_timeout_ms",
                server.write_stall_timeout_ms,
            ),
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
            if !is_literal_path(path) {
                error(format!(
                    "{name} must be a literal path starting with '/', without '{{', '}}', '*', spaces, or a segment starting with ':'"
                ));
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
            if tls.client_auth == ClientAuth::None && !tls.client_crl_paths.is_empty() {
                error("server.tls.client_crl_paths is set but client_auth is none".into());
            }
            let interval = tls.reload_interval_ms;
            if interval != 0 && !TLS_RELOAD_INTERVAL_MS.contains(&interval) {
                error(format!(
                    "server.tls.reload_interval_ms must be 0 (disabled) or between {} and {}",
                    TLS_RELOAD_INTERVAL_MS.start(),
                    TLS_RELOAD_INTERVAL_MS.end()
                ));
            }
            if tls.client_crl_paths.is_empty() {
                for (name, changed) in [
                    (
                        "server.tls.client_crl_depth",
                        tls.client_crl_depth != CrlDepth::default(),
                    ),
                    (
                        "server.tls.client_crl_unknown_status",
                        tls.client_crl_unknown_status != CrlUnknownStatus::default(),
                    ),
                    (
                        "server.tls.client_crl_expiration",
                        tls.client_crl_expiration != CrlExpiration::default(),
                    ),
                ] {
                    if changed {
                        error(format!("{name} has no effect without client_crl_paths"));
                    }
                }
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
        if self.openapi.ui {
            openapi_ui_issues(self, &mut error);
            if !has("openapi-ui") {
                error("openapi.ui is true but the `openapi-ui` feature is not enabled".into());
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

        let diagnostics = &self.diagnostics;
        if diagnostics.max_records == 0 || diagnostics.max_records > MAX_DIAGNOSTICS_RECORDS {
            error(format!(
                "diagnostics.max_records must be within 1..={MAX_DIAGNOSTICS_RECORDS}"
            ));
        }
        if !DIAGNOSTICS_BYTES.contains(&diagnostics.max_bytes) {
            error(format!(
                "diagnostics.max_bytes must be within {}..={}",
                DIAGNOSTICS_BYTES.start(),
                DIAGNOSTICS_BYTES.end()
            ));
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
        if self.telemetry.request_id.accept_incoming == ferrum_alloy_telemetry::AcceptPolicy::Any {
            warn("telemetry.request_id.accept_incoming = any lets every caller choose the request id its request is logged, traced, and retained for diagnostics under".into());
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
        if let Some(tls) = &server.tls
            && !tls.client_crl_paths.is_empty()
        {
            if tls.client_crl_unknown_status == CrlUnknownStatus::Allow {
                warn("server.tls.client_crl_unknown_status = allow accepts client certificates that no configured CRL covers".into());
            }
            if tls.client_crl_expiration == CrlExpiration::Ignore {
                warn("server.tls.client_crl_expiration = ignore keeps using CRLs past their nextUpdate time".into());
            }
        }
        if self.openapi.ui && self.openapi.public {
            warn("openapi.ui with openapi.public serves the documentation UI unauthenticated on the application listener; do not expose it publicly in production".into());
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

fn redact_endpoint(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return "<redacted>".to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let authority = authority.rfind('@').map_or_else(
        || authority.to_owned(),
        |at| format!("<redacted>@{}", &authority[at + 1..]),
    );
    let suffix = &rest[authority_end..];
    let query_start = suffix.find('?');
    let fragment_start = suffix.find('#');
    let safe_suffix_end = query_start
        .into_iter()
        .chain(fragment_start)
        .min()
        .unwrap_or(suffix.len());
    let mut redacted = format!("{scheme}://{authority}{}", &suffix[..safe_suffix_end]);
    if query_start.is_some() {
        redacted.push_str("?<redacted>");
    }
    redacted
}

/// Checks `openapi.ui_path`, which the documentation page embeds in its
/// markup and serves its assets beneath, so it must neither need escaping
/// nor shadow another route on either listener.
fn openapi_ui_issues(config: &AlloyConfig, error: &mut impl FnMut(String)) {
    let ui = config.openapi.ui_path.as_str();
    if !is_ui_path(ui) {
        error("openapi.ui_path must be a path like /docs: segments of letters, digits, '-', '.', '_', or '~', without a trailing '/'".into());
        return;
    }
    // `path` is the UI page or beneath it.
    let within = |path: &str| match path.strip_prefix(ui) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    };
    let mut served = vec![
        config.openapi.path.as_str(),
        config.health.liveness_path.as_str(),
        config.health.readiness_path.as_str(),
    ];
    served.extend(crate::management::FIXED_PATHS);
    if served.into_iter().any(within) {
        error("openapi.ui_path must not be or contain another served path (openapi.path, the health paths, or the management paths)".into());
    }
    if ui == "/diagnostics" || ui.starts_with("/diagnostics/") {
        error("openapi.ui_path must be outside /diagnostics".into());
    }
}

/// A path that axum routes literally. `{`, `}`, and `*` are capture syntax,
/// and axum 0.8 refuses a segment starting with `:` (its former capture
/// syntax) by panicking, so none of them may appear.
fn is_literal_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['{', '}', '*', ' '])
        && !path.split('/').any(|segment| segment.starts_with(':'))
}

/// An absolute path of non-empty segments other than `.` and `..`, made only
/// of unreserved URL characters.
fn is_ui_path(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|rest| {
        rest.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        })
    })
}
