//! A JSON log layer that renders each span field once and each event once.
//!
//! [`JsonLayer`] writes the same lines as tracing-subscriber's JSON `fmt`
//! layer configured with a flattened event, the current span, and no span
//! list, which is the layout [`crate::init::fmt_layer`] has always produced:
//!
//! ```text
//! {"timestamp":"…","level":"INFO",<event fields>,"target":"…","span":{<span fields>,"name":"…"}}
//! ```
//!
//! Event fields keep their callsite order. Span fields are sorted by name,
//! and `span` is omitted when the event has no parent and no span is entered.
//!
//! tracing-subscriber keeps a span's fields as one JSON string. It parses and
//! re-serializes that string on every `Span::record` call, and parses it
//! again for every event. This layer instead keeps each field's rendered JSON
//! value in a span extension: recording renders only the new values, and an
//! event copies the stored bytes. Each event is rendered into a reused
//! per-thread buffer and written with one `write_all`.
//!
//! Two deliberate differences from tracing-subscriber:
//! - A float span field is written from the recorded value. tracing-subscriber
//!   re-parses it, which can change the last digit of a 16- or 17-digit value.
//! - A span field whose `Debug` implementation returns an error is skipped.
//!   tracing-subscriber panics.
//! - U+0085, U+2028, and U+2029 are escaped to keep log lines intact for
//!   consumers that split on Unicode line separators.

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::io::{self, Write as _};

use serde_json::ser::{CompactFormatter, Formatter as _};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Per-thread buffer capacity kept between events. A larger event still
/// formats; its buffer is released afterwards.
const RETAINED_CAPACITY: usize = 64 * 1024;

/// Superseded span values are compacted once they exceed this many bytes and
/// half of the span's value storage.
const COMPACT_AFTER: usize = 1024;

/// Formats events as one JSON object per line.
///
/// Compose it into an application-owned subscriber like any other layer; it
/// installs nothing globally.
#[derive(Debug)]
pub struct JsonLayer<W = fn() -> io::Stdout, T = SystemTime> {
    make_writer: W,
    timer: T,
}

impl JsonLayer {
    /// A layer writing to standard output with RFC 3339 UTC timestamps.
    pub fn new() -> Self {
        Self {
            make_writer: io::stdout,
            timer: SystemTime,
        }
    }
}

impl Default for JsonLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<W, T> JsonLayer<W, T> {
    /// Writes lines to `make_writer` instead of standard output.
    pub fn with_writer<W2>(self, make_writer: W2) -> JsonLayer<W2, T>
    where
        W2: for<'w> MakeWriter<'w> + 'static,
    {
        JsonLayer {
            make_writer,
            timer: self.timer,
        }
    }

    /// Formats the `timestamp` field with `timer`.
    pub fn with_timer<T2: FormatTime>(self, timer: T2) -> JsonLayer<W, T2> {
        JsonLayer {
            make_writer: self.make_writer,
            timer,
        }
    }
}

impl<W, T> JsonLayer<W, T>
where
    W: for<'w> MakeWriter<'w> + 'static,
    T: FormatTime,
{
    fn emit<S>(&self, event: &Event<'_>, ctx: &Context<'_, S>, buf: &mut Vec<u8>)
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        buf.clear();
        let meta = event.metadata();
        if self.format_event(event, ctx, buf).is_err() {
            // The same fallback line as tracing-subscriber.
            buf.clear();
            let _ = writeln!(
                buf,
                "Unable to format the following event. Name: {}; Fields: {:?}",
                meta.name(),
                event.fields()
            );
        }
        let mut writer = self.make_writer.make_writer_for(meta);
        if let Err(error) = writer.write_all(buf) {
            let _ = writeln!(
                io::stderr(),
                "[ferrum-alloy] unable to write a log event: {error}"
            );
        }
        buf.clear();
        buf.shrink_to(RETAINED_CAPACITY);
    }

    fn format_event<S>(
        &self,
        event: &Event<'_>,
        ctx: &Context<'_, S>,
        buf: &mut Vec<u8>,
    ) -> fmt::Result
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let meta = event.metadata();
        buf.extend_from_slice(b"{\"timestamp\":\"");
        self.timer
            .format_time(&mut Writer::new(&mut Escaped(&mut *buf)))?;
        buf.extend_from_slice(b"\",\"level\":\"");
        buf.extend_from_slice(meta.level().as_str().as_bytes());
        buf.push(b'"');

        // Event fields run application `Debug` code, so they are rendered
        // before any span extension is locked.
        let mut visitor = EventVisitor {
            buf: &mut *buf,
            result: Ok(()),
        };
        event.record(&mut visitor);
        visitor.result?;

        buf.extend_from_slice(b",\"target\":");
        write_string(buf, meta.target());

        // tracing-subscriber's choice: the explicit parent, else the current
        // span (also for an explicit root event).
        let span = event
            .parent()
            .and_then(|id| ctx.span(id))
            .or_else(|| ctx.lookup_current());
        if let Some(span) = span {
            buf.extend_from_slice(b",\"span\":{");
            {
                let extensions = span.extensions();
                if let Some(fields) = extensions.get::<SpanFields>() {
                    fields.write_entries(buf);
                }
            }
            buf.extend_from_slice(b"\"name\":");
            write_string(buf, span.name());
            buf.push(b'}');
        }
        buf.extend_from_slice(b"}\n");
        Ok(())
    }
}

impl<S, W, T> Layer<S> for JsonLayer<W, T>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
    T: FormatTime + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        // `Debug` implementations are application code and can re-enter
        // tracing, so render before locking the span extensions.
        let mut rendered = SpanFields::default();
        attrs.record(&mut SpanVisitor(&mut rendered));
        let mut extensions = span.extensions_mut();
        if extensions.get_mut::<SpanFields>().is_none() {
            extensions.insert(rendered);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        // `Debug` implementations are application code and can re-enter
        // tracing, so render before locking the span extensions.
        let mut rendered = SpanFields::default();
        values.record(&mut SpanVisitor(&mut rendered));
        let mut extensions = span.extensions_mut();
        match extensions.get_mut::<SpanFields>() {
            Some(fields) => fields.merge(rendered),
            None => {
                extensions.insert(rendered);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        thread_local! {
            static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        }
        // An event logged from inside a `Debug` implementation finds the
        // buffer borrowed and formats into its own.
        let done = BUF
            .try_with(|buf| match buf.try_borrow_mut() {
                Ok(mut buf) => {
                    self.emit(event, &ctx, &mut buf);
                    true
                }
                Err(_) => false,
            })
            .unwrap_or(false);
        if !done {
            self.emit(event, &ctx, &mut Vec::new());
        }
    }
}

/// A span's fields as rendered JSON values, sorted by field name.
#[derive(Debug, Default)]
struct SpanFields {
    entries: Vec<Entry>,
    values: Vec<u8>,
    /// Bytes of `values` that no entry refers to any more.
    stale: usize,
}

#[derive(Debug)]
struct Entry {
    name: &'static str,
    start: usize,
    end: usize,
}

impl SpanFields {
    /// Sets `name` to the value `render` appends. A later value replaces an
    /// earlier one, as in tracing-subscriber's field map.
    fn set(&mut self, name: &'static str, render: impl FnOnce(&mut Vec<u8>) -> fmt::Result) {
        let start = self.values.len();
        if render(&mut self.values).is_err() {
            self.values.truncate(start);
            return;
        }
        let end = self.values.len();
        match self.entries.binary_search_by(|entry| entry.name.cmp(name)) {
            Ok(index) => {
                if let Some(entry) = self.entries.get_mut(index) {
                    self.stale += entry.end - entry.start;
                    entry.start = start;
                    entry.end = end;
                }
            }
            Err(index) => self.entries.insert(index, Entry { name, start, end }),
        }
        if self.stale > COMPACT_AFTER && self.stale * 2 > self.values.len() {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut values = Vec::with_capacity(self.values.len().saturating_sub(self.stale));
        for entry in &mut self.entries {
            let start = values.len();
            values.extend_from_slice(self.values.get(entry.start..entry.end).unwrap_or_default());
            entry.start = start;
            entry.end = values.len();
        }
        self.values = values;
        self.stale = 0;
    }

    /// Merges fields rendered without holding the span extensions lock.
    fn merge(&mut self, other: SpanFields) {
        for entry in &other.entries {
            let value = other.values.get(entry.start..entry.end).unwrap_or_default();
            self.set(entry.name, |buf| {
                buf.extend_from_slice(value);
                Ok(())
            });
        }
    }

    /// Appends `"name":value,` for every field.
    fn write_entries(&self, buf: &mut Vec<u8>) {
        for entry in &self.entries {
            write_string(buf, entry.name);
            buf.push(b':');
            buf.extend_from_slice(self.values.get(entry.start..entry.end).unwrap_or_default());
            buf.push(b',');
        }
    }
}

/// Renders span fields the way tracing-subscriber's `JsonFields` stores them.
struct SpanVisitor<'a>(&'a mut SpanFields);

impl Visit for SpanVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.set(field.name(), |buf| write_f64(buf, value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.set(field.name(), |buf| write_i64(buf, value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.set(field.name(), |buf| write_u64(buf, value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.set(field.name(), |buf| {
            write_bool(buf, value);
            Ok(())
        });
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.set(field.name(), |buf| {
            write_string(buf, value);
            Ok(())
        });
    }

    /// Bytes become an array of numbers, as `serde_json::Value::from(&[u8])`.
    fn record_bytes(&mut self, field: &Field, value: &[u8]) {
        self.0.set(field.name(), |buf| {
            buf.push(b'[');
            for (index, byte) in value.iter().enumerate() {
                if index > 0 {
                    buf.push(b',');
                }
                write_u64(buf, u64::from(*byte))?;
            }
            buf.push(b']');
            Ok(())
        });
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // tracing-subscriber strips a raw-identifier prefix here, and only
        // for fields recorded through `Debug`.
        let name = field.name();
        let name = name.strip_prefix("r#").unwrap_or(name);
        self.0.set(name, |buf| write_debug(buf, value));
    }
}

/// Appends event fields as `,"name":value`, the way `tracing_serde`'s map
/// visitor serializes them. Byte slices, 128-bit integers, and errors use the
/// default `Visit` methods, which format them with `Debug`.
struct EventVisitor<'a> {
    buf: &'a mut Vec<u8>,
    result: fmt::Result,
}

impl EventVisitor<'_> {
    /// Writes the key, or returns `false` after an earlier field failed.
    fn key(&mut self, field: &Field) -> bool {
        if self.result.is_err() {
            return false;
        }
        self.buf.push(b',');
        write_string(self.buf, field.name());
        self.buf.push(b':');
        true
    }
}

impl Visit for EventVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        if self.key(field) {
            self.result = write_f64(self.buf, value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if self.key(field) {
            self.result = write_i64(self.buf, value);
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if self.key(field) {
            self.result = write_u64(self.buf, value);
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if self.key(field) {
            write_bool(self.buf, value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if self.key(field) {
            write_string(self.buf, value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.key(field) {
            self.result = write_debug(self.buf, value);
        }
    }
}

/// A `fmt::Write` that appends JSON-escaped text to a byte buffer.
struct Escaped<'a>(&'a mut Vec<u8>);

impl fmt::Write for Escaped<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_escaped(self.0, s);
        Ok(())
    }
}

/// Appends `value`'s `Debug` output as a JSON string.
fn write_debug(buf: &mut Vec<u8>, value: &dyn fmt::Debug) -> fmt::Result {
    buf.push(b'"');
    write!(Escaped(&mut *buf), "{value:?}")?;
    buf.push(b'"');
    Ok(())
}

fn write_string(buf: &mut Vec<u8>, value: &str) {
    buf.push(b'"');
    write_escaped(buf, value);
    buf.push(b'"');
}

/// Appends `value` escaped as serde_json does, with line separators escaped:
/// `\"` and `\\`, the short forms `\b`, `\t`, `\n`, `\f`, and `\r`, and
/// `\u00xx` (lowercase hex) for the other bytes below 0x20. The UTF-8 byte
/// sequences for U+0085, U+2028, and U+2029 are also escaped to protect
/// line-oriented consumers. Everything else, including DEL and `/`, is copied
/// unchanged. A `str` is always valid UTF-8, and every byte this matches is
/// ASCII, so the output is valid UTF-8 too.
fn write_escaped(buf: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let mut start = 0;
    let mut index = 0;
    let mut unicode = *b"\\u0000";
    while index < bytes.len() {
        let (escape, width): (&[u8], usize) = match bytes[index] {
            b'"' => (b"\\\"", 1),
            b'\\' => (b"\\\\", 1),
            0x08 => (b"\\b", 1),
            b'\t' => (b"\\t", 1),
            b'\n' => (b"\\n", 1),
            0x0c => (b"\\f", 1),
            b'\r' => (b"\\r", 1),
            0xc2 if bytes.get(index + 1) == Some(&0x85) => (b"\\u0085", 2),
            0xe2 if bytes.get(index + 1) == Some(&0x80) => match bytes.get(index + 2) {
                Some(&0xa8) => (b"\\u2028", 3),
                Some(&0xa9) => (b"\\u2029", 3),
                _ => {
                    index += 1;
                    continue;
                }
            },
            0x00..=0x1f => {
                let byte = bytes[index];
                unicode[4] = b'0' + (byte >> 4);
                unicode[5] = hex_digit(byte & 0x0f);
                (&unicode, 1)
            }
            _ => {
                index += 1;
                continue;
            }
        };
        buf.extend_from_slice(bytes.get(start..index).unwrap_or_default());
        buf.extend_from_slice(escape);
        index += width;
        start = index;
    }
    buf.extend_from_slice(bytes.get(start..).unwrap_or_default());
}

fn hex_digit(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    }
}

fn write_bool(buf: &mut Vec<u8>, value: bool) {
    let text: &[u8] = if value { b"true" } else { b"false" };
    buf.extend_from_slice(text);
}

fn write_u64(buf: &mut Vec<u8>, value: u64) -> fmt::Result {
    CompactFormatter
        .write_u64(buf, value)
        .map_err(|_| fmt::Error)
}

fn write_i64(buf: &mut Vec<u8>, value: i64) -> fmt::Result {
    CompactFormatter
        .write_i64(buf, value)
        .map_err(|_| fmt::Error)
}

/// serde_json's float format; NaN and infinities become `null`, as they do
/// in serde_json.
fn write_f64(buf: &mut Vec<u8>, value: f64) -> fmt::Result {
    if value.is_finite() {
        CompactFormatter
            .write_f64(buf, value)
            .map_err(|_| fmt::Error)
    } else {
        buf.extend_from_slice(b"null");
        Ok(())
    }
}
