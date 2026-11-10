//! The rule for an explicitly configured OTLP/HTTP traces endpoint.
//!
//! It is available without the `otel` feature, so a configuration check
//! that does not compile the exporter accepts exactly the endpoints the
//! exporter accepts at startup.

/// Longest accepted endpoint, in bytes.
pub const MAX_LENGTH: usize = 2_048;

/// Checks an OTLP/HTTP traces endpoint: at most [`MAX_LENGTH`] bytes, an
/// `http://` or `https://` URL that parses as a URI, and no userinfo in its
/// authority. Returns why it is refused.
pub fn validate(endpoint: &str) -> Result<(), &'static str> {
    if endpoint.len() > MAX_LENGTH {
        return Err("endpoint must not exceed 2048 bytes");
    }
    let scheme_ok = endpoint.starts_with("http://") || endpoint.starts_with("https://");
    let has_userinfo = endpoint
        .split_once("://")
        .map(|(_, rest)| {
            let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            rest[..authority_end].contains('@')
        })
        .unwrap_or(false);
    if !scheme_ok || has_userinfo || endpoint.parse::<http::Uri>().is_err() {
        return Err("endpoint must be a valid http(s) URL without credentials");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https_urls() {
        assert_eq!(validate("http://127.0.0.1:4318/v1/traces"), Ok(()));
        assert_eq!(validate("https://collector.example/v1/traces"), Ok(()));
    }

    #[test]
    fn refuses_other_schemes_credentials_and_overlong_urls() {
        for endpoint in [
            "grpc://collector:4317",
            "collector:4318",
            "https://user:secret@collector/v1/traces",
        ] {
            assert!(validate(endpoint).is_err(), "{endpoint}");
        }
        let long = format!("https://collector/{}", "a".repeat(MAX_LENGTH));
        assert_eq!(validate(&long), Err("endpoint must not exceed 2048 bytes"));
    }
}
