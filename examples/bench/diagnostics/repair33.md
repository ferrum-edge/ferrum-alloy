# Bounded typed I/O evidence repair33 at 70b3837

PR [#147](https://github.com/ferrum-edge/ferrum-alloy/pull/147) references
[#142](https://github.com/ferrum-edge/ferrum-alloy/issues/142) only. The initiating
cause remains **UNKNOWN**. This delivery adds observation and corrects the failure
capture boundary; it does not claim a lifecycle repair, protocol pass, ordinary
qualification or production fix.

## Actual hosted evidence

The inspected clean local and PR head was
`70b383739a75643d704767f92629b3b1f8eef71f`, on
`root/20261005/alloy142-server-evidence`, with merge base
`b998587fee6e28297e4ef32cd34b89b9a76ad76f`. Protocol run
[37343970312](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37343970312)
failed the strict alive dispatcher control in five OS/feature groups. Each failed
control group reports 85 passed, one failed and two filtered. Windows' all-feature
alive control passed; that does not explain the other five failures.

All five complete `controlled-before-zero-error-check` captures, from their
instance/provenance records through their loss footers, were inspected in the
supplied full primary REST job logs. No older head or failed-step excerpt is used
as the basis for this change.

| OS / feature | Job | Capture lines in full log | Owner 2 original socket / paired server socket | Error evidence at capture |
| --- | --- | --- | --- | --- |
| Windows / default | [111877783977](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37343970312/job/111877783977) | 2643–3325 | 59450 / 59448 | All socket and plaintext errors zero; paired server write in poll, socket not dropped; subsequent live assertion fails |
| macOS / all | [111877784390](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37343970312/job/111877784390) | 1309–1995 | 49221 / 49219 | Paired server socket write error and plaintext flush error; Tx write-error counter zero; server received NO_ERROR GOAWAY and EOF |
| macOS / default | same job | 3322–4006 | 49433 / 49431 | Paired server socket write error and plaintext vectored-write error; final client CANCEL/GOAWAY not decoded by server at capture |
| Ubuntu / all | [111877784427](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37343970312/job/111877784427) | 3749–4438 | 38490 / 45245 | Paired server socket write error and plaintext vectored-write error; final client CANCEL/GOAWAY not decoded by server at capture |
| Ubuntu / default | same job | 5767–6455 | 42780 / 34725 | Paired server socket write error and plaintext vectored-write error; final client CANCEL/GOAWAY not decoded by server at capture |

The socket ports are actual loopback endpoint identities in these captures,
not stream IDs or inferred owner numbers. The paired accepted-server owner is
socket ordinal 1; the client owner is 2. Every capture has readiness and joins
4/4, successful reuse on all four workers, and zero worker `errors`,
`error_events` and `wire_events`. The selected client dispatcher and original wire
child completed normally, with client Tx NO_ERROR GOAWAY. None supplies a typed
I/O kind for the paired-server error. The Windows capture predates the failing
live check and therefore does not contain that check's error evidence.

Supplied full-log SHA-256 values, computed from actual bytes:

```text
fa76b28dc1c6750503a2a443893b404b2558486fa7b94a156e15232e2dec22ee  alloy147-70b-111877783977-full1659.log
2e39899d093f6cb659605137f53cf1c5240b27777a620391f09b94fbe45bf3c3  alloy147-70b-111877784390-full1659.log
141b67fa098584186eb06c9d4003be0e23d686d915ed54a3e091ff5754a67d25  alloy147-70b-111877784427-full1659.log
```

## Source trace and causal limits

`WireObservation::outcome` increments Tx `errors` only for actual scalar and
vectored write failures, and Rx `errors` for read failures. Flush and shutdown
failures remain in their separate outcome counters. `socket_outcome` counts
original-socket read/write/flush outcomes; scalar and vectored socket writes
share one counter. `protocol_error_free` strictly checks every directional error
counter, every operation's error outcomes, and every socket error outcome.
At the old head, all those callbacks receive only `Outcome::Error`, discarding
`io::Error::kind()`. Thus the counter of one cannot distinguish BrokenPipe,
ConnectionReset, a different kind, or its initiating cause.

`protocol_pair` obtains owner 2's generation 1 local address from the worker
record, finds the original client observation by that address and the accepted
server observation by its remote address, then asserts both reciprocal socket
identities. The client owner and server accept ordinal are separate namespaces.
The TLS fixture wraps the original accepted socket in `SocketIo`, establishes
mTLS on that same socket, attaches its observer after establishment, and wraps
that stream in `TlsIo` and `PlaintextIo` before Hyper serving. Its `JoinSet` owns
those connection tasks. The server connection's returned result is discarded,
while its typed protocol and I/O observers retain numerical state.

The alive fixture first completes health, joins all workers and performs reuse.
`protocol_origin_pending` then proves both retained actual senders are open,
the public dispatcher and original wire child are Pending and undropped, and
runtime destruction has not entered. It takes owner 2's retained sender from
its slot with the mutex released before dropping it. It next takes the same
public dispatcher, awaits its result, waits for the original wire child's drop,
and asserts Ready, terminal OK and Tx NO_ERROR GOAWAY before the strict error
check. The other retained sender and dispatcher remain in their existing slots.
The dispatcher guard and poison-safe cleanup paths are unchanged.

The pinned published [Hyper 1.11.1](https://static.crates.io/crates/hyper/hyper-1.11.1.crate)
`src/proto/h2/client.rs` constructs the original
`ConnTask` at handshake (lines 164–205), maintains its drop channel, and returns
public `Dispatched::Shutdown` when the request sender channel closes (lines
778–782). `ConnTask::poll` handles that drop notification while continuing to
poll the same connection (lines 341–362). The retained h2 connection flushes and
shuts down its codec in `State::Closing`. The pinned published
[tokio-rustls 0.26.6](https://static.crates.io/crates/tokio-rustls/tokio-rustls-0.26.6.crate)
client shutdown (`src/client.rs:512–530`) sends close_notify; its common stream
shutdown (`src/common/mod.rs:362–376`) drains encrypted writes and delegates
original socket shutdown. Its existing NotConnected handling is unchanged. These
source paths establish that releasing the sender permits normal client shutdown.
They do not prove why the still-owned server returned a write/flush error.
The server runtime is cleaned up only after these alive assertions, and the
original client runtime, retained slots and pending-dispatcher cleanup retain
their existing custody. No lifetime change is justified by the supplied data.

A captured client GOAWAY plus a server error is insufficient to label the error
benign or caused by fixture teardown. A release marker sampled after an I/O poll
cannot order that poll's entry or establish peer/kernel causality. The exact
kind and initiating cause of all five failed assertions remain **UNKNOWN**.
The separate Alloy150 `02bb` Ubuntu default health-matrix timeout in run
37339091922, actual log line 577, also remains UNKNOWN; no common cause is inferred.

## Literal observation change

Only the isolated `observer.patch` changes the seam and fixture. The ordinary
sources, production benchmark, h2 fork/error policy, TLS policy and typed GOAWAY
NO_ERROR mask are unchanged.

`IoObserver::io_error` receives only boundary, operation and `io::ErrorKind` after
one actual inner delegation and before its existing outcome callback. No error
text, Debug/Display formatting, source chain, raw OS code or encrypted bytes are
passed or stored. Every poll result, byte prefix, waker, socket shutdown behavior
and existing error counter is preserved. Original-socket shutdown observation
remains UNKNOWN, as before.

One `u32` per endpoint stores the first plaintext error and first original-socket
error independently, plus the controlled sender-release markers. Later errors
continue to increment strict counters but do not overwrite either first sample.
There are no new timestamps, history/event slots, participants or socket records.
The sample is copied and cloned under the existing endpoint mutex with its exact
instance, owner, generation, socket and reciprocal peer identity. Endpoint copies
and release marking across endpoints are sequential, not atomic.

The alive fixture marks release entry and return on owner 2's exact original
client/server pair, around the existing `release_sender(2)` call. It moves no
sender, dispatcher, socket, TLS stream or runtime to a different custodian.
`protocol_zero_errors` still runs the same strict live assertions; on failure it
now captures cumulative endpoint state after the check fails, while outer fixture
custody remains, before resuming the original panic. Its truthful boundary is
`controlled-after-zero-error-check-before-fixture-unwind`. Naturally completed
connection futures may already have destroyed their own I/O, as the drop bits
record; this capture does not claim to precede all connection teardown.

The compact line is `socket drop=bit/hex`, where `bit` retains the original
wrapper-drop bit and `hex` has seven hexadecimal digits:

| Packed bits | Meaning |
| --- | --- |
| 0–11 | First plaintext error sample; zero means absent |
| 12–23 | First original-socket error sample; zero means absent |
| 24 | Controlled sender-release entry marked on this endpoint |
| 25 | Controlled sender-release return marked on this endpoint |
| 26–31 | Reserved zero |

Within each 12-bit first sample:

| Bits | Meaning |
| --- | --- |
| 0–3 | Kind: 1 BrokenPipe, 2 ConnectionReset, 3 ConnectionAborted, 4 NotConnected, 5 UnexpectedEof, 6 TimedOut, 7 WouldBlock, 8 Interrupted, 9 WriteZero, a InvalidData, b InvalidInput, c PermissionDenied, d Other, f UNKNOWN fallback; zero absent, e unused |
| 4–6 | Actual operation: 0 read, 1 scalar write, 2 vectored write, 3 flush, 4 shutdown |
| 7–8 | This endpoint's release entry/return bits sampled by the error callback |
| 9 | This endpoint's existing protocol runtime-entry bit sampled by the callback |
| 10 | This endpoint's plaintext Rx or original-socket read EOF already observed |
| 11 | This endpoint's original socket wrapper drop already observed |

The flags describe callback-time observations after delegation, never inferred
cause or benignness. A zero first sample does not prove no error outside the
observed boundary or later in time. Kind `f` deliberately leaves unlisted kinds
UNKNOWN rather than inventing a platform-specific interpretation. `d` is the
actual `ErrorKind::Other`, which also does not identify the error's cause.

The drop-label compaction adds a net five bytes per endpoint, twenty for all four.
The unchanged 256-byte protocol record, 428-byte `p2` line, 142-byte schema and
1854 <= 1884 protocol rendering assertion remain intact. Existing strict
`compact.len() < 35_200`, total wire < 49,152 bytes, footprint limits, all 816 marks,
72 first-stream rows, 128 mandatory controls and 48 completed settings remain.
The saturated control now fills the new 26-bit field and asserts all four copies
render completely. Actual fresh-head footprint and render bounds require hosted
execution; no local run is claimed.

Hosted controls also exercise first-kind retention despite subsequent different
operation/kind, independent socket/plaintext boundaries, exact client identity,
other-endpoint isolation, release/runtime/EOF/drop sampling, frozen clone
immutability, UNKNOWN fallback and unchanged strict rejection of write/read/flush
errors. Existing wrapper controls now check their numerical first kinds while
preserving exact buffers, pointers, wakers, short/vector writes, returned errors,
raw-text omission and all existing negative controls.

## Actual retained-byte integrity and validation

Literal edits were made to the corresponding retained byte postimages; no file
was regenerated or replaced by an assembler. Declared patch coordinates/counts
were edited as literal metadata. Only three actual-byte SHA-256 entries change
in `alloy-patched.sha256`; the other eleven retained files keep their prior hashes.
Standard `shasum -a 256 -c` checks cover all fourteen before and after editing.

Alloy retained root: `target/repair23-evidence/alloy`:

```text
03a3fed3f41fe3843ed61552e42648ed57c37032f89801c21940970d01ddc72e  crates/ferrum-alloy/src/bench_diagnostics.rs
31cc8190037ce8c46a0281e2c13609fda780603baece499d41e208bd82d1fc01  crates/ferrum-alloy/src/server.rs
0c584ebd956ac8e6449f6ddd5d1f651fdcf12c9bc247191a39f8a455ab82434f  examples/bench/src/client.rs
f9b24528ff7922f23d66e2f5e16d056c53447884b71dc153c96e84bd94b116f1  examples/bench/src/server.rs
b24ab442a3877d25ca5f7ecdf79823e81cec607720df4e58a41319722bc1ff60  examples/bench/src/health.rs
13f1c5d5691177f03ac7fc010835c46d02c2dd755bd78f3549210cd1c35d54db  examples/bench/src/run.rs
```

h2 retained root: `target/repair23-evidence/h2` (all unchanged):

```text
dae9e111926ee8984bc101e23f8736c1ff27d1c604c06b444fec90d311a87324  src/lib.rs
30e8f27a1e4e98770447d10537e2e6425d6680a3bdaa6a6d23737b4b05c4028f  src/alloy_diagnostics.rs
a7afff46c20d6f809636ca67d16694b81f39d544882fe549bec0366d997bdeb1  src/proto/connection.rs
0588cbb12e6415829c06a05952314327d4fe2e0973934a12a95dca975bc5dcc0  src/proto/streams/streams.rs
55f72e4dedcd785acc6d63d686b634b034a5acfd753b747eaef5cd6223535764  src/proto/streams/send.rs
a47debc97ba5c0186384c67b37f1c7ab565f963a79a9978eb18bd101a2fa5f5a  src/proto/streams/prioritize.rs
33a920942741aae5d0e746f64fde6acfbeba037c9bb0da436063c728193b0a8f  Cargo.toml
b21623012e6c453d944b0342c515b631cfcbf30704c2621b291526b69c10724d  LICENSE
```

Only static source/log inspection, literal text editing, ordinary Git/GitHub reads,
actual-byte hashes and `git diff --check` were used locally. No repository code,
preparation/patch/lock/hash-generation/validation algorithm, formatter, compiler,
linter, test, benchmark or server ran. No nested worker or review was requested.

The pushed head remains unqualified. Root owns fresh hosted formatting,
compilation, strict lint, privacy/footprint controls, both feature selections on
all three OSes, full new logic review, immutable independent review, metadata and
any merge decision. The no-error assertion may still fail: new typed captures
must be interpreted before a causal repair can be justified. The observation
adds callback locking on error paths and a small per-endpoint field; its timing
perturbation and exact layout are pending hosted evidence. No rerun is used as
causal proof and no issue closure or PR metadata change is made.
