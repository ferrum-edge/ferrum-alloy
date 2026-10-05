# Bounded repair29 at aac2334

PR [#147](https://github.com/ferrum-edge/ferrum-alloy/pull/147) references
[#142](https://github.com/ferrum-edge/ferrum-alloy/issues/142) only. This repairs
two demonstrated fixture/observer defects and retains two separate unknowns.
It does not establish a fix for the initiating cancellation-health failure.

## Actual hosted evidence

The inspected source/remote head was
`aac23344106d25f0ae73d809d7d5d4f670df6efe`. All six protocol feature sections
reached strict controls in run
[37321227955](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37321227955):

| Job | All features | Default features | Full failed raw-log SHA-256 |
| --- | --- | --- | --- |
| macOS 111800508141 | 81 pass / 4 fail | 81 / 4 | `87fb73c45a7adfc8a49d02a17d30fba3542e0928dac5527f26269076820d5ba0` |
| Windows 111800508208 | 82 / 3 | 83 / 2 | `ca89574ff9614f139aab0bc8a604ede86f1fed4dd6a83bd02f239508a37f4b0d` |
| Ubuntu 111800508251 | 82 / 3 | 81 / 4 | `83c25f613bf0b2f0fb21db97f86a9d73df94396c74a23975d9957c477e934347` |

Failures were the live mTLS controls' client line 4332 zero-error assertion,
receive-control lines 4105 and 4836 exact-eight assertions (observed 4/6/7),
and real-child line 3012 `wire_events > 0` (observed zero). The Windows
line 4712 SendError followed the receive assertion panic. Preparation, full
locked graphs, formatting and the previously repaired strict lints passed.

The separate ordinary shared run
[37321227762/job 111800508249](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37321227762/job/111800508249)
had 76 passes and one `cancellation_remains_healthy_after_normal_warmup` failure:
client line 941 `error_events == 0`, observed 1. Its complete failed raw log hashes
to `87a194789f3c4a116381ca1831e5c4212f9569321e0d4fdf6f52553a6fb38165`.
Artifact 11350496082 names this exact source head; its harness log hashes to
`c891be2db51221bd576fcebc80a56b9959d0fe05770b279ead88da493f8d1b04`.
All five downloaded artifact files match their retained SHA256SUMS. Its lock
and toolchain input hashes also match the assigned checkout.

## Source-supported corrections

The receive gate's completion seam inspected only workers 2/3 and then asserted
workers 0/1 were also finished. There was no happens-before relationship between
the two owners. If owner 2 finished first, the all-four assertion could panic
before `read.hold()` while workers 0/1 were still short of eight. This directly
explains the premature line 4105 assertion; a later end-probe failure can follow
that worker panic. It does not explain the separate endpoint-error assertion.

The overlay makes the existing completion hook await a fixture-owned four-worker
barrier only at measurement completion eight. Each worker first records its real
first DATA and retains that response body. The leader keeps the original all-four
exact-eight assertion and arms the original read gate. A second barrier keeps every
body alive until arming finishes. Worker mutex guards end before either await.
No counter is synthesized; no deadline, quota, payload, socket, window or protocol
limit changes. Alive/teardown completion selection stays on the original owner 2.

The retained h2 `connection.rs` postimage has SHA-256
`4ddda3d895d81734cd3af2e876b5e0ab714d30495e3f56636802b95c8c84fa41`.
Full source inspection locates the typed `kind` DEBUG event at 514 within
`handle_poll2_result`'s `Error::Io` arm, and opaque `e` DEBUG event at 544 within
`handle_go_away`. The overlay selected 516/546, which are not these callsites.
The whitelist now selects exactly 514/544; only 514 formats `ErrorKind`. Target,
level, event/field checks, child-owned subscriber, pinned source hashes and opaque
protocol privacy remain intact. The actual real-child error control stays strict.

## Remaining UNKNOWN and distinguishing capture

No protocol-control failure capture identifies which endpoint/operation made
line 4332 false. Sender release, endpoint shutdown and actual wire failure must
not be conflated. The live positive control remains required to finish normally
with zero public/wire/socket errors; teardown still polls the same retained public
future after destroying its owning runtime and requires typed destructor origin.

The zero-error check now retains its complete bounded HealthCapture before any
assertion, catches only to emit that capture, and resumes the identical panic.
All four actual endpoints, their frame state, socket outcomes, typed h2 terminal
and assignment/drop/runtime fields use the existing 48/64-KiB rendering limits
and 256-byte protocol records. The boundary is explicitly before the zero-error
check, not before all naturally completed futures' cleanup. Assertions are unchanged.
Unexpected public results from the original polled dispatchers or the alive companion
also emit these bounded records before their unchanged fatal unwrap.

The ordinary artifact records owner 0's public BrokenPipe, owner 2's real wire
event, and both server endpoints' write errors. Its boundary is
`current-state-after-possible-teardown`; runtime-owned children and sockets were
already dropped. This does not order the error before runtime destruction or
prove a TLS-version, transport, reset-queue or waiter cause. Ordinary health,
warmup/accounting assertions and its original clocks remain unchanged.

## Static-only delivery

Only literal edits, source/diff inspection, Git/GitHub reads and byte hashes were
used locally. No preparation, lock builder, formatter, compilation, lint or test
was executed. The actual retained postimage was edited literally and hashed;
overlay positions were adjusted by hand. All 14 retained postimages are checked
against their manifests; the other 13 hashes, all preimages, h2 patch, toolchain,
ordinary lock and graph requirements are unchanged.

Root must review the complete new diff, obtain a fresh independent review and
qualify every actual hosted gate at the pushed head. If endpoint errors recur,
read the new bounded captures before selecting a causal repair. No rerun,
workflow dispatch, merge, review request, PR metadata change or issue closure
is part of this delivery.
