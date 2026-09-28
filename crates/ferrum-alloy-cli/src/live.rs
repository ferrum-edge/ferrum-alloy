//! `ferrum-alloy diagnose --url`: fetches one live report from a running
//! service's management listener (ADR 0008).
//!
//! It runs only when invoked with `--url`. It sends one `GET` directly to
//! the service (no proxy), follows no redirects, bounds the connection time,
//! the whole request, and the response size, and writes nothing anywhere,
//! gateways included. The credential comes from
//! `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or a file, never from an argument, and
//! goes over plain `http` only to a loopback address.

use std::io::Read as _;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ferrum_alloy::telemetry::RequestId;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::{ACCEPT, RETRY_AFTER};

use crate::error::CliError;
use crate::input::read_regular_file_bounded;

/// Environment variable holding the credential for `--url`. Service
/// configuration knows it as the command's own and ignores it.
pub(crate) const TOKEN_ENV: &str = ferrum_alloy::config::DIAGNOSTICS_TOKEN_ENV;

/// Largest credential file read.
const MAX_TOKEN_FILE_BYTES: u64 = 16 * 1024;

/// The service answers every refusal with the same `404`.
const NOT_FOUND: &str =
    "no report for this request id and credential: unknown, evicted, or another tenant's";

/// Longest time to set up the connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The credential: read from `token_file` when given, otherwise from
/// [`TOKEN_ENV`]. Errors never repeat it.
pub(crate) fn token(token_file: Option<&Path>) -> Result<Option<String>, CliError> {
    let raw = match token_file {
        Some(path) => {
            let bytes = read_regular_file_bounded(path, MAX_TOKEN_FILE_BYTES)
                .map_err(|e| e.invalid(path, MAX_TOKEN_FILE_BYTES))?;
            String::from_utf8(bytes)
                .map_err(|_| CliError::Invalid(format!("{} is not UTF-8", path.display())))?
        }
        None => match std::env::var(TOKEN_ENV) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(CliError::Invalid(format!("{TOKEN_ENV} is not UTF-8")));
            }
        },
    };
    let token = raw.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(CliError::Invalid(
            "the diagnostics credential must be non-empty printable ASCII without spaces".into(),
        ));
    }
    Ok(Some(token.to_owned()))
}

/// Whether `url` names this host by a loopback address or `localhost`.
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The report URL of `request_id` under the management listener at `base`.
pub(crate) fn report_url(base: &str, request_id: &str, has_token: bool) -> Result<Url, CliError> {
    let mut url = Url::parse(base).map_err(|e| CliError::Invalid(format!("--url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CliError::Invalid("--url must use http or https".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CliError::Invalid(format!(
            "--url must not contain credentials; use {TOKEN_ENV} or --token-file"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(CliError::Invalid(
            "--url must not have a query or fragment".into(),
        ));
    }
    if has_token && url.scheme() == "http" && !is_loopback(&url) {
        return Err(CliError::Invalid(
            "the credential is sent over plain http only to a loopback address; use https".into(),
        ));
    }
    if RequestId::parse(request_id).is_none() {
        return Err(CliError::Invalid(
            "--request-id must be 1 to 256 characters of [A-Za-z0-9._-]".into(),
        ));
    }
    let base_path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{base_path}/diagnostics/v1/requests/{request_id}"));
    Ok(url)
}

/// TLS for `https`: rustls with the `ring` provider passed explicitly and the
/// platform trust store. Nothing is installed globally.
fn tls(require_roots: bool) -> Result<rustls::ClientConfig, CliError> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| CliError::Io(format!("TLS configuration: {e}")))?;
    match builder.clone().with_platform_verifier() {
        Ok(builder) => Ok(builder.with_no_client_auth()),
        Err(error) if require_roots => Err(CliError::Io(format!(
            "--url uses https but the platform trust store is unavailable: {error}"
        ))),
        Err(_) => Ok(builder
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth()),
    }
}

/// Fetches the report at `url`: its body when the service answers `200`,
/// at most `max_bytes` long.
pub(crate) fn fetch(
    url: Url,
    token: Option<&str>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Vec<u8>, CliError> {
    let client = reqwest::blocking::Client::builder()
        .tls_backend_preconfigured(tls(url.scheme() == "https")?)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| CliError::Io(format!("HTTP client: {e}")))?;
    let mut request = client.get(url).header(ACCEPT, "application/json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|e| CliError::Io(format!("fetching the report failed: {e}")))?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => return Err(CliError::Io(NOT_FOUND.into())),
        StatusCode::TOO_MANY_REQUESTS => {
            let retry = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 6 && value.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or("a few");
            return Err(CliError::Io(format!(
                "the service rate-limited the request; retry after {retry} seconds"
            )));
        }
        status => {
            return Err(CliError::Io(format!(
                "the service answered {status}; expected 200 with a report"
            )));
        }
    }
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut body = Vec::new();
    response
        .take(limit)
        .read_to_end(&mut body)
        .map_err(|e| CliError::Io(format!("reading the report failed: {e}")))?;
    if body.len() > max_bytes {
        return Err(CliError::Invalid(format!(
            "the report is larger than {max_bytes} bytes"
        )));
    }
    Ok(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn url(base: &str, token: bool) -> Result<String, String> {
        report_url(base, "req-1", token)
            .map(String::from)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn report_urls_extend_the_management_base() {
        let expected = "http://127.0.0.1:9090/diagnostics/v1/requests/req-1";
        assert_eq!(url("http://127.0.0.1:9090", true).unwrap(), expected);
        assert_eq!(url("http://127.0.0.1:9090/", true).unwrap(), expected);
        let proxied = url("https://ops.example/svc/", true).unwrap();
        let expected = "https://ops.example/svc/diagnostics/v1/requests/req-1";
        assert_eq!(proxied, expected);
        assert!(url("http://localhost:9090", true).is_ok());
        assert!(url("http://[::1]:9090", true).is_ok());
    }

    #[test]
    fn unsafe_urls_and_ids_are_refused() {
        let error = url("http://10.0.0.8:9090", true).unwrap_err();
        assert!(error.contains("loopback"), "{error}");
        assert!(url("http://10.0.0.8:9090", false).is_ok(), "no credential");
        let error = url("http://user:pass@127.0.0.1:9090", false).unwrap_err();
        assert!(!error.contains("pass@"), "{error}");
        assert!(url("ftp://127.0.0.1", false).is_err());
        assert!(url("http://127.0.0.1:9090/?a=b", false).is_err());
        assert!(url("not a url", false).is_err());
        let bad_id = report_url("http://127.0.0.1:9090", "../metrics", false);
        assert!(bad_id.is_err());
    }
}
