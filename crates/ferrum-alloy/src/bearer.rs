//! The `Authorization: Bearer` credential, read one way for every check
//! Alloy makes: the management token, JWT verification, and
//! `DiagnosticsRequest::bearer_token` for diagnostic retrieval authorizers.
//!
//! A request with more than one `Authorization` header is ambiguous and is
//! refused rather than picking one, matching the rule for every other
//! singleton request header (see `ferrum_alloy_telemetry::trace_context`)
//! and Ferrum Edge, which refuses such a request at ingress. Another
//! component, such as a proxy that authorizes on the last copy, could
//! otherwise act on a different credential than the one Alloy verified.

use http::HeaderMap;
use http::header::{AUTHORIZATION, HeaderValue};

/// The request has more than one `Authorization` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ambiguous;

/// The credential of one `Authorization` value whose scheme is `Bearer`
/// (case-insensitive, RFC 9110), with surrounding whitespace trimmed. An
/// empty credential is no credential (RFC 6750, section 3.1: a request
/// without authentication information).
pub(crate) fn credential(value: &HeaderValue) -> Option<&str> {
    let (scheme, credential) = value.to_str().ok()?.split_once(' ')?;
    let credential = credential.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !credential.is_empty()).then_some(credential)
}

/// The bearer credential of the request's only `Authorization` header.
/// `Ok(None)` when there is no `Authorization` header, or its single value
/// is not a non-empty bearer credential; `Err(Ambiguous)` when there are
/// several, whatever their values.
pub(crate) fn sole(headers: &HeaderMap) -> Result<Option<&str>, Ambiguous> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => Ok(credential(value)),
        (Some(_), Some(_)) => Err(Ambiguous),
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
    fn only_the_bearer_scheme_yields_a_trimmed_non_empty_credential() {
        assert_eq!(credential(&value("Bearer abc")), Some("abc"));
        assert_eq!(credential(&value("bearer abc")), Some("abc"));
        assert_eq!(credential(&value("BEARER abc")), Some("abc"));
        assert_eq!(credential(&value("Bearer   abc  ")), Some("abc"));
        assert_eq!(credential(&value("Bearer a b")), Some("a b"));
        assert_eq!(credential(&value("Bearer  ")), None, "empty is none");
        assert_eq!(credential(&value("Bearer")), None, "no separator");
        assert_eq!(credential(&value("Basic abc")), None);
        assert_eq!(credential(&value("Bearerabc")), None);
        assert_eq!(credential(&value("Bearer-x abc")), None);
        assert_eq!(credential(&value(" Bearer abc")), None);
        let opaque = HeaderValue::from_bytes(b"Bearer caf\xc3\xa9").unwrap();
        assert_eq!(credential(&opaque), None, "not visible ASCII");
    }

    #[test]
    fn sole_requires_exactly_one_authorization_header() {
        assert_eq!(sole(&HeaderMap::new()), Ok(None));
        assert_eq!(sole(&headers(&["bearer abc "])), Ok(Some("abc")));
        assert_eq!(sole(&headers(&["Basic abc"])), Ok(None));
        assert_eq!(sole(&headers(&["Bearer  "])), Ok(None));
        assert_eq!(
            sole(&headers(&["Bearer abc", "Bearer def"])),
            Err(Ambiguous)
        );
        assert_eq!(
            sole(&headers(&["Bearer abc", "Bearer abc"])),
            Err(Ambiguous)
        );
        assert_eq!(sole(&headers(&["Bearer abc", "Basic def"])), Err(Ambiguous));
        assert_eq!(sole(&headers(&["Basic abc", "Bearer def"])), Err(Ambiguous));
        assert_eq!(sole(&headers(&["Bearer abc", "Bearer  "])), Err(Ambiguous));
    }
}
