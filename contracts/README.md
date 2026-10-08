# Contracts

| Path | Contract | Status |
|---|---|---|
| `diagnostics/diagnostic-report.v1.schema.json` | `ferrum.diagnostic_report` v1, JSON Schema 2020-12 | Implemented by `ferrum-alloy-diagnostics`; vendored and pinned from `contracts-edge-0.9.15` under `ferrum-contracts/`; shared status **EXISTING**/implemented with root's accepted unchanged v1 freeze. Anvil's read-only importer is merged and qualified in [#312](https://github.com/ferrum-edge/ferrum-anvil/pull/312). |
| `ferrum-contracts/schemas/diagnostic-ref/v1.schema.json` | `ferrum.diagnostic_ref` v1, JSON Schema 2020-12 | Vendored and pinned from `contracts-edge-0.9.15`; Alloy validates and binds authenticated lookup records per ADR 0009 |
| `ferrum-contracts/fixtures/diagnostic-ref/{valid,invalid}/*.json` | Ten selected released lookup record fixtures | Byte-exact from `contracts-edge-0.9.15`, unchanged from r2; parser, binding, and hosted HTTP fixture tests |
| `fixtures/reports/*.json` | Reports exercising rules r001–r004, r006, and r007 (including an observed Ferrum Edge diagnostic reference), a forged `verified` claim, a newer minor version (1.1), and an unsupported major version (2.0) | Used by `crates/ferrum-alloy-diagnostics/tests` and the CLI tests; `fixtures/reports/gateway-diagnostic-ref.json` is a candidate for upstreaming to ferrum-contracts |
| `fixtures/reports/*.expected.txt` | Deterministic rendering snapshots | Regenerate with `UPDATE_SNAPSHOTS=1` and review the diff |
| `fixtures/otlp/*.jsonl` | OTLP/JSON trace exports (Collector `file` exporter format) with Ferrum Edge and Alloy spans | Importer tests |
| `fixtures/manifests/*.toml` | `ferrum.service_manifest` v1 examples | **EXISTING**/implemented with root's accepted unchanged v1 freeze published in `contracts-edge-0.9.11`; fixture comments retain historical PROPOSED text. Foundry [#540](https://github.com/ferrum-edge/ferrum-foundry/pull/540) and Nexus [#519](https://github.com/ferrum-edge/ferrum-nexus/pull/519) have merged, qualified authenticated previews as of 2026-10-04. |
| `fixtures/manifests/*.edge.yaml` | Generated Ferrum Edge file-mode configuration | Snapshots; regenerate with `UPDATE_SNAPSHOTS=1` in hosted CI. CI validates `plain-http.edge.yaml` with `ferrum-edge validate` on every supported Edge release (v0.9.15 and v0.9.14); the new pairing awaits adoption CI. |
| `fixtures/openapi/orders-api.{toml,input.json}` | A manifest with the `[agents]` section and a utoipa-style document declaring agent tools (`x-ferrum-mcp`) | `[agents]` is included in the shared service-manifest schema as of ferrum-contracts #8 (`contracts-edge-0.9.9-r2`); the extension is unchanged in supported Edge v0.9.15 and v0.9.14 |
| `fixtures/openapi/orders-api.openapi.json` | What `openapi export` writes for them | Snapshot; regenerate with `UPDATE_SNAPSHOTS=1`. CI submits it to the real `POST /api-specs` on every supported Edge release (`edge-config` job). |

A parity test (`crates/ferrum-alloy-diagnostics/tests/schema_parity.rs`) fails when the Rust enums and the JSON Schema disagree.

The [consumer evidence ledger](../docs/implementation-status.md#cross-repository-dependencies)
records immutable heads, hosted gates and consumer boundaries. Anvil qualification includes
the shared fixtures and a real hosted Alloy exporter golden; its preview grants no trust or
confidence authority. Foundry and Nexus qualification covers authenticated, namespace-authorized,
redacted previews, without production apply. Both strict consumers test the shared manifest
fixtures. GitForgeOps separately qualifies generated resource trees. The
[accepted owner qualification and adoption record](../docs/shared-contract-qualification.md)
records qualified owner `81cbb410`, canonical publication and this adoption candidate.
Fresh hosted owner/consumer adoption checks and root disposition remain for #27/#28;
the earlier qualified consumer slices are not relabeled as new-tag qualification.

## Ferrum contracts pin

`ferrum-contracts/` vendors the gateway vocabularies, diagnostic-report and
diagnostic-ref schemas, and diagnostic-finding and diagnostic-ref fixtures from the tag recorded
in `ferrum-contracts/PIN`: `contracts-edge-0.9.15` at
`6fb64c5dc2e014204c17609fc717d976f3b4589e`. Both shared v1 contracts are now
EXISTING/implemented; Alloy owner availability remains unreleased. The
vendored files are byte-verified and never edited or line-ending converted
(`.gitattributes` marks them `-text`). The pairing tests in
`crates/ferrum-alloy-edge/tests/pairing.rs` verify every vendored file's
SHA-256, compare Alloy's gateway error tokens and released gateway diagnostic
headers with the pinned vocabularies, compare the entire diagnostic-report schema with the pin
except `$id` and `x-contract` (including all descriptions), and
check the shared Finding fixtures against Alloy's schema and `Finding` type.

The pinned token meanings are Edge's release-specific wording. Alloy never
renders them: rule `alloy.r007` renders the version-neutral explanations in
`catalog::EDGE_GATEWAY_ERROR_TOKENS`, because a header names no Edge version
and a token's meaning must never be narrowed. The pairing test records the
pinned meanings separately so that a pin bump that changes one fails until
Alloy's explanations are re-reviewed.

To bump the pin, select a released `contracts-edge-*` tag, download the adopted
files from that tag into the same paths, update the tag, commit SHA, and file
hashes in `ferrum-contracts/PIN` and the pinned tag/commit assertion in
`crates/ferrum-alloy-edge/tests/pairing.rs`, then run CI. Update local
vocabulary copies to match and review any reported drift, including the
recorded pinned meanings.

The adopted report is byte-identical to the v0.9.11 pin and differs from r2 only
in `x-contract`. Its `coordinated_release.contracts_tag` records the original
v0.9.11 freeze, while local `contracts_tag`/`contracts_commit` track this adoption.
The diagnostic-ref schema changes only provenance; all 12 diagnostic fixtures
remain byte-identical. X-Gateway-Error token meanings, diagnostic header
availability and reference grammar are unchanged; admin ETag/If-Match descriptions
add the released deployment profile. Its new schemas/fixtures are outside this same
16-file scope and grant Alloy no deployment capability. All descriptions outside
`$id`/`x-contract`, including historical PROPOSED/incomplete-import wording, remain
exactly paired with the qualified owner. Current status is in `x-contract`; the
tagged metadata's prepared/pending-publication text records the pre-publication
state, superseded by the [actual release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.11)
published at 22:41:21 UTC on 2026-10-04. Never rewrite canonical-owned bytes or
weaken full schema parity to change those descriptions. This adoption updates the
pin and local metadata together. The current
[contracts-edge-0.9.15 release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.15)
was published at 20:42:07 UTC on 2026-10-08. Its gateway-errors vocabulary
confirms the eight client-facing tokens are unchanged; the gateway-headers
vocabulary adds the released `X-Authenticated-Identity` assertion.

## Service manifest schema

ferrum-contracts publishes `schemas/service-manifest/v1.schema.json` (EXISTING/implemented),
transcribed from `manifest.rs`, with `additionalProperties: false` at the top
level. The shared schema includes the optional `[agents]` section (`enabled`,
`endpoint_path`, `namespace`) as of ferrum-contracts #8, released in
`contracts-edge-0.9.9-r2`, retained unchanged in `contracts-edge-0.9.15`. Alloy does
not vendor the service-manifest schema; Alloy's own parser accepts manifests with
or without `[agents]`. The canonical schema is a transcription, not an owner-exported
schema: post-default validation, derived resource ID lengths and endpoint relationships
still belong to the owner parser. Consumer presentation bounds do not redefine them.
