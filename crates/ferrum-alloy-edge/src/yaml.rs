//! A minimal, deterministic YAML emitter for generated gateway resources.
//!
//! Output uses block mappings and sequences. Every string scalar is
//! double-quoted with JSON escapes, which YAML 1.2 double-quoted scalars
//! accept verbatim, so no value can change type or inject structure.
//! Mapping keys are emitted bare only when they are simple identifiers.

use std::fmt::Write as _;

use serde_json::Value;

/// Renders `value` as a YAML document.
pub fn to_string(value: &Value) -> String {
    let mut out = String::new();
    emit(value, 0, &mut out, false);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn quoted(text: &str) -> String {
    let json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned());
    if !json.chars().any(needs_yaml_escape) {
        return json;
    }
    // JSON escapes only C0 controls, `"` and `\`. A raw character outside
    // that set is never part of an escape, so it can be replaced in place.
    let mut out = String::with_capacity(json.len() + 8);
    for c in json.chars() {
        if needs_yaml_escape(c) {
            let _ = write!(out, "\\u{:04X}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Characters a YAML parser may not read back verbatim from a double-quoted
/// scalar: DEL, the C1 controls and U+FFFE/U+FFFF are not YAML printable
/// characters, so a parser refuses the whole document (Ferrum Edge's YAML
/// loader does); NEL, LS and PS are line breaks to a YAML 1.1 parser, which
/// would fold them; and a BOM is reserved for the stream start. All are
/// written as `\u` escapes, which both YAML versions decode.
fn needs_yaml_escape(c: char) -> bool {
    matches!(
        u32::from(c),
        0x7F..=0x9F | 0x2028 | 0x2029 | 0xFEFF | 0xFFFE | 0xFFFF
    )
}

fn key(text: &str) -> String {
    // YAML plain-scalar typing is case-insensitive for the core-schema
    // words `True`/`NULL`/`Off`, and `-123` reads back as a number, so
    // anything spelled like a bool, null, or integer is quoted too.
    let reserved_word = ["true", "false", "null", "yes", "no", "on", "off", "y", "n"]
        .iter()
        .any(|word| text.eq_ignore_ascii_case(word));
    // A leading minus followed by a digit starts every signed numeric
    // spelling YAML resolves (`-123`, `-0xFF`, `-1e3`, `-1_000`), so it
    // is quoted whether or not the rest is all digits.
    let digits = text.strip_prefix('-').unwrap_or(text);
    let numeric = digits.bytes().next().is_some_and(|b| b.is_ascii_digit());
    let simple = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && !text.as_bytes()[0].is_ascii_digit()
        && text != "-"
        && !reserved_word
        && !numeric;
    if simple {
        text.to_owned()
    } else {
        quoted(text)
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_owned()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(quoted(s)),
        Value::Array(items) if items.is_empty() => Some("[]".to_owned()),
        Value::Object(map) if map.is_empty() => Some("{}".to_owned()),
        _ => None,
    }
}

fn emit(value: &Value, indent: usize, out: &mut String, inline_first: bool) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (index, (k, v)) in map.iter().enumerate() {
                let prefix = if index == 0 && inline_first {
                    String::new()
                } else {
                    pad.clone()
                };
                match scalar(v) {
                    Some(s) => {
                        let _ = writeln!(out, "{prefix}{}: {s}", key(k));
                    }
                    None => {
                        let _ = writeln!(out, "{prefix}{}:", key(k));
                        emit(v, indent + 2, out, false);
                    }
                }
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (index, item) in items.iter().enumerate() {
                // Like a mapping's first key, the first item of a sequence
                // nested in a sequence continues the parent's `- ` line.
                let prefix = if index == 0 && inline_first {
                    ""
                } else {
                    pad.as_str()
                };
                match scalar(item) {
                    Some(s) => {
                        let _ = writeln!(out, "{prefix}- {s}");
                    }
                    None => {
                        let _ = write!(out, "{prefix}- ");
                        emit(item, indent + 2, out, true);
                    }
                }
            }
        }
        other => {
            let _ = writeln!(out, "{pad}{}", scalar(other).unwrap_or_default());
        }
    }
}
