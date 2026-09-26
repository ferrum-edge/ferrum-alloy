//! Validated request (correlation) identifiers.
//!
//! The accepted alphabet and length match the Ferrum Edge `correlation_id`
//! plugin (v0.9.7): at most 256 bytes of `[A-Za-z0-9._-]`. An id accepted by
//! Edge is therefore preserved by Alloy, and an id Alloy generates is one Edge
//! would accept. A request id is a correlation aid, never a credential.

use std::fmt;

use crate::trace_context::fill_random;

/// Default request id header, matching Ferrum Edge's `correlation_id` default.
pub const DEFAULT_REQUEST_ID_HEADER: &str = "x-request-id";
/// Maximum accepted length in bytes.
pub const MAX_REQUEST_ID_BYTES: usize = 256;

/// A validated request id.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Validates an incoming value.
    pub fn parse(value: &str) -> Option<Self> {
        valid(value).then(|| Self(value.to_owned()))
    }

    /// Generates a new id in hyphenated UUIDv4 form.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        fill_random(&mut bytes);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self(format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        ))
    }

    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId({})", self.0)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_ID_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// How the request id for a request was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestIdSource {
    /// A valid incoming id was accepted.
    Accepted,
    /// No incoming id; a new one was generated.
    Generated,
    /// The incoming id was invalid and was replaced.
    ReplacedInvalid,
    /// The incoming id came from an untrusted peer under a trusted-only
    /// policy and was replaced.
    ReplacedUntrusted,
}

impl RequestIdSource {
    /// Stable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Generated => "generated",
            Self::ReplacedInvalid => "replaced_invalid",
            Self::ReplacedUntrusted => "replaced_untrusted",
        }
    }
}
