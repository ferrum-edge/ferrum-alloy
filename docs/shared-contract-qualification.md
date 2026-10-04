# Shared contract v1 qualification and freeze proposal

2026-10-04. **Proposed owner decision for root review; not finalized or published.**
Propose a root-coordinated freeze of the current `ferrum.diagnostic_report` v1 and
`ferrum.service_manifest` v1 wire contracts as the **next canonical release step**,
pending a matching qualified Alloy owner commit, canonical documentation/status
metadata and hosted CI. This record changes no producer wire fields, validation
bounds, schema descriptions, pins or fixtures.

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
unchanged from the [inspected owner source](https://github.com/ferrum-edge/ferrum-alloy/tree/d7ddb3688e058ec3cc2e17d166a801aa0037b5b1).

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
incomplete-consumer wording does not negate the qualified inputs above or establish
a shipped freeze. The owner's [pairing test](https://github.com/ferrum-edge/ferrum-alloy/blob/d7ddb3688e058ec3cc2e17d166a801aa0037b5b1/crates/ferrum-alloy-edge/tests/pairing.rs)
compares the entire diagnostic-report schema outside `$id` and `x-contract`,
**including descriptions**.

1. **Review and qualify the owner commit:** root reviews this proposal and the
   matching Alloy commit's hosted gates; earlier green heads do not qualify it.
2. **Record and release canonically:** coordinate the adoption record, qualified
   immutable inputs and status/annotation metadata in ferrum-contracts, retain
   wire fields/bounds, pass hosted CI, then release a canonical tag.
3. **Adopt the released tag together:** update the owner pin and matching local
   schema annotations, including descriptions, in the same change with hosted CI.
   Preserve full parity; do not weaken it to bypass an annotation mismatch.

Alloy #27 and #28 remain open for root disposition. This proposal does not grant
the separate owner publishing approval required by [#24](release.md#open-owner-decisions).
