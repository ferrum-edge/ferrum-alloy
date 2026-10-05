# Bounded paired socket-close observation repair35 at 8101a0c

PR [#147](https://github.com/ferrum-edge/ferrum-alloy/pull/147) references
[#142](https://github.com/ferrum-edge/ferrum-alloy/issues/142). All five current
strict alive-control failures have observed typed I/O kinds. Their initiating
cause remains **UNKNOWN**. This delivery adds only the missing paired observation;
it does not claim a lifecycle repair, protocol pass or production fix.

## Five complete actual failure captures

The inspected clean branch was `root/20261005/alloy142-server-evidence`, local and
remote head `8101a0c65def890b615723d1ef7e31f8d4b9b7b5`, with base
`b998587fee6e28297e4ef32cd34b89b9a76ad76f`. Protocol run
[37350246346](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246346)
failed five strict alive-control groups. macOS all-features passed; it does not
explain the five failures. Each complete persisted failure capture was personally
read from its instance/provenance records through its loss footer in the supplied
full primary job log, including every matched endpoint, task, body and worker row.

| OS / feature | Actual job | Capture lines | Owner 2 generation 1 client local / remote ports | Paired accepted-server local / remote ports | Server packed word | First socket / plaintext kind and operation | Initiating cause |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Ubuntu / all | [111898997201](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246346/job/111898997201) | 3749–4435 | 35734 / 34357 | 34357 / 35734 | `31a11a1` | BrokenPipe / BrokenPipe, vectored write | UNKNOWN |
| Ubuntu / default | same job | 5765–6450 | 36192 / 34673 | 34673 / 36192 | `31a11a1` | BrokenPipe / BrokenPipe, vectored write | UNKNOWN |
| Windows / all | [111898996744](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246346/job/111898996744) | 1301–1981 | 60375 / 60373 | 60373 / 60375 | `31a31a3` | ConnectionAborted / ConnectionAborted, vectored write | UNKNOWN |
| Windows / default | same job | 3311–3996 | 60616 / 60614 | 60614 / 60616 | `31a31a3` | ConnectionAborted / ConnectionAborted, vectored write | UNKNOWN |
| macOS / default | [111898997243](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246346/job/111898997243) | 2652–3340 | 50015 / 50013 | 50013 / 50015 | `31a21a2` | ConnectionReset / ConnectionReset, vectored write | UNKNOWN |

The server owner is accepted-socket ordinal 1, a different namespace from client
owner 2. These reciprocal ports come from the actual original sockets, not worker
or stream ordinals. In every row the two 12-bit first-error samples are respectively
`1a1`, `1a3` or `1a2`: kind 1/3/2, operation 2 (vectored write), release-entry and
return sample 3, runtime-entry/EOF/socket-drop samples zero. This is the numerical
packing in [repair33.md](repair33.md) and the actual error callback source, not
interpretation of an error string or a raw OS code. Scalar and vector writes share
the socket progress row, but the typed operation distinguishes them.

All five captures have readiness and joins 4/4, successful preparation 1/1,
warmup 8/8, included measurement 8/8 and reuse 1/1 on each of the four workers.
Every worker has `errors=0`, `error_events=0` and `wire_events=0`. The selected
client's public dispatcher completed OK and its original wire child's unit future
returned Ready and dropped. Those public/unit completions alone do not establish
inner wire health. Client socket and plaintext error counters are zero, its
plaintext shutdown outcome is OK and its transmitted GOAWAY is NO_ERROR. The
other retained client remains Pending/undropped with no endpoint errors.

Each matched server has one socket write error and one plaintext vectored-write
error, no observed read error or EOF, and wrapper drop recorded at capture. Its
last retained received complete frame is HEADERS on stream `47`; final client
CANCELs on `45`/`47` and GOAWAY are not retained as decoded on that server at this
cut. The macOS server additionally received a WINDOW_UPDATE before that last
HEADERS. The other server retains zero socket/plaintext errors and is undropped.
The observed handler/body completions and drops are local future observations,
not acknowledgements that all queued DATA or CANCEL bytes reached the peer.

The capture boundary is
`controlled-after-zero-error-check-before-fixture-unwind`. The actual strict
assertion already failed, while outer fixture custody remains. Naturally completed
futures may already have destroyed their own socket/TLS/body locals; this is not a
claim to precede all teardown. Required detail/wire losses are zero in all five.
Optional detail/wire losses are, in table order, 40600/2639, 40589/2643,
40613/2799, 40609/2654 and 40548/2554. Reading each complete bounded capture does
not imply that its omitted optional history is available.

Full supplied log SHA-256 values, from actual bytes:

```text
72cc093719eaf6804b64fd1c01887587efb61b91adeb674588cd383965639d51  alloy147-8101-111898997201-full1802.log
f88bf3c936c5a056b3d08a4215025aad29e3115e451cfc5421a236344a79e75c  alloy147-8101-111898996744-full1802.log
c7a0a52bb9d8d8937638b34dd4204f74810cb6b4f9e22a927e1210bc9ccbf08b  alloy147-8101-111898997243-full1802.log
```

## Actual source order and causal limits

`protocol_pair` selects the original owner 2 generation 1 client address from its
worker record and asserts reciprocal addresses against the accepted server. The
fixture observes the established original sockets below TLS and plaintext above
TLS. Server `JoinSet` custody continues until outer cleanup; completed connection
futures can independently drop their own transports before that cleanup.

The actual cancellation exchange reads the first nonempty DATA frame, records its
completion and returns, dropping its `Incoming` body. Published Hyper 1.11.1
`src/body/incoming.rs` stores an h2 `RecvStream`; published h2 0.4.19
`src/share.rs:480` clears received DATA on its drop. The retained
`src/proto/streams/streams.rs:1741–1825` then drops stream references and schedules
implicit CANCEL when interest is cancelled. Neither body completion nor sender
reuse proves the peer decoded that last CANCEL.

Before releasing owner 2's last retained actual sender, `protocol_origin_pending`
asserts both sender channels open, selected public dispatcher and original wire
child Pending/undropped and no runtime-entry mark. The sender-slot mutex releases
before sender drop. The same retained public dispatcher is then taken and awaited,
the wire-child drop is awaited, and the unchanged strict check runs. The other
sender and dispatcher stay in their slots. Runtime cleanup, dispatcher guard,
poison recovery and server shutdown remain in their original order.

Published Hyper 1.11.1 `src/proto/h2/client.rs:164–205` creates the original
`ConnTask` and its drop channel; `:778–782` returns public Shutdown when the request
sender channel closes. `ConnTask::poll` (`:341–362`) polls the same connection and
handles that drop notification. Published h2 client `:1470–1485` checks remaining
stream references; the retained connection `:340–348` shuts down its codec in
Closing. Published `src/codec/framed_write.rs:139–185` writes queued DATA/control
bytes, propagates actual write/flush errors and flushes before shutdown.

Published tokio-rustls 0.26.6 `src/client.rs:512–530` sends close_notify and delegates
to its common stream. `src/common/mod.rs:362–376` drains encrypted writes, then
delegates original socket shutdown, retaining its existing NotConnected policy.
Its vectored-write path (`:313–342`) returns encrypted-write errors unchanged.
The observer's SocketIo samples the actual delegated error kind below TLS;
PlaintextIo samples the actual returned kind above TLS. None of these source paths
establishes that the captured error is harmless or identifies a kernel initiator.

The dependency archives personally inspected match the ordinary lock checksums:

```text
27b501faa50e7a26c3d3560ca625132f4078a17771f4810baf70475ae48cbe43  hyper-1.11.1.crate
ef8e5e5a340588f4452631496976cf8636d4a7ecf600239fdc27615d2530bc16  h2-0.4.19.crate
c9cc2678c2cdd569ef8215e2afd7954ada2ae20b4fdd2c5fe6139a3b02d105db  tokio-rustls-0.26.6.crate
```

These five old captures do not observe original client socket shutdown, and their
client socket-drop bits have no cross-endpoint order. Release flags are sampled
after a delegated error poll; they cannot tell whether that poll began before
shutdown/drop. Thus sender release permitting normal shutdown is source-supported;
sender release or fixture teardown causing these exact server errors is UNKNOWN.
No specific lifetime repair is justified. The historical #142 initiating cause,
the separate #150 timeout and any common cause remain UNKNOWN. No dedicated
216-cell/1080-observation physical proof is claimed.

## Minimal missing observation

Only the isolated `observer.patch` changes execution. Ordinary source, dependency
graphs, h2 fork, TLS policy, production behavior, clocks, four workers/two streams,
phase counts, request-success checks and frame requirements are unchanged.

After the existing Pending/open prerequisite check and before the existing sender
release, the exact client/server pair gets one shared `Arc<AtomicU32>` through
one-time endpoint cells. Instance, generation and reciprocal socket identities
are asserted. Only that client publishes three monotone bits: original socket
shutdown entry before delegation (1), actual successful shutdown return (2), and
original wrapper-drop callback entry (4). The drop bit precedes automatic inner
socket field destruction; it does not claim kernel close completion.

The same server samples those bits immediately before each delegated socket poll
and again in its first socket-error callback. The entry sample is retained when
that first error is recorded; later polls/errors cannot overwrite either sample.
This distinguishes a close observed before poll entry from one observed only at
the error callback. The entry checkpoint precedes inner delegation; interleaving
between them and kernel syscall order remain unknown. The error checkpoint is
after the actual delegated result. It does not convert observation order into a
kernel cause.
The first plaintext kind remains separately retained without a peer-close sample.

The existing `io_errors: u32` is reused without new capture records, timestamps,
history slots or participants. `socket drop=bit/hex` now uses eight hex digits.
Bits 0–25 keep [repair33](repair33.md)'s definitions. The remaining bits are:

| Endpoint in this paired alive control | Bits 26–28 | Bits 29–31 |
| --- | --- | --- |
| Selected original client | Cumulative shutdown-entry/OK-return/wrapper-drop-entry flags | Zero |
| Its accepted server | Peer flags at the first failing socket poll's entry; latest poll entry until a first socket error exists | Peer flags at the first socket-error callback; zero until it exists |
| Unpaired endpoint | Zero / UNKNOWN | Zero / UNKNOWN |

Zero at an armed server checkpoint means no published close marker was observed
by that checkpoint; it does not assert the peer or kernel had no other close
cause. Endpoint copies and client state/atomic updates remain sequential. Captures
and clones contain the packed values, not a live reference to the shared flags.

Socket shutdown has one actual inner delegation with the original context, waker
and result. Its entry/return callbacks record numerical state only; actual errors
call the existing typed error seam once. The strict predicate still checks every
original directional and operation outcome and socket outcome, and now also
rejects any first typed error, including original socket shutdown errors that TLS
may translate. No ErrorKind, reason, direction or operation is exempted. The
assertion and failure-capture boundary are unchanged.

There is one shared four-byte flag value for one armed fixture pair and fixed
one-time endpoint cells; the shared value owns no endpoint and creates no ownership
cycle. Publication uses SeqCst atomic OR; server checkpoints
use SeqCst loads under their already-existing endpoint mutex. No peer mutex,
protocol lock or lock across delegation is added. Shutdown/drop publication locks
only its own endpoint after the atomic update. Observer overhead can perturb
scheduling; this remains a labelled fork control, not ordinary qualification.

Hosted controls cover close before poll entry versus close between entry and error,
first-sample retention, other-endpoint isolation and frozen clone immutability.
The shutdown mock checks exactly one delegation per call, original waker, Pending,
unchanged typed/text error and OK results, strict rejection and raw-text omission.
Existing footprint limits stay strict; new controls bound the pair value/cell.
The saturated renderer fills all 32 bits and retains all four endpoint copies.
The width adds one byte per endpoint and the socket schema adds five bytes, with
no new ages. The 256-byte protocol record, 428-byte `p2` line, 142-byte protocol
schema, 816 marks, 72 stream rows, 128 controls, 48 settings, `compact.len() < 35_200`
and total wire < 49,152 remain unchanged gates. Actual formatting, compilation,
lint, privacy/footprint and protocol validation require fresh hosted execution.

## Retained bytes and validation

Only literal edits were made to the three corresponding retained Alloy postimages
in `target/repair23-evidence/alloy`. Patch coordinates/counts were changed as
literal metadata. No project decoder, patch assembler, preparation, graph or hash
producer ran or was copied. Native actual-byte hashes update exactly three
`alloy-patched.sha256` entries; the other eleven files are unchanged. Native
`shasum -a 256 -c` checks passed for all fourteen both before and after editing.

Final Alloy retained hashes:

```text
0ee41d38cfbd4dc1da17bba3a0878d3df6d8554a6375950cb8ceebf1d10fa25a  crates/ferrum-alloy/src/bench_diagnostics.rs
31cc8190037ce8c46a0281e2c13609fda780603baece499d41e208bd82d1fc01  crates/ferrum-alloy/src/server.rs
7fbd18e1de4f2bfae3005befdfdf2b28eac5cccd31a344cc18646beceff123f5  examples/bench/src/client.rs
f9b24528ff7922f23d66e2f5e16d056c53447884b71dc153c96e84bd94b116f1  examples/bench/src/server.rs
2959005e3ee5655890c90b893741b4c9cc46e23b2d40617fe3b7172cb0e09934  examples/bench/src/health.rs
13f1c5d5691177f03ac7fc010835c46d02c2dd755bd78f3549210cd1c35d54db  examples/bench/src/run.rs
```

Final h2 retained hashes (`target/repair23-evidence/h2`, all unchanged):

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

The personally inspected full jobs REST response for old-head ordinary CI
[37350246587](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246587)
has all 18 jobs completed/success. Old-head health run
[37350246364](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246364)
and shared qualification run
[37350246293](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37350246293)
also completed/success. Those results do not qualify this delivery or establish
the failed protocol control's cause.

Local validation is static source/log/diff inspection, native hashes and
`git diff --check`. No local project code, formatter, compiler, linter, test,
benchmark, server or integration ran. No nested agent or review was requested.
Root owns whole-head/fresh independent review, fresh hosted gates and actual logs
on all three OSes/both features, causal interpretation and landing approval. The
new head remains unqualified; strict alive controls may continue to fail while
supplying the missing order. No merge, closure, qualification or unrelated backlog
work is part of this delivery.
