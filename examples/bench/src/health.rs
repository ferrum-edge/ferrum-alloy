//! Bounded, instance-owned test observations. No opaque protocol contents are retained.

use std::fmt::Write;
use std::future::Future;
use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const TASK_SLOTS: usize = 160;
const REQUEST_SLOTS: usize = 72;
const SERVER_PROGRESS_ROWS: usize = 4;
const WIRE_CONNECTIONS: usize = 2;
const WIRE_STREAMS: usize = 36;
const WIRE_EVENTS: usize = 64;

const H2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Tx,
    Rx,
}

impl Direction {
    fn index(self) -> usize {
        match self {
            Self::Tx => 0,
            Self::Rx => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WireMark {
    sequence: u64,
    at: Instant,
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameHeader {
    length: u32,
    kind: u8,
    flags: u8,
    stream: u32,
}

impl FrameHeader {
    fn invalid_length(self) -> bool {
        match self.kind {
            2 => self.length != 5,
            3 | 8 => self.length != 4,
            4 if self.flags & 1 != 0 => self.length != 0,
            4 => !self.length.is_multiple_of(6),
            5 => self.length < 4,
            6 => self.length != 8,
            7 => self.length < 8,
            _ => false,
        }
    }

    fn invalid_stream(self) -> bool {
        match self.kind {
            0..=3 | 5 | 9 => self.stream == 0,
            4 | 6 | 7 => self.stream != 0,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FramePoint {
    mark: WireMark,
    header: FrameHeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireValue {
    Reset(u32),
    GoAway { last_stream: u32, reason: u32 },
    Setting { id: u16, value: u32 },
    Window(u32),
}

#[derive(Clone, Copy, Debug)]
struct WireEvent {
    direction: Direction,
    point: FramePoint,
    complete: bool,
    value: Option<WireValue>,
}

#[derive(Clone, Copy, Debug, Default)]
struct DataProgress {
    seen: u64,
    complete: u64,
    bytes: u64,
    length: u32,
    flags: u8,
    first: Option<WireMark>,
    last: Option<WireMark>,
}

#[derive(Clone, Copy, Debug, Default)]
struct StreamDirection {
    headers_seen: u64,
    headers_complete: u64,
    blocks_complete: u64,
    first_header: Option<WireMark>,
    first_complete: Option<WireMark>,
    first_block: Option<WireMark>,
    data: DataProgress,
}

#[derive(Clone, Copy, Debug)]
struct WireStream {
    id: u32,
    directions: [StreamDirection; 2],
    reset: Option<WireMark>,
    reset_reason: Option<u32>,
    first_cancel: Option<WireMark>,
    late: DataProgress,
}

#[derive(Clone, Copy, Debug)]
struct Setting {
    value: u32,
    mark: WireMark,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WireDirection {
    pub(crate) bytes: u64,
    pub(crate) headers_seen: u64,
    pub(crate) frames_complete: u64,
    pub(crate) headers_complete: u64,
    pub(crate) blocks_complete: u64,
    pub(crate) data_complete: u64,
    preface: usize,
    preface_invalid: bool,
    header_bytes: u8,
    header: FrameHeader,
    remaining: Option<u32>,
    numeric: u64,
    setting_id: u16,
    late: bool,
    pub(crate) eof: bool,
    eof_mark: Option<WireMark>,
    pub(crate) invalid_lengths: u64,
    pub(crate) invalid_streams: u64,
    settings_omitted: u64,
    settings_pending: [Option<u32>; 6],
    settings_complete: [Option<Setting>; 6],
    errors: u64,
    last_header: Option<FramePoint>,
    last_complete: Option<FramePoint>,
}

impl WireDirection {
    pub(crate) fn eof_partial(&self) -> bool {
        self.eof && (self.preface < H2_PREFACE.len() || self.header_bytes > 0)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WireState {
    pub(crate) directions: [WireDirection; 2],
    streams: [Option<WireStream>; WIRE_STREAMS],
    events: [Option<WireEvent>; WIRE_EVENTS],
    sequence: u64,
    events_next: usize,
    pub(crate) events_overwritten: u64,
    pub(crate) streams_omitted: u64,
}

impl Default for WireState {
    fn default() -> Self {
        let mut directions = std::array::from_fn(|_| WireDirection::default());
        // The server sends no connection preface string.
        directions[1].preface = H2_PREFACE.len();
        Self {
            directions,
            streams: [None; WIRE_STREAMS],
            events: [None; WIRE_EVENTS],
            sequence: 0,
            events_next: 0,
            events_overwritten: 0,
            streams_omitted: 0,
        }
    }
}

impl WireState {
    pub(crate) fn has_goaway(&self, direction: Direction, last_stream: u32, reason: u32) -> bool {
        self.events.iter().flatten().any(|event| {
            event.direction == direction
                && event.complete
                && matches!(
                    event.value,
                    Some(WireValue::GoAway { last_stream: last, reason: code })
                        if last == last_stream && code == reason
                )
        })
    }

    fn mark(&mut self) -> WireMark {
        self.sequence = self.sequence.saturating_add(1);
        WireMark {
            sequence: self.sequence,
            at: Instant::now(),
        }
    }

    fn stream(&mut self, id: u32, admit: bool) -> Option<&mut WireStream> {
        if id == 0 {
            return None;
        }
        let existing = self
            .streams
            .iter()
            .position(|slot| slot.is_some_and(|stream| stream.id == id));
        let index = existing.or_else(|| {
            admit
                .then(|| self.streams.iter().position(Option::is_none))
                .flatten()
        });
        let Some(index) = index else {
            if admit {
                self.streams_omitted = self.streams_omitted.saturating_add(1);
            }
            return None;
        };
        Some(self.streams[index].get_or_insert(WireStream {
            id,
            directions: [StreamDirection::default(); 2],
            reset: None,
            reset_reason: None,
            first_cancel: None,
            late: DataProgress::default(),
        }))
    }

    fn event(&mut self, event: WireEvent) {
        if self.events[self.events_next].is_some() {
            self.events_overwritten = self.events_overwritten.saturating_add(1);
        }
        self.events[self.events_next] = Some(event);
        self.events_next = (self.events_next + 1) % WIRE_EVENTS;
    }

    fn header_seen(&mut self, direction: Direction) {
        let index = direction.index();
        let mark = self.mark();
        let parser = &mut self.directions[index];
        let header = parser.header;
        parser.headers_seen = parser.headers_seen.saturating_add(1);
        parser.invalid_lengths = parser
            .invalid_lengths
            .saturating_add(u64::from(header.invalid_length()));
        parser.invalid_streams = parser
            .invalid_streams
            .saturating_add(u64::from(header.invalid_stream()));
        parser.remaining = Some(header.length);
        parser.last_header = Some(FramePoint { mark, header });
        parser.late = false;
        if header.kind == 4 {
            parser.settings_pending = [None; 6];
        }
        let mut late = false;
        if let Some(stream) = self.stream(header.stream, true) {
            let progress = &mut stream.directions[index];
            if header.kind == 1 {
                progress.headers_seen = progress.headers_seen.saturating_add(1);
                let _ = progress.first_header.get_or_insert(mark);
            }
            if header.kind == 0 {
                progress.data.seen = progress.data.seen.saturating_add(1);
                progress.data.length = header.length;
                progress.data.flags = header.flags;
                let _ = progress.data.first.get_or_insert(mark);
                progress.data.last = Some(mark);
                late = direction == Direction::Rx && stream.reset.is_some();
                if late {
                    stream.late.seen = stream.late.seen.saturating_add(1);
                    stream.late.length = header.length;
                    stream.late.flags = header.flags;
                    let _ = stream.late.first.get_or_insert(mark);
                    stream.late.last = Some(mark);
                }
            }
        }
        self.directions[index].late = late;
        if header.kind != 0 {
            self.event(WireEvent {
                direction,
                point: FramePoint { mark, header },
                complete: false,
                value: None,
            });
        }
    }

    fn frame_complete(&mut self, direction: Direction) {
        let index = direction.index();
        let mark = self.mark();
        let parser = &mut self.directions[index];
        let header = parser.header;
        let numeric = parser.numeric;
        let late = parser.late;
        parser.frames_complete = parser.frames_complete.saturating_add(1);
        parser.headers_complete = parser
            .headers_complete
            .saturating_add(u64::from(header.kind == 1));
        let block = matches!(header.kind, 1 | 9) && header.flags & 4 != 0;
        parser.blocks_complete = parser.blocks_complete.saturating_add(u64::from(block));
        parser.data_complete = parser
            .data_complete
            .saturating_add(u64::from(header.kind == 0));
        parser.last_complete = Some(FramePoint { mark, header });
        parser.header_bytes = 0;
        parser.remaining = None;
        parser.numeric = 0;
        if header.kind == 4 && !header.invalid_length() && !header.invalid_stream() {
            for (pending, completed) in parser
                .settings_pending
                .iter_mut()
                .zip(&mut parser.settings_complete)
            {
                if let Some(value) = pending.take() {
                    *completed = Some(Setting { value, mark });
                }
            }
        }
        let value = if header.invalid_length() || header.invalid_stream() {
            None
        } else {
            match header.kind {
                3 => Some(WireValue::Reset(numeric as u32)),
                7 => Some(WireValue::GoAway {
                    last_stream: (numeric >> 32) as u32 & 0x7fff_ffff,
                    reason: numeric as u32,
                }),
                8 => Some(WireValue::Window(numeric as u32 & 0x7fff_ffff)),
                _ => None,
            }
        };
        if let Some(stream) = self.stream(header.stream, false) {
            let progress = &mut stream.directions[index];
            progress.headers_complete = progress
                .headers_complete
                .saturating_add(u64::from(header.kind == 1));
            progress.blocks_complete = progress.blocks_complete.saturating_add(u64::from(block));
            if header.kind == 1 {
                let _ = progress.first_complete.get_or_insert(mark);
            }
            if block {
                let _ = progress.first_block.get_or_insert(mark);
            }
            if header.kind == 0 {
                progress.data.complete = progress.data.complete.saturating_add(1);
                progress.data.last = Some(mark);
                if late {
                    stream.late.complete = stream.late.complete.saturating_add(1);
                    stream.late.last = Some(mark);
                }
            }
            if direction == Direction::Tx
                && let Some(WireValue::Reset(reason)) = value
            {
                stream.reset = Some(mark);
                stream.reset_reason = Some(reason);
                if reason == 8 {
                    let _ = stream.first_cancel.get_or_insert(mark);
                }
            }
        }
        if header.kind != 0 {
            self.event(WireEvent {
                direction,
                point: FramePoint { mark, header },
                complete: true,
                value,
            });
        }
    }

    fn feed(&mut self, direction: Direction, mut bytes: &[u8]) {
        let index = direction.index();
        self.directions[index].bytes = self.directions[index]
            .bytes
            .saturating_add(bytes.len() as u64);
        while !bytes.is_empty() {
            let parser = &mut self.directions[index];
            if parser.preface < H2_PREFACE.len() {
                parser.preface_invalid |= bytes[0] != H2_PREFACE[parser.preface];
                parser.preface += 1;
                bytes = &bytes[1..];
                continue;
            }
            if parser.header_bytes < 9 {
                if parser.header_bytes == 0 {
                    parser.header = FrameHeader::default();
                }
                let byte = u32::from(bytes[0]);
                match parser.header_bytes {
                    0..=2 => parser.header.length = (parser.header.length << 8) | byte,
                    3 => parser.header.kind = byte as u8,
                    4 => parser.header.flags = byte as u8,
                    5 => parser.header.stream = byte & 0x7f,
                    _ => parser.header.stream = (parser.header.stream << 8) | byte,
                }
                parser.header_bytes += 1;
                bytes = &bytes[1..];
                if parser.header_bytes == 9 {
                    self.header_seen(direction);
                    if self.directions[index].remaining == Some(0) {
                        self.frame_complete(direction);
                    }
                }
                continue;
            }
            let remaining = parser.remaining.unwrap_or(0);
            let count = bytes.len().min(remaining as usize);
            let offset = parser.header.length - remaining;
            let header = parser.header;
            let late = parser.late;
            // Only fixed-size numerical control fields enter scratch state.
            // HPACK, DATA, PING, unknown settings and GOAWAY debug are skipped.
            let numeric_length = match header.kind {
                3 | 8 if !header.invalid_length() => 4,
                7 if !header.invalid_length() => 8,
                _ => 0,
            };
            if numeric_length > offset {
                let take = count.min((numeric_length - offset) as usize);
                for byte in &bytes[..take] {
                    parser.numeric = (parser.numeric << 8) | u64::from(*byte);
                }
            }
            if header.kind == 4 && !header.invalid_length() && !header.invalid_stream() {
                for (position, byte) in bytes[..count].iter().enumerate() {
                    let parser = &mut self.directions[index];
                    // Discard unselected setting values, not merely their event.
                    let field = (offset as usize + position) % 6;
                    match field {
                        0 => parser.setting_id = u16::from(*byte) << 8,
                        1 => parser.setting_id |= u16::from(*byte),
                        _ if matches!(parser.setting_id, 1..=6) => {
                            parser.numeric = (parser.numeric << 8) | u64::from(*byte);
                        }
                        _ => {}
                    }
                    if field == 5 {
                        let id = parser.setting_id;
                        let value = parser.numeric as u32;
                        parser.numeric = 0;
                        parser.setting_id = 0;
                        if matches!(id, 1..=6) {
                            parser.settings_pending[usize::from(id - 1)] = Some(value);
                            let mark = self.mark();
                            self.event(WireEvent {
                                direction,
                                point: FramePoint { mark, header },
                                complete: false,
                                value: Some(WireValue::Setting { id, value }),
                            });
                        } else {
                            let parser = &mut self.directions[index];
                            parser.settings_omitted = parser.settings_omitted.saturating_add(1);
                        }
                    }
                }
            }
            self.directions[index].remaining = Some(remaining - count as u32);
            if header.kind == 0 {
                let mark = self.mark();
                if let Some(stream) = self.stream(header.stream, false) {
                    let data = &mut stream.directions[index].data;
                    data.bytes = data.bytes.saturating_add(count as u64);
                    data.last = Some(mark);
                    if late {
                        stream.late.bytes = stream.late.bytes.saturating_add(count as u64);
                        stream.late.last = Some(mark);
                    }
                }
            }
            bytes = &bytes[count..];
            if self.directions[index].remaining == Some(0) {
                self.frame_complete(direction);
            }
        }
    }
}

pub(crate) struct WireObservation {
    instance: [u8; 36],
    pub(crate) owner: usize,
    pub(crate) generation: u64,
    pub(crate) socket: Option<SocketAddr>,
    state: Mutex<WireState>,
}

impl WireObservation {
    pub(crate) fn snapshot(&self) -> WireState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn feed(&self, direction: Direction, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .feed(direction, bytes);
    }
}

struct WireCapture {
    instance: [u8; 36],
    owner: usize,
    generation: u64,
    socket: Option<SocketAddr>,
    state: WireState,
}

impl WireCapture {
    fn write_core(&self, text: &mut impl Write, now: Instant) {
        let state = &self.state;
        let instance = std::str::from_utf8(&self.instance).unwrap_or("invalid-instance");
        let _ = writeln!(
            text,
            "wire instance={instance} owner={} gen={} socket={:?} seq={} \
             stream_header_omissions={} control_overwrites={}",
            self.owner,
            self.generation,
            self.socket,
            state.sequence,
            state.streams_omitted,
            state.events_overwritten,
        );
        for direction in [Direction::Tx, Direction::Rx] {
            let parser = &state.directions[direction.index()];
            let _ = writeln!(
                text,
                "wire {direction:?} bytes={} seen={} complete={} headers={} blocks={} data={} \
                 preface={}/24 bad_preface={} eof={} eof_partial={} header_bytes={} \
                 remaining={:?} partial(type,flags,length,stream)=({:?},{:?},{:?},{:?}) \
                 invalid_length={} invalid_stream={} settings_omitted={} errors={}",
                parser.bytes,
                parser.headers_seen,
                parser.frames_complete,
                parser.headers_complete,
                parser.blocks_complete,
                parser.data_complete,
                parser.preface,
                parser.preface_invalid,
                parser.eof,
                parser.eof_partial(),
                parser.header_bytes,
                parser.remaining,
                (parser.header_bytes >= 4).then_some(parser.header.kind),
                (parser.header_bytes >= 5).then_some(parser.header.flags),
                (parser.header_bytes >= 3).then_some(parser.header.length),
                (parser.header_bytes >= 9).then_some(parser.header.stream),
                parser.invalid_lengths,
                parser.invalid_streams,
                parser.settings_omitted,
                parser.errors,
            );
            for (stage, point) in [
                ("last_header", parser.last_header),
                ("last_complete", parser.last_complete),
            ] {
                if let Some(point) = point {
                    let _ = write!(text, "wire {direction:?} {stage} ");
                    write_point(text, point, now);
                    let _ = writeln!(text);
                }
            }
            if let Some(mark) = parser.eof_mark {
                let _ = writeln!(
                    text,
                    "wire {direction:?} eof_seq={} eof_age_us={}",
                    mark.sequence,
                    now.saturating_duration_since(mark.at).as_micros(),
                );
            }
        }
        for direction in [Direction::Tx, Direction::Rx] {
            for (index, setting) in state.directions[direction.index()]
                .settings_complete
                .iter()
                .enumerate()
            {
                if let Some(setting) = setting {
                    let _ = write!(
                        text,
                        "wire settings_complete {direction:?} id={} value={} mark=",
                        index + 1,
                        setting.value,
                    );
                    write_mark(text, Some(setting.mark), now);
                    let _ = writeln!(text);
                }
            }
        }
        let _ = writeln!(
            text,
            "wire first marks_hex=seq:age_us s_hex=numeric-stream-id \
             Tx/Rx=first-HEADERS-seen/complete/END_HEADERS cancel=first-complete-Tx-CANCEL \
             rst=latest-complete-Tx-RST:reason_hex",
        );
        for stream in state.streams.iter().flatten() {
            let _ = write!(text, "wire first s_hex={:x}", stream.id);
            for direction in [Direction::Tx, Direction::Rx] {
                let progress = stream.directions[direction.index()];
                let _ = write!(text, " {direction:?}=");
                write_mark(text, progress.first_header, now);
                let _ = write!(text, "/");
                write_mark(text, progress.first_complete, now);
                let _ = write!(text, "/");
                write_mark(text, progress.first_block, now);
            }
            let _ = write!(text, " cancel=");
            write_mark(text, stream.first_cancel, now);
            let _ = write!(text, " rst=");
            write_mark(text, stream.reset, now);
            let _ = write!(text, ":");
            if let Some(reason) = stream.reset_reason {
                let _ = write!(text, "{reason:x}");
            } else {
                let _ = write!(text, "-");
            }
            let _ = writeln!(text);
        }
    }

    fn write_streams(&self, text: &mut impl Write, now: Instant) {
        let state = &self.state;
        let _ = writeln!(
            text,
            "wire streams_hex s=numeric-stream-id h=seen/complete/blocks \
             d(DATA)=seen/complete/payload_bytes,last_length:flags,first,last \
             marks=seq:age_us late=Rx-DATA-header-after-complete-Tx-RST",
        );
        for stream in state.streams.iter().flatten() {
            let _ = write!(text, "wire s={:x}", stream.id);
            for direction in [Direction::Tx, Direction::Rx] {
                let progress = stream.directions[direction.index()];
                let _ = write!(
                    text,
                    " {direction:?} h={:x}/{:x}/{:x} d=",
                    progress.headers_seen, progress.headers_complete, progress.blocks_complete,
                );
                write_data(text, progress.data, now);
            }
            let _ = write!(text, " rst=");
            write_mark(text, stream.reset, now);
            let _ = write!(text, " reason=");
            if let Some(reason) = stream.reset_reason {
                let _ = write!(text, "{reason:x}");
            } else {
                let _ = write!(text, "-");
            }
            let _ = write!(text, " late=");
            write_data(text, stream.late, now);
            let _ = writeln!(text);
        }
    }

    fn write_events(&self, text: &mut impl Write, now: Instant, unowned: bool) {
        let state = &self.state;
        let _ = writeln!(
            text,
            "wire controls_hex owner={:x} gen={:x} unowned={unowned}",
            self.owner, self.generation,
        );
        for offset in 0..WIRE_EVENTS {
            let index = (state.events_next + offset) % WIRE_EVENTS;
            if let Some(event) = state.events[index] {
                let retained = state
                    .streams
                    .iter()
                    .flatten()
                    .any(|s| s.id == event.point.header.stream);
                if unowned == retained {
                    continue;
                }
                let header = event.point.header;
                let _ = write!(
                    text,
                    "wire control_hex {:?} c={} mark=",
                    event.direction,
                    u8::from(event.complete),
                );
                write_mark(text, Some(event.point.mark), now);
                let _ = write!(
                    text,
                    " t={:x} f={:x} l={:x} s_hex={:x}",
                    header.kind, header.flags, header.length, header.stream,
                );
                match event.value {
                    Some(WireValue::Reset(reason)) => {
                        let _ = write!(text, " rst_reason={reason:x}");
                    }
                    Some(WireValue::GoAway {
                        last_stream,
                        reason,
                    }) => {
                        let _ = write!(text, " goaway_last={last_stream:x} reason={reason:x}");
                    }
                    Some(WireValue::Setting { id, value }) => {
                        let _ = write!(text, " setting={id:x}:{value:x}");
                    }
                    Some(WireValue::Window(increment)) => {
                        let _ = write!(text, " window_increment={increment:x}");
                    }
                    None => {}
                }
                let _ = writeln!(text);
            }
        }
    }
}

fn write_point(text: &mut impl Write, point: FramePoint, now: Instant) {
    let header = point.header;
    let _ = write!(
        text,
        "seq={} age_us={} type={} flags={} length={} stream={} invalid_length={} invalid_stream={}",
        point.mark.sequence,
        now.saturating_duration_since(point.mark.at).as_micros(),
        header.kind,
        header.flags,
        header.length,
        header.stream,
        header.invalid_length(),
        header.invalid_stream(),
    );
}

fn write_mark(text: &mut impl Write, mark: Option<WireMark>, now: Instant) {
    if let Some(mark) = mark {
        // The age cap affects rendering only, not retained Instants.
        let age = now
            .saturating_duration_since(mark.at)
            .as_micros()
            .min(u128::from(u64::MAX));
        let _ = write!(text, "{:x}:{age:x}", mark.sequence);
    } else {
        let _ = write!(text, "-");
    }
}

fn write_data(text: &mut impl Write, data: DataProgress, now: Instant) {
    let _ = write!(
        text,
        "{:x}/{:x}/{:x},",
        data.seen, data.complete, data.bytes,
    );
    if data.seen > 0 {
        let _ = write!(text, "{:x}:{:x}", data.length, data.flags);
    } else {
        let _ = write!(text, "-");
    }
    let _ = write!(text, ",");
    write_mark(text, data.first, now);
    let _ = write!(text, ",");
    write_mark(text, data.last, now);
}

// health.rs is compiled only under cfg(test). The wrapper owns no buffering
// and never substitutes a context, waker, I/O call, result or vectored policy.
pub(crate) struct WireIo<I> {
    inner: I,
    observation: Arc<WireObservation>,
}

impl<I> WireIo<I> {
    pub(crate) fn new(inner: I, observation: Arc<WireObservation>) -> Self {
        Self { inner, observation }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for WireIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let capacity = buf.remaining();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        let bytes = &buf.filled()[before..];
        if !bytes.is_empty() {
            this.observation.feed(Direction::Rx, bytes);
        }
        if matches!(&result, Poll::Ready(Ok(()))) && bytes.is_empty() && capacity > 0 {
            let mut state = this
                .observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mark = state.mark();
            state.directions[1].eof = true;
            let _ = state.directions[1].eof_mark.get_or_insert(mark);
        }
        if matches!(&result, Poll::Ready(Err(_))) {
            let mut state = this
                .observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let parser = &mut state.directions[1];
            parser.errors = parser.errors.saturating_add(1);
        }
        result
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for WireIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(count)) = &result {
            this.observation.feed(Direction::Tx, &buf[..*count]);
        }
        result
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(count)) = &result {
            let mut state = this
                .observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut remaining = *count;
            for buf in bufs {
                let count = remaining.min(buf.len());
                if count > 0 {
                    state.feed(Direction::Tx, &buf[..count]);
                }
                remaining -= count;
                if remaining == 0 {
                    break;
                }
            }
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[derive(Clone, Default)]
pub(crate) struct PollState {
    pub(crate) polls: u64,
    pub(crate) inner_polls: u64,
    pub(crate) pending: u64,
    pub(crate) ready: u64,
    pub(crate) wakes: u64,
    pub(crate) dropped: bool,
    first: Option<Instant>,
    last: Option<Instant>,
    last_wake: Option<Instant>,
    max_sync: Option<Duration>,
    last_task: Option<tokio::task::Id>,
    drop_task: Option<tokio::task::Id>,
}

#[derive(Default)]
pub(crate) struct PollObservation {
    state: Mutex<PollState>,
    changed: tokio::sync::Notify,
}

impl PollObservation {
    pub(crate) fn snapshot(&self) -> PollState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) async fn pending(&self) {
        while self.snapshot().pending == 0 {
            self.changed.notified().await;
        }
    }

    pub(crate) async fn ready(&self) {
        while self.snapshot().ready == 0 {
            self.changed.notified().await;
        }
    }

    fn begin(&self) -> Instant {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.polls = state.polls.saturating_add(1);
        let _ = state.first.get_or_insert(now);
        state.last = Some(now);
        state.last_task = tokio::task::try_id();
        now
    }

    fn finish(&self, start: Instant, ready: bool, inner: bool) {
        let duration = start.elapsed();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.inner_polls = state.inner_polls.saturating_add(u64::from(inner));
        if ready {
            state.ready = state.ready.saturating_add(1);
        } else {
            state.pending = state.pending.saturating_add(1);
        }
        let previous = state.max_sync.unwrap_or(duration);
        state.max_sync = Some(previous.max(duration));
        drop(state);
        self.changed.notify_one();
    }

    fn dropped(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.dropped = true;
        state.drop_task = tokio::task::try_id();
    }
}

impl PollState {
    fn write(&self, text: &mut impl Write, now: Instant) {
        let state = self;
        let age = |at: Option<Instant>| at.map(|at| now.saturating_duration_since(at).as_micros());
        let _ = write!(
            text,
            "polls={} inner={} pending={} ready={} drop={} wakes={} \
             first_age_us={:?} last_age_us={:?} wake_age_us={:?} max_sync_us={:?}",
            state.polls,
            state.inner_polls,
            state.pending,
            state.ready,
            state.dropped,
            state.wakes,
            age(state.first),
            age(state.last),
            age(state.last_wake),
            state.max_sync.map(|duration| duration.as_micros()),
        );
    }
}

struct ForwardWake {
    observation: Arc<PollObservation>,
    target: Mutex<Option<Waker>>,
}

impl Wake for ForwardWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        {
            let mut state = self
                .observation
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.wakes = state.wakes.saturating_add(1);
            state.last_wake = Some(Instant::now());
        }
        let target = self
            .target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Never forward a wake while holding an observation or target lock.
        if let Some(target) = target {
            target.wake();
        }
    }
}

pub(crate) struct Observed<F> {
    inner: Pin<Box<F>>,
    wake: Option<Arc<ForwardWake>>,
    gate: Option<Arc<Gate>>,
}

impl<F> Observed<F> {
    pub(crate) fn new(future: F, observation: Option<Arc<PollObservation>>) -> Self {
        Self {
            inner: Box::pin(future),
            wake: observation.map(|observation| {
                Arc::new(ForwardWake {
                    observation,
                    target: Mutex::new(None),
                })
            }),
            gate: None,
        }
    }

    pub(crate) fn gated(mut self, gate: Option<Arc<Gate>>) -> Self {
        self.gate = gate;
        self
    }
}

impl<F: Future> Future for Observed<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(wake) = &this.wake else {
            return this.inner.as_mut().poll(cx);
        };
        *wake.target.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        let waker = Waker::from(Arc::clone(wake));
        let mut context = Context::from_waker(&waker);
        let start = wake.observation.begin();
        if let Some(gate) = &this.gate
            && gate.poll(&mut context).is_pending()
        {
            wake.observation.finish(start, false, false);
            return Poll::Pending;
        }
        let result = this.inner.as_mut().poll(&mut context);
        wake.observation.finish(start, result.is_ready(), true);
        result
    }
}

impl<F> Drop for Observed<F> {
    fn drop(&mut self) {
        if let Some(wake) = &self.wake {
            wake.observation.dropped();
            let _ = wake.target.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

// Only controlled tests install a gate. Registration and release are local to
// one health instance; a blocked wrapper is explicitly not an inner wire poll.
#[derive(Default)]
pub(crate) struct Gate {
    released: AtomicBool,
    reached: AtomicBool,
    changed: tokio::sync::Notify,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        self.reached.store(true, Ordering::SeqCst);
        self.changed.notify_one();
        if self.released.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub(crate) async fn reached(&self) {
        while !self.reached.load(Ordering::SeqCst) {
            self.changed.notified().await;
        }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        let waker = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct Task {
    kind: &'static str,
    owner: usize,
    generation: u64,
    socket: Option<SocketAddr>,
    ordinal: usize,
    observation: Arc<PollObservation>,
}

#[derive(Clone, Copy, Default)]
struct ServerIdentity {
    version: Option<http::Version>,
    tls: Option<bool>,
    verified_client: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BodyResult {
    #[default]
    Unpolled,
    GatePending,
    Pending,
    Data,
    Trailers,
    Other,
    End,
    Error,
}

#[derive(Clone, Default)]
struct CustodyState {
    last_result: BodyResult,
    issued_buffers: u64,
    issued_bytes: u64,
    retained_buffers: u64,
    retained_bytes: u64,
    last_release: Option<Instant>,
    release_task: Option<tokio::task::Id>,
}

pub(crate) struct RequestObservation {
    pub(crate) socket: Option<SocketAddr>,
    pub(crate) ordinal: usize,
    entered: Instant,
    pub(crate) response: Mutex<Option<Instant>>,
    pub(crate) handler: Arc<PollObservation>,
    pub(crate) body: Arc<PollObservation>,
    pub(crate) frames: AtomicU64,
    identity: ServerIdentity,
    custody: Mutex<CustodyState>,
}

struct Slots {
    tasks: [Option<Task>; TASK_SLOTS],
    requests: [Option<Arc<RequestObservation>>; REQUEST_SLOTS],
    tasks_omitted: u64,
    requests_omitted: u64,
}

struct WireSlots {
    connections: [Option<Arc<WireObservation>>; WIRE_CONNECTIONS],
    omitted: u64,
}

struct TaskCapture {
    kind: &'static str,
    owner: usize,
    generation: u64,
    socket: Option<SocketAddr>,
    ordinal: usize,
    state: PollState,
}

struct RequestCapture {
    socket: Option<SocketAddr>,
    ordinal: usize,
    entered: Instant,
    response: Option<Instant>,
    handler: PollState,
    body: PollState,
    frames: u64,
    identity: ServerIdentity,
    custody: CustodyState,
}

pub(crate) struct ObserverCapture {
    tasks: [Option<TaskCapture>; TASK_SLOTS],
    requests: [Option<RequestCapture>; REQUEST_SLOTS],
    connections: [Option<WireCapture>; WIRE_CONNECTIONS],
    tasks_omitted: u64,
    requests_omitted: u64,
    connections_omitted: u64,
}

pub(crate) struct Observer {
    slots: Mutex<Slots>,
    wire: Mutex<WireSlots>,
    pub(crate) wire_gate: Mutex<Option<Arc<Gate>>>,
}

impl Default for Observer {
    fn default() -> Self {
        Self {
            slots: Mutex::new(Slots {
                tasks: std::array::from_fn(|_| None),
                requests: std::array::from_fn(|_| None),
                tasks_omitted: 0,
                requests_omitted: 0,
            }),
            wire_gate: Mutex::new(None),
            wire: Mutex::new(WireSlots {
                connections: std::array::from_fn(|_| None),
                omitted: 0,
            }),
        }
    }
}

impl Observer {
    pub(crate) fn wire(
        &self,
        instance: &str,
        owner: usize,
        generation: u64,
        socket: Option<SocketAddr>,
    ) -> Option<Arc<WireObservation>> {
        let mut slots = self.wire.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.connections.iter_mut().find(|slot| slot.is_none()) else {
            slots.omitted = slots.omitted.saturating_add(1);
            return None;
        };
        let mut identity = [0; 36];
        let count = instance.len().min(identity.len());
        identity[..count].copy_from_slice(&instance.as_bytes()[..count]);
        let observation = Arc::new(WireObservation {
            instance: identity,
            owner,
            generation,
            socket,
            state: Mutex::new(WireState::default()),
        });
        *slot = Some(Arc::clone(&observation));
        Some(observation)
    }

    pub(crate) fn wires(&self) -> Vec<Arc<WireObservation>> {
        let slots = self.wire.lock().unwrap_or_else(|e| e.into_inner());
        slots.connections.iter().flatten().map(Arc::clone).collect()
    }

    pub(crate) fn capture(&self) -> ObserverCapture {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let tasks = std::array::from_fn(|index| {
            slots.tasks[index].as_ref().map(|task| TaskCapture {
                kind: task.kind,
                owner: task.owner,
                generation: task.generation,
                socket: task.socket,
                ordinal: task.ordinal,
                state: task.observation.snapshot(),
            })
        });
        let requests = std::array::from_fn(|index| {
            slots.requests[index]
                .as_ref()
                .map(|request| RequestCapture {
                    socket: request.socket,
                    ordinal: request.ordinal,
                    entered: request.entered,
                    response: *request.response.lock().unwrap_or_else(|e| e.into_inner()),
                    handler: request.handler.snapshot(),
                    body: request.body.snapshot(),
                    frames: request.frames.load(Ordering::Relaxed),
                    identity: request.identity,
                    custody: request
                        .custody
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone(),
                })
        });
        let tasks_omitted = slots.tasks_omitted;
        let requests_omitted = slots.requests_omitted;
        drop(slots);
        let wire = self.wire.lock().unwrap_or_else(|e| e.into_inner());
        ObserverCapture {
            tasks,
            requests,
            connections: std::array::from_fn(|index| {
                wire.connections[index]
                    .as_ref()
                    .map(|observation| WireCapture {
                        instance: observation.instance,
                        owner: observation.owner,
                        generation: observation.generation,
                        socket: observation.socket,
                        state: observation.snapshot(),
                    })
            }),
            tasks_omitted,
            requests_omitted,
            connections_omitted: wire.omitted,
        }
    }

    pub(crate) fn write_wire(&self, text: &mut impl Write, now: Instant) {
        self.capture().write_wire(text, now);
    }

    pub(crate) fn tasks(&self, kind: &'static str) -> Vec<Arc<PollObservation>> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .tasks
            .iter()
            .flatten()
            .filter(|task| task.kind == kind)
            .map(|task| Arc::clone(&task.observation))
            .collect()
    }

    pub(crate) fn requests(&self) -> Vec<Arc<RequestObservation>> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.requests.iter().flatten().map(Arc::clone).collect()
    }

    pub(crate) fn task(
        &self,
        kind: &'static str,
        owner: usize,
        generation: u64,
        socket: Option<SocketAddr>,
        ordinal: usize,
    ) -> Option<Arc<PollObservation>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.tasks.iter_mut().find(|slot| slot.is_none()) else {
            slots.tasks_omitted = slots.tasks_omitted.saturating_add(1);
            return None;
        };
        let observation = Arc::new(PollObservation::default());
        *slot = Some(Task {
            kind,
            owner,
            generation,
            socket,
            ordinal,
            observation: Arc::clone(&observation),
        });
        Some(observation)
    }

    pub(crate) fn request(&self, socket: Option<SocketAddr>) -> Option<Arc<RequestObservation>> {
        self.request_identity(socket, ServerIdentity::default())
    }

    pub(crate) fn server_request<B>(
        &self,
        request: &http::Request<B>,
    ) -> Option<Arc<RequestObservation>> {
        let peer = request
            .extensions()
            .get::<ferrum_alloy::telemetry::peer::PeerInfo>();
        let socket = peer.and_then(|peer| peer.remote_addr).or_else(|| {
            request
                .extensions()
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .map(|info| info.0)
        });
        // This optional client-certificate identity does not distinguish
        // plaintext from TLS without a client certificate.
        let tls_identity = peer.and_then(|peer| peer.tls.as_ref());
        let identity = ServerIdentity {
            version: Some(request.version()),
            tls: tls_identity.map(|_| true),
            verified_client: tls_identity.map(|tls| tls.client_cert_verified),
        };
        self.request_identity(socket, identity)
    }

    fn request_identity(
        &self,
        socket: Option<SocketAddr>,
        identity: ServerIdentity,
    ) -> Option<Arc<RequestObservation>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let ordinal = slots
            .requests
            .iter()
            .flatten()
            .filter(|request| request.socket == socket)
            .count()
            + 1;
        let Some(slot) = slots.requests.iter_mut().find(|slot| slot.is_none()) else {
            slots.requests_omitted = slots.requests_omitted.saturating_add(1);
            return None;
        };
        let request = Arc::new(RequestObservation {
            socket,
            ordinal,
            entered: Instant::now(),
            response: Mutex::new(None),
            handler: Arc::new(PollObservation::default()),
            body: Arc::new(PollObservation::default()),
            frames: AtomicU64::new(0),
            identity,
            custody: Mutex::new(CustodyState::default()),
        });
        *slot = Some(Arc::clone(&request));
        Some(request)
    }
}

impl ObserverCapture {
    pub(crate) fn write_server_progress(&self, text: &mut impl Write, now: Instant) {
        let active = |row: &&RequestCapture| {
            row.response.is_none() || !row.body.dropped || row.custody.retained_buffers > 0
        };
        let count = self.requests.iter().flatten().filter(active).count();
        let _ = writeln!(
            text,
            "server progress rows_max={SERVER_PROGRESS_ROWS} omitted={} newest_active_first \
             boundary=router-body native_stream_id=unobserved transport_progress=unobserved \
             ordinal_is_not_stream_id task_id_may_be_reused \
             buffers/bytes=issued/retained-original-buffer-size \
             buffer_release_is_not_transport_write",
            count.saturating_sub(SERVER_PROGRESS_ROWS),
        );
        for request in self
            .requests
            .iter()
            .flatten()
            .rev()
            .filter(active)
            .take(SERVER_PROGRESS_ROWS)
        {
            let connection = request.socket.and_then(|socket| {
                self.connections
                    .iter()
                    .flatten()
                    .find(|connection| connection.socket == Some(socket))
                    .map(|connection| (connection.owner, connection.generation))
            });
            let _ = writeln!(
                text,
                "server progress socket={:?} client_owner_generation={connection:?} \
                 ordinal={} version={:?} tls={:?} \
                 verified_client={:?} router_task={:?} body_task={:?} body_drop_task={:?} \
                 last={:?} last_poll_age_us={:?} buffers={}/{} bytes={}/{} \
                 release_age_us={:?} release_task={:?} body_drop={}",
                request.socket,
                request.ordinal,
                request.identity.version,
                request.identity.tls,
                request.identity.verified_client,
                request.handler.last_task,
                request.body.last_task,
                request.body.drop_task,
                request.custody.last_result,
                request.body.last.map(|at| age_us(now, at)),
                request.custody.issued_buffers,
                request.custody.retained_buffers,
                request.custody.issued_bytes,
                request.custody.retained_bytes,
                request.custody.last_release.map(|at| age_us(now, at)),
                request.custody.release_task,
                request.body.dropped,
            );
        }
    }

    pub(crate) fn write_wire(&self, text: &mut impl Write, now: Instant) {
        self.write_wire_core(text, now);
        self.write_wire_detail(text, now);
    }

    pub(crate) fn write_wire_core(&self, text: &mut impl Write, now: Instant) {
        let _ = writeln!(
            text,
            "wire slots(connection,stream,control)=({WIRE_CONNECTIONS},{WIRE_STREAMS},{WIRE_EVENTS}) \
             connections_omitted={} boundary=client-plaintext-I/O \
             receipt_is_not_decode socket_ordinal_is_not_stream_id",
            self.connections_omitted,
        );
        for observation in self.connections.iter().flatten() {
            observation.write_core(text, now);
        }
        // Preserve controls without a retained stream owner, including stream
        // zero and omitted stream IDs, before potentially saturated DATA detail.
        for observation in self.connections.iter().flatten() {
            observation.write_events(text, now, true);
        }
    }

    pub(crate) fn write_wire_detail(&self, text: &mut impl Write, now: Instant) {
        for observation in self.connections.iter().flatten() {
            observation.write_streams(text, now);
        }
        for observation in self.connections.iter().flatten() {
            observation.write_events(text, now, false);
        }
    }

    pub(crate) fn write_required(&self, text: &mut impl Write, now: Instant) {
        let _ = writeln!(
            text,
            "observer slots(task,request)=({TASK_SLOTS},{REQUEST_SLOTS}) omitted=({},{}) \
             child_ready=unit-completion-inner-wire-result-unknown socket_ordinal_is_not_stream_id",
            self.tasks_omitted, self.requests_omitted,
        );
        let _ = writeln!(
            text,
            "server compact_hex entry/response=age_us r/b=router/body-had-Pending/Ready/dropped \
             '-'=unobserved socket_ref=first-request-slot ordinal_is_not_stream_id",
        );
        for (index, request) in self.requests.iter().enumerate() {
            let Some(request) = request else { continue };
            let reference = self
                .requests
                .iter()
                .position(|slot| {
                    slot.as_ref()
                        .is_some_and(|other| other.socket == request.socket)
                })
                .unwrap_or(index);
            if reference == index {
                let _ = writeln!(
                    text,
                    "server identity socket_ref={reference:x} socket={:?}",
                    request.socket,
                );
            }
            let _ = write!(
                text,
                "server socket_ref={reference:x} ordinal={:x} entry={:x} response=",
                request.ordinal,
                age_us(now, request.entered),
            );
            if let Some(at) = request.response {
                let _ = write!(text, "{:x}", age_us(now, at));
            } else {
                let _ = write!(text, "-");
            }
            let _ = writeln!(
                text,
                " frames={:x} r={}/{}/{} b={}/{}/{}",
                request.frames,
                u8::from(request.handler.pending > 0),
                u8::from(request.handler.ready > 0),
                u8::from(request.handler.dropped),
                u8::from(request.body.pending > 0),
                u8::from(request.body.ready > 0),
                u8::from(request.body.dropped),
            );
        }
        // Pending and never-polled tasks precede completed/destroyed history.
        for (id, task) in self.tasks.iter().enumerate() {
            if let Some(task) = task
                && task.state.ready == 0
                && !task.state.dropped
            {
                task.write_required(text, id, now);
            }
        }
    }

    pub(crate) fn write_history(&self, text: &mut impl Write, now: Instant) {
        for (id, task) in self.tasks.iter().enumerate() {
            if let Some(task) = task
                && task.state.ready == 0
                && !task.state.dropped
            {
                task.write(text, id, now);
            }
        }
        for (id, task) in self.tasks.iter().enumerate() {
            if let Some(task) = task
                && (task.state.ready > 0 || task.state.dropped)
            {
                task.write(text, id, now);
            }
        }
        for request in self.requests.iter().flatten() {
            let _ = write!(
                text,
                "server detail socket={:?} ordinal={} router_future(",
                request.socket, request.ordinal,
            );
            request.handler.write(text, now);
            let _ = write!(text, ") body(");
            request.body.write(text, now);
            let _ = writeln!(text, ")");
        }
    }
}

impl TaskCapture {
    fn write_required(&self, text: &mut impl Write, id: usize, now: Instant) {
        let _ = write!(
            text,
            "task={id} kind={} owner={} gen={} socket={:?} ordinal={} \
             polls_hex(polls,inner,pending,ready,wakes)={:x}/{:x}/{:x}/{:x}/{:x} \
             drop=false last_age_us_hex=",
            self.kind,
            self.owner,
            self.generation,
            self.socket,
            self.ordinal,
            self.state.polls,
            self.state.inner_polls,
            self.state.pending,
            self.state.ready,
            self.state.wakes,
        );
        if let Some(at) = self.state.last {
            let _ = write!(text, "{:x}", age_us(now, at));
        } else {
            let _ = write!(text, "-");
        }
        let _ = writeln!(text);
    }

    fn write(&self, text: &mut impl Write, id: usize, now: Instant) {
        let _ = write!(
            text,
            "task={id} kind={} owner={} gen={} socket={:?} ordinal={} ",
            self.kind, self.owner, self.generation, self.socket, self.ordinal,
        );
        self.state.write(text, now);
        let _ = writeln!(text);
    }
}

fn age_us(now: Instant, at: Instant) -> u64 {
    now.saturating_duration_since(at)
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) async fn response(
    future: impl Future<Output = axum::response::Response>,
    observation: Option<Arc<RequestObservation>>,
) -> axum::response::Response {
    response_with_gates(future, observation, None, None).await
}

pub(crate) async fn response_with_gates(
    future: impl Future<Output = axum::response::Response>,
    observation: Option<Arc<RequestObservation>>,
    headers: Option<Arc<Gate>>,
    data: Option<Arc<Gate>>,
) -> axum::response::Response {
    let Some(observation) = observation else {
        return future.await;
    };
    let response = Observed::new(future, Some(Arc::clone(&observation.handler)))
        .gated(headers)
        .await;
    *observation
        .response
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    response.map(|body| {
        axum::body::Body::new(ObservedBody {
            inner: Box::pin(body),
            observation,
            gate: data,
        })
    })
}

struct ObservedBody<B> {
    inner: Pin<Box<B>>,
    observation: Arc<RequestObservation>,
    gate: Option<Arc<Gate>>,
}

impl<B: hyper::body::Body<Data = bytes::Bytes>> hyper::body::Body for ObservedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let start = this.observation.body.begin();
        if let Some(gate) = &this.gate
            && gate.poll(cx).is_pending()
        {
            this.observation
                .custody
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_result = BodyResult::GatePending;
            this.observation.body.finish(start, false, false);
            return Poll::Pending;
        }
        let result = this.inner.as_mut().poll_frame(cx);
        let last_result = match &result {
            Poll::Pending => BodyResult::Pending,
            Poll::Ready(None) => BodyResult::End,
            Poll::Ready(Some(Err(_))) => BodyResult::Error,
            Poll::Ready(Some(Ok(frame))) if frame.is_data() => BodyResult::Data,
            Poll::Ready(Some(Ok(frame))) if frame.is_trailers() => BodyResult::Trailers,
            Poll::Ready(Some(Ok(_))) => BodyResult::Other,
        };
        this.observation
            .custody
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_result = last_result;
        this.observation.body.finish(start, result.is_ready(), true);
        if matches!(&result, Poll::Ready(Some(Ok(_)))) {
            this.observation.frames.fetch_add(1, Ordering::Relaxed);
        }
        result.map(|frame| {
            frame.map(|frame| {
                frame
                    .map(|frame| frame.map_data(|data| ObservedData::wrap(data, &this.observation)))
            })
        })
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for ObservedBody<B> {
    fn drop(&mut self) {
        self.observation.body.dropped();
    }
}

// Native Bytes ownership follows clones/slices through Alloy, Hyper and h2.
// We retain only counters in the observer, never a payload reference. The
// original buffer can be released on copy, reset or write; none proves delivery.
struct ObservedData {
    inner: bytes::Bytes,
    observation: Arc<RequestObservation>,
    bytes: u64,
}

impl ObservedData {
    fn wrap(inner: bytes::Bytes, observation: &Arc<RequestObservation>) -> bytes::Bytes {
        if inner.is_empty() {
            return inner;
        }
        let bytes = inner.len() as u64;
        {
            let mut state = observation
                .custody
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.issued_buffers = state.issued_buffers.saturating_add(1);
            state.issued_bytes = state.issued_bytes.saturating_add(bytes);
            state.retained_buffers = state.retained_buffers.saturating_add(1);
            state.retained_bytes = state.retained_bytes.saturating_add(bytes);
        }
        bytes::Bytes::from_owner(Self {
            inner,
            observation: Arc::clone(observation),
            bytes,
        })
    }
}

impl AsRef<[u8]> for ObservedData {
    fn as_ref(&self) -> &[u8] {
        &self.inner
    }
}

impl Drop for ObservedData {
    fn drop(&mut self) {
        let mut state = self
            .observation
            .custody
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.retained_buffers = state.retained_buffers.saturating_sub(1);
        state.retained_bytes = state.retained_bytes.saturating_sub(self.bytes);
        state.last_release = Some(Instant::now());
        state.release_task = tokio::task::try_id();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, reason = "observer proofs")]

    use std::collections::VecDeque;

    use super::*;

    const INSTANCE: &str = "00000000-0000-4000-8000-000000000001";

    fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
        let length = payload.len() as u32;
        let mut bytes = vec![
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
            kind,
            flags,
        ];
        bytes.extend_from_slice(&stream.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    enum ReadStep {
        Bytes(Vec<u8>),
        Pending,
        Error,
        Eof,
    }

    enum WriteStep {
        Accept(usize),
        Pending,
        Error,
    }

    #[derive(Default)]
    struct MockIo {
        reads: VecDeque<ReadStep>,
        writes: VecDeque<WriteStep>,
        read_calls: Vec<(usize, usize, usize)>,
        scalar_calls: Vec<(usize, Vec<u8>)>,
        vector_calls: Vec<Vec<(usize, Vec<u8>)>>,
        accepted: Vec<u8>,
        expected_waker: Option<Waker>,
        vectored: bool,
        flushes: usize,
        shutdowns: usize,
    }

    impl MockIo {
        fn context(&self, cx: &Context<'_>) {
            if let Some(waker) = &self.expected_waker {
                assert!(cx.waker().will_wake(waker));
            }
        }

        fn write_result(&mut self, cx: &Context<'_>) -> Poll<io::Result<usize>> {
            self.context(cx);
            match self.writes.pop_front().unwrap() {
                WriteStep::Accept(count) => Poll::Ready(Ok(count)),
                WriteStep::Pending => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                WriteStep::Error => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "private-write-error",
                ))),
            }
        }

        fn terminal(cx: &Context<'_>, calls: &mut usize) -> Poll<io::Result<()>> {
            *calls += 1;
            match *calls {
                1 => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                2 => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "private-terminal-error",
                ))),
                _ => Poll::Ready(Ok(())),
            }
        }
    }

    impl AsyncRead for MockIo {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.context(cx);
            this.read_calls.push((
                buf.filled().as_ptr() as usize,
                buf.filled().len(),
                buf.remaining(),
            ));
            match this.reads.pop_front().unwrap() {
                ReadStep::Bytes(bytes) => {
                    buf.put_slice(&bytes);
                    Poll::Ready(Ok(()))
                }
                ReadStep::Pending => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                ReadStep::Error => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "private-read-error",
                ))),
                ReadStep::Eof => Poll::Ready(Ok(())),
            }
        }
    }

    impl AsyncWrite for MockIo {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.scalar_calls
                .push((buf.as_ptr() as usize, buf.to_vec()));
            let result = this.write_result(cx);
            if let Poll::Ready(Ok(count)) = &result {
                this.accepted.extend_from_slice(&buf[..*count]);
            }
            result
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.vector_calls.push(
                bufs.iter()
                    .map(|buf| (buf.as_ptr() as usize, buf.to_vec()))
                    .collect(),
            );
            let result = this.write_result(cx);
            if let Poll::Ready(Ok(count)) = &result {
                let mut remaining = *count;
                for buf in bufs {
                    let accepted = remaining.min(buf.len());
                    this.accepted.extend_from_slice(&buf[..accepted]);
                    remaining -= accepted;
                }
                assert_eq!(remaining, 0);
            }
            result
        }

        fn is_write_vectored(&self) -> bool {
            self.vectored
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.context(cx);
            Self::terminal(cx, &mut this.flushes)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.context(cx);
            Self::terminal(cx, &mut this.shutdowns)
        }
    }

    #[derive(Default)]
    struct CountWake(AtomicU64);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn wire_io_forwards_exact_buffers_wakers_short_writes_and_vectored_policy() {
        let observer = Observer::default();
        let observation = observer.wire(INSTANCE, 2, 3, None).unwrap();
        let wake = Arc::new(CountWake::default());
        let waker = Waker::from(Arc::clone(&wake));
        let mut cx = Context::from_waker(&waker);
        let mut transcript = H2_PREFACE.to_vec();
        transcript.extend(frame(1, 5, 1, b"private-hpack"));
        transcript.extend(frame(3, 0, 1, &8_u32.to_be_bytes()));
        let inner = MockIo {
            reads: [ReadStep::Pending, ReadStep::Error, ReadStep::Eof].into(),
            writes: [
                WriteStep::Pending,
                WriteStep::Error,
                WriteStep::Accept(5),
                WriteStep::Pending,
                WriteStep::Error,
                WriteStep::Accept(11),
                WriteStep::Accept(transcript.len() - 16),
                WriteStep::Accept(0),
            ]
            .into(),
            expected_waker: Some(waker.clone()),
            vectored: true,
            ..MockIo::default()
        };
        let mut io = WireIo::new(inner, Arc::clone(&observation));
        assert!(io.is_write_vectored());
        assert!(
            Pin::new(&mut io)
                .poll_write(&mut cx, &transcript)
                .is_pending()
        );
        let error = match Pin::new(&mut io).poll_write(&mut cx, &transcript) {
            Poll::Ready(Err(error)) => error,
            _ => panic!("write error was changed"),
        };
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "private-write-error");
        assert_eq!(observation.snapshot().directions[0].bytes, 0);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &transcript),
            Poll::Ready(Ok(5))
        ));
        assert_eq!(observation.snapshot().directions[0].preface, 5);
        let bufs = [
            IoSlice::new(&[]),
            IoSlice::new(&transcript[5..8]),
            IoSlice::new(&transcript[8..15]),
            IoSlice::new(&transcript[15..]),
        ];
        assert!(
            Pin::new(&mut io)
                .poll_write_vectored(&mut cx, &bufs)
                .is_pending()
        );
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Err(_))
        ));
        assert_eq!(observation.snapshot().directions[0].bytes, 5);
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(11))
        ));
        assert_eq!(observation.snapshot().directions[0].preface, 16);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &transcript[16..]),
            Poll::Ready(Ok(count)) if count == transcript.len() - 16
        ));
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &transcript),
            Poll::Ready(Ok(0))
        ));
        assert_eq!(io.inner.accepted, transcript);
        assert_eq!(io.inner.scalar_calls.len(), 5);
        for (pointer, bytes) in &io.inner.scalar_calls[..3] {
            assert_eq!(*pointer, transcript.as_ptr() as usize);
            assert_eq!(*bytes, transcript);
        }
        for call in &io.inner.vector_calls {
            assert_eq!(call.len(), bufs.len());
            for ((pointer, bytes), buf) in call.iter().zip(&bufs) {
                assert_eq!(*pointer, buf.as_ptr() as usize);
                assert_eq!(bytes.as_slice(), &buf[..]);
            }
        }
        let mut storage = [0; 16];
        let mut buf = ReadBuf::new(&mut storage);
        buf.put_slice(b"old");
        let pointer = buf.filled().as_ptr() as usize;
        assert!(Pin::new(&mut io).poll_read(&mut cx, &mut buf).is_pending());
        let error = match Pin::new(&mut io).poll_read(&mut cx, &mut buf) {
            Poll::Ready(Err(error)) => error,
            _ => panic!("read error was changed"),
        };
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(error.to_string(), "private-read-error");
        assert_eq!(buf.filled(), b"old");
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut buf),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(io.inner.read_calls, [(pointer, 3, 13); 3]);
        assert_eq!(observation.snapshot().directions[1].bytes, 0);
        assert!(observation.snapshot().directions[1].eof);
        for shutdown in [false, true] {
            for step in 0..3 {
                let result = if shutdown {
                    Pin::new(&mut io).poll_shutdown(&mut cx)
                } else {
                    Pin::new(&mut io).poll_flush(&mut cx)
                };
                match step {
                    0 => assert!(result.is_pending()),
                    1 => assert!(matches!(result, Poll::Ready(Err(_)))),
                    _ => assert!(matches!(result, Poll::Ready(Ok(())))),
                }
            }
        }
        assert_eq!(wake.0.load(Ordering::SeqCst), 5);
        io.inner.vectored = false;
        assert!(!io.is_write_vectored());
        let state = observation.snapshot();
        assert_eq!(state.directions[0].bytes, transcript.len() as u64);
        assert_eq!(state.directions[0].headers_seen, 2);
        assert_eq!(state.directions[0].frames_complete, 2);
        assert!(state.streams[0].unwrap().reset.is_some());
    }

    #[test]
    fn short_vectored_prefixes_do_not_complete_a_partial_reset() {
        let observer = Observer::default();
        let observation = observer.wire(INSTANCE, 0, 1, None).unwrap();
        let reset = frame(3, 0, 17, &8_u32.to_be_bytes());
        let inner = MockIo {
            writes: [
                WriteStep::Accept(24),
                WriteStep::Accept(8),
                WriteStep::Accept(0),
                WriteStep::Accept(4),
                WriteStep::Accept(1),
            ]
            .into(),
            vectored: true,
            ..MockIo::default()
        };
        let mut io = WireIo::new(inner, Arc::clone(&observation));
        let waker = Waker::from(Arc::new(CountWake::default()));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, H2_PREFACE),
            Poll::Ready(Ok(24))
        ));
        let bufs = [IoSlice::new(&reset[..3]), IoSlice::new(&reset[3..])];
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(8))
        ));
        assert_eq!(observation.snapshot().directions[0].headers_seen, 0);
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(0))
        ));
        let bufs = [IoSlice::new(&reset[8..9]), IoSlice::new(&reset[9..])];
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(4))
        ));
        let state = observation.snapshot();
        assert_eq!(state.directions[0].headers_seen, 1);
        assert_eq!(state.directions[0].frames_complete, 0);
        assert_eq!(state.directions[0].remaining, Some(1));
        assert!(state.streams[0].unwrap().reset.is_none());
        let bufs = [IoSlice::new(&[]), IoSlice::new(&reset[12..])];
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(1))
        ));
        let state = observation.snapshot();
        assert_eq!(state.directions[0].frames_complete, 1);
        assert_eq!(state.streams[0].unwrap().reset_reason, Some(8));
        assert_eq!(state.directions[0].bytes, 37);
        let mut accepted = H2_PREFACE.to_vec();
        accepted.extend(reset);
        assert_eq!(io.inner.accepted, accepted);
        assert_eq!(io.inner.vector_calls.len(), 4);
    }

    #[test]
    fn controlled_fragmented_and_coalesced_transcript_retains_only_numbers() {
        // CONTROLLED input proves observation only. No claim about spontaneous
        // recurrence, internal reset expiration or a Tokio substitute for std Instant.
        let observer = Observer::default();
        let observation = observer.wire(INSTANCE, 0, 1, None).unwrap();
        let mut tx = H2_PREFACE.to_vec();
        tx.extend(frame(1, 5, 1, b"private-hpack"));
        tx.extend(frame(3, 0, 1, &8_u32.to_be_bytes()));
        let waker = Waker::from(Arc::new(CountWake::default()));
        let mut cx = Context::from_waker(&waker);
        let inner = MockIo {
            writes: (0..tx.len()).map(|_| WriteStep::Accept(1)).collect(),
            ..MockIo::default()
        };
        let mut io = WireIo::new(inner, Arc::clone(&observation));
        for offset in 0..tx.len() {
            assert!(matches!(
                Pin::new(&mut io).poll_write(&mut cx, &tx[offset..]),
                Poll::Ready(Ok(1))
            ));
        }
        let mut rx = frame(4, 0, 0, &[0, 4, 0, 0, 255, 255, 0, 5, 0, 0, 64, 0]);
        rx.extend(frame(4, 0, 0, &[255, 255, b'c', b'r', b'e', b'd']));
        rx.extend(frame(1, 0, 1, b"private-hpack"));
        rx.extend(frame(9, 4, 1, b"private-continuation"));
        rx.extend(frame(0, 0, 1, b"private-data"));
        rx.extend(frame(0, 0, 3, b"other-data"));
        rx.extend(frame(6, 0, 0, b"private!"));
        rx.extend(frame(8, 0, 0, &0x8000_002a_u32.to_be_bytes()));
        let mut goaway = Vec::from(0x8000_0001_u32.to_be_bytes());
        goaway.extend(11_u32.to_be_bytes());
        goaway.extend(b"private-goaway-debug");
        rx.extend(frame(7, 0, 0, &goaway));
        let split = 34; // Fragment numerical SETTINGS; coalesce the rest.
        io.inner.reads = rx[..split]
            .iter()
            .map(|byte| ReadStep::Bytes(vec![*byte]))
            .chain([ReadStep::Bytes(rx[split..].to_vec()), ReadStep::Eof])
            .collect();
        for _ in 0..split + 2 {
            let mut storage = [0; 512];
            let mut buf = ReadBuf::new(&mut storage);
            buf.put_slice(b"old-buffer-prefix");
            let before = buf.filled().len();
            assert!(matches!(
                Pin::new(&mut io).poll_read(&mut cx, &mut buf),
                Poll::Ready(Ok(()))
            ));
            assert_eq!(&buf.filled()[..before], b"old-buffer-prefix");
            let state = observation.snapshot();
            if (32..=34).contains(&state.directions[1].bytes) {
                assert_eq!(state.directions[1].numeric, 0);
            }
        }
        let state = observation.snapshot();
        assert_eq!(state.directions[1].bytes, rx.len() as u64);
        assert_eq!(state.directions[1].headers_seen, 9);
        assert_eq!(state.directions[1].frames_complete, 9);
        assert_eq!(state.directions[1].headers_complete, 1);
        assert_eq!(state.directions[1].blocks_complete, 1);
        assert!(!state.directions[1].eof_partial());
        assert_eq!(state.directions[1].settings_omitted, 1);
        let stream = state.streams[0].unwrap();
        assert_eq!(stream.id, 1);
        assert_eq!(stream.directions[1].data.bytes, 12);
        assert_eq!(stream.late.seen, 1);
        assert_eq!(stream.late.complete, 1);
        assert_eq!(stream.late.bytes, 12);
        assert_eq!(state.streams[1].unwrap().id, 3);
        assert_eq!(state.streams[1].unwrap().late.seen, 0);
        assert!(stream.late.first.unwrap().sequence > stream.reset.unwrap().sequence);
        assert!(stream.late.last.unwrap().at >= stream.late.first.unwrap().at);
        let values: Vec<_> = state
            .events
            .iter()
            .flatten()
            .filter_map(|event| event.value)
            .collect();
        assert!(
            values
                .iter()
                .any(|value| matches!(value, WireValue::Reset(8)))
        );
        assert!(
            values
                .iter()
                .any(|value| matches!(value, WireValue::Window(42)))
        );
        assert!(values.iter().any(|value| matches!(
            value,
            WireValue::Setting {
                id: 4,
                value: 65535
            }
        )));
        assert!(values.iter().any(|value| matches!(
            value,
            WireValue::Setting {
                id: 5,
                value: 16384
            }
        )));
        assert!(values.iter().any(|value| matches!(
            value,
            WireValue::GoAway {
                last_stream: 1,
                reason: 11
            }
        )));
        let mut text = String::new();
        observer.write_wire(&mut text, Instant::now());
        let retained = format!("{state:?}");
        for marker in [
            "private-hpack",
            "private-continuation",
            "private-data",
            "private!",
            "private-goaway-debug",
            "cred",
        ] {
            assert!(!text.contains(marker));
            assert!(!retained.contains(marker));
        }
        let marks: Vec<_> = state
            .events
            .iter()
            .flatten()
            .map(|event| event.point.mark)
            .collect();
        assert!(
            marks
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        assert!(marks.windows(2).all(|pair| pair[0].at <= pair[1].at));
    }

    #[test]
    fn numerical_controls_survive_every_fragment_boundary() {
        let mut goaway = Vec::from(17_u32.to_be_bytes());
        goaway.extend(11_u32.to_be_bytes());
        goaway.extend(b"private-debug");
        for (bytes, expected) in [
            (frame(3, 0, 17, &8_u32.to_be_bytes()), WireValue::Reset(8)),
            (
                frame(7, 0, 0, &goaway),
                WireValue::GoAway {
                    last_stream: 17,
                    reason: 11,
                },
            ),
            (
                frame(8, 0, 17, &0x8000_002a_u32.to_be_bytes()),
                WireValue::Window(42),
            ),
            (
                frame(4, 0, 0, &[0, 1, 0, 0, 16, 0]),
                WireValue::Setting { id: 1, value: 4096 },
            ),
        ] {
            for split in 0..=bytes.len() {
                let mut state = WireState::default();
                state.feed(Direction::Rx, &bytes[..split]);
                if split >= 9 && split < bytes.len() {
                    assert_eq!(state.directions[1].headers_seen, 1);
                    assert_eq!(state.directions[1].frames_complete, 0);
                }
                state.feed(Direction::Rx, &bytes[split..]);
                assert_eq!(state.directions[1].bytes, bytes.len() as u64);
                assert_eq!(state.directions[1].frames_complete, 1);
                assert!(
                    state
                        .events
                        .iter()
                        .flatten()
                        .any(|event| event.value == Some(expected))
                );
                assert!(!format!("{state:?}").contains("private-debug"));
            }
        }
    }

    #[test]
    fn header_seen_complete_and_eof_partial_are_distinct_without_length_allocation() {
        for cut in [0, 1, 8, 9, 10, 16, 17] {
            let observer = Observer::default();
            let observation = observer.wire(INSTANCE, 0, 1, None).unwrap();
            let ping = frame(6, 0, 0, b"private!");
            let inner = MockIo {
                reads: [ReadStep::Bytes(ping[..cut].to_vec()), ReadStep::Eof].into(),
                ..MockIo::default()
            };
            let mut io = WireIo::new(inner, Arc::clone(&observation));
            let waker = Waker::from(Arc::new(CountWake::default()));
            let mut cx = Context::from_waker(&waker);
            for _ in 0..2 {
                let mut storage = [0; 32];
                let mut buf = ReadBuf::new(&mut storage);
                assert!(matches!(
                    Pin::new(&mut io).poll_read(&mut cx, &mut buf),
                    Poll::Ready(Ok(()))
                ));
            }
            let state = observation.snapshot();
            let rx = &state.directions[1];
            assert_eq!(rx.headers_seen, u64::from(cut >= 9));
            assert_eq!(rx.frames_complete, u64::from(cut == 17));
            assert_eq!(rx.eof_partial(), cut > 0 && cut < 17);
            if (9..17).contains(&cut) {
                assert_eq!(rx.remaining, Some((17 - cut) as u32));
            }
            assert_eq!(rx.numeric, 0); // PING opaque bytes never enter scratch.
        }
        let mut state = WireState::default();
        let size = std::mem::size_of_val(&state);
        state.feed(Direction::Rx, &[255, 255, 255, 3, 0, 0, 0, 0, 1]);
        state.feed(Direction::Rx, b"private-payload");
        assert_eq!(state.directions[1].invalid_lengths, 1);
        assert_eq!(state.directions[1].remaining, Some(0xff_ffff - 15));
        assert_eq!(state.directions[1].numeric, 0);
        assert_eq!(std::mem::size_of_val(&state), size);
        for bytes in [
            frame(3, 0, 1, b"bad"),
            frame(7, 0, 0, b"short"),
            frame(4, 0, 0, b"seven!!"),
            frame(4, 1, 0, &[0; 6]),
            frame(8, 0, 1, &[0; 5]),
        ] {
            let mut malformed = WireState::default();
            malformed.feed(Direction::Rx, &bytes);
            assert_eq!(malformed.directions[1].invalid_lengths, 1);
            assert_eq!(malformed.directions[1].frames_complete, 1);
            assert!(
                malformed
                    .events
                    .iter()
                    .flatten()
                    .all(|event| event.value.is_none())
            );
        }
        let observer = Observer::default();
        let observation = observer.wire(INSTANCE, 0, 1, None).unwrap();
        let mut io = WireIo::new(
            MockIo {
                reads: [ReadStep::Eof, ReadStep::Eof].into(),
                ..MockIo::default()
            },
            Arc::clone(&observation),
        );
        let waker = Waker::from(Arc::new(CountWake::default()));
        let mut cx = Context::from_waker(&waker);
        let mut empty = [];
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut ReadBuf::new(&mut empty)),
            Poll::Ready(Ok(()))
        ));
        assert!(!observation.snapshot().directions[1].eof);
        let mut storage = [0; 1];
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut ReadBuf::new(&mut storage)),
            Poll::Ready(Ok(()))
        ));
        assert!(observation.snapshot().directions[1].eof);
    }

    #[test]
    fn first_headers_cancel_and_completed_settings_survive_ring_overwrite() {
        // CONTROLLED numerical frames exercise retention, not a live cause.
        let observer = Observer::default();
        let wire = observer.wire(INSTANCE, 0, 1, None).unwrap();
        wire.feed(Direction::Tx, H2_PREFACE);
        wire.feed(Direction::Tx, &frame(1, 1, 11, b"private-request-hpack"));
        wire.feed(Direction::Tx, &frame(9, 4, 11, b"private-continuation"));
        wire.feed(Direction::Rx, &frame(1, 4, 11, b"private-response-hpack"));
        wire.feed(Direction::Tx, &frame(3, 0, 11, &8_u32.to_be_bytes()));
        let settings = frame(4, 0, 0, &[0, 4, 0, 0, 255, 255, 0, 5, 0, 0, 64, 0]);
        wire.feed(Direction::Rx, &settings[..15]);
        assert!(wire.snapshot().directions[1].settings_complete[3].is_none());
        wire.feed(Direction::Rx, &settings[15..]);
        let first = wire.snapshot();
        let original = first.streams[0].unwrap();
        assert_eq!(
            first.directions[1].settings_complete[3].unwrap().value,
            65535
        );
        assert_eq!(
            first.directions[1].settings_complete[4].unwrap().value,
            16384
        );
        let tx = original.directions[0];
        assert!(tx.first_header.unwrap().sequence < tx.first_complete.unwrap().sequence);
        assert!(tx.first_complete.unwrap().sequence < tx.first_block.unwrap().sequence);
        for _ in 0..WIRE_EVENTS {
            wire.feed(Direction::Tx, &frame(3, 0, 11, &5_u32.to_be_bytes()));
            wire.feed(Direction::Rx, &frame(6, 0, 0, b"private!"));
        }
        // An incomplete replacement SETTINGS frame must not commit its tuple.
        let replacement = frame(4, 0, 0, &[0, 4, 0, 0, 0, 42, 0, 1, 0, 0, 0, 0]);
        wire.feed(Direction::Rx, &replacement[..15]);
        let retained = wire.snapshot();
        let stream = retained.streams[0].unwrap();
        assert!(retained.events_overwritten > 0);
        for direction in [0, 1] {
            let before = original.directions[direction];
            let after = stream.directions[direction];
            assert_eq!(after.first_header, before.first_header);
            assert_eq!(after.first_complete, before.first_complete);
            assert_eq!(after.first_block, before.first_block);
        }
        assert_eq!(stream.first_cancel, original.first_cancel);
        assert!(stream.reset.unwrap().sequence > stream.first_cancel.unwrap().sequence);
        assert_eq!(stream.reset_reason, Some(5));
        assert_eq!(
            retained.directions[1].settings_complete[3].unwrap().value,
            65535
        );
        assert!(retained.events.iter().flatten().all(|event| {
            event.point.header.kind != 1 && !matches!(event.value, Some(WireValue::Reset(8)))
        }));
        let capture = observer.capture();
        let mut text = String::new();
        capture.write_wire_core(&mut text, Instant::now());
        assert!(text.contains("wire first s_hex=b"));
        assert!(text.contains("settings_complete Rx id=4 value=65535"));
        assert!(!text.contains("private"));
        assert!(!format!("{retained:?}").contains("private"));
        wire.feed(Direction::Rx, &replacement[15..]);
        assert_eq!(
            wire.snapshot().directions[1].settings_complete[3]
                .unwrap()
                .value,
            42
        );
    }

    #[test]
    fn frozen_wire_capture_excludes_later_drop_reset_and_goaway() {
        struct DropFrames(Arc<WireObservation>);
        impl Drop for DropFrames {
            fn drop(&mut self) {
                self.0
                    .feed(Direction::Tx, &frame(3, 0, 13, &8_u32.to_be_bytes()));
                self.0.feed(Direction::Tx, &frame(7, 0, 0, &[0; 8]));
            }
        }
        let observer = Observer::default();
        let wire = observer.wire(INSTANCE, 2, 1, None).unwrap();
        wire.feed(Direction::Tx, H2_PREFACE);
        wire.feed(Direction::Tx, &frame(1, 5, 13, b"private-hpack"));
        let guard = DropFrames(Arc::clone(&wire));
        let capture = observer.capture();
        let sampled_at = Instant::now();
        let mut before = String::new();
        capture.write_wire(&mut before, sampled_at);
        drop(guard);
        let mut frozen = String::new();
        capture.write_wire(&mut frozen, sampled_at);
        assert_eq!(before, frozen);
        let state = wire.snapshot();
        assert!(state.streams[0].unwrap().first_cancel.is_some());
        assert!(state.has_goaway(Direction::Tx, 0, 0));
        let mut after = String::new();
        observer.write_wire(&mut after, sampled_at);
        assert_ne!(frozen, after);
        assert!(!frozen.contains("goaway_last="));
        assert!(after.contains("goaway_last=0 reason=0"));
    }

    #[test]
    fn wire_capacity_loss_concurrent_isolation_and_compact_reserve_are_bounded() {
        let left = Observer::default();
        let right = Observer::default();
        std::thread::scope(|scope| {
            for (observer, kind) in [(&left, 1), (&right, 0)] {
                scope.spawn(move || {
                    for owner in [0, 2] {
                        let wire = observer.wire(INSTANCE, owner, 1, None).unwrap();
                        for stream in 1..=WIRE_STREAMS + 5 {
                            wire.feed(Direction::Rx, &frame(kind, 4, stream as u32, &[]));
                        }
                    }
                    for _ in 0..5 {
                        assert!(observer.wire(INSTANCE, 4, 2, None).is_none());
                    }
                });
            }
        });
        for (observer, frames) in [(&left, 0), (&right, 41)] {
            assert_eq!(observer.wires().len(), 2);
            assert_eq!(observer.wire.lock().unwrap().omitted, 5);
            for wire in observer.wires() {
                let state = wire.snapshot();
                assert_eq!(state.streams.iter().flatten().count(), WIRE_STREAMS);
                assert_eq!(state.streams_omitted, 5);
                assert_eq!(state.directions[1].data_complete, frames);
                if frames == 0 {
                    assert_eq!(state.events_overwritten, 18);
                } else {
                    assert_eq!(state.events_overwritten, 0);
                }
            }
        }
        // First marks/settings and controls without a retained stream owner
        // must fit before optional DATA and owned-control history.
        let mark = WireMark {
            sequence: u64::MAX,
            at: Instant::now(),
        };
        let data = DataProgress {
            seen: u64::MAX,
            complete: u64::MAX,
            bytes: u64::MAX,
            length: u32::MAX,
            flags: u8::MAX,
            first: Some(mark),
            last: Some(mark),
        };
        for wire in left.wires() {
            let mut state = wire.state.lock().unwrap();
            state.sequence = u64::MAX;
            state.streams_omitted = u64::MAX;
            state.events_overwritten = u64::MAX;
            for direction in &mut state.directions {
                direction.bytes = u64::MAX;
                direction.headers_seen = u64::MAX;
                direction.frames_complete = u64::MAX;
                direction.headers_complete = u64::MAX;
                direction.blocks_complete = u64::MAX;
                direction.data_complete = u64::MAX;
                direction.invalid_lengths = u64::MAX;
                direction.invalid_streams = u64::MAX;
                direction.settings_omitted = u64::MAX;
                direction.errors = u64::MAX;
                direction.settings_complete = [Some(Setting {
                    value: u32::MAX,
                    mark,
                }); 6];
                let point = FramePoint {
                    mark,
                    header: FrameHeader {
                        length: u32::MAX,
                        kind: u8::MAX,
                        flags: u8::MAX,
                        stream: u32::MAX,
                    },
                };
                direction.header = point.header;
                direction.header_bytes = 9;
                direction.remaining = Some(u32::MAX);
                direction.last_header = Some(point);
                direction.last_complete = Some(point);
                direction.eof_mark = Some(mark);
            }
            for (index, stream) in state.streams.iter_mut().flatten().enumerate() {
                stream.id = 0x7fff_ffff - index as u32;
                stream.directions = [StreamDirection {
                    headers_seen: u64::MAX,
                    headers_complete: u64::MAX,
                    blocks_complete: u64::MAX,
                    first_header: Some(mark),
                    first_complete: Some(mark),
                    first_block: Some(mark),
                    data,
                }; 2];
                stream.reset = Some(mark);
                stream.reset_reason = Some(u32::MAX);
                stream.first_cancel = Some(mark);
                stream.late = data;
            }
        }
        for wire in left.wires() {
            let mut state = wire.state.lock().unwrap();
            state.events = [Some(WireEvent {
                direction: Direction::Tx,
                point: FramePoint {
                    mark,
                    header: FrameHeader {
                        length: u32::MAX,
                        kind: 7,
                        flags: u8::MAX,
                        stream: 0,
                    },
                },
                complete: true,
                value: Some(WireValue::GoAway {
                    last_stream: u32::MAX,
                    reason: u32::MAX,
                }),
            }); WIRE_EVENTS];
        }
        let mut compact = String::new();
        left.capture().write_wire_core(&mut compact, mark.at);
        // Reserve maximum-width ages for first HEADERS/CANCEL/latest reset,
        // completed SETTINGS and every unowned control, plus decimal point ages.
        let widest_ages = WIRE_CONNECTIONS * ((WIRE_STREAMS * 8 + WIRE_EVENTS + 12) * 15 + 240);
        let widest_identity = WIRE_CONNECTIONS * 256;
        assert!(compact.len() + widest_ages + widest_identity < 48 * 1024);
        assert_eq!(compact.matches("wire first s_hex=").count(), 72);
        assert_eq!(compact.matches("wire control_hex ").count(), 128);
        assert_eq!(compact.matches("settings_complete ").count(), 24);
    }

    #[test]
    fn slots_bound_tasks_requests_and_socket_ordinals_per_instance() {
        let observer = Observer::default();
        let other = Observer::default();
        let socket = Some(SocketAddr::from(([127, 0, 0, 1], 12345)));
        for ordinal in 0..TASK_SLOTS + 5 {
            let task = observer.task("test", 0, 1, socket, ordinal);
            assert_eq!(task.is_some(), ordinal < TASK_SLOTS);
        }
        for ordinal in 1..=REQUEST_SLOTS + 5 {
            let request = observer.request(socket);
            assert_eq!(request.is_some(), ordinal <= REQUEST_SLOTS);
            if let Some(request) = request {
                assert_eq!(request.ordinal, ordinal);
                assert_eq!(request.socket, socket);
            }
        }
        let slots = observer.slots.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(slots.tasks_omitted, 5);
        assert_eq!(slots.requests_omitted, 5);
        assert!(other.tasks("test").is_empty());
        assert!(other.requests().is_empty());
    }

    #[test]
    fn server_progress_keeps_missing_tls_identity_unknown() {
        use ferrum_alloy::telemetry::peer::PeerInfo;

        for version in [http::Version::HTTP_11, http::Version::HTTP_2] {
            let observer = Observer::default();
            let socket = Some(SocketAddr::from(([127, 0, 0, 1], 12345)));
            let mut request = http::Request::builder().version(version).body(()).unwrap();
            request.extensions_mut().insert(PeerInfo {
                remote_addr: socket,
                tls: None,
            });
            let observation = observer.server_request(&request).unwrap();
            observer.wire(INSTANCE, 2, 1, socket).unwrap();
            assert_eq!(observation.socket, socket);
            assert_eq!(observation.identity.version, Some(version));
            assert_eq!(observation.identity.tls, None);
            assert_eq!(observation.identity.verified_client, None);
            let mut text = String::new();
            observer
                .capture()
                .write_server_progress(&mut text, Instant::now());
            assert!(text.contains("client_owner_generation=Some((2, 1))"));
            assert!(text.contains("tls=None verified_client=None"));
        }
    }

    #[test]
    fn server_progress_preserves_tls_identity_verification() {
        use ferrum_alloy::telemetry::peer::{PeerInfo, TlsPeer};

        for verified in [false, true] {
            let observer = Observer::default();
            let mut request = http::Request::new(());
            request.extensions_mut().insert(PeerInfo {
                remote_addr: Some(SocketAddr::from(([127, 0, 0, 1], 12345))),
                tls: Some(TlsPeer {
                    client_cert_verified: verified,
                    ..TlsPeer::default()
                }),
            });
            let observation = observer.server_request(&request).unwrap();
            assert_eq!(observation.identity.tls, Some(true));
            assert_eq!(observation.identity.verified_client, Some(verified));
            let mut text = String::new();
            observer
                .capture()
                .write_server_progress(&mut text, Instant::now());
            let expected = format!("tls=Some(true) verified_client=Some({verified})");
            assert!(text.contains(&expected));
        }
    }

    #[test]
    fn server_progress_keeps_missing_peer_unknown_with_connect_info_fallback() {
        use axum::extract::ConnectInfo;
        use ferrum_alloy::telemetry::peer::PeerInfo;

        let socket = SocketAddr::from(([127, 0, 0, 1], 12345));
        for with_peer in [false, true] {
            let observer = Observer::default();
            let mut request = http::Request::new(());
            let observation = observer.server_request(&request).unwrap();
            assert_eq!(observation.socket, None);
            assert_eq!(observation.identity.tls, None);
            assert_eq!(observation.identity.verified_client, None);
            request.extensions_mut().insert(ConnectInfo(socket));
            if with_peer {
                request.extensions_mut().insert(PeerInfo::default());
            }
            let observation = observer.server_request(&request).unwrap();
            assert_eq!(observation.socket, Some(socket));
            assert_eq!(observation.identity.tls, None);
            assert_eq!(observation.identity.verified_client, None);
            let mut text = String::new();
            observer
                .capture()
                .write_server_progress(&mut text, Instant::now());
            assert_eq!(text.matches("tls=None verified_client=None").count(), 2);
        }
    }

    #[tokio::test]
    async fn server_custody_follows_native_bytes_owners_after_body_drop() {
        use axum::body::Body;
        use axum::response::Response;
        use bytes::Buf;
        use ferrum_alloy::telemetry::peer::{PeerInfo, TlsPeer};
        use http_body_util::BodyExt;

        let observer = Observer::default();
        let mut request = http::Request::builder()
            .version(http::Version::HTTP_2)
            .header("x-private", "RAW-HEADER")
            .body(())
            .unwrap();
        request.extensions_mut().insert(PeerInfo {
            remote_addr: Some(SocketAddr::from(([127, 0, 0, 1], 12345))),
            tls: Some(TlsPeer {
                client_cert_verified: true,
                spiffe_id: Some("RAW-SPIFFE".into()),
                dns_names: vec!["RAW-DNS".into()],
            }),
        });
        let observation = observer.server_request(&request).unwrap();
        let socket = observation.socket;
        observer.wire(INSTANCE, 2, 1, socket).unwrap();
        let observed = Arc::clone(&observation);
        let (mut data, poll_task) = tokio::spawn(async move {
            let task = tokio::task::id();
            let future = std::future::ready(Response::new(Body::from("RAW-BODY")));
            let response = response(future, Some(observed)).await;
            let mut body = response.into_body();
            let data = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert!(body.frame().await.is_none());
            drop(body);
            (data, task)
        })
        .await
        .unwrap();
        assert_eq!(data.as_ref(), b"RAW-BODY");
        let clone = data.copy_to_bytes(3).clone();
        drop(data);
        let body = observation.body.snapshot();
        assert!(body.dropped);
        assert_eq!(body.last_task, Some(poll_task));
        assert_eq!(body.drop_task, Some(poll_task));
        let handler = observation.handler.snapshot();
        assert_eq!(handler.last_task, Some(poll_task));
        let state = observation.custody.lock().unwrap().clone();
        assert_eq!(state.last_result, BodyResult::End);
        assert_eq!((state.issued_buffers, state.retained_buffers), (1, 1));
        assert_eq!((state.issued_bytes, state.retained_bytes), (8, 8));
        assert!(state.last_release.is_none());
        let capture = observer.capture();
        let now = Instant::now();
        let mut before = String::new();
        capture.write_server_progress(&mut before, now);
        assert!(before.contains("client_owner_generation=Some((2, 1))"));
        assert!(before.contains("version=Some(HTTP/2.0) tls=Some(true)"));
        assert!(before.contains("verified_client=Some(true)"));
        assert!(before.contains("bytes=8/8"));
        assert!(before.contains("body_drop=true"));
        assert!(!before.contains("RAW-"));
        let release_task = tokio::spawn(async move {
            let task = tokio::task::id();
            drop(clone);
            task
        })
        .await
        .unwrap();
        let state = observation.custody.lock().unwrap().clone();
        assert_eq!((state.retained_buffers, state.retained_bytes), (0, 0));
        assert!(state.last_release.is_some());
        assert_eq!(state.release_task, Some(release_task));
        let mut frozen = String::new();
        capture.write_server_progress(&mut frozen, now);
        assert_eq!(before, frozen);
        let mut after = String::new();
        observer.capture().write_server_progress(&mut after, now);
        assert!(!after.contains("server progress socket="));
        assert!(after.contains("transport_progress=unobserved"));
        assert!(after.contains("buffer_release_is_not_transport_write"));
    }

    #[tokio::test]
    async fn server_progress_distinguishes_gate_pending_and_opaque_body_error() {
        use axum::body::Body;
        use axum::response::Response;
        use http_body_util::BodyExt;
        use hyper::body::Body as _;

        struct FailedBody;

        impl hyper::body::Body for FailedBody {
            type Data = bytes::Bytes;
            type Error = io::Error;

            fn poll_frame(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
                Poll::Ready(Some(Err(io::Error::other("RAW-BODY-ERROR"))))
            }
        }

        let observer = Observer::default();
        let observation = observer.request(None).unwrap();
        let gate = Arc::new(Gate::default());
        let future = std::future::ready(Response::new(Body::new(FailedBody)));
        let response = response_with_gates(
            future,
            Some(Arc::clone(&observation)),
            None,
            Some(Arc::clone(&gate)),
        )
        .await;
        let mut body = response.into_body();
        std::future::poll_fn(|cx| {
            assert!(Pin::new(&mut body).poll_frame(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(observation.body.snapshot().inner_polls, 0);
        assert_eq!(
            observation.custody.lock().unwrap().last_result,
            BodyResult::GatePending
        );
        gate.release();
        assert!(body.frame().await.unwrap().is_err());
        assert_eq!(observation.body.snapshot().inner_polls, 1);
        let mut text = String::new();
        observer
            .capture()
            .write_server_progress(&mut text, Instant::now());
        assert!(text.contains("last=Error"));
        assert!(text.contains("bytes=0/0"));
        assert!(!text.contains("RAW-"));
    }

    #[test]
    fn server_progress_bounds_rows_without_retiring_buffer_custody() {
        let observer = Observer::default();
        let other = Observer::default();
        let socket = Some(SocketAddr::from(([127, 0, 0, 1], 12345)));
        let mut data = Vec::new();
        for ordinal in 1..=REQUEST_SLOTS + 5 {
            let observation = observer.request(socket);
            assert_eq!(observation.is_some(), ordinal <= REQUEST_SLOTS);
            if let Some(observation) = observation {
                observation.body.dropped();
                data.push(ObservedData::wrap(
                    bytes::Bytes::from_static(b"RAW-BUFFER"),
                    &observation,
                ));
            }
        }
        let capture = observer.capture();
        assert_eq!(capture.requests_omitted, 5);
        assert_eq!(capture.requests.iter().flatten().count(), REQUEST_SLOTS);
        for request in capture.requests.iter().flatten() {
            assert!(request.body.dropped);
            assert_eq!(request.custody.retained_buffers, 1);
        }
        let now = Instant::now();
        let mut text = String::new();
        capture.write_server_progress(&mut text, now);
        assert_eq!(
            text.matches("server progress socket=").count(),
            SERVER_PROGRESS_ROWS
        );
        assert!(text.contains("rows_max=4 omitted=68"));
        for ordinal in 69..=72 {
            assert!(text.contains(&format!("ordinal={ordinal} ")));
        }
        assert!(!text.contains("RAW-"));
        assert!(text.len() < 2048);
        drop(data);
        for request in observer.requests() {
            assert_eq!(request.custody.lock().unwrap().retained_buffers, 0);
        }
        let mut frozen = String::new();
        capture.write_server_progress(&mut frozen, now);
        assert_eq!(text, frozen);
        let mut isolated = String::new();
        other.capture().write_server_progress(&mut isolated, now);
        assert!(!isolated.contains("server progress socket="));
    }
}
