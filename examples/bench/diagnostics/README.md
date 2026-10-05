# Isolated protocol observer experiment for #142

This passive measurement fork is not a production repair or ordinary qualification.
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

## Boundaries and limits

The adapter scopes construction/polling on the actual selected socket. h2 captures
that observer at connection construction and restores it on every driver poll,
including an explicit absent observer. The synchronous thread-local scope restores
its previous context on return or unwind and never spans an await. There is no
registry, global logging, payload formatting or ordinal-derived stream identity.
Management and unobserved runtimes supply no observer.

Only decoded HEADERS/CANCEL IDs, reset send-state tuples and typed Pending stages
are emitted. HEADERS means codec decode before stream admission, not waiter
notification. For a retained CANCEL stream, state is copied before reset application
and after the existing `recv_reset`/`handle_error` calls. The callback runs after both
stream-state and send-buffer mutexes release. No operation, result, protocol
transition, wake target or wake timing is replaced or deferred.

Queue fields are `empty/buffered/requested/stream-cap/conn-cap/staged`. `empty` is
the pending-send deque state. `buffered` is h2's `buffered_send_data`, including
codec-owned data until accounting/reclamation; it is not queue-only or unsent bytes.
Capacity uses the existing requested fields and nonnegative `as_size()` view; raw
negative windows remain unobserved. `staged` is in-flight bookkeeping: none, this
stream, another stream, or invalidated `Drop`. It reports ownership/invalidation,
not private buffers or remaining unsent length. Accepted bytes cannot be retracted.

Pending stages are GOAWAY, control output, codec decode/read, send codec readiness,
send flush, and shutdown. They count actual awaited-branch Pending returns, not
parked tasks, lost wakes or kernel readiness. A later branch may replace the last
branch/cursor within the same poll.

The deliberately partial reservation retains **one first decoded CANCEL and its
first successful application per endpoint**, total decode/application counts,
latest decoded HEADERS ID/cursor and fixed Pending counters. Later cancellation
queue states, per-stream first HEADERS, response-waiter registration/notification,
Hyper body-driver state, Tokio cooperative/readiness reasons, private TLS state
and kernel queues remain unobserved. A relevant later cancellation can be missed.
No initiating-interval or streams-5/7 explanation follows from a first-stream
summary. Existing evidence/loss accounting still applies; acceptance is not delivery.

## Frozen storage and controls

One boxed <=256-byte `ProtocolState` uses each existing endpoint. No task, connection,
stream, request, control, SETTINGS or timestamp slots are added; no history is kept.
Counters saturate like existing counters. Unknown first/application fields are `-`.
Cursors share the endpoint sequence without adding `Instant` marks. The precise
baseline count is **776 original plus 40 existing socket/TLS marks**, all retained;
the experiment adds zero.

Maximum numeric output is 343 bytes per endpoint. Wire schema is bounded at 512
bytes: `4 * 343 + 512 = 1,884` new mandatory wire bytes. Exact provenance uses 332
bytes of the existing detail reserve, including H1 and failed-dial captures; H1's
512-byte empty-wire reserve is preserved. Compaction retains every
numerical value/identity: `wf` is first-stream, `c`/`s` controls/SETTINGS with
direction 0(Tx)/1(Rx), `wp H/C` points, and `we` EOF marks. Schemas define aliases.
The full-width witness retains strict `core < 35,200` and the original allowances:
`35,200 + 13,864 = 49,064 < 49,152` (48 KiB). Immutable saturation also populates
protocol fields and requires all 72 server rows, 72 client first-stream rows,
128 controls, 48 SETTINGS, four workers, twelve pending tasks, zero required loss
and a <=65,536-byte first-failure snapshot. No witness is removed to fund the seam.
These assertions require hosted execution; static inspection is not qualification.

The strict retained-socket control runs independent instances concurrently. Both
receive actual cancellation DATA before CANCEL. One also queues 32 KiB and holds
socket writes until h2 owns a codec frame. It requires real prefix/decode/application
cursors, queue clearing, capacity reclamation and staged invalidation. Release uses
the same socket, observes acceptance of already staged DATA, then requires two empty
GET responses/decoded HEADERS on that original connection. Thus post-prefix DATA
acceptance cannot automatically mean fresh post-reset production. Frozen rendering
must survive release/cleanup unchanged and exclude the private DATA marker.

Another original-socket control constructs unobserved connections under a different
ambient poll observer and requires zero callbacks through two real GETs. A separate
control polls the actual Hyper `diagnostic_handshake(..., None)` adapter under a
foreign observer without fixture masking, verifies scope restoration, two real GETs
on its original accepted socket, public-driver completion and wire-child drop, and requires zero
foreign callbacks through closure. The `Some(wire)` adapter remains unchanged.
The existing transport/protocol error control preserves the published whitelist at
patched lines 514/544: only 514 formats `std::io::ErrorKind`; 544 keeps peer details
omitted. Server saturation still requires exactly one omitted endpoint and its
server-plaintext-I/O boundary. The EOF control uses the compact `p=` preface label
while preserving its zero raw-read-error assertion. The existing
released Hyper gate additionally requires actual client decoder HEADERS. Held-HEADERS
fixtures remain controls, never causal evidence for the genuine failure.

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
