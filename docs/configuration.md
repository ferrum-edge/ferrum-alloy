# Configuration

Alloy configuration is typed (`ferrum_alloy::config::AlloyConfig`) and validated before anything binds a socket. Invalid configuration stops startup with an error that names the problem. Alloy never quietly falls back to a default.

## Precedence

Highest first:

1. **Builder overrides**: explicit calls such as `AlloyApp::new("name")` (sets `service.name`), `.bind(addr)`, `.management_bind(addr)`, `.version(v)`.
2. **Environment**: the `FERRUM_ALLOY_*` variables listed below.
3. **Configuration file**: TOML, from `AlloyApp::config_file(path)` or else `FERRUM_ALLOY_CONFIG`. The file must exist when named. Without either, no file is read.
4. **Defaults**: shown in the reference below.

`AlloyApp::config(AlloyConfig)` bypasses loading entirely: no file or environment is read.

Merging is per key. A table in a higher layer replaces only the keys it sets.

## Strictness

- Unknown keys and sections are errors (`deny_unknown_fields`).
- Unknown `FERRUM_ALLOY_*` variables are errors, which catches typos.
- Values must parse:
  - integers must be non-negative;
  - booleans are `true`/`false`/`1`/`0`;
  - floats must be finite;
  - socket addresses must be `IP:port` (host names are rejected).
- Every section parses whether or not its Cargo feature is compiled. Enabling a section whose feature is missing is an error, not a no-op. For example, `otlp.enabled = true` without `otel` fails.
- A configuration file is limited to 1 MiB and must be a regular file.

## Secrets

`management.token` and `database.url` are secrets:

- They are held in `Secret`, and `Debug`, `Display`, and serialization print `<redacted>`.
- `ferrum-alloy check --show-effective` and `AlloyConfig::redacted_toml()` never reveal them.
- Each can be supplied as `FERRUM_ALLOY_<NAME>_FILE=/path`; one trailing newline is trimmed.
- Setting both `<NAME>` and `<NAME>_FILE` is an error.

## Environment variables

`FERRUM_ALLOY_CONFIG` names the configuration file. Every other supported variable maps to one configuration key:

| Variable | Key | Type |
|---|---|---|
| `FERRUM_ALLOY_SERVICE_NAME` | `service.name` | string |
| `FERRUM_ALLOY_SERVICE_VERSION` | `service.version` | string |
| `FERRUM_ALLOY_ENVIRONMENT` | `service.environment` | string |
| `FERRUM_ALLOY_BIND` | `server.bind` | `IP:port` |
| `FERRUM_ALLOY_REQUEST_BODY_LIMIT_BYTES` | `server.request_body_limit_bytes` | integer |
| `FERRUM_ALLOY_REQUEST_TIMEOUT_MS` | `server.request_timeout_ms` | integer |
| `FERRUM_ALLOY_HEADER_READ_TIMEOUT_MS` | `server.header_read_timeout_ms` | integer |
| `FERRUM_ALLOY_MAX_CONNECTIONS` | `server.max_connections` | integer |
| `FERRUM_ALLOY_MAX_IN_FLIGHT_REQUESTS` | `server.max_in_flight_requests` | integer |
| `FERRUM_ALLOY_TLS_CERT_PATH` | `server.tls.cert_path` | path |
| `FERRUM_ALLOY_TLS_KEY_PATH` | `server.tls.key_path` | path |
| `FERRUM_ALLOY_TLS_CLIENT_CA_PATH` | `server.tls.client_ca_path` | path |
| `FERRUM_ALLOY_TLS_CLIENT_AUTH` | `server.tls.client_auth` | `none` / `optional` / `required` |
| `FERRUM_ALLOY_SHUTDOWN_READINESS_GRACE_MS` | `shutdown.readiness_grace_ms` | integer |
| `FERRUM_ALLOY_SHUTDOWN_DRAIN_TIMEOUT_MS` | `shutdown.drain_timeout_ms` | integer |
| `FERRUM_ALLOY_MANAGEMENT_ENABLED` | `management.enabled` | bool |
| `FERRUM_ALLOY_MANAGEMENT_BIND` | `management.bind` | `IP:port` |
| `FERRUM_ALLOY_MANAGEMENT_TOKEN` (`_FILE`) | `management.token` | secret |
| `FERRUM_ALLOY_LOG_FORMAT` | `logging.format` | `json` / `pretty` / `compact` |
| `FERRUM_ALLOY_LOG_FILTER` | `logging.filter` | `EnvFilter` directives |
| `FERRUM_ALLOY_OTLP_ENABLED` | `otlp.enabled` | bool |
| `FERRUM_ALLOY_OTLP_ENDPOINT` | `otlp.endpoint` | URL |
| `FERRUM_ALLOY_OTLP_SAMPLING_RATIO` | `otlp.sampling_ratio` | float |
| `FERRUM_ALLOY_OTLP_TIMEOUT_MS` | `otlp.timeout_ms` | integer |
| `FERRUM_ALLOY_TRACE_CONTEXT_ACCEPT` | `telemetry.trace_context.accept_incoming` | `never` / `trusted_peers` / `any` |
| `FERRUM_ALLOY_SERVER_TIMING` | `telemetry.server_timing` | `disabled` / `trusted_peers` / `always` |
| `FERRUM_ALLOY_TRUSTED_IDENTITIES` | `trust.identities` | comma list |
| `FERRUM_ALLOY_TRUSTED_NETWORKS` | `trust.networks` | comma list of CIDRs |
| `FERRUM_ALLOY_EDGE_MODE` | `edge.mode` | `standalone` / `gateway_preferred` / `gateway_required` |
| `FERRUM_ALLOY_DATABASE_URL` (`_FILE`) | `database.url` | secret |
| `FERRUM_ALLOY_DATABASE_MAX_CONNECTIONS` | `database.max_connections` | integer |
| `FERRUM_ALLOY_DATABASE_MIGRATE_ON_STARTUP` | `database.migrate_on_startup` | bool |
| `FERRUM_ALLOY_JWT_ISSUER` | `auth.jwt.issuer` | string |
| `FERRUM_ALLOY_JWT_AUDIENCES` | `auth.jwt.audiences` | comma list |
| `FERRUM_ALLOY_JWT_JWKS_URL` | `auth.jwt.jwks_url` | URL |
| `FERRUM_ALLOY_CORS_ALLOWED_ORIGINS` | `cors.allowed_origins` | comma list |

A test fails when a variable in `config::ENV_VARS` is missing from this table.

### `RUST_LOG`

`RUST_LOG` is read only when `logging.filter` is unset, and then only for the subscriber Alloy installs. An application-owned subscriber is untouched.

### `OTEL_*` variables

Alloy passes its own OTLP settings to the OpenTelemetry SDK programmatically, so the corresponding variables have no effect. The SDK and exporter still read some others:

| Variable | Behavior |
|---|---|
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_ENDPOINT` | Used only when `otlp.endpoint` is unset. The traces variable is used as-is; the general one gets `/v1/traces` appended. Otherwise the exporter default `http://localhost:4318/v1/traces` applies. |
| `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, `OTEL_EXPORTER_OTLP_HEADERS` | Merged into export request headers (for example collector auth). Treat their values as secrets; Alloy never logs them. |
| `OTEL_EXPORTER_OTLP_TRACES_COMPRESSION`, `OTEL_EXPORTER_OTLP_COMPRESSION` | Read by the exporter. `gzip` and `zstd` are not compiled in, so setting either fails startup with a clear error; `none` is accepted. |
| `OTEL_RESOURCE_ATTRIBUTES` | Added to the resource by the SDK. Alloy's `service.name`, `service.version`, `service.instance.id`, and `deployment.environment.name` take precedence. |
| `OTEL_SERVICE_NAME` | Overridden by Alloy's service name. |
| `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TIMEOUT` (and `_TRACES_` variants) | No effect. Alloy always exports OTLP/HTTP protobuf with `otlp.timeout_ms`. |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG`, `OTEL_SDK_DISABLED`, `OTEL_BSP_*` | No effect. Use `otlp.sampling_ratio`, `otlp.enabled`, and the `otlp.max_*` limits. |

## Reference

Each section below shows a key, its default, and its meaning.

### `[service]`

| Key | Default | Meaning |
|---|---|---|
| `name` | builder name | Service name (resource `service.name`) |
| `version` | none | Service version |
| `environment` | `"development"` | Deployment environment label |

### `[server]`

| Key | Default | Meaning |
|---|---|---|
| `bind` | `127.0.0.1:8080` | Application listener. Loopback by default; bind `0.0.0.0:8080` explicitly in containers. |
| `request_body_limit_bytes` | `2097152` | Maximum request body. A declared `Content-Length` over the limit gets `413` before the handler runs; chunked bodies are capped while streaming. |
| `max_header_count` | `100` | Maximum request headers (HTTP/1.1). Exceeding it returns `431`. |
| `max_header_bytes` | `65536` | Request-head buffer (HTTP/1.1) and header list size (HTTP/2). Minimum 8192. |
| `max_connections` | `10000` | Further connections are closed immediately. Upgraded (WebSocket) sessions are not counted. |
| `http2_max_concurrent_streams` | `256` | Per-connection HTTP/2 stream limit. |
| `header_read_timeout_ms` | `10000` | Time to receive a request head (slow-header protection). It also bounds the time from a ready connection (accepted, and past the TLS handshake if any) to its first complete request head, whatever the protocol: a connection that sends nothing, stops inside the HTTP/2 preface, or opens HTTP/2 without sending a request is closed and frees its `max_connections` slot. On HTTP/1.1 keep-alive connections it also bounds the wait for the next request. The management listener uses the same value. |
| `request_timeout_ms` | `30000` | Deadline to produce response **headers**; `503 request-timeout` when exceeded. Never applied to response bodies (SSE) or upgraded sessions. `0` disables it. |
| `max_in_flight_requests` | `0` (unlimited) | Concurrent handler admission limit; excess gets `503 overloaded`. The permit is released when headers are produced. |
| `admission_wait_timeout_ms` | `0` | How long to wait for a permit. `0` rejects immediately. The wait is recorded as `alloy.admission.wait_ms`. |

#### `[server.tls]` (feature `tls`)

| Key | Default | Meaning |
|---|---|---|
| `cert_path` | required | PEM certificate chain |
| `key_path` | required | PEM private key |
| `client_ca_path` | none | CA bundle for client certificates |
| `client_auth` | `none` | `none`, `optional`, or `required` client certificates |
| `handshake_timeout_ms` | `10000` | TLS handshake timeout. Handshakes still in progress when shutdown starts are abandoned. |

### `[shutdown]`

| Key | Default | Meaning |
|---|---|---|
| `readiness_grace_ms` | `0` | After SIGTERM, keep accepting while readiness reports `draining`, so load balancers stop routing first. A second signal skips the grace. |
| `drain_timeout_ms` | `30000` | After accepting stops: time for in-flight requests and response streams. Remaining connections are then force-closed and counted in `ferrum_alloy_force_closed_connections_total`. Serving returns only after every connection is closed. |
| `telemetry_flush_timeout_ms` | `5000` | Bound on the final span flush. |

### `[management]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Serve the management listener. |
| `bind` | `127.0.0.1:9090` | Must differ from `server.bind`. A non-loopback bind **requires** `token`. |
| `token` | none | Bearer token (at least 32 characters) for `/health`, `/metrics`, and the OpenAPI document. `/livez` and `/readyz` stay unauthenticated. |

### `[health]`

| Key | Default | Meaning |
|---|---|---|
| `app_endpoints` | `true` | Also serve liveness and readiness on the application listener, for gateway checks. These take precedence over app routes with the same path. |
| `liveness_path` | `/livez` | Literal path |
| `readiness_path` | `/readyz` | Literal path |
| `cache_ttl_ms` | `5000` | Readiness results are reused for this long, with single-flight refresh. |
| `check_timeout_ms` | `2000` | Per-check timeout |

### `[logging]`

| Key | Default | Meaning |
|---|---|---|
| `format` | `json` | `json`, `pretty`, or `compact` |
| `filter` | `RUST_LOG`, else `info` | `EnvFilter` directives |
| `ansi` | `false` | Colors for text formats |

### `[telemetry]`

| Key | Default | Meaning |
|---|---|---|
| `request_id.header` | `x-request-id` | Correlation header. Must not be a reserved name (`authorization`, `traceparent`, `x-consumer-username`, …). |
| `request_id.accept_incoming` | `any` | Whose incoming ids are kept, after validation: at most 256 bytes of `[A-Za-z0-9._-]`, matching Ferrum Edge's `correlation_id` plugin. |
| `request_id.echo_in_response` | `true` | Echo the id on responses that shared caches cannot store. |
| `trace_context.accept_incoming` | `trusted_peers` | Whose `traceparent` becomes the parent. Others are re-rooted. |
| `trace_context.link_untrusted_parent` | `false` | Link a re-rooted untrusted context as a span link. |
| `server_timing` | `disabled` | `Server-Timing: alloy;dur=…` on non-cacheable responses: `trusted_peers` or `always`. |
| `record.url_path` / `record.client_address` / `record.user_agent` | `false` | Optional span attributes that may carry personal or high-cardinality data. |
| `access_log` | `true` | One `ferrum_alloy::access` event per finalized request. |

### `[otlp]` (feature `otel`)

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Export traces over OTLP/HTTP protobuf. |
| `endpoint` | see `OTEL_*` | Full traces URL. Must be `http(s)` with no credentials in the URL. |
| `timeout_ms` | `10000` | Per-export timeout including retries. |
| `max_export_retries` | `2` | Retries for retryable failures (exponential backoff). |
| `sampling_ratio` | `1.0` | Root trace sampling. Accepted remote parents keep their own decision. |
| `max_queue_spans` | `2048` | Queue bound. Excess spans are dropped and counted (`queue_full`). |
| `max_queue_bytes` | `8388608` | Estimated byte budget. Excess spans are dropped and counted (`byte_budget`). |
| `max_export_batch` | `512` | Spans per request. |
| `max_request_bytes` | `4194304` | Encoded bytes per export request. |
| `scheduled_delay_ms` | `1000` | Export interval. |

### `[trust]`

| Key | Default | Meaning |
|---|---|---|
| `identities` | `[]` | Verified client-certificate identities: `spiffe://…` ids or `dns:<name>`. |
| `networks` | `[]` | Source CIDRs treated as a trusted termination boundary. `0.0.0.0/0` and `::/0` are rejected. |

Trust affects only propagation metadata and gateway-asserted headers. It never authenticates end users. See [security.md](security.md).

### `[edge]` (feature `edge`)

| Key | Default | Meaning |
|---|---|---|
| `mode` | `standalone` | `standalone`, `gateway_preferred`, or `gateway_required`. `gateway_required` needs `trust.identities`; health paths stay reachable. |
| `accept_consumer_identity` | `false` | Expose Edge's `X-Consumer-Username` / `X-Consumer-Custom-Id` as `GatewayContext`, only from a verified mTLS identity. Unverified copies are always removed. |

### `[cors]` (feature `cors`)

Disabled unless `enabled = true`, and nothing is allowed unless listed.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Enable CORS handling. |
| `allowed_origins` | `[]` | Exact origins. `*` is rejected together with credentials. |
| `allowed_methods` | `[]` | Allowed methods. |
| `allowed_headers` | `[]` | Allowed request headers. |
| `allow_credentials` | `false` | Allow credentials. |
| `max_age_seconds` | none | Preflight cache duration. |

### `[compression]` (feature `compression`)

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Off by default. Compressing responses that mix secrets with reflected input enables BREACH-style attacks. When enabled, Alloy never compresses `text/event-stream`, `Set-Cookie`, or `Cache-Control: no-store` responses. |
| `min_size_bytes` | `1024` | Minimum body size to compress. |

### `[openapi]` (feature `openapi`)

| Key | Default | Meaning |
|---|---|---|
| `serve` | `true` | Serve a registered document on the management listener (token-protected). |
| `path` | `/openapi.json` | Literal path. |
| `public` | `false` | Also serve it unauthenticated on the application listener. |

### `[database]` (feature `postgres`)

| Key | Default | Meaning |
|---|---|---|
| `url` | none | Secret connection URL |
| `max_connections` / `min_connections` | `10` / `0` | Pool size |
| `acquire_timeout_ms` | `5000` | Pool wait bound (measured as `alloy.db.pool_wait_ms`) |
| `idle_timeout_ms` / `max_lifetime_ms` | `600000` / `1800000` | Connection recycling |
| `statement_timeout_ms` | none | Server-side `statement_timeout` per connection |
| `migrate_on_startup` | `false` | Run embedded migrations at startup. Prefer a separate step in production. |
| `readiness_check` | `true` | Register a cached `SELECT 1` readiness check |

### `[auth.jwt]` (feature `jwt`)

| Key | Default | Meaning |
|---|---|---|
| `issuer` | required | Required `iss` |
| `audiences` | required | Accepted `aud` values |
| `algorithms` | `["RS256"]` | Asymmetric algorithms only. `none` and HMAC are rejected. |
| `jwks_url` | required | `https`, or `http` to loopback only |
| `jwks_min_refresh_interval_ms` | `60000` | Refresh rate limit (also applied for unknown `kid`s) |
| `jwks_max_bytes` | `262144` | JWKS response size bound |
| `jwks_timeout_ms` | `5000` | JWKS request timeout |
| `leeway_seconds` | `30` | Clock skew for `exp`/`nbf` |

### `[http_client]` (feature `http-client`)

| Key | Default | Meaning |
|---|---|---|
| `connect_timeout_ms` | `2000` | Connection timeout |
| `request_timeout_ms` | `10000` | Whole-request timeout |
| `max_redirects` | `0` | Same-origin redirects only. Cross-origin redirects are never followed. |
| `propagate_trace_context_to` | `[]` | Hosts (exact, or `.suffix`) that receive `traceparent`. Credentials, cookies, and baggage are never forwarded automatically. |

## Checking configuration

```bash
ferrum-alloy check --config alloy.toml
```

This runs the same validation as startup and prints disabled capabilities and warnings. `--show-effective` prints the redacted merged configuration.
