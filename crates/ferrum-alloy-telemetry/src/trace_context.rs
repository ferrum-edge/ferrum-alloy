//! W3C Trace Context (`traceparent` / `tracestate`) parsing, validation, and
//! identifier generation.
//!
//! Parsing follows <https://www.w3.org/TR/trace-context/>: lowercase hex only,
//! version `ff` invalid, all-zero ids invalid, version `00` has exactly four
//! fields, and future versions may carry additional `-`-prefixed data. This
//! matches Ferrum Edge's `otel_tracing` parser (v0.9.8), so both hops accept
//! and reject the same inputs.
//!
//! Receiving a valid `traceparent` never authenticates the sender. Whether a
//! parsed context is *used* is decided by the trust policy in [`crate::layer`].

use std::fmt;

use http::HeaderMap;
use http::header::HeaderValue;

/// The `traceparent` header name.
pub const TRACEPARENT: &str = "traceparent";
/// The `tracestate` header name.
pub const TRACESTATE: &str = "tracestate";

/// Maximum `tracestate` list members (W3C limit).
pub const MAX_TRACESTATE_MEMBERS: usize = 32;
/// Maximum accepted `tracestate` length. W3C requires propagating at least
/// 512 characters; longer values are dropped rather than truncated so a
/// member is never cut in half.
pub const MAX_TRACESTATE_BYTES: usize = 512;

/// A 16-byte trace id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(pub [u8; 16]);

/// An 8-byte span id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanId(pub [u8; 8]);

impl TraceId {
    /// Generates a random, non-zero trace id.
    pub fn random() -> Self {
        loop {
            let mut bytes = [0u8; 16];
            fill_random(&mut bytes);
            if bytes != [0; 16] {
                return Self(bytes);
            }
        }
    }

    /// Lowercase hex (32 characters).
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

impl SpanId {
    /// Generates a random, non-zero span id.
    pub fn random() -> Self {
        loop {
            let mut bytes = [0u8; 8];
            fill_random(&mut bytes);
            if bytes != [0; 8] {
                return Self(bytes);
            }
        }
    }

    /// Lowercase hex (16 characters).
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({})", self.to_hex())
    }
}
impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({})", self.to_hex())
    }
}
impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Fills `bytes` from the operating system RNG.
///
/// If the OS RNG is unavailable (which `getrandom` documents as extremely
/// rare), falls back to a time-and-counter mix so id generation never panics.
/// Such ids are unique but not unpredictable; trace ids are identifiers, not
/// secrets, and must never be used as credentials.
pub(crate) fn fill_random(bytes: &mut [u8]) {
    if getrandom::fill(bytes).is_ok() {
        return;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    for chunk in bytes.chunks_mut(8) {
        // splitmix64
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let out = z.to_le_bytes();
        chunk.copy_from_slice(&out[..chunk.len()]);
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn parse_hex<const N: usize>(input: &str) -> Option<[u8; N]> {
    let bytes = input.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let hi = lower_hex_value(pair[0])?;
        let lo = lower_hex_value(pair[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn lower_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// A parsed `traceparent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceParent {
    /// Trace id.
    pub trace_id: TraceId,
    /// The sender's span id (our parent).
    pub parent_id: SpanId,
    /// Trace flags byte. Bit 0 is `sampled`.
    pub flags: u8,
}

impl TraceParent {
    /// Returns the `sampled` flag.
    pub fn sampled(&self) -> bool {
        self.flags & 0x01 == 0x01
    }

    /// Formats as a version-00 `traceparent` with only the sampled bit, the
    /// only flag this implementation understands.
    pub fn to_header_value(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id.to_hex(),
            self.parent_id.to_hex(),
            self.flags & 0x01
        )
    }
}

/// Why an incoming trace context was not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceContextError {
    /// No `traceparent` header.
    Missing,
    /// More than one `traceparent` header.
    Duplicate,
    /// The value is malformed.
    Malformed,
}

impl TraceContextError {
    /// Stable label for metrics and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Duplicate => "duplicate",
            Self::Malformed => "malformed",
        }
    }
}

/// Parses one `traceparent` value.
pub fn parse_traceparent(value: &str) -> Result<TraceParent, TraceContextError> {
    // version "-" trace-id "-" parent-id "-" flags, optionally followed by
    // "-" and future-version data.
    if value.len() < 55 {
        return Err(TraceContextError::Malformed);
    }
    let bytes = value.as_bytes();
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return Err(TraceContextError::Malformed);
    }
    let version = parse_hex::<1>(&value[0..2]).ok_or(TraceContextError::Malformed)?[0];
    if version == 0xff {
        return Err(TraceContextError::Malformed);
    }
    if version == 0x00 && value.len() != 55 {
        return Err(TraceContextError::Malformed);
    }
    if version > 0x00 && value.len() > 55 && bytes[55] != b'-' {
        return Err(TraceContextError::Malformed);
    }
    let trace_id = parse_hex::<16>(&value[3..35]).ok_or(TraceContextError::Malformed)?;
    let parent_id = parse_hex::<8>(&value[36..52]).ok_or(TraceContextError::Malformed)?;
    let flags = parse_hex::<1>(&value[53..55]).ok_or(TraceContextError::Malformed)?[0];
    if trace_id == [0; 16] || parent_id == [0; 8] {
        return Err(TraceContextError::Malformed);
    }
    Ok(TraceParent {
        trace_id: TraceId(trace_id),
        parent_id: SpanId(parent_id),
        flags,
    })
}

/// Extracts the single `traceparent` from `headers`.
///
/// Header names are case-insensitive in `http`. More than one value is
/// treated as invalid rather than picking one, matching Ferrum Edge.
pub fn extract_traceparent(headers: &HeaderMap) -> Result<TraceParent, TraceContextError> {
    let mut values = headers.get_all(TRACEPARENT).iter();
    let first = values.next().ok_or(TraceContextError::Missing)?;
    if values.next().is_some() {
        return Err(TraceContextError::Duplicate);
    }
    let value = first.to_str().map_err(|_| TraceContextError::Malformed)?;
    parse_traceparent(value.trim_matches(|c| c == ' ' || c == '\t'))
}

/// A validated `tracestate` value, kept opaque.
///
/// Alloy never writes its own members into `tracestate`; Ferrum metadata is
/// recorded as bounded span attributes instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceState(String);

impl TraceState {
    /// The validated header value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the value as a header value.
    pub fn to_header_value(&self) -> Option<HeaderValue> {
        HeaderValue::from_str(&self.0).ok()
    }
}

/// Validates and combines all `tracestate` header values. Returns `None` for
/// a missing, oversized, or invalid list, which then is not propagated.
pub fn extract_tracestate(headers: &HeaderMap) -> Option<TraceState> {
    let mut combined = String::new();
    for value in headers.get_all(TRACESTATE) {
        let value = value.to_str().ok()?;
        if !combined.is_empty() {
            combined.push(',');
        }
        combined.push_str(value);
        if combined.len() > MAX_TRACESTATE_BYTES {
            return None;
        }
    }
    validate_tracestate(&combined)
}

/// Validates a `tracestate` list per the W3C grammar.
pub fn validate_tracestate(value: &str) -> Option<TraceState> {
    if value.len() > MAX_TRACESTATE_BYTES {
        return None;
    }
    let mut members = Vec::new();
    for member in value.split(',') {
        let member = member.trim_matches(|c| c == ' ' || c == '\t');
        if member.is_empty() {
            continue;
        }
        let (key, val) = member.split_once('=')?;
        if !valid_tracestate_key(key) || !valid_tracestate_value(val) {
            return None;
        }
        if members.iter().any(|(k, _): &(&str, &str)| *k == key) {
            return None;
        }
        members.push((key, val));
        if members.len() > MAX_TRACESTATE_MEMBERS {
            return None;
        }
    }
    if members.is_empty() {
        return None;
    }
    let normalized = members
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    Some(TraceState(normalized))
}

fn valid_tracestate_key(key: &str) -> bool {
    // key = simple-key / multi-tenant-key
    // simple-key = lcalpha 0*255( lcalpha / DIGIT / "_" / "-"/ "*" / "/" )
    // multi-tenant-key = tenant-id "@" system-id
    fn simple(key: &str, max: usize, first_may_be_digit: bool) -> bool {
        let bytes = key.as_bytes();
        let Some(first) = bytes.first() else {
            return false;
        };
        let first_ok = first.is_ascii_lowercase() || (first_may_be_digit && first.is_ascii_digit());
        first_ok
            && bytes.len() <= max
            && bytes[1..].iter().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'-' | b'*' | b'/')
            })
    }
    match key.split_once('@') {
        None => simple(key, 256, false),
        Some((tenant, system)) => simple(tenant, 241, true) && simple(system, 14, false),
    }
}

fn valid_tracestate_value(value: &str) -> bool {
    // value = 0*255(chr) nblk-chr ; chr = %x20 / nblk-chr ;
    // nblk-chr = %x21-2B / %x2D-3C / %x3E-7E
    let bytes = value.as_bytes();
    let nblk = |b: u8| matches!(b, 0x21..=0x2b | 0x2d..=0x3c | 0x3e..=0x7e);
    !bytes.is_empty()
        && bytes.len() <= 256
        && bytes.last().is_some_and(|b| nblk(*b))
        && bytes.iter().all(|b| *b == 0x20 || nblk(*b))
}
