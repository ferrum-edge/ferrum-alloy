# ADR 0001: Workspace crates and dependency boundaries

**Status:** Accepted (2026-09-26)

## Context

Alloy has to be adoptable in two ways:

- **Existing Axum applications:** telemetry only, keeping their runtime, subscriber, and router.
- **New applications:** full defaults.

Offline tooling, such as `ferrum-alloy diagnose` and possibly other products, must read diagnostic evidence without an HTTP stack. A minimal service must not compile database drivers, JWT stacks, exporters, or gateway code.

## Decision

Five crates, with dependencies flowing one way:

```text
ferrum-alloy-diagnostics   serde only: schema, parser, rules, OTLP import
        ▲
ferrum-alloy-telemetry     http/tower/tracing; optional axum, subscriber, x509, otel
        ▲
ferrum-alloy-edge          telemetry + diagnostics; optional axum
        ▲ (feature `edge`)
ferrum-alloy               AlloyApp, config, server, batteries (all optional features)
        ▲
ferrum-alloy-cli           umbrella (no features) + edge + diagnostics
```

- Telemetry never depends on the umbrella. The Edge adapter never depends on the umbrella.
- `ferrum-alloy-diagnostics` is the one crate beyond the four originally planned. It exists because offline diagnosis needs no async runtime, and because the schema should stay light enough for other products to reuse.
- Every integration is an additive feature: `otel`, `edge`, `tls`, `postgres`, `openapi`, `jwt`, `http-client`, `compression`, `cors`. The default is none. CI checks each feature alone, `--no-default-features`, and `full`, and fails if the minimal build pulls in sqlx, jsonwebtoken, utoipa, reqwest, opentelemetry, rustls, or the Edge adapter.
- Configuration types are always compiled, so enabling a section without its feature is reported rather than ignored.
- All crates are `publish = false` and licensed PolyForm Noncommercial 1.0.0 with dual commercial licensing, matching Ferrum Edge.

## Consequences

- The CLI links the umbrella crate (and axum) to validate configuration exactly as startup does.
- Adding a crate requires a real boundary, like the one diagnostics has.
