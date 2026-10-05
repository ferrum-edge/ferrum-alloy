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

use ferrum_alloy::bench_diagnostics::{IoObserver, Operation, Outcome};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const TASK_SLOTS: usize = 160;
const REQUEST_SLOTS: usize = 72;
const WIRE_CONNECTIONS: usize = 2;
const SERVER_CONNECTIONS: usize = 2;
const WIRE_STREAMS: usize = 36;
const WIRE_EVENTS: usize = 64;

const H2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

// Build one record at a time in heap storage. Converting the boxed slice
// preserves its allocation; no full fixed-capacity array is returned by value.
#[allow(
    clippy::unwrap_used,
    reason = "fixed-capacity test observation conversion"
)]
pub(crate) fn boxed_slots<T, const N: usize>(capture: impl FnMut(usize) -> T) -> Box<[T; N]> {
    (0..N)
        .map(capture)
        .collect::<Vec<_>>()
        .into_boxed_slice()
        .try_into()
        .ok()
        .unwrap()
}

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
pub(crate) struct SocketProgress {
    pub(crate) outcomes: [u64; 3],
    pub(crate) bytes: u64,
    requested: Option<usize>,
    in_poll: bool,
    last: Option<u8>,
    eof: bool,
    pub(crate) wakes: u64,
    last_poll: Option<WireMark>,
    progress: Option<WireMark>,
    last_wake: Option<WireMark>,
}

#[derive(Clone, Copy, Debug)]
struct TlsSample {
    flags: [bool; 3],
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
    outcomes: [[u64; 3]; 5],
    last_header: Option<FramePoint>,
    last_complete: Option<FramePoint>,
}

impl WireDirection {
    pub(crate) fn eof_partial(&self) -> bool {
        self.eof && (self.preface < H2_PREFACE.len() || self.header_bytes > 0)
    }
}

#[derive(Debug)]
pub(crate) struct WireState {
    pub(crate) directions: [WireDirection; 2],
    streams: Box<[Option<WireStream>; WIRE_STREAMS]>,
    events: Box<[Option<WireEvent>; WIRE_EVENTS]>,
    sequence: u64,
    reset_direction: Direction,
    events_next: usize,
    pub(crate) events_overwritten: u64,
    pub(crate) streams_omitted: u64,
    pub(crate) socket: Box<[SocketProgress; 3]>,
    socket_dropped: bool,
    tls_sample: Option<TlsSample>,
}

impl Clone for WireState {
    fn clone(&self) -> Self {
        Self {
            directions: self.directions.clone(),
            streams: boxed_slots(|index| self.streams[index]),
            events: boxed_slots(|index| self.events[index]),
            sequence: self.sequence,
            reset_direction: self.reset_direction,
            events_next: self.events_next,
            events_overwritten: self.events_overwritten,
            streams_omitted: self.streams_omitted,
            socket: boxed_slots(|index| self.socket[index].clone()),
            socket_dropped: self.socket_dropped,
            tls_sample: self.tls_sample,
        }
    }
}

impl Default for WireState {
    fn default() -> Self {
        let mut directions = std::array::from_fn(|_| WireDirection::default());
        // The server sends no connection preface string.
        directions[1].preface = H2_PREFACE.len();
        Self {
            directions,
            streams: boxed_slots(|_| None),
            events: boxed_slots(|_| None),
            sequence: 0,
            reset_direction: Direction::Tx,
            events_next: 0,
            events_overwritten: 0,
            streams_omitted: 0,
            socket: boxed_slots(|_| SocketProgress::default()),
            socket_dropped: false,
            tls_sample: None,
        }
    }
}

impl WireState {
    fn server() -> Self {
        let mut state = Self::default();
        state.directions[0].preface = H2_PREFACE.len();
        state.directions[1].preface = 0;
        state.reset_direction = Direction::Rx;
        state
    }

    pub(crate) fn header_streams(&self, direction: Direction) -> Vec<u32> {
        self.streams
            .iter()
            .flatten()
            .filter(|stream| stream.directions[direction.index()].blocks_complete > 0)
            .map(|stream| stream.id)
            .collect()
    }

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
        let reset_direction = self.reset_direction;
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
                late = direction != reset_direction && stream.reset.is_some();
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
        let reset_direction = self.reset_direction;
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
            if direction == reset_direction
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
    endpoint: &'static str,
    pub(crate) remote: Option<SocketAddr>,
    tls: bool,
}

impl WireObservation {
    fn capture(&self) -> WireCapture {
        WireCapture {
            instance: self.instance,
            owner: self.owner,
            generation: self.generation,
            socket: self.socket,
            remote: self.remote,
            tls: self.tls,
            endpoint: self.endpoint,
            state: Box::new(self.snapshot()),
        }
    }

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

impl IoObserver for WireObservation {
    fn prefix(&self, operation: Operation, bytes: &[u8]) {
        let direction = match operation {
            Operation::Read => Direction::Rx,
            Operation::Write | Operation::WriteVectored => Direction::Tx,
            Operation::Flush | Operation::Shutdown => return,
        };
        self.feed(direction, bytes);
    }

    fn outcome(&self, operation: Operation, outcome: Outcome, eof: bool) {
        let (direction, operation) = match operation {
            Operation::Read => (1, 0),
            Operation::Write => (0, 1),
            Operation::WriteVectored => (0, 2),
            Operation::Flush => (0, 3),
            Operation::Shutdown => (0, 4),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if eof {
            let mark = state.mark();
            state.directions[direction].eof = true;
            let _ = state.directions[direction].eof_mark.get_or_insert(mark);
        }
        let parser = &mut state.directions[direction];
        let result = match outcome {
            Outcome::Pending => 0,
            Outcome::Ok => 1,
            Outcome::Error => 2,
        };
        let counter = &mut parser.outcomes[operation][result];
        *counter = counter.saturating_add(1);
        // Tx errors count actual scalar/vector writes, never flush/shutdown.
        if result == 2 && operation <= 2 {
            parser.errors = parser.errors.saturating_add(1);
        }
    }

    fn socket_start(&self, operation: Operation, requested: Option<usize>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mark = state.mark();
        let progress = &mut state.socket[socket_index(operation)];
        progress.in_poll = true;
        progress.requested = requested;
        progress.last_poll = Some(mark);
    }

    fn socket_outcome(&self, operation: Operation, outcome: Outcome, bytes: usize, eof: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = match outcome {
            Outcome::Pending => 0,
            Outcome::Ok => 1,
            Outcome::Error => 2,
        };
        let made_progress = bytes > 0 || matches!(operation, Operation::Flush) && result == 1;
        let mark = made_progress.then(|| state.mark());
        let progress = &mut state.socket[socket_index(operation)];
        progress.in_poll = false;
        progress.last = Some(result as u8);
        progress.outcomes[result] = progress.outcomes[result].saturating_add(1);
        progress.bytes = progress.bytes.saturating_add(bytes as u64);
        progress.eof |= eof;
        if let Some(mark) = mark {
            progress.progress = Some(mark);
        }
    }

    fn socket_wake(&self, operation: Operation) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mark = state.mark();
        let progress = &mut state.socket[socket_index(operation)];
        progress.wakes = progress.wakes.saturating_add(1);
        progress.last_wake = Some(mark);
    }

    fn socket_drop(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .socket_dropped = true;
    }

    fn tls_state(&self, flags: [bool; 3]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.tls_sample = Some(TlsSample {
            flags,
            mark: state.mark(),
        });
    }
}

fn socket_index(operation: Operation) -> usize {
    match operation {
        Operation::Read => 0,
        Operation::Write | Operation::WriteVectored => 1,
        Operation::Flush | Operation::Shutdown => 2,
    }
}

#[derive(Clone)]
struct WireCapture {
    instance: [u8; 36],
    owner: usize,
    generation: u64,
    socket: Option<SocketAddr>,
    state: Box<WireState>,
    endpoint: &'static str,
    remote: Option<SocketAddr>,
    tls: bool,
}

impl WireCapture {
    fn write_core(&self, text: &mut impl Write, now: Instant) {
        let state = &self.state;
        let instance = std::str::from_utf8(&self.instance).unwrap_or("invalid-instance");
        let _ = writeln!(
            text,
            "wire instance={instance} owner={} gen={} socket={:?} seq={} \
             stream_header_omissions={} control_overwrites={} endpoint={} remote={:?} tls={}",
            self.owner,
            self.generation,
            self.socket,
            state.sequence,
            state.streams_omitted,
            state.events_overwritten,
            self.endpoint,
            self.remote,
            self.tls,
        );
        let _ = writeln!(text, "socket dropped={}", u8::from(state.socket_dropped));
        for (index, progress) in state.socket.iter().enumerate() {
            let _ = write!(
                text,
                "socket {index} {:x}/{:x}/{:x} {:x} ",
                progress.outcomes[0], progress.outcomes[1], progress.outcomes[2], progress.bytes,
            );
            if let Some(requested) = progress.requested {
                let _ = write!(text, "{requested:x}");
            } else {
                let _ = write!(text, "-");
            }
            let _ = write!(
                text,
                " {:x} {}/",
                progress.wakes,
                u8::from(progress.in_poll)
            );
            if let Some(last) = progress.last {
                let _ = write!(text, "{last}");
            } else {
                let _ = write!(text, "-");
            }
            let _ = write!(text, "/{} ", u8::from(progress.eof));
            for (offset, mark) in [progress.last_poll, progress.progress, progress.last_wake]
                .into_iter()
                .enumerate()
            {
                if offset > 0 {
                    let _ = write!(text, "/");
                }
                write_mark(text, mark, now);
            }
            let _ = writeln!(text);
        }
        let _ = write!(text, "tls demand ");
        if let Some(sample) = state.tls_sample {
            let _ = write!(
                text,
                "{}/{}/{} ",
                u8::from(sample.flags[0]),
                u8::from(sample.flags[1]),
                u8::from(sample.flags[2]),
            );
            write_mark(text, Some(sample.mark), now);
        } else {
            let _ = write!(text, "-");
        }
        let _ = writeln!(text);
        for direction in [Direction::Tx, Direction::Rx] {
            let parser = &state.directions[direction.index()];
            let _ = write!(
                text,
                "wire {direction:?} counts_hex={:x}/{:x}/{:x}/{:x}/{:x}/{:x}/{:x}/{:x}/{:x}/{:x} \
                 preface={}/24 flags={}/{}/{} hb={} remaining_hex=",
                parser.bytes,
                parser.headers_seen,
                parser.frames_complete,
                parser.headers_complete,
                parser.blocks_complete,
                parser.data_complete,
                parser.invalid_lengths,
                parser.invalid_streams,
                parser.settings_omitted,
                parser.errors,
                parser.preface,
                u8::from(parser.preface_invalid),
                u8::from(parser.eof),
                u8::from(parser.eof_partial()),
                parser.header_bytes,
            );
            write_optional_hex(text, parser.remaining);
            let _ = write!(text, " partial_hex=");
            let partial = [
                (parser.header_bytes >= 4).then_some(u32::from(parser.header.kind)),
                (parser.header_bytes >= 5).then_some(u32::from(parser.header.flags)),
                (parser.header_bytes >= 3).then_some(parser.header.length),
                (parser.header_bytes >= 9).then_some(parser.header.stream),
            ];
            for (index, value) in partial.into_iter().enumerate() {
                if index > 0 {
                    let _ = write!(text, "/");
                }
                write_optional_hex(text, value);
            }
            let _ = writeln!(text);
            let (operations, outcomes) = match direction {
                Direction::Tx => ("scalar/vector/flush/shutdown", &parser.outcomes[1..]),
                Direction::Rx => ("read", &parser.outcomes[..1]),
            };
            let _ = writeln!(
                text,
                "wire {direction:?} outcomes_hex({operations})={outcomes:x?} \
                 fields=pending/ok/error",
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
                let _ = write!(text, "wire {direction:?} eof_mark_hex=");
                write_mark(text, Some(mark), now);
                let _ = writeln!(text);
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
                        "ws {direction:?} {:x} {:x} ",
                        index + 1,
                        setting.value,
                    );
                    write_mark(text, Some(setting.mark), now);
                    let _ = writeln!(text);
                }
            }
        }
    }

    fn write_first(&self, text: &mut impl Write, now: Instant) {
        let state = &self.state;
        let _ = writeln!(
            text,
            "wire first owner={} gen={} endpoint={} remote={:?} \
             reset_direction={:?}",
            self.owner, self.generation, self.endpoint, self.remote, state.reset_direction,
        );
        for stream in state.streams.iter().flatten() {
            let _ = write!(text, "wf {:x}", stream.id);
            for direction in [Direction::Tx, Direction::Rx] {
                let progress = stream.directions[direction.index()];
                let _ = write!(text, " ");
                write_mark(text, progress.first_header, now);
                let _ = write!(text, "/");
                write_mark(text, progress.first_complete, now);
                let _ = write!(text, "/");
                write_mark(text, progress.first_block, now);
            }
            let _ = write!(text, " ");
            write_mark(text, stream.first_cancel, now);
            let _ = write!(text, " ");
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
            "wire streams_hex endpoint={} owner={:x} gen={:x} \
             s=numeric-stream-id h=seen/complete/blocks \
             d(DATA)=seen/complete/payload_bytes,last_length:flags,first,last \
             marks=seq:age_us late=opposite-direction-DATA-header-after-complete-RST \
             reset_direction={:?}",
            self.endpoint, self.owner, self.generation, state.reset_direction,
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
            "wire controls_hex endpoint={} owner={:x} gen={:x} unowned={unowned}",
            self.endpoint, self.owner, self.generation,
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
                    "wc {:?} {} ",
                    event.direction,
                    u8::from(event.complete),
                );
                write_mark(text, Some(event.point.mark), now);
                let _ = write!(
                    text,
                    " {:x}/{:x}/{:x}/{:x}",
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
                        let _ = write!(text, " goaway={last_stream:x}:{reason:x}");
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
    let _ = write!(text, "hex=");
    write_mark(text, Some(point.mark), now);
    let _ = write!(
        text,
        " {:x}/{:x}/{:x}/{:x} {}/{}",
        header.kind,
        header.flags,
        header.length,
        header.stream,
        u8::from(header.invalid_length()),
        u8::from(header.invalid_stream()),
    );
}

fn write_optional_hex(text: &mut impl Write, value: Option<u32>) {
    if let Some(value) = value {
        let _ = write!(text, "{value:x}");
    } else {
        let _ = write!(text, "-");
    }
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

// Both endpoints use the identical one-delegation wrapper. The library is
// explicitly compiled with the seam by the benchmark's dev dependency only.
pub(crate) type WireIo<I> = ferrum_alloy::bench_diagnostics::PlaintextIo<I>;

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
        self.state.lock().unwrap_or_else(|e| e.into_inner()).dropped = true;
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

pub(crate) struct RequestObservation {
    pub(crate) socket: Option<SocketAddr>,
    pub(crate) ordinal: usize,
    entered: Instant,
    pub(crate) response: Mutex<Option<Instant>>,
    pub(crate) handler: Arc<PollObservation>,
    pub(crate) body: Arc<PollObservation>,
    pub(crate) frames: AtomicU64,
}

struct Slots {
    tasks: [Option<Task>; TASK_SLOTS],
    requests: [Option<Arc<RequestObservation>>; REQUEST_SLOTS],
    tasks_omitted: u64,
    requests_omitted: u64,
}

struct WireSlots<const CONNECTIONS: usize> {
    connections: [Option<Arc<WireObservation>>; CONNECTIONS],
    omitted: u64,
}

#[derive(Clone)]
struct TaskCapture {
    kind: &'static str,
    owner: usize,
    generation: u64,
    socket: Option<SocketAddr>,
    ordinal: usize,
    state: PollState,
}

#[derive(Clone)]
struct RequestCapture {
    socket: Option<SocketAddr>,
    ordinal: usize,
    entered: Instant,
    response: Option<Instant>,
    handler: PollState,
    body: PollState,
    frames: u64,
}

pub(crate) struct ObserverCapture {
    tasks: Box<[Option<TaskCapture>; TASK_SLOTS]>,
    requests: Box<[Option<RequestCapture>; REQUEST_SLOTS]>,
    connections: [Option<WireCapture>; WIRE_CONNECTIONS],
    server_connections: [Option<WireCapture>; SERVER_CONNECTIONS],
    server_connections_omitted: u64,
    tasks_omitted: u64,
    requests_omitted: u64,
    connections_omitted: u64,
}

impl Clone for ObserverCapture {
    fn clone(&self) -> Self {
        Self {
            tasks: boxed_slots(|index| self.tasks[index].clone()),
            requests: boxed_slots(|index| self.requests[index].clone()),
            connections: self.connections.clone(),
            server_connections: self.server_connections.clone(),
            server_connections_omitted: self.server_connections_omitted,
            tasks_omitted: self.tasks_omitted,
            requests_omitted: self.requests_omitted,
            connections_omitted: self.connections_omitted,
        }
    }
}

pub(crate) struct Observer {
    slots: Mutex<Slots>,
    wire: Mutex<WireSlots<WIRE_CONNECTIONS>>,
    server_wire: Mutex<WireSlots<SERVER_CONNECTIONS>>,
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
            server_wire: Mutex::new(WireSlots {
                connections: std::array::from_fn(|_| None),
                omitted: 0,
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
        self.wire_endpoint(instance, owner, generation, socket, None, false)
    }

    pub(crate) fn wire_endpoint(
        &self,
        instance: &str,
        owner: usize,
        generation: u64,
        socket: Option<SocketAddr>,
        remote: Option<SocketAddr>,
        tls: bool,
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
            endpoint: "client",
            remote,
            tls,
        });
        *slot = Some(Arc::clone(&observation));
        Some(observation)
    }

    pub(crate) fn server_wire(
        &self,
        instance: &str,
        local: Option<SocketAddr>,
        remote: SocketAddr,
        tls: bool,
    ) -> Option<Arc<WireObservation>> {
        let mut slots = self.server_wire.lock().unwrap_or_else(|e| e.into_inner());
        let index = slots.connections.iter().position(Option::is_none);
        let Some(index) = index else {
            slots.omitted = slots.omitted.saturating_add(1);
            return None;
        };
        let mut identity = [0; 36];
        let count = instance.len().min(identity.len());
        identity[..count].copy_from_slice(&instance.as_bytes()[..count]);
        let observation = Arc::new(WireObservation {
            instance: identity,
            owner: index,
            generation: 1,
            socket: local,
            remote: Some(remote),
            tls,
            endpoint: "server-accepted-connection",
            state: Mutex::new(WireState::server()),
        });
        slots.connections[index] = Some(Arc::clone(&observation));
        Some(observation)
    }

    pub(crate) fn server_wires(&self) -> Vec<Arc<WireObservation>> {
        let slots = self.server_wire.lock().unwrap_or_else(|e| e.into_inner());
        slots.connections.iter().flatten().map(Arc::clone).collect()
    }

    pub(crate) fn wires(&self) -> Vec<Arc<WireObservation>> {
        let slots = self.wire.lock().unwrap_or_else(|e| e.into_inner());
        slots.connections.iter().flatten().map(Arc::clone).collect()
    }

    pub(crate) fn capture(&self) -> Box<ObserverCapture> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let tasks = boxed_slots(|index| {
            slots.tasks[index].as_ref().map(|task| TaskCapture {
                kind: task.kind,
                owner: task.owner,
                generation: task.generation,
                socket: task.socket,
                ordinal: task.ordinal,
                state: task.observation.snapshot(),
            })
        });
        let requests = boxed_slots(|index| {
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
                })
        });
        let tasks_omitted = slots.tasks_omitted;
        let requests_omitted = slots.requests_omitted;
        drop(slots);
        let wire = self.wire.lock().unwrap_or_else(|e| e.into_inner());
        let connections = std::array::from_fn(|index| {
            wire.connections[index].as_ref().map(|wire| wire.capture())
        });
        let connections_omitted = wire.omitted;
        drop(wire);
        let server = self.server_wire.lock().unwrap_or_else(|e| e.into_inner());
        Box::new(ObserverCapture {
            tasks,
            requests,
            connections,
            server_connections: std::array::from_fn(|index| {
                server.connections[index]
                    .as_ref()
                    .map(|wire| wire.capture())
            }),
            server_connections_omitted: server.omitted,
            tasks_omitted,
            requests_omitted,
            connections_omitted,
        })
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
        });
        *slot = Some(Arc::clone(&request));
        Some(request)
    }
}

impl ObserverCapture {
    pub(crate) fn retained_counts(&self) -> [usize; 9] {
        let clients = self.connections.iter().flatten();
        let servers = self.server_connections.iter().flatten();
        [
            self.tasks.iter().flatten().count(),
            self.requests.iter().flatten().count(),
            clients.clone().count(),
            servers.clone().count(),
            clients
                .clone()
                .map(|wire| wire.state.streams.iter().flatten().count())
                .sum(),
            servers
                .clone()
                .map(|wire| wire.state.streams.iter().flatten().count())
                .sum(),
            clients
                .clone()
                .map(|wire| wire.state.events.iter().flatten().count())
                .sum(),
            servers
                .clone()
                .map(|wire| wire.state.events.iter().flatten().count())
                .sum(),
            clients
                .chain(servers)
                .flat_map(|wire| &wire.state.directions)
                .map(|direction| direction.settings_complete.iter().flatten().count())
                .sum(),
        ]
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
        if self.server_connections.iter().any(Option::is_some)
            || self.server_connections_omitted > 0
        {
            let _ = writeln!(
                text,
                "server wire slots(connection,stream,control)=({SERVER_CONNECTIONS},{WIRE_STREAMS},{WIRE_EVENTS}) \
                 connections_omitted={} boundary=server-plaintext-I/O \
                 client-Tx/server-Rx=preface server-Tx/client-Rx=no-preface \
                 server_owner=accepted-socket-ordinal handler_stream_id=unknown \
                 endpoint_samples=sequential-non-atomic TLS_acceptance_is_not_ciphertext_transmission",
                self.server_connections_omitted,
            );
        } else {
            let _ = writeln!(
                text,
                "server wire observed_connections=0 status=no-retained-server-endpoint",
            );
        }
        if self
            .connections
            .iter()
            .chain(&self.server_connections)
            .any(Option::is_some)
        {
            let _ = writeln!(
                text,
                "wire compact_hex counts=bytes/seen/complete/headers/blocks/data/invalid_length/invalid_stream/settings_omitted/errors \
                 flags=bad_preface/eof/eof_partial hb=header_bytes remaining=payload-bytes \
                 partial=type/flags/length/stream '-'=unobserved",
            );
            let _ = writeln!(
                text,
                "wire compact_hex mark=seq:age_us point=mark,type/flags/length/stream,invalid_length/invalid_stream \
                 settings=direction,id,value,mark control=direction,complete,mark,type/flags/length/stream \
                 goaway(last-stream:reason)",
            );
            let _ = writeln!(
                text,
                "wire first columns=Tx,Rx,cancel,rst Tx/Rx=first-HEADERS-seen/complete/END_HEADERS \
                 cancel=first-complete-CANCEL rst=latest-complete-RST:reason_hex \
                 rows(wf/wc/ws)=first-stream/control/completed-SETTINGS",
            );
            let _ = writeln!(
                text,
                "socket hex rows=0(read)/1(scalar+vector-write)/2(flush) \
                 columns=pending/ok/error,bytes,last_requested,wakes,in_poll/last_outcome/eof,last_poll/progress/last_wake \
                 outcome=0(Pending)/1(Ok)/2(error) marks=endpoint-seq:age_us '-'=unobserved \
                 progress=positive-bytes-or-Ok-flush shutdown=unobserved",
            );
            let _ = writeln!(
                text,
                "tls demand=wants_read/wants_write/handshaking,mark \
                 buffer_lengths/kernel_queues/need_flush=unknown \
                 socket_bytes=encrypted-if-tls TLS_flags=after-poll-sequential \
                 endpoint_copy=one-lock-records-only cross_endpoint=sequential-non-atomic",
            );
        }
        for observation in self
            .connections
            .iter()
            .chain(&self.server_connections)
            .flatten()
        {
            observation.write_core(text, now);
        }
        for observation in self.connections.iter().flatten() {
            observation.write_first(text, now);
        }
        // Preserve controls without a retained stream owner, including stream
        // zero and omitted stream IDs, before potentially saturated DATA detail.
        for observation in self.connections.iter().flatten() {
            observation.write_events(text, now, true);
        }
    }

    pub(crate) fn write_wire_detail(&self, text: &mut impl Write, now: Instant) {
        for observation in self.server_connections.iter().flatten() {
            observation.write_first(text, now);
            observation.write_events(text, now, true);
            observation.write_events(text, now, false);
            observation.write_streams(text, now);
        }
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

impl<B: hyper::body::Body> hyper::body::Body for ObservedBody<B> {
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
            this.observation.body.finish(start, false, false);
            return Poll::Pending;
        }
        let result = this.inner.as_mut().poll_frame(cx);
        this.observation.body.finish(start, result.is_ready(), true);
        if matches!(&result, Poll::Ready(Some(Ok(_)))) {
            this.observation.frames.fetch_add(1, Ordering::Relaxed);
        }
        result
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
    fn socket_observer_counts_actual_prefixes_and_forwards_wakes_without_retaining_bytes() {
        use ferrum_alloy::bench_diagnostics::SocketIo;

        let observer = Observer::default();
        let wire = observer.wire(INSTANCE, 0, 1, None).unwrap();
        let wake = Arc::new(CountWake::default());
        let waker = Waker::from(Arc::clone(&wake));
        let mut cx = Context::from_waker(&waker);
        let inner = MockIo {
            reads: [
                ReadStep::Pending,
                ReadStep::Bytes(b"private-ciphertext".to_vec()),
                ReadStep::Error,
                ReadStep::Eof,
                ReadStep::Eof,
            ]
            .into(),
            writes: [WriteStep::Pending, WriteStep::Accept(3), WriteStep::Error].into(),
            vectored: true,
            ..MockIo::default()
        };
        let mut io = SocketIo::new(inner);
        io.observe(Some(wire.clone()));
        let mut storage = [0; 64];
        let mut buf = ReadBuf::new(&mut storage);
        buf.put_slice(b"old");
        let pointer = buf.filled().as_ptr() as usize;
        assert!(Pin::new(&mut io).poll_read(&mut cx, &mut buf).is_pending());
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut buf),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(&buf.filled()[3..], b"private-ciphertext");
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut buf),
            Poll::Ready(Err(_))
        ));
        let mut empty = [];
        let mut zero = ReadBuf::new(&mut empty);
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut zero),
            Poll::Ready(Ok(()))
        ));
        assert!(!wire.snapshot().socket[0].eof);
        assert!(matches!(
            Pin::new(&mut io).poll_read(&mut cx, &mut buf),
            Poll::Ready(Ok(()))
        ));
        let bytes = b"private-ciphertext";
        assert!(Pin::new(&mut io).poll_write(&mut cx, bytes).is_pending());
        let bufs = [IoSlice::new(&bytes[..2]), IoSlice::new(&bytes[2..])];
        assert!(io.is_write_vectored());
        assert!(matches!(
            Pin::new(&mut io).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(3))
        ));
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, bytes),
            Poll::Ready(Err(_))
        ));
        assert!(Pin::new(&mut io).poll_flush(&mut cx).is_pending());
        assert!(matches!(
            Pin::new(&mut io).poll_flush(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(matches!(
            Pin::new(&mut io).poll_flush(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(io.inner_mut().read_calls[0], (pointer, 3, 61));
        assert_eq!(io.inner_mut().read_calls.len(), 5);
        assert_eq!(io.inner_mut().scalar_calls.len(), 2);
        assert_eq!(io.inner_mut().vector_calls.len(), 1);
        assert_eq!(io.inner_mut().scalar_calls[0].0, bytes.as_ptr() as usize);
        assert_eq!(io.inner_mut().vector_calls[0][0].0, bytes.as_ptr() as usize);
        assert_eq!(io.inner_mut().accepted, &bytes[..3]);
        assert_eq!(io.inner_mut().flushes, 3);
        assert_eq!(wake.0.load(Ordering::SeqCst), 3);
        let state = wire.snapshot();
        assert_eq!(state.socket[0].outcomes, [1, 3, 1]);
        assert_eq!(state.socket[0].bytes, bytes.len() as u64);
        assert!(state.socket[0].eof);
        assert_eq!(state.socket[1].outcomes, [1, 1, 1]);
        assert_eq!(state.socket[1].bytes, 3);
        assert_eq!(state.socket[1].requested, Some(bytes.len()));
        assert_eq!(state.socket[2].outcomes, [1, 1, 1]);
        assert!(state.socket[2].progress.is_some());
        assert!(state.socket.iter().all(|progress| progress.wakes == 1));
        assert!(
            state
                .directions
                .iter()
                .all(|direction| direction.bytes == 0)
        );
        let frozen = observer.capture();
        let sampled_at = Instant::now();
        let mut before = String::new();
        frozen.write_wire_core(&mut before, sampled_at);
        drop(io);
        assert!(wire.snapshot().socket_dropped);
        let mut after = String::new();
        frozen.clone().write_wire_core(&mut after, sampled_at);
        assert_eq!(before, after);
        assert!(!before.contains("private-"));
        assert!(before.contains("socket dropped=0"));
        // Without an observer, even the original waker identity is unchanged.
        let mut passive = SocketIo::new(MockIo {
            writes: [WriteStep::Pending, WriteStep::Accept(0)].into(),
            expected_waker: Some(waker.clone()),
            ..MockIo::default()
        });
        assert!(!passive.is_write_vectored());
        assert!(
            Pin::new(&mut passive)
                .poll_write(&mut cx, bytes)
                .is_pending()
        );
        assert!(matches!(
            Pin::new(&mut passive).poll_write(&mut cx, bytes),
            Poll::Ready(Ok(0))
        ));
        assert_eq!(passive.inner_mut().scalar_calls.len(), 2);
        assert_eq!(wake.0.load(Ordering::SeqCst), 4);
    }

    // Only this deterministic control installs socket gates. Production SocketIo
    // always delegates; the fixture's Pending is a labelled held inner boundary.
    struct HeldSocket {
        inner: tokio::net::TcpStream,
        gates: [Option<Arc<Gate>>; 3],
    }

    impl AsyncRead for HeldSocket {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if let Some(gate) = &this.gates[0]
                && gate.poll(cx).is_pending()
            {
                return Poll::Pending;
            }
            Pin::new(&mut this.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for HeldSocket {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if let Some(gate) = &this.gates[1]
                && gate.poll(cx).is_pending()
            {
                return Poll::Pending;
            }
            Pin::new(&mut this.inner).poll_write(cx, bytes)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if let Some(gate) = &this.gates[2]
                && gate.poll(cx).is_pending()
            {
                return Poll::Pending;
            }
            Pin::new(&mut this.inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    #[tokio::test]
    async fn established_mtls_original_socket_distinguishes_held_read_write_and_flush() {
        use ferrum_alloy::bench_diagnostics::{SocketIo, TlsIo};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let pki = crate::pki::Pki::generate().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            let observer = Observer::default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(address).await.unwrap();
            let original = client.local_addr().unwrap();
            let (server, peer) = listener.accept().await.unwrap();
            assert_eq!(peer, original);
            let acceptor = tokio_rustls::TlsAcceptor::from(pki.server(true).unwrap().rustls);
            let connector = tokio_rustls::TlsConnector::from(
                pki.client(crate::dims::Transport::H2Mtls).unwrap(),
            );
            let held = HeldSocket {
                inner: server,
                gates: std::array::from_fn(|_| None),
            };
            let (client, server) = tokio::join!(
                connector.connect(
                    rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                    SocketIo::new(client),
                ),
                acceptor.accept(SocketIo::new(held)),
            );
            let mut client = client.unwrap();
            let mut server = server.unwrap();
            assert!(!client.get_ref().1.is_handshaking());
            assert!(!server.get_ref().1.is_handshaking());
            assert_eq!(client.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            assert!(server.get_ref().1.peer_certificates().is_some());
            let client_wire = observer
                .wire_endpoint(INSTANCE, 2, 1, Some(original), Some(address), true)
                .unwrap();
            let server_wire = observer
                .server_wire(INSTANCE, Some(address), peer, true)
                .unwrap();
            client.get_mut().0.observe(Some(client_wire.clone()));
            server.get_mut().0.observe(Some(server_wire.clone()));
            let read = Arc::new(Gate::default());
            let write = Arc::new(Gate::default());
            let flush = Arc::new(Gate::default());
            // Start with unheld established TLS traffic, including frame marks.
            let mut client = WireIo::new(
                TlsIo::optional(client.into(), Some(client_wire.clone())),
                client_wire.clone(),
            );
            let mut server = WireIo::new(
                TlsIo::optional(server.into(), Some(server_wire.clone())),
                server_wire.clone(),
            );
            let mut request = H2_PREFACE.to_vec();
            request.extend(frame(1, 5, 1, &[]));
            client.write_all(&request).await.unwrap();
            client.flush().await.unwrap();
            let mut received = vec![0; request.len()];
            server.read_exact(&mut received).await.unwrap();
            assert_eq!(received, request);
            let response = frame(1, 4, 1, &[]);
            server.write_all(&response).await.unwrap();
            server.flush().await.unwrap();
            let mut received = vec![0; response.len()];
            client.read_exact(&mut received).await.unwrap();
            assert_eq!(received, response);
            let baseline = server_wire.snapshot();
            assert!(baseline.socket[0].bytes > 0 && baseline.socket[1].bytes > 0);
            assert!(baseline.socket[2].outcomes[1] > 0);
            // Arm beneath established TLS, without replacing the original socket.
            server.inner_mut().inner_mut().get_mut().0.inner_mut().gates =
                [Some(read.clone()), Some(write.clone()), Some(flush.clone())];
            let mut request = frame(3, 0, 1, &8_u32.to_be_bytes());
            request.extend(frame(1, 5, 3, &[]));
            client.write_all(&request).await.unwrap();
            client.flush().await.unwrap();
            let mut received = vec![0; request.len()];
            let mut reading = Box::pin(server.read_exact(&mut received));
            std::future::poll_fn(|cx| {
                assert!(reading.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(read.reached.load(Ordering::SeqCst));
            let held_read = server_wire.snapshot();
            assert_eq!(held_read.socket[0].last, Some(0));
            assert_eq!(held_read.socket[0].bytes, baseline.socket[0].bytes);
            assert_eq!(held_read.header_streams(Direction::Rx), [1]);
            let read_wakes = held_read.socket[0].wakes;
            read.release();
            reading.await.unwrap();
            assert_eq!(received, request);
            let released_read = server_wire.snapshot();
            assert!(released_read.socket[0].bytes > baseline.socket[0].bytes);
            assert!(released_read.socket[0].wakes > read_wakes);
            assert_eq!(released_read.header_streams(Direction::Rx), [1, 3]);
            assert!(released_read.streams[0].unwrap().first_cancel.is_some());
            let response = frame(1, 4, 3, &[]);
            server.write_all(&response).await.unwrap();
            let mut flushing = Box::pin(server.flush());
            std::future::poll_fn(|cx| {
                assert!(flushing.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(write.reached.load(Ordering::SeqCst));
            let held_write = server_wire.snapshot();
            assert_eq!(held_write.socket[1].last, Some(0));
            assert_eq!(held_write.socket[1].bytes, baseline.socket[1].bytes);
            assert!(held_write.tls_sample.unwrap().flags[1]);
            assert_eq!(held_write.header_streams(Direction::Tx), [1, 3]);
            assert_eq!(client_wire.snapshot().header_streams(Direction::Rx), [1]);
            // Immutable capture at the held ciphertext-write boundary, before
            // release/drop. Endpoint copies are sequential, not simultaneous.
            let frozen = observer.capture();
            let cloned = frozen.clone();
            let sampled_at = Instant::now();
            let mut before = String::new();
            frozen.write_wire(&mut before, sampled_at);
            let write_wakes = held_write.socket[1].wakes;
            write.release();
            std::future::poll_fn(|cx| {
                assert!(flushing.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(flush.reached.load(Ordering::SeqCst));
            let held_flush = server_wire.snapshot();
            assert!(held_flush.socket[1].bytes > baseline.socket[1].bytes);
            assert!(held_flush.socket[1].wakes > write_wakes);
            assert_eq!(held_flush.socket[2].last, Some(0));
            assert!(!held_flush.tls_sample.unwrap().flags[1]);
            client
                .read_exact(&mut received[..response.len()])
                .await
                .unwrap();
            assert_eq!(&received[..response.len()], response);
            assert_eq!(client_wire.snapshot().header_streams(Direction::Rx), [1, 3]);
            let flush_wakes = held_flush.socket[2].wakes;
            flush.release();
            flushing.await.unwrap();
            let released = server_wire.snapshot();
            assert_eq!(released.socket[2].last, Some(1));
            assert!(released.socket[2].wakes > flush_wakes);
            let original_socket = &server.inner_mut().inner_mut().get_mut().0.inner_mut().inner;
            assert_eq!(original_socket.local_addr().unwrap(), address);
            assert_eq!(original_socket.peer_addr().unwrap(), original);
            assert_eq!(client_wire.socket, Some(original));
            assert_eq!(server_wire.remote, Some(original));
            drop(server);
            drop(client);
            assert!(server_wire.snapshot().socket_dropped);
            assert!(client_wire.snapshot().socket_dropped);
            let mut after = String::new();
            cloned.write_wire(&mut after, sampled_at);
            assert_eq!(before, after);
            assert!(!before.contains("socket dropped=1"));
            assert!(before.len() < 48 * 1024);
            assert_eq!(observer.wires().len(), 1);
            assert_eq!(observer.server_wires().len(), 1);
        })
        .await
        .unwrap();
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
        assert_eq!(observation.snapshot().directions[0].errors, 1);
        assert_eq!(observation.snapshot().directions[0].outcomes[1], [1, 0, 1]);
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
        assert_eq!(observation.snapshot().directions[0].errors, 2);
        assert_eq!(observation.snapshot().directions[0].outcomes[2], [1, 0, 1]);
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
        assert_eq!(io.inner_mut().accepted, transcript);
        assert_eq!(io.inner_mut().scalar_calls.len(), 5);
        for (pointer, bytes) in &io.inner_mut().scalar_calls[..3] {
            assert_eq!(*pointer, transcript.as_ptr() as usize);
            assert_eq!(*bytes, transcript);
        }
        for call in &io.inner_mut().vector_calls {
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
        assert_eq!(io.inner_mut().read_calls, [(pointer, 3, 13); 3]);
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
        io.inner_mut().vectored = false;
        assert!(!io.is_write_vectored());
        let state = observation.snapshot();
        assert_eq!(state.directions[0].bytes, transcript.len() as u64);
        assert_eq!(state.directions[0].headers_seen, 2);
        assert_eq!(state.directions[0].frames_complete, 2);
        assert_eq!(state.directions[0].errors, 2);
        assert_eq!(state.directions[0].outcomes[1], [1, 3, 1]);
        assert_eq!(state.directions[0].outcomes[2], [1, 1, 1]);
        assert_eq!(state.directions[0].outcomes[3], [1, 1, 1]);
        assert_eq!(state.directions[0].outcomes[4], [1, 1, 1]);
        assert_eq!(state.directions[1].errors, 1);
        assert_eq!(state.directions[1].outcomes[0], [1, 1, 1]);
        assert!(state.streams[0].unwrap().reset.is_some());
    }

    #[test]
    fn server_preface_and_unaccepted_encoded_write_control_are_distinct() {
        let observer = Observer::default();
        let remote = "127.0.0.1:12345".parse().unwrap();
        let wire = observer.server_wire(INSTANCE, None, remote, false).unwrap();
        let mut request = H2_PREFACE.to_vec();
        request.extend(frame(1, 5, 17, b"private-request-hpack"));
        request.extend(frame(3, 0, 17, &8_u32.to_be_bytes()));
        let mut response = frame(1, 4, 17, b"private-response-hpack");
        response.extend(frame(0, 0, 17, b"private-data"));
        let inner = MockIo {
            reads: request
                .iter()
                .map(|byte| ReadStep::Bytes(vec![*byte]))
                .collect(),
            writes: [
                WriteStep::Pending,
                WriteStep::Error,
                WriteStep::Accept(8),
                WriteStep::Accept(response.len() - 8),
            ]
            .into(),
            ..MockIo::default()
        };
        let mut io = WireIo::new(inner, Arc::clone(&wire));
        let waker = Waker::from(Arc::new(CountWake::default()));
        let mut cx = Context::from_waker(&waker);
        for _ in 0..request.len() {
            let mut storage = [0; 1];
            let mut buf = ReadBuf::new(&mut storage);
            assert!(matches!(
                Pin::new(&mut io).poll_read(&mut cx, &mut buf),
                Poll::Ready(Ok(()))
            ));
        }
        assert_eq!(wire.snapshot().header_streams(Direction::Rx), [17]);
        assert_eq!(wire.snapshot().streams[0].unwrap().reset_reason, Some(8));
        assert!(
            Pin::new(&mut io)
                .poll_write(&mut cx, &response)
                .is_pending()
        );
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &response),
            Poll::Ready(Err(_))
        ));
        let state = wire.snapshot();
        assert_eq!(state.directions[0].bytes, 0);
        assert_eq!(state.directions[0].errors, 1);
        assert!(state.header_streams(Direction::Tx).is_empty());
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &response),
            Poll::Ready(Ok(8))
        ));
        assert!(wire.snapshot().header_streams(Direction::Tx).is_empty());
        let frozen = observer.capture();
        let now = Instant::now();
        let mut before = String::new();
        frozen.write_wire(&mut before, now);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &response[8..]),
            Poll::Ready(Ok(_))
        ));
        assert_eq!(wire.snapshot().header_streams(Direction::Tx), [17]);
        assert_eq!(io.inner_mut().accepted, response);
        assert_eq!(wire.snapshot().streams[0].unwrap().late.complete, 1);
        assert!(!wire.snapshot().directions[1].preface_invalid);
        let mut after = String::new();
        frozen.write_wire(&mut after, now);
        assert_eq!(before, after);
        assert!(!before.contains("private-"));
        assert_eq!(observer.server_wires().len(), 1);
        observer.server_wire(INSTANCE, None, remote, true).unwrap();
        assert!(
            observer
                .server_wire(INSTANCE, None, remote, false)
                .is_none()
        );
        assert!(observer.wires().is_empty());
        let mut bounded = String::new();
        observer.capture().write_wire(&mut bounded, now);
        assert!(bounded.contains("connections_omitted=1 boundary=server-plaintext-I/O"));
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
        assert_eq!(io.inner_mut().accepted, accepted);
        assert_eq!(io.inner_mut().vector_calls.len(), 4);
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
        io.inner_mut().reads = rx[..split]
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
        assert!(text.contains("wf b"));
        assert!(text.contains("ws Rx 4 ffff "));
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
        assert!(!frozen.contains(" goaway="));
        assert!(after.contains(" goaway=0:0"));
    }

    #[test]
    fn snapshot_holders_have_bounded_value_footprints() {
        // Full-capacity arrays stay on the heap during construction and deep
        // cloning. These hosted bounds cover the remaining by-value records.
        assert!(std::mem::size_of::<WireDirection>() <= 1024);
        assert!(std::mem::size_of::<WireState>() <= 4096);
        assert!(std::mem::size_of::<WireCapture>() <= 256);
        assert!(std::mem::size_of::<ObserverCapture>() <= 1024);
        assert!(std::mem::size_of::<TaskCapture>() <= 256);
        assert!(std::mem::size_of::<RequestCapture>() <= 512);
        assert!(std::mem::size_of::<Option<WireStream>>() <= 1024);
        assert!(std::mem::size_of::<Option<WireEvent>>() <= 128);
        assert!(std::mem::size_of::<SocketProgress>() <= 256);
    }

    #[test]
    fn compact_wire_core_preserves_counter_order_distinct_marks_and_unknown_fields() {
        let observer = Observer::default();
        let now = Instant::now();
        let mut empty = String::new();
        observer.capture().write_wire_core(&mut empty, now);
        assert!(empty.len() < 512);
        assert!(!empty.contains("compact_hex"));
        observer.wire(INSTANCE, 0, 1, None).unwrap();
        let mark = |sequence| WireMark { sequence, at: now };
        let mut capture = observer.capture();
        let wire = capture.connections[0].as_mut().unwrap();
        wire.state.streams[0] = Some(WireStream {
            id: 13,
            directions: [
                StreamDirection {
                    first_header: Some(mark(1)),
                    first_complete: Some(mark(2)),
                    first_block: Some(mark(3)),
                    ..StreamDirection::default()
                },
                StreamDirection {
                    first_header: Some(mark(4)),
                    first_complete: Some(mark(5)),
                    first_block: Some(mark(6)),
                    ..StreamDirection::default()
                },
            ],
            reset: Some(mark(8)),
            reset_reason: Some(8),
            first_cancel: Some(mark(7)),
            late: DataProgress::default(),
        });
        let tx = &mut wire.state.directions[0];
        tx.bytes = 1;
        tx.headers_seen = 2;
        tx.frames_complete = 3;
        tx.headers_complete = 4;
        tx.blocks_complete = 5;
        tx.data_complete = 6;
        tx.invalid_lengths = 7;
        tx.invalid_streams = 8;
        tx.settings_omitted = 9;
        tx.errors = 10;
        let rx = &mut wire.state.directions[1];
        rx.header_bytes = 9;
        rx.remaining = Some(0);
        rx.last_header = Some(FramePoint {
            mark: mark(9),
            header: FrameHeader::default(),
        });
        rx.eof_mark = Some(mark(10));
        let mut text = String::new();
        capture.write_wire_core(&mut text, now);
        let tx = text
            .lines()
            .find(|line| line.starts_with("wire Tx counts_hex="))
            .unwrap();
        assert!(tx.contains("counts_hex=1/2/3/4/5/6/7/8/9/a "));
        assert!(tx.ends_with("remaining_hex=- partial_hex=-/-/-/-"));
        let rx = text
            .lines()
            .find(|line| line.starts_with("wire Rx counts_hex="))
            .unwrap();
        assert!(rx.ends_with("remaining_hex=0 partial_hex=0/0/0/0"));
        assert!(text.contains("wf d 1:0/2:0/3:0 4:0/5:0/6:0 7:0 8:0:8\n"));
        assert!(text.contains("wire Rx last_header hex=9:0 0/0/0/0 0/1\n"));
        assert!(text.contains("wire Rx eof_mark_hex=a:0\n"));
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
            for progress in state.socket.iter_mut() {
                *progress = SocketProgress {
                    outcomes: [u64::MAX; 3],
                    bytes: u64::MAX,
                    requested: Some(usize::MAX),
                    in_poll: true,
                    last: Some(2),
                    eof: true,
                    wakes: u64::MAX,
                    last_poll: Some(mark),
                    progress: Some(mark),
                    last_wake: Some(mark),
                };
            }
            state.tls_sample = Some(TlsSample {
                flags: [true; 3],
                mark,
            });
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
                direction.outcomes = [[u64::MAX; 3]; 5];
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
            state.events.fill(Some(WireEvent {
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
            }));
        }
        for (index, wire) in left.wires().iter().enumerate() {
            let server = left
                .server_wire(
                    INSTANCE,
                    None,
                    SocketAddr::from(([127, 0, 0, 1], 12345 + index as u16)),
                    false,
                )
                .unwrap();
            *server.state.lock().unwrap() = wire.snapshot();
        }
        let mut compact = String::new();
        left.capture().write_wire_core(&mut compact, mark.at);
        // Reserve maximum-width ages for first HEADERS/CANCEL/latest reset,
        // completed SETTINGS and every unowned control, plus point/EOF ages.
        // Shorter wf/wc/ws prefixes retain every original field and save
        // 3,760 bytes. Added socket rows <=12*174, drop rows <=4*17 and TLS
        // rows <=4*36 at zero ages, plus schemas, stay below the unchanged
        // 35,200-byte assertion. All 40 new marks reserve 15 extra age digits.
        // Combined maximum age/identity allowance is 13,864 bytes:
        // 35,200 + 13,864 = 49,064 < the unchanged 49,152-byte wire reserve.
        let widest_ages = WIRE_CONNECTIONS * ((WIRE_STREAMS * 8 + WIRE_EVENTS + 12) * 15 + 240);
        let widest_server_ages = SERVER_CONNECTIONS * (12 * 15 + 240);
        let widest_identity = (WIRE_CONNECTIONS + SERVER_CONNECTIONS) * 256;
        let socket_ages = (WIRE_CONNECTIONS + SERVER_CONNECTIONS) * (3 * 3 + 1) * 15;
        assert!(
            compact.len() + widest_ages + widest_server_ages + widest_identity + socket_ages
                < 48 * 1024
        );
        assert!(compact.len() < 35_200);
        assert_eq!(compact.matches("wf ").count(), 72);
        assert_eq!(compact.matches("wc ").count(), 128);
        assert_eq!(compact.matches("ws ").count(), 48);
        // Every mandatory mark and numerical control/setting/point value
        // survives compaction; these counts include both server core records.
        assert_eq!(compact.matches("ffffffffffffffff:0").count(), 776 + 40);
        let original_marks: usize = compact
            .lines()
            .filter(|line| !line.starts_with("socket ") && !line.starts_with("tls demand "))
            .map(|line| line.matches("ffffffffffffffff:0").count())
            .sum();
        assert_eq!(original_marks, 776);
        assert_eq!(
            compact
                .lines()
                .filter(|line| line.starts_with("socket "))
                .count(),
            17,
        );
        assert_eq!(
            compact
                .matches("tls demand 1/1/1 ffffffffffffffff:0\n")
                .count(),
            4,
        );
        assert_eq!(compact.matches(" goaway=ffffffff:ffffffff\n").count(), 128);
        assert_eq!(
            compact.matches(" ffffffff ffffffffffffffff:0\n").count(),
            48,
        );
        assert_eq!(
            compact
                .matches("hex=ffffffffffffffff:0 ff/ff/ffffffff/ffffffff 0/0\n")
                .count(),
            16,
        );
        assert_eq!(
            compact.matches("eof_mark_hex=ffffffffffffffff:0\n").count(),
            8,
        );
        assert_eq!(
            compact
                .matches("endpoint=server-accepted-connection")
                .count(),
            2,
        );
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
}
