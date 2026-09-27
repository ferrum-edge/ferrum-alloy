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
- Each can be supplied as `FERRUM_ALLOY_<NAME>_FILE=/path`. Trailing `\n` and `\r` characters are trimmed.
- Setting both `<NAME>` and `<NAME>_FILE` is an error.
- Syntax and schema (type) errors never quote values. A TOML syntax error reports the file, line, column, and parser message, without the source excerpt. A schema error reports the key path and the expected type or variants, never the supplied value. Both apply to startup errors and to `ferrum-alloy check` in human and JSON output. For a syntax error, the JSON output also has `location.line` and `location.column`. Semantic validation errors, reported after parsing, may quote non-secret values, such as an unaccepted algorithm name or a bind address; they never quote secrets.

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
| `FERRUM_ALLOY_IDLE_TIMEOUT_MS` | `server.idle_timeout_ms` | integer |
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
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_ENABLED` | `management.rate_limit.enabled` | bool |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_REQUESTS_PER_SECOND` | `management.rate_limit.requests_per_second` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_BURST` | `management.rate_limit.burst` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_GLOBAL_REQUESTS_PER_SECOND` | `management.rate_limit.global_requests_per_second` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_GLOBAL_BURST` | `management.rate_limit.global_burst` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_PROBE_REQUESTS_PER_SECOND` | `management.rate_limit.probe_requests_per_second` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_PROBE_BURST` | `management.rate_limit.probe_burst` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_MAX_CLIENTS` | `management.rate_limit.max_clients` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_IPV6_PREFIX_LEN` | `management.rate_limit.ipv6_prefix_len` | integer |
| `FERRUM_ALLOY_MANAGEMENT_RATE_LIMIT_EXEMPT_NETWORKS` | `management.rate_limit.exempt_networks` | comma list of CIDRs (empty for none) |
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
| `FERRUM_ALLOY_JWT_JWKS_MAX_AGE_MS` | `auth.jwt.jwks_max_age_ms` | integer |
| `FERRUM_ALLOY_JWT_JWKS_MAX_STALE_MS` | `auth.jwt.jwks_max_stale_ms` | integer |
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
| `header_read_timeout_ms` | `10000` | Time to receive a request head (slow-header protection). It also bounds the time from a ready connection (accepted, and past the TLS handshake if any) to its first complete request head, whatever the protocol: a connection that sends nothing, stops inside the HTTP/2 preface, or opens HTTP/2 without sending a request is closed and frees its `max_connections` slot. An established HTTP/2 connection is sent `GOAWAY` first and dropped after a grace period of at most one second. These closes are counted in `ferrum_alloy_first_request_timeouts_total` (application listener). On HTTP/1.1 keep-alive connections it also bounds the wait for the next request. The management listener uses the same value. |
| `idle_timeout_ms` | `60000` | After a connection's first request: how long it may stay open with no request in flight. A request counts as in flight from its request head until its response body has been sent or dropped, so slow handlers and long response streams (SSE) are never idle. HTTP/2 keep-alive pings do not count as activity, so a peer that sends one request and then only answers pings cannot keep its `max_connections` slot. An HTTP/2 connection is sent `GOAWAY` and dropped after the same grace period as above; a request that raced the `GOAWAY` gets up to `shutdown.drain_timeout_ms` to finish. These closes are counted in `ferrum_alloy_idle_timeouts_total` (application listener). HTTP/1.1 keep-alive connections are normally closed first by `header_read_timeout_ms`. A response stalled on flow control (the peer withholds HTTP/2 `WINDOW_UPDATE` or keeps a zero TCP receive window) still counts as in flight, so this timeout does not close its connection ([#46](https://github.com/ferrum-edge/ferrum-alloy/issues/46)). Behind a load balancer or proxy that pools connections, set it above that proxy's idle timeout, so the proxy closes idle connections first and never sends a request on one Alloy is closing. The management listener uses the same value. Must be greater than zero. |
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
| `drain_timeout_ms` | `30000` | After accepting stops: time for in-flight requests and response streams. Remaining connections are then force-closed and counted in `ferrum_alloy_force_closed_connections_total`. HTTP/2 stream tasks (a request's handler and its response body) still running are cancelled, which drops the handler, and counted in `ferrum_alloy_force_closed_streams_total`. Serving returns only after every connection socket is closed and every aborted connection task and cancelled stream task has finished unwinding, so no HTTP/2 handler is still running. |
| `telemetry_flush_timeout_ms` | `5000` | Bound on the final span flush. |

### `[management]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Serve the management listener. |
| `bind` | `127.0.0.1:9090` | Must differ from `server.bind`. A non-loopback bind **requires** `token`. |
| `token` | none | Bearer token (at least 32 characters) for `/health`, `/metrics`, and the OpenAPI document. `/livez` and `/readyz` stay unauthenticated. |

#### `[management.rate_limit]`

Every request to the management listener is rate-limited before any handler or token check runs, so failed token attempts count too. Requests are charged to one of two budgets: **probes** (`/livez` and `/readyz`) and **endpoints** (every other path, including unknown ones). Each budget has its own client table and lock, so traffic to one budget never throttles or slows the other, and kubelet and Edge health probes keep working while `/metrics` is saturated.

An endpoint request is charged to a token bucket of its client and one of the whole listener. It is admitted only when both hold a token, and a rejected request consumes neither, so one client over its limit cannot drain the listener budget for the others. A probe is charged to its client's bucket only: there is no listener-wide probe budget, so no set of sources can use up `/livez` for everyone else. A rejected request gets `429` Problem Details (`tag:ferrumedge.com,2026:alloy/problem/rate-limited`) with `Retry-After` (whole seconds until a token is available) and `Cache-Control: no-store`.

A client is the transport peer address of the connection, never a header such as `X-Forwarded-For`. IPv4-mapped IPv6 addresses count as IPv4. Other IPv6 addresses are keyed by their first `ipv6_prefix_len` bits, because one host usually controls a whole /64. Lower it (down to 48) when one tenant controls more, and raise it (up to 128) when distinct clients share a /64: in Kubernetes, pod addresses of one node often come from a single /64, and behind NAT64 many IPv4 clients arrive from one /96 prefix.

Peers in `exempt_networks` bypass the limits entirely; the default is empty, so loopback peers are limited too. Kubelet probes the pod from its node's address, so add the node network (for example `10.244.0.0/16`, or the node CIDR of your cluster) only when exempting those probes is intended; otherwise a refused probe can restart the pod. Behind a proxy or sidecar, every client is the proxy's address: Istio connects to the application from `127.0.0.6`, and all proxied clients share that address's budgets. Add the sidecar address (for example `127.0.0.6/32`) only if bypassing the limits for every proxied client is intended. IPv4-mapped IPv6 CIDRs are rejected; write their IPv4 equivalent. A network of every address (`0.0.0.0/0`, `::/0`) is refused; set `enabled = false` instead.

Each client table holds at most `max_clients` clients. A client gets an entry only when one of its requests is admitted, so rejected requests never take room. A client whose bucket has refilled completely is indistinguishable from a new one and is forgotten: each request examines the entry examined longest ago, and a newcomer that finds the table full examines a few more. The work per request is bounded, and a table is never swept as a whole. While the endpoint table is full of clients that are still spending their budget, further clients share one per-client budget. While the probe table is full, probes from further clients are served untracked rather than refused. When you serve `AlloyParts::management_router` yourself, insert `PeerInfo` or axum `ConnectInfo`; without either, every request is one unknown client and all of them share one budget.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Enforce the limits. When `false`, the other keys are not validated. |
| `requests_per_second` | `10` | Sustained endpoint requests per second from one client. |
| `burst` | `20` | Endpoint requests one client may send at once. |
| `global_requests_per_second` | `100` | Sustained endpoint requests per second from all clients together. |
| `global_burst` | `200` | Endpoint requests all clients together may send at once. |
| `probe_requests_per_second` | `20` | Sustained probe requests per second from one client. |
| `probe_burst` | `40` | Probe requests one client may send at once. |
| `max_clients` | `1024` | Clients tracked individually per budget, `1` to `65536`, and at least `global_burst`. |
| `ipv6_prefix_len` | `64` | Leading bits of an IPv6 address that identify one client, `48` to `128`. |
| `exempt_networks` | `[]` | Peer networks that bypass rate limits. |

Every rate and burst must be greater than zero when `enabled` is `true`. Rejections are counted in `ferrum_alloy_management_rate_limited_total{budget,scope}` on `/metrics`, where `scope` names the empty bucket: `client` (the client's own), `shared` (the one for requests without a transport address and, for endpoints, for clients beyond `max_clients`), or `global` (the listener's, endpoints only). `ferrum_alloy_management_rate_limit_clients{budget}` is the number of clients currently tracked, and `ferrum_alloy_management_rate_limit_untracked_probes_total` counts probes served untracked because the probe table was full. Exempt requests are not counted. The application listener, including its own `/livez` and `/readyz`, is not affected.

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
| `request_id.echo_in_response` | `true` | Echo the id on responses that shared caches cannot store. A bare GET 200 or 404 is heuristically cacheable and gets no echo unless the application marks it `Cache-Control: private` or `no-store` (see [security](security.md#response-headers-and-caches)). |
| `trace_context.accept_incoming` | `trusted_peers` | Whose `traceparent` becomes the parent. Others are re-rooted. |
| `trace_context.link_untrusted_parent` | `false` | Link a re-rooted untrusted context as a span link. |
| `server_timing` | `disabled` | `Server-Timing: alloy;dur=…` on responses shared caches cannot store, under the same rule as the request id echo: `trusted_peers` or `always`. |
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
| `algorithms` | `["RS256"]` | Any of `RS256`, `RS384`, `RS512`, `PS256`, `PS384`, `PS512`, `ES256`, `ES384`, `EdDSA`. `none` and HMAC are rejected. |
| `jwks_url` | required | `https`, or `http` to loopback only |
| `jwks_min_refresh_interval_ms` | `60000` | Rate limit for JWKS fetches, whatever triggers them (expiry or unknown `kid`). Also the shortest key-set lifetime. |
| `jwks_max_age_ms` | `300000` | Longest time a fetched key set is trusted before it is revalidated, even for known `kid`s. Must be greater than zero, at least `jwks_min_refresh_interval_ms`, and at most 24 hours (`86400000`). |
| `jwks_max_stale_ms` | `300000` | How long an expired key set keeps verifying known `kid`s while it is revalidated in the background or while refreshes fail. After that, requests get `503 auth-unavailable`. `0` fails closed as soon as the key set expires. At most 24 hours (`86400000`). |
| `jwks_max_bytes` | `262144` | JWKS response size bound |
| `jwks_timeout_ms` | `5000` | JWKS request timeout |
| `leeway_seconds` | `30` | Clock skew for `exp`/`nbf` |

Key-set lifetime: a JWKS response's `Cache-Control: max-age` (minus its `Age` header) sets the lifetime, bounded below by `jwks_min_refresh_interval_ms` and above by `jwks_max_age_ms`. `no-cache` and `no-store` count as `max-age=0`. Without `max-age`, the lifetime is `jwks_max_age_ms`. The lifetime is measured from when the fetch started. A key removed from the JWKS stops verifying within `jwks_max_age_ms` of the last successful fetch while the JWKS is reachable (plus one fetch, bounded by `jwks_timeout_ms`, when `jwks_max_stale_ms` is nonzero), and within `jwks_max_age_ms + jwks_max_stale_ms` in the worst case.

Refreshes never hold up requests that can be answered from the cache. While the key set is stale, a request with a known `kid` verifies against it at once and starts a background refresh. Requests wait for a refresh only when the key set has expired, has never been fetched, or lacks the token's `kid`. A refresh runs in its own task, so a request that is cancelled while waiting does not cancel it. Keys from a successful refresh are used even if the fetch took longer than their lifetime.

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

This runs the configuration validation that startup runs, without binding sockets or reading TLS files. It prints each capability as enabled or disabled, then warnings and errors, and exits `3` when the configuration is invalid.

- `--features otel,tls,...` names the features your build compiles. By default every feature is assumed, so only feature-independent problems fail.
- `--no-env` ignores `FERRUM_ALLOY_*` variables in the current environment.
- `--show-effective` prints the merged configuration with secrets redacted.
- `--format json` prints the result as JSON.
