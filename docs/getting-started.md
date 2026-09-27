# Getting started

Ferrum Alloy is not published to crates.io. Depend on it by git revision, or by path from a checkout. Pin a commit for reproducible builds. Rust 1.94 or newer is required.

```toml
[dependencies]
ferrum-alloy = { git = "https://github.com/ferrum-edge/ferrum-alloy", rev = "<commit>" }
```

## Path 1: a new service

### With the CLI

```bash
cargo install --git https://github.com/ferrum-edge/ferrum-alloy ferrum-alloy-cli
ferrum-alloy new orders-api --with openapi
cd orders-api
cargo test
cargo run
```

The generated project contains:

- ordinary Axum handlers (`src/lib.rs`);
- `alloy.toml`;
- a service manifest for gateway configuration (`ferrum-service.toml`);
- handler tests;
- a README;
- a GitHub Actions workflow.

`--with` takes a comma-separated list of `openapi`, `otel`, `edge`, `tls`, `postgres`, `jwt`, and `http-client`, which become Cargo features of the dependency. Some also add code:

| Option | Adds |
|---|---|
| `openapi` | An `openapi` binary and a parity test |
| `postgres` | `src/db.rs` (`POST /notes`, `GET /notes/{id}`) over a pool built from `[database]`, the `postgres` readiness check, `migrations/`, a `migrate` subcommand, tests, and a PostgreSQL service container in the generated CI |
| `jwt` | `src/auth.rs` (`GET /me`) with a verifier built from `[auth.jwt]` and an `Authorize` scope policy, and tests that sign tokens with a local key and serve its JWKS on loopback |
| `http-client` | `src/upstream.rs` (`GET /upstream`, calling `UPSTREAM_URL`) with the client built from `[http_client]`: explicit timeouts, no redirects, and an empty trace-propagation allow-list, and a test against a local server |

Options combine. CI generates a plain project, one each with `openapi`, `postgres`, `jwt`, and `http-client`, and one with all four, builds them, runs their tests (the database tests against a PostgreSQL service container), and runs `clippy -D warnings` and `rustfmt --check` on them. CI does not generate projects with `otel`, `edge`, or `tls`, which only add Cargo features; the workspace CI checks those features on the library.

The `postgres` starter never migrates implicitly. `cargo run -- migrate` applies the embedded migrations and exits; run it once per release, before the new version serves traffic. `database.migrate_on_startup = true` migrates at every start instead, which is for local development only. Queries are checked at runtime, so building needs neither a database nor `.sqlx/` offline metadata. Only the tests that need a database do: they are ignored unless you set `TEST_DATABASE_URL` and pass `--include-ignored`, which the generated CI workflow does against a PostgreSQL service container.

The `jwt` starter's `alloy.toml` sets the JWKS lifetime explicitly, using the defaults: `jwks_max_age_ms = 300000` (a shorter `Cache-Control: max-age` wins), `jwks_max_stale_ms = 300000`, and `jwks_min_refresh_interval_ms = 60000`. Replace the example issuer, audience, and JWKS URL, or set `FERRUM_ALLOY_JWT_ISSUER`, `FERRUM_ALLOY_JWT_AUDIENCES`, and `FERRUM_ALLOY_JWT_JWKS_URL`.

The generated `Cargo.toml` follows the `main` branch by default. Pass `--alloy-rev <40-character commit>` to pin a commit, or `--alloy-path <checkout>/crates/ferrum-alloy` to use a local checkout.

`ferrum-alloy new` refuses non-empty or symlinked targets, validates the name, never overwrites files, and downloads nothing. Cargo fetches dependencies when you build.

### By hand

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

This is `examples/minimal`. With no configuration it provides:

| Surface | Default |
|---|---|
| Application listener | `127.0.0.1:8080`. Bind `0.0.0.0:8080` explicitly in containers (`FERRUM_ALLOY_BIND`). |
| `/livez`, `/readyz` on the application listener | Status only |
| Management listener | `127.0.0.1:9090`: `/livez`, `/readyz`, `/health`, `/metrics` |
| Errors | RFC 9457 Problem Details for unmatched routes, 405, oversized bodies, timeouts, overload, and panics |
| Limits | 2 MiB bodies, 100 headers / 64 KiB request head, 10 s head read, 30 s to response headers, 10 000 connections |
| Telemetry | JSON logs with request ids and trace ids; one access event per request; Prometheus metrics |
| Shutdown | SIGTERM/SIGINT: readiness reports `draining`, accepting stops, in-flight requests and streams get 30 s, then telemetry flushes |
| Off | CORS, compression, public OpenAPI, OTLP export, TLS, gateway trust |

Handlers stay plain Axum: `State<T>`, `FromRef`, `Json<T>`, `Path<T>`, `Query<T>`, `Extension<T>`, layers, and services all work. Supply state with `Router::with_state` before passing the router. For Problem Details on extractor failures, use `ferrum_alloy::extract::{Json, Path, Query, ValidJson}`. Raw axum extractors keep axum's own plain-text rejections.

### Configuration and resources

```rust
let mut app = AlloyApp::new("orders-api")
    .version(env!("CARGO_PKG_VERSION"))
    .config_file("alloy.toml");
let config = app.prepare()?; // validates config, initializes telemetry
let pool = ferrum_alloy::postgres::connect(&config.database, "orders-api")?;
app.router(routes(pool.clone()))
    .readiness_check("postgres", ferrum_alloy::postgres::readiness(pool))
    .run()
    .await?;
```

See [configuration.md](configuration.md) for precedence, every key, and every `FERRUM_ALLOY_*` variable. To validate without starting:

```bash
ferrum-alloy check --config alloy.toml --features postgres
```

### The escape hatch

`AlloyApp::into_parts()` returns the composed `axum::Router`, the management router, the `Lifecycle` handle, the validated config, and a `TelemetryGuard`. Serve the router however you like, but keep the parts alive: dropping the guard flushes and stops trace export.

```rust
let parts = AlloyApp::new("svc").router(router).into_parts()?;
let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
axum::serve(listener, parts.router.clone().into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
```

Alloy's own server (`AlloyParts::serve`) adds header limits, connection limits, TLS identity, and bounded draining. `axum::serve` does not provide those.

## Path 2: an existing Axum application

Add only the telemetry crate and keep your runtime, subscriber, state, middleware, and server. This is `examples/existing-axum`.

```toml
[dependencies]
ferrum-alloy-telemetry = { git = "https://github.com/ferrum-edge/ferrum-alloy", rev = "<commit>" }
```

```rust
use ferrum_alloy_telemetry::{RecordRouteLayer, TelemetryConfig, TelemetryLayer};
use tower::Layer;

let router = Router::new()
    .route("/hello/{name}", get(visit))
    .with_state(state)
    .layer(my_middleware)
    .layer(RecordRouteLayer); // records the route template after matching

let telemetry = TelemetryLayer::new(TelemetryConfig::default())?;
let metrics = telemetry.metrics(); // render with metrics.render_prometheus()
let app = Router::new().fallback_service(telemetry.layer(router)); // outermost

axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
```

Handlers can take `ferrum_alloy_telemetry::RequestContext` (request id, trace id, span id, trust, trace decision). Tests that call handlers without the telemetry layer build one with `RequestContext::new(trace_id, span_id, sampled)`, an untrusted root context, rather than a struct literal: new fields may be added. Nothing installs a global subscriber. To export traces, compose `ferrum_alloy_telemetry::otel::OtelPipeline::layer()` into your own subscriber (feature `otel`).

To trust a gateway's trace context, pass a classifier:

```rust
let peers = TrustedPeers::new(&TrustedPeersConfig {
    identities: vec!["spiffe://example.org/ns/edge/sa/gateway".into()],
    networks: vec![],
})?;
let telemetry = TelemetryLayer::new(config)?.with_classifier(Arc::new(peers));
```

If you terminate TLS yourself, insert `PeerInfo` with `TlsPeer::from_verified_leaf(leaf_der)` (feature `x509`) for certificates your TLS stack verified.

## Behind Ferrum Edge

1. Generate the gateway configuration from a service manifest, and review it:

   ```bash
   ferrum-alloy edge export --manifest ferrum-service.toml --output edge.yaml
   ferrum-edge validate -m file -c edge.yaml
   ```

   Or write a GitForgeOps tree with `--format gitforgeops --output DIR`. Nothing is applied to a gateway.

2. Give Edge a client certificate with a SPIFFE URI SAN (`upstream.gateway_client_cert_path` in the manifest), and configure Alloy:

   ```toml
   [server.tls]
   cert_path = "/certs/alloy.pem"
   key_path = "/certs/alloy.key"
   client_ca_path = "/certs/ca.pem"
   client_auth = "optional"

   [trust]
   identities = ["spiffe://example.org/ns/edge/sa/gateway"]

   [edge]
   mode = "gateway_required"
   accept_consumer_identity = true
   ```

   Build with `features = ["tls", "edge"]`, and add `"otel"` for trace export. Handlers can take `Option<ferrum_alloy::edge::GatewayContext>` for Edge's authenticated consumer.

3. `examples/edge-observability` runs Edge v0.9.7, Alloy, and an OpenTelemetry Collector together:

   ```bash
   docker compose -f examples/edge-observability/compose.yaml up -d --build
   cargo run -p example-edge-observability --bin edge-e2e -- \
     --compose examples/edge-observability/compose.yaml --out target/e2e
   ferrum-alloy diagnose --otlp target/e2e/traces.jsonl --list-traces
   ```

## Diagnosing a request

```bash
ferrum-alloy diagnose --otlp traces.jsonl --trace-id <id>
ferrum-alloy diagnose --input report.json --format json
```

Diagnosis is offline and deterministic. It explains only the supplied evidence, and file input is never treated as authenticated. See [measurement-semantics.md](measurement-semantics.md) for what each timing means.

## Feature matrix

| Feature | Adds | Main dependencies |
|---|---|---|
| (none) | App builder, config, problems, health, limits, server, telemetry, metrics | axum, hyper, hyper-util, tokio, tower-http (catch-panic), tracing-subscriber |
| `otel` | OTLP/HTTP trace export with a bounded processor | opentelemetry 0.33, opentelemetry-otlp, tracing-opentelemetry 0.34, reqwest (blocking), rustls |
| `tls` | rustls listener, client-certificate identity | rustls (ring), tokio-rustls, x509-parser |
| `edge` | Ferrum Edge trust modes, consumer identity handoff | ferrum-alloy-edge |
| `postgres` | SQLx pool, readiness, measured acquisition, migrations | sqlx 0.9 (postgres, rustls/ring) |
| `openapi` | Serve a registered utoipa document; re-exports utoipa and utoipa-axum | utoipa 6, utoipa-axum 0.3 |
| `jwt` | JWT/JWKS verification and an authorization hook (implies `http-client`) | jsonwebtoken 11 (rust_crypto) |
| `http-client` | Instrumented outbound client | reqwest 0.13 (rustls/ring), rustls-platform-verifier |
| `compression` | gzip/br, never for SSE, `Set-Cookie`, or `no-store` responses | tower-http |
| `cors` | Explicit allowlist CORS | tower-http |
| `full` | All of the above | — |
