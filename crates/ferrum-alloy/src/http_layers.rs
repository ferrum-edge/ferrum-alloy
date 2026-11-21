//! Optional CORS and compression layers built from configuration.

#[cfg(feature = "compression")]
use crate::config::CompressionSettings;
#[cfg(feature = "cors")]
use crate::config::CorsSettings;
#[cfg(feature = "cors")]
use crate::error::AlloyError;

/// Builds a CORS layer that permits only what is configured.
#[cfg(feature = "cors")]
pub(crate) fn cors(settings: &CorsSettings) -> Result<tower_http::cors::CorsLayer, AlloyError> {
    use http::{HeaderName, HeaderValue, Method};
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let invalid = |what: &str, value: &str| {
        AlloyError::Integration(format!("cors: invalid {what} {value:?}"))
    };
    // Every origin is parsed, even next to `*`, so a malformed entry is
    // refused rather than hidden by the wildcard (as `AlloyConfig::check`
    // refuses it).
    let values = settings
        .allowed_origins
        .iter()
        .map(|o| HeaderValue::from_str(o).map_err(|_| invalid("origin", o)))
        .collect::<Result<Vec<_>, _>>()?;
    let origins = if settings.allowed_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(values)
    };
    let methods = settings
        .allowed_methods
        .iter()
        .map(|m| m.parse::<Method>().map_err(|_| invalid("method", m)))
        .collect::<Result<Vec<_>, _>>()?;
    let headers = settings
        .allowed_headers
        .iter()
        .map(|h| h.parse::<HeaderName>().map_err(|_| invalid("header", h)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut layer = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(methods)
        .allow_headers(headers)
        .allow_credentials(settings.allow_credentials);
    if let Some(max_age) = settings.max_age_seconds {
        layer = layer.max_age(std::time::Duration::from_secs(max_age));
    }
    Ok(layer)
}

/// Builds a compression layer that never compresses event streams or
/// responses marked `no-store` or carrying `Set-Cookie`.
///
/// `no-store` is a directive name ([`CacheControl::has`]), not a substring:
/// `ext="no-store"` is not a prohibition. A `Cache-Control` value with an
/// unterminated quoted string could hide one, so it is not compressed.
///
/// [`CacheControl::has`]: ferrum_alloy_telemetry::cache::CacheControl::has
#[cfg(feature = "compression")]
pub(crate) fn compression(
    settings: &CompressionSettings,
) -> tower_http::compression::CompressionLayer<impl tower_http::compression::Predicate + use<>> {
    use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
    use tower_http::compression::{CompressionLayer, DefaultPredicate};

    let predicate = DefaultPredicate::new()
        .and(SizeAbove::new(settings.min_size_bytes))
        .and(NotForContentType::const_new("text/event-stream"))
        .and(
            |_: http::StatusCode,
             _: http::Version,
             headers: &http::HeaderMap,
             _: &http::Extensions| { compressible(headers) },
        );
    CompressionLayer::new().compress_when(predicate)
}

/// The response-header part of the compression predicate: no `Set-Cookie`,
/// and a `Cache-Control` that is well formed and has no `no-store`.
#[cfg(feature = "compression")]
fn compressible(headers: &http::HeaderMap) -> bool {
    use ferrum_alloy_telemetry::cache::CacheControl;

    let cache_control = CacheControl::parse(headers);
    !headers.contains_key(http::header::SET_COOKIE)
        && !cache_control.is_malformed()
        && !cache_control.has("no-store")
}

#[cfg(all(test, feature = "compression"))]
mod tests {
    use http::HeaderMap;
    use http::header::{CACHE_CONTROL, HeaderValue, SET_COOKIE};

    use super::compressible;

    fn headers(cache_control: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in cache_control {
            headers.append(CACHE_CONTROL, HeaderValue::from_static(value));
        }
        headers
    }

    /// `no-store` is matched as a directive name, never as a substring of
    /// another directive or of a quoted value (#279).
    #[test]
    fn only_a_no_store_directive_prevents_compression() {
        let compressed: [&[&'static str]; 6] = [
            &[],
            &["private, max-age=60"],
            &["private, ext=\"no-store\""],
            &["ext=\"a, no-store, b\""],
            &["no-storey"],
            &["x-no-store"],
        ];
        for compressed in compressed {
            assert!(compressible(&headers(compressed)), "{compressed:?}");
        }
        let skipped: [&[&'static str]; 6] = [
            &["no-store"],
            &["NO-STORE"],
            &["private, no-store"],
            &["ext=\"a, b\", no-store"],
            &["max-age=60", "no-store"],
            &["no-store=\"x\""],
        ];
        for skipped in skipped {
            assert!(!compressible(&headers(skipped)), "{skipped:?}");
        }
    }

    /// An unterminated quoted string could hide `no-store`, and `Set-Cookie`
    /// always prevents compression.
    #[test]
    fn malformed_cache_control_and_set_cookie_prevent_compression() {
        assert!(!compressible(&headers(&["ext=\"a, no-store"])));
        assert!(!compressible(&headers(&["public", "ext=\"open"])));
        let mut cookie = headers(&["public"]);
        cookie.insert(SET_COOKIE, HeaderValue::from_static("id=1"));
        assert!(!compressible(&cookie));
    }
}

#[cfg(all(test, feature = "cors"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod cors_tests {
    use super::cors;
    use crate::config::{CorsSettings, cors_syntax_issues};

    fn settings(origins: &[&str], methods: &[&str], headers: &[&str]) -> CorsSettings {
        let mut settings = CorsSettings::default();
        settings.enabled = true;
        settings.allowed_origins = origins.iter().map(|s| (*s).to_owned()).collect();
        settings.allowed_methods = methods.iter().map(|s| (*s).to_owned()).collect();
        settings.allowed_headers = headers.iter().map(|s| (*s).to_owned()).collect();
        settings
    }

    /// The offline check refuses exactly the entries startup cannot build a
    /// layer from, including a malformed origin listed next to `*`.
    #[test]
    fn check_and_startup_agree_on_cors_syntax() {
        let cases: [(&[&str], &[&str], &[&str], bool); 10] = [
            (&["https://app.example"], &["GET"], &["content-type"], true),
            (&["*"], &["GET", "POST"], &["x-request-id"], true),
            (&["https://app.example"], &[], &[], true),
            (&["bad\r\norigin"], &[], &[], false),
            (&["*", "bad\r\norigin"], &[], &[], false),
            (&["bad\norigin", "*"], &[], &[], false),
            (&["*"], &["GE T"], &[], false),
            (&["*"], &[""], &[], false),
            (&["*"], &["GET"], &["bad header"], false),
            (&["*"], &["GET"], &["x-ok", "bad:name"], false),
        ];
        for (origins, methods, headers, valid) in cases {
            let settings = settings(origins, methods, headers);
            let issues = cors_syntax_issues(&settings);
            assert_eq!(issues.is_empty(), valid, "{settings:?}: {issues:?}");
            assert_eq!(cors(&settings).is_ok(), valid, "{settings:?}");
        }
    }
}
