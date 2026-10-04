//! Pure validation and response binding for released Edge G01 records.
//!
//! This module authenticates nothing. [`BoundRecord::finding`] always yields
//! at most `likely`. Only the CLI's successful, authenticated admin GET may
//! promote that finding (ADR 0009). JSON trust flags are never consulted.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::catalog::{
    EDGE_GATEWAY_ERROR_TOKENS, edge_diagnostic_ref_replica, is_edge_diagnostic_ref,
};
use crate::model::{Confidence, Evidence, EvidenceSource, Finding, Owner, Severity, SourceScope};

/// Largest observation or lookup body accepted, independently of report limits.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
/// Wire version in the released contract.
pub const SCHEMA_VERSION: &str = "ferrum.diagnostic_ref.v1";
/// Record fields required even when their values are null.
pub const REQUIRED_KEYS: &[&str] = &[
    "schema_version",
    "ref",
    "namespace",
    "created_at",
    "expires_at",
    "protocol",
    "status",
    "gateway_error",
    "detail_available",
    "detail",
];
/// Fields required in non-null detail.
pub const DETAIL_REQUIRED_KEYS: &[&str] = &[
    "error_class",
    "body_error_class",
    "rejection_phase",
    "route_timeout_phase",
    "backend_dispatch",
    "proxy_id",
    "backend_target",
    "duration_bucket",
];
/// Client protocols in the released schema.
pub const PROTOCOLS: &[&str] = &["http1", "http2", "http3"];
/// Released dispatch states; attempt records exclude `not_dispatched`.
pub const DISPATCH: &[&str] = &[
    "not_dispatched",
    "pre_wire_failure",
    "ambiguous_failure",
    "backend_response",
];
/// Coarse duration labels, never converted into measurements.
pub const DURATION_BUCKETS: &[&str] = &[
    "lt_10ms", "lt_100ms", "lt_1s", "lt_10s", "ge_10s", "unknown",
];
/// Granular classes from the pinned gateway-errors vocabulary.
pub const ERROR_CLASSES: &[&str] = &[
    "connection_timeout",
    "connection_refused",
    "connection_reset",
    "connection_closed",
    "dns_lookup_error",
    "tls_error",
    "read_write_timeout",
    "client_disconnect",
    "protocol_error",
    "response_body_too_large",
    "gateway_buffer_capacity",
    "request_body_too_large",
    "connection_pool_error",
    "port_exhaustion",
    "graceful_remote_close",
    "dispatch_policy_rejected",
    "backend_connection_limit",
    "trust_withdrawn",
    "request_error",
];
/// Schema vocabulary for admission fences.
pub const REJECTION_PHASES: &[&str] = &[
    "circuit_breaker_open",
    "concurrency_limit",
    "overload",
    "config_stale",
];
/// Schema vocabulary for route deadlines.
pub const ROUTE_TIMEOUT_PHASES: &[&str] = &["before_dispatch", "dispatch", "retry_backoff"];
/// Schema vocabulary for rejection authors.
pub const REJECTION_SOURCES: &[&str] = &["plugin", "gateway", "routing"];
/// Schema vocabulary for typed TLS failures.
pub const TLS_FAILURES: &[&str] = &[
    "certificate_verification",
    "alert_received",
    "no_certificates_presented",
    "peer_incompatible",
    "peer_misbehaved",
    "invalid_message",
    "unexpected_message",
    "decrypt_error",
    "no_application_protocol",
    "invalid_crl",
    "other",
];

/// An explicit operator-supplied observation of one client response. This
/// input is separate from service reports and must come from a trusted
/// capture. No serialized provenance switch can replace that requirement.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientObservation {
    reference: String,
    namespace: String,
    status: u16,
    gateway_error: Option<String>,
    protocol: String,
    request_started_at: String,
    response_received_at: String,
}

/// Validation errors deliberately contain no supplied values or JSON errors.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RecordError(&'static str);

type Result<T> = std::result::Result<T, RecordError>;

impl ClientObservation {
    /// The validated reference, safe as one URL path segment.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    fn window(&self) -> Result<(i128, i128)> {
        let start = timestamp(&self.request_started_at).ok_or(RecordError("invalid start time"))?;
        let end = timestamp(&self.response_received_at).ok_or(RecordError("invalid end time"))?;
        // Explicitly bounded capture window; no automatic clock-skew expansion.
        if end < start || end - start > 300_000_000_000 {
            return Err(RecordError(
                "observation window must be within 0..=300 seconds",
            ));
        }
        Ok((start, end))
    }
}

/// Parse a bounded, explicit observation. A missing token field is refused:
/// `gateway_error: null` explicitly means the header was absent.
pub fn parse_observation(bytes: &[u8]) -> Result<ClientObservation> {
    let value = json(bytes)?;
    required(object(&value)?, &["gateway_error"])?;
    let observation: ClientObservation =
        serde_json::from_slice(bytes).map_err(|_| RecordError("invalid client observation"))?;
    if !is_edge_diagnostic_ref(&observation.reference)
        || !text_ok(&observation.namespace)
        || !(100..=599).contains(&observation.status)
        || !PROTOCOLS.contains(&observation.protocol.as_str())
        || observation
            .gateway_error
            .as_deref()
            .is_some_and(|v| !label_ok(v))
    {
        return Err(RecordError("invalid client observation fields"));
    }
    observation.window()?;
    Ok(observation)
}

/// A structurally validated record bound to explicit response facts. It
/// cannot be deserialized and carries no claim about its transport.
#[derive(Debug)]
pub struct BoundRecord {
    value: Value,
    known: bool,
}

impl BoundRecord {
    /// Whether all interpreted vocabulary is known to this reader.
    pub fn known_vocabulary(&self) -> bool {
        self.known
    }

    /// A bounded, redacted claim about the record, never authentication.
    /// Raw operator configuration and extension fields are never exported.
    pub fn finding(&self) -> Finding {
        let mut evidence = Vec::new();
        for key in ["ref", "status", "gateway_error", "protocol", "created_at"] {
            evidence.push(Evidence {
                source: EvidenceSource::GatewayDetail,
                key: format!("edge.record.{key}"),
                value: self.value[key]
                    .as_str()
                    .map_or_else(|| self.value[key].to_string(), str::to_owned),
                attempt: None,
            });
        }
        if let Some(detail) = self.value["detail"].as_object() {
            for key in [
                "backend_dispatch",
                "error_class",
                "body_error_class",
                "route_timeout_phase",
            ] {
                if let Some(text) = detail[key].as_str() {
                    // Unknown labels affect the confidence ceiling but are
                    // not echoed, just like all free-text record fields.
                    if DISPATCH.contains(&text)
                        || ERROR_CLASSES.contains(&text)
                        || ROUTE_TIMEOUT_PHASES.contains(&text)
                    {
                        evidence.push(Evidence {
                            source: EvidenceSource::GatewayDetail,
                            key: format!("edge.record.detail.{key}"),
                            value: text.to_owned(),
                            attempt: None,
                        });
                    }
                }
            }
        }
        Finding {
            code: "alloy.edge.bound_diagnostic_record".into(),
            rule_id: "alloy.r008".into(),
            rule_version: 1,
            title: "Edge record matches the observed response".into(),
            explanation: "The record matches the explicit client observation's reference, replica, status, X-Gateway-Error, protocol, namespace, and request time window. Detail fields cite the gateway's recorded classifications; they are not independent measurements.".into(),
            scope: match self.value["detail"]["backend_dispatch"].as_str() {
                Some("not_dispatched") => SourceScope::GatewayAdmission,
                Some(_) => SourceScope::GatewayToUpstream,
                None => SourceScope::Unknown,
            },
            confidence: Confidence::Likely,
            severity: Severity::Info,
            evidence,
            alternatives: vec![
                "A mistaken or substituted client capture can match another response's record.".into(),
            ],
            does_not_prove: vec![
                "The record alone does not authenticate its producer or transport.".into(),
                "It does not prove the client capture's integrity, backend health, a service crash, packet loss, or the root cause of a gateway classification.".into(),
                "It does not link any service report or span, confirm timing attribution, or prove that earlier attempts were not dispatched.".into(),
            ],
            remediation: Vec::new(),
            owner: Owner::GatewayOperator,
            confirm_with: vec![
                "Fetch this record from the explicitly selected Edge admin listener over verified TLS or direct literal-loopback HTTP with a diagnostics:read credential bound to the namespace.".into(),
            ],
            supporting_observations: Vec::new(),
            missing_evidence: if self.known {
                vec!["authenticated admin lookup".into()]
            } else {
                vec!["authenticated admin lookup".into(), "recognized record vocabulary".into()]
            },
        }
    }
}

/// Validate the released schema's interpreted fields and bind the record.
/// Additive fields are ignored; they never establish authentication. Unknown
/// schema enums are refused; unknown granular classes cap findings at likely.
pub fn bind_record(bytes: &[u8], observation: &ClientObservation) -> Result<BoundRecord> {
    let value = json(bytes)?;
    let root = object(&value)?;
    required(root, REQUIRED_KEYS)?;
    if string(root, "schema_version")? != SCHEMA_VERSION {
        return Err(RecordError("unsupported Edge record schema"));
    }
    let reference = string(root, "ref")?;
    if !is_edge_diagnostic_ref(reference) {
        return Err(RecordError("malformed record reference"));
    }
    let replica = optional_string(root, "replica_id")?;
    if replica != edge_diagnostic_ref_replica(reference) {
        return Err(RecordError("record replica does not match reference"));
    }
    let namespace = string(root, "namespace")?;
    if !text_ok(namespace) {
        return Err(RecordError("invalid record namespace"));
    }
    let created = timestamp(string(root, "created_at")?)
        .ok_or(RecordError("invalid record creation time"))?;
    let expires =
        timestamp(string(root, "expires_at")?).ok_or(RecordError("invalid record expiry time"))?;
    if expires < created {
        return Err(RecordError("record expires before creation"));
    }
    enum_value(string(root, "protocol")?, PROTOCOLS)?;
    status(&value["status"])?;
    let token = nullable_string(root, "gateway_error")?;
    if token.is_some_and(|v| !EDGE_GATEWAY_ERROR_TOKENS.iter().any(|(t, _)| *t == v)) {
        return Err(RecordError("unknown gateway error token"));
    }
    let available = value["detail_available"]
        .as_bool()
        .ok_or(RecordError("invalid detail availability"))?;
    if available == value["detail"].is_null() {
        return Err(RecordError("detail availability does not match detail"));
    }
    let known = if available {
        validate_detail(object(&value["detail"])?)?
    } else {
        true
    };
    let (start, end) = observation.window()?;
    if reference != observation.reference
        || namespace != observation.namespace
        || value["status"].as_u64() != Some(u64::from(observation.status))
        || token != observation.gateway_error.as_deref()
        || string(root, "protocol")? != observation.protocol
        || created < start
        || created > end
        || expires < end
    {
        return Err(RecordError(
            "Edge record does not bind to the observed response",
        ));
    }
    Ok(BoundRecord { value, known })
}

fn validate_detail(detail: &Map<String, Value>) -> Result<bool> {
    required(detail, DETAIL_REQUIRED_KEYS)?;
    let mut known = true;
    for key in ["error_class", "body_error_class"] {
        if let Some(class) = nullable_string(detail, key)? {
            label(class)?;
            known &= ERROR_CLASSES.contains(&class);
        }
    }
    for (key, allowed) in [
        ("rejection_phase", REJECTION_PHASES),
        ("route_timeout_phase", ROUTE_TIMEOUT_PHASES),
    ] {
        if let Some(value) = nullable_string(detail, key)? {
            enum_value(value, allowed)?;
        }
    }
    enum_value(string(detail, "backend_dispatch")?, DISPATCH)?;
    enum_value(string(detail, "duration_bucket")?, DURATION_BUCKETS)?;
    for key in ["proxy_id", "backend_target"] {
        if nullable_string(detail, key)?.is_some_and(|v| !text_ok(v)) {
            return Err(RecordError("invalid operator configuration field"));
        }
    }
    if let Some(value) = detail.get("rejection") {
        let rejection = object(value)?;
        enum_value(string(rejection, "source")?, REJECTION_SOURCES)?;
        label(string(rejection, "phase")?)?;
        if let Some(plugin) = optional_string(rejection, "plugin")? {
            label(plugin)?;
        }
        // Phase and plugin are extensible labels. We do not interpret them
        // as known vocabulary, so their presence keeps this reader conservative.
        known = false;
    }
    if let Some(attempts) = detail.get("attempts") {
        let attempts = attempts.as_array().ok_or(RecordError("invalid attempts"))?;
        if attempts.len() > 8 {
            return Err(RecordError("more than eight attempts"));
        }
        let mut previous = 0;
        for value in attempts {
            let attempt = object(value)?;
            let number = attempt.get("attempt").and_then(Value::as_u64);
            let Some(number) = number.filter(|n| *n > previous && *n <= u64::from(u32::MAX)) else {
                return Err(RecordError("invalid attempt order or number"));
            };
            previous = number;
            enum_value(string(attempt, "backend_dispatch")?, &DISPATCH[1..])?;
            if let Some(value) = attempt.get("status") {
                status(value)?;
            }
            if let Some(class) = optional_string(attempt, "error_class")? {
                label(class)?;
                known &= ERROR_CLASSES.contains(&class);
            }
            if let Some(value) = attempt.get("tls") {
                let tls = object(value)?;
                enum_value(string(tls, "failure")?, TLS_FAILURES)?;
                if let Some(reason) = optional_string(tls, "reason")? {
                    label(reason)?;
                    // Free-form in the shared schema; no inferred TLS cause.
                    known = false;
                }
            }
        }
    }
    if detail.get("attempts_omitted").is_some_and(|v| {
        !v.as_u64()
            .is_some_and(|n| (1..=u64::from(u32::MAX)).contains(&n))
    }) {
        return Err(RecordError("invalid omitted attempt count"));
    }
    Ok(known)
}

fn json(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(RecordError("Edge input exceeds 64 KiB"));
    }
    serde_json::from_slice(bytes).map_err(|_| RecordError("invalid Edge input JSON"))
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().ok_or(RecordError("expected an object"))
}

fn required(object: &Map<String, Value>, keys: &[&str]) -> Result<()> {
    if keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(RecordError("missing required record field"));
    }
    Ok(())
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or(RecordError("expected a string"))
}

fn nullable_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match object.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(RecordError("expected a string or null")),
    }
}

fn optional_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    if object.contains_key(key) {
        string(object, key).map(Some)
    } else {
        Ok(None)
    }
}

fn enum_value(value: &str, allowed: &[&str]) -> Result<()> {
    if !allowed.contains(&value) {
        return Err(RecordError("unknown record vocabulary"));
    }
    Ok(())
}

fn status(value: &Value) -> Result<()> {
    if !value.as_u64().is_some_and(|v| (100..=599).contains(&v)) {
        return Err(RecordError("invalid HTTP status"));
    }
    Ok(())
}

fn text_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 300 && !value.chars().any(char::is_control)
}

fn label_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn label(value: &str) -> Result<()> {
    if !label_ok(value) {
        return Err(RecordError("invalid record label"));
    }
    Ok(())
}

/// RFC 3339 to nanoseconds, for binding only. No clocks are subtracted to
/// infer latency. Leap seconds and unknown local offsets fail closed.
fn timestamp(value: &str) -> Option<i128> {
    let b = value.as_bytes();
    if !(20..=35).contains(&b.len()) || !value.is_ascii() {
        return None;
    }
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let digits = |start: usize, end: usize| -> Option<i128> {
        let part = b.get(start..end)?;
        part.iter().try_fold(0, |n, c| {
            c.is_ascii_digit().then(|| n * 10 + i128::from(c - b'0'))
        })
    };
    let year = digits(0, 4)?;
    let month = digits(5, 7)?;
    let day = digits(8, 10)?;
    let hour = digits(11, 13)?;
    let minute = digits(14, 16)?;
    let second = digits(17, 19)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut pos = 19;
    let mut fraction = 0;
    if b[pos] == b'.' {
        pos += 1;
        let start = pos;
        while b.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        let count = pos - start;
        if !(1..=9).contains(&count) {
            return None;
        }
        fraction = digits(start, pos)? * 10_i128.pow(u32::try_from(9 - count).ok()?);
    }
    let offset = match b.get(pos..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let hours = digits(pos + 1, pos + 3)?;
            let minutes = digits(pos + 4, pos + 6)?;
            if hours > 23 || minutes > 59 || (*sign == b'-' && hours == 0 && minutes == 0) {
                return None;
            }
            (hours * 3600 + minutes * 60) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return None,
    };
    let y = year - i128::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some((days * 86400 + hour * 3600 + minute * 60 + second - offset) * 1_000_000_000 + fraction)
}
