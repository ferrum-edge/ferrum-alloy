# Ferrum Alloy agent guide

Ferrum Alloy is a Rust API toolkit on Axum with optional Ferrum Edge integration. It is a Cargo workspace (edition 2024, MSRV 1.94, toolchain pinned in `rust-toolchain.toml`), licensed PolyForm Noncommercial 1.0.0 with dual commercial licensing. Every crate is `publish = false`.

## Layout and boundaries

- `crates/ferrum-alloy-diagnostics`: serde-only evidence schema, parser, rules, OTLP import. No HTTP or async dependencies.
- `crates/ferrum-alloy-telemetry`: Tower/Axum instrumentation. Must not depend on the umbrella crate.
- `crates/ferrum-alloy-edge`: Edge adapter. May depend on telemetry and diagnostics, never on the umbrella crate.
- `crates/ferrum-alloy`: `AlloyApp` and batteries. Every integration is an additive, optional feature; the default is none.
- `crates/ferrum-alloy-cli`: the `ferrum-alloy` binary and its embedded project templates.
- `examples/*`: compiled and tested in CI. `edge-observability` runs real Ferrum Edge.

## Before every commit

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
```

Local cargo runs are permitted in this repository; hosted CI remains the source of truth. When templates change, also run `cargo test -p ferrum-alloy-cli --test cli -- --ignored`. When dependencies change, run `cargo deny check`. Docker-based checks, such as the Edge end-to-end stack and PostgreSQL, run in CI.

## Engineering rules

- No `unwrap`/`expect`/`panic` in library code. The workspace lints deny them; tests opt out explicitly.
- Never install a global subscriber, OpenTelemetry provider, or crypto provider from library code. `AlloyApp` may install a subscriber only in `TelemetryInit::Auto` when none exists.
- Trust comes from transport identity (`PeerInfo`), never from headers. Do not add a `trusted = true` switch.
- Do not invent Ferrum Edge contracts. Anything new goes into `docs/edge-contract-inventory.md` marked PROPOSED, until Edge implements it.
- Timing fields must be defined in `docs/measurement-semantics.md` and `crates/ferrum-alloy-diagnostics/src/catalog.rs` before they are exposed. Unknown is not zero. Never sum nested durations. Never call a residual "network latency".
- Diagnosis rules stay deterministic, with no network and no AI. Every finding lists `does_not_prove`. Offline input never yields `confirmed`.
- New `FERRUM_ALLOY_*` variables go in `config::ENV_VARS` and `docs/configuration.md`; a test enforces this.
- Changing the pinned Edge image means updating `docs/compatibility.json`, `docs/compatibility.md`, the CI workflow, the demo Dockerfile, and `contract::EDGE_*`; `crates/ferrum-alloy-edge/tests/pairing.rs` enforces this.
- Snapshots (`*.expected.txt`, `*.edge.yaml`) regenerate with `UPDATE_SNAPSHOTS=1`. Review the diff.

## PRs

Use concise imperative commit messages. PRs need a summary, the changes, and a test plan. Never merge or publish without the owner's approval.
