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

    // The same rules as `AlloyConfig::check`, so startup refuses exactly what
    // the check reports, and never builds a layer `tower-http` would panic on.
    if let Some(issue) = crate::config::cors_entry_issues(settings)
        .into_iter()
        .next()
    {
        return Err(AlloyError::Integration(format!("cors: {issue}")));
    }
    let invalid = |what: &str, index: usize| {
        AlloyError::Integration(format!("cors: invalid {what} [{index}]"))
    };
    let values = settings
        .allowed_origins
        .iter()
        .enumerate()
        .map(|(i, o)| HeaderValue::from_str(o).map_err(|_| invalid("origin", i)))
        .collect::<Result<Vec<_>, _>>()?;
    let origins = if settings.allowed_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(values)
    };
    let methods = settings
        .allowed_methods
        .iter()
        .enumerate()
        .map(|(i, m)| m.parse::<Method>().map_err(|_| invalid("method", i)))
        .collect::<Result<Vec<_>, _>>()?;
    let headers = settings
        .allowed_headers
        .iter()
        .enumerate()
        .map(|(i, h)| h.parse::<HeaderName>().map_err(|_| invalid("header", i)))
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
    use crate::config::{CorsSettings, cors_entry_issues};

    struct Case {
        origins: &'static [&'static str],
        methods: &'static [&'static str],
        headers: &'static [&'static str],
        credentials: bool,
        valid: bool,
    }

    const fn case(
        origins: &'static [&'static str],
        methods: &'static [&'static str],
        headers: &'static [&'static str],
        credentials: bool,
        valid: bool,
    ) -> Case {
        Case {
            origins,
            methods,
            headers,
            credentials,
            valid,
        }
    }

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The offline check refuses exactly the entries startup cannot build a
    /// layer from, including a malformed origin listed next to `*` and the
    /// wildcards `tower-http` refuses (by panicking) to combine with
    /// credentials. Every accepted layer is applied, so such a panic fails
    /// the test.
    #[test]
    fn check_and_startup_agree_on_cors_entries() {
        let cases = [
            case(
                &["https://app.example"],
                &["GET"],
                &["content-type"],
                false,
                true,
            ),
            case(
                &["https://app.example"],
                &["GET"],
                &["content-type"],
                true,
                true,
            ),
            case(&["*"], &["GET", "POST"], &["x-request-id"], false, true),
            case(&["*"], &["*"], &["*"], false, true),
            case(&["https://app.example"], &[], &[], false, true),
            case(&["bad\r\norigin"], &[], &[], false, false),
            case(&["*", "bad\r\norigin"], &[], &[], false, false),
            case(&["bad\norigin", "*"], &[], &[], false, false),
            case(&["*"], &["GE T"], &[], false, false),
            case(&["*"], &[""], &[], false, false),
            case(&["*"], &["GET"], &["bad header"], false, false),
            case(&["*"], &["GET"], &["x-ok", "bad:name"], false, false),
            case(&["*"], &[], &[], true, false),
            case(&["https://app.example"], &["*"], &[], true, false),
            case(&["https://app.example"], &[], &["*"], true, false),
            case(
                &["https://app.example"],
                &["PATCH", "*"],
                &["x-request-id", "*"],
                true,
                true,
            ),
            case(&["https://app.example"], &["*", "*"], &[], true, true),
        ];
        for case in cases {
            let settings = CorsSettings {
                enabled: true,
                allowed_origins: owned(case.origins),
                allowed_methods: owned(case.methods),
                allowed_headers: owned(case.headers),
                allow_credentials: case.credentials,
                ..CorsSettings::default()
            };
            let issues = cors_entry_issues(&settings);
            assert_eq!(issues.is_empty(), case.valid, "{settings:?}: {issues:?}");
            match cors(&settings) {
                Ok(layer) => {
                    assert!(case.valid, "{settings:?}");
                    let _ = tower::Layer::layer(
                        &layer,
                        tower::service_fn(|()| async { Ok::<_, std::convert::Infallible>(()) }),
                    );
                }
                Err(error) => assert!(!case.valid, "{settings:?}: {error}"),
            }
        }
    }

    /// Diagnostics name an entry by index, so a credential pasted into an
    /// origin is not echoed by `ferrum-alloy check` or startup.
    #[test]
    fn cors_diagnostics_do_not_quote_entries() {
        let settings = CorsSettings {
            enabled: true,
            allowed_origins: owned(&["*", "https://user:EXAMPLE-PASSWORD@app.example\n"]),
            ..CorsSettings::default()
        };
        let issues = cors_entry_issues(&settings);
        assert_eq!(
            issues,
            ["cors.allowed_origins[1] is not a valid header value"]
        );
        let error = cors(&settings).unwrap_err().to_string();
        assert!(!error.contains("EXAMPLE-PASSWORD"), "{error}");
    }
}
