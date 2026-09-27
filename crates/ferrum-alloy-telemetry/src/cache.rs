//! Shared-cache safety for request-specific response headers.
//!
//! Adding a request id or `Server-Timing` value to a response that a shared
//! cache may store would replay one request's identifiers and timing to other
//! clients. Alloy therefore adds request-specific headers only when the
//! response is not storable by a shared cache (RFC 9111), and otherwise leaves
//! the application's caching behavior untouched.

use http::header::{CACHE_CONTROL, EXPIRES, HeaderMap, LAST_MODIFIED};
use http::{Method, StatusCode};

/// Status codes that are heuristically cacheable (RFC 9110 §15.1).
const HEURISTICALLY_CACHEABLE: &[u16] =
    &[200, 203, 204, 206, 300, 301, 308, 404, 405, 410, 414, 501];

/// Returns `true` when a shared cache may store this response.
///
/// Conservative: when in doubt it returns `true`, which only means Alloy
/// withholds its own request-specific headers.
pub fn is_shared_cacheable(
    method: &Method,
    request_has_authorization: bool,
    status: StatusCode,
    response_headers: &HeaderMap,
) -> bool {
    let directives = cache_directives(response_headers);
    let has = |name: &str| {
        directives
            .iter()
            .any(|d| d == name || d.starts_with(&format!("{name}=")))
    };

    if has("no-store") || has("private") {
        return false;
    }
    let explicit_shared_freshness = has("public") || has("s-maxage");
    let explicit_freshness =
        explicit_shared_freshness || has("max-age") || response_headers.contains_key(EXPIRES);

    // RFC 9111 §3.5: shared caches must not store responses to requests
    // with Authorization unless explicitly allowed.
    if request_has_authorization && !(explicit_shared_freshness || has("must-revalidate")) {
        return false;
    }

    match *method {
        Method::GET | Method::HEAD => {
            explicit_freshness
                || (HEURISTICALLY_CACHEABLE.contains(&status.as_u16())
                    && response_headers.contains_key(LAST_MODIFIED))
        }
        // POST responses are storable only with explicit freshness; other
        // methods are not storable.
        Method::POST => explicit_freshness,
        _ => false,
    }
}

fn cache_directives(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|d| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect()
}
