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
        with_scratch(|rendered| {
            attrs.record(&mut SpanVisitor(rendered));
            let mut extensions = span.extensions_mut();
            if extensions.get_mut::<SpanFields>().is_none() {
                extensions.insert(std::mem::take(rendered));
            }
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        // `Debug` implementations are application code and can re-enter
        // tracing, so render before locking the span extensions.
        with_scratch(|rendered| {
            values.record(&mut SpanVisitor(rendered));
            let mut extensions = span.extensions_mut();
            match extensions.get_mut::<SpanFields>() {
                Some(fields) => fields.merge(rendered),
                None => {
                    extensions.insert(std::mem::take(rendered));
                }
            }
        });
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

/// Runs `f` with a per-thread scratch [`SpanFields`], cleared afterwards,
/// so rendering a `record` call reuses its vectors instead of growing new
/// ones every time. A re-entrant call (from a `Debug` implementation that
/// logs) finds the scratch borrowed and uses a fresh one.
fn with_scratch(f: impl FnOnce(&mut SpanFields)) {
    thread_local! {
        static SCRATCH: RefCell<SpanFields> = const {
            RefCell::new(SpanFields {
                entries: Vec::new(),
                values: Vec::new(),
                stale: 0,
            })
        };
    }
    let mut f = Some(f);
    let _ = SCRATCH.try_with(|scratch| {
        if let Ok(mut scratch) = scratch.try_borrow_mut()
            && let Some(f) = f.take()
        {
            // Clear on entry as well as on exit: if a previous call
            // panicked mid-render (a field's `Debug` can), the exit
            // clear never ran and its partial entries must not leak
            // into this span.
            scratch.clear();
            f(&mut scratch);
            scratch.clear();
        }
    });
    if let Some(f) = f {
        f(&mut SpanFields::default());
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
    fn merge(&mut self, other: &SpanFields) {
        for entry in &other.entries {
            let value = other.values.get(entry.start..entry.end).unwrap_or_default();
            self.set(entry.name, |buf| {
                buf.extend_from_slice(value);
                Ok(())
            });
        }
    }

    /// Empties the scratch, keeping a bounded capacity for the next use.
    fn clear(&mut self) {
        self.entries.clear();
        self.values.clear();
        self.stale = 0;
        if self.values.capacity() > RETAINED_CAPACITY {
            self.values.shrink_to(RETAINED_CAPACITY);
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

/// One queued line. A line that is dropped without being written —
/// the queue was full, the worker had stopped, it sat behind the
/// shutdown message, or its write failed — counts itself on drop, so
/// every loss path is accounted exactly once.
struct QueuedLine {
    bytes: Vec<u8>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    written: bool,
}

impl Drop for QueuedLine {
    fn drop(&mut self) {
        if !self.written {
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Messages for the non-blocking log worker.
enum LogMessage {
    Line(QueuedLine),
    Shutdown,
}

/// A `MakeWriter` that hands each rendered line to a bounded queue drained
/// by a dedicated writer thread, so a slow log consumer (a full pipe, a
/// stalled collector) never blocks the threads serving requests.
///
/// The trade is loss: when the queue is full, the line is dropped and
/// counted in [`NonBlockingMakeWriter::dropped_lines`] instead of
/// blocking. Keep the paired [`LogWorkerGuard`] alive for as long as
/// anything may log; dropping it flushes what is queued and stops the
/// thread, after which lines are counted as dropped.
///
/// Each `write` call is one queue entry: a layer that renders one event
/// with several `write` calls (the bundled [`JsonLayer`] writes once)
/// queues those fragments independently, so fragments of concurrent
/// events can interleave and be lost independently.
#[derive(Debug, Clone)]
pub struct NonBlockingMakeWriter {
    sender: std::sync::mpsc::SyncSender<LogMessage>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl NonBlockingMakeWriter {
    /// Lines dropped so far because the queue was full (or the worker
    /// had stopped).
    pub fn dropped_lines(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Owns the non-blocking log worker thread. Dropping the guard flushes
/// queued lines and joins the thread, so it may block while the writer
/// drains.
#[derive(Debug)]
pub struct LogWorkerGuard {
    sender: Option<std::sync::mpsc::SyncSender<LogMessage>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The terminal stop signal, independent of queue capacity: once set,
    /// writers refuse new lines (counted as dropped) and the worker exits
    /// as soon as it has written what is already queued, even if the
    /// shutdown message never fit in the queue.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the worker thread as its last act, so any number of callers
    /// can wait for it without owning the join handle.
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LogWorkerGuard {
    /// Flushes queued lines and stops the worker, waiting at most
    /// `timeout`. Returns whether the worker finished in time. Either way
    /// the writers stop accepting lines (later ones are counted as
    /// dropped). On a timeout (a sink that stopped taking writes) the
    /// worker is left running detached: it writes what is already queued
    /// if its sink recovers and then exits, and lines it never writes
    /// count as dropped. Unlike dropping the guard, this never blocks past
    /// `timeout`.
    pub fn shutdown_timeout(mut self, timeout: std::time::Duration) -> bool {
        let finished = self.finish_by(std::time::Instant::now() + timeout);
        // Dropping an unfinished handle detaches the thread.
        drop(self.worker.take());
        finished
    }

    /// Stops the worker and waits for it until `deadline`. On a timeout
    /// the handle stays in the guard, so a later call can wait again.
    fn finish_by(&mut self, deadline: std::time::Instant) -> bool {
        self.signal_stop();
        let finished = wait_until_done(std::slice::from_ref(&self.done), deadline);
        if finished {
            self.detach();
        }
        finished
    }

    /// Stops accepting lines and wakes an idle worker, without waiting.
    fn signal_stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(sender) = self.sender.take() {
            // If the queue is full the worker is busy and sees the stop
            // signal after its current write.
            let _ = sender.try_send(LogMessage::Shutdown);
        }
    }

    fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Lets go of a worker that has finished its work. Not joined: a
    /// join also waits for the thread's thread-local destructors (a sink
    /// may own some), which a timed shutdown must not wait for.
    fn detach(&mut self) {
        drop(self.worker.take());
    }
}

/// Waits until every flag is set or `deadline` passes.
fn wait_until_done(
    done: &[std::sync::Arc<std::sync::atomic::AtomicBool>],
    deadline: std::time::Instant,
) -> bool {
    const POLL: std::time::Duration = std::time::Duration::from_millis(1);
    loop {
        if done
            .iter()
            .all(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

impl Drop for LogWorkerGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LogMessage::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Starts a bounded non-blocking writer over `writer`, holding at most
/// `capacity_lines` rendered lines (at least one). See
/// [`NonBlockingMakeWriter`] for the loss semantics.
///
/// # Errors
///
/// Returns the error if the worker thread cannot be spawned; no writer
/// is started in that case.
pub fn non_blocking<W>(
    writer: W,
    capacity_lines: usize,
) -> io::Result<(NonBlockingMakeWriter, LogWorkerGuard)>
where
    W: io::Write + Send + 'static,
{
    non_blocking_with_counter(
        writer,
        capacity_lines,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    )
}

fn non_blocking_with_counter<W>(
    mut writer: W,
    capacity_lines: usize,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> io::Result<(NonBlockingMakeWriter, LogWorkerGuard)>
where
    W: io::Write + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(capacity_lines.max(1));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = std::sync::Arc::clone(&stop);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = std::sync::Arc::clone(&done);
    let worker = std::thread::Builder::new()
        .name("ferrum-alloy-log-writer".into())
        .spawn(move || {
            // Nothing the writer does may create telemetry about itself.
            let _no_tracing = tracing::dispatcher::set_default(&tracing::Dispatch::none());
            let mut write = |message: LogMessage| match message {
                LogMessage::Line(mut line) => {
                    if writer.write_all(&line.bytes).is_ok() {
                        line.written = true;
                    }
                    true
                }
                // Lines still queued behind the shutdown are dropped
                // with the channel and count themselves.
                LogMessage::Shutdown => false,
            };
            while let Ok(message) = receiver.recv() {
                if !write(message) {
                    break;
                }
                // Stopped while this write was in progress (the shutdown
                // message may not have fit in the queue): write what is
                // already queued, then end without waiting for more.
                if stopping.load(std::sync::atomic::Ordering::Acquire) {
                    while let Ok(message) = receiver.try_recv() {
                        if !write(message) {
                            break;
                        }
                    }
                    break;
                }
            }
            let _ = writer.flush();
            drop(writer);
            finished.store(true, std::sync::atomic::Ordering::Release);
        })?;
    let make_writer = NonBlockingMakeWriter {
        sender: sender.clone(),
        dropped,
        stop: std::sync::Arc::clone(&stop),
    };
    Ok((
        make_writer,
        LogWorkerGuard {
            sender: Some(sender),
            worker: Some(worker),
            stop,
            done,
        },
    ))
}

static STDOUT_DROPPED: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicU64>> =
    std::sync::OnceLock::new();

/// Every stdout writer shares one dropped-line counter, so
/// [`dropped_log_lines`] reports the process total no matter which
/// writer is active.
fn stdout_dropped_counter() -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    std::sync::Arc::clone(
        STDOUT_DROPPED.get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))),
    )
}

/// Workers whose guards are held until a shutdown sees them finish. A
/// shutdown keeps every worker it is waiting for registered, so a
/// concurrent shutdown waits for the same workers (within its own
/// deadline) instead of finding nothing and reporting success.
struct WorkerRegistry {
    guards: std::sync::Mutex<Vec<LogWorkerGuard>>,
    /// Told once a shutdown has signalled its workers and is waiting.
    #[cfg(test)]
    waiting: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
}

impl WorkerRegistry {
    const fn new() -> Self {
        Self {
            guards: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            waiting: std::sync::Mutex::new(None),
        }
    }

    fn guards(&self) -> std::sync::MutexGuard<'_, Vec<LogWorkerGuard>> {
        self.guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&self, guard: LogWorkerGuard) {
        self.guards().push(guard);
    }

    /// Stops every registered worker and waits for all of them until
    /// `timeout`. Finished workers are joined and removed; unfinished
    /// ones stay registered.
    fn shutdown(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let flags: Vec<_> = {
            let mut guards = self.guards();
            for guard in guards.iter_mut() {
                guard.signal_stop();
            }
            guards
                .iter()
                .map(|guard| std::sync::Arc::clone(&guard.done))
                .collect()
        };
        #[cfg(test)]
        if let Some(waiting) = self
            .waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = waiting.send(());
        }
        let drained = wait_until_done(&flags, deadline);
        let mut guards = self.guards();
        let (mut finished, unfinished): (Vec<_>, Vec<_>) = std::mem::take(&mut *guards)
            .into_iter()
            .partition(LogWorkerGuard::is_done);
        *guards = unfinished;
        drop(guards);
        for guard in &mut finished {
            guard.detach();
        }
        drained
    }
}

/// Guards of stdout workers started through [`non_blocking_stdout`],
/// held until [`shutdown_nonblocking_stdout`] sees them finish.
static STDOUT_WORKERS: WorkerRegistry = WorkerRegistry::new();

/// Starts a bounded non-blocking writer to standard output. Its worker
/// guard is held process-wide (the subscriber it feeds is global);
/// [`shutdown_nonblocking_stdout`] flushes and stops it. Used by
/// [`crate::init::fmt_layer`] when `logging.non_blocking_queue_lines`
/// is set.
///
/// # Errors
///
/// Returns the error if the worker thread cannot be spawned.
pub fn non_blocking_stdout(capacity_lines: usize) -> io::Result<NonBlockingMakeWriter> {
    let (make_writer, guard) =
        non_blocking_with_counter(io::stdout(), capacity_lines, stdout_dropped_counter())?;
    STDOUT_WORKERS.register(guard);
    Ok(make_writer)
}

/// Flushes and stops every stdout log worker started through
/// [`non_blocking_stdout`] (and so by [`crate::init::fmt_layer`],
/// [`crate::init::init_logging`], and `init_logging_and_otel` when
/// `logging.non_blocking_queue_lines` is set), waiting at most `timeout`.
/// Call it last on exit, after the final events are logged: lines logged
/// afterwards are dropped and counted. Returns whether every worker
/// finished in time, also when another call is shutting them down
/// concurrently; on a timeout the workers are left to finish in the
/// background (a stalled sink cannot hold shutdown forever, and this
/// never blocks past `timeout`), stay registered so a later call can wait
/// again, and lines they never write count as dropped.
pub fn shutdown_nonblocking_stdout(timeout: std::time::Duration) -> bool {
    STDOUT_WORKERS.shutdown(timeout)
}

/// Lines dropped by the process-wide non-blocking stdout writers
/// started through [`non_blocking_stdout`] (zero when none was
/// started). Writers started with [`non_blocking`] report through
/// their own handle instead.
pub fn dropped_log_lines() -> u64 {
    STDOUT_DROPPED.get().map_or(0, |counter| {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// The per-event writer handed out by [`NonBlockingMakeWriter`]. Each
/// `write` call is queued as one message, preserving write-call order.
#[derive(Debug)]
pub struct NonBlockingWriter {
    sender: std::sync::mpsc::SyncSender<LogMessage>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl io::Write for NonBlockingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = QueuedLine {
            bytes: buf.to_vec(),
            dropped: std::sync::Arc::clone(&self.dropped),
            written: false,
        };
        // Full or stopped: the refused line counts itself on drop,
        // rather than blocking the caller or erroring per event.
        if !self.stop.load(std::sync::atomic::Ordering::Acquire) {
            let _ = self.sender.try_send(LogMessage::Line(line));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'w> MakeWriter<'w> for NonBlockingMakeWriter {
    type Writer = NonBlockingWriter;

    fn make_writer(&'w self) -> Self::Writer {
        NonBlockingWriter {
            sender: self.sender.clone(),
            dropped: std::sync::Arc::clone(&self.dropped),
            stop: std::sync::Arc::clone(&self.stop),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A writer whose first write blocks until released.
    struct Gated {
        started: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
        sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        first: bool,
    }

    impl io::Write for Gated {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                let _ = self.started.send(());
                let _ = self.release.recv();
            }
            self.sink.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The gated writer, its "write started" signal, its release
    /// handle, and the bytes it wrote.
    type GatedParts = (
        Gated,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    );

    fn gated() -> GatedParts {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = Gated {
            started: started_tx,
            release: release_rx,
            sink: std::sync::Arc::clone(&sink),
            first: true,
        };
        (writer, started_rx, release_tx, sink)
    }

    /// A line queued behind the shutdown message is never written and
    /// counts as dropped. The shutdown message is enqueued directly, so
    /// the order does not depend on thread scheduling.
    #[test]
    fn lines_queued_behind_shutdown_are_counted_as_dropped() {
        use std::io::Write as _;

        let (writer, started, release, sink) = gated();
        let (make_writer, guard) = non_blocking(writer, 4).unwrap();
        let mut log = make_writer.make_writer();
        log.write_all(b"one\n").unwrap();
        // The worker holds "one" inside its blocked write.
        started.recv().unwrap();
        log.write_all(b"two\n").unwrap();
        assert!(
            guard
                .sender
                .as_ref()
                .unwrap()
                .try_send(LogMessage::Shutdown)
                .is_ok()
        );
        log.write_all(b"three\n").unwrap();
        release.send(()).unwrap();
        drop(guard);
        assert_eq!(
            String::from_utf8(sink.lock().unwrap().clone()).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(make_writer.dropped_lines(), 1, "the line behind shutdown");
    }

    /// A sink that never takes its write cannot hold shutdown past its
    /// timeout: the worker is detached and shutdown reports failure.
    #[test]
    fn shutdown_with_a_stalled_sink_returns_by_its_timeout() {
        use std::io::Write as _;

        let (writer, started, release, sink) = gated();
        let (make_writer, guard) = non_blocking(writer, 1).unwrap();
        let mut log = make_writer.make_writer();
        log.write_all(b"one\n").unwrap();
        started.recv().unwrap();
        // The queue (one line) is full behind the stalled write.
        log.write_all(b"two\n").unwrap();
        let begun = std::time::Instant::now();
        let mut guard = guard;
        assert!(!guard.finish_by(begun + std::time::Duration::from_millis(100)));
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(2),
            "shutdown returned by its timeout"
        );
        // Lines after the stop are refused and counted, not queued.
        log.write_all(b"late\n").unwrap();
        assert_eq!(make_writer.dropped_lines(), 1);
        // Once the sink recovers, the worker writes what was queued and
        // ends, although the writers (and so the channel) are still alive.
        release.send(()).unwrap();
        assert!(guard.finish_by(std::time::Instant::now() + std::time::Duration::from_secs(5)));
        assert_eq!(
            String::from_utf8(sink.lock().unwrap().clone()).unwrap(),
            "one\ntwo\n"
        );
        drop(log);
        drop(make_writer);
    }

    /// With the sink taking writes, a timed shutdown drains and reports it.
    #[test]
    fn shutdown_drains_a_healthy_sink() {
        use std::io::Write as _;

        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl io::Write for Shared {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (make_writer, guard) = non_blocking(Shared(std::sync::Arc::clone(&sink)), 8).unwrap();
        let mut log = make_writer.make_writer();
        log.write_all(b"a\n").unwrap();
        log.write_all(b"b\n").unwrap();
        assert!(guard.shutdown_timeout(std::time::Duration::from_secs(5)));
        assert_eq!(sink.lock().unwrap().as_slice(), b"a\nb\n");
        assert_eq!(make_writer.dropped_lines(), 0);
    }

    /// A timed shutdown returns once the worker has written everything,
    /// without waiting for the thread's thread-local destructors (which a
    /// join would): a sink may own one that blocks.
    #[test]
    fn timed_shutdown_does_not_wait_for_thread_local_destructors() {
        use std::io::Write as _;

        struct BlocksOnDrop(std::sync::mpsc::Receiver<()>);
        impl Drop for BlocksOnDrop {
            fn drop(&mut self) {
                let _ = self.0.recv();
            }
        }
        thread_local! {
            static HELD: RefCell<Option<BlocksOnDrop>> = const { RefCell::new(None) };
        }
        struct Sink(Option<std::sync::mpsc::Receiver<()>>);
        impl io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if let Some(release) = self.0.take() {
                    HELD.with(|held| *held.borrow_mut() = Some(BlocksOnDrop(release)));
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (release_tx, release) = std::sync::mpsc::sync_channel(1);
        let (make_writer, guard) = non_blocking(Sink(Some(release)), 4).unwrap();
        make_writer.make_writer().write_all(b"line\n").unwrap();
        let begun = std::time::Instant::now();
        assert!(guard.shutdown_timeout(std::time::Duration::from_secs(5)));
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(2),
            "shutdown did not wait for the thread-local destructor"
        );
        let _ = release_tx.send(());
    }

    /// A shutdown that runs while another is still waiting for a stalled
    /// worker waits for the same worker and reports failure by its own
    /// deadline; it never finds an empty registry and reports success.
    #[test]
    fn concurrent_shutdowns_both_wait_for_in_progress_workers() {
        use std::io::Write as _;

        let (writer, started, release, sink) = gated();
        let (make_writer, guard) = non_blocking(writer, 4).unwrap();
        let registry = std::sync::Arc::new(WorkerRegistry::new());
        registry.register(guard);
        let mut log = make_writer.make_writer();
        log.write_all(b"one\n").unwrap();
        started.recv().unwrap();
        log.write_all(b"two\n").unwrap();

        let (waiting_tx, waiting) = std::sync::mpsc::sync_channel(1);
        *registry.waiting.lock().unwrap() = Some(waiting_tx);
        let first = {
            let registry = std::sync::Arc::clone(&registry);
            std::thread::spawn(move || registry.shutdown(std::time::Duration::from_secs(5)))
        };
        // The first shutdown has signalled the worker and is waiting for
        // it; the worker is stalled, so a second one must report failure
        // (had the first taken the worker out of the registry while it
        // waited, the second would find nothing and report success).
        waiting.recv().unwrap();
        assert!(!registry.shutdown(std::time::Duration::from_millis(50)));
        assert_eq!(registry.guards().len(), 1, "the worker stays registered");
        release.send(()).unwrap();
        assert!(first.join().unwrap(), "the first shutdown saw it finish");
        assert!(registry.guards().is_empty());
        assert!(registry.shutdown(std::time::Duration::ZERO));
        assert_eq!(
            String::from_utf8(sink.lock().unwrap().clone()).unwrap(),
            "one\ntwo\n"
        );
    }

    /// The process-wide stdout workers shut down within the timeout and
    /// report success; a second call has nothing left to drain.
    #[test]
    fn stdout_workers_shut_down_through_the_registry() {
        let _writer = non_blocking_stdout(4).unwrap();
        let timeout = std::time::Duration::from_secs(5);
        assert!(shutdown_nonblocking_stdout(timeout));
        assert!(shutdown_nonblocking_stdout(std::time::Duration::ZERO));
    }

    /// A render that panics midway (a field's `Debug` can) skips the
    /// scratch's exit clear; the next use on this thread must still
    /// start empty instead of inheriting the partial entries.
    #[test]
    fn scratch_is_cleared_after_a_panicking_render() {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_scratch(|fields| {
                fields.set("leaked", |buf| {
                    buf.extend_from_slice(b"1");
                    Ok(())
                });
                panic!("render failed");
            });
        }));
        assert!(panicked.is_err());
        with_scratch(|fields| {
            assert!(
                fields.entries.is_empty(),
                "a panicking render must not leak entries into the next span"
            );
        });
    }
}
