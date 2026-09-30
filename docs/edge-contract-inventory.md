# Ferrum Edge contract inventory

This document lists every Ferrum Edge header, attribute, endpoint, error token, and timing field Ferrum Alloy relies on, emits, or deliberately does not rely on. Each entry gives its source, producer, consumer, trust rules, lifecycle boundary, protocol coverage, tests, and status. Every item was checked against Edge source; nothing is listed as a contract on the strength of an example or design note alone.

**Status values**

| Status | Meaning |
|---|---|
| **EXISTING** | Present in the Edge release below and verified in its source. For Alloy-owned items, implemented in this repository. |
| **PROPOSED** | Defined by Alloy, or by another Ferrum product's design document, but not implemented by its would-be producer or consumer. |
| **UNAVAILABLE** | Not produced by Edge v0.9.8 (the contract baseline) or v0.9.7. Alloy must not assume it and reports it as missing evidence. |

## Revisions inspected

| Repository | Revision | Role |
|---|---|---|
| ferrum-edge/ferrum-edge | `v0.9.8` = `e27f2109216352c3fe9e67a7014611f3f66daa91` (2026-09-27) | **Contract baseline.** The latest published release and the default image (`ferrumedge/ferrum-edge@sha256:e5b204f9…b385`). Line references marked "v0.9.8" are from this revision. |
| ferrum-edge/ferrum-edge | `v0.9.7` = `8fed1346ce2e267eb69c03683cb89ea44d785e0b` (2026-09-25) | Previous release, still supported and tested in CI (`ferrumedge/ferrum-edge@sha256:4c9530e0…874a`). Differs from v0.9.8 in the entries marked "v0.9.8" in §3. |
| ferrum-edge/ferrum-edge | `05997cee91bb4e1fa3dd1e64506b1512c7b16b8a` (main, 2026-09-24) | Unmarked line references below. `src/plugins/otel_tracing.rs`, `src/plugins/correlation_id.rs`, and `src/config/types.rs` are byte-identical to v0.9.7 and v0.9.8. Line numbers in `src/proxy/mod.rs`, `src/proxy/headers.rs`, `src/plugins/mod.rs`, `src/health_check.rs`, `src/admin/mod.rs`, and `src/config/env_config.rs` differ at both releases. |
| ferrum-edge/ferrum-edge | `f638465034734e335bde7e76c56e8c8244afba8a` (main, 2026-09-30) | Read only for the G01 entry in §6, which is implemented there but unreleased. Not a supported or pinned revision. |
| ferrum-edge/ferrum-anvil | `075890f9418718113d5d83da4074c6b00b92bde9` (origin/main) | `DiagnosticFinding` schema, Edge v0.9.7 outcome catalog, G01 proposal. |
| ferrum-edge/ferrum-foundry | `e2f60b1eb0e881b3302ee7416df2d085f757dec4` | Pinned-Edge approach (`docs/compatibility.json`). No diagnostic UI. |
| ferrum-edge/ferrum-nexus | `b803a95cf4afd012d3cbb93324f5beac937b20c0` | OpenAPI 3.x publication through Edge `/api-specs`. |
| ferrum-edge/ferrum-edge-git-forge-ops | `fa56bd790e848544fdc9fea1a1ac3024d514a3b6` | `kind`/`spec` resource format. |
| ferrum-edge/ferrumedge | `0b796393b63e12a9fd643430446620dc23f140a8` | Website. Does not mention Alloy. |

**v0.9.8 re-audit (2026-09-27).** Following the checklist in [compatibility.md](compatibility.md#re-audit-checklist) against `v0.9.7..v0.9.8`:

- `src/plugins/otel_tracing.rs`, `src/plugins/correlation_id.rs`, and `src/config/types.rs` are unchanged, so §1 `traceparent`/`tracestate`/`x-request-id`, §4, and §7 carry over.
- `src/retry.rs` adds the eighth `X-Gateway-Error` token `request_timeout` and narrows `backend_timeout` (§3). Alloy mirrors the token in `contract::GATEWAY_ERROR_TOKENS` and the diagnostics catalog.
- `src/proxy/headers.rs` now strips backend copies of `X-Gateway-Error` and `X-Gateway-Upstream-Status` (§3). Header trust in Alloy is unchanged.
- The reserved `x-consumer-*` names (§1) are unchanged. The `rejection_phase` values behind `catalog::EDGE_PRE_UPSTREAM_PHASES` (§5) are unchanged except the HTTP/3-only `route_request_timeout_h3_upload` (Edge v0.9.8), which is not yet mapped.
- Every UNAVAILABLE entry is still unavailable: v0.9.8 exports no attempt identity, connection timing, or diagnostic reference.
- The remaining EXISTING entries in `src/proxy/mod.rs`, `src/plugins/mod.rs`, `src/health_check.rs`, and `src/admin/mod.rs` were spot-checked (the functions and strings they cite are present). The e2e and `edge-config` jobs cover them on both releases. Their unmarked line numbers remain those of `05997cee`.

## 1. Request metadata Edge sends to the service

| Item | Status | Source (Edge) | Producer → consumer | Trust rules | Lifecycle | Protocols | Tests |
|---|---|---|---|---|---|---|---|
| `traceparent` | EXISTING | `src/plugins/otel_tracing.rs:978-998` (`before_proxy`), parse `:675-711` | `otel_tracing` plugin → Alloy telemetry layer | Edge removes every case variant and inserts its own value. The parent id is the **Edge SERVER span**; flags are only `00`/`01`. Alloy accepts it only from a trusted peer (default `trusted_peers`). Receiving it never authenticates anything. | Request head, once per request. **Every retry attempt carries the same value** (`src/proxy/mod.rs` `proxy_to_backend_retry` reuses headers). | HTTP/1.1 and HTTP/2 to Alloy. The e2e run used HTTP/2 via ALPN. | Edge: `tests/unit/plugins/otel_tracing_tests.rs` (`…propagates_existing_traceparent`, `…before_proxy_replaces_all_caller_trace_context_casings`, `…untrusted_parent_creates_fresh_root`). Alloy: `crates/ferrum-alloy-telemetry/tests/context_and_trust.rs`, `crates/ferrum-alloy/tests/gateway_mtls.rs`, `edge-e2e` (including the retry case: two Alloy SERVER spans with the same Edge parent) |
| `tracestate` | EXISTING | `otel_tracing.rs:924`, `:941-944` | Edge → Alloy | Forwarded verbatim only when Edge trusted the caller's context (`trace_context_trust: trusted`); otherwise dropped. Edge adds no member. Alloy validates the W3C grammar (≤32 members, ≤512 bytes) and propagates only accepted values. | Request head | HTTP/1.1, HTTP/2 | Edge: `…preserves_tracestate`, `…invalid_parent_drops_tracestate`. Alloy: `tracestate_validation_follows_the_w3c_grammar` |
| `baggage` | EXISTING (pass-through) | Not touched by `otel_tracing`. Mesh egress strip only: `src/proxy/mod.rs:32881-32894` | Client → Edge → service | Untrusted. Alloy never parses it and never forwards it; `http_client` strips it. | Request head | all | Alloy: `trace_context_goes_only_to_listed_hosts` |
| `x-request-id` | EXISTING (when the `correlation_id` plugin is attached) | `src/plugins/correlation_id.rs:103`, `:209-268`, `:289-330` | `correlation_id` → Alloy | Edge keeps a client value of at most 256 bytes of `[A-Za-z0-9._-]`, otherwise generates a UUIDv4. Alloy applies the same rule and reuses Edge's id. It is a correlation aid, not a credential. Without the plugin Edge sends no request id, and Alloy generates one. | Request head; Edge echoes it on responses and rejects | all | Edge: `tests/unit/plugins/correlation_id_tests.rs`. Alloy: `valid_request_ids_are_kept_and_echoed`, `invalid_or_oversized_request_ids_are_replaced`, `edge-e2e` ("Alloy used the gateway's request id") |
| `x-consumer-username` | EXISTING | Doc `docs/plugins.md:288-297`. Built `src/plugins/mod.rs:6516-6560`, injected `src/proxy/mod.rs:16606-16645`, client copies stripped `src/plugins/mod.rs:6142-6146` (v0.9.8 `:6363-6368`) | Edge auth plugins → Alloy Edge adapter (`GatewayContext`) | Accepted only when the peer is a **verified mTLS identity** in `trust.identities` and `edge.accept_consumer_identity = true`. Otherwise removed before handlers run. Network-boundary trust never authorizes it. Authentication by Edge; authorization stays in the application. | Request head | all | Alloy: `crates/ferrum-alloy-edge/tests/policy.rs`, `gateway_mtls.rs` |
| `x-consumer-custom-id` | EXISTING | same as above | same | same | same | same | same |
| `x-consumer-*` (other names) | EXISTING gap | Only the two exact names are reserved on the plain HTTP path (`src/plugins/mod.rs:6142-6146`; v0.9.8 `:6363-6368`) | client → service | A client can send, for example, `X-Consumer-Role` through Edge. Alloy trusts no other `x-consumer-*` name. | — | HTTP | — |
| `X-Forwarded-For` | EXISTING | `src/proxy/mod.rs:4884-4927` (`build_xff_value`) | Edge → Alloy | Regenerated. An untrusted chain is dropped unless the peer is in `FERRUM_TRUSTED_PROXIES`, so the rightmost hop is written by Edge. Alloy exposes it only as `GatewayContext.client_address` from a verified identity, and never uses it for trust decisions. | Request head | all | Edge: `tests/functional/functional_forwarded_via_headers_test.rs`. Alloy: `verified_gateway_identity_is_handed_off`, `forwarded_headers_never_establish_trust` |
| `X-Forwarded-Proto`, `X-Forwarded-Host` | EXISTING | `src/proxy/mod.rs:42075-42086`, `src/proxy/headers.rs:127-170` | Edge → service | Overwritten by Edge. Alloy does not consume them. | Request head | all | Edge functional tests |
| `Forwarded` | EXISTING (opt-in) | `FERRUM_ADD_FORWARDED_HEADER`, default `false` (`src/config/env_config.rs:4869`) | Edge → service | When disabled, **a client's value passes through**. Alloy ignores it. | Request head | all | Edge functional tests |
| `Via` | EXISTING | `src/proxy/mod.rs:9782-9791`, default on (`env_config.rs:4867`) | Edge → service | Appended; spoofable. Not an Edge marker for Alloy. | Request and response | all | — |
| `X-Real-IP`, `X-Forwarded-Port` | EXISTING | `src/proxy/headers.rs:180-183` | client → service | Not generated. `X-Forwarded-Port` is not stripped. Alloy ignores both. | Request head | all | — |
| `x-geo-country`, `x-path-param-*` | EXISTING | `src/plugins/mod.rs:6142-6146` | Edge → service | Stripped from clients, injected by Edge. Alloy does not consume them yet. | Request head | all | — |
| Route id header (`x-ferrum-route-id` or similar) | **UNAVAILABLE** | none (`grep x-ferrum` finds only internal and admin names) | — | Alloy uses its own route template. | — | — | — |
| Attempt number or attempt id to the backend | **UNAVAILABLE** | none | — | Alloy reports it as missing evidence. Retries appear as sibling Alloy SERVER spans under one Edge SERVER span. | — | — | Alloy rule `alloy.gateway.multiple_service_attempts` |
| Diagnostics-request header | **UNAVAILABLE** | none | — | — | — | — | — |
| Signed gateway context | **UNAVAILABLE** | none | — | Alloy relies on mTLS identity instead. It does not add ad hoc signatures. | — | — | — |

## 2. Gateway identity toward the service

| Item | Status | Source | Notes | Tests |
|---|---|---|---|---|
| Backend client certificate: `backend_tls_client_cert_path` / `backend_tls_client_key_path` (proxy, upstream, or global `FERRUM_BACKEND_TLS_CLIENT_CERT_PATH`) | EXISTING | `src/config/types.rs:2720-2724` (proxy), `:1928-1931` (upstream); `docs/backend_mtls.md:24-35` | With `upstream_id` set, the upstream's TLS settings win. Pointing the paths at SPIFFE SVID files presents an SVID. Edge does not present an SVID automatically. | e2e: Edge presents `spiffe://ferrum.demo/ns/edge/sa/gateway` and Alloy verifies it (`peer.trust=verified_identity`) |
| Server verification: `backend_tls_verify_server_cert` (default true), `backend_tls_server_ca_cert_path` | EXISTING | `types.rs:2728-2732`, `:1934-1937` | A custom CA replaces public roots. | e2e (Alloy's demo CA) |
| Active health probes present the backend client certificate | EXISTING | `src/health_check.rs:2729-2750`, `:3661-3683` | So `client_auth = required` works with Edge health checks. | Generated upstream in the e2e stack |

## 3. Edge responses and error signals

| Item | Status | Source | Semantics | Trust | Alloy use |
|---|---|---|---|---|---|
| `X-Gateway-Error` | EXISTING | v0.9.8: `src/retry.rs:212-244` (tokens), `:310-318` (class mapping); backend copies stripped by `src/proxy/headers.rs:754-757`, `:777-816` | Closed vocabulary, on gateway-authored 5xx only: `connection_failure`, `backend_timeout`, `backend_error`, `circuit_breaker_open`, `overload`, `config_stale`, `concurrency_limit`, and, from v0.9.8, `request_timeout` (a route's total request deadline expired before any backend held the request). In v0.9.8 `backend_timeout` means a backend held the request; in v0.9.7 it also covered route deadlines that expired before dispatch. | v0.9.8 strips backend-supplied copies at every backend response boundary. v0.9.7 does not on every path (Anvil `catalog/ferrum/ferrum-edge-0.9.7/outcomes.json`). The header is unauthenticated and names no Edge version, so it stays unverified evidence. | Diagnosis rule `alloy.r007` caps confidence at `likely`, keeps the broader v0.9.7 meaning of `backend_timeout`, and lists what each token does not prove. For example, `connection_failure` does not prove a DNS failure. An unknown token yields `unknown`. `edge-e2e` checks `connection_failure` for a refused backend connection on both releases. |
| `X-Gateway-Upstream-Status: degraded` | EXISTING | `src/proxy/mod.rs:40214`; v0.9.8 `:41018`, `:41120` | The all-unhealthy fallback target was used. | Stripped from backend responses in v0.9.8 (same list as `X-Gateway-Error`); spoofable in v0.9.7 | Not interpreted yet |
| Gateway error bodies `{"error":"…"}` | EXISTING | `src/proxy/mod.rs:25817`, `:48013-48040` | Plain JSON, not Problem Details. | — | Not parsed. Body text is weak evidence (Anvil convention). |
| `traceparent` echoed to the client | EXISTING | `otel_tracing.rs:1000-1014` (`after_proxy`) | The same value Edge sent upstream. | — | `edge-e2e` checks it equals the Alloy span's parent. |
| `Server-Timing` | EXISTING (untouched) | No references in `src/`, `tests/`, `docs/` | Edge neither emits nor strips it. It is not in the backend-response strip set (`src/proxy/headers.rs:745-757`). | — | Alloy's opt-in `Server-Timing` would reach clients unchanged. **Not tested through Edge.** |

## 4. Edge telemetry (OTLP) Alloy interprets

Edge exports **OTLP/HTTP JSON only** (`otel_tracing.rs:2165-2173`), hand-written without an OTel SDK, and only **SERVER** spans (`:140-173`). There are no CLIENT spans for upstream attempts and no per-retry spans. The span ends at transaction summary time, which for streamed responses is body completion (`src/proxy/deferred_log.rs`).

| Attribute | Status | Source | Meaning | Alloy handling |
|---|---|---|---|---|
| `gateway.latency.total_ms` | EXISTING | `otel_tracing.rs:2419` | Handler entry until summary; refreshed at body completion for streamed responses. | `edge.request.total` |
| `gateway.latency.backend_ttfb_ms` | EXISTING | `:2420` | Backend dispatch start until response headers, **across every retry attempt and backoff**. For buffered responses it **equals the full backend exchange** (`src/proxy/mod.rs:39286-39292`). Always exported; `-1` means unknown. | `edge.backend.time_to_headers`. A negative value becomes `unavailable`, never zero. |
| `gateway.latency.backend_total_ms` | EXISTING | `:2448-2452` | Buffered responses only. Omitted when unknown. | `edge.backend.total` |
| `gateway.latency.processing_ms`, `gateway.overhead_ms`, `gateway.plugin_execution_ms` | EXISTING | `:2424-2458` | Derived (`src/plugins/mod.rs:8234-8251`). | `edge.plugin_execution` only |
| `gateway.response.streamed` | EXISTING | `:2538-2559` | Whether the response streamed. | Selects which Alloy measurement is comparable (rule `alloy.r003`). |
| `gateway.error.class` | EXISTING | `:2538-2559`, classes `src/retry.rs:23-173` | Typed gateway failure class (19 values). | `edge.gateway_error` event |
| `gateway.proxy.id` | EXISTING | `:2466` | Proxy id. | Attribute only |
| `http.route` | EXISTING, **different meaning** | `:3482-3516` | Holds the **proxy name**, not a path template. | Never compared with Alloy's `http.route`. |
| `http.response.status_code` | EXISTING | `:2433` | Final status. | `edge.response` event |
| Resource `telemetry.sdk.name = "ferrum-edge"`, scope `ferrum-edge` | EXISTING | `:2639-2659` | Producer identification. | Used by the OTLP importer, which marks provenance `unverified`. |
| Sampling: `root_sampling`, `root_sampling_ratio`; parent-based only for trusted callers | EXISTING | `:3088-3149`, `:3433-3442` | An untrusted caller's sampled flag has no effect. | Matches Alloy's policy. |
| Semantic-convention version / `schema_url` | **UNAVAILABLE** | none | Names match the stable HTTP conventions. | — |
| Per-attempt duration, connection setup (DNS/TCP/TLS), connection reuse flag | **UNAVAILABLE** | `final_backend_dispatch_elapsed` exists internally but is not exported (`src/proxy/mod.rs:39231`; v0.9.8 `:39993`, `otel_tracing.rs` unchanged) | — | Reported as `missing_evidence` in diagnosis. Never inferred. |

## 5. Edge transaction logs

These fields exist in Edge access logs (`TransactionSummary`, `src/plugins/mod.rs:7893-8123`). Alloy reads them only when an operator supplies them in a diagnostic report; there is no automated collection.

| Field | Status | Meaning |
|---|---|---|
| `latency_total_ms`, `latency_backend_ttfb_ms`, `latency_backend_total_ms` | EXISTING | As in §4; `-1` means unknown. |
| `metadata.rejection_phase` | EXISTING | The phase that rejected before upstream: `authenticate`, `authorize`, `before_proxy`, `on_request_received`, `circuit_breaker_open`, … Maps to Alloy's `edge.request.rejected` (`phase`). Plugin identity is not recorded. |
| `error_class`, `body_error_class` | EXISTING | Typed classes (`src/retry.rs:151-173`) |
| Warning log `"Retrying backend request"` with `attempt` | EXISTING | `src/proxy/mod.rs:38614-38619` (v0.9.8 `:39282`). Log only; not in spans or summaries. |

## 6. Endpoints

| Endpoint | Status | Notes |
|---|---|---|
| Edge active health check `GET {http_path}` (default `/health`, healthy `[200, 302]`) | EXISTING | `src/config/types.rs:1463-1526`, `src/health_check.rs:85-92`, `:3242-3277`. Alloy's export sets `http_path` to the manifest's `health.path` (typically `/readyz`) and `healthy_status_codes: [200]`. |
| Edge admin `POST/PUT/GET/DELETE /api-specs` | EXISTING | `src/admin/mod.rs:3673-3716`, `docs/api_specs.md`. Not available in file mode. **Alloy never calls it.** `ferrum-alloy openapi export` produces the artifact that operators or Nexus publish. |
| G01 authenticated diagnostic lookup: `X-Ferrum-Diagnostic-Ref`, `GET /diagnostics/v1/refs/{ref}`, `diagnostics:read` | **UNAVAILABLE** (ferrum-edge#5767) | Not produced by v0.9.8 or v0.9.7. Implemented on Edge main at `f6384650` (`src/admin/mod.rs:2679-2711`, `:3749-3751`; `X_FERRUM_DIAGNOSTIC_REF_HEADER` in `src/proxy/headers.rs:842`), not yet released; the pinned `contracts-edge-0.9.8` `vocabularies/gateway-headers.json` marks the header `unreleased`. Alloy does not rely on it. Once a supported Edge release ships it, Alloy would treat it as `gateway_detail` evidence. |
| Alloy `/livez`, `/readyz` (application and management listeners) | EXISTING (Alloy) | Status only, `no-store` |
| Alloy management `/health`, `/metrics`, `/openapi.json` | EXISTING (Alloy) | Bearer token when configured; loopback bind by default |

## 7. Configuration schema Alloy generates

`ferrum-alloy edge export` writes only fields that exist in the `deny_unknown_fields` resources of Edge v0.9.8 and v0.9.7 (identical `src/config/types.rs`: `Proxy` 2645, `Upstream` 1842, `PluginConfig` 3100, `GatewayConfig` 3251) and GitForgeOps's `kind`/`spec` wrapper (`src/config/strict.rs:321-332`).

| Generated item | Status | Validation |
|---|---|---|
| Proxy: `listen_path`, `backend_scheme` (never `backend_protocol`), `strip_listen_path`, `backend_path`, `backend_*_timeout_ms`, `upstream_id`, `plugins`, `labels`, `backend_tls_*` | EXISTING | CI: `ferrum-edge validate -m file` passes on v0.9.8 and v0.9.7 for `contracts/fixtures/manifests/plain-http.edge.yaml` and for the e2e TLS config. The e2e stack serves traffic with it. |
| Upstream with `health_checks.active` and backend TLS on the upstream | EXISTING | same |
| `correlation_id` plugin config (`header_name`, `echo_downstream`) | EXISTING | same |
| `otel_tracing` plugin config (`endpoint`, `service_name`, `trace_context_trust: untrusted`, `include_url_path: false`, optional `root_sampling`/`root_sampling_ratio`) | EXISTING | same; keys checked against `ALLOWED_CONFIG_KEYS` (`otel_tracing.rs:63-81`) |
| End-to-end stack only (`gen-e2e-edge-config`, not `edge export`): proxy `backend_host`, `backend_port`, and `retry` (`max_retries`, `retryable_status_codes`, `retryable_methods`, `retry_on_connect_failure`); Edge environment `FERRUM_POOL_WARMUP_ENABLED`, `FERRUM_POOL_HTTP2_CONNECTIONS_PER_HOST` | EXISTING | `ferrum-edge validate` in the `edge-e2e` job on v0.9.8 and v0.9.7; the stack serves traffic with it |
| Service manifest `ferrum.service_manifest` v1 | **PROPOSED** | Alloy-defined. No Nexus, Foundry, or GitForgeOps consumer. |

## 8. Alloy-owned telemetry (produced by this repository)

| Item | Status | Meaning |
|---|---|---|
| Alloy SERVER span, scope `ferrum-alloy-telemetry` | EXISTING | Parent = the accepted remote parent (the Edge SERVER span behind Edge), otherwise a new root. |
| `alloy.trace.parent` | EXISTING | `accepted_remote`, `root`, `rerooted_untrusted`, `rerooted_invalid`, or `ignored_by_policy` |
| `alloy.peer.trust` | EXISTING | `verified_identity`, `network_boundary`, or `untrusted` |
| `alloy.request_id` | EXISTING | Validated request id |
| `alloy.server.time_to_headers_ms`, `alloy.server.body_duration_ms`, `alloy.server.duration_ms` | EXISTING | See [measurement-semantics.md](measurement-semantics.md) |
| `alloy.response.body.outcome`, `alloy.response.body.bytes`, `alloy.response.upgraded` | EXISTING | Body finalization |
| `alloy.admission.wait_ms` | EXISTING | Admission wait when enabled |
| Operation spans with `alloy.operation.duration_ms`, `alloy.operation.kind`, `alloy.db.pool_wait_ms` | EXISTING | Explicitly instrumented operations |
| Diagnostic report `ferrum.diagnostic_report` v1 | EXISTING in Alloy; the shared schema is vendored and pinned from `contracts-edge-0.9.8`, and its shared status is still **PROPOSED** | Alloy's Finding remains a superset of Anvil's `DiagnosticFinding`. The shared tag fixtures are validated against Alloy's Finding schema and deserialized into Alloy's `Finding` type; Anvil import is **not tested**. |

## 9. Cross-repository dependencies

Alloy does not implement these, and does not claim them:

1. **Edge**: per-attempt CLIENT spans or attempt identity, connection-setup and reuse evidence, and the G01 authenticated diagnostic reference. These are needed for `confirmed` gateway-vs-service timing attribution. All are UNAVAILABLE in the supported releases. G01 is implemented on Edge main at `f6384650` but not yet released.
2. **Edge**: stripping every client-supplied `x-consumer-*` header, not only the two exact names.
3. **Anvil**: accepting `gateway_telemetry` / `service_telemetry` evidence sources and importing `ferrum.diagnostic_report`.
4. **Nexus / Foundry / GitForgeOps**: consuming `ferrum.service_manifest`. None do.
5. **Website**: no Alloy page exists. Any future page should say "in development" or "preview", not "tested" or "released".
