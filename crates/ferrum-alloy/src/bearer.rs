//! The `Authorization: Bearer` credential, parsed one way for every check
//! Alloy makes: the management token, JWT verification, and
//! `DiagnosticsRequest::bearer_token` for diagnostic retrieval authorizers.
//!
//! Which `Authorization` header is read is each caller's choice, named at
//! the call site: [`first`] reads the first one, as `HeaderMap::get` does,
//! and [`sole`] requires exactly one.

use http::HeaderMap;
use http::header::{AUTHORIZATION, HeaderValue};

/// The credential of one `Authorization` value whose scheme is `Bearer`
/// (case-insensitive, RFC 9110), with surrounding whitespace trimmed. The
/// credential may be empty.
pub(crate) fn credential(value: &HeaderValue) -> Option<&str> {
    let (scheme, credential) = value.to_str().ok()?.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credential.trim())
}

/// The bearer credential of the first `Authorization` header. Later
/// `Authorization` headers are not read.
pub(crate) fn first(headers: &HeaderMap) -> Option<&str> {
    headers.get(AUTHORIZATION).and_then(credential)
}

/// The bearer credential when the request has exactly one `Authorization`
/// header. Several are ambiguous and yield `None`.
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn sole(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => credential(value),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn value(text: &'static str) -> HeaderValue {
        HeaderValue::from_static(text)
    }

    fn headers(values: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for text in values {
            headers.append(AUTHORIZATION, value(text));
        }
        headers
    }

    #[test]
    fn only_the_bearer_scheme_yields_a_trimmed_credential() {
        assert_eq!(credential(&value("Bearer abc")), Some("abc"));
        assert_eq!(credential(&value("bearer abc")), Some("abc"));
        assert_eq!(credential(&value("BEARER abc")), Some("abc"));
        assert_eq!(credential(&value("Bearer   abc  ")), Some("abc"));
        assert_eq!(credential(&value("Bearer a b")), Some("a b"));
        assert_eq!(credential(&value("Bearer  ")), Some(""), "may be empty");
        assert_eq!(credential(&value("Bearer")), None, "no separator");
        assert_eq!(credential(&value("Basic abc")), None);
        assert_eq!(credential(&value("Bearerabc")), None);
        assert_eq!(credential(&value("Bearer-x abc")), None);
        assert_eq!(credential(&value(" Bearer abc")), None);
        let opaque = HeaderValue::from_bytes(b"Bearer caf\xc3\xa9").unwrap();
        assert_eq!(credential(&opaque), None, "not visible ASCII");
    }

    #[test]
    fn first_reads_only_the_first_authorization_header() {
        assert_eq!(first(&HeaderMap::new()), None);
        assert_eq!(first(&headers(&["Bearer abc"])), Some("abc"));
        assert_eq!(first(&headers(&["Bearer abc", "Bearer def"])), Some("abc"));
        assert_eq!(first(&headers(&["Basic abc", "Bearer def"])), None);
    }

    #[test]
    fn sole_requires_exactly_one_authorization_header() {
        assert_eq!(sole(&HeaderMap::new()), None);
        assert_eq!(sole(&headers(&["bearer abc "])), Some("abc"));
        assert_eq!(sole(&headers(&["Bearer abc", "Bearer def"])), None);
        assert_eq!(sole(&headers(&["Bearer abc", "Basic def"])), None);
        assert_eq!(sole(&headers(&["Basic abc"])), None);
    }
}
