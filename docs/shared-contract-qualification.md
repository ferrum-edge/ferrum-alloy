# Shared contract v1 qualification, freeze and adoption

2026-10-04. **Root accepted the unchanged shared v1 freeze at qualified Alloy owner
`81cbb410d34ff5fba1f3d54cfd2e7ebccaed397e`; canonical publication is complete.**
[`contracts-edge-0.9.11`](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.11)
at `390edbd5b2485af0988e02f7827fde778d76ae0a` marks `ferrum.diagnostic_report` v1
and `ferrum.service_manifest` v1 EXISTING/implemented. This branch now adopts
[`contracts-edge-0.9.12`](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.12)
at `31f0a21d707795be293d15837c2f77c3d84219d8` and verified Edge v0.9.12;
its new hosted qualification remains pending.
No producer wire fields, bounds, enums, reader rules or fixture payloads change.
The only generated snapshot changes are the known Edge release/source headers.

## Current adoption (2026-10-05)

The canonical v0.9.12 release was published at 13:58:38 UTC after its sole
main PUSH [Validate contracts run 37320780987](https://github.com/ferrum-edge/ferrum-contracts/actions/runs/37320780987)
succeeded. All 16 existing adopted paths match that immutable Git tree. The
report and all 12 diagnostic fixtures are byte-identical to the v0.9.11 pin;
the diagnostic-ref schema refreshes only provenance. The report's original
v0.9.11 coordinated-release metadata and owner-unreleased status remain intact.
The two gateway vocabularies refresh Edge provenance, and admin ETag/If-Match
wording adds deployment-v1. Alloy consumes no new admin deployment surface or
schema. The [inventory](edge-contract-inventory.md#revisions-inspected) records
the static re-audit and unchanged data-plane/trust/lifecycle boundaries.

The preceding bot head `39e9404` passed both release-specific Edge jobs in
[run 37328114541](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37328114541)
but failed all three OS test jobs and feature combinations because both
canonical vocabularies still named v0.9.11. Its separate PR workflow ran no
jobs (`action_required`). The pin repair requires fresh whole-head hosted
qualification; those earlier passing subsets and upstream publication do not
qualify the repaired commit. Historical freeze/publication evidence follows.

## Qualified inputs

The [implementation ledger](implementation-status.md#cross-repository-dependencies)
records hosted gates, merge commits and consumer limits for these immutable inputs:

| Consumer | Qualified head | Scope |
|---|---|---|
| [Anvil #312](https://github.com/ferrum-edge/ferrum-anvil/pull/312) | [`591cb734`](https://github.com/ferrum-edge/ferrum-anvil/tree/591cb7343dc2cac3a3b540cdc7ba4dd7f2826c0d) | Merged bounded read-only diagnostic importer; all 27 canonical fixtures plus the real Alloy golden from `0c260f5379939ff46d681666bfbcd65b8518b08d`, with native Linux/macOS/Windows qualification. |
| [Foundry #540](https://github.com/ferrum-edge/ferrum-foundry/pull/540) | [`ea322e9f`](https://github.com/ferrum-edge/ferrum-foundry/tree/ea322e9f584885c09e59fcbbbe255148754b476c) | Merged authenticated, namespace-authorized, redacted manifest preview with strict shared-fixture tests and gateway admission coverage. |
| [Nexus #519](https://github.com/ferrum-edge/ferrum-nexus/pull/519) | [`77fdb767`](https://github.com/ferrum-edge/ferrum-nexus/tree/77fdb767ec8ef04e88f13df9fb291bc77fbd0344) | Merged strict manifest preview and unreleased MCP subsets; all 11 checks across both PR workflows passed, including all eight required Actions checks; zero review threads. Root recorded whole-scope and fresh independent reviews. |
| [GitForgeOps #461](https://github.com/ferrum-edge/ferrum-edge-git-forge-ops/pull/461) | [`e06f986d`](https://github.com/ferrum-edge/ferrum-edge-git-forge-ops/tree/e06f986dfeabb9bcb0c546c74b646f01e1c2a932) | Merged validation of two actual Alloy-generated resource trees, not direct manifest JSON adoption or production apply. |

The diagnostic importer exists and is qualified. Existing manifest fields now have
agreement through two qualified strict consumer fixture suites:
[Foundry](https://github.com/ferrum-edge/ferrum-foundry/blob/ea322e9f584885c09e59fcbbbe255148754b476c/server/routes/service-manifest.test.ts)
and [Nexus](https://github.com/ferrum-edge/ferrum-nexus/blob/77fdb767ec8ef04e88f13df9fb291bc77fbd0344/server/src/test/service-manifest.test.ts).
Both consume canonical r2 fixtures; consumer presentation bounds do not redefine
the producer contract. The current Alloy producer's v1 wire fields and bounds remain
unchanged at the [qualified owner source](https://github.com/ferrum-edge/ferrum-alloy/tree/81cbb410d34ff5fba1f3d54cfd2e7ebccaed397e).
Its sole applicable main PUSH [CI 37238543236](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37238543236)
passed all 18 checks/jobs. Root accepted the owner qualification and unchanged v1
freeze after full owner review and fresh independent review with no findings.
Those gates qualify that immutable owner, not this later adoption commit.

## Authority and release limits

Foundry's [accepted presentation ADR](https://github.com/ferrum-edge/ferrum-foundry/blob/ea322e9f584885c09e59fcbbbe255148754b476c/docs/adr/0001-alloy-authenticated-presentation.md)
preserves the boundary: no new diagnostics backend or trace store, no production
apply, URL fetch, TLS-file read or diagnostic import. Anvil's preview remains
unverified/unknown. Service reports and manifest previews grant no confirmation
authority; only a bound authenticated Edge-admin G01 record may confirm its own
recorded facts under [ADR 0009](adr/0009-authenticated-edge-diagnostic-lookup.md), never timing attribution.

`contracts-edge-0.9.9-r2` at
[`591c73a3`](https://github.com/ferrum-edge/ferrum-contracts/tree/591c73a3f965fdab440c3a76b2707accdf491ba5)
and its pins/PROPOSED annotations remain historical released bytes. Their older
incomplete-consumer wording does not negate the qualified inputs above. The new
[canonical adoption record](https://github.com/ferrum-edge/ferrum-contracts/blob/390edbd5b2485af0988e02f7827fde778d76ae0a/docs/adoption.md)
binds those same four slices and qualified Alloy owner. The owner's
[pairing test](https://github.com/ferrum-edge/ferrum-alloy/blob/81cbb410d34ff5fba1f3d54cfd2e7ebccaed397e/crates/ferrum-alloy-edge/tests/pairing.rs)
compares the entire diagnostic-report schema outside `$id` and `x-contract`,
**including descriptions**.

The canonical [PR #13](https://github.com/ferrum-edge/ferrum-contracts/pull/13)
merged at 22:40:08 UTC with final reviewed head
`0cf926686f2164ad0b4de7b27e2eb5a25df6a261` as its second parent. Final-head
[CI 37240041628](https://github.com/ferrum-edge/ferrum-contracts/actions/runs/37240041628)
and exact merge/main PUSH [CI 37240886730](https://github.com/ferrum-edge/ferrum-contracts/actions/runs/37240886730)
passed; the canonical tag/release was published at 22:41:21 UTC.

Canonical `x-contract` carries the EXISTING/implemented decision with owner
`availability: unreleased`. Report descriptions outside `$id`/`x-contract`
still contain the historical PROPOSED and untested-import text copied from owner
81cbb. They are exact historical annotations, not a reversal of current status.
The tagged source's prepared/pending-publication wording records its earlier
state; the actual GitHub release above records completed publication. Vendored
bytes are never manually rewritten, and parity still includes every description.
The manifest schema remains a transcription of owner structs and validation;
post-default bounds, derived IDs and endpoint relationships remain owner rules.

1. **Adoption candidate (this branch):** pin all 16 adopted files byte-exact from
   the canonical tag, update every checksum and local report status metadata,
   and pair the Edge baseline with `v0.9.12` at
   `0d917701b63ef38210c49df830f48cf0457cbc7d`, default image index
   `sha256:80526b59cbbdc2bfcc8bae9241da4e5395414cf07bf0be4effd4c73c51684ee4`.
   Keep v0.9.11 as the previous release under the existing latest-plus-previous policy.
2. **Fresh hosted adoption gates (root):** require formatting, full pairing/schema
   parity, strict fixtures, HTTP lookup and both Edge matrix jobs on the new Alloy
   commit; qualified owner/canonical CI does not establish this branch's CI result.
   Preserve immutable earlier consumer slices and qualify each new consumer pin
   separately, including strict manifest/cross-store/HTTP boundaries.
3. **Tracker disposition (root):** record all qualified coordinated adoptions before
   deciding #27/#28. This candidate does not close either issue or grant performance,
   production-apply, trace-store or separate crate publishing acceptance.

Alloy #27 and #28 remain open for root disposition. This accepted shared freeze does not grant
the separate owner publishing approval required by [#24](release.md#open-owner-decisions).
