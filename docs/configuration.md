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

A test fails when a variable in `config::ENV_VARS` is missing from this table.

Variables of the `ferrum-alloy` command itself (`config::CLI_ENV_VARS`) share the prefix but set no configuration key. Service configuration ignores them instead of rejecting them as unknown, and never reads their values:

| Variable | Used by |
|---|---|
| `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` | `ferrum-alloy diagnose --url`: the credential sent to a service's diagnostic retrieval endpoint ([`[diagnostics]`](#diagnostics-feature-diagnostics)) |

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
| `idle_timeout_ms` | `60000` | After a connection's first request: how long it may stay open with no request in flight and no response data written. A request counts as in flight from its request head until its response body has been sent or dropped, so slow handlers and long response streams (SSE) are never idle. Hyper lets go of a response body once it has taken the last chunk, which may still be on its way to the client, so idle time counts from the last response byte written: a slow reader downloading the end of a large response is not cut while it keeps taking data. HTTP/2 keep-alive pings do not count as activity, so a peer that sends one request and then only answers pings cannot keep its `max_connections` slot. An HTTP/2 connection is sent `GOAWAY` and dropped after the same grace period as above; a request that raced the `GOAWAY` gets up to `shutdown.drain_timeout_ms` to finish. These closes are counted in `ferrum_alloy_idle_timeouts_total` (application listener). HTTP/1.1 keep-alive connections are normally closed first by `header_read_timeout_ms`. A response stalled on flow control (the peer withholds HTTP/2 `WINDOW_UPDATE` or keeps a zero TCP receive window) still counts as in flight; `write_stall_timeout_ms` closes its connection instead. A response whose body has ended but whose last data the peer takes none of (it withholds HTTP/2 `WINDOW_UPDATE`) writes nothing, so this timeout closes its connection, or `write_stall_timeout_ms` if that comes first. Neither this timeout nor `write_stall_timeout_ms` is a minimum transfer rate: a reader that takes a little data at a time, such as one byte of window or one TCP segment every half period, is never closed, as with nginx `send_timeout`. Behind a load balancer or proxy that pools connections, set it above that proxy's idle timeout, so the proxy closes idle connections first and never sends a request on one Alloy is closing. The management listener uses the same value. Must be greater than zero. |
| `write_stall_timeout_ms` | `60000` | How long a connection may go without writing any response data while response data waits to be written, that is, while the peer takes none of it: it withholds HTTP/2 `WINDOW_UPDATE` or keeps a zero TCP receive window. Only response data counts: on HTTP/2, that is `DATA` frames, so a peer cannot hold a stalled response open by sending `PING`s or `SETTINGS` that the server must acknowledge. On HTTP/2, response data waits from the moment a response body hands it to Hyper until Hyper takes it for writing or drops it with a reset stream, including the part of a chunk that Hyper holds beyond the peer's flow-control window while the body waits for the application, so a peer cannot hold its slot by taking part of a chunk and then withholding `WINDOW_UPDATE` from a stream that has nothing more to send yet. A slow reader that keeps taking data, however slowly, is never cut (this is not a minimum transfer rate), and neither is a response stream waiting for the application to produce its next event once the peer has taken the data it already produced. Progress is checked every half period, so a stalled connection is closed between one and one and a half times this value after its last response data was written. An HTTP/2 connection is sent `GOAWAY`, if the transport still takes it, and dropped after the same grace period as above, together with any other request in flight on it. These closes are counted in `ferrum_alloy_write_stall_timeouts_total` (application listener). A client that deliberately stops reading for longer, such as a media player with a full buffer, is disconnected too; raise the value for such workloads. The management listener uses the same value. Must be greater than zero. |
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
| `drain_timeout_ms` | `30000` | After accepting stops: time for in-flight requests, response streams, and upgraded (WebSocket) connections, which applications should close when `Lifecycle::shutdown_token` is cancelled. Remaining connections are then force-closed and counted in `ferrum_alloy_force_closed_connections_total`; for an upgraded connection, every read and write then fails and every task waiting on it is woken, so an application that reads or writes its session ends it, and serving waits up to one more second for the application to drop it. HTTP/2 stream tasks (a request's handler and its response body) still running are cancelled, which drops the handler, and counted in `ferrum_alloy_force_closed_streams_total`. Serving returns only after every aborted connection task and cancelled stream task has finished unwinding, so no HTTP/2 handler is still running, and every connection socket is closed except an upgraded one that the application still holds without reading or writing it: Alloy cannot drop that for the application, so it stays open until the application drops it. |
| `telemetry_flush_timeout_ms` | `5000` | Bound on the final span flush. |

### `[management]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Serve the management listener. |
| `bind` | `127.0.0.1:9090` | Must differ from `server.bind`. A non-loopback bind **requires** `token`, and is refused while diagnostic retrieval is installed ([`[diagnostics]`](#diagnostics-feature-diagnostics)). `AlloyParts::serve_on` applies the same rules to the address a listener handed to it is actually bound to, which may differ from this setting, and refuses to serve otherwise: any loopback address is accepted without a token, any address with one, and diagnostic retrieval only on loopback. If you serve `AlloyParts::management_router` yourself, call `AlloyParts::check_management_listener` first. |
| `token` | none | Bearer token (at least 32 characters) for `/health`, `/metrics`, the OpenAPI document, and its documentation UI. `/livez` and `/readyz` stay unauthenticated. |

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
| `serve` | `true` | Serve a registered document on the management listener (token-protected). |
| `path` | `/openapi.json` | Literal path, like `health.liveness_path`. It must not be a path Alloy already serves on a listener that serves the document: `/livez`, `/readyz`, `/health`, or `/metrics` on the management listener, or, with `public = true`, a health path on the application listener while `health.app_endpoints` is on. With a registered document, `AlloyApp::into_parts` fails with a configuration error naming both settings; `ferrum-alloy check` cannot see whether a document is registered. |
| `public` | `false` | Also serve it unauthenticated on the application listener. |
| `ui` | `false` | Serve the documentation UI (feature `openapi-ui`) wherever the document is served: on the management listener behind `management.token`, and on the application listener, unauthenticated, only with `public = true`. Needs a registered document and `serve = true`. |
| `ui_path` | `/docs` | Path of the UI page; its assets are served beneath it. Segments of letters, digits, `-`, `.`, `_`, and `~`, with no trailing `/`. It must not be or contain another served path, and must be outside `/diagnostics`. See the note on your routes below. |

#### Documentation UI (feature `openapi-ui`)

The UI is [Swagger UI](https://github.com/swagger-api/swagger-ui) 5.33.0, compiled into the binary from files vendored in `crates/ferrum-alloy/assets/swagger-ui/` (Apache-2.0; the directory carries its `LICENSE`, `NOTICE`, the bundled dependencies' notices, and the hashes the tests check). It makes no request to another origin: the page loads its script, stylesheet, and the OpenAPI document from the listener that served it. It is read-only; "Try it out" is disabled. The page's assets are `swagger-ui.css`, `swagger-ui-bundle.js`, `swagger-initializer.js`, and `swagger-ui-bundle.js.LICENSE.txt` (the bundle's license notices, which its first line points to, served as `text/plain`), all beneath `ui_path` and under the same access policy.

CI loads the UI in Google Chrome, on both listeners, and fails if Swagger UI does not render the document, if the browser reports any Content-Security-Policy violation or console error, or if the page requests anything from another origin. See [testing](testing.md#browser-smoke-test).

**Your routes.** With `public = true`, Alloy serves `path` (`/openapi.json`), `ui_path`, and every asset beneath `ui_path` on the application listener, ahead of your router, so startup fails if one of your routes matches any of them (see [route conflicts](#route-conflicts-on-the-application-listener)). If your API has a route at `/docs` (or beneath it) or at `/openapi.json`, pick a different `ui_path` (or `path`).

With a management token, a browser must send `Authorization: Bearer <token>` with the page, its assets, and the document, for example through a local proxy or a header-injecting extension. Browsers do not add bearer tokens by themselves. A management listener on loopback without a token (the default) needs nothing.

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

Bounds of the in-memory evidence that authorized diagnostic retrieval serves ([ADR 0008](adr/0008-tenant-scoped-diagnostic-retrieval.md), [security](security.md#diagnostic-retrieval)). They apply only when the application installs a `DiagnosticsAuthorizer`; without one, no evidence is retained and `GET /diagnostics/v1/requests/{request_id}` does not exist. Only requests the application tagged with a tenant (`TenantTag`) are retained. When a new record would exceed either bound, the oldest records are evicted first.

| Key | Default | Meaning |
|---|---|---|
| `max_records` | `1024` | Most requests retained, `1` to `65536`. |
| `max_bytes` | `1048576` | Most estimated bytes retained, `4096` to `67108864`. A record is charged a fixed overhead derived from the sizes of the in-memory structures (roughly 330 bytes on 64-bit targets) plus its tenant, twice its request id, and its route template, so one record is at most about 1.5 KiB. The bound is an estimate: allocator overhead and the spare capacity of the queue and index, which never shrink, are not charged, so actual memory use can exceed it somewhat. |

At most 16 records share one tenant and request id, such as the attempts of a retried request; a further one evicts the oldest of them. Retrieval also requires `management.enabled`, `management.rate_limit.enabled`, and a loopback `management.bind`, and `management.rate_limit.exempt_networks` does not apply to the retrieval route. `/metrics` reports `ferrum_alloy_diagnostics_records` and `ferrum_alloy_diagnostics_bytes` (current retention), `ferrum_alloy_diagnostics_stored_total`, `ferrum_alloy_diagnostics_evicted_total{reason="count"|"bytes"|"request_id_limit"}`, `ferrum_alloy_diagnostics_skipped_total{reason="untagged"|"too_large"}`, `ferrum_alloy_diagnostics_retrievals_total{outcome="served"|"not_found"}` (denials count as `not_found`, like the response they get), and `ferrum_alloy_diagnostics_authorizer_failures_total{reason="timeout"|"panic"}`.

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
