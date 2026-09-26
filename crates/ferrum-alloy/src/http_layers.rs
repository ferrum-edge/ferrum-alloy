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
    let origins = if settings.allowed_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        let values = settings
            .allowed_origins
            .iter()
            .map(|o| HeaderValue::from_str(o).map_err(|_| invalid("origin", o)))
            .collect::<Result<Vec<_>, _>>()?;
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
             _: &http::Extensions| {
                !headers.contains_key(http::header::SET_COOKIE)
                    && !headers
                        .get_all(http::header::CACHE_CONTROL)
                        .iter()
                        .filter_map(|v| v.to_str().ok())
                        .any(|v| v.to_ascii_lowercase().contains("no-store"))
            },
        );
    CompressionLayer::new().compress_when(predicate)
}
