# Contracts

| Path | Contract | Status |
|---|---|---|
| `diagnostics/diagnostic-report.v1.schema.json` | `ferrum.diagnostic_report` v1, JSON Schema 2020-12 | Implemented by `ferrum-alloy-diagnostics`; vendored and pinned from `contracts-edge-0.9.21` under `ferrum-contracts/`; shared status **EXISTING**/implemented with root's accepted unchanged v1 freeze. Anvil's read-only importer is merged and qualified in [#312](https://github.com/ferrum-edge/ferrum-anvil/pull/312). |
| `ferrum-contracts/schemas/diagnostic-ref/v1.schema.json` | `ferrum.diagnostic_ref` v1, JSON Schema 2020-12 | Vendored and pinned from `contracts-edge-0.9.21` (byte-identical to `contracts-edge-0.9.17`); Alloy validates and binds authenticated lookup records per ADR 0009 |
| `ferrum-contracts/fixtures/diagnostic-ref/{valid,invalid}/*.json` | Ten selected released lookup record fixtures | Byte-exact from `contracts-edge-0.9.21`, unchanged from r2; parser, binding, and hosted HTTP fixture tests |
| `ferrum-contracts/fixtures/service-manifest/{valid,invalid}/*.json` | The shared `ferrum.service_manifest` v1 fixtures | Byte-exact from `contracts-edge-0.9.21`; `pinned_service_manifest_fixtures_match_validate` in `crates/ferrum-alloy-edge/tests/pairing.rs` requires `ServiceManifest` to accept every valid fixture and refuse every invalid one. `contracts-edge-0.9.21` moved `valid/orders-api.json` from an `http://otel-collector` OTLP endpoint, which Edge drops, to `https://` (ferrum-contracts#82), so no valid fixture is excepted any more. The tag's newer service-manifest fixtures are not vendored. |
| `fixtures/reports/*.json` | Reports exercising rules r001–r004, r006, and r007 (including an observed Ferrum Edge diagnostic reference), a forged `verified` claim, a newer minor version (1.1), and an unsupported major version (2.0) | Used by `crates/ferrum-alloy-diagnostics/tests` and the CLI tests; `fixtures/reports/gateway-diagnostic-ref.json` is a candidate for upstreaming to ferrum-contracts |
| `fixtures/reports/*.expected.txt` | Deterministic rendering snapshots | Regenerate with `UPDATE_SNAPSHOTS=1` and review the diff |
| `fixtures/otlp/*.jsonl` | OTLP/JSON trace exports (Collector `file` exporter format) with Ferrum Edge and Alloy spans | Importer tests |
| `fixtures/manifests/*.toml` | `ferrum.service_manifest` v1 examples | **EXISTING**/implemented with root's accepted unchanged v1 freeze published in `contracts-edge-0.9.11`; fixture comments retain historical PROPOSED text. Foundry [#540](https://github.com/ferrum-edge/ferrum-foundry/pull/540) and Nexus [#519](https://github.com/ferrum-edge/ferrum-nexus/pull/519) have merged, qualified authenticated previews as of 2026-10-04. |
| `fixtures/manifests/*.edge.yaml` | Generated Ferrum Edge file-mode configuration | Snapshots; regenerate with `UPDATE_SNAPSHOTS=1` in hosted CI. CI validates `plain-http.edge.yaml` and `minimal.edge.yaml` (no `gateway.otel_endpoint`, so its `otel_tracing` plugin is propagation-only) with `ferrum-edge validate` on every supported Edge release (v0.9.21 and v0.9.20), and serves `minimal.edge.yaml` in front of a header-echo backend to check that a caller's `traceparent`/`tracestate` never reach it. `orders-api.toml` and its snapshot export to `https://otel-collector:4318`, because Edge v0.9.19 drops a cleartext OTLP exporter to a host that is not a loopback IP literal. |
| `fixtures/openapi/orders-api.{toml,input.json}` | A manifest with the `[agents]` section and a utoipa-style document declaring agent tools (`x-ferrum-mcp`) | `[agents]` is included in the shared service-manifest schema as of ferrum-contracts #8 (`contracts-edge-0.9.9-r2`); the extension is unchanged in supported Edge v0.9.21 and v0.9.20 |
| `fixtures/openapi/orders-api.openapi.json` | What `openapi export` writes for them | Snapshot; regenerate with `UPDATE_SNAPSHOTS=1`. CI submits it to the real `POST /api-specs` on every supported Edge release (`edge-config` job). |

A parity test (`crates/ferrum-alloy-diagnostics/tests/schema_parity.rs`) fails when the Rust enums and the JSON Schema disagree.

Owner and consumer qualification, the shared v1 freeze and the adoption history
(#27 and #28 are closed) are recorded in the
[owner/adoption record](../docs/shared-contract-qualification.md) and the
[consumer evidence ledger](../docs/implementation-status.md#cross-repository-dependencies).

## Ferrum contracts pin

`ferrum-contracts/` vendors the gateway vocabularies, diagnostic-report and
diagnostic-ref schemas, and diagnostic-finding, diagnostic-ref, and service-manifest fixtures from the tag recorded
in `ferrum-contracts/PIN`: `contracts-edge-0.9.21` at
`92d8da5be93ea1ece9f47729b1f7aa10ae8a7c3f` (SSH-signed tag object
`3e7e10ef6d4aefa9874319326943ec2186b29981`). Both shared v1 contracts are now
EXISTING/implemented; their metadata records Alloy owner availability as
`unreleased`, the provenance of the original freeze, which the vendored bytes
keep even though Alloy has since been released as `v0.1.0`. The
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
All 12 diagnostic fixtures remain byte-identical. Canonical deployment
schemas/fixtures are outside this 16-file scope and grant Alloy no deployment
capability. (The pin has since grown to 28 files: #319 vendored the 12
`service-manifest` fixtures of the `contracts-edge-0.9.20` tag, and the PIN has selected the same 28 files since.) All descriptions outside `$id`/`x-contract`, including historical
PROPOSED/incomplete-import wording, remain exactly paired with the qualified owner. Current status is in `x-contract`; the
tagged metadata's prepared/pending-publication text records the pre-publication
state, superseded by the [actual release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.11)
published at 22:41:21 UTC on 2026-10-04. Never rewrite canonical-owned bytes or
weaken full schema parity to change those descriptions. This adoption updates the
pin and local metadata together. The current
[contracts-edge-0.9.21 release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.21),
published on 2026-11-22, changes four of the 28 pinned files. The two gateway
vocabularies change their Edge provenance (`edge_release` v0.9.21), move each
header's `availability` to the first published Edge release that has it, and
drop the removed `circuit_breaker` rejection-phase alias of
`circuit_breaker_open` (Edge #7158); the nine tokens, their meanings, the 19
`ErrorClass` values and every header name, role and direction are unchanged.
`fixtures/service-manifest/valid/orders-api.json` and
`invalid/client-cert-without-key.json` move their OTLP endpoint to
`https://otel-collector:4318/v1/traces` (ferrum-contracts#82). The
diagnostic-ref and diagnostic-report schemas and all 12 vendored diagnostic
fixtures are byte-identical to `contracts-edge-0.9.20`. The tag's new
service-manifest and diagnostic-ref fixtures, and its other schemas and
vocabularies, are outside the PIN selection. The
[contracts-edge-0.9.20 release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.20)
it supersedes, published on 2026-11-03, again changed only the two gateway vocabularies, and
only in Edge provenance (`edge_release` v0.9.20), Edge's notes, a new
`docs/size_limits.md` source and the `protocol_error` `ErrorClass` meaning (a
gRPC backend failure seen at or after the client deadline settles as that
deadline). Its nine tokens, their meanings and every header entry are
unchanged, and the diagnostic-ref and diagnostic-report schemas and all 12
vendored fixtures are byte-identical to `contracts-edge-0.9.19`. The
[contracts-edge-0.9.19 release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.19)
it supersedes changed only the two gateway vocabularies; the diagnostic-ref and
diagnostic-report schemas and all 12 vendored fixtures are byte-identical to
`contracts-edge-0.9.17` (there is no `contracts-edge-0.9.18`: Edge v0.9.18 was
never published). Its gateway-errors vocabulary keeps the nine tokens and says
`connection_failure` also arrives with `504` (connect timeout) or `503` (a
pooled-transport DestinationRule `maxConnections` refusal before dialing),
and adds a client-abort terminal source. The gateway-headers vocabulary adds `x-ferrum-peer-spiffe-id`
and the mesh ext-authz `x-ferrum-source-*`/`x-ferrum-request-principal`
headers, none consumed by Alloy. The
[contracts-edge-0.9.17 release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.17)
it supersedes was published on 2026-10-10. Its gateway-errors vocabulary adds the ninth
client-facing token, `loop_detected`, and clarifies the timeout meanings; the
diagnostic-ref schema adds `loop_detected` to both its `gateway_error` and
`rejection_phase` enums, and the gateway-headers vocabulary adds the `X-Ferrum-Hops` assertion (not consumed by
Alloy). The `contracts-edge-0.9.15` release that one superseded confirmed the eight
client-facing tokens unchanged and added the released
`X-Authenticated-Identity` assertion.

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
