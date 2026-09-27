# Contracts

| Path | Contract | Status |
|---|---|---|
| `diagnostics/diagnostic-report.v1.schema.json` | `ferrum.diagnostic_report` v1, JSON Schema 2020-12 | Implemented by `ferrum-alloy-diagnostics`; **PROPOSED** as a shared Ferrum contract (Anvil import not tested) |
| `fixtures/reports/*.json` | Reports exercising each rule, forged trust, a newer minor version, and an unsupported major version | Used by `crates/ferrum-alloy-diagnostics/tests` and the CLI tests |
| `fixtures/reports/*.expected.txt` | Deterministic rendering snapshots | Regenerate with `UPDATE_SNAPSHOTS=1` and review the diff |
| `fixtures/otlp/*.jsonl` | OTLP/JSON trace exports (Collector `file` exporter format) with Ferrum Edge and Alloy spans | Importer tests |
| `fixtures/manifests/*.toml` | `ferrum.service_manifest` v1 examples | **PROPOSED**; no consumer outside Alloy |
| `fixtures/manifests/*.edge.yaml` | Generated Ferrum Edge file-mode configuration | Validated by `ferrum-edge validate` (v0.9.7) in CI |

A parity test (`crates/ferrum-alloy-diagnostics/tests/schema_parity.rs`) fails when the Rust enums and the JSON Schema disagree.
