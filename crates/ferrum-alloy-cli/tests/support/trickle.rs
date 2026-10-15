//! Shared proof that a trickled response body cannot extend the CLI's
//! whole-operation deadline, for every HTTP retrieval path.
//!
//! The deadline is enforced inside a separate CLI process over real sockets,
//! so a paused tokio clock cannot drive it. The timing proof is instead taken
//! at the server, where the trickle is either still being accepted or not.
//! That connection lifetime excludes process teardown and the test thread's
//! own scheduling, which together once added more than 2.5 s on a hosted
//! Windows runner after the client had already given up.

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `--timeout-ms` for the proof. The CLI applies it as the whole-request
/// deadline and, in reqwest's blocking client, as each read's wait, so the
/// trickle keeps every gap well below it.
pub(crate) const TRICKLE_TIMEOUT: Duration = Duration::from_millis(600);
/// Gap between trickled body bytes, far below the per-read wait.
pub(crate) const TRICKLE_INTERVAL: Duration = Duration::from_millis(100);
/// Bytes trickled before the fixture sends the rest of the body. A client
/// without a whole-operation deadline reads for at least
/// `TRICKLE_WRITES * TRICKLE_INTERVAL` (8 s) and then succeeds.
pub(crate) const TRICKLE_WRITES: usize = 80;
/// How long the client may keep the connection after the response head:
/// the 600 ms deadline (which already started before connect) plus 1.4 s for
/// timer resolution (about 15.6 ms on Windows), the fixture's 10 ms
/// disconnect polling, and scheduling of the CLI on a shared hosted runner.
/// It is a quarter of the time a client without the deadline would take.
pub(crate) const TRICKLE_CONNECTION_BOUND: Duration = Duration::from_secs(2);
/// An attempt whose fixture was itself descheduled for a whole per-read wait
/// before the bound was reached proves nothing either way (that wait, not the
/// deadline, may have ended it), so it is repeated, up to this many attempts.
const TRICKLE_ATTEMPTS: usize = 3;

/// What the fixture's own timeline says about one attempt.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// Every gap stayed below the per-read wait and every event, including
    /// the observed disconnect, came before the connection bound.
    Live,
    /// The fixture stalled for this long before the bound was reached.
    Stalled(Duration),
    /// The fixture was continuously live while the client still accepted
    /// body bytes this long after the head: a proven deadline violation,
    /// whatever happened afterwards.
    HeldPastBound(Duration),
}

/// Classifies the fixture timeline: `events` are the trickled writes followed
/// by the observed disconnect, all after `started` (the end of the head).
pub(crate) fn liveness(started: Instant, events: &[Instant]) -> Liveness {
    let mut previous = started;
    for &event in events {
        let gap = event.duration_since(previous);
        if gap >= TRICKLE_TIMEOUT {
            return Liveness::Stalled(gap);
        }
        let held = event.duration_since(started);
        if held >= TRICKLE_CONNECTION_BOUND {
            return Liveness::HeldPastBound(held);
        }
        previous = event;
    }
    Liveness::Live
}

/// The client's request and the CLI's output from the proving attempt.
pub(crate) struct TrickleProof {
    /// The fixture's URL, which the CLI's error must not disclose.
    pub(crate) url: String,
    pub(crate) request: String,
    pub(crate) output: Output,
}

/// Runs the CLI against a fixture that sends a `200` head and then trickles
/// `body`'s whitespace prefix, and asserts that the client gave up within the
/// deadline although no single read ever waited long. `run` receives the
/// fixture URL and the `--timeout-ms` value. The caller checks the request
/// and the CLI's error output.
pub(crate) fn prove_trickle_deadline(
    body: &str,
    run: impl Fn(&str, &str) -> Output,
) -> TrickleProof {
    let mut stalls = Vec::new();
    for _ in 0..TRICKLE_ATTEMPTS {
        match trickle_attempt(body, &run) {
            Ok(proof) => return proof,
            Err(stall) => stalls.push(stall),
        }
    }
    panic!("every trickle fixture stalled, so nothing was proven: {stalls:#?}");
}

/// How the fixture saw the client give up on the response.
#[derive(Debug)]
enum Disconnect {
    Closed,
    Failed(std::io::Error, &'static str),
}

struct TrickleFixture {
    request: String,
    /// When the complete response head had been written.
    started: Instant,
    write_times: Vec<Instant>,
    /// The client's disconnect and when the fixture observed it.
    disconnect: Option<(Disconnect, Instant)>,
}

/// One attempt. Returns `Err` only when the fixture itself stalled; every
/// client-attributable failure panics.
fn trickle_attempt(
    body: &str,
    run: &impl Fn(&str, &str) -> Output,
) -> Result<TrickleProof, String> {
    let body = body.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&cancelled);
    let server = std::thread::spawn(move || {
        let result = (|| -> std::io::Result<_> {
            let deadline = Instant::now() + Duration::from_secs(20);
            let (mut stream, _) = trickle_io(&stop, deadline, || listener.accept())?;
            stream.set_nonblocking(true)?;
            stream.set_nodelay(true)?;
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                let read = trickle_io(&stop, deadline, || stream.read(&mut byte))?;
                if read == 0 {
                    return Err(std::io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "trickling request ended before its headers",
                    ));
                }
                request.push(byte[0]);
                if request.len() >= 32 * 1024 {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "trickling request headers exceeded the fixture limit",
                    ));
                }
            }
            let request = String::from_utf8(request)
                .map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error))?;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                body.len() + TRICKLE_WRITES,
            );
            trickle_write(&mut stream, head.as_bytes(), &stop, deadline)?;
            let started = Instant::now();
            let mut write_times = Vec::new();
            let mut disconnect = None;
            for _ in 0..TRICKLE_WRITES {
                if let Err(error) = trickle_write(&mut stream, b" ", &stop, deadline) {
                    disconnect = Some((Disconnect::Failed(error, "trickle"), Instant::now()));
                    break;
                }
                let written_at = Instant::now();
                write_times.push(written_at);
                let next = written_at + TRICKLE_INTERVAL;
                if let Some(seen) = watch_for_disconnect(&mut stream, next, &stop, deadline)? {
                    disconnect = Some((seen, Instant::now()));
                    break;
                }
            }
            if disconnect.is_none() {
                disconnect = trickle_write(&mut stream, body.as_bytes(), &stop, deadline)
                    .err()
                    .map(|error| (Disconnect::Failed(error, "final body"), Instant::now()));
            }
            Ok(TrickleFixture {
                request,
                started,
                write_times,
                disconnect,
            })
        })();
        // The socket has been dropped before completion is recorded, including
        // every I/O error path. A panic closes the channel and is reported at join.
        let _ = completed_tx.send(());
        result
    });
    // Keep ownership of the fixture outside all fallible client work so even
    // a helper panic or a channel failure cannot bypass cancellation/join.
    let proof = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let output = run(&url, &TRICKLE_TIMEOUT.as_millis().to_string());
        // Measure before cleanup; joining must not inflate the CLI return time.
        let returned_at = Instant::now();
        // The CLI's exit closes its socket, so a live fixture finishes at once.
        let completed = completed_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        (output, returned_at, completed)
    }));
    cancelled.store(true, Ordering::Relaxed);
    let server_result = match server.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    let (output, returned_at, completed) = match proof {
        Ok(proof) => proof,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    if !completed {
        return Err(format!(
            "fixture did not finish within one second of CLI return: {:?}",
            server_result.err()
        ));
    }
    let fixture = server_result.unwrap();
    let writes = fixture.write_times.len();
    let returned = returned_at.duration_since(fixture.started);
    let events: Vec<_> = fixture
        .write_times
        .iter()
        .copied()
        .chain(fixture.disconnect.iter().map(|(_, at)| *at))
        .collect();
    let timing = format!(
        "{writes} writes, disconnect {:?} after the head, CLI returned after {returned:?}",
        fixture
            .disconnect
            .as_ref()
            .map(|(_, at)| at.duration_since(fixture.started)),
    );
    // A violation while the fixture was live fails even if it stalled later.
    match liveness(fixture.started, &events) {
        Liveness::Live => {}
        Liveness::Stalled(gap) => {
            return Err(format!("fixture stalled for {gap:?}: {timing}"));
        }
        Liveness::HeldPastBound(held) => {
            panic!("client kept accepting the trickle {held:?} after the head: {timing}");
        }
    }
    // Live implies the disconnect, if any, came within the connection bound.
    let (disconnect, _) = fixture
        .disconnect
        .unwrap_or_else(|| panic!("client read the complete trickling response: {timing}"));
    if let Disconnect::Failed(error, operation) = &disconnect {
        assert_client_disconnect(error, operation);
    }
    // The request deadline starts before connect/send, so time spent reaching
    // the fixture has already consumed it and there is no minimum body-read
    // time. One accepted byte shows the read was under way while trickling.
    assert!(
        writes >= 1,
        "client gave up before any trickled byte: {timing}"
    );
    assert!(
        writes < TRICKLE_WRITES,
        "client outlasted the trickle: {timing}"
    );
    // Process exit is outside the deadline, but the CLI must still return
    // before a client without it could have finished reading.
    let trickle = TRICKLE_INTERVAL * u32::try_from(TRICKLE_WRITES).unwrap();
    assert!(
        returned < trickle,
        "CLI did not return before the trickle could finish: {timing}"
    );
    Ok(TrickleProof {
        url,
        request: fixture.request,
        output,
    })
}

/// Accepts the connection-ending errors a client's close produces on each
/// supported OS (Windows reports WSAECONNABORTED/WSAECONNRESET).
pub(crate) fn assert_client_disconnect(error: &std::io::Error, operation: &str) {
    assert!(
        matches!(
            error.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
        ) || matches!(error.raw_os_error(), Some(10053 | 10054)),
        "unexpected {operation} error: kind={:?}, raw_os_error={:?}",
        error.kind(),
        error.raw_os_error()
    );
}

/// Retry nonblocking fixture I/O within one absolute lifetime, checking
/// cancellation even when the peer makes progress or interrupts a syscall.
fn trickle_io<T>(
    cancelled: &AtomicBool,
    deadline: Instant,
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                ErrorKind::Interrupted,
                "trickle fixture cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "trickle fixture exceeded its lifetime",
            ));
        }
        match operation() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn trickle_write(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Instant,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = trickle_io(cancelled, deadline, || stream.write(bytes))?;
        if written == 0 {
            return Err(std::io::Error::new(
                ErrorKind::WriteZero,
                "trickle fixture could not write the response",
            ));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

/// Polls the nonblocking socket until `until`, so a client close is
/// timestamped within about 10 ms instead of at the next trickled write.
fn watch_for_disconnect(
    stream: &mut TcpStream,
    until: Instant,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> std::io::Result<Option<Disconnect>> {
    let mut byte = [0; 1];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                ErrorKind::Interrupted,
                "trickle fixture cancelled",
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "trickle fixture exceeded its lifetime",
            ));
        }
        if now >= until {
            return Ok(None);
        }
        match stream.read(&mut byte) {
            Ok(0) => return Ok(Some(Disconnect::Closed)),
            Ok(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "client sent bytes after its request",
                ));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10).min(until - now));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Ok(Some(Disconnect::Failed(error, "read"))),
        }
    }
}
