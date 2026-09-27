# ADR 0008: Tenant-scoped diagnostic retrieval from a running service

**Status:** Accepted (2026-09-27)

## Context

Detailed diagnostic evidence leaves a running service only through telemetry export and offline `ferrum-alloy diagnose` files. An operator who holds one request id cannot ask the service for that request's evidence.

Retrieval discloses data about other people's requests, so it needs its own authorization. The management token cannot provide it: it is one shared secret for the whole listener, it names no tenant, and anyone who can scrape `/metrics` holds it. Request ids and trace ids cannot provide it either: callers choose request ids (`telemetry.request_id.accept_incoming` defaults to `any`), and both kinds of id appear in logs, headers, and traces. Ferrum Edge's authenticated diagnostic reference (G01, `X-Ferrum-Diagnostic-Ref`) is PROPOSED and not implemented, so retrieval must work without it.

## Decision

### Where evidence is held

- The telemetry layer hands one `RequestEvidence` per finalized request to an optional `EvidenceSink` (`TelemetryLayer::with_evidence_sink`). It is built from values the layer already measures when the request finalizes, at body completion, error, or drop (ADR 0003). Streaming completion therefore comes from telemetry, never from response headers or buffered bodies.
- `AlloyApp` installs a sink only when the application installs an authorizer (below). The sink is a bounded in-memory ring in the service process, keyed by tenant and request id. Nothing is written to disk and nothing survives a restart.
- Evidence is retained only for requests the application attributes to a tenant, through the request's `TenantTag` (a per-request slot, like the route slot, that the first valid write fills). An untagged request is counted and not retained: there is no tenant a retrieval could be authorized for.
- Retention is bounded by count (`diagnostics.max_records`, default 1,024) and by estimated bytes (`diagnostics.max_bytes`, default 1 MiB). The oldest records are evicted first when either bound would be exceeded, and each eviction is counted by reason (`count`, `bytes`). At most 16 records share one tenant and request id, for example the attempts of a retried request; further ones are counted and dropped. Per-record strings are bounded: request id 256 bytes (the request id alphabet), tenant 128 bytes, route template 512 bytes (a longer template is dropped).

### Authorization

- Retrieval is disabled unless the application installs a `DiagnosticsAuthorizer` (Cargo feature `diagnostics`, off by default and in `full`). Without one, the route does not exist and no evidence is retained.
- The authorizer is application code. It receives the transport peer (`PeerInfo`) and the request headers, and returns either one tenant the caller may read, or a denial. Alloy never derives a tenant from headers itself, and there is no switch that trusts a caller wholesale.
- The management token is neither required nor sufficient for this route. Its bearer credential slot belongs to the authorizer, which verifies a tenant-scoped credential (for example a JWT whose claims name the tenant) or a verified transport identity.
- The authorizer has 5 seconds. A timeout, a denial, and a tenant outside the tag alphabet all deny.
- A request id is a lookup key, never a credential, and a trace id is not accepted at all. A lookup matches only records whose tenant equals the authorized tenant.

### Responses

- `GET /diagnostics/v1/requests/{request_id}` on the management listener returns a `ferrum.diagnostic_report` v1 document, `application/json`, with `Cache-Control: no-store`.
- A denied caller, a malformed id, an unknown id, an evicted id, and another tenant's id all get the same `404` Problem Details response, byte for byte, so none of them can be told apart or used to enumerate ids. The authorizer runs before the id is examined.
- Requests are charged to the management rate limiter (#12) before the authorizer runs, like every other management request. Startup fails if the authorizer is installed while the management listener or its rate limit is disabled.

### What a report contains

- Collection: collector `ferrum-alloy`, method `live_export`, verification `unverified`. The CLI reads it with `parse_offline`, which keeps it unverified, so a live report never yields `confirmed`.
- For each retained attempt: `alloy.server.time_to_headers`, `alloy.server.body_duration`, and `alloy.server.duration` (all already in the catalog and in `docs/measurement-semantics.md`), with availability instead of zero for missing values, and one `alloy.response` event with the status, route template, trace decision, and peer trust label. The producer is the service's own telemetry (`ferrum-alloy-telemetry`, producer kind `alloy`), so findings cite it as `service_telemetry` evidence. Findings are not included; readers recompute them (ADR 0005).
- Redaction by construction: the evidence never holds header values, the raw path or query, bodies, SQL text or parameters, credentials, peer addresses, certificate identities, host or instance names, the service version, or error text from upstreams or dependencies. Only the labels listed above leave the process.

### CLI

- `ferrum-alloy diagnose --url <management base URL> --request-id <id>` fetches one report on explicit invocation only, with connect and whole-request timeouts, no redirects, and a response size cap. It sends one `GET` to the service and never writes to a gateway.
- The credential comes from `FERRUM_DIAGNOSTICS_TOKEN` or `--token-file`, never from an argument, so it stays out of shell history and process listings. The variable deliberately lacks the `FERRUM_ALLOY_` prefix, which the service configuration reserves and rejects when unknown. A credential is sent over plain `http` only to a loopback address.

## Consequences

- Operators can retrieve one request's service-side evidence without a telemetry backend, within the retention window, and only for the tenant their credential names.
- Evidence is per process. Behind a load balancer, retrieval must reach the replica that served the request.
- Operation-level evidence (`alloy.operation.duration`, `alloy.db.pool_wait`, `alloy.admission.wait`) is not retained yet. Reports hold server-level timing only; OTLP export remains the source for operations.
- Tagging costs one allocation per request, and retention one short critical section per finalized request, only when an authorizer is installed. The benchmark scenario `alloy-diagnostics` measures it; no result exists yet.
