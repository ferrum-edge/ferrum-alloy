# Literal hosted formatting repair32 at 9d18a2a

PR [#147](https://github.com/ferrum-edge/ferrum-alloy/pull/147) references
[#142](https://github.com/ferrum-edge/ferrum-alloy/issues/142) only. The initiating
cause remains **UNKNOWN** and the issue remains open. This delivery changes only
the demonstrated formatting region, its patch coordinates, one integrity entry
and this documentation.

## Actual hosted evidence

The inspected clean local, remote and PR head was
`9d18a2a65cbd7fac98b342ffead59fff5a48e63d`, with merge base
`b998587fee6e28297e4ef32cd34b89b9a76ad76f`. Protocol run
[37336660480](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37336660480)
failed its experiment workspace formatting check on all three OSes:

| OS | Job | Supplied log scope | Supplied log SHA-256 |
| --- | --- | --- | --- |
| macOS | [111853049974](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37336660480/job/111853049974) | Full job | `7dbc704b7d8cf792668f3e638a96f79752e9539e5dd910a3dc7a9cf3bffc11cf` |
| Ubuntu | [111853050443](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37336660480/job/111853050443) | Failed step | `0a861f72bc638ed64873451aa803cc31adf05c7e81985a6efe92111dd229185b` |
| Windows | [111853050485](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37336660480/job/111853050485) | Failed step | `17456a8774f3b9b918e3ba6c4fa924614810191e869d6fc21957e06ecc003cca` |

Each log prints the same `examples/bench/src/client.rs:2103` replacement:
split the filtered `Dispatch::new(registry().with(...))` construction into the
formatter's nested multiline call. These jobs stopped before protocol tests;
neither feature section supplies protocol-control or real-matrix qualification.

## Literal repair and integrity scope

The exact printed replacement is applied to `observer.patch` and the retained
actual `target/repair23-evidence/alloy/examples/bench/src/client.rs` postimage.
The insertion at original line 2021 grows from 49 to 51 lines; all later client
postimage hunk offsets increase by two, with original coordinates unchanged.
Other file sections and their hunk coordinates are unchanged.

Only the client entry in `alloy-patched.sha256` changes. Its SHA-256 is computed
from the actual retained file bytes:
`29f06ec94cb3c7564c68c648aff29e13c519d39d904510a3c65b75342f223b5a`.
All 14 retained primary postimages matched their manifests before the edit and
match after it: six Alloy and eight h2 files, one changed and thirteen unchanged.
Both original-source integrity manifests, the h2 patch and h2 postimage manifest
are unchanged. No postimage was regenerated or replaced.

Static inspection, literal editing, actual-byte SHA-256 checks and
`git diff --check` are the only local validation. No local repository code,
preparation/patch/lock algorithm, formatter, compiler, lint or test ran.
The original Pending/server-owner/dispatcher and sender retention, double-poison
cleanup, receive/reuse controls, strict error filters and negative controls remain
unchanged. Schema, pins, runtime behavior, storage/rendering bounds, capacities,
budgets and 1/5/15-second clocks remain unchanged.

## Fresh-head limits

The commit containing this repair is unqualified until fresh hosted gates run.
Prior ordinary CI, Health Diagnostic and Hosted Shared metadata successes do not
qualify it. Protocol formatting, compilation, strict lint, privacy/footprint
controls and both feature matrices on all three OSes still require fresh hosted
execution. The previously observed Windows write-error symptom is not qualified
by this formatting-only run. Root owns new-head CI, full review, any focused
read-only review needed for later logic changes, PR metadata and merge decisions.
This delivery makes no protocol-green, causal-fix or issue-closure claim.
