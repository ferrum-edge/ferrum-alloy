//! Shared-cache safety for request-specific response headers.
//!
//! Adding a request id or `Server-Timing` value to a response that a shared
//! cache may store would replay one request's identifiers and timing to other
//! clients. Alloy therefore adds request-specific headers only when the
//! response is not storable by a shared cache (RFC 9111), and otherwise leaves
//! the application's caching behavior untouched.
//!
//! A GET or HEAD response with a heuristically cacheable status is storable
//! even without explicit freshness or validators (RFC 9111 §3 and §4.2.2):
//! `Last-Modified` only feeds the heuristic freshness lifetime. Applications
//! that want the request-specific headers on such responses mark them
//! `Cache-Control: private` or `no-store`.

use http::header::{CACHE_CONTROL, EXPIRES, HeaderMap};
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
    let (dirs, malformed) = directives(response_headers);
    let has = |name: &str| dirs.iter().any(|d| directive_is(d, name));

    // A qualified `private="field"` still lets a shared cache store the rest
    // of the response (RFC 9111 §5.2.2.7), so only the bare form prohibits it.
    if has("no-store") || dirs.iter().any(|d| d.eq_ignore_ascii_case("private")) {
        return false;
    }
    // An ambiguous value (an unterminated quoted string) is treated as
    // storable, which only withholds Alloy's request-specific headers.
    if malformed {
        return true;
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
        // Heuristic freshness needs no validator (RFC 9111 §4.2.2).
        Method::GET | Method::HEAD => {
            explicit_freshness || HEURISTICALLY_CACHEABLE.contains(&status.as_u16())
        }
        // POST responses are storable only with explicit freshness; other
        // methods are not storable.
        Method::POST => explicit_freshness,
        _ => false,
    }
}

/// Trimmed response `Cache-Control` directives, borrowed from the header
/// values (no lowercasing copies; comparisons are ASCII-insensitive), and
/// whether any value was malformed (an unterminated quoted string).
///
/// Commas separate directives only outside quoted strings (RFC 9110
/// §5.6.4): splitting inside `ext="x, private, y"` would invent a bare
/// `private` directive the sender never sent (#217). Quoted pairs (`\"`,
/// `\\`) do not end the quoted string.
fn directives(headers: &HeaderMap) -> (Vec<&str>, bool) {
    let mut out = Vec::new();
    let mut malformed = false;
    for value in headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
    {
        let bytes = value.as_bytes();
        let mut start = 0usize;
        let mut at = 0usize;
        let mut quoted = false;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if quoted => at += 1, // skip the quoted pair's byte below
                b'"' => quoted = !quoted,
                b',' if !quoted => {
                    push_directive(&mut out, &value[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
            at += 1;
        }
        if quoted {
            malformed = true;
        }
        push_directive(&mut out, &value[start..]);
    }
    (out, malformed)
}

fn push_directive<'v>(out: &mut Vec<&'v str>, raw: &'v str) {
    let directive = raw.trim();
    if !directive.is_empty() {
        out.push(directive);
    }
}

/// Whether `directive` is `name` or its `name=value` form. `name` is an
/// ASCII constant, so byte comparisons need no case folding copies.
fn directive_is(directive: &str, name: &str) -> bool {
    let d = directive.as_bytes();
    let n = name.as_bytes();
    d.len() >= n.len()
        && d[..n.len()].eq_ignore_ascii_case(n)
        && (d.len() == n.len() || d[n.len()] == b'=')
}
