# Contracts

| Path | Contract | Status |
|---|---|---|
| `diagnostics/diagnostic-report.v1.schema.json` | `ferrum.diagnostic_report` v1, JSON Schema 2020-12 | Implemented by `ferrum-alloy-diagnostics`; **PROPOSED** as a shared Ferrum contract (Anvil import not tested) |
| `fixtures/reports/*.json` | Reports exercising rules r001–r004, r006, and r007, a forged `verified` claim, a newer minor version (1.1), and an unsupported major version (2.0) | Used by `crates/ferrum-alloy-diagnostics/tests` and the CLI tests |
| `fixtures/reports/*.expected.txt` | Deterministic rendering snapshots | Regenerate with `UPDATE_SNAPSHOTS=1` and review the diff |
| `fixtures/otlp/*.jsonl` | OTLP/JSON trace exports (Collector `file` exporter format) with Ferrum Edge and Alloy spans | Importer tests |
| `fixtures/manifests/*.toml` | `ferrum.service_manifest` v1 examples | **PROPOSED**; no consumer outside Alloy |
| `fixtures/manifests/*.edge.yaml` | Generated Ferrum Edge file-mode configuration | Snapshots; regenerate with `UPDATE_SNAPSHOTS=1`. CI validates `plain-http.edge.yaml` with `ferrum-edge validate` on every supported Edge release (v0.9.8 and v0.9.7). |

A parity test (`crates/ferrum-alloy-diagnostics/tests/schema_parity.rs`) fails when the Rust enums and the JSON Schema disagree.
