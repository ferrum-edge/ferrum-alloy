//! The service manifest: a versioned, reviewable description of how a
//! service is published behind Ferrum Edge.
//!
//! **Status: EXISTING shared v1 at `contracts-edge-0.9.11`.** Root accepted the
//! unchanged wire freeze at qualified Alloy owner 81cbb410. Foundry #540 and
//! Nexus #519 qualify authenticated read-only previews of the historical r2 fields;
//! their immutable slices do not qualify this new pin adoption. Alloy is released
//! from Git only, starting with the tag `v0.1.0`.
//! `ferrum-alloy edge export` turns the manifest into Ferrum Edge file-mode
//! configuration or GitForgeOps resources using Edge's *existing*
//! file-mode schema; hosted CI validates exports against supported Edge
//! v0.9.21 and v0.9.20.
//! Edge does not consume the manifest. See `docs/shared-contract-qualification.md`;
//! current wire fields and validation bounds are unchanged.
//!
//! The optional `[agents]` section is read only by `ferrum-alloy openapi
//! export`, which stamps it into the OpenAPI document as Edge v0.9.9's
//! document-level `x-ferrum-mcp` extension (see [`crate::agents`]).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Manifest schema identifier.
pub const MANIFEST_SCHEMA: &str = "ferrum.service_manifest";
/// Supported major version.
pub const MANIFEST_MAJOR: u32 = 1;
/// Maximum resource ID length accepted by the supported Ferrum Edge releases,
/// in bytes.
pub const EDGE_RESOURCE_ID_MAX_LENGTH: usize = 254;
/// Largest backend timeout and health-probe timeout the supported Ferrum
/// Edge releases accept, in milliseconds (`src/config/types.rs`
/// `MAX_TIMEOUT_MS`, 24 hours).
pub const EDGE_MAX_TIMEOUT_MS: u64 = 86_400_000;
/// Largest active health-check interval Edge accepts, in seconds
/// (`MAX_HEALTH_CHECK_INTERVAL`).
pub const EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS: u64 = 3_600;
/// Longest `listen_path` Edge accepts, in bytes (`MAX_LISTEN_PATH_LENGTH`).
pub const EDGE_MAX_LISTEN_PATH_LENGTH: usize = 500;
/// Longest `backend_path` and `active.http_path` Edge accepts, in bytes
/// (`MAX_BACKEND_PATH_LENGTH`).
pub const EDGE_MAX_BACKEND_PATH_LENGTH: usize = 2_048;
/// Longest `backend_host` and `targets[].host` Edge accepts, in bytes
/// (`MAX_BACKEND_HOST_LENGTH`).
pub const EDGE_MAX_BACKEND_HOST_LENGTH: usize = 255;

/// A service manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ServiceManifest {
    /// Always [`MANIFEST_SCHEMA`].
    pub schema: String,
    /// `"1.0"`.
    pub schema_version: String,
    /// Service identity.
    pub service: ManifestService,
    /// Public API surface.
    pub api: ManifestApi,
    /// How the gateway reaches the service.
    pub upstream: ManifestUpstream,
    /// Health endpoint for gateway active checks.
    #[serde(default)]
    pub health: Option<ManifestHealth>,
    /// Gateway-side timeouts.
    #[serde(default)]
    pub timeouts: ManifestTimeouts,
    /// Gateway resources to generate.
    #[serde(default)]
    pub gateway: ManifestGateway,
    /// Declared authentication requirement. `edge export` refuses
    /// `gateway`, which it cannot generate yet.
    #[serde(default)]
    pub auth: ManifestAuth,
    /// AI-agent tools: the document-level `x-ferrum-mcp` that `openapi
    /// export` writes. Absent: the document is left as the code produced it.
    #[serde(default)]
    pub agents: Option<ManifestAgents>,
}

/// Service identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ManifestService {
    /// Service name (`[a-z0-9][a-z0-9-]{0,62}`).
    pub name: String,
    /// Version.
    #[serde(default)]
    pub version: Option<String>,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Public API surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ManifestApi {
    /// Gateway listen path prefix, e.g. `/orders`.
    pub public_path: String,
    /// Path prefix the service itself serves. With `strip_public_path`, a
    /// request to `/orders/1` reaches the service as `{service_base_path}1`;
    /// without it, as `{service_base_path}orders/1`. OpenAPI export removes
    /// that forwarded prefix from operation paths and refuses any outside it.
    #[serde(default = "default_base_path")]
    pub service_base_path: String,
    /// Remove `public_path` before forwarding.
    #[serde(default = "default_true")]
    pub strip_public_path: bool,
    /// Path to the exported OpenAPI document, relative to the manifest.
    #[serde(default)]
    pub openapi: Option<String>,
}

fn default_base_path() -> String {
    "/".to_owned()
}
fn default_true() -> bool {
    true
}

/// Upstream location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ManifestUpstream {
    /// Host the gateway connects to (no scheme).
    pub host: String,
    /// Port.
    pub port: u16,
    /// `http` or `https` (Edge `backend_scheme`).
    pub scheme: String,
    /// HTTP versions the service supports (`http1`, `http2`). Informational;
    /// Edge negotiates via ALPN.
    #[serde(default = "default_protocols")]
    pub protocols: Vec<String>,
    /// Client certificate Edge presents to the service
    /// (`backend_tls_client_cert_path`). Required for verified gateway
    /// identity.
    #[serde(default)]
    pub gateway_client_cert_path: Option<String>,
    /// Matching private key (`backend_tls_client_key_path`).
    #[serde(default)]
    pub gateway_client_key_path: Option<String>,
    /// CA bundle Edge uses to verify the service certificate
    /// (`backend_tls_server_ca_cert_path`).
    #[serde(default)]
    pub server_ca_path: Option<String>,
}

fn default_protocols() -> Vec<String> {
    vec!["http1".to_owned(), "http2".to_owned()]
}

/// Health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ManifestHealth {
    /// Readiness path on the application listener.
    pub path: String,
    /// Probe interval.
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    /// Probe timeout.
    #[serde(default = "default_probe_timeout")]
    pub timeout_ms: u64,
}

fn default_interval() -> u64 {
    10
}
fn default_probe_timeout() -> u64 {
    2_000
}

/// Gateway timeouts (Edge semantics: read timeout bounds header wait *and*
/// the idle gap between streamed frames; `0` disables it for long-lived
/// streams such as SSE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct ManifestTimeouts {
    /// `backend_connect_timeout_ms`.
    pub connect_ms: u64,
    /// `backend_read_timeout_ms`.
    pub read_ms: u64,
    /// `backend_write_timeout_ms`.
    pub write_ms: u64,
}

impl Default for ManifestTimeouts {
    fn default() -> Self {
        Self {
            connect_ms: 2_000,
            read_ms: 30_000,
            write_ms: 30_000,
        }
    }
}

/// Gateway resources to generate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct ManifestGateway {
    /// Edge namespace.
    pub namespace: String,
    /// Proxy id (defaults to the service name).
    pub proxy_id: Option<String>,
    /// Attach Edge's `correlation_id` plugin (`x-request-id`).
    pub correlation_id: bool,
    /// OTLP/HTTP traces URL Edge's `otel_tracing` plugin exports to. The
    /// plugin is attached either way, because only it removes a caller's
    /// `traceparent`/`tracestate`; without an endpoint it exports nothing
    /// and forwards no trace context.
    pub otel_endpoint: Option<String>,
    /// Edge root sampling ratio for new traces.
    pub otel_root_sampling_ratio: Option<f64>,
}

impl Default for ManifestGateway {
    fn default() -> Self {
        Self {
            namespace: "ferrum".to_owned(),
            proxy_id: None,
            correlation_id: true,
            otel_endpoint: None,
            otel_root_sampling_ratio: None,
        }
    }
}

/// Declared authentication requirement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct ManifestAuth {
    /// `none`, `gateway` (Edge authenticates), or `service` (the service
    /// verifies tokens itself). Export does not generate Edge
    /// authentication yet, so `edge export` refuses `gateway` rather than
    /// produce configuration that authenticates nobody.
    pub mode: Option<String>,
}

/// AI-agent tools published through Edge's OpenAPI to MCP bridge (shared v1).
///
/// Only operations whose handlers declare `expose: true` in their own
/// `x-ferrum-mcp` become tools; `openapi export` lists them in the extension's
/// `include`, so Edge's default of publishing every `GET` never applies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
pub struct ManifestAgents {
    /// Publish the exposed operations as MCP tools. Off by default; `false`
    /// exports `x-ferrum-mcp: false`.
    pub enabled: bool,
    /// MCP endpoint path (`x-ferrum-mcp.endpoint.path`), under
    /// `api.public_path`. Edge's default is `{public_path}/mcp`.
    pub endpoint_path: Option<String>,
    /// Tool-name prefix (`x-ferrum-mcp.namespace`, 1-64 characters of
    /// `A-Za-z0-9_-`). Defaults to `service.name`.
    pub namespace: Option<String>,
}

/// Manifest validation error.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid service manifest:\n  - {}", .0.join("\n  - "))]
#[non_exhaustive]
pub struct ManifestError(pub Vec<String>);

impl ManifestError {
    /// Every validation problem, one per entry.
    pub fn reasons(&self) -> &[String] {
        &self.0
    }
}

fn valid_id(value: &str) -> bool {
    // Edge proxy/upstream id rule: ^[a-zA-Z0-9][a-zA-Z0-9._-]*$, <= 254.
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= EDGE_RESOURCE_ID_MAX_LENGTH
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A literal absolute path: RFC 3986 `pchar`s and `/` only, minus `;`, `%`,
/// `~`, `*`, with no `//` or `.`/`..` segment. Anything else — control or
/// non-ASCII bytes, spaces, query/fragment delimiters, templates — is
/// refused by construction, not by enumerating bad characters.
///
/// `~` and `*` are refused because the shared `service-manifest` v1 schema
/// in ferrum-contracts refuses them in every manifest path, and Alloy's
/// rule must stay within that wire (ferrum-edge/ferrum-contracts#58).
///
/// For `api.public_path`, exported as the `listen_path`, this is Alloy's
/// canonical subset of what Ferrum Edge admits as written (`policy_path`
/// accepts more, including non-ASCII bytes and `|`): v0.9.9 refuses a `;`
/// path parameter unless the proxy sets `allow_path_parameters`, which
/// export never does, and both supported releases refuse percent-escapes,
/// backslashes, and dot segments. Export writes only plain ASCII `pchar`
/// paths, which every supported release admits unambiguously.
/// `api.service_base_path` and `health.path` use the same rule for
/// simplicity; it is stricter than Edge requires there.
fn valid_path(value: &str) -> bool {
    value.starts_with('/')
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'.'
                        | b'_'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'+'
                        | b','
                        | b'='
                        | b':'
                        | b'@'
                        | b'/'
                )
        })
        && !value.contains("//")
        && !value.split('/').any(|s| matches!(s, "." | ".."))
}

impl ServiceManifest {
    /// Parses TOML.
    pub fn from_toml(text: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            toml::from_str(text).map_err(|e| ManifestError(vec![e.to_string()]))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The proxy id.
    pub fn proxy_id(&self) -> &str {
        self.gateway
            .proxy_id
            .as_deref()
            .unwrap_or(&self.service.name)
    }

    /// Validates every field that feeds generated gateway configuration,
    /// against both bounds of the supported Ferrum Edge releases' own
    /// validation (`Proxy`, `Upstream`, and active health-check rules in
    /// `src/config/types.rs`), so an accepted manifest exports configuration
    /// Edge loads.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut errors = Vec::new();
        if self.schema != MANIFEST_SCHEMA {
            errors.push(format!("schema must be {MANIFEST_SCHEMA:?}"));
        }
        let major = self
            .schema_version
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        if major != Some(MANIFEST_MAJOR) {
            errors.push(format!(
                "unsupported schema_version {:?}",
                self.schema_version
            ));
        }
        let name = &self.service.name;
        if name.is_empty()
            || name.len() > 63
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || name.starts_with('-')
        {
            errors.push("service.name must match [a-z0-9][a-z0-9-]{0,62}".into());
        }
        if !valid_id(self.proxy_id()) {
            errors.push("gateway.proxy_id must match ^[a-zA-Z0-9][a-zA-Z0-9._-]*$".into());
        }
        let proxy_id = self.proxy_id();
        let longest_derived_id = [
            self.health
                .as_ref()
                .map(|_| format!("{proxy_id}{}", crate::export::UPSTREAM_ID_SUFFIX)),
            self.gateway.correlation_id.then(|| {
                format!(
                    "{proxy_id}{}",
                    crate::export::CORRELATION_ID_PLUGIN_ID_SUFFIX
                )
            }),
            // Always generated: it neutralizes caller trace context.
            Some(format!(
                "{proxy_id}{}",
                crate::export::OTEL_TRACING_PLUGIN_ID_SUFFIX
            )),
        ]
        .into_iter()
        .flatten()
        .max_by_key(String::len);
        if let Some(derived_id) = longest_derived_id
            && derived_id.len() > EDGE_RESOURCE_ID_MAX_LENGTH
        {
            errors.push(format!(
                "gateway.proxy_id produces derived resource ID {derived_id:?} over Edge's {EDGE_RESOURCE_ID_MAX_LENGTH}-character limit"
            ));
        }
        if !valid_id(&self.gateway.namespace) {
            errors.push("gateway.namespace is invalid".into());
        }
        if !valid_path(&self.api.public_path) {
            errors.push("api.public_path must be a literal absolute path".into());
        }
        if self.api.public_path.len() > EDGE_MAX_LISTEN_PATH_LENGTH {
            errors.push(format!(
                "api.public_path must not exceed {EDGE_MAX_LISTEN_PATH_LENGTH} bytes (Ferrum Edge's listen_path limit)"
            ));
        }
        if !valid_path(&self.api.service_base_path) || !self.api.service_base_path.ends_with('/') {
            errors
                .push("api.service_base_path must be a literal absolute path ending in '/'".into());
        }
        // Export writes the path without its trailing `/` as `backend_path`,
        // and Edge bounds that value.
        if self.api.service_base_path.trim_end_matches('/').len() > EDGE_MAX_BACKEND_PATH_LENGTH {
            errors.push(format!(
                "api.service_base_path without its trailing '/' must not exceed {EDGE_MAX_BACKEND_PATH_LENGTH} bytes (Ferrum Edge's backend_path limit)"
            ));
        }
        let host = &self.upstream.host;
        if host.is_empty() || host.contains("://") || host.contains(['/', ' ', '@']) {
            errors.push("upstream.host must be a bare host name or address".into());
        }
        if host.chars().any(|c| c.is_control() || c.is_whitespace()) {
            errors.push("upstream.host must not contain control or whitespace characters".into());
        }
        if host.len() > EDGE_MAX_BACKEND_HOST_LENGTH {
            errors.push(format!(
                "upstream.host must not exceed {EDGE_MAX_BACKEND_HOST_LENGTH} bytes (Ferrum Edge's backend host limit)"
            ));
        }
        if self.upstream.port == 0 {
            errors.push("upstream.port must be greater than zero".into());
        }
        if !matches!(self.upstream.scheme.as_str(), "http" | "https") {
            errors.push("upstream.scheme must be http or https".into());
        }
        let protocols: BTreeSet<&str> =
            self.upstream.protocols.iter().map(String::as_str).collect();
        if protocols.is_empty() || !protocols.iter().all(|p| matches!(*p, "http1" | "http2")) {
            errors.push("upstream.protocols may contain only http1 and http2 (service-side HTTP/3 is not supported)".into());
        }
        let cert = self.upstream.gateway_client_cert_path.is_some();
        let key = self.upstream.gateway_client_key_path.is_some();
        if cert != key {
            errors.push("upstream.gateway_client_cert_path and gateway_client_key_path must be set together".into());
        }
        if (cert || self.upstream.server_ca_path.is_some()) && self.upstream.scheme != "https" {
            errors.push("TLS paths require upstream.scheme = https".into());
        }
        if let Some(health) = &self.health {
            if !valid_path(&health.path) {
                errors.push("health.path must be a literal absolute path".into());
            }
            if health.path.len() > EDGE_MAX_BACKEND_PATH_LENGTH {
                errors.push(format!(
                    "health.path must not exceed {EDGE_MAX_BACKEND_PATH_LENGTH} bytes (Ferrum Edge's active.http_path limit)"
                ));
            }
            if health.interval_seconds == 0 || health.timeout_ms == 0 {
                errors.push("health interval and timeout must be greater than zero".into());
            }
            if health.interval_seconds > EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS {
                errors.push(format!(
                    "health.interval_seconds must not exceed {EDGE_MAX_HEALTH_CHECK_INTERVAL_SECONDS} (Ferrum Edge's limit)"
                ));
            }
            if health.timeout_ms > EDGE_MAX_TIMEOUT_MS {
                errors.push(format!(
                    "health.timeout_ms must not exceed {EDGE_MAX_TIMEOUT_MS} (Ferrum Edge's limit)"
                ));
            }
        }
        if self.timeouts.connect_ms == 0 || self.timeouts.write_ms == 0 {
            errors.push("timeouts.connect_ms and write_ms must be greater than zero".into());
        }
        for (field, value) in [
            ("connect_ms", self.timeouts.connect_ms),
            ("read_ms", self.timeouts.read_ms),
            ("write_ms", self.timeouts.write_ms),
        ] {
            if value > EDGE_MAX_TIMEOUT_MS {
                errors.push(format!(
                    "timeouts.{field} must not exceed {EDGE_MAX_TIMEOUT_MS} (Ferrum Edge's limit)"
                ));
            }
        }
        if let Some(endpoint) = &self.gateway.otel_endpoint
            && let Err(reason) = otel_endpoint_issue(endpoint)
        {
            errors.push(reason.into());
        }
        if let Some(ratio) = self.gateway.otel_root_sampling_ratio
            && !(0.0..=1.0).contains(&ratio)
        {
            errors.push("gateway.otel_root_sampling_ratio must be within 0.0..=1.0".into());
        }
        if let Some(mode) = &self.auth.mode
            && !matches!(mode.as_str(), "none" | "gateway" | "service")
        {
            errors.push("auth.mode must be none, gateway, or service".into());
        }
        if let Some(agents) = &self.agents {
            if let Some(endpoint) = &agents.endpoint_path {
                let prefix = format!("{}/", self.api.public_path.trim_end_matches('/'));
                if !valid_path(endpoint) {
                    errors.push("agents.endpoint_path must be a literal absolute path".into());
                } else if !endpoint.starts_with(&prefix) || endpoint.len() == prefix.len() {
                    errors.push("agents.endpoint_path must be below api.public_path".into());
                }
            }
            if let Some(namespace) = &agents.namespace
                && !crate::agents::is_valid_namespace(namespace)
            {
                errors.push("agents.namespace must be 1-64 characters of A-Za-z0-9_-".into());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestError(errors))
        }
    }
}

/// Why Ferrum Edge would refuse `gateway.otel_endpoint` as `otel_tracing`'s
/// OTLP `endpoint`, if it would. Edge then omits the plugin, and with it the
/// strip of caller-supplied trace context (#319).
///
/// This ports Edge main's `validate_endpoint_for_provider` with
/// `allow_plaintext` unset, which the export never writes: the endpoint must
/// parse as an `http` or `https` URL with a host and no user information,
/// and an `http://` one must name a loopback IP literal as Edge's
/// `is_loopback_ip_literal_endpoint` reads it. Alloy is stricter in a few
/// places, so nothing it accepts is refused by Edge: control and whitespace
/// characters (which the URL parser would strip or remove) are refused, as
/// are an `@` anywhere in the authority, a backslash anywhere (in the
/// authority or path the parser reads it as `/`, so the spelling and the
/// parsed URL would disagree; a query or fragment one is refused too, for
/// one simple rule), and port 0. An upper-case scheme and an `@` before the
/// first `/` (in a query or fragment too) are refused because the shared
/// service-manifest v1 schema refuses them, so every endpoint Alloy accepts
/// is valid under v1.
fn otel_endpoint_issue(endpoint: &str) -> Result<(), &'static str> {
    const NOT_A_URL: &str = "gateway.otel_endpoint must be an http(s) URL with a host, a valid port, and no credentials";
    if endpoint
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("gateway.otel_endpoint must not contain control or whitespace characters");
    }
    if endpoint.contains('\\') {
        return Err("gateway.otel_endpoint must not contain a backslash");
    }
    let url = url::Url::parse(endpoint).map_err(|_| NOT_A_URL)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(NOT_A_URL);
    }
    let authority = endpoint
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port() == Some(0)
    {
        return Err(NOT_A_URL);
    }
    // The shared service-manifest v1 schema spells the scheme in lower case
    // and admits no `@` before the first `/`, a query or fragment included.
    // Refusing both keeps every endpoint Alloy accepts valid under v1, so
    // other v1 readers (Foundry and Nexus previews) accept the manifest too.
    if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
        return Err("gateway.otel_endpoint must start with a lower-case http:// or https://");
    }
    let before_path = endpoint
        .split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .unwrap_or_default();
    if before_path.contains('@') {
        return Err("gateway.otel_endpoint must not contain @ before the first /");
    }
    match url.host() {
        Some(url::Host::Domain(host)) if !host.is_empty() => {}
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => {}
        _ => return Err(NOT_A_URL),
    }
    if url.scheme() == "http" && !is_loopback_ip_literal_endpoint(endpoint, url.host()) {
        return Err(
            "gateway.otel_endpoint must use https:// unless its host is a loopback IP literal (127.d.d.d or [::1]): Ferrum Edge v0.9.19 and later drop a cleartext exporter to any other host",
        );
    }
    Ok(())
}

/// Ferrum Edge main's `is_loopback_ip_literal_endpoint`
/// (`src/plugins/utils/socket_host.rs`): the configured `http://` endpoint
/// spells its host as a dotted-quad `127.d.d.d` or `[::1]`, optionally with
/// a numeric port, and the parsed host is loopback. Shortened or expanded
/// spellings (`127.1`, `[0:0:0:0:0:0:0:1]`) and every name, `localhost`
/// included, do not qualify. Edge v0.9.19 and later admit a cleartext
/// exporter only to such a host without the `allow_plaintext` opt-in, which
/// the export never writes (v0.9.17 refused it as an unknown key).
fn is_loopback_ip_literal_endpoint(endpoint: &str, parsed_host: Option<url::Host<&str>>) -> bool {
    let parsed_is_loopback = match parsed_host {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if !parsed_is_loopback {
        return false;
    }
    let Some(rest) = endpoint
        .get(.."http://".len())
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .and_then(|_| endpoint.get("http://".len()..))
    else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end).unwrap_or_default();
    let host = if let Some(after) = authority.strip_prefix("[::1]") {
        if !after.is_empty() && !after.starts_with(':') {
            return false;
        }
        return after
            .strip_prefix(':')
            .is_none_or(|port| port.bytes().all(|byte| byte.is_ascii_digit()));
    } else {
        match authority.split_once(':') {
            Some((host, port)) => {
                if !port.bytes().all(|byte| byte.is_ascii_digit()) {
                    return false;
                }
                host
            }
            None => authority,
        }
    };
    let mut octets = host.split('.');
    octets.next() == Some("127")
        && octets.clone().count() == 3
        && octets.all(|octet| {
            (1..=3).contains(&octet.len()) && octet.bytes().all(|byte| byte.is_ascii_digit())
        })
}
