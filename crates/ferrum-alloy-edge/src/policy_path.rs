//! Ferrum Edge's canonical policy path, ported for the `x-ferrum-mcp`
//! admission checks.
//!
//! Edge evaluates every request-path decision on one canonical form
//! (ferrum-edge `src/policy_path.rs`, `canonicalize_policy_path`). Its
//! `mcp_gateway` claims requests by comparing that canonical path with the
//! endpoint scope, refuses an `endpoint.path` that is not already canonical,
//! and decodes each literal run of an operation path template with the same
//! function before deciding whether the template can reach the scope
//! (`path_template_may_enter_endpoint_scope`). Alloy's checks must reach the
//! same verdicts, so this is a faithful port of the literal-path half of that
//! contract (Edge's `LiteralStructure::Enforced`), not a re-derivation:
//!
//! 1. Every `%` starts a complete `%XX` escape ([`PolicyPathRejection::InvalidEscape`]).
//! 2. An escape of `/`, `?`, or `#` is refused ([`PolicyPathRejection::EncodedSeparator`]),
//!    as is an escaped or literal `\` ([`PolicyPathRejection::EncodedBackslash`],
//!    [`PolicyPathRejection::LiteralBackslash`]).
//! 3. An encoded `%` (`%25`) is refused ([`PolicyPathRejection::DoubleEncoding`]).
//! 4. Only an escape of an RFC 3986 `pchar` (unreserved, sub-delims, `:`,
//!    `@`) is decoded; any other escape is refused
//!    ([`PolicyPathRejection::UnrepresentableEscape`]), and an escaped C0
//!    control or `DEL` is [`PolicyPathRejection::EncodedControl`]. No percent
//!    escape survives.
//! 5. No `.` or `..` segment survives, with or without a `;` parameter
//!    (`..;`, `.;x`): one an escape helped form is
//!    [`PolicyPathRejection::AmbiguousDotSegment`], a literal one
//!    [`PolicyPathRejection::LiteralDotSegment`].
//! 6. No empty segment survives except a trailing one, and no non-final
//!    segment is empty before its first `;` ([`PolicyPathRejection::EmptySegment`]).
//!
//! Edge's per-proxy `;` rule (`allow_path_parameters`) is decided after
//! routing and is not part of `canonicalize_policy_path`, so it is not part
//! of this port either: a `;` path parameter is accepted here, as in Edge.
//!
//! The function is idempotent, and its result is borrowed exactly when the
//! input contained no escape.

use std::borrow::Cow;

/// Why a path is not a canonical policy path (Edge `PolicyPathRejection`,
/// less the per-proxy `PathParameter`, which the canonicalizer never returns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyPathRejection {
    /// A `%` was not followed by two hexadecimal digits.
    InvalidEscape,
    /// An encoded `%` (`%25`), the lead byte of a double encoding.
    DoubleEncoding,
    /// An encoded `/`, `?`, or `#`.
    EncodedSeparator,
    /// An encoded `\`.
    EncodedBackslash,
    /// A literal `\`.
    LiteralBackslash,
    /// An encoded C0 control character or `DEL` (includes `%00`).
    EncodedControl,
    /// An escape of a byte outside the `pchar` decode table (space, `{`, `[`,
    /// any non-ASCII byte, ...).
    UnrepresentableEscape,
    /// A percent escape helped produce a `.` or `..` path segment: an escaped
    /// dot (`%2e%2e`, `.%2e;x`) or an escaped `;` delimiter (`..%3B`).
    AmbiguousDotSegment,
    /// A literal `.` or `..` path segment, including one carrying a `;` path
    /// parameter (`..;`, `.;x`).
    LiteralDotSegment,
    /// A non-final empty path segment (`//a`, `/a//b`), or a non-final
    /// segment that is empty before its first `;` (`/;x/a`).
    EmptySegment,
}

impl PolicyPathRejection {
    /// Edge's stable reason token.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::InvalidEscape => "invalid_escape",
            Self::DoubleEncoding => "double_encoding",
            Self::EncodedSeparator => "encoded_separator",
            Self::EncodedBackslash => "encoded_backslash",
            Self::LiteralBackslash => "literal_backslash",
            Self::EncodedControl => "encoded_control",
            Self::UnrepresentableEscape => "unrepresentable_escape",
            Self::AmbiguousDotSegment => "ambiguous_dot_segment",
            Self::LiteralDotSegment => "literal_dot_segment",
            Self::EmptySegment => "empty_segment",
        }
    }
}

/// Bytes whose escape is decoded: RFC 3986 `pchar` minus `pct-encoded`, i.e.
/// `unreserved / sub-delims / ":" / "@"`. `/` is deliberately absent.
const DECODE_TO_LITERAL: [bool; 256] = build_decode_table();

const fn build_decode_table() -> [bool; 256] {
    let mut table = [false; 256];
    let mut index = 0usize;
    while index < 256 {
        let byte = index as u8;
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        let sub_delims = matches!(
            byte,
            b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
        );
        table[index] = unreserved || sub_delims || byte == b':' || byte == b'@';
        index += 1;
    }
    table
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// How many leading bytes of `segment` form a dot segment (its text before
/// the first `;` is `.` or `..`, plus that `;`), or `None` when it is not one.
fn dot_segment_len(segment: &[u8]) -> Option<usize> {
    match segment {
        [b'.'] => Some(1),
        [b'.', b'.'] | [b'.', b';', ..] => Some(2),
        [b'.', b'.', b';', ..] => Some(3),
        _ => None,
    }
}

/// Where a completed segment sits in the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentPosition {
    /// The text before the first `/`: empty for every absolute path.
    Leading,
    /// Any segment followed by a `/`.
    Inner,
    /// The last segment of a path that contains a `/`.
    Final,
}

fn segment_position(segment_start: usize, at_end: bool) -> SegmentPosition {
    if segment_start == 0 {
        SegmentPosition::Leading
    } else if at_end {
        SegmentPosition::Final
    } else {
        SegmentPosition::Inner
    }
}

/// Refuse a completed segment under rules 5 and 6. `first_escape` is the
/// offset, within the segment, of its first escape-decoded byte.
fn check_segment(
    segment: &[u8],
    position: SegmentPosition,
    first_escape: Option<usize>,
) -> Result<(), PolicyPathRejection> {
    match segment {
        [] if position != SegmentPosition::Inner => Ok(()),
        [b';', ..] if position == SegmentPosition::Final => Ok(()),
        [] | [b';', ..] => Err(PolicyPathRejection::EmptySegment),
        _ => match dot_segment_len(segment) {
            Some(dot_len) if first_escape.is_some_and(|offset| offset < dot_len) => {
                Err(PolicyPathRejection::AmbiguousDotSegment)
            }
            Some(_) => Err(PolicyPathRejection::LiteralDotSegment),
            None => Ok(()),
        },
    }
}

/// Whether the escape-free scan proved the path canonical, or a `%` needs the
/// decoding pass (which re-validates from the start).
enum Prescan {
    AlreadyCanonical,
    NeedsDecoding,
}

fn prescan(bytes: &[u8]) -> Result<Prescan, PolicyPathRejection> {
    let mut segment_start = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'%' => return Ok(Prescan::NeedsDecoding),
            b'\\' => return Err(PolicyPathRejection::LiteralBackslash),
            b'/' => {
                check_segment(
                    &bytes[segment_start..index],
                    segment_position(segment_start, false),
                    None,
                )?;
                segment_start = index + 1;
            }
            _ => {}
        }
    }
    check_segment(
        &bytes[segment_start..],
        segment_position(segment_start, true),
        None,
    )?;
    Ok(Prescan::AlreadyCanonical)
}

/// Edge `canonicalize_policy_path`: the canonical policy path for `raw`, or
/// why Edge refuses it. Borrowed when `raw` contains no escape.
pub(crate) fn canonicalize_policy_path(raw: &str) -> Result<Cow<'_, str>, PolicyPathRejection> {
    let bytes = raw.as_bytes();
    if let Prescan::AlreadyCanonical = prescan(bytes)? {
        return Ok(Cow::Borrowed(raw));
    }

    let mut canonical: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut segment_start = 0usize;
    let mut segment_first_escape: Option<usize> = None;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];

        if byte == b'/' {
            check_segment(
                &canonical[segment_start..],
                segment_position(segment_start, false),
                segment_first_escape,
            )?;
            canonical.push(b'/');
            segment_start = canonical.len();
            segment_first_escape = None;
            index += 1;
            continue;
        }

        if byte == b'\\' {
            return Err(PolicyPathRejection::LiteralBackslash);
        }

        if byte != b'%' {
            canonical.push(byte);
            index += 1;
            continue;
        }

        let (Some(high), Some(low)) = (
            bytes.get(index + 1).copied().and_then(hex_value),
            bytes.get(index + 2).copied().and_then(hex_value),
        ) else {
            return Err(PolicyPathRejection::InvalidEscape);
        };
        let value = (high << 4) | low;

        match value {
            b'%' => return Err(PolicyPathRejection::DoubleEncoding),
            b'/' | b'?' | b'#' => return Err(PolicyPathRejection::EncodedSeparator),
            b'\\' => return Err(PolicyPathRejection::EncodedBackslash),
            0x00..=0x1F | 0x7F => return Err(PolicyPathRejection::EncodedControl),
            _ if !DECODE_TO_LITERAL[usize::from(value)] => {
                return Err(PolicyPathRejection::UnrepresentableEscape);
            }
            _ => {}
        }

        if segment_first_escape.is_none() {
            segment_first_escape = Some(canonical.len() - segment_start);
        }
        canonical.push(value);
        index += 3;
    }

    check_segment(
        &canonical[segment_start..],
        segment_position(segment_start, true),
        segment_first_escape,
    )?;

    // `raw`'s literal bytes, copied in order, interleaved with decoded ASCII
    // `pchar`s: valid UTF-8 by construction.
    String::from_utf8(canonical)
        .map(Cow::Owned)
        .map_err(|_| PolicyPathRejection::UnrepresentableEscape)
}

/// Edge `non_canonical_policy_path_reason`: why `path` is not already a
/// canonical policy path, or `None` when it is. A path that canonicalizes to
/// a different spelling can never match a canonical request path either.
pub(crate) fn non_canonical_policy_path_reason(path: &str) -> Option<&'static str> {
    match canonicalize_policy_path(path) {
        Ok(Cow::Borrowed(_)) => None,
        Ok(Cow::Owned(_)) => Some("percent-escapes that canonicalize to a different path"),
        Err(rejection) => Some(rejection.reason()),
    }
}

/// Parity tests: the cases of Edge's own `tests/unit/gateway_core/policy_path_tests.rs`
/// that apply to `canonicalize_policy_path` and
/// `non_canonical_policy_path_reason`, with Edge's expected verdicts.
#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;

    fn canonical(path: &str) -> String {
        canonicalize_policy_path(path)
            .unwrap_or_else(|rejection| panic!("{path:?} unexpectedly rejected: {rejection:?}"))
            .into_owned()
    }

    fn rejection(path: &str) -> PolicyPathRejection {
        canonicalize_policy_path(path)
            .err()
            .unwrap_or_else(|| panic!("{path:?} was unexpectedly accepted"))
    }

    fn assert_borrowed(path: &str) {
        let result = canonicalize_policy_path(path)
            .unwrap_or_else(|rejection| panic!("{path:?} rejected: {rejection:?}"));
        assert!(matches!(result, Cow::Borrowed(_)), "{path:?} must borrow");
        assert_eq!(result, path);
    }

    fn assert_rejections(rejection_expected: PolicyPathRejection, paths: &[&str]) {
        for path in paths {
            assert_eq!(rejection(path), rejection_expected, "{path:?}");
        }
    }

    #[test]
    fn ordinary_paths_are_borrowed_unchanged() {
        for path in [
            "",
            "/",
            "*",
            "/admin",
            "/api/v1/users/42",
            "/api/v1/",
            "/a/.hidden/b",
            "/a/..hidden/b",
            "/a/b./c",
            "/v1.0/users",
            "/weird!$&'()*+,;=:@chars",
        ] {
            assert_borrowed(path);
        }
    }

    #[test]
    fn the_escape_free_scan_still_rejects_literal_structure() {
        assert_eq!(rejection("/a/../b"), PolicyPathRejection::LiteralDotSegment);
        assert_eq!(rejection("/a\\b"), PolicyPathRejection::LiteralBackslash);
    }

    #[test]
    fn ordinary_single_encoding_of_a_legal_path_character_is_decoded() {
        assert_eq!(canonical("/%61dmin"), "/admin");
        assert_eq!(canonical("/%41DMIN"), "/ADMIN");
        assert_eq!(canonical("/api/%76%31/users"), "/api/v1/users");
        assert_eq!(canonical("/%61%64%6d%69%6e"), "/admin");
    }

    #[test]
    fn every_unreserved_and_sub_delim_escape_is_decoded() {
        for (raw, expected) in [
            ("/%2Dx", "/-x"),
            ("/%2Ex", "/.x"),
            ("/%5Fx", "/_x"),
            ("/%7Ex", "/~x"),
            ("/%21x", "/!x"),
            ("/%24x", "/$x"),
            ("/%26x", "/&x"),
            ("/%27x", "/'x"),
            ("/%28x", "/(x"),
            ("/%29x", "/)x"),
            ("/%2Ax", "/*x"),
            ("/%2Bx", "/+x"),
            ("/%2Cx", "/,x"),
            ("/a%3Bx", "/a;x"),
            ("/%3Dx", "/=x"),
            ("/%3Ax", "/:x"),
            ("/%40x", "/@x"),
        ] {
            assert_eq!(canonical(raw), expected, "decoding {raw:?}");
        }
    }

    #[test]
    fn escapes_of_characters_illegal_in_a_path_are_rejected() {
        assert_rejections(
            PolicyPathRejection::UnrepresentableEscape,
            &[
                "/api%20name",
                "/api%7bname",
                "/api%7Dname",
                "/api%5Bname%5D",
                "/api%22name",
                "/api%3Cname%3E",
                "/api%5Ename",
                "/api%60name",
                "/api%7Cname",
                "/caf%c3%a9",
                "/%E2%9C%93",
                "/%FF",
            ],
        );
    }

    #[test]
    fn no_percent_escape_survives_canonicalization() {
        for raw in [
            "/%61dmin",
            "/a/%2Ehidden",
            "/%40user/m%3Bmatrix",
            "/api/%76%31/users",
        ] {
            let once = canonical(raw);
            assert!(!once.contains('%'), "{raw:?} kept an escape: {once:?}");
            assert_eq!(canonical(&once), once, "not idempotent for {raw:?}");
            assert!(
                !once.bytes().any(|byte| byte <= 0x20 || byte >= 0x7F),
                "{once:?} must be transmissible as a request target"
            );
        }
    }

    #[test]
    fn encoded_separators_are_rejected() {
        assert_rejections(
            PolicyPathRejection::EncodedSeparator,
            &[
                "/api%2Fadmin",
                "/api%2fadmin",
                "/%2F",
                "/api%3Fquery",
                "/api%23fragment",
            ],
        );
    }

    #[test]
    fn encoded_backslash_is_rejected() {
        assert_rejections(
            PolicyPathRejection::EncodedBackslash,
            &["/api%5Cadmin", "/api%5cadmin"],
        );
    }

    #[test]
    fn double_encoding_is_rejected_at_the_lead_byte() {
        assert_rejections(
            PolicyPathRejection::DoubleEncoding,
            &["/api%252Fadmin", "/api%252fadmin", "/%2561dmin", "/a%25b"],
        );
    }

    #[test]
    fn incomplete_or_non_hex_escapes_are_rejected() {
        assert_rejections(
            PolicyPathRejection::InvalidEscape,
            &["/api%", "/api%2", "/api%zz", "/api%2z", "/api%g0/more"],
        );
    }

    #[test]
    fn escaped_non_ascii_bytes_are_rejected_whether_or_not_they_form_valid_utf8() {
        assert_rejections(
            PolicyPathRejection::UnrepresentableEscape,
            &["/caf%C3%A9", "/caf%C3%28", "/%FF", "/%C3", "/%E2%82"],
        );
    }

    #[test]
    fn encoded_control_characters_including_nul_are_rejected() {
        assert_rejections(
            PolicyPathRejection::EncodedControl,
            &["/api%00", "/api%00admin", "/api%0A", "/api%0d", "/api%7F"],
        );
    }

    #[test]
    fn escape_synthesized_dot_segments_are_rejected() {
        assert_rejections(
            PolicyPathRejection::AmbiguousDotSegment,
            &[
                "/api/%2e%2e/admin",
                "/api/%2E%2E/admin",
                "/api/.%2e/admin",
                "/api/%2e./admin",
                "/api/%2e/admin",
                "/api/%2E",
            ],
        );
    }

    #[test]
    fn literal_dot_segments_are_rejected_too() {
        assert_rejections(
            PolicyPathRejection::LiteralDotSegment,
            &[
                "/api/../admin",
                "/api/./admin",
                "/..",
                "/.",
                "/api/..",
                "/api/.",
                "/../api",
                "/./api",
                "..",
                ".",
                "/a/b/../../c",
            ],
        );
    }

    #[test]
    fn a_literal_dot_segment_is_rejected_even_alongside_accepted_escapes() {
        assert_rejections(
            PolicyPathRejection::LiteralDotSegment,
            &["/api/../%61dmin", "/%61pi/../admin", "/%61pi/.."],
        );
    }

    #[test]
    fn literal_dot_segments_with_a_path_parameter_are_rejected() {
        assert_rejections(
            PolicyPathRejection::LiteralDotSegment,
            &[
                "/public/..;/admin",
                "/public/..;/admin/users",
                "/a/.;/b",
                "/a/.;x/b",
                "/a/..;a=b/c",
                "/a/..;jsessionid=1/b",
                "/a/..;;",
                "/a/..;",
                "/a/.;",
                "..;",
                ".;x",
                "/..;/admin",
                "/%61/..;/admin",
                "/a/..;/%61dmin",
                "/a/..;%61/b",
                "/a/.;%78",
            ],
        );
    }

    #[test]
    fn escaped_dot_segments_with_a_path_parameter_are_ambiguous() {
        assert_rejections(
            PolicyPathRejection::AmbiguousDotSegment,
            &[
                "/a/%2e%2e;/b",
                "/a/%2E%2E;x/b",
                "/a/.%2e;x/b",
                "/a/%2e.;/b",
                "/a/%2e;/b",
                "/a/%2e;x",
                "/a/..%3b/b",
                "/a/..%3B/b",
                "/a/..%3bjsessionid=1/b",
                "/a/.%3bx/b",
                "/a/%2e%2e%3b/b",
            ],
        );
    }

    #[test]
    fn a_path_parameter_on_an_ordinary_segment_is_still_accepted() {
        for path in [
            "/a;b",
            "/v1;version=2",
            "/api/v1;version=2/users",
            "/a/..a;b/c",
            "/a/a..;b/c",
            "/a/...;x/c",
            "/a/.hidden;x/c",
            "/a/b;..",
            "/a/b;../c",
        ] {
            assert_borrowed(path);
        }
        assert_eq!(canonical("/a%3bb"), "/a;b");
        assert_eq!(canonical("/v1%3Bversion=2"), "/v1;version=2");
        assert_eq!(canonical("/a/%2e%2ea%3bb"), "/a/..a;b");
    }

    #[test]
    fn non_final_empty_segments_are_rejected() {
        assert_rejections(
            PolicyPathRejection::EmptySegment,
            &[
                "//",
                "//admin",
                "//admin/users",
                "/a//b",
                "/a//",
                "/a///b",
                "/;x/admin",
                "/;/admin",
                "/a/;x/b",
                "/a/;/b",
                ";x",
                "//%61dmin",
                "/%61//b",
                "/%3Bx/admin",
                "/a/%3B/b",
                "/ctx/;jsessionid=abc/x",
            ],
        );
    }

    #[test]
    fn the_root_path_a_trailing_slash_and_a_final_parameter_are_not_empty_segments() {
        for path in [
            "",
            "/",
            "*",
            "/a/",
            "/a/b/",
            "=/",
            "=/api/",
            "/ctx/;jsessionid=abc",
            "/;",
            "/;x",
        ] {
            assert_borrowed(path);
        }
        assert_eq!(canonical("/%61/"), "/a/");
        assert_eq!(canonical("/ctx/%3Bjsessionid=abc"), "/ctx/;jsessionid=abc");
    }

    #[test]
    fn literal_backslash_is_rejected() {
        assert_rejections(
            PolicyPathRejection::LiteralBackslash,
            &[
                "/api\\admin",
                "/\\",
                "/a\\..\\b",
                "\\admin",
                "/%61pi\\admin",
                "/api\\%61dmin",
            ],
        );
    }

    #[test]
    fn escapes_in_a_segment_that_is_not_a_dot_segment_are_fine() {
        assert_eq!(canonical("/api/%2ehidden"), "/api/.hidden");
        assert_eq!(canonical("/api/%2e%2ehidden"), "/api/..hidden");
        assert_eq!(canonical("/api/a%2e/b"), "/api/a./b");
    }

    #[test]
    fn rejection_reasons_are_edges() {
        for (variant, reason) in [
            (PolicyPathRejection::InvalidEscape, "invalid_escape"),
            (PolicyPathRejection::DoubleEncoding, "double_encoding"),
            (PolicyPathRejection::EncodedSeparator, "encoded_separator"),
            (PolicyPathRejection::EncodedBackslash, "encoded_backslash"),
            (PolicyPathRejection::LiteralBackslash, "literal_backslash"),
            (PolicyPathRejection::EncodedControl, "encoded_control"),
            (
                PolicyPathRejection::UnrepresentableEscape,
                "unrepresentable_escape",
            ),
            (
                PolicyPathRejection::AmbiguousDotSegment,
                "ambiguous_dot_segment",
            ),
            (
                PolicyPathRejection::LiteralDotSegment,
                "literal_dot_segment",
            ),
            (PolicyPathRejection::EmptySegment, "empty_segment"),
        ] {
            assert_eq!(variant.reason(), reason);
        }
    }

    #[test]
    fn non_canonical_reason_flags_values_that_can_never_match() {
        for (path, reason) in [
            ("/admin", None),
            ("*", None),
            ("/v1.0/reports", None),
            ("/api/", None),
            ("/api/v1;version=2", None),
            ("/api/../admin", Some("literal_dot_segment")),
            ("/api/..;/admin", Some("literal_dot_segment")),
            ("/api/.;x", Some("literal_dot_segment")),
            ("/api/..%3B/admin", Some("ambiguous_dot_segment")),
            ("/api//admin", Some("empty_segment")),
            ("//api", Some("empty_segment")),
            ("/api/;x/y", Some("empty_segment")),
            ("/api\\admin", Some("literal_backslash")),
            (
                "/%61dmin",
                Some("percent-escapes that canonicalize to a different path"),
            ),
            ("/api%2Fadmin", Some("encoded_separator")),
            ("/api%252Fadmin", Some("double_encoding")),
            ("/api%2", Some("invalid_escape")),
            ("/api%20name", Some("unrepresentable_escape")),
            ("/api%7bname", Some("unrepresentable_escape")),
            ("/caf%C3%A9", Some("unrepresentable_escape")),
        ] {
            assert_eq!(non_canonical_policy_path_reason(path), reason, "{path:?}");
        }
    }
}
