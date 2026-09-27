# Implementation status

This page records what exists and what evidence backs it. "Tested" means an automated test in this repository exercises the behavior. Local results are from macOS arm64 on 2026-09-26. Hosted CI results are in the CI checks and are not restated here.

Everything is pre-release. No crate is published (`publish = false` everywhere).

## Implemented and tested

| Area | What exists | Evidence |
|---|---|---|
| Configuration | Typed, strict TOML and `FERRUM_ALLOY_*` environment variables, builder overrides, documented precedence, `Secret` redaction, syntax and schema errors that never quote configuration values, feature-aware validation | `crates/ferrum-alloy/tests/config.rs`, `ferrum-alloy check` CLI tests |
| Problem Details | RFC 9457 bodies for framework errors (400, 404, 405, 413, 415, 422, 503 overload/timeout/draining, 403 gateway-required, 401/403/503 auth), Problem-returning `Json`, `Path`, `Query`, `ValidJson` extractors. Application bodies are not rewritten. | `app_core.rs` |
| Health | Liveness, cached single-flight readiness with per-check timeouts, draining state, detailed health behind the management token | `app_core.rs` |
| Limits | Body, header, connection, and admission limits; first-request deadline on every connection and protocol, on both listeners; time-to-headers deadline that never cuts streams | `app_core.rs`, `server_hardening.rs` |
| Lifecycle | SIGTERM/Ctrl-C draining with a budget and forced close; pending TLS handshakes are abandoned; serving returns only after every connection socket is closed; readiness flips to draining first | `app_core.rs`, `server_hardening.rs`; against the built binary, `examples/minimal/tests/sigterm.rs` (real SIGTERM, Unix only) and `examples/minimal/tests/ctrl_c.rs` (real Ctrl-C: SIGINT on Unix, a console `CTRL_C_EVENT` on Windows) |
| Request telemetry | Request ids, W3C trace context accepted only from trusted peers, route-template labels, accounting that ends when the response body ends, exactly once (completed, aborted, errored, dropped) | `ferrum-alloy-telemetry/tests/lifecycle.rs`, `context_and_trust.rs`, `axum_adoption.rs`; `ferrum-alloy/tests/connection_reset.rs` resets a raw client socket mid-request-body and mid-response-body against a running app and asserts the `cancelled` outcome, exactly one finalization, and no request left in flight; the `features` CI job lints every target and runs the tests with no features and with each telemetry feature alone |
| Transport trust | `PeerInfo`, verified-leaf `TlsPeer` (SPIFFE URI SAN, DNS SAN), CIDR network trust with `0.0.0.0/0` refused, no header-derived trust | `x509_identity.rs`, `gateway_mtls.rs` |
| Metrics | Bounded Prometheus text output with loss counters | `lifecycle.rs`, `app_core.rs` |
| OTLP export (`otel`) | Explicit pipeline, bounded span processor with counted drops, no global provider installed by constructors, conflict detection with an existing global subscriber | `otel_export.rs`, `telemetry_conflict.rs` |
| TLS (`tls`) | rustls with the `ring` provider passed explicitly, optional or required client auth against a configured CA | `gateway_mtls.rs` |
| Ferrum Edge (`edge`) | `standalone`, `gateway_preferred`, `gateway_required` modes; `GatewayContext`; consumer identity accepted only from a verified identity; manifest; file-mode and GitForgeOps YAML export | `ferrum-alloy-edge/tests/{export,policy,pairing}.rs`, e2e |
| Real gateway e2e | Ferrum Edge v0.9.7 (pinned digest) → Alloy over HTTPS + mTLS, Collector, 19 checks | `examples/edge-observability/src/bin/edge_e2e.rs`: **passed locally** (all 19). CI job `edge-e2e`: see the CI checks. |
| Generated Edge configuration | Validated by the real `ferrum-edge validate` for both TLS and plain fixtures | CI job `edge-config`; passed locally |
| PostgreSQL (`postgres`) | SQLx pool, readiness check, instrumented pool wait, migrations, scheme and runtime checks | `postgres.rs`: the migrations test passed locally against Postgres 17. Fault-injection tests run only in the CI `postgres` job (not run locally): an exhausted one-connection pool records `alloy.db.pool_wait_ms` apart from the operation duration and fails with `PoolTimedOut` within `acquire_timeout_ms`; a `pg_sleep` call behind a real request dominates time to headers, and the imported trace yields `alloy.service.operation_dominates` (r002, placed in the header phase, `likely`) whose `does_not_prove` names database server execution time; `pg_terminate_backend` mid-query fails the call as an error and the pool replaces the connection without losing its permit. The database tests are ignored without `FERRUM_ALLOY_TEST_DATABASE_URL` |
| OpenAPI (`openapi`) | utoipa document serving (management by default, public opt-in), CLI export with drift detection | `optional_layers.rs`, CLI tests |
| JWT (`jwt`) | JWKS verification, fail-closed policy, single-flight rate-limited refresh, bounded key-set lifetime (`Cache-Control` aware) with revalidation of known `kid`s, bounded stale-if-error window, `503` on JWKS outage, `Authorize` | `jwt.rs` (17 tests) |
| HTTP client (`http-client`) | Instrumented CLIENT spans, allow-listed trace propagation, same-origin redirects only, no automatic retries | `http_client.rs` (6 tests; request timeouts and refused connections use separate clients, and a refusal must surface as `ConnectionRefused` on every platform) |
| CORS, compression | Off by default; compression skips streams, `no-store`, and `set-cookie` responses | `optional_layers.rs` |
| Diagnostics | Versioned report schema (JSON Schema in `contracts/diagnostics/`), bounded offline parser, rules r001–r008, OTLP/JSON import, deterministic rendering | `ferrum-alloy-diagnostics/tests/*` including `schema_parity.rs`. Rules r005 (streaming) and r008 (incomplete body) have no test or fixture yet. |
| CLI | `new` (with `openapi`, `otel`, `edge`, `tls`, `postgres`, `jwt`, and `http-client`), `check`, `openapi export`, `edge export`, `diagnose`, `version` with stable exit codes | `ferrum-alloy-cli/tests/cli.rs` (15 tests); the generator test (ignored by default; the CI `generator` job runs it) builds plain, `openapi`, `postgres`, `jwt`, `http-client`, and combined projects and runs `cargo test`, `clippy -D warnings`, and `fmt --check` in each. Generated database tests are ignored unless `TEST_DATABASE_URL` is set; the generated CI workflow runs them against a service container, and so does the `generator` job, with its own PostgreSQL service container. |
| Fuzz and property tests | Property tests (`proptest`) for offline diagnosis (never `confirmed`, deterministic rules and rendering), trace-context and request-id parity with Ferrum Edge v0.9.7, exactly-once body finalization with the implied outcome, and configuration errors that never repeat values. cargo-fuzz targets for `parse_traceparent`/`validate_tracestate`, `RequestId::parse`, `parse_offline`, `otlp::import`, and `config::load_from`, seeded from `contracts/fixtures`. See [testing.md](testing.md). | `*/tests/properties.rs`, `ferrum-alloy/tests/config_properties.rs` (CI `test` job); `fuzz/` (CI `fuzz-smoke` job, 60 s per target). Not run locally. Longer fuzzing runs are manual. |
| Existing-Axum adoption | `ferrum-alloy-telemetry` alone as a Tower layer | `examples/existing-axum` test |

## Partial

| Area | State |
|---|---|
| Benchmarks | A harness exists (`examples/bench`). Local, same-host, interleaved runs on a shared machine measured small-payload HTTP/1.1 throughput and latency, and a before/after comparison of span-record batching; see [benchmarks.md](benchmarks.md). Large payloads, h2c, TLS, CPU time, memory, allocations, and a healthy Collector were not validly measured. No regression budget is enforced in CI. |
| Platforms | Only macOS arm64 was run locally. Linux x86_64, macOS, and Windows results come from hosted CI. |
| Ferrum Edge versions | Only v0.9.7 is tested. Other releases are unverified. |
| Service manifest | `ferrum.service_manifest` v1 is PROPOSED. Edge does not consume it. |
| Anvil interoperability | The finding shape is a superset of Anvil's `DiagnosticFinding`. Anvil's diagnostic reference G01 is PROPOSED only; no Anvil import exists. |

## Not implemented

- A live diagnostics endpoint. `diagnose` is offline and file-based only.
- Certificate revocation (CRL/OCSP) checks for client certificates.
- TLS certificate hot reload.
- Rate limiting on the management listener (keep it on loopback or behind network policy).
- Scheduled long-running fuzzing. CI runs a 60-second smoke run per target; longer runs are manual (see [testing.md](testing.md)).
- Service-side HTTP/3, gRPC tooling, and WebSocket message tracing.
- Tenant or namespace authorization (application responsibility by design).
- Publishing to crates.io, and any release process.

## Cross-repository dependencies

| Repository | Dependency | State |
|---|---|---|
| Ferrum Edge | Contracts verified against v0.9.7 source (`8fed1346`); image pinned by digest | Pinned. Moving versions follows the rules in [compatibility.md](compatibility.md). |
| Ferrum Edge | Consuming the service manifest, emitting per-attempt CLIENT spans or `Server-Timing` | Not present upstream. Alloy makes no claims that depend on them. |
| Ferrum Anvil | G01 diagnostic reference | PROPOSED; no code dependency |
| GitForgeOps | `kind` + `spec` resource wrapper for `edge export --format gitforgeops` | Output shape only; not applied in any test |
