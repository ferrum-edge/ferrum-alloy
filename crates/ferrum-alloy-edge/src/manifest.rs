//! The service manifest: a versioned, reviewable description of how a
//! service is published behind Ferrum Edge.
//!
//! **Status: PROPOSED contract.** No Ferrum product consumes this manifest
//! yet. `ferrum-alloy edge export` turns it into Ferrum Edge file-mode
//! configuration or GitForgeOps resources using Edge's *existing* schema
//! (verified against Edge v0.9.9 and v0.9.8). Nexus and Foundry consumption is future
//! work; field names may change before any consumer implements them.
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

/// A service manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Declared authentication requirement (informational).
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
pub struct ManifestApi {
    /// Gateway listen path prefix, e.g. `/orders`.
    pub public_path: String,
    /// Path prefix the service itself serves. With `strip_public_path`, a
    /// request to `/orders/1` reaches the service as `{service_base_path}1`;
    /// OpenAPI export removes this prefix from matching operation paths.
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
pub struct ManifestGateway {
    /// Edge namespace.
    pub namespace: String,
    /// Proxy id (defaults to the service name).
    pub proxy_id: Option<String>,
    /// Attach Edge's `correlation_id` plugin (`x-request-id`).
    pub correlation_id: bool,
    /// Attach Edge's `otel_tracing` plugin exporting to this OTLP/HTTP
    /// traces URL.
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
pub struct ManifestAuth {
    /// `none`, `gateway` (Edge authenticates), or `service` (the service
    /// verifies tokens itself).
    pub mode: Option<String>,
}

/// AI-agent tools published through Edge's OpenAPI to MCP bridge (PROPOSED).
///
/// Only operations whose handlers declare `expose: true` in their own
/// `x-ferrum-mcp` become tools; `openapi export` lists them in the extension's
/// `include`, so Edge's default of publishing every `GET` never applies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
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
pub struct ManifestError(pub Vec<String>);

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

/// A literal absolute path with no template, query, fragment, `;`, `%`, `\`,
/// `//`, or `.`/`..` segment.
///
/// For `api.public_path`, exported as the `listen_path`, this is what Ferrum
/// Edge admits as written (`policy_path` canonical form): v0.9.9 refuses a `;`
/// path parameter unless the proxy sets `allow_path_parameters`, which export
/// never does, and both supported releases refuse percent-escapes,
/// backslashes, and dot segments. `api.service_base_path` and `health.path`
/// use the same rule for simplicity; it is stricter than Edge requires there.
fn valid_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.contains(['{', '}', '*', '~', ' ', '?', '#', ';', '%', '\\'])
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

    /// Validates every field that feeds generated gateway configuration.
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
            self.gateway
                .otel_endpoint
                .as_ref()
                .map(|_| format!("{proxy_id}{}", crate::export::OTEL_TRACING_PLUGIN_ID_SUFFIX)),
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
        if !valid_path(&self.api.service_base_path) || !self.api.service_base_path.ends_with('/') {
            errors
                .push("api.service_base_path must be a literal absolute path ending in '/'".into());
        }
        if self.upstream.host.is_empty()
            || self.upstream.host.contains("://")
            || self.upstream.host.contains(['/', ' ', '@'])
        {
            errors.push("upstream.host must be a bare host name or address".into());
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
            if health.interval_seconds == 0 || health.timeout_ms == 0 {
                errors.push("health interval and timeout must be greater than zero".into());
            }
        }
        if self.timeouts.connect_ms == 0 || self.timeouts.write_ms == 0 {
            errors.push("timeouts.connect_ms and write_ms must be greater than zero".into());
        }
        if let Some(endpoint) = &self.gateway.otel_endpoint {
            let authority = endpoint
                .split_once("://")
                .map(|(_, rest)| rest.split('/').next().unwrap_or_default());
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                || authority.is_none_or(|a| a.is_empty() || a.contains('@'))
            {
                errors.push(
                    "gateway.otel_endpoint must be an http(s) URL without credentials".into(),
                );
            }
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
