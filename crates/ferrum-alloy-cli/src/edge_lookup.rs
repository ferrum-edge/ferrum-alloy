//! Authenticated Edge G01 lookup. No deserialized report or record can
//! construct [`AuthenticatedLookup`]; only a successful GET on the validated
//! admin channel does so. ADR 0009 defines the limited claim it supports.

use std::io::Read as _;
use std::net::IpAddr;
use std::time::Duration;

use ferrum_alloy_diagnostics::edge_record::{self, BoundRecord, ClientObservation};
use ferrum_alloy_diagnostics::model::{Confidence, Finding};
use reqwest::Url;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};

use crate::error::CliError;

const TOKEN_ENV: &str = ferrum_alloy::config::EDGE_DIAGNOSTICS_TOKEN_ENV;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Private authority: never serialized or populated from input flags.
pub(crate) struct AuthenticatedLookup {
    record: BoundRecord,
    credential: String,
}

impl AuthenticatedLookup {
    pub(crate) fn finding(&self) -> Finding {
        let mut finding = self.record.finding();
        finding
            .missing_evidence
            .retain(|v| v != "authenticated admin lookup");
        finding.confirm_with.clear();
        finding
            .does_not_prove
            .retain(|v| v != "The record alone does not authenticate its producer or transport.");
        finding.explanation.push_str(
            " This invocation fetched it from the explicitly selected admin listener over verified TLS or direct literal-loopback HTTP. Authentication applies only to this bound record, not the accompanying service report.",
        );
        if self.record.known_vocabulary() {
            finding.confidence = Confidence::Confirmed;
        }
        finding
    }

    /// Scrub reflected credentials even if another input repeats them.
    pub(crate) fn redact(&self, text: &str) -> String {
        text.replace(&self.credential, "[REDACTED]")
    }

    /// Redact JSON string values before serialization, preserving syntax even
    /// when a supplied credential coincides with a JSON keyword.
    pub(crate) fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.redact(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_json(value);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    self.redact_json(value);
                }
            }
            _ => {}
        }
    }
}

/// Validate the base and reference before constructing the request target.
/// HTTP must use a literal loopback address; DNS localhost is refused.
fn lookup_url(base: &str, reference: &str) -> Result<Url, CliError> {
    let invalid = |message: &str| CliError::Invalid(message.to_owned());
    if !ferrum_alloy_diagnostics::catalog::is_edge_diagnostic_ref(reference) {
        return Err(invalid("malformed Edge diagnostic reference"));
    }
    let mut url = Url::parse(base).map_err(|_| invalid("invalid --edge-admin-url"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("--edge-admin-url must not contain credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("--edge-admin-url must not contain a query or fragment"));
    }
    if url.host_str().is_none() || !matches!(url.scheme(), "https" | "http") {
        return Err(invalid("--edge-admin-url must use https or literal-loopback http"));
    }
    if url.scheme() == "http" {
        let host = url.host_str().unwrap_or_default();
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
            return Err(invalid("plain HTTP Edge lookup requires a literal loopback IP; use https"));
        }
        // URL parsing normalizes shorthand, octal and integer IPv4 hosts.
        // Require the original authority to contain the canonical literal.
        let authority = base
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let authority = authority.split('/').next().unwrap_or_default();
        let canonical = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let suffix = authority.strip_prefix(&canonical);
        if !suffix.is_some_and(|s| s.is_empty() || s.starts_with(':')) {
            return Err(invalid("plain HTTP Edge lookup requires a canonical literal loopback IP"));
        }
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{path}/diagnostics/v1/refs/{reference}"));
    Ok(url)
}

/// The token is read only from the explicit CLI environment variable. Its
/// scope and namespace are enforced by Edge, not by decoding unverified JWTs.
fn credential() -> Result<String, CliError> {
    let raw = std::env::var(TOKEN_ENV).map_err(|_| {
        CliError::Invalid(format!(
            "set {TOKEN_ENV} to a diagnostics:read credential with an ns claim"
        ))
    })?;
    let token = raw.trim();
    if token.is_empty()
        || token.len() > 16 * 1024
        || !token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return Err(CliError::Invalid("invalid Edge diagnostics credential".into()));
    }
    Ok(token.to_owned())
}

/// One bounded GET. Errors never include the URL, credential, headers, body,
/// server-supplied error text, or reqwest's request debug representation.
pub(crate) fn fetch(
    base: &str,
    observation: &ClientObservation,
    timeout: Duration,
) -> Result<AuthenticatedLookup, CliError> {
    let url = lookup_url(base, observation.reference())?;
    let credential = credential()?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {credential}"))
        .map_err(|_| CliError::Invalid("invalid Edge diagnostics credential".into()))?;
    authorization.set_sensitive(true);
    let timeout = timeout.min(TOTAL_TIMEOUT);
    let client = reqwest::blocking::Client::builder()
        .tls_backend_preconfigured(crate::live::tls(url.scheme() == "https")?)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .build()
        .map_err(|_| CliError::Io("could not initialize Edge lookup client".into()))?;
    let response = client
        .get(url)
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, authorization)
        .send()
        .map_err(|_| CliError::Io("Edge lookup failed or timed out".into()))?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => {
            return Err(CliError::Io(
                "Edge refused the diagnostics:read credential or namespace".into(),
            ));
        }
        404 => {
            return Err(CliError::Io(
                "Edge reference unresolved: unknown, expired, evicted, disabled, another namespace, or another replica".into(),
            ));
        }
        429 => return Err(CliError::Io("Edge lookup rate-limited; retry later".into())),
        status => {
            return Err(CliError::Io(format!(
                "Edge lookup answered HTTP {status}; expected 200"
            )));
        }
    }
    let mut body = Vec::new();
    response
        .take((edge_record::MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| CliError::Io("Edge record read failed or timed out".into()))?;
    let record = edge_record::bind_record(&body, observation)
        .map_err(|e| CliError::Invalid(e.to_string()))?;
    Ok(AuthenticatedLookup { record, credential })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const REF: &str = "fd1_3f9c2a7e5b1d4c8a9e0f6b2d7c4a1e5f";

    #[test]
    fn targets_are_validated_before_building_the_lookup_path() {
        for base in [
            "https://admin.example/base/",
            "http://127.0.0.1:9090",
            "http://[::1]:9090",
        ] {
            assert!(lookup_url(base, REF).is_ok(), "{base}");
        }
        for base in [
            "http://localhost:9090",
            "http://192.0.2.1:9090",
            "http://[::ffff:127.0.0.1]:9090",
            "http://127.1:9090",
            "http://2130706433:9090",
            "http://user:secret@127.0.0.1:9090",
            "https://admin.example/?secret=1",
            "https://admin.example/#secret",
            "ftp://127.0.0.1",
        ] {
            assert!(lookup_url(base, REF).is_err(), "{base}");
        }
        for reference in [
            "../metrics",
            "fd1_short",
            "fd1_x?credential=secret",
            "FD1_AAAAA",
        ] {
            assert!(lookup_url("http://127.0.0.1", reference).is_err());
        }
        let url = lookup_url("https://admin.example/base/", REF).unwrap();
        assert_eq!(url.path(), format!("/base/diagnostics/v1/refs/{REF}"));
    }
}
