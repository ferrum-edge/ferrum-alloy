# Contracts

| Path | Contract | Status |
|---|---|---|
| `diagnostics/diagnostic-report.v1.schema.json` | `ferrum.diagnostic_report` v1, JSON Schema 2020-12 | Implemented by `ferrum-alloy-diagnostics`; vendored and pinned from `contracts-edge-0.9.9-r2` under `ferrum-contracts/`; Anvil's read-only importer is merged and qualified in [#312](https://github.com/ferrum-edge/ferrum-anvil/pull/312). Shared status remains **PROPOSED** pending the canonical qualification record and freeze review. |
| `ferrum-contracts/schemas/diagnostic-ref/v1.schema.json` | `ferrum.diagnostic_ref` v1, JSON Schema 2020-12 | Vendored and pinned from `contracts-edge-0.9.9-r2`; Alloy validates and binds authenticated lookup records per ADR 0009 |
| `ferrum-contracts/fixtures/diagnostic-ref/{valid,invalid}/*.json` | Ten released lookup record fixtures | Byte-exact from `contracts-edge-0.9.9-r2`; parser, binding, and hosted HTTP fixture tests |
| `fixtures/reports/*.json` | Reports exercising rules r001–r004, r006, and r007 (including an observed Ferrum Edge diagnostic reference), a forged `verified` claim, a newer minor version (1.1), and an unsupported major version (2.0) | Used by `crates/ferrum-alloy-diagnostics/tests` and the CLI tests; `fixtures/reports/gateway-diagnostic-ref.json` is a candidate for upstreaming to ferrum-contracts |
| `fixtures/reports/*.expected.txt` | Deterministic rendering snapshots | Regenerate with `UPDATE_SNAPSHOTS=1` and review the diff |
| `fixtures/otlp/*.jsonl` | OTLP/JSON trace exports (Collector `file` exporter format) with Ferrum Edge and Alloy spans | Importer tests |
| `fixtures/manifests/*.toml` | `ferrum.service_manifest` v1 examples | **PROPOSED** at the released r2 tag; Foundry [#540](https://github.com/ferrum-edge/ferrum-foundry/pull/540) and Nexus [#519](https://github.com/ferrum-edge/ferrum-nexus/pull/519) have merged, qualified authenticated previews as of 2026-10-04. Canonical qualification and coordinated freeze remain the next release step. |
| `fixtures/manifests/*.edge.yaml` | Generated Ferrum Edge file-mode configuration | Snapshots; regenerate with `UPDATE_SNAPSHOTS=1`. CI validates `plain-http.edge.yaml` with `ferrum-edge validate` on every supported Edge release (v0.9.10 and v0.9.9). |
| `fixtures/openapi/orders-api.{toml,input.json}` | A manifest with the `[agents]` section and a utoipa-style document declaring agent tools (`x-ferrum-mcp`) | `[agents]` is included in the shared service-manifest schema as of ferrum-contracts #8 (`contracts-edge-0.9.9-r2`); the extension is supported by Edge v0.9.9 and v0.9.10 |
| `fixtures/openapi/orders-api.openapi.json` | What `openapi export` writes for them | Snapshot; regenerate with `UPDATE_SNAPSHOTS=1`. CI submits it to the real `POST /api-specs` on every supported Edge release (`edge-config` job). |

A parity test (`crates/ferrum-alloy-diagnostics/tests/schema_parity.rs`) fails when the Rust enums and the JSON Schema disagree.

The [consumer evidence ledger](../docs/implementation-status.md#cross-repository-dependencies)
records immutable heads, hosted gates and consumer boundaries. Anvil qualification includes
the shared fixtures and a real hosted Alloy exporter golden; its preview grants no trust or
confidence authority. Foundry and Nexus qualification covers authenticated, namespace-authorized,
redacted previews, without production apply. Both strict consumers test the shared manifest
fixtures. GitForgeOps separately qualifies generated resource trees. The
[proposed owner qualification/freeze decision](../docs/shared-contract-qualification.md)
awaits root review and a matching qualified owner commit before canonical release;
the current tag's diagnostic-report/service-manifest annotations remain **PROPOSED**.

## Ferrum contracts pin

`ferrum-contracts/` vendors the gateway vocabularies, diagnostic-report and
diagnostic-ref schemas, and diagnostic-finding and diagnostic-ref fixtures from the tag recorded
in `ferrum-contracts/PIN`. The diagnostic-report schema's shared status is still PROPOSED. The
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

A future status/annotation release must land in a canonical tag first, then update the
owner pin and matching local schema annotations together with hosted CI. Existing r2
annotations and pins record historical released bytes; do not silently weaken schema parity
to update their status. See the [proposed coordinated release steps](../docs/shared-contract-qualification.md).

## Service manifest schema

ferrum-contracts publishes `schemas/service-manifest/v1.schema.json` (PROPOSED),
transcribed from `manifest.rs`, with `additionalProperties: false` at the top
level. The shared schema includes the optional `[agents]` section (`enabled`,
`endpoint_path`, `namespace`) as of ferrum-contracts #8, released in
`contracts-edge-0.9.9-r2`. Alloy does not vendor the service-manifest schema;
Alloy's own parser accepts manifests with or without `[agents]`.
