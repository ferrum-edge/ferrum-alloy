# Security model

This document states what Alloy trusts, why, and what it deliberately does not protect against. Deployment choices that weaken these properties are called out explicitly.

## Separate concerns

Alloy keeps five decisions separate. None of them implies another.

| Concern | Decided by | Alloy mechanism |
|---|---|---|
| **Transport trust**: is the direct peer the gateway (or another trusted hop)? | The TLS stack's verified client certificate, or a configured network boundary | `[trust]` → `TrustedPeers`. Never forwarded headers. |
| **End-user authentication**: who is the caller? | A verified JWT, or Ferrum Edge's consumer identity from a verified gateway | `jwt` feature; `edge.accept_consumer_identity` |
| **Application authorization**: may this caller do this? | Application code | `jwt::Authorize`, handlers |
| **Tenant / namespace authorization** | Application code | Not provided by Alloy |
| **Diagnostic disclosure**: who may see detailed health, metrics, or diagnostic evidence? | Management token and loopback binding for health and metrics; an application-supplied authorizer, per tenant, for one request's evidence; offline tooling | Management listener; `DiagnosticsAuthorizer` (feature `diagnostics`); `ferrum-alloy diagnose` |

An authenticated gateway connection does not authorize end-user operations or grant access to other tenants' data. A trace id or request id is an identifier, never a credential.

## Transport trust

`trust.identities` lists exact identities accepted from **verified** client certificates:

- `spiffe://…` ids, taken from the leaf's single SPIFFE URI SAN;
- `dns:<name>` DNS SANs.

rustls verifies the chain against `server.tls.client_ca_path` during the handshake, and against the CRLs in `server.tls.client_crl_paths` when any are configured (see [TLS](#tls)). `TlsPeer::from_verified_leaf` is called only for certificates rustls accepted. An unverified or unparsable certificate yields no identity. A leaf with more than one SPIFFE URI is treated as having none.

`trust.networks` treats source addresses as a trusted termination boundary. Use it only when the path is isolated, for example a sidecar on loopback with the service bound to `127.0.0.1`, or a network policy that prevents direct access. `0.0.0.0/0` and `::/0` are rejected.

Network trust is weaker than identity:

- It never authorizes gateway-asserted identity headers. Configuration validation refuses `accept_consumer_identity` without `trust.identities`.
- `gateway_required` requires a verified identity.

Trust controls only:

- whether an incoming `traceparent`/`tracestate` becomes the parent (`telemetry.trace_context.accept_incoming`, default `trusted_peers`);
- whether an incoming request id is kept, when `request_id.accept_incoming = "trusted_peers"` (the default `any` keeps validated ids from anyone, as Ferrum Edge does);
- whether `Server-Timing` is emitted, when `server_timing = "trusted_peers"`;
- whether Edge consumer identity is accepted (`edge` feature).

## Ferrum Edge deployment modes (feature `edge`)

| Mode | Request without a verified gateway identity | Request from a verified gateway identity |
|---|---|---|
| `standalone` (default) | Served, without `GatewayContext`. | Served. |
| `gateway_preferred` | Served, without `GatewayContext`. | Served. |
| `gateway_required` | `403 gateway-required`, except the configured liveness and readiness paths, so kubelet and Edge probes work. | Served. |

In every mode, `x-consumer-username` and `x-consumer-custom-id` are removed unless the peer is a verified identity in `trust.identities` **and** `edge.accept_consumer_identity = true`. In that case handlers get a `GatewayContext` with the consumer identity. `standalone` and `gateway_preferred` currently behave the same; only `gateway_required` rejects requests.

The recommended first deployment is Edge presenting an X.509-SVID through `backend_tls_client_cert_path`, with Alloy using `client_auth = "optional"` or `"required"`, `trust.identities = ["spiffe://…/gateway"]`, and `edge.mode = "gateway_required"`. The `edge-observability` example and CI job run exactly this.

If a sidecar terminates TLS instead, Alloy sees a plaintext loopback connection and must rely on `trust.networks`. The deployment must then guarantee that only the sidecar can reach the service port. Alloy cannot verify that.

Edge v0.9.8 and v0.9.7 reserve only `x-consumer-username` and `x-consumer-custom-id` on the plain HTTP path. Other `x-consumer-*` names sent by clients pass through Edge. Alloy trusts only those two names, and only from a verified identity.

## Trace context and sampling

- The W3C parser matches Edge's. Invalid or duplicated `traceparent` headers are re-rooted, never "repaired".
- For an untrusted peer, Alloy starts a new trace and removes `traceparent`/`tracestate` from the request before handlers see it, so naive forwarding cannot leak caller-chosen ids. Optionally it records the untrusted context as a span link (`link_untrusted_parent`, off by default because links point at caller-chosen trace ids).
- Sampling is parent-based only for accepted parents. An untrusted caller's sampled flag cannot force export (`untrusted_callers_cannot_force_sampling`).
- `tracestate` is propagated only when accepted and valid, and Alloy never adds its own members. `baggage` is never parsed or propagated.
- To correlate a re-rooted request, an operator uses the response `x-request-id` (present when the response is not shared-cacheable, see below), and the span link if enabled.

## Response headers and caches

`x-request-id` echo and `Server-Timing` are request-specific. Alloy adds them only when a shared cache cannot store the response, and otherwise withholds them and counts the suppression (`ferrum_alloy_response_header_suppressions_total{reason="shared_cacheable"}`). Following RFC 9111 §3, §3.5, and §4.2.2, a response is treated as storable when:

- it carries explicit freshness (`public`, `max-age`, `s-maxage`, or `Expires`) and the method is GET, HEAD, or POST; or
- it answers a GET or HEAD with a heuristically cacheable status (200, 203, 204, 206, 300, 301, 308, 404, 405, 410, 414, 501), even with no freshness information and no validators. `Last-Modified` only feeds the heuristic freshness lifetime; it is not required for storage.

A response is not storable when it carries `Cache-Control: no-store` or an unqualified `private`, when the method is not GET, HEAD, or POST, or when the request had `Authorization` and the response lacks `public`, `s-maxage`, or `must-revalidate`. `no-cache` and qualified `private="…"` still permit storage, so they do not restore the headers.

Consequently a plain GET 200 or 404 without cache headers carries neither header. Alloy never changes the application's caching headers to make room for its own. An application that wants the echo or `Server-Timing` on such a response marks it `Cache-Control: private` or `no-store`, which is also the correct policy for any per-user or per-request body. Behind Ferrum Edge with the `correlation_id` plugin, Edge echoes `x-request-id` to the client on every response regardless.

`Server-Timing` is off by default. When enabled it carries one bounded value, the service time to response headers, which may still reveal timing side channels such as authentication paths. Enable it for trusted peers only unless that is acceptable.

## Management surface

- The listener binds to `127.0.0.1:9090` by default.
- A non-loopback bind requires a bearer token of at least 32 characters, compared in constant time.
- `/livez` and `/readyz` return only a status. `/health` (check names and errors), `/metrics`, `/openapi.json`, and the documentation UI (feature `openapi-ui`, `openapi.ui`) require the token when one is configured. `/diagnostics/v1/requests/{request_id}` (feature `diagnostics`) is guarded by the application's authorizer instead; see [Diagnostic retrieval](#diagnostic-retrieval).
- Responses are `no-store`.
- Readiness checks are cached (`health.cache_ttl_ms`) with single-flight refresh and per-check timeouts, so floods cannot probe dependencies.
- Every request is rate-limited before any handler or token check runs, so token guessing and scrape floods are bounded, including requests from loopback. Endpoint requests are limited per client and for the whole listener. Probes have their own budget and client table and are limited per client only, so traffic to `/health` or `/metrics` never throttles `/livez` and `/readyz`, and no set of sources can use up the probes for everyone. A client is the transport peer address (an IPv6 prefix, /64 by default), never a forwarded header. Exempt networks default to empty; add the kubelet node network only when bypassing its probe budget is intended (`management.rate_limit.exempt_networks`). Client tables are bounded by `management.rate_limit.max_clients`; only admitted requests take an entry, and refilled entries are forgotten with bounded work per request. Excess requests get `429 rate-limited` with `Retry-After`. See [configuration](configuration.md#managementrate_limit).
- Rate limits bound the rate, not the reach. Keep the listener on loopback or behind network policy anyway: many source addresses together can still use up the endpoint listener budget, and behind a proxy or sidecar all clients share the proxy's address. Istio's `127.0.0.6` sidecar address is rate-limited by default; exempting it bypasses limits for all proxied clients.

## OpenAPI documentation UI

The `openapi-ui` feature and `openapi.ui = true` serve Swagger UI at `openapi.ui_path` (`/docs`) with its assets beneath it. It is off by default, and it follows the document's access policy exactly: it is served where `/openapi.json` is, behind the management token on the management listener, and unauthenticated on the application listener only with `openapi.public = true`. Without a registered document, or with `openapi.serve = false`, there is no UI either.

**Do not expose the UI publicly in production.** It shows the whole API surface to anyone who can load it, and it adds a browser-facing page to the attack surface. `openapi.ui` with `openapi.public` makes startup and `ferrum-alloy check` warn. Keep it on the management listener, reached through a port-forward or tunnel; with a management token, the browser must send `Authorization: Bearer <token>` itself, for example through a local proxy.

- The UI is compiled into the binary from files vendored in `crates/ferrum-alloy/assets/swagger-ui/5.33.0/`: the `swagger-ui-dist` 5.33.0 npm tarball, checked against the registry's `dist.integrity` (SHA-512) before extraction, and pinned file by file in `SHA256SUMS`, which a test checks against the files and the served bytes. The bundle's third-party notices (`swagger-ui-bundle.js.LICENSE.txt`, which the bundle's first line names) are served, unmodified, beside it as `text/plain`, under the same access policy. Nothing is fetched at build time or at runtime, and the page loads nothing from another origin.
- Swagger UI is licensed under Apache-2.0, and its bundle includes MIT, BSD-3-Clause, and DOMPurify code (Apache-2.0 or MPL-2.0; Alloy uses it under Apache-2.0). These permissive licenses allow the files to be redistributed, unmodified and under their own terms, alongside Alloy's PolyForm Noncommercial and commercial licensing. Their `LICENSE`, `NOTICE`, and bundled notices ship in the same directory and in the packaged crate; an application that distributes a binary built with `openapi-ui` redistributes Swagger UI and should carry them too. `cargo deny` checks Cargo dependencies only, not these files: update them as the directory's README describes.
- Every UI response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`, with no `unsafe-inline` or `unsafe-eval`, plus `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`, and `Cross-Origin-Resource-Policy: same-origin`. The page has no inline script or style; Alloy's own initializer is a separate file.
- The initializer loads the document from `openapi.path` on the listener that served the page, refuses a document URL of another origin, and ignores configuration in the page URL (`queryConfigEnabled: false`). The online validator badge is off (`validatorUrl: null`).
- The UI is read-only: "Try it out" is disabled, so the page never sends requests to the API.
- `openapi.ui_path` is limited to unreserved URL characters and may not shadow another Alloy route; `openapi.path` is escaped where the page embeds it. With `openapi.public`, the UI and document paths take precedence over the application's own router, so startup fails closed if an application route matches one of them, rather than leaving that route silently unreachable. See [route conflicts](configuration.md#route-conflicts-on-the-application-listener).

## Errors

- Framework errors are RFC 9457 problems with stable `tag:` type URIs. `detail` is fixed text or parser output from the client's own input (control characters replaced with spaces, truncated to 256 bytes).
- Panics become `500 internal`; the panic message goes to server logs only.
- Database and JWKS errors are never returned to clients.
- Application response bodies are never rewritten. Only axum's empty 404 (router fallback) and 405 (`Allow` present) become problems.

## Configuration and secrets

`management.token` and `database.url` are `Secret`s and are never printed by `Debug`, `Display`, serialization, `ferrum-alloy check --show-effective`, or error messages. For example, an invalid database URL error does not echo the URL. `_FILE` variants read secrets from files. Unknown `FERRUM_ALLOY_*` variables and unknown keys are errors.

Parsing fails before values reach `Secret`, so syntax and schema (type) errors never quote configuration content:

- A TOML syntax error keeps the file, line, column, and the parser's own message. The source line excerpt that `toml` prints is dropped, because the malformed line may hold a token or database password.
- A schema error keeps the key path and the expected type or variants. The supplied value (`invalid type: string "..."`, `unknown variant ...`) is replaced with `(value redacted)`. Unknown keys are named only when they are short bare keys; any other key, such as a quoted URL or a bare key over 32 characters that mixes letters and digits, is shown as `(key redacted)`. serde repeats an unknown variant or key verbatim, including any newline or `, expected` text inside it, so Alloy keeps nothing of that part of the message. The list of valid variants or keys comes from asking the schema separately, and a key is named only when it is exactly what precedes that list. Messages that match no known shape become a generic "does not match the expected type or format" with the key path.
- `ferrum-alloy check` prints these messages unchanged in human and JSON output, and startup returns the same `ConfigError`.
- Semantic validation errors, reported after parsing, may quote non-secret values, such as an unaccepted algorithm name or a bind address. They never quote `Secret` values.

## TLS

rustls with the `ring` provider, passed explicitly. Alloy never installs a process-wide crypto provider. OpenSSL, native-tls, and aws-lc are banned in `deny.toml`. Client-certificate verification uses rustls' WebPKI verifier.

### Client certificate revocation

Revocation is checked only against certificate revocation lists (CRLs) named in `server.tls.client_crl_paths`. Without them, a leaked client certificate stays valid until it expires, so rotate short-lived certificates (SVIDs) or configure CRLs. The defaults fail closed:

- `client_crl_depth = "chain"`: the leaf and every intermediate the client presents are checked. `end_entity` checks only the leaf. Trust anchors are never checked.
- `client_crl_unknown_status = "deny"`: a certificate whose issuer has no configured CRL fails the handshake. `allow` accepts it and is reported as a warning.
- `client_crl_expiration = "enforce"`: an expired CRL fails startup and is refused by a reload, and a loaded CRL that expires fails every client-certificate handshake it covers until a fresh CRL is loaded by the next reload. `ignore` keeps using stale CRLs and is reported as a warning.

Every certificate in `client_ca_path` is a trust anchor, and trust anchors are never checked against a CRL. An intermediate placed in `client_ca_path` therefore becomes a trust anchor, and neither it nor its revocation by the root is ever checked. Put only root CAs in `client_ca_path` and have clients present their intermediates.

Provide exactly one CRL per issuing CA (a full CRL; combine partitioned CRLs into one), because the verifier consults only the first CRL whose issuer matches. A revocation listed only in a later CRL from the same issuer would be missed, so startup fails, naming both files, when two configured CRLs have the same issuer, whether or not they carry an issuing distribution point. When rotating a CRL, replace the file rather than adding the new CRL beside the old one.

A missing, unreadable, or unparsable CRL file fails startup, and fails a reload; it is never skipped. Errors name the file but never quote its contents. A new CRL published over a configured file is loaded by the next reload (see [Reloading certificates and trust material](#reloading-certificates-and-trust-material)), with no restart; with `reload_interval_ms = 0`, publishing a new CRL takes a restart. Whenever CRLs are loaded, Alloy logs the earliest `nextUpdate` time of the configured CRLs, as a warning when it is less than 24 hours away. With `client_crl_expiration = "enforce"`, an expired CRL fails every handshake it covers; under the Edge `gateway_required` mode that blocks all gateway traffic until a fresh CRL is loaded, which happens at the first reload after the fresh CRL is published, so publish CRLs well before their `nextUpdate` time and alert on the warning. OCSP, including OCSP stapling of client certificates, is out of scope: it is rarely deployed for client certificates.

### Reloading certificates and trust material

Every `server.tls.reload_interval_ms` (a minute by default), Alloy reads the certificate chain, private key, client CA bundle, and CRLs again, so short-lived SVIDs, CA bundle changes, and new CRLs take effect without a restart. Nothing changes unless a file's bytes changed. Changed files are read a second time after a settle delay (the interval, at most one second), and are used only when both reads match: a CA bundle or multi-CRL file truncated at a PEM boundary can still parse, with trust anchors or CRLs missing, and a certificate can be read before its new key. Changed material is validated as a whole, with the startup checks (the chain parses, the key matches the certificate, the CA bundle parses, one CRL per issuer, and CRL expiry when enforced), and only then replaces the server configuration that new handshakes use. The certificate, key, and client verifier are replaced together, never one without the others.

- Invalid material is never used. The previous material keeps serving, and the error is logged without quoting any file. Once the same files fail twice in a row, `ferrum_alloy_tls_reload_failures_total` counts the failure at every interval until the files are fixed; the error is logged at error level when it starts or changes. Files that change between the two reads at three reloads in a row are never used either; that is logged once as a warning and counted in `ferrum_alloy_tls_reload_stalls_total`. Alert on both counters: a certificate that cannot be replaced eventually expires.
- `ferrum_alloy_tls_server_cert_not_after_timestamp_seconds` and `ferrum_alloy_tls_client_crl_next_update_timestamp_seconds` give the `notAfter` time of the serving certificate and the earliest `nextUpdate` time of the serving CRLs. Alert when either is near: with `client_crl_expiration = "enforce"`, an expired CRL fails every handshake it covers. While a serving CRL expires within 24 hours, the warning log is repeated about once an hour.
- A reload cannot weaken client authentication. The policy (`client_auth` and the `client_crl_*` settings) comes from the configuration, which a reload never re-reads, so an empty or missing CA bundle or CRL fails the reload instead of turning verification off.
- Connections established before a reload keep their negotiated session and the identity verified then, until they close. A client whose certificate is revoked or whose CA is removed keeps any connection it already has: `server.idle_timeout_ms` closes it once it goes idle, but a connection kept busy stays open. Sessions cannot be resumed across a reload, so every new handshake is verified against the new material.

## JWT / JWKS (feature `jwt`)

- Only asymmetric algorithms from an explicit allowlist. `none` and HMAC are rejected at configuration and at verification.
- `iss`, `aud`, and `exp` are required; `nbf` is checked when present; leeway is configurable.
- Keys come only from the configured JWKS URL: `https`, or `http` to loopback. Token-supplied `jku`, `x5u`, and embedded `jwk` are never used.
- A token without `kid` is accepted only if the key set holds exactly one signing key.
- JWKS fetches never follow redirects, are bounded in time and size, and are single-flight. Refreshes, including those triggered by unknown `kid`s or by expiry, happen at most once per `jwks_min_refresh_interval_ms`. Callers that wait behind a refresh use its result instead of fetching again. A refresh runs in its own task, so a caller that stops waiting never cancels it.
- A fetched key set has a bounded lifetime: `Cache-Control: max-age` from the JWKS response, bounded by `jwks_min_refresh_interval_ms` and `jwks_max_age_ms` (default 5 minutes, at most 24 hours). After that, the set is revalidated even when the token's `kid` is cached. Within `jwks_max_stale_ms`, requests with a known `kid` verify against the stale set while it is revalidated in the background; after that, requests wait for the refresh. Keys missing from the refreshed set stop verifying, and a key replaced under the same `kid` is replaced in the cache.
- If a refresh fails, the expired set keeps verifying for at most `jwks_max_stale_ms` (default 5 minutes, at most 24 hours), with a retry at most once per `jwks_min_refresh_interval_ms`. After that, verification fails closed with `503 auth-unavailable` until a refresh succeeds. This trades a bounded window, during which a key retired while the JWKS was unreachable may still verify, for availability during short JWKS outages. Set `jwks_max_stale_ms = 0` to fail closed as soon as the set expires.
- A JWKS outage with no usable keys returns `503 auth-unavailable`.
- Forwarded identity headers never bypass token verification.
- **Advisory exception:** RUSTSEC-2023-0071 (`rsa`, via jsonwebtoken's `rust_crypto` backend) is a timing side channel in RSA private-key operations. Alloy only verifies signatures with public keys. The exception is time-boxed in `deny.toml` (expires 2026-12-26).

## Outbound HTTP (feature `http-client`)

- `traceparent` is sent only to listed hosts. Caller-set `traceparent`/`tracestate` to other hosts, and `baggage` to any host, are removed.
- Redirects are off by default. When enabled, only same-origin redirects are followed and cross-origin redirects are returned to the caller, so propagated context and credentials never follow to another origin.
- There are no automatic retries.

## Diagnostic retrieval

With the `diagnostics` feature, a service can serve one request's evidence from memory ([ADR 0008](adr/0008-tenant-scoped-diagnostic-retrieval.md)). It is off unless the application installs a `DiagnosticsAuthorizer`; without one, no evidence is retained and the route does not exist.

- **Authorization is the application's.** The authorizer receives the transport peer (`PeerInfo`) and the request headers, and returns one tenant the caller may read, or a denial. It must verify a tenant-scoped credential, such as a JWT whose claims name the tenant, or a verified transport identity. Alloy never derives a tenant from a header, and there is no switch that trusts a caller wholesale. The authorizer has 5 seconds; a timeout denies, and so does a panic in the authorizer, which is counted; Alloy's warning omits the credential, but the process panic hook prints the panic message, so never put a credential in one. Compare secrets in constant time.
- **The management token is neither required nor sufficient.** It names no tenant. A request carrying it is authorized like any other, by the authorizer.
- **Identifiers are not credentials.** Records are keyed by tenant and request id, and a lookup matches only the authorized tenant's records. Callers choose request ids, so two tenants may use the same one; each sees only its own. Trace ids are not accepted at all.
- **One answer for every refusal.** A denied caller, a failed or panicking authorizer, a malformed id, an unknown id, an evicted id, and another tenant's id all get byte-identical `404` Problem Details responses, so no response shows which case applied and ids cannot be enumerated. The authorizer runs before the id is examined. Timing can show whether the authorizer accepted a credential, as the authorizer's own latency can, but not whether an id exists. `/metrics` counts denials with the misses (`outcome="not_found"`).
- **Loopback only.** The management listener has no TLS, so startup fails if retrieval is installed while `management.bind` is not a loopback address; bearer credentials never cross a network in cleartext. To serve other hosts, terminate TLS in a proxy on the same host that forwards to the loopback listener. The authorizer then sees the proxy as the transport peer.
- **Rate-limited and non-cacheable.** Requests pass the management rate limiter before the authorizer runs, like every other management request, and `exempt_networks` never applies to the retrieval route. Startup fails if retrieval is installed while the management listener or its rate limit is disabled. Every retrieval response is `Cache-Control: no-store`.
- **Bounded.** Evidence is kept in memory only, by count and estimated bytes (`[diagnostics]`), with the oldest evicted first and evictions counted. The byte bound is an estimate that leaves out allocator overhead and spare collection capacity. Only requests the application tagged with a tenant (`TenantTag`) are kept; the first valid tag wins, so later code cannot move a request to another tenant. At most 16 records share one tenant and request id, and a further one evicts the oldest of them.
- **Redacted by construction.** A report holds the request id, trace and span ids, the route template, the status, the three server timings from the measurement catalog, the body outcome, the trace decision, and the peer trust label. It never holds header values, the raw path or query, bodies, SQL text or parameters, credentials, peer addresses, certificate identities, host or instance names, the service version, or upstream and dependency error text.
- **Never verified.** A report says `live_export` and `unverified`, and `ferrum-alloy diagnose --url` reads it with `parse_offline`, so a live report never yields `confirmed`.

`ferrum-alloy diagnose --url` fetches a report only when invoked with it. It sends one `GET` directly to the service (no proxy), follows no redirects, bounds the connection time, the whole request, and the response size, and writes nothing, to a gateway or anywhere else. It reads the credential from `FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or `--token-file`, never from an argument, refuses credentials embedded in the URL, sends a credential over plain `http` only to a loopback address, and replaces control characters and Unicode format characters (including bidirectional controls) in human-readable output, since the report comes from a remote service.

## Offline diagnostics

Reports and OTLP files are untrusted input. They are bounded (size, nesting depth, string length, counts, time range) and validated. Mutation tests confirm parsing never panics.

A `verified` claim in a file is downgraded to `unverified` and reported, so file input can never produce `confirmed` findings.

Rules are deterministic. They run no commands, make no network calls, and use no AI service. Remediation text is prose, never an executable command.

## Generated projects

`ferrum-alloy new`:

- validates names (no shell evaluation);
- refuses non-empty or symlinked targets;
- creates files with `create_new`;
- downloads nothing.

The generated CI pins `actions/checkout` by commit, and the `postgres` starter's service container by digest. Starters take secrets and endpoints from `alloy.toml` and documented `FERRUM_ALLOY_*` variables, except the `http-client` starter's upstream endpoint, which comes from `UPSTREAM_URL`. That value must be an `http` or `https` URL without credentials; an invalid value stops startup rather than falling back to the default, and errors never repeat it. The `jwt` starter trusts keys only from the configured JWKS URL, and the `http-client` starter propagates trace context to no host until one is allow-listed.

## Threat model

| Threat | Mitigation | Evidence |
|---|---|---|
| A direct caller forges `X-Consumer-Username` | Stripped unless the peer is a verified gateway identity and `accept_consumer_identity` is on | `standalone_mode_still_removes_forged_identity`, `a_different_identity_from_the_same_ca_is_not_the_gateway` |
| A caller forges `X-Forwarded-For` to gain trust | Trust never reads headers | `forwarded_headers_never_establish_trust` |
| A caller picks trace ids or forces sampling | Re-root untrusted context; parent-based sampling only for accepted parents | `untrusted_trace_context_is_rerooted_and_not_forwarded`, `untrusted_callers_cannot_force_sampling` |
| Gateway bypass | `gateway_required` with a verified SPIFFE identity; rogue CAs fail the handshake | `gateway_required_rejects_direct_callers_but_not_health_probes`, `certificates_from_another_ca_fail_the_handshake`, `edge-e2e` |
| A leaked client certificate keeps working | CRLs from `client_crl_paths` checked at the handshake and reloaded without a restart; unknown status and expired CRLs fail closed; bad CRL files fail startup and are refused by reloads | `a_serial_added_to_the_crl_is_refused_after_the_reload`, `an_expired_crl_is_replaced_at_the_next_reload`, `a_revoked_client_certificate_is_refused_and_others_are_accepted`, `der_crl_files_are_read`, `unknown_revocation_status_is_refused_unless_allowed`, `chain_depth_checks_intermediates_and_end_entity_depth_does_not`, `a_missing_crl_file_fails_startup`, `an_unparsable_crl_file_fails_startup_without_quoting_it`, `an_expired_crl_fails_startup_unless_expiration_is_ignored`, `optional_client_auth_refuses_revoked_certificates_and_accepts_anonymous_clients`, `two_crls_from_one_issuer_fail_startup`, `crls_from_different_issuers_are_accepted`, `a_crl_that_expires_after_startup_fails_later_handshakes` |
| Rotated certificates or trust material need a restart, or a bad or half-written replacement breaks TLS or client authentication | Periodic reload that uses changed files only once two reads a settle delay apart match, validates the material fully before swapping it in, keeps the previous material on failure, counts repeated failures, and never changes the client authentication policy | `a_rotated_certificate_serves_new_handshakes_and_established_connections_keep_theirs`, `a_rotated_client_ca_refuses_old_client_certificates_and_accepts_new_ones`, `invalid_replacements_keep_the_previous_material_serving_and_are_counted`, `a_reload_swaps_only_changed_material_that_validates`, `a_certificate_read_before_its_key_is_neither_swapped_in_nor_counted`, `files_that_change_between_the_two_reads_are_not_used`, `a_zero_interval_disables_reloading`, `unparsable_keys_and_chains_fail_startup_without_quoting_them` |
| A cache replays another request's ids or timing | Request-specific headers only on non-shared-cacheable responses, including heuristically cacheable ones without validators | `request_specific_headers_are_withheld_from_shared_cacheable_responses`, `heuristically_cacheable_responses_withhold_request_specific_headers`, `explicit_prohibitions_keep_request_specific_headers`, `authenticated_requests_keep_headers_unless_shared_storage_is_explicit` |
| High-cardinality labels exhaust memory | Route templates only; series cap and overflow bucket | `metric_series_are_capped`, `matched_unmatched_and_method_not_allowed_routes_use_bounded_labels` |
| Slow-header or connection floods | `header_read_timeout_ms` (request heads, and the first request on every connection whatever the protocol, on both listeners), `idle_timeout_ms` (connections with no request in flight and no response data written after the first request, including HTTP/2 connections kept open only by keep-alive pings), `write_stall_timeout_ms` (responses the peer takes no data of, by withholding HTTP/2 `WINDOW_UPDATE` or keeping a zero TCP receive window, whatever control frames it sends), `max_header_count`, `max_header_bytes`, `max_connections` | `slow_request_heads_are_cut_off`, `oversized_request_heads_are_rejected`, `connections_beyond_the_limit_are_closed`, `a_silent_connection_is_closed_and_releases_its_slot`, `a_partial_http2_preface_is_closed_and_releases_its_slot`, `an_http2_connection_without_a_request_is_closed`, `an_http2_connection_without_a_request_is_sent_goaway`, `the_management_listener_closes_silent_connections_and_frees_its_slots`, `an_idle_http2_connection_after_a_request_is_sent_goaway`, `an_idle_http2_client_is_disconnected_and_releases_its_slot`, `slow_responses_and_idle_http2_connections_are_not_cut`, `long_response_streams_outlive_the_idle_timeout_on_either_protocol`, `a_websocket_session_outlives_the_idle_timeout`, `an_http2_response_without_window_updates_is_sent_goaway_and_releases_its_slot`, `an_http1_client_that_stops_reading_is_disconnected_and_releases_its_slot`, `slow_readers_that_keep_reading_are_not_cut_on_either_protocol`, `slow_readers_of_a_finished_response_outlive_the_idle_timeout`, `a_finished_http2_response_without_window_updates_is_closed_by_the_idle_timeout`, `a_tls_client_that_stops_reading_is_disconnected_and_releases_its_slot`, `an_http2_response_over_tls_without_window_updates_is_sent_goaway` |
| Stalled TLS handshakes, connections, or handlers outlive shutdown | Handshakes are abandoned when draining starts; the listener waits for every connection task and HTTP/2 stream task, aborts connections left at the drain budget, counted in `ferrum_alloy_force_closed_connections_total`, and cancels stream tasks left then, counted in `ferrum_alloy_force_closed_streams_total` | `a_stalled_handshake_does_not_survive_a_short_drain`, `a_stalled_handshake_does_not_hold_up_a_long_drain`, `every_connection_is_closed_when_serve_on_returns`, `a_hanging_http2_handler_is_cancelled_at_the_drain_budget` |
| Oversized bodies | `Content-Length` precheck and streaming cap | `chunked_bodies_without_content_length_are_still_limited` |
| A collector outage slows or fails requests | Bounded queue; drop and count | `collector_failures_never_fail_requests_and_are_counted`, `a_full_queue_drops_spans_instead_of_blocking_requests` |
| Health floods probe the database | Cached, single-flight readiness | `readiness_checks_are_cached_and_single_flight` |
| Management exposed without auth | Validation refuses a non-loopback bind without a token | `unsafe_combinations_fail_validation` |
| The documentation UI exposes the API, loads third-party code at runtime, or runs injected script | Off by default; the document's access policy (token on management, public only with `openapi.public`, which warns); embedded, hash-pinned assets; strict CSP without inline script; escaped document path; no "Try it out" | `ui_requires_the_management_token_and_stays_off_the_public_listener`, `ui_is_public_only_on_explicit_opt_in`, `ui_is_off_unless_configured_and_needs_a_document`, `ui_responses_are_locked_down`, `openapi_ui_assets_match_the_manifest`, `the_document_path_is_escaped`, `openapi_ui_needs_its_feature_and_a_path_of_its_own`, `a_public_openapi_ui_is_flagged` |
| Management token guessing or scrape floods | Per-client and listener token buckets keyed by transport address, per-client probe budget, exempt networks, bounded client tables that only admitted requests enter, `429` with `Retry-After` | `a_burst_beyond_the_limit_gets_a_429_problem`, `probes_are_served_while_metrics_is_saturated`, `clients_are_limited_independently_by_transport_address`, `the_client_table_stays_bounded_under_many_distinct_peers`, `loopback_and_exempt_networks_are_not_limited`, `management_rate_limits_are_validated`, `rate_limit::tests` |
| JWT algorithm confusion, `alg=none`, key-refresh floods | Allowlist; JWKS-only keys; rate-limited refresh | `algorithm_confusion_and_unsigned_tokens_are_rejected`, `unknown_kids_refresh_at_most_once_per_interval`, `concurrent_requests_on_an_expired_set_refresh_once` |
| A retired or compromised signing key keeps verifying | Bounded key-set lifetime with revalidation of known `kid`s; bounded stale window, then fail closed | `removed_keys_stop_verifying_after_the_max_age`, `a_replaced_key_with_the_same_kid_is_picked_up_after_the_max_age`, `failed_refreshes_serve_stale_keys_only_within_the_grace_period` |
| Hostile diagnostic files | Bounds, schema checks, provenance downgrade, mutation testing | `crates/ferrum-alloy-diagnostics/tests/bounds_and_hostile_input.rs` |
| A caller reads another tenant's diagnostic evidence, or learns which ids exist | Application authorizer decides the one readable tenant; records keyed by tenant and request id; one byte-identical `404` for denials, authorizer panics and timeouts, other tenants, and unknown, malformed, or evicted ids; denials counted with misses; the management token is not sufficient | `other_tenants_denied_callers_and_unknown_ids_get_the_same_404`, `a_panicking_authorizer_denies_like_any_refusal`, `a_request_id_shared_by_two_tenants_discloses_only_the_callers_own`, `the_authorizer_sees_the_transport_peer_and_never_trusts_headers`, `an_authorizer_granting_an_invalid_tenant_denies`, `records_are_scoped_to_their_tenant_and_capped_per_request_id` |
| Retrieval credentials cross the network in cleartext | Startup refuses retrieval on a non-loopback management listener; remote access goes through a same-host TLS-terminating proxy | `startup_requires_a_loopback_management_listener` |
| A caller files records under another caller's request id to hide the real one | The oldest record of a request id is evicted, never the newest | `records_already_under_a_request_id_cannot_keep_a_later_one_out`, `slots_of_records_evicted_out_of_order_count_until_they_are_oldest` |
| Diagnostic retrieval floods, caching, or unbounded retention | Management rate limit before the authorizer, required at startup, with no exempt networks on the retrieval route; `no-store`; count and byte bounds with counted evictions | `retrieval_is_rate_limited_before_the_authorizer_runs`, `exempt_networks_never_bypass_the_retrieval_rate_limit`, `startup_requires_the_management_listener_and_its_rate_limit`, `a_tenant_retrieves_its_own_request_as_a_no_store_report`, `evicted_and_untagged_requests_are_not_found_and_counted`, `the_ring_is_bounded_by_count`, `the_ring_is_bounded_by_bytes` |
| Live reports leak request details | Evidence built from labels and timings only | `a_tenant_retrieves_its_own_request_as_a_no_store_report`, `every_finalized_request_is_handed_over_once_with_its_tenant`, `reports_round_trip_through_the_offline_parser` |
| The retrieval credential leaks through argv, the URL, or plain HTTP, or a report drives or reorders the terminal | Credential from the environment or a file only; URL credentials refused; plain `http` only to loopback; control and format characters, bidirectional controls included, replaced | `diagnose_url_never_takes_a_credential_from_arguments`, `diagnose_url_sends_credentials_over_plain_http_only_to_loopback`, `diagnose_url_reads_a_token_file_and_keeps_escapes_off_the_terminal`, `printable_text_replaces_control_and_format_characters`, `live::tests` |
| Secrets in logs or output | `Secret` redaction; sanitized errors; configuration errors without source excerpts or values | `secrets_are_never_printed`, `check_never_prints_secrets`, `invalid_urls_fail_without_revealing_the_secret`, `syntax_errors_report_the_location_without_the_source_line`, `schema_errors_name_keys_but_never_values`, `check_never_prints_secrets_from_malformed_files` |

## Known gaps

- Diagnostic retrieval is only as strong as the application's authorizer and tenant tagging, which Alloy cannot verify. Evidence is per process, so behind a load balancer retrieval must reach the replica that served the request, and it does not survive a restart. Live reports hold server-level timing only; operation, pool-wait, and admission evidence still requires telemetry export. Retention is shared by all tenants, so one tenant's traffic can evict another's evidence sooner; there is no per-tenant quota. Within a tenant, callers choose request ids, so a caller who knows or predicts another caller's id can add records to that caller's report or, by sending 16 more, evict the real one. The management listener has no TLS, so retrieval requires a loopback bind: reach it over loopback (for example a port-forward) or through a TLS-terminating proxy on the same host. The byte bound on retained evidence is an estimate.
- Upgraded (WebSocket) sessions are not counted against `max_connections` and are not drained. Applications should watch `Lifecycle::shutdown_token`.
- On HTTP/2, when a response stream hands over a chunk larger than the remaining flow-control window and then waits for the application, Hyper holds the unsent rest where Alloy cannot see it, so `write_stall_timeout_ms` does not treat the connection as stalled until the stream produces more data. Streams that send periodic events or keep-alive comments are covered, and a response whose body has ended is covered by `idle_timeout_ms`.
- Neither `idle_timeout_ms` nor `write_stall_timeout_ms` is a minimum transfer rate. A trickle reader that takes a little response data at a time, such as one byte of HTTP/2 window or one TCP segment every half period, keeps its connection open for as long as the response lasts, as with nginx `send_timeout`. `max_connections` bounds how many such connections one can hold.
- Client certificate revocation uses only CRLs read from `client_crl_paths`, at startup and at each reload. There is no OCSP and no CRL fetching from distribution points. A revocation reaches connections established before it only when they close.
- Network-boundary trust depends on deployment isolation that Alloy cannot verify.
- The [route-conflict check](configuration.md#route-conflicts-on-the-application-listener) does not report a root catch-all route (`/{*path}`) or fallbacks, including nested routers' fallbacks: Alloy's paths on the application listener take precedence over them without an error.
- The Edge v0.9.8 gaps listed in [edge-contract-inventory.md](edge-contract-inventory.md) §9.
