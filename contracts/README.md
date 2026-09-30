# Contracts

| Path | Contract | Status |
|---|---|---|
| `diagnostics/diagnostic-report.v1.schema.json` | `ferrum.diagnostic_report` v1, JSON Schema 2020-12 | Implemented by `ferrum-alloy-diagnostics`; vendored and pinned from `contracts-edge-0.9.8` under `ferrum-contracts/`; shared status still **PROPOSED** (Anvil import not tested) |
| `fixtures/reports/*.json` | Reports exercising rules r001–r004, r006, and r007, a forged `verified` claim, a newer minor version (1.1), and an unsupported major version (2.0) | Used by `crates/ferrum-alloy-diagnostics/tests` and the CLI tests |
| `fixtures/reports/*.expected.txt` | Deterministic rendering snapshots | Regenerate with `UPDATE_SNAPSHOTS=1` and review the diff |
| `fixtures/otlp/*.jsonl` | OTLP/JSON trace exports (Collector `file` exporter format) with Ferrum Edge and Alloy spans | Importer tests |
| `fixtures/manifests/*.toml` | `ferrum.service_manifest` v1 examples | **PROPOSED**; no consumer outside Alloy |
| `fixtures/manifests/*.edge.yaml` | Generated Ferrum Edge file-mode configuration | Snapshots; regenerate with `UPDATE_SNAPSHOTS=1`. CI validates `plain-http.edge.yaml` with `ferrum-edge validate` on every supported Edge release (v0.9.8 and v0.9.7). |

A parity test (`crates/ferrum-alloy-diagnostics/tests/schema_parity.rs`) fails when the Rust enums and the JSON Schema disagree.

## Ferrum contracts pin

`ferrum-contracts/` vendors the gateway vocabularies and the shared
diagnostic-report schema and fixtures from the tag recorded in
`ferrum-contracts/PIN`. The schema's shared status is still PROPOSED. The
vendored files are byte-verified and never edited or line-ending converted
(`.gitattributes` marks them `-text`). The pairing tests in
`crates/ferrum-alloy-edge/tests/pairing.rs` verify every vendored file's
SHA-256, compare Alloy's gateway error tokens and released gateway diagnostic
headers with the pinned vocabularies, compare the schema with the pin, and
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
