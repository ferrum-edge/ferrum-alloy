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
- Unknown `FERRUM_ALLOY_*` variables are errors, which catches typos. The command's own variables (below) are known and ignored.
- Values must parse:
  - integers must be non-negative;
  - booleans are `true`/`false`/`1`/`0`;
  - floats must be finite;
  - socket addresses must be `IP:port` (host names are rejected).
- Every section parses whether or not its Cargo feature is compiled. Enabling a section whose feature is missing is an error, not a no-op. For example, `otlp.enabled = true` without `otel` fails.
- A configuration file is limited to 1 MiB (`config::MAX_CONFIG_FILE_BYTES`) and must be a regular file (symbolic links followed). Devices, FIFOs, sockets, and directories are refused before they are opened, and the read stops one byte past the limit, so a file that grows after the check is still refused (see [security](security.md#bounded-file-reads)).

## Secrets

`management.token` and `database.url` are secrets:

- They are held in `Secret`, and `Debug`, `Display`, and serialization print `<redacted>`.
- `ferrum-alloy check --show-effective` and `AlloyConfig::redacted_toml()` never reveal them.
- Each can be supplied as `FERRUM_ALLOY_<NAME>_FILE=/path`. Trailing `\n` and `\r` characters are trimmed. The file must be a regular file of at most 64 KiB (`config::MAX_SECRET_FILE_BYTES`), read like the configuration file; otherwise loading fails with an error that names the variable and never quotes the file.
- Setting both `<NAME>` and `<NAME>_FILE` is an error.
- Syntax and schema (type) errors never quote values. A TOML syntax error reports the file, line, column, and parser message, without the source excerpt. A schema error reports the key path and the expected type or variants, never the supplied value. Both apply to startup errors and to `ferrum-alloy check` in human and JSON output. For a syntax error, the JSON output also has `location.line` and `location.column`. Semantic validation errors, reported after parsing, may quote non-secret values, such as an unaccepted algorithm name or a bind address; they never quote secrets.

The management listener binds to loopback by default, but loopback does not authenticate an operator: local users and containers or sidecars in the same pod share it, and Kubernetes NetworkPolicy does not isolate containers sharing loopback. Detailed health, metrics, and management OpenAPI/UI always require a configured `management.token` of at least 32 characters. Without one, these routes return `401`, even on loopback; `/livez` and `/readyz` remain minimal, status-only and token-free. Prefer `FERRUM_ALLOY_MANAGEMENT_TOKEN_FILE` to supply the secret. Diagnostic retrieval retains its separate tenant authorizer; the management token is neither required nor sufficient. See [security](security.md#management-surface).

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
| `FERRUM_ALLOY_WRITE_STALL_TIMEOUT_MS` | `server.write_stall_timeout_ms` | integer |
| `FERRUM_ALLOY_MAX_CONNECTIONS` | `server.max_connections` | integer |
| `FERRUM_ALLOY_MAX_IN_FLIGHT_REQUESTS` | `server.max_in_flight_requests` | integer |
| `FERRUM_ALLOY_TLS_CERT_PATH` | `server.tls.cert_path` | path |
| `FERRUM_ALLOY_TLS_KEY_PATH` | `server.tls.key_path` | path |
| `FERRUM_ALLOY_TLS_CLIENT_CA_PATH` | `server.tls.client_ca_path` | path |
| `FERRUM_ALLOY_TLS_CLIENT_AUTH` | `server.tls.client_auth` | `none` / `optional` / `required` |
| `FERRUM_ALLOY_TLS_CLIENT_CRL_PATHS` | `server.tls.client_crl_paths` | comma list of paths |
| `FERRUM_ALLOY_TLS_CLIENT_CRL_DEPTH` | `server.tls.client_crl_depth` | `chain` / `end_entity` |
| `FERRUM_ALLOY_TLS_CLIENT_CRL_UNKNOWN_STATUS` | `server.tls.client_crl_unknown_status` | `deny` / `allow` |
| `FERRUM_ALLOY_TLS_CLIENT_CRL_EXPIRATION` | `server.tls.client_crl_expiration` | `enforce` / `ignore` |
| `FERRUM_ALLOY_TLS_RELOAD_INTERVAL_MS` | `server.tls.reload_interval_ms` | `0`, or 100 to 86400000 |
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
| `FERRUM_ALLOY_DIAGNOSTICS_MAX_RECORDS` | `diagnostics.max_records` | integer |
| `FERRUM_ALLOY_DIAGNOSTICS_MAX_BYTES` | `diagnostics.max_bytes` | integer |
| `FERRUM_ALLOY_DIAGNOSTICS_MAX_AGE_MS` | `diagnostics.max_age_ms` | integer |

A test fails when a variable in `config::ENV_VARS` is missing from this table.

Variables of the `ferrum-alloy` command itself (`config::CLI_ENV_VARS`) share the prefix but set no configuration key. Service configuration ignores them instead of rejecting them as unknown, and never reads their values:

| Variable | Used by |
|---|---|
| `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` | `ferrum-alloy diagnose --url`: the credential sent to a service's diagnostic retrieval endpoint ([`[diagnostics]`](#diagnostics-feature-diagnostics)) |
| `FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN` | `ferrum-alloy diagnose --edge-admin-url`: Edge admin JWT with `diagnostics:read` scope and an `ns` claim; never used for the service's `--url` endpoint |

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
| `max_connections` | `10000` | Further connections are closed immediately. An upgraded (WebSocket) connection keeps its slot after the `101` response until the application drops it, so upgraded sessions count against this limit and in `ferrum_alloy_active_connections`; size it for long-lived sessions as well as requests. |
| `http2_max_concurrent_streams` | `256` | Per-connection HTTP/2 stream limit. |
| `header_read_timeout_ms` | `10000` | Time to receive a request head (slow-header protection). It also bounds the time from a ready connection (accepted, and past the TLS handshake if any) to its first complete request head, whatever the protocol: a connection that sends nothing, stops inside the HTTP/2 preface, or opens HTTP/2 without sending a request is closed and frees its `max_connections` slot. An established HTTP/2 connection is sent `GOAWAY` first and dropped after a grace period of at most one second. These closes are counted in `ferrum_alloy_first_request_timeouts_total` (application listener). On HTTP/1.1 keep-alive connections it also bounds the wait for the next request. The management listener uses the same value. |
| `idle_timeout_ms` | `60000` | After a connection's first request: how long it may stay open with no request in flight and no response data written. A request counts as in flight from its request head until its response body has ended or been dropped, so slow handlers and long response streams (SSE) are never idle. Hyper lets go of a response body once it has taken the last chunk, which may still be on its way to the client, so idle time counts from the last response byte accepted by the transport. Transport acceptance does not prove client receipt: after every byte has been accepted and no write is pending, the connection can legitimately close as idle while the client consumes buffered response data, without truncating the body (see [measurement semantics](measurement-semantics.md#connection-timeout-boundaries)). An HTTP/1.1 connection whose transport cannot take a write is not idle: the rest of a large response can remain in Hyper's write buffer when its body ends, so a reader that pauses, or a network stall, longer than this timeout does not cut the response; `write_stall_timeout_ms` closes the connection instead if the peer takes nothing more. HTTP/2 control writes do not count as activity or defer idle closure, even when PING or SETTINGS acknowledgements are blocked in the transport, so a peer that sends one request and then only control frames cannot keep its `max_connections` slot. An HTTP/2 connection is sent `GOAWAY` and dropped after the same grace period as above; a request that raced the `GOAWAY` gets up to `shutdown.drain_timeout_ms` to finish. These closes are counted in `ferrum_alloy_idle_timeouts_total` (application listener). HTTP/1.1 keep-alive connections are normally closed first by `header_read_timeout_ms`. A response stalled on flow control (the peer withholds HTTP/2 `WINDOW_UPDATE` or keeps a zero TCP receive window) still counts as in flight; `write_stall_timeout_ms` closes its connection instead. A response whose body has ended but whose last data the peer takes none of (it withholds HTTP/2 `WINDOW_UPDATE`) writes nothing, so this timeout closes its connection, or `write_stall_timeout_ms` if that comes first. Neither this timeout nor `write_stall_timeout_ms` is a minimum transfer rate: a reader that takes a little data at a time, such as one byte of window or one TCP segment every half period, keeps its connection while the response is pending, as with nginx `send_timeout`. Behind a load balancer or proxy that pools connections, set it above that proxy's idle timeout, so the proxy closes idle connections first and never sends a request on one Alloy is closing. The management listener uses the same value. Must be greater than zero. |
| `write_stall_timeout_ms` | `60000` | How long a connection may go without writing any response data while response data waits to be written, that is, while the peer takes none of it: it withholds HTTP/2 `WINDOW_UPDATE` or keeps a zero TCP receive window. A transport that cannot take a write counts as waiting whether or not the response body has ended, so this timeout, not `idle_timeout_ms`, disconnects an HTTP/1.1 client that stops reading a finished response. Only response data counts: on HTTP/2, that is `DATA` frames, so a peer cannot hold a stalled response open by sending `PING`s or `SETTINGS` that the server must acknowledge. On HTTP/2, response data waits from the moment a response body hands it to Hyper until Hyper takes it for writing or drops it with a reset stream, including the part of a chunk that Hyper holds beyond the peer's flow-control window while the body waits for the application, so a peer cannot hold its slot by taking part of a chunk and then withholding `WINDOW_UPDATE` from a stream that has nothing more to send yet. A slow reader that keeps taking data, however slowly, is never cut (this is not a minimum transfer rate), and neither is a response stream waiting for the application to produce its next event once the peer has taken the data it already produced. Progress is checked every half period, so a stalled connection is closed between one and one and a half times this value after its last response data was written. An HTTP/2 connection is sent `GOAWAY`, if the transport still takes it, and dropped after the same grace period as above, together with any other request in flight on it. These closes are counted in `ferrum_alloy_write_stall_timeouts_total` (application listener). A client that deliberately stops reading for longer, such as a media player with a full buffer, is disconnected too; raise the value for such workloads. The management listener uses the same value. Must be greater than zero. |
| `request_timeout_ms` | `30000` | Deadline to produce response **headers**; `503 request-timeout` when exceeded. Never applied to response bodies (SSE) or upgraded sessions. `0` disables it. |
| `max_in_flight_requests` | `0` (unlimited) | Concurrent handler admission limit; excess gets `503 overloaded`. The permit is released when headers are produced. The liveness path on the application listener (`health.liveness_path`) takes no permit, so a busy process still passes its liveness probe; readiness and every other path do. |
| `admission_wait_timeout_ms` | `0` | How long to wait for a permit. `0` rejects immediately. The wait is recorded as `alloy.admission.wait_ms`. |

#### `[server.tls]` (feature `tls`)

| Key | Default | Meaning |
|---|---|---|
| `cert_path` | required | PEM certificate chain. A regular file of at most 1 MiB (`tls::MAX_PEM_FILE_BYTES`). |
| `key_path` | required | PEM private key. A regular file of at most 1 MiB. |
| `client_ca_path` | none | CA bundle for client certificates. A regular file of at most 1 MiB. |
| `client_auth` | `none` | `none`, `optional`, or `required` client certificates. With `optional` or `required`, TLS session resumption is disabled, so client certificate validity and revocation are re-checked on every handshake; this costs a full handshake per connection. With `none`, sessions resume. |
| `handshake_timeout_ms` | `10000` | TLS handshake timeout. Handshakes still in progress when shutdown starts are abandoned. |
| `client_crl_paths` | `[]` | Certificate revocation lists (CRLs) checked against client certificates during the handshake; a revoked certificate fails the handshake. Each file holds one DER CRL, or any number of PEM `X509 CRL` sections, and is a regular file of at most 16 MiB (`tls::MAX_CRL_FILE_BYTES`; a CRL lists every unexpired revoked certificate, so a large CA's can reach several MiB). Empty disables revocation checking. Provide exactly one CRL per issuing CA (a full CRL; combine partitioned CRLs into one), because the verifier consults only the first CRL whose issuer matches; two CRLs with the same issuer fail startup. Requires `client_auth` `optional` or `required`; setting the other `client_crl_*` keys without it is an error. At startup, a missing, unreadable, or unparsable file, a file that is not a regular file or exceeds 16 MiB, a file without a CRL, or (with `client_crl_expiration = "enforce"`) an expired CRL fails startup with an error that names the file and never quotes it. A new CRL is picked up by the next reload (see `reload_interval_ms`), which applies the same checks; with reloading disabled, restart. The earliest `nextUpdate` time is logged whenever CRLs are loaded, as a warning when it is less than 24 hours away. |
| `client_crl_depth` | `chain` | Which certificates are checked: `chain` checks the leaf and every intermediate the client presents, `end_entity` only the leaf. Trust anchors from `client_ca_path` are never checked, so an intermediate placed in `client_ca_path` is never checked either. With `chain`, supply a CRL from each issuing CA, including the root's CRL for its intermediates. |
| `client_crl_unknown_status` | `deny` | A certificate whose issuer has no CRL in `client_crl_paths` has an unknown status. `deny` fails the handshake; `allow` accepts the certificate, and `ferrum-alloy check` warns about it. |
| `client_crl_expiration` | `enforce` | A CRL is expired once its `nextUpdate` time has passed. `enforce` fails startup on an expired CRL, refuses a reload that would load one, and fails handshakes once a loaded CRL expires, until a fresh CRL is loaded: the next reload loads a fresh CRL published in its place, without a restart; `ignore` keeps using the stale CRL, and `ferrum-alloy check` warns about it. |
| `reload_interval_ms` | `60000` | How often `cert_path`, `key_path`, `client_ca_path`, and `client_crl_paths` are read again. When any file's bytes changed, they are read once more after a settle delay (the interval, at most one second); only when both reads match is the new material validated exactly as at startup (every certificate of the chain parses, the key matches the certificate, the CA bundle parses, and the CRL checks above pass) and then swapped in, all together, for new handshakes. Files still changing between the two reads are left for the next interval, so a file caught halfway through being written is never used. Established connections keep the session they negotiated; sessions cannot be resumed across a reload (nor at all with client authentication, see `client_auth`), so every handshake after it is verified against the new material. Material that fails validation is never used: the previous material keeps serving, and the error is logged without quoting any file. A failure is counted in `ferrum_alloy_tls_reload_failures_total` (application listener) once the same files fail twice in a row, and then at every interval until the files are fixed; it is logged at error level when it starts and whenever the error changes, and at debug level otherwise. Files whose bytes change between the two reads at three reloads in a row, with no swap or unchanged reload in between (a writer that rewrites them continuously), are never used: this is logged once as a warning and counted once in `ferrum_alloy_tls_reload_stalls_total`, and the previous material keeps serving; a later swap or unchanged reload ends the streak, so the next one is reported again. Successful swaps are counted in `ferrum_alloy_tls_reloads_total`. The gauges `ferrum_alloy_tls_server_cert_not_after_timestamp_seconds` and `ferrum_alloy_tls_client_crl_next_update_timestamp_seconds` give the `notAfter` time of the serving certificate and the earliest `nextUpdate` time of the serving CRLs, in Unix seconds, and are left out when unknown (the CRL gauge without CRLs, or when no CRL has a `nextUpdate` time). While a serving CRL expires within 24 hours or has expired, the warning is repeated about once an hour. A reload never changes `client_auth` or any other setting, so it can never turn client authentication off. The files must stay readable after startup (a reload that cannot read them fails and is counted), or set `reload_interval_ms = 0`. A file replaced by one that is not a regular file, or that exceeds its limit (see `cert_path` and `client_crl_paths`), fails the reload the same way, and the previous material keeps serving. Replace files atomically where possible (write a new file and rename it over the old one, as Kubernetes secret volumes do). `0` disables reloading; otherwise 100 to 86400000 (one day). |

### `[shutdown]`

| Key | Default | Meaning |
|---|---|---|
| `readiness_grace_ms` | `0` | After SIGTERM, keep accepting while readiness reports `draining`, so load balancers stop routing first. A second signal skips the grace. |
| `drain_timeout_ms` | `30000` | After accepting stops: budget for requests, response streams, and upgrades. At the budget, all remaining accepted TCP connections are shut down through owned handles, including TLS and upgrades the application never polls; connection/HTTP2 tasks end before return. An additional wait of at most one second lets applications drop upgrades. Their permits/counts and handles remain until drop, even after TCP shutdown. |
| `telemetry_flush_timeout_ms` | `5000` | Bound on the final span flush. |

### `[management]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Serve the management listener. |
| `bind` | `127.0.0.1:9090` | Must differ from `server.bind`. A non-loopback bind **requires** `token`, and is refused while diagnostic retrieval is installed ([`[diagnostics]`](#diagnostics-feature-diagnostics)). `AlloyParts::serve_on` applies the same bind rules to the address a listener is actually bound to, which may differ from this setting: any loopback address is accepted, and a non-loopback address requires a token. Diagnostic retrieval is allowed only on loopback. If you serve `AlloyParts::management_router` yourself, call `AlloyParts::check_management_listener` first. |
| `token` | none | Operator bearer token (at least 32 characters). Detailed health, metrics, and management OpenAPI/UI always require it; absent tokens deny access even on loopback. Minimal probes remain token-free; diagnostics has a separate authorizer. |

#### `[management.rate_limit]`

Every request to the management listener is rate-limited before any handler or token check runs, so failed token attempts count too. Requests are charged to one of two budgets: **probes** (`/livez` and `/readyz`) and **endpoints** (every other path, including unknown ones). Each budget has its own client table and lock, so traffic to one budget never throttles or slows the other, and kubelet and Edge health probes keep working while `/metrics` is saturated.

An endpoint request is charged to a token bucket of its client and one of the whole listener. It is admitted only when both hold a token, and a rejected request consumes neither, so one client over its limit cannot drain the listener budget for the others. A probe is charged to its client's bucket only: there is no listener-wide probe budget, so no set of sources can use up `/livez` for everyone else. A rejected request gets `429` Problem Details (`tag:ferrumedge.com,2026:alloy/problem/rate-limited`) with `Retry-After` (whole seconds until a token is available) and `Cache-Control: no-store`.

A client is the transport peer address of the connection, never a header such as `X-Forwarded-For`. IPv4-mapped IPv6 addresses count as IPv4. Other IPv6 addresses are keyed by their first `ipv6_prefix_len` bits, because one host usually controls a whole /64. Lower it (down to 48) when one tenant controls more, and raise it (up to 128) when distinct clients share a /64: in Kubernetes, pod addresses of one node often come from a single /64, and behind NAT64 many IPv4 clients arrive from one /96 prefix.

Peers in `exempt_networks` bypass the limits entirely, except on the diagnostic retrieval route, which limits every peer; the default is empty, so loopback peers are limited too. Kubelet probes the pod from its node's address, so add the node network (for example `10.244.0.0/16`, or the node CIDR of your cluster) only when exempting those probes is intended; otherwise a refused probe can restart the pod. Behind a proxy or sidecar, every client is the proxy's address: Istio connects to the application from `127.0.0.6`, and all proxied clients share that address's budgets. Add the sidecar address (for example `127.0.0.6/32`) only if bypassing the limits for every proxied client is intended. IPv4-mapped IPv6 CIDRs are rejected; write their IPv4 equivalent. A network of every address (`0.0.0.0/0`, `::/0`) is refused; set `enabled = false` instead.

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

#### Scraping `/metrics` with Prometheus

`/metrics` is served on the management listener and, like every detailed management route, always requires `Authorization: Bearer <management.token>`. Without a configured token it returns `401`, even on loopback, because loopback is not an authentication boundary. Give Prometheus the token through a credentials file so the secret never appears in `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: ferrum-alloy
    scheme: http
    metrics_path: /metrics
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/alloy-management-token
    static_configs:
      - targets: ["127.0.0.1:9090"]
```

The credentials file holds the token and nothing else; point it at the same file the service reads through `FERRUM_ALLOY_MANAGEMENT_TOKEN_FILE` (Alloy ignores a trailing CR/LF) and keep it readable only by the Prometheus service account. The management listener has no TLS, so scrape it over loopback or a trusted network; a non-loopback `management.bind` already requires the token. A scrape with no token, or a wrong one, gets `401` and counts against the endpoint rate-limit budget before any metrics handler runs.

### `[health]`

| Key | Default | Meaning |
|---|---|---|
| `app_endpoints` | `true` | Also serve liveness and readiness on the application listener, for gateway checks. These take precedence over app routes, so startup fails if an app route matches either path (see [route conflicts](#route-conflicts-on-the-application-listener)). Liveness there is not subject to `server.max_in_flight_requests`; readiness is. |
| `liveness_path` | `/livez` | Literal path: it starts with `/` and has no `{`, `}`, `*`, space, or segment starting with `:`. |
| `readiness_path` | `/readyz` | Literal path, like `liveness_path`. Must differ from it. |
| `cache_ttl_ms` | `5000` | Readiness results are reused for this long, with single-flight refresh. |
| `check_timeout_ms` | `2000` | Per-check timeout |

#### Route conflicts on the application listener

Alloy serves its own paths on the application listener ahead of your router, which is Alloy's fallback: the health paths with `app_endpoints = true`, and, with `openapi.public = true` and a registered document, `openapi.path` and (with `openapi.ui`) `openapi.ui_path` and every asset beneath it. A route of yours that matches one of those paths would never be reached, for any method, so startup fails closed with `AlloyError::ShadowedRoute`. It lists every conflict at once, each naming the path, the route that requests for it would never reach (or, for a `nest_service`, that its prefix contains the path), and the setting that serves the path. Change that setting, or move or remove the route. Paths served only on the management listener cannot conflict.

A root parameter route such as `/{code}` matches both default health paths, `/livez` and `/readyz`. To keep it, move `liveness_path` and `readiness_path` under a prefix, such as `/_alloy/livez` and `/_alloy/readyz` (a parameter matches a single segment), or set `health.app_endpoints = false` and serve health only on the management listener.

axum cannot list a router's routes, so `AlloyApp` asks your router instead. At startup it routes a `GET` for each of those paths through a copy of your router whose every endpoint (handlers, method-not-allowed handlers, nested services, and fallbacks) is replaced by a stub that never calls it, and reads which kind of endpoint matched. None of your handlers, layers, or fallbacks runs, so the check has no side effects. What counts:

- Any route that matches the path is a conflict, whatever its methods: a literal route (`/docs`), a parameter (`/docs/{file}` matches `/docs/swagger-ui.css`; `/{code}` matches `/livez`), a nested router's route, or a nested service (`nest_service`).
- A root catch-all route (`/{*path}`) matches every path, so it is treated like a fallback and is not a conflict: Alloy's paths take precedence over it, as they do over your fallback.
- Fallbacks, including nested routers' fallbacks, are not routes and are not conflicts.

`ferrum-alloy check` validates configuration only and cannot see your router; the check runs when the application composes (`AlloyApp::run` or `AlloyApp::into_parts`).

### `[logging]`

| Key | Default | Meaning |
|---|---|---|
| `format` | `json` | `json`, `pretty`, or `compact` |
| `filter` | `RUST_LOG`, else `info` | `EnvFilter` directives |
| `ansi` | `false` | Colors for text formats |

With `json`, each event is one line:

```text
{"timestamp":"2026-09-27T12:00:00.000000Z","level":"INFO",<event fields>,"target":"ferrum_alloy::access","span":{<span fields>,"name":"http.server.request"}}
```

- Event fields, including `message`, are flattened into the object in the order the event declares them.
- `span` is the event's parent span, or else the current span, and is omitted when there is neither. Its fields are sorted by name, followed by `name`, the span name. Fields that were never recorded are absent.
- The timestamp is RFC 3339 in UTC with microseconds.
- Strings use JSON escapes: `\"`, `\\`, `\b`, `\t`, `\n`, `\f`, `\r`, and `\u00xx` for other control characters. U+0085, U+2028, and U+2029 are also escaped as `\u0085`, `\u2028`, and `\u2029` so line-oriented consumers cannot split a JSON value at a Unicode line separator; other text, including non-ASCII, is written as UTF-8. Byte-slice fields are written as `"[ff 00]"` on events and `[255,0]` on spans. Non-finite floats are `null`.

This is the layout tracing-subscriber's JSON formatter produces with flattened events, the current span, and no span list, which Alloy used before it had its own JSON layer. The layer writes the same bytes, with three exceptions: a float span field is written from the recorded value, where tracing-subscriber's re-parse could change its last digit; a span field whose `Debug` implementation fails is left out instead of panicking; and U+0085, U+2028, and U+2029 are escaped to protect line-oriented consumers. Span field `Debug` code runs before the span extensions lock is taken, allowing it to log re-entrant events. `crates/ferrum-alloy-telemetry/tests/snapshots/json-access-log.expected.txt` pins the access-event line.

### `[telemetry]`

| Key | Default | Meaning |
|---|---|---|
| `request_id.header` | `x-request-id` | Correlation header. Must not be a reserved name (`authorization`, `traceparent`, `x-consumer-username`, …). |
| `request_id.accept_incoming` | `trusted_peers` | Whose incoming ids are kept, after validation: at most 256 bytes of `[A-Za-z0-9._-]`, matching Ferrum Edge's `correlation_id` plugin. Requests from other peers get a generated id, which handlers see and the response echoes, so a direct untrusted caller cannot choose its logged/traced correlation id. Retention ownership always uses a local diagnostic id, including when incoming correlation is accepted (see [diagnostics](#diagnostics-feature-diagnostics)). Ferrum Edge keeps its correlation ids only when it is a trusted peer: when upgrading from a release that defaulted to `any`, list Edge in [`[trust]`](#trust) (`trust.identities`, preferred, or `trust.networks`), or Alloy logs and traces Edge's requests under generated correlation ids that do not match the id Edge echoes to its client. `never` generates every id; `any` keeps ids from every caller, and validation warns about it. |
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
| `mode` | `standalone` | `standalone`, `gateway_preferred`, or `gateway_required`. `gateway_required` needs `trust.identities`. The health paths are exempt from it only when `health.app_endpoints = true`, where Alloy's status-only handlers serve them. With `app_endpoints = false`, probe the management listener, bound to an address the node can reach rather than loopback. |
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
| `serve` | `true` | Serve a registered document on the management listener, always requiring a configured `management.token`. |
| `path` | `/openapi.json` | Literal path, like `health.liveness_path`. It must not be a path Alloy already serves on a listener that serves the document: `/livez`, `/readyz`, `/health`, or `/metrics` on the management listener, or, with `public = true`, a health path on the application listener while `health.app_endpoints` is on. With a registered document, `AlloyApp::into_parts` fails with a configuration error naming both settings; `ferrum-alloy check` cannot see whether a document is registered. |
| `public` | `false` | Also serve it unauthenticated on the application listener. |
| `ui` | `false` | Serve the documentation UI (feature `openapi-ui`) wherever the document is served: on the management listener always requiring a configured `management.token`, and on the application listener, unauthenticated, only with `public = true`. Needs a registered document and `serve = true`. |
| `ui_path` | `/docs` | Path of the UI page; its assets are served beneath it. Segments of letters, digits, `-`, `.`, `_`, and `~`, with no trailing `/`. It must not be or contain another served path, and must be outside `/diagnostics`. See the note on your routes below. |

#### Documentation UI (feature `openapi-ui`)

The UI is [Swagger UI](https://github.com/swagger-api/swagger-ui) 5.33.0, compiled into the binary from files vendored in `crates/ferrum-alloy/assets/swagger-ui/` (Apache-2.0; the directory carries its `LICENSE`, `NOTICE`, the bundled dependencies' notices, and the hashes the tests check). It makes no request to another origin: the page loads its script, stylesheet, and the OpenAPI document from the listener that served it. It is read-only; "Try it out" is disabled. The page's assets are `swagger-ui.css`, `swagger-ui-bundle.js`, `swagger-initializer.js`, and `swagger-ui-bundle.js.LICENSE.txt` (the bundle's license notices, which its first line points to, served as `text/plain`), all beneath `ui_path` and under the same access policy.

CI loads the UI in Google Chrome, on both listeners, and fails if Swagger UI does not render the document, if the browser reports any Content-Security-Policy violation or console error, or if the page requests anything from another origin. See [testing](testing.md#browser-smoke-test).

**Your routes.** With `public = true`, Alloy serves `path` (`/openapi.json`), `ui_path`, and every asset beneath `ui_path` on the application listener, ahead of your router, so startup fails if one of your routes matches any of them (see [route conflicts](#route-conflicts-on-the-application-listener)). If your API has a route at `/docs` (or beneath it) or at `/openapi.json`, pick a different `ui_path` (or `path`).

The management UI, its assets, and the document require `Authorization: Bearer <management.token>` even on loopback. Browsers do not add bearer tokens by themselves; use a local proxy or a carefully scoped header-injecting extension that reads the secret from a protected source. Never put the token in a URL, and do not store a live token in this configuration file or browser URL. Without a configured token these routes return `401`; only `/livez` and `/readyz` remain available without one. Store the token in a secret manager or a file readable only by the service account (for example, mode `0600` on Unix), then set `FERRUM_ALLOY_MANAGEMENT_TOKEN_FILE` to its path. Loopback is reachable by other local processes and same-network-namespace sidecars, so it does not replace authentication. See [secrets](#secrets) and [security](security.md#management-surface).

**Do not expose the UI publicly in production.** `ui` with `public` serves it, like the document, to anyone who can reach the application listener, and `ferrum-alloy check` and startup warn about that combination. Prefer the management listener, and reach it through a port-forward or tunnel. See [security](security.md#openapi-documentation-ui).

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

Refreshes never hold up requests that can be answered from the cache. While the key set is stale, a request with a known `kid` verifies against it at once and starts a background refresh. Requests wait for a refresh only when the key set has expired, has never been fetched, or has no key for the token's `kid` and algorithm; a stale set that matches several keys waits too. On a fresh set, several matching keys are ambiguous and get `401` at once, without a refresh. When the refresh fails, or the rate limit holds behind a failed one, a token for which the cached set has no key gets `503 auth-unavailable`, not `401`; `401` for an unknown key follows only a successful refresh. An ambiguous match is always `401`. A refresh runs in its own task, so a request that is cancelled while waiting does not cancel it. Keys from a successful refresh are used even if the fetch took longer than their lifetime.

An RSA JWK without `alg` verifies every `RS*` and `PS*` algorithm in `algorithms`. To bind each RSA key to one scheme ([RFC 8725, section 3.1](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.1)), publish `alg` on the JWKS keys, or allowlist a single RSA algorithm. EC and Ed25519 keys are bound to one algorithm by their curve.

### `[http_client]` (feature `http-client`)

| Key | Default | Meaning |
|---|---|---|
| `connect_timeout_ms` | `2000` | Connection timeout |
| `request_timeout_ms` | `10000` | Whole-request timeout |
| `max_redirects` | `0` | Same-origin redirects only. Cross-origin redirects are never followed. |
| `propagate_trace_context_to` | `[]` | Hosts (exact, or `.suffix`) that receive `traceparent` and the accepted `tracestate`. Caller-supplied trace headers are replaced, never forwarded. Credentials, cookies, and baggage are never forwarded automatically. |

### `[diagnostics]` (feature `diagnostics`)

Bounds of the in-memory evidence that authorized diagnostic retrieval serves ([ADR 0008](adr/0008-tenant-scoped-diagnostic-retrieval.md), [security](security.md#diagnostic-retrieval)). They apply only when the application installs a `DiagnosticsAuthorizer`; without one, no evidence is retained and `GET /diagnostics/v1/requests/{request_id}` does not exist. Only requests the application tagged with a tenant (`TenantTag`) are retained.

Records whose age on the monotonic clock, measured from admission, reaches `diagnostics.max_age_ms` (default 15 minutes) expire: before each admission, lookup and metrics scrape they are removed oldest first and counted as `evicted_total{reason="age"}`, and an expired id gets the same `404` as an evicted one. Admission reserves every required eviction under the store lock before changing any record, including a local request's 16-attempt replacement. Count pressure is handled before byte pressure. At each step, the oldest record of the heaviest tenant in the pressured dimension may be reserved only if that tenant's **remaining** count or estimated bytes would be at least the candidate tenant's projected holding, including the new record and excluding earlier reservations. Otherwise admission reserves the candidate tenant's own oldest record. Once the candidate holds nothing, counting reservations, it takes over a record instead. While another tenant holds two or more records, it takes the oldest record of the tenant holding the most records, which keeps at least the one record the candidate will hold, in the byte dimension too. Once every tenant holds at most one record, it takes the oldest record, whose owner is the least recently active tenant (the one whose newest record is oldest), but only once that record has been retained for 1 second (`diagnostics::TAKEOVER_AGE`). If no record can be reserved while a bound is still exceeded, the candidate is dropped as `fair_share` and all reservations are discarded: neither its previous evidence nor another tenant's evidence is lost. That happens only while every other tenant holds at most one record and the oldest retained record is younger than 1 second, so admission never freezes, by count or by bytes, even with `diagnostics.max_age_ms` disabled. A new tenant can reclaim real overshare, and displaces another tenant's sole record only through that takeover, never before the record is 1 second old. One admission takes over sole records only until the new record fits: at most the new record's estimated bytes divided by the smallest taken record's, rounded up, which is two or three between the smallest and largest records the estimate allows. The donor check is conservative in the byte dimension: it looks only at the heaviest tenant's oldest record. The takeover keeps record counts fair, not bytes: a new tenant can end up holding more bytes than a tenant it took a record from. In the count dimension, checking only the heaviest tenant is exact. Ties use tenant name and records use insertion sequence; no fixed per-tenant quota or equal-share convergence guarantee is asserted.

| Key | Default | Meaning |
|---|---|---|
| `max_records` | `1024` | Most requests retained, `1` to `65536`. |
| `max_bytes` | `1048576` | Most estimated bytes retained, `4096` to `67108864`. The charge includes fixed record, tenant, primary-index and alias/owner-index overhead (B-tree nodes at minimum occupancy, hash entries at maximum load, with four capacity slots per live alias owner), tenant text, twice the correlation id's allocated text capacity, three times the local id's allocated text capacity, and the bounded route template. Shared entries are conservatively charged per record. The local and alias indexes increase the charge compared with earlier unreleased source; capacity sized for small records may hold fewer. Allocator overhead, control-group padding, temporary reservations, and spare capacity of the other collections are outside the estimate, so this is not a resident-memory limit. |
| `max_age_ms` | `900000` | Longest a record is retained, in milliseconds on the monotonic clock from its admission, so wall-clock changes do not affect it. `0` disables expiry; otherwise `1000` to `86400000`. |

Every frontend request has a fresh, immutable local diagnostic id, available through `RequestContext::diagnostic_id()` and the `alloy.diagnostic_id` server-span field. Generated correlation ids use that same value; accepted remote ids get a separate local value. The telemetry layer captures local ownership before invoking application code. Clones of the same local evidence can retain up to 16 consistent attempts; a further attempt replaces the oldest only after successful capacity reservation. Inconsistent correlation, origin, trace identity or local/accepted-remote provenance on a cloned local owner is skipped as `request_id_conflict`. Separate frontend requests never share this cap, even when a trusted gateway forwards exactly the same external id and accepted trace. The built-in sink still emits one finalized server record per frontend request; outbound operations and their retries remain in trace export, not additional retained server records.

Lookup first checks the authorized tenant's local diagnostic ids. Otherwise the correlation alias uses origin preference (`generated`, `trusted_peer`, then `untrusted_caller`): the first origin present must name exactly one retained local owner. Multiple owners make that alias ambiguous and return the same `404` as an unknown id, even if their remote traces match; no first-preclaim winner or fallback to a lower origin is selected. Less preferred origins are omitted and counted in report notes. Alias entries and owner counts are removed with their retained records, by capacity eviction or age expiry. A report resolved through an alias says so in a note. After a request's local id has been evicted or expired, the same text can resolve as the correlation alias of a different retained request of the same tenant, for example when a client reused an echoed generated id; that report names its own local lookup id and carries the alias note. When earlier attempts of a request are no longer retained while later ones are, a note gives how many are missing. The count covers only attempts dropped while a later attempt of the request was retained: once every retained attempt is gone, the count is forgotten, and a later attempt of the same request is reported without the note. All records remain individually retrievable by their local ids within retention bounds. Reusing an external alias can deny the convenience of alias lookup, but cannot append to, overwrite or suppress another local request through the per-request cap. Ordinary tenant count/byte pressure can still evict older requests.

Alias owner tables are rebuilt at half-load when their allocation high-water capacity exceeds four times their live owner count. Tracking the allocation high-water mark avoids mistaking deletion tombstones for freed buckets. After each committed mutation, aggregate owner-table allocation capacity is at most four times the number of live alias owners, which is at most the retained record count; table rounding and control bytes add a constant factor. Rebuilds move existing ids and occur only after geometric depletion, so total compaction work is amortized over owner removals. A single rebuild can still scan one alias's previous allocation while holding the retention mutex. Top-level hash tables retain capacity proportional to their configured record-count high-water mark; `max_bytes` remains an estimate, not a process memory limit.

HTTP request/response correlation-id behavior and accepted trace propagation are unchanged. Reports keep the actual correlation id in `subject.request_id`, the actual trace/span identities and existing provenance fields, and describe the local lookup id in collection notes; they do not claim that the local id was transmitted. No new HTTP header, shared schema field, Edge API, G01 behavior or contract pin is introduced. Trusted transport establishes who delivered correlation bits, not who generated them. Trace ids are correlation hints, never lookup ownership or authenticated caller identity.

Retrieval also requires `management.enabled`, `management.rate_limit.enabled`, and a loopback `management.bind`, and `management.rate_limit.exempt_networks` does not apply to the retrieval route. `/metrics` reports `ferrum_alloy_diagnostics_records` and `ferrum_alloy_diagnostics_bytes` (current retention), `ferrum_alloy_diagnostics_stored_total`, `ferrum_alloy_diagnostics_evicted_total{reason="count"|"bytes"|"request_id_limit"|"age"}`, `ferrum_alloy_diagnostics_skipped_total{reason="untagged"|"too_large"|"request_id_conflict"|"fair_share"}`, `ferrum_alloy_diagnostics_retrievals_total{outcome="served"|"not_found"}` (denials count as `not_found`, like the response they get), `ferrum_alloy_diagnostics_authorizer_failures_total{reason="timeout"|"panic"}`, and `ferrum_alloy_diagnostics_resets_total` (times the store discarded all retained evidence because a panic poisoned its lock, which may have left its indexes inconsistent). A sustained rise in `skipped_total{reason="fair_share"}` means tenants that need room arrive faster than retained sole records reach the 1 second takeover age, so evidence is refused to protect other tenants' records; raise the bounds or lower `max_age_ms`.

The diagnostic retrieval command, `ferrum-alloy diagnose --url`, reads its credential from `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or `--token-file`. That variable belongs to the command, not to service configuration: it is listed in `config::CLI_ENV_VARS`, and loading service configuration ignores it rather than rejecting it as an unknown `FERRUM_ALLOY_*` variable.

## Checking configuration

```bash
ferrum-alloy check --config alloy.toml
```

This runs the configuration validation that startup runs, without binding sockets or reading TLS files. It prints each capability as enabled or disabled, then warnings and errors, and exits `3` when the configuration is invalid.

- `--features otel,tls,...` names the features your build compiles. By default every feature is assumed, so only feature-independent problems fail.
- `--no-env` ignores `FERRUM_ALLOY_*` variables in the current environment.
- `--show-effective` prints the merged configuration with secrets redacted.
- `--format json` prints the result as JSON.
