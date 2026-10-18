//! Bounded parsing and validation of untrusted report files.

#[cfg(test)]
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::Value;

use crate::catalog;
use crate::model::{
    Availability, CollectionMethod, DiagnosticReport, ObservationKind, SCHEMA_MAJOR, SCHEMA_MINOR,
    SCHEMA_NAME, Trust, Verification,
};

/// Input bounds. Every limit applies before or during parsing so a hostile
/// file cannot force unbounded work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum input size in bytes.
    pub max_bytes: usize,
    /// Maximum JSON nesting depth. Values above 127 are clamped to 127:
    /// that is the deepest nesting the JSON parser itself accepts (its
    /// recursion limit rejects depth 128), so a larger setting could
    /// never take effect.
    pub max_depth: usize,
    /// Maximum number of observations.
    pub max_observations: usize,
    /// Maximum number of supplied findings.
    pub max_findings: usize,
    /// Maximum length of any string value, in bytes.
    pub max_string_bytes: usize,
    /// Maximum attributes per observation.
    pub max_attributes: usize,
    /// Maximum wall-clock span covered by all intervals, in nanoseconds.
    pub max_time_range_nanos: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            max_depth: 32,
            max_observations: 5_000,
            max_findings: 1_000,
            max_string_bytes: 2_048,
            max_attributes: 32,
            max_time_range_nanos: 24 * 60 * 60 * 1_000_000_000,
        }
    }
}

/// Why a report could not be accepted at all.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReportError {
    /// The input exceeds a hard bound.
    #[error("report exceeds limit: {0}")]
    TooLarge(String),
    /// The input is not valid JSON.
    #[error("report is not valid JSON: {0}")]
    InvalidJson(String),
    /// The `schema` field is missing or names a different schema.
    #[error("unsupported schema {found:?}; expected {SCHEMA_NAME:?}")]
    WrongSchema {
        /// What the report declared.
        found: Option<String>,
    },
    /// The major version is not supported.
    #[error(
        "unsupported schema_version {found:?}; this reader supports major version {SCHEMA_MAJOR}"
    )]
    UnsupportedVersion {
        /// What the report declared.
        found: String,
    },
    /// The structure does not match the schema.
    #[error("report does not match schema: {0}")]
    InvalidStructure(String),
    /// Semantic validation found errors.
    #[error("report failed validation with {} error(s)", .0.len())]
    Invalid(Vec<Issue>),
}

/// Most issues (errors or warnings) one parse reports, plus one marker
/// issue saying the rest were suppressed. Validation runs to completion
/// either way; only the reporting is bounded, so output size stays
/// proportional to this cap rather than to the input.
const MAX_ISSUES: usize = 100;

/// Most bytes of an untrusted `schema`/`schema_version` value echoed in
/// an error. A longer value is omitted entirely, never truncated to a
/// prefix: a prefix of a reflected credential would survive downstream
/// redactors that match the whole credential.
const MAX_FOUND_BYTES: usize = 128;

fn truncate_found(value: &str) -> String {
    if value.len() <= MAX_FOUND_BYTES {
        return value.to_owned();
    }
    format!("<{} bytes>", value.len())
}

/// Most bytes of one issue's path or message. Issue counts are capped
/// by [`MAX_ISSUES`]; this bounds each issue too, so total reported
/// output stays proportional to the caps rather than to the input
/// (a warning joining thousands of long unrecognized keys, say). Oversized
/// text is omitted entirely: a retained prefix could split a credential
/// before a downstream redactor has a chance to match it.
const MAX_ISSUE_BYTES: usize = 512;

fn bound_issue_text(text: String) -> String {
    if text.len() <= MAX_ISSUE_BYTES {
        return text;
    }
    format!("<{} bytes omitted>", text.len())
}

/// Names up to eight short keys and counts the rest. Long keys are
/// omitted entirely, never shortened to a potentially credential-bearing
/// prefix. Complete short keys remain available to downstream redactors.
fn key_preview<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut more = 0usize;
    for key in keys {
        if shown.len() < 8 {
            shown.push(if key.len() <= 64 {
                key.clone()
            } else {
                format!("<{}-byte key omitted>", key.len())
            });
        } else {
            more += 1;
        }
    }
    let mut preview = shown.join(", ");
    if more > 0 {
        preview.push_str(&format!(", and {more} more"));
    }
    preview
}

fn push_issue(list: &mut Vec<Issue>, issue: Issue, suppressed: &'static str) {
    if list.len() < MAX_ISSUES {
        list.push(Issue {
            path: bound_issue_text(issue.path),
            message: bound_issue_text(issue.message),
        });
    } else if list.len() == MAX_ISSUES {
        list.push(Issue {
            path: String::new(),
            message: suppressed.to_owned(),
        });
    }
}

fn warn(warnings: &mut Vec<Issue>, path: &str, message: String) {
    push_issue(
        warnings,
        Issue {
            path: path.to_owned(),
            message,
        },
        "further warnings suppressed",
    );
}

/// A validation problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// JSON-pointer-like location.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

/// A successfully parsed report plus everything the reader had to adjust.
#[derive(Debug, Clone)]
pub struct ParsedReport {
    /// The report, with provenance downgraded for offline reading. Its
    /// `findings` are always empty: supplied findings are discarded, and
    /// findings come only from running [`analyze`](crate::rules::analyze) on
    /// the observations.
    pub report: DiagnosticReport,
    /// Non-fatal problems: unknown names, newer minor version, downgraded trust.
    pub warnings: Vec<Issue>,
    /// The verification state the file claimed before downgrading.
    pub claimed_verification: Verification,
}

/// Parses a report read from a file or other offline source.
///
/// Offline input cannot be authenticated, so regardless of what the report
/// claims, its collection is treated as `unverified` and every `verified`
/// observation is downgraded to `unverified`. The original claim is returned
/// in [`ParsedReport::claimed_verification`] and reported as a warning.
///
/// Supplied findings are conclusions the file asserts, not evidence. They
/// are still bounded by [`Limits::max_findings`] and must deserialize, but
/// they are then discarded, with a warning that counts them: the returned
/// report never carries them. Recompute findings with
/// [`analyze`](crate::rules::analyze).
pub fn parse_offline(input: &[u8], limits: &Limits) -> Result<ParsedReport, ReportError> {
    if input.len() > limits.max_bytes {
        return Err(ReportError::TooLarge(format!(
            "{} bytes > {} bytes",
            input.len(),
            limits.max_bytes
        )));
    }
    check_depth(input, limits.max_depth.min(127))?;

    let value: Value =
        serde_json::from_slice(input).map_err(|e| ReportError::InvalidJson(e.to_string()))?;
    let mut warnings = Vec::new();
    check_header(&value, &mut warnings)?;
    check_string_lengths(&value, &mut Vec::new(), limits.max_string_bytes)?;

    let mut report: DiagnosticReport =
        serde_json::from_value(value).map_err(|e| ReportError::InvalidStructure(e.to_string()))?;

    let errors = validate(&report, limits, &mut warnings);
    if !errors.is_empty() {
        return Err(ReportError::Invalid(errors));
    }

    let claimed_verification = report.collection.verification.clone();
    if claimed_verification != Verification::Unverified {
        warn(
            &mut warnings,
            "/collection/verification",
            format!(
                "report claims verification {:?}; offline input cannot be authenticated and is treated as unverified",
                claimed_verification.as_str()
            ),
        );
    }
    report.collection.verification = Verification::Unverified;
    if report.collection.method == CollectionMethod::LiveExport {
        report
            .collection
            .notes
            .push("read from an offline file; the live-export claim was not verified".to_owned());
    }
    let mut downgraded = 0usize;
    for observation in &mut report.observations {
        if observation.trust == Trust::Verified {
            observation.trust = Trust::Unverified;
            downgraded += 1;
        }
    }
    if downgraded > 0 {
        warn(
            &mut warnings,
            "/observations",
            format!("{downgraded} observation(s) claimed verified trust; treated as unverified"),
        );
    }
    // Supplied findings never leave the parser, so no caller can mistake
    // them for conclusions recomputed from the downgraded observations.
    let supplied = std::mem::take(&mut report.findings);
    if !supplied.is_empty() {
        warn(
            &mut warnings,
            "/findings",
            format!(
                "{} supplied finding(s) discarded; findings are recomputed from observations",
                supplied.len()
            ),
        );
    }

    Ok(ParsedReport {
        report,
        warnings,
        claimed_verification,
    })
}

/// Checks that `report`, serialized as compact JSON, is accepted by
/// [`parse_offline`] under `limits`.
///
/// Anything that produces report files uses this so that what it writes can
/// be read back. Serialization stops once the output exceeds
/// `limits.max_bytes`, so an oversized report is never buffered in full.
pub fn check_report(report: &DiagnosticReport, limits: &Limits) -> Result<(), ReportError> {
    let mut out = BoundedWriter {
        bytes: Vec::new(),
        max: limits.max_bytes,
    };
    if let Err(e) = serde_json::to_writer(&mut out, report) {
        let error = if e.is_io() {
            ReportError::TooLarge(format!("more than {} bytes serialized", limits.max_bytes))
        } else {
            ReportError::InvalidStructure(e.to_string())
        };
        return Err(error);
    }
    parse_offline(&out.bytes, limits).map(|_| ())
}

/// A buffer that refuses to grow past `max` bytes.
struct BoundedWriter {
    bytes: Vec<u8>,
    max: usize,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.max {
            return Err(std::io::Error::other("byte limit reached"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn check_header(value: &Value, warnings: &mut Vec<Issue>) -> Result<(), ReportError> {
    let schema = value.get("schema").and_then(Value::as_str);
    if schema != Some(SCHEMA_NAME) {
        return Err(ReportError::WrongSchema {
            found: schema.map(truncate_found),
        });
    }
    let version = value
        .get("schema_version")
        .and_then(Value::as_str)
        .ok_or_else(|| ReportError::UnsupportedVersion {
            found: "<missing>".into(),
        })?;
    let (major, minor) = parse_version(version).ok_or_else(|| ReportError::UnsupportedVersion {
        found: truncate_found(version),
    })?;
    if major != SCHEMA_MAJOR {
        return Err(ReportError::UnsupportedVersion {
            found: truncate_found(version),
        });
    }
    if minor > SCHEMA_MINOR {
        warn(
            warnings,
            "/schema_version",
            format!(
                "report uses minor version {minor}, newer than {SCHEMA_MINOR}; unrecognized fields are preserved but not interpreted"
            ),
        );
    }
    Ok(())
}

fn parse_version(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    let valid = |s: &str| !s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit());
    if !valid(major) || !valid(minor) {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Rejects JSON nested deeper than `max_depth` without building a tree.
fn check_depth(input: &[u8], max_depth: usize) -> Result<(), ReportError> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in input {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max_depth {
                    return Err(ReportError::TooLarge(format!(
                        "nesting depth exceeds {max_depth}"
                    )));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// One step of the location being checked. Keys are borrowed from the value,
/// so the walk costs the same however long the ancestor keys are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

/// The JSON-pointer-like location of `path`: `""` for the root, otherwise
/// `/` before each key or index. Only built when reporting an error.
fn pointer(path: &[Segment<'_>]) -> String {
    #[cfg(test)]
    POINTER_SEGMENTS_VISITED.with(|visited| visited.set(visited.get() + path.len()));

    let mut out = String::new();
    for segment in path {
        match segment {
            Segment::Key(key) => {
                out.push('/');
                for c in key.chars() {
                    if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                        let _ = write!(out, "{}", c.escape_default());
                    } else {
                        out.push(c);
                    }
                }
            }
            Segment::Index(index) => {
                let _ = write!(out, "/{index}");
            }
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    static POINTER_SEGMENTS_VISITED: Cell<usize> = const { Cell::new(0) };
}

/// Rejects any string or object key longer than `max` bytes. `path` is a
/// stack of borrowed segments, pushed and popped on the way, so each node
/// costs constant work and the location is formatted only for the error.
fn check_string_lengths<'a>(
    value: &'a Value,
    path: &mut Vec<Segment<'a>>,
    max: usize,
) -> Result<(), ReportError> {
    match value {
        Value::String(s) if s.len() > max => Err(ReportError::TooLarge(format!(
            "string at {} is {} bytes (limit {max})",
            pointer(path),
            s.len()
        ))),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(Segment::Index(index));
                check_string_lengths(item, path, max)?;
                path.pop();
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, item) in map {
                if key.len() > max {
                    return Err(ReportError::TooLarge(format!(
                        "object key at {} exceeds {max} bytes",
                        pointer(path)
                    )));
                }
                path.push(Segment::Key(key));
                check_string_lengths(item, path, max)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn hours(nanos: u64) -> f64 {
    nanos as f64 / 3_600_000_000_000.0
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && value.bytes().any(|b| b != b'0')
}

/// Semantic validation. Returns errors; pushes warnings.
fn validate(report: &DiagnosticReport, limits: &Limits, warnings: &mut Vec<Issue>) -> Vec<Issue> {
    let mut errors = Vec::new();
    let mut error = |path: String, message: String| {
        push_issue(
            &mut errors,
            Issue { path, message },
            "further validation errors suppressed",
        );
    };

    if report.observations.len() > limits.max_observations {
        error(
            "/observations".into(),
            format!(
                "{} observations exceed the limit of {}",
                report.observations.len(),
                limits.max_observations
            ),
        );
    }
    if report.findings.len() > limits.max_findings {
        error(
            "/findings".into(),
            format!(
                "{} findings exceed the limit of {}",
                report.findings.len(),
                limits.max_findings
            ),
        );
    }
    if let Some(trace_id) = &report.subject.trace_id
        && !valid_hex(trace_id, 32)
    {
        error(
            "/subject/trace_id".into(),
            "must be 32 lowercase hex characters, not all zero".into(),
        );
    }
    if report.collection.verification.is_unrecognized() {
        warn(
            warnings,
            "/collection/verification",
            format!(
                "unrecognized verification {:?}",
                report.collection.verification.as_str()
            ),
        );
    }
    // Unknown members of nested objects are preserved on round-trip (see
    // the flatten fields in the model); say so, as for the top level.
    for (path, unrecognized) in [
        ("/collection", &report.collection.unrecognized),
        (
            "/collection/collector",
            &report.collection.collector.unrecognized,
        ),
        ("/subject", &report.subject.unrecognized),
    ] {
        if !unrecognized.is_empty() {
            warn(
                warnings,
                path,
                format!(
                    "unrecognized fields preserved but not interpreted: {}",
                    key_preview(unrecognized.keys())
                ),
            );
        }
    }

    let mut seen = BTreeSet::new();
    let mut earliest = u64::MAX;
    let mut latest = 0u64;
    for (index, observation) in report.observations.iter().enumerate() {
        let path = format!("/observations/{index}");
        if !valid_id(&observation.id) {
            error(
                format!("{path}/id"),
                "must match [A-Za-z0-9._:-]{1,64}".into(),
            );
        } else if !seen.insert(observation.id.as_str()) {
            error(
                format!("{path}/id"),
                format!("duplicate id {:?}", observation.id),
            );
        }
        if observation.attributes.len() > limits.max_attributes {
            error(
                format!("{path}/attributes"),
                format!(
                    "{} attributes exceed the limit of {}",
                    observation.attributes.len(),
                    limits.max_attributes
                ),
            );
        }
        if let Some(span) = &observation.span {
            if !valid_hex(&span.trace_id, 32) {
                error(
                    format!("{path}/span/trace_id"),
                    "must be 32 lowercase hex characters, not all zero".into(),
                );
            }
            if !valid_hex(&span.span_id, 16) {
                error(
                    format!("{path}/span/span_id"),
                    "must be 16 lowercase hex characters, not all zero".into(),
                );
            }
            if let Some(parent) = &span.parent_span_id
                && !valid_hex(parent, 16)
            {
                error(
                    format!("{path}/span/parent_span_id"),
                    "must be 16 lowercase hex characters, not all zero".into(),
                );
            }
        }
        if let Some(interval) = &observation.interval {
            if interval.end_unix_nano < interval.start_unix_nano {
                error(format!("{path}/interval"), "end precedes start".into());
            }
            earliest = earliest.min(interval.start_unix_nano);
            latest = latest.max(interval.end_unix_nano);
        }
        match (&observation.availability, observation.value) {
            (Availability::Measured, None) if observation.kind == ObservationKind::Measurement => {
                error(
                    format!("{path}/value"),
                    "a measured observation needs a value".into(),
                );
            }
            (Availability::Measured, Some(value)) => {
                if !value.is_finite() {
                    error(format!("{path}/value"), "value must be finite".into());
                } else if value < 0.0 {
                    warn(warnings, &format!("{path}/value"), "negative measured value; it is reported as an inconsistency and never clamped to zero".into(),);
                }
                if observation.unit.is_none() {
                    error(
                        format!("{path}/unit"),
                        "a measured value needs a unit".into(),
                    );
                }
            }
            (availability, Some(_)) if *availability != Availability::Measured => {
                warn(
                    warnings,
                    &format!("{path}/value"),
                    format!(
                        "value ignored because availability is {:?}",
                        availability.as_str()
                    ),
                );
            }
            _ => {}
        }
        for (field, unknown) in [
            ("availability", observation.availability.is_unrecognized()),
            ("kind", observation.kind.is_unrecognized()),
            ("trust", observation.trust.is_unrecognized()),
            ("scope/leg", observation.scope.leg.is_unrecognized()),
        ] {
            if unknown {
                warn(
                    warnings,
                    &format!("{path}/{field}"),
                    "unrecognized value; the observation is not used as evidence for this field"
                        .into(),
                );
            }
        }
        if !catalog::is_known(&observation.name) {
            warn(
                warnings,
                &format!("{path}/name"),
                format!(
                    "unknown observation name {:?}; preserved but not interpreted",
                    observation.name
                ),
            );
        }
        if !observation.unrecognized.is_empty() {
            warn(
                warnings,
                &path,
                format!(
                    "unrecognized fields preserved but not interpreted: {}",
                    key_preview(observation.unrecognized.keys())
                ),
            );
        }
        // Unknown members of nested evidence objects are preserved on
        // round-trip too (see the flatten fields in the model).
        let nested = [
            ("producer", Some(&observation.producer.unrecognized)),
            (
                "boundaries",
                observation.boundaries.as_ref().map(|b| &b.unrecognized),
            ),
            (
                "interval",
                observation.interval.as_ref().map(|i| &i.unrecognized),
            ),
            ("scope", Some(&observation.scope.unrecognized)),
            ("span", observation.span.as_ref().map(|s| &s.unrecognized)),
        ];
        for (member, unrecognized) in nested {
            if let Some(unrecognized) = unrecognized
                && !unrecognized.is_empty()
            {
                warn(
                    warnings,
                    &format!("{path}/{member}"),
                    format!(
                        "unrecognized fields preserved but not interpreted: {}",
                        key_preview(unrecognized.keys())
                    ),
                );
            }
        }
    }
    if earliest != u64::MAX && latest.saturating_sub(earliest) > limits.max_time_range_nanos {
        error(
            "/observations".into(),
            format!(
                "time range too wide: intervals run from {earliest} to {latest} (Unix nanoseconds), {:.1} h apart; the limit is {} h",
                hours(latest.saturating_sub(earliest)),
                hours(limits.max_time_range_nanos)
            ),
        );
    }
    if !report.unrecognized.is_empty() {
        warn(
            warnings,
            "",
            format!(
                "unrecognized top-level fields preserved but not interpreted: {}",
                key_preview(report.unrecognized.keys())
            ),
        );
    }
    errors
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{POINTER_SEGMENTS_VISITED, ReportError, Segment, check_string_lengths, pointer};

    #[test]
    fn the_length_walk_only_keeps_a_stack_of_borrowed_segments() {
        let key = "k".repeat(2_048);
        let mut value = Value::Array(vec![json!(0); 10_000]);
        for _ in 0..30 {
            let mut map = Map::new();
            map.insert(key.clone(), value);
            value = Value::Object(map);
        }
        let mut path = Vec::new();
        check_string_lengths(&value, &mut path, 2_048).unwrap();
        assert!(path.is_empty());
        // One segment per level, whatever the key length or element count.
        assert!(path.capacity() <= 64, "{}", path.capacity());
    }

    #[test]
    fn locations_are_formatted_only_for_errors_and_unchanged() {
        assert_eq!(pointer(&[]), "");
        let path = [
            Segment::Key("observations"),
            Segment::Index(3),
            Segment::Key("name"),
        ];
        assert_eq!(pointer(&path), "/observations/3/name");

        let value = json!({ "a": [{ "b": "xyz" }] });
        let error = check_string_lengths(&value, &mut Vec::new(), 2).unwrap_err();
        let expected = "string at /a/0/b is 3 bytes (limit 2)";
        assert_eq!(error, ReportError::TooLarge(expected.into()));

        let value = json!({ "a": [1, { "long": 1 }] });
        let error = check_string_lengths(&value, &mut Vec::new(), 2).unwrap_err();
        let expected = "object key at /a/1 exceeds 2 bytes";
        assert_eq!(error, ReportError::TooLarge(expected.into()));

        let error = check_string_lengths(&json!("xyz"), &mut Vec::new(), 2).unwrap_err();
        let expected = "string at  is 3 bytes (limit 2)";
        assert_eq!(error, ReportError::TooLarge(expected.into()));
    }

    #[test]
    fn long_ancestor_paths_are_formatted_only_once_after_a_wide_array() {
        let key = "k".repeat(2_048);
        let mut value = Value::Array(
            (0..10_000)
                .map(|index| {
                    if index == 9_999 {
                        json!("x".repeat(2_049))
                    } else {
                        json!(0)
                    }
                })
                .collect(),
        );
        for _ in 0..30 {
            let mut map = Map::new();
            map.insert(key.clone(), value);
            value = Value::Object(map);
        }

        POINTER_SEGMENTS_VISITED.with(|visited| visited.set(0));
        let error = check_string_lengths(&value, &mut Vec::new(), 2_048).unwrap_err();
        let expected_path = format!("/{}/9999", [key.as_str(); 30].join("/"));
        assert_eq!(
            error,
            ReportError::TooLarge(format!(
                "string at {expected_path} is 2049 bytes (limit 2048)"
            ))
        );
        POINTER_SEGMENTS_VISITED.with(|visited| {
            assert_eq!(visited.get(), 31, "each ancestor segment is formatted once")
        });
    }

    #[test]
    fn parser_locations_escape_control_characters_in_keys() {
        let key = "\n\u{1b}";
        let mut map = Map::new();
        map.insert(key.into(), json!("xyz"));
        let error = check_string_lengths(&Value::Object(map), &mut Vec::new(), 2).unwrap_err();
        assert_eq!(
            error,
            ReportError::TooLarge(r"string at /\n\u{1b} is 3 bytes (limit 2)".into())
        );
    }
}
