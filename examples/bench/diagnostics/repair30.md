# Bounded repair30 at d699ca7

PR [#147](https://github.com/ferrum-edge/ferrum-alloy/pull/147) references
[#142](https://github.com/ferrum-edge/ferrum-alloy/issues/142) only. This changes
the isolated diagnostic overlay and labelled fixtures. The initiating production
cause and separate ordinary short-warmup timeout remain **UNKNOWN**.

## Actual hosted evidence

The inspected local, remote and source head was
`d699ca723fbd386ea817ee8a17dfc5c1a71c3ab7`. Protocol run
[37329385680](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37329385680)
passed preparation, formatting and strict lint before failing the controls:

| Job | All features | Default features | Supplied raw-log SHA-256 |
| --- | --- | --- | --- |
| Ubuntu 111828283129 | 82 pass / 3 fail | 83 / 2 | `e519143aa411c994c383d36a75bd4fc4dc1fd11a4af4d544f9a919d4375df94f` |
| Windows 111828283164 | 82 / 3 | 83 / 2 | `d13c8ea7d6aa1d99aee96928bbd19187898a4c14abd808786ca3c9a2e76d2162` |
| macOS 111828314983 | 82 / 3 | 82 / 3 | `6dfc21410c8c517d473b6e6248f4405f3b321e61f2029900d7780cad1cffac7d` |

The strict client line 4340 `wire_events == 0` assertion observed 1 in the live
controls. The first Ubuntu alive capture precedes assertion unwind: all four
workers completed reuse, measured eight and 8192 bytes each, with no worker
errors. Owner 2's public dispatcher completed OK, its wire child returned and
dropped, and its client sent a complete GOAWAY with last-stream/reason 0/0.
The paired server saw that GOAWAY and EOF. All four endpoint socket outcomes
in that capture had zero I/O errors. Owner 0 remained running. The owner-2
terminal class was OK and runtime-entry bits were absent.

These observations do not classify every old opaque connection event. The first
Windows alive capture also records a real paired-server write error and I/O
terminal. The first macOS capture records public completion before wire completion,
with zero worker wire events at sampling; the fatal assertion subsequently sees 1.
Captures are explicitly non-atomic. Real endpoint errors remain fatal and require
fresh evidence if they recur.

## Source-supported changes

Published Hyper 1.11.1 `proto/h2/client.rs` returns normal dispatch shutdown at
`req_rx.poll_recv` Ready(None), line 779. The dispatcher owns the receiver, not
the request sender handles. The old controls retained a public future while
worker futures naturally released all senders, allowing it to complete before
the intended Pending/teardown proof. Published h2 0.4.19 closes an idle connection
with no stream references using NO_ERROR (lines 261-266). Its `poll2` routes that
library closure through `Error::library_go_away` (359), and `handle_go_away`
emits the connection DEBUG event even for reason zero. `take_error` returns OK
only when both local and peer reasons are NO_ERROR (249); a nonzero peer reason
remains an error. Public completion alone therefore proves neither wire health
nor the cause of every connection event.

The labelled fixtures now retain two clones of their actual original H2 senders.
Alive/teardown require both open and the selected original public dispatcher and
wire child Pending and undropped before sender release/runtime destruction.
Alive releases owner 2 explicitly, awaits that same dispatcher and wire child,
and requires wire Ready/OK, actual encoded NO_ERROR GOAWAY and strict zero errors.
Teardown keeps the senders through runtime destruction and the existing single
public poll requiring genuine BrokenPipe and typed destructor origin. Receive
keeps them through its held proof, release and strict all-four reuse checks.
The fixture guard takes both slot pairs, recovers poison and unlocks both mutexes
before dropping any retained sender or future. The existing unwind controls now
check both slot pairs are unlocked/empty, including when both mutexes are poisoned.

The private h2 hook at patched line 544 replaces opaque error Debug with a raw-free
`u64`: `(reason << 2) | initiator`, where Library/User/Remote are 1/2/3. The
collector excludes only scalar 1 (Library, NO_ERROR). Every nonzero reason, other
initiator and missing/opaque scalar remains an error. It never formats GOAWAY
debug data. Exact target, DEBUG level, two-field and 514/544 callsite filtering
remain; line 514 still records only typed ErrorKind. No protocol behavior changes.

The new collector control rejects nonzero, user, remote, invalid and unknown
values, panics if an opaque value is formatted, and checks unrelated callsites
stay excluded. The existing real invalid-DATA control now requires typed
PROTOCOL_ERROR/Library evidence; its truncated-input I/O negative remains strict.
The alive companion supplies the real normal-completion control. None ran locally.

## Separate ordinary failure and delivery limits

Ordinary Shared run
[37329385981/job 111828285521](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37329385981/job/111828285521)
had 76 passes and one line 719 Elapsed timeout, with the unchanged 100-ms warmup,
200-ms window and 15-second wrapper. Its supplied full log hashes to
`b6a05e462833d36750debf630bd47da6655e6673f4d98c4e8811d78abb2e20d5`.
Before outer health-future drop, workers 0/1 had completed, while 2/3 awaited
reuse HEADERS on original socket 47392. Its two real response callbacks were
Pending without wakes; server Tx recorded stream 1d/1f HEADERS absent from client
Rx. Owner 0 had already closed socket 47388 with an opaque wire event, NO_ERROR
GOAWAY and a paired-server write error. This does not prove the initiating cause
of owner 2's stalled callbacks. Ordinary code, workload and clocks are unchanged.

Only static inspection, literal overlay/postimage edits, byte hashes and Git checks
were used. No local formatter, compiler, test, preparation or lock algorithm ran.
All 14 retained actual postimages match their manifests; three changed and eleven
are unchanged. Preimages, dependencies, locks, feature/OS controls, quotas, barriers,
first/latest/sticky CANCEL records, 256-byte W23/N16/F8 endpoint state and the
1854 <= 1884 rendering bound remain unchanged.

Formatting, compilation, strict lint, privacy/footprint controls and real matrices
at the new head remain pending hosted CI. Root must qualify the pushed head and
interpret any real write/EOF/nonzero-GOAWAY evidence before another causal change.
This delivery makes no green, production-fix, TLS attribution or closure claim.
