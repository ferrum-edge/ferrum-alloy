# Isolated protocol observer experiment for #142

This temporary observer and labelled controls are not a production repair or ordinary qualification.
PR #147 still only references #142; its initiating cause remains unknown. Genuine
`1a399a4` diagnostic run 37276391581/job 111654235607/artifact 11330048749 remains
failed evidence. Its server `wf` columns are Tx(response), Rx(request): streams 9/b
have request Rx ages about 7.276 s and response Tx ages about 4.575 s. There is no
observer inversion repair. Green ordinary gates do not explain the failure.

Do not run these scripts locally. Preparation requires GitHub Actions and modifies
only a `RUNNER_TEMP` copy exported from the exact checked-out source head. Ordinary
manifests, lockfile, CI, diagnostic and qualification jobs remain unchanged. The
separate workflow runs strict controls and both original real matrices once per
feature selection on all three OSes. It preserves 1/5/15 s clocks, concurrency 4,
quota 8, original sockets, drain/reset/flood protections and assertions. There is
no repetition, reconnect workaround, relaxed gate or soft failure.

On Windows the workflow converts `RUNNER_TEMP` to a Git Bash path for archive
extraction, then passes native paths to Python and the diagnostic environment.
Source identity, run/attempt and OS are recorded before export/preparation can fail.
Preparation reads source, patches, integrity manifests and locks as strict UTF-8
bytes, with no BOM removal or newline translation, and writes UTF-8 bytes explicitly.
Appending the temporary manifest override also preserves all preceding bytes.

## Provenance

Upstream is published **h2 0.4.19**, MIT, from
<https://static.crates.io/crates/h2/h2-0.4.19.crate>. Archive SHA-256:
`ef8e5e5a340588f4452631496976cf8636d4a7ecf600239fdc27615d2530bc16`.
The version, manifest, LICENSE and requirements are preserved. LICENSE SHA-256:
`b21623012e6c453d944b0342c515b631cfcbf30704c2621b291526b69c10724d`.
No Hyper/Tokio fork or global-cache mutation is used.

`prepare.py` verifies the archive and every patch preimage against checked-in
integrity manifests. Each unified hunk applies at its declared original line with
exact old text: no search, offset or fuzz. Zero-context hunks avoid whitespace-only
context lines in the checked-in patch files; full-file preimage checksums supply the
unchanged context. Every postimage is then checksum-verified.
`h2-0.4.19.patch` adds the private numerical seam; `observer.patch` attaches the
instance adapter and compacts only experiment rendering labels. Changes to their
preimages require regeneration and review of patches and integrity manifests.

Only the temporary workspace gets the h2 override and direct h2 dependencies in
Alloy/example-bench. CI requires the exact expected lock delta: remove h2's registry
source/checksum and add those two edges, preserving every other package pin and edge.
Each feature graph's full Cargo metadata is retained and its single local h2 node
validated. Snapshot `pf` records archive, both patches, graph SHA-256 and source
head. Artifacts retain the graphs, instrumented manifest/lock, identity, outcome
and bounded captures. `fork=true` / `qualification=false` excludes interpreting a
fork pass as ordinary qualification.

The reviewed expected lock is seeded only into the `RUNNER_TEMP` workspace.
Both feature graphs must resolve with `cargo metadata --locked` and pass the strict
local-h2 source check before controls can run. No `cargo update` re-resolves allowed
transitive ranges. Later policy, lint, controls and real matrices remain locked.
Graph selection sets each feature section's own full metadata fingerprint.

Before reporting metadata failure or an exact byte mismatch, artifacts retain the complete
`instrumented.lock`, `expected-protocol.lock`, instrumented manifest and patch
provenance. A mismatch prints and retains `lock-diff.txt`, then exits with failure;
both attempted metadata files and their exit statuses are retained even on failure.
No serialization, semver, pin or target-specific edge normalization is applied.
If Cargo rejects the seeded graph, preparation fails rather than rewriting it.

Run 37290098822 at `159d8f9` failed preparation on all three OSes; it supplied no
compilation or observer qualification. Windows passed original-source verification
then failed the patched `src/proto/connection.rs` checksum. Its untouched line 288
contains a UTF-8 em dash. The old platform-default `read_text()` followed by UTF-8
encoding can transcode those bytes on Windows; binary UTF-8 decoding preserves them.
The archive, original and patched checksum gates remain unchanged.

The complete Linux/macOS lock artifacts show exactly five transitive edges changing
from `windows-sys 0.61.2` to `0.52.0` after `cargo update`. They are real graph changes,
not serialization. The affected published manifests allow the original locked version:

| Published package | Windows dependency requirement | Archive SHA-256 (matches ordinary lock) |
| --- | --- | --- |
| [errno 0.3.14](https://static.crates.io/crates/errno/errno-0.3.14.crate) | `>=0.52, <0.62` | `39cab71617ae0d63f51a36d69f866391735b51691dbda63cf6f96d042b63efeb` |
| [rustix 1.1.5](https://static.crates.io/crates/rustix/rustix-1.1.5.crate) | `>=0.52, <0.62` | `891efababe418670775f199f0d233d84843c227a0949a883ce15b37c78d6629d` |
| [rustls-platform-verifier 0.7.1](https://static.crates.io/crates/rustls-platform-verifier/rustls-platform-verifier-0.7.1.crate) | `>=0.52.0, <0.62.0` | `1167586491e2b18b8bfbb293e8180ec17c201c4f076d7cb3070ca964e7598f98` |
| [tempfile 3.27.0](https://static.crates.io/crates/tempfile/tempfile-3.27.0.crate) | `>=0.52, <0.62` | `32497e9a4c7b38532efcdebeef879707aa9f794296a4f0244f6f69e9bc8574bd` |
| [winapi-util 0.1.11](https://static.crates.io/crates/winapi-util/winapi-util-0.1.11.crate) | `>=0.48.0, <=0.61.*` | `c2a7b1c03c876122aa43f3020e6c3c3ee5c05081c9a00739faf7503aeba10d22` |

`rustls-native-certs` and `windows-link` themselves retain their original edges.
These static constraints justify retaining the reviewed graph; hosted locked
metadata still must validate the entire graph on all three OSes and both features.

Run 37318325950 at `47ef332` passed exact preparation, both locked metadata graphs
and formatting on all three OSes. Both feature sections then failed strict lint on
the same manual no-op waker and nested teardown condition, before observer controls
or real matrices ran. The overlay uses `Waker::noop()` and an equivalent let-chain,
preserving the single retained-dispatcher poll after runtime destruction and guard
cleanup. These lint repairs require fresh hosted gates and do not explain #142.

## Boundaries and limits

The adapter scopes construction/polling on the actual selected socket. h2 captures
that observer at connection construction and restores it on every driver poll,
including an explicit absent observer. The synchronous thread-local scope restores
its previous context on return or unwind and never spans an await. There is no
registry, global logging, payload formatting or ordinal-derived stream identity.
Management and unobserved runtimes supply no observer.

Decoded HEADERS, CANCEL and DATA are observed in `recv_frame` before stream
admission. DATA bytes are the decoded payload length, excluding padding. These
counts distinguish plaintext receipt from decoder progress; they do not establish
stream admission, response-waiter notification or fresh post-cancellation output.

Every successful retained-CANCEL application copies its before/after send state
under the existing stream-state and send-buffer locks. The callback runs after
both release. The first decoded CANCEL and its first successful application remain
immutable. A separate selected tuple tracks the latest successful retained CANCEL
until the first queue-invariant violation, then permanently retains that violating
tuple. All later applications still increment the application count.

The invariants are: after-queue empty; buffered bytes, requested bytes and stream
capacity all zero; before-staged THIS implies after-staged INVALIDATED. Connection
capacity need not increase because reclaimed capacity may immediately be reassigned.
Decode/application count differences alone are not evidence of a source defect.

Queue fields retain their prior meanings: `empty` is the pending-send deque state;
`buffered` is `buffered_send_data`, including codec-owned DATA until reclamation;
requested/capacity use the existing fields and nonnegative `as_size()` view.
Staged codes are 0 none, 1 this stream, 2 another stream, 3 invalidated Drop.
Accepted bytes cannot be retracted. Private buffers and negative windows are unknown.

Pending counters cover the actual GOAWAY, control-output, codec decode/read,
send-ready, send-flush and shutdown branches. They do not identify lost wakes,
parked tasks, cooperative budget, kernel readiness or private TLS occupancy.

All three actual `conn_error` assignment paths retain their original assignments
and overwrites: handle-error, received GOAWAY and EOF. Only prior-None assignments
supply a first-assignment event. A borrowed flag reference is obtained before
protocol locking; exactly two atomic loads surround the unchanged assignment.
Events are emitted after both protocol locks release. Connection drop restores
its captured observer, including explicit None, observes entry and typed prior-error
presence (poison means unknown), then keeps the existing `recv_eof(true)`. The
existing user-GOAWAY API also restores its captured observer. Actual h2
`Poll<Result<(), Error>>` terminal results include send-ready/flush/shutdown errors,
which can bypass read-error assignment. Stream resets consumed by `poll2` are not
connection terminal results. No peer error Debug/string or OS-code inference is added.

The first executed close branch is frozen in protocol F bits 29–31 under the
same endpoint mutex. Zero means no observed branch. Each callback runs immediately
before its unchanged close action; later branches never replace the first. The
no-stream/reference helper restores the connection's captured observer, including
explicit None, because it runs before the inner poll scope. This field records
an executed branch only. It does not establish kernel close/reset causality,
remote TLS/frame decoding, completed shutdown or a passing strict control.

Runtime entry is marked for each bounded endpoint immediately before its owning
runtime's existing destruction. This includes the real matrix client runtime and
the server runtime after its existing accept-loop future returns, including unwind.
The temporary `run.rs` overlay wraps the existing runtime with a test-only entry observer; the
wrapper marks immediately before dropping that same runtime, including unwind.
The explicit `drop(runtime)` and runtime construction/polling remain unchanged;
its original full-file SHA-256 is
`4d251399906264e67a760f15fa8ce149fed18d5cc7a7d79ca4f05f4bb843a1b5`.
Its exact preimage hunks and full postimage checksum are checked by hosted preparation;
the existing five preimage hashes remain unchanged. Collector and unobserved
runtimes remain absent. Entry does not mean orderly socket/TLS shutdown.

Interpret assignment samples conservatively: before=true means the prior atomic
load observed runtime entry; false/false means neither load observed entry;
false/true brackets an observed flag change, with assignment order inside that
interval unknown. These classify atomic read intervals, not universal causality.
Cursors order observed callbacks. Endpoint captures remain sequential.

## README-v2

The existing live protocol box moves from `WireState` to `WireObservation`; its
frozen counterpart moves to `WireCapture`. There is one live box per endpoint,
not a second live reservation. Corresponding atomic arrays use the existing endpoint
mutex for mutations and frozen copying. Runtime flags can be borrowed without taking
that mutex under protocol locks. No history, registry, task, connection, request,
stream, control, SETTINGS, timestamp or clock slots are added to the observer.

Both records use `repr(C)`: W `[u64; 23]` (184 bytes), N `[u32; 16]` (64 bytes),
F `u64` (8 bytes), exactly **256 bytes**. Live arrays use corresponding atomics.
Hosted assertions check both actual sizes. Buffered `usize` values fit losslessly
in `u64` on the three hosted 64-bit targets. Counters saturate; cursor zero is absence.
IDs and queue zeroes are interpreted only when their corresponding cursor is present.

| W index | Meaning |
| --- | --- |
| 0 | Decoded HEADERS count |
| 1 | Latest HEADERS cursor |
| 2 | Decoded CANCEL count |
| 3 | First CANCEL cursor |
| 4 | Successful retained-CANCEL application count |
| 5 | First application cursor for the first decoded CANCEL |
| 6–11 | Pending counts: GOAWAY, control, decode, send-ready, send-flush, shutdown |
| 12 | Latest Pending cursor |
| 13 | Decoded DATA frame count |
| 14 | Decoded DATA payload bytes, excluding padding |
| 15 | Latest DATA cursor |
| 16 | Selected application cursor |
| 17 | First actual connection-error assignment cursor |
| 18 | Connection-drop entry cursor |
| 19–20 | First application buffered bytes: before, after |
| 21–22 | Selected application buffered bytes: before, after |

| N index | Meaning |
| --- | --- |
| 0 | Latest HEADERS stream ID |
| 1 | First CANCEL stream ID; also identifies its first application |
| 2 | Latest DATA stream ID |
| 3 | Selected application stream ID |
| 4–6 | First-before: requested, stream capacity, connection capacity |
| 7–9 | First-after: requested, stream capacity, connection capacity |
| 10–12 | Selected-before: requested, stream capacity, connection capacity |
| 13–15 | Selected-after: requested, stream capacity, connection capacity |

| F bits | Meaning |
| --- | --- |
| 0 | Protocol observation present |
| 1–3 | First-before: empty, two staged bits |
| 4–6 | First-after: empty, two staged bits |
| 7–9 | Selected-before: empty, two staged bits |
| 10–12 | Selected-after: empty, two staged bits |
| 13–15 | Latest Pending stage |
| 16 | Sticky queue-invariant violation |
| 17–19 | First assignment origin |
| 20 | Destructor prior-error inspection succeeded |
| 21 | Owning runtime destruction has begun |
| 22 | Runtime-begun immediately before first assignment |
| 23 | Prior connection-error present at destructor inspection |
| 24–25 | First actual h2 terminal: 0 absent, 1 OK, 2 I/O, 3 GOAWAY |
| 26–27 | GOAWAY initiator: 0 absent, 1 library, 2 user, 3 remote |
| 28 | Runtime-begun immediately after first assignment |
| 29–31 | First executed close branch; 0 absent, values below |
| 32–63 | Terminal ErrorKind discriminant or GOAWAY reason |

Origins are 0 absent, 1 handle-error I/O, 2 handle-error GOAWAY, 3 received GOAWAY,
4 codec EOF, 5 destructor EOF, 6 other handle-error, 7 reserved. I/O discriminants
come from the pinned **Rust 1.98.1** `std::io::ErrorKind` enum; they are not
cross-toolchain or OS codes. Terminal code zero requires its class/initiator bits;
it does not mean unknown information is zero. Destructor inspection is a separate
pre-EOF locked sample; poison leaves bit 20 clear. No payload, TLS credentials,
PING opaque bytes, peer GOAWAY debug data or strings enter these records.

Close branches are 1 no streams or other references, before `go_away_now(NO_ERROR)`;
2 idle after peer GOAWAY or local close-on-idle, before `go_away_now(NO_ERROR)`;
3 codec EOF, before `recv_eof(false)`; 4 close-now, before either unchanged return;
5 normal `poll2` completion, before the `Closing(NO_ERROR, Library)` transition;
6 already going away with the same reason, before the GOAWAY `Closing` transition;
7 buffer-empty UnexpectedEof with the existing server/peer-NO_ERROR condition,
before the `Closed(NO_ERROR, Library)` transition. These values belong to protocol
F, independently of socket `io_errors` bits 29–31, which retain paired close flags.

Each endpoint prints `p2 `, then W as 23 zero-padded 13-digit base36 integers,
N as 16 zero-padded 7-digit base36 integers, F as one 13-digit integer, and LF.
Digits are lowercase `0-9a-z` with no separators. The existing enclosing wire
record supplies endpoint identity. A fixed 13-byte scratch buffer renders numbers.
The exact schema, including its final LF, is:

```text
p2 fork=h2-0.4.19 qualification=false radix=36 widths=23x13,16x7,1x13 order=README-v2 cursor0=absent flags=README-v2 waiter/readiness=unknown
```

Full-integer widths satisfy `36^6 < 2^32 < 36^7` and `36^12 < 2^64 < 36^13`.
Each row is `3 + 23*13 + 16*7 + 13 + 1 = 428` bytes. Schema is 142 bytes;
**four rows plus schema are 1,854 bytes, within the existing 1,884-byte allowance**.
The live/frozen saturation witness includes full integer maxima and exact row width.
Provenance still uses 332 bytes of the existing detail reserve, including H1/failed
dials; H1's 512-byte empty-wire reserve remains unchanged.

## Frozen storage and controls

All **816 marks = 776 original + 40 existing socket/TLS marks** remain, with zero
additional marks. Label compaction preserves every value and identity: `wf` first
stream, `c`/`s` controls/SETTINGS with 0 Tx and 1 Rx, `wp H/C` points, `we` EOF.
The complete saturation witness still requires all 72 server rows, 72 client
first-stream rows, 128 mandatory controls, 48 SETTINGS, four workers, twelve Pending
records and zero required loss. Keep `core < 35,200`, the unchanged 13,864-byte
age/identity allowance and `35,200 + 13,864 = 49,064 < 49,152` wire bytes (48 KiB),
plus at most 65,536 snapshot bytes. No witness is removed to fund instrumentation.
These are hosted assertions, not local or production-heap qualification.

The original concurrent raw-h2 retained-socket controls remain: real first DATA,
actual CANCEL prefix/decode/application, queue clearing, staged invalidation, release
of already staged DATA, and two real empty-GET responses on the original connection.
The captured-observer no-reference close control also completes two actual empty
GET/END_STREAM exchanges before releasing its last sender: the client handshake
returns without reading the peer's SETTINGS. Both it and the explicit-None control
join their original client and server tasks within the unchanged ten-second timeout;
client connection errors and server accept errors remain fatal. The observed control
requires two decoded request/response HEADERS, absent close branches before release,
client first branch 1 and terminal OK after close, an actual server close branch,
reciprocal UUID/generation/endpoints and zero foreign callbacks. This establishes
connection progress before the close seam; it does not identify a kernel reset cause.
Connection-capacity reassignment is permitted without claiming a reset source defect.
Absent-observer, actual Hyper None adapter, socket/TLS gates, privacy and isolation
controls remain. The frozen GOAWAY fixture now checks only actual `c ` control rows,
so its negative assertion cannot match the schema's mandatory `g=goaway` legend.
It retains full before/frozen equality, live CANCEL/GOAWAY witnesses and changed live
output with an actual `g=0:0` control row.

The combined controls adapt the existing retained-sender accept loop, two server
connection tasks, original public/wire/callback futures, four workers, quotas and
1/5/15-second clocks. Authenticated mutual TLS and h2 ALPN are established before
the gate is armed. The existing 4,096-frame burst body supplies old DATA without
changing protocol windows, capacities or reset limits. Accept ordinals never select
owner 2; selection uses the actual socket in endpoint and worker records.

The only held transport boundary is client plaintext reads:
`PlaintextIo<ReadGate<TlsIo<TlsStream<SocketIo<TcpStream>>>>>`. It starts open;
scalar/vectored write, flush and shutdown delegate exactly once. Held reads register
the current waker and recheck; release delegates with the original context. All four
quota-eighth completions rendezvous while retaining their real response bodies. The
leader verifies all four exact counts of eight and arms the gate; a second rendezvous
prevents any of those bodies from dropping before arming. Before capture, assert later
retained CANCEL application, actual earlier DATA plus an unread remainder, both NEXT IDs from
endpoint records, accepted response HEADERS after old DATA, zero NEXT client plaintext
and decoded headers, and two already-Pending undropped callbacks on that socket.
No callback ordinal is mapped to a stream ID. An absent prerequisite fails the
control; no repeat, wider window, reconnect or softer assertion is provided.

Capture precedes cleanup. Release once and require increased decoded DATA/bytes,
both NEXT response headers, forwarded callback wakes/completion/drop, first DATA,
all four joins and strict generation-1 socket reuse. Render the frozen capture after
release and all runtime cleanup for exact equality, excluding the actual burst payload
marker of 32 consecutive `x` bytes. The burst supplies the existing one-KiB frames; all retained fixtures keep their exact 32-success/32-KiB accounting
assertions. The actual short health gate is unchanged.

The labelled controls retain both actual H2 request senders through their required
proofs. Retaining the public dispatcher alone does not keep its request channel open.
Teardown and alive-companion controls retain the existing public Hyper dispatcher
outside client-runtime task ownership while its original wire child remains runtime-owned.
Poll those same futures while real cancellation/reuse runs; stop owner 2's dispatcher
at the final real reuse completion. Require both retained senders open, zero errors,
and Pending undropped original public and wire futures. The alive companion explicitly
releases owner 2's sender, then polls that dispatcher under the same 15-second wrapper,
with both runtimes alive. Require normal public/wire completion, the actual encoded
NO_ERROR GOAWAY and zero public/wire/socket errors. Receive retains both senders through
its held proof, release and strict all-four joins/reuse assertions.
The teardown control marks runtime entry, destroys the client runtime, then polls
the same retained dispatcher exactly once. Require drop-entry before first assignment,
prior-error absent, destructor-EOF origin, before/after runtime flags true, no h2 Ready
result, and the genuine public BrokenPipe chain. The expected error is confined to
that labelled control. A fixture-scoped dispatcher guard is created before either
retained slot can be filled and stays outside the diagnostics/control/future ownership
cycle. On timeout or assertion unwind it takes both sender and dispatcher slot pairs,
recovers either mutex's poison and releases both mutexes before dropping any owned
public future or sender. Successful cleanup still runs after the same client-runtime
destruction and single retained-dispatcher poll,
before server-runtime cleanup. Completed dispatchers also drop outside the mutex.
Two early-unwind fixtures store both actual diagnostic adapters with owned pending
futures. Their destructors verify both slot pairs unlocked and empty; `Weak` diagnostics and control
references prove reclamation for both ordinary and double-poisoned-mutex unwind paths.
No replacement driver task or manufactured error is used.
Reachability does not attribute the earlier macOS failure. The actual 100/200-ms
health case retains its fatal zero-error assertion and unchanged 15-second wrapper.

Published h2 error callsites remain original 491/521, now patched **523/554**.
Both the diagnostic whitelist and I/O classifier follow those exact positions:
only 523 formats typed `ErrorKind`; 554 records only an actual typed reason/initiator
scalar from the private fork. Scalar 1 is library-initiated NO_ERROR completion;
only that exact value is excluded from the error count. Nonzero reasons, other
initiators and missing/opaque scalars remain errors without formatting peer bytes.
The real protocol-error and truncated-input I/O controls remain fatal, and the scalar
collector has negative controls for every excluded alternative. Do not broaden the filter.
Server saturation's omitted endpoint and plaintext-I/O label, compact `p=` EOF
control, required controls/rows and ordinary graph gates remain strict.

The actual `aac2334` hosted failures and bounded repair are recorded in
[repair29.md](repair29.md). The receive rendezvous and stale callsite whitelist have
source-supported corrections. Unexpected mTLS endpoint errors remain **UNKNOWN**.
The strict zero-error check now captures all bounded endpoint/frame/typed protocol
records before checking, emits that capture on assertion failure, then resumes the
same panic. Unexpected retained public results also emit the bounded capture before
their fatal unwrap. This precedes assertion-driven runtime/dispatcher cleanup, not
every naturally completed future's teardown. It adds no observer slots or rendering budget.
The separate ordinary short-warmup failure remains UNKNOWN; the older post-teardown
BrokenPipe/wire/write-error evidence does not establish an initiating cause.

The actual `d699ca7` controls and separate ordinary timeout are recorded in
[repair30.md](repair30.md). This round corrects the retained-sender fixture lifetime
and adds typed GOAWAY classification. It does not classify every old opaque event
as benign: the Windows capture also contains a real server write error. Fresh hosted
qualification is pending; ordinary workload and production code are unchanged.

The literal formatting repair for protocol run 37336660480 at `9d18a2a` is recorded
in [repair32.md](repair32.md). All three OS jobs stopped at the same formatting
region before protocol tests. The replacement head requires fresh hosted execution
on all three OSes with both feature selections; no protocol pass is claimed.

The actual `70b3837` alive-control failures and bounded typed I/O observations are
recorded in [repair33.md](repair33.md). All five complete failure captures were
inspected. Four contain paired-server socket/write or flush errors; Windows'
earlier capture has zero errors before the live assertion fails. Their exact I/O
kinds and initiating causes remain **UNKNOWN**. The overlay now retains the first
numerical error kind and operation separately at the socket and plaintext
boundaries, with sampled release/runtime/EOF/drop flags on the exact endpoint.
`socket drop=bit/hex` is decoded in that report. Failure capture occurs after the
strict live check fails, before outer fixture unwind. No errors are filtered,
subtracted or classified as benign. Fresh hosted validation remains pending.

The five actual `8101a0c` alive-control captures are classified in
[repair35.md](repair35.md): paired-server first socket and plaintext vectored-write
kinds are BrokenPipe on Ubuntu, ConnectionAborted on Windows and ConnectionReset
on macOS. The initiating cause remains **UNKNOWN**. A fixture-only pairing now
samples client socket shutdown entry, successful return and wrapper-drop entry
before the server socket poll and at its first socket error callback. The report
defines the eight-digit packed word and the observation's ordering limits.
Shutdown still delegates once and returns the original result; every observed
typed error also fails the strict check. No error is filtered or reclassified.

## Retirement and delivery

Retire this directory/workflow when #142 has sufficient evidence or a reviewed typed
upstream API replaces the fork. Do not promote the override/private API to ordinary
dependencies. Pin, overlay, boundary or rendering changes require fresh integrity
manifests, graph provenance, strict controls and whole-head review.

Local work is static inspection/editing, checksums, Git and `git diff --check` only.
Formatting, lint, compilation, footprint/privacy controls and real matrices remain
pending hosted CI. Observer overhead can perturb scheduling. Root owns independent
full review, CI/log interpretation and merge decisions. No cause fix, closure or
merge-readiness claim is made.
