# ADR 0008: Tenant-scoped diagnostic retrieval from a running service

**Status:** Accepted (2026-09-27). Amended (2026-10-03): retention is shared fairly between tenants; records are filed by who chose the request id and bound to the trace of the first record under it; `telemetry.request_id.accept_incoming` defaults to `trusted_peers`.

## Context

Detailed diagnostic evidence leaves a running service only through telemetry export and offline `ferrum-alloy diagnose` files. An operator who holds one request id cannot ask the service for that request's evidence.

Retrieval discloses data about other people's requests, so it needs its own authorization. The management token cannot provide it: it is one shared secret for the whole listener, it names no tenant, and anyone who can scrape `/metrics` holds it. Request ids and trace ids cannot provide it either: a gateway's clients can choose request ids, and so can every caller under `telemetry.request_id.accept_incoming = any`, and both kinds of id appear in logs, headers, and traces. Ferrum Edge's authenticated diagnostic reference (G01, `X-Ferrum-Diagnostic-Ref`) is not produced by any supported Edge release (it is implemented, unreleased, on Edge main at `f6384650`), so retrieval must work without it.

## Decision

### Where evidence is held

- The telemetry layer hands one `RequestEvidence` per finalized request to an optional `EvidenceSink` (`TelemetryLayer::with_evidence_sink`). It is built from values the layer already measures when the request finalizes, at body completion, error, or drop (ADR 0003). Streaming completion therefore comes from telemetry, never from response headers or buffered bodies.
- `AlloyApp` installs a sink only when the application installs an authorizer (below). The sink is a bounded in-memory store in the service process, keyed by tenant, by who chose the request id, and by the id. Nothing is written to disk and nothing survives a restart.
- Evidence is retained only for requests the application attributes to a tenant, through the request's `TenantTag` (a per-request slot, like the route slot, that the first valid write fills). An untagged request is counted and not retained: there is no tenant a retrieval could be authorized for.
- Retention is bounded by count (`diagnostics.max_records`, default 1,024) and by estimated bytes (`diagnostics.max_bytes`, default 1 MiB), shared fairly between tenants. When either bound would be exceeded, another tenant's oldest record is evicted only while that tenant holds more than the new record's tenant will once the record is added (in records under the count bound, in bytes under the byte bound), choosing the tenant that holds the most; otherwise the tenant's own oldest record is evicted. A tenant that holds its share therefore evicts only its own evidence, and busy tenants converge on equal shares. A fixed per-tenant quota was rejected: it would leave the store idle while few tenants are active, or need tuning to the number of tenants. Per-record strings are bounded: request id 256 bytes (the request id alphabet), tenant 128 bytes, route template 512 bytes (a longer template is dropped).
- `telemetry.request_id.accept_incoming` defaults to `trusted_peers`, so a request from any other peer gets an id the service generates. Each record carries who chose its id: `generated`, `trusted_peer`, or `untrusted_caller` (only under `any`). Records whose ids different parties chose are filed apart, and a lookup answers from the most trustworthy origin that has records, so an id a caller chose can neither join nor evict the records of a generated id. Under one id, only records of the trace of its first record are kept together, for example the attempts of a request Ferrum Edge retried, which carry its trace context. At most 16 share one id; a further one evicts the oldest of them, so attempts already filed never keep a later attempt out. A record of another trace is not retained while the first trace's records are, so a request that reuses the id later can neither join nor evict them; it is counted (`skipped_total{reason="request_id_conflict"}`) and the report notes the reuse. Each eviction is counted by reason (`count`, `bytes`, `request_id_limit`).
- The byte bound is an estimate: each record is charged a fixed overhead derived from the sizes of the in-memory structures, plus its strings. Allocator overhead and the spare capacity of the hash tables, which never shrink, are not charged, so the process can use somewhat more than `max_bytes`.

### Authorization

- Retrieval is disabled unless the application installs a `DiagnosticsAuthorizer` (Cargo feature `diagnostics`, off by default and in `full`). Without one, the route does not exist and no evidence is retained.
- The authorizer is application code. It receives the transport peer (`PeerInfo`) and the request headers, and returns either one tenant the caller may read, or a denial. Alloy never derives a tenant from headers itself, and there is no switch that trusts a caller wholesale.
- The management token is neither required nor sufficient for this route. Its bearer credential slot belongs to the authorizer, which verifies a tenant-scoped credential (for example a JWT whose claims name the tenant) or a verified transport identity.
- The authorizer has 5 seconds. A timeout, a denial, and a tenant outside the tag alphabet all deny. So does a panic, whether in the call or in the future it returns: it is counted, Alloy's own warning names neither the request nor its credential, and it is answered like any refusal, never by a dropped connection or a `500`. The process panic hook still prints the panic message, so an authorizer must never put a credential in a panic message. (With `panic = "abort"`, a panic still ends the process.)
- A request id is a lookup key, never a credential, and a trace id is not accepted at all. A lookup matches only records whose tenant equals the authorized tenant.

### Responses

- `GET /diagnostics/v1/requests/{request_id}` on the management listener returns a `ferrum.diagnostic_report` v1 document, `application/json`, with `Cache-Control: no-store`.
- A denied caller, a failed authorizer, a malformed id, an unknown id, an evicted id, and another tenant's id all get byte-identical `404` Problem Details responses, so no response reveals which case applied or whether an id exists. The authorizer runs before the id is examined.
- Response timing is not uniform, and does not need to be. A refused credential skips the lookup, so timing can show whether the authorizer accepted a credential, which the authorizer's own latency already shows. It cannot show whether an id exists: for an accepted credential, another tenant's id and an unknown id are the same miss in the same index, under the same lock.
- `/metrics` counts retrievals as `served` or `not_found`, with denials among the misses, so the management token does not separate refused credentials from unknown ids either. Authorizer timeouts and panics are counted separately, as operational failures.
- Requests are charged to the management rate limiter (#12) before the authorizer runs, like every other management request. `management.rate_limit.exempt_networks` never applies to this route, so no peer gets unlimited authorizer attempts. Startup fails if the authorizer is installed while the management listener or its rate limit is disabled.
- Startup also fails if the management listener binds to a non-loopback address. It has no TLS, and the authorizer's bearer credentials must not cross a network in cleartext. To serve other hosts, terminate TLS in a proxy on the same host that forwards to the loopback listener. The authorizer then sees the proxy as the transport peer, and the per-client rate limit applies to the proxy as a whole.

### What a report contains

- Collection: collector `ferrum-alloy`, method `live_export`, verification `unverified`. The CLI reads it with `parse_offline`, which keeps it unverified, so a live report never yields `confirmed`.
- For each retained attempt: `alloy.server.time_to_headers`, `alloy.server.body_duration`, and `alloy.server.duration` (all already in the catalog and in `docs/measurement-semantics.md`), with availability instead of zero for missing values, and one `alloy.response` event with the status, route template, trace decision, peer trust label, and request id origin. The producer is the service's own telemetry (`ferrum-alloy-telemetry`, producer kind `alloy`), so findings cite it as `service_telemetry` evidence. Findings are not included; readers recompute them (ADR 0005).
- Redaction by construction: the evidence never holds header values, the raw path or query, bodies, SQL text or parameters, credentials, peer addresses, certificate identities, host or instance names, the service version, or error text from upstreams or dependencies. Only the labels listed above leave the process.

### CLI

- `ferrum-alloy diagnose --url <management base URL> --request-id <id>` fetches one report on explicit invocation only, with connect and whole-request timeouts, no redirects, and a response size cap. It sends one `GET` to the service and never writes to a gateway.
- The credential comes from `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or `--token-file`, never from an argument, so it stays out of shell history and process listings. The variable is registered as the command's own (`config::CLI_ENV_VARS`): service configuration ignores it rather than rejecting it as unknown, and never reads it. A credential is sent over plain `http` only to a loopback address.

## Consequences

- Operators can retrieve one request's service-side evidence without a telemetry backend, within the retention window, and only for the tenant their credential names.
- Evidence is per process. Behind a load balancer, retrieval must reach the replica that served the request.
- A caller of tenant A who knows another tenant-A request's id cannot add records to its report or evict it by reusing the id: a reuse in another trace is not retained, and an id a caller chose is filed apart from a generated one. A caller who predicts an id that a gateway will keep from its client can claim it before its owner does, so the owner's request is not retained under it; the report notes the reuse. Across tenants nothing is disclosed: a lookup matches only the authorized tenant's records. Services that must separate callers within a tenant should keep the `trusted_peers` default, let the gateway generate ids, or tag requests with a finer-grained tenant.
- A request whose attempts arrive in different traces, for example a client that retries with the same id through a gateway that re-roots its trace context, keeps only the first trace's attempts.
- Fair sharing is per tenant tag. An application that lets one caller act as many tenants gives it many shares.
- Changing the request id default means direct callers no longer see their own ids echoed; deployments that relied on it set `accept_incoming = "any"`, which validation warns about.
- Operation-level evidence (`alloy.operation.duration`, `alloy.db.pool_wait`, `alloy.admission.wait`) is not retained yet. Reports hold server-level timing only; OTLP export remains the source for operations.
- Tagging costs one allocation per request, and retention one short critical section per finalized request, only when an authorizer is installed. The benchmark scenario `alloy-diagnostics` measures it; no result exists yet.
