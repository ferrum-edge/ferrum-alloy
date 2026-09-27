# Ferrum Alloy

**Production-ready Axum. Connected to the Edge.**

Ferrum Alloy is a batteries-included toolkit for Rust API services built on [Axum](https://docs.rs/axum). It removes repetitive production setup — configuration, errors, health, limits, graceful shutdown, request telemetry — while leaving handlers, extractors, state, routers, and Tower middleware as ordinary Axum.

It works on its own. Behind [Ferrum Edge](https://ferrumedge.com/) it gains a verified, shared request story: which gateway span a service span belongs to, whether the gateway's identity was cryptographically verified, and what the evidence does and does not prove about where time went.

> **Status: in development (pre-release).** Nothing is published to crates.io. APIs and contracts may change. See [docs/implementation-status.md](docs/implementation-status.md) for exactly what is implemented and tested.

```rust
use axum::{Router, routing::get};
use ferrum_alloy::AlloyApp;

async fn hello() -> &'static str {
    "Hello from Ferrum Alloy"
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new().route("/hello", get(hello));

    AlloyApp::new("hello-api").router(router).run().await?;

    Ok(())
}
```

## What you get

- **Configuration**: typed and strict, with documented precedence (builder > `FERRUM_ALLOY_*` > TOML > defaults) and redacted secrets. See [docs/configuration.md](docs/configuration.md).
- **Errors**: RFC 9457 Problem Details for framework errors and Problem-returning extractors. Application bodies are never rewritten.
- **Health**: minimal liveness, cached single-flight readiness, draining state, and detailed health on a token-protected management listener.
- **Limits and lifecycle**:
  - body, header, connection, and admission limits;
  - a deadline on time to response headers that never cuts SSE streams;
  - SIGTERM draining with a budget and forced close.
- **Truthful telemetry**:
  - request ids;
  - W3C trace context accepted only from trusted transport peers;
  - route-template metrics;
  - accounting that ends when the response *body* ends, exactly once;
  - bounded OTLP export with counted loss.
- **Optional batteries** (Cargo features): `otel`, `tls` (rustls, verified client identity), `edge`, `postgres` (SQLx), `openapi` (utoipa), `jwt` (JWKS), `http-client`, `compression`, `cors`.
- **CLI** (`ferrum-alloy`):
  - `new`: starters that compile and pass their own tests;
  - `check`: configuration validation;
  - `openapi export`: with drift detection;
  - `edge export`: Edge file-mode YAML (checked in CI with `ferrum-edge validate`) or a GitForgeOps tree;
  - `diagnose`: offline, deterministic explanations of evidence.

## Two ways to adopt it

1. **New service**: `ferrum-alloy new orders-api`, or use `AlloyApp` directly.
2. **Existing Axum app**: add only `ferrum-alloy-telemetry`, which provides a Tower layer. You keep your runtime, subscriber, router, and server.

See [docs/getting-started.md](docs/getting-started.md).

## Workspace

| Crate | Purpose |
|---|---|
| `crates/ferrum-alloy` | `AlloyApp`, configuration, server, problems, health, limits, optional batteries |
| `crates/ferrum-alloy-telemetry` | Independently usable Tower/Axum instrumentation, trust classification, OpenTelemetry bridge |
| `crates/ferrum-alloy-edge` | Ferrum Edge contracts, gateway trust modes, service manifest, config export |
| `crates/ferrum-alloy-diagnostics` | Versioned evidence schema, bounded parser, deterministic rules, OTLP/JSON import |
| `crates/ferrum-alloy-cli` | The `ferrum-alloy` command |
| `examples/` | `minimal`, `existing-axum`, `postgres-api`, `edge-observability` (real Edge + Collector), `bench` (overhead harness) |

## Documentation

- [Getting started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Measurement semantics](docs/measurement-semantics.md): what every timing means
- [Security model](docs/security.md)
- [Ferrum Edge contract inventory](docs/edge-contract-inventory.md)
- [Compatibility](docs/compatibility.md): the exact tested matrix
- [Architecture decisions](docs/adr/README.md)
- [Implementation status](docs/implementation-status.md)
- [Testing](docs/testing.md): property tests and fuzz targets for untrusted input
- [Benchmarks](docs/benchmarks.md): local, same-host overhead measurements and their limits

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test -p ferrum-alloy-cli --test cli -- --ignored     # generate, build, and test starter projects
cargo deny check
```

The Ferrum Edge end-to-end stack is described in `examples/edge-observability/compose.yaml`.

## License

PolyForm Noncommercial 1.0.0 ([LICENSE](LICENSE)), with commercial licensing available ([LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)). "Ferrum Alloy" and the crate and command names are working names. Registry and trademark availability has not been verified.
