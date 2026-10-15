# Changelog

## [Unreleased]

- `ferrum-alloy edge export` now warns when `auth.mode` is `gateway`:
  no Edge authentication plugin is exported yet, so the generated
  configuration authenticates nobody until authentication is configured
  in Edge. Also in this batch: atomic CLI writes create the temporary
  file with its final mode (replaced private files are never briefly
  world-readable), `--alloy-git` is URL-parsed and credentialed URLs
  are refused instead of being baked into the generated `Cargo.toml`,
  template substitution no longer re-expands operator values, Windows
  device names are refused as project names, printed follow-up commands
  shell-quote their paths for the platform shell, manifest paths use an
  RFC 3986 allowlist, and YAML keys YAML would type as bool/null/number
  are quoted in any letter case (#197).
- **Breaking:** manifest literal paths must now be plain ASCII
  RFC 3986 `pchar` paths. Paths previously accepted with non-ASCII
  bytes or characters such as `|` are refused; Edge itself admits some
  of them, so this is Alloy's narrower canonical subset for generated
  configuration, not an Edge limit.
- JSON logging gained an opt-in bounded non-blocking writer:
  `logging.non_blocking_queue_lines` (`FERRUM_ALLOY_LOG_NON_BLOCKING_QUEUE_LINES`)
  sends lines through a dedicated writer thread so a slow stdout
  consumer cannot block request threads; lines past the queue are
  dropped and counted by `ferrum_alloy_telemetry::json::dropped_log_lines`.
  The default stays synchronous and lossless. Span-field rendering now
  reuses a per-thread scratch instead of allocating per `record` call
  (#197).
- OTLP export requests are now chunked by estimated bytes
  (`max_request_bytes`) as well as by span count, so a batch of large
  spans no longer sails past the exporter's request-body cap by span
  size alone and gets rejected whole. The estimate is the queue
  budget's (resource and instrumentation-scope encoding are not in
  it), so staying within the cap is best-effort, not guaranteed; a
  single span larger than the budget is sent alone.
  `OtelPipeline::queued_spans()` and `queued_bytes()` expose the
  export queue depth (#197).
- **Breaking:** Alloy's server now classifies each connection's peer
  once and passes the result to the telemetry layer as the new
  `ClassifiedPeer` extension, instead of classifying on every request;
  a custom `TrustClassifier` is therefore invoked once per connection
  with extensions holding only `PeerInfo` and must decide from the
  transport identity alone. Applications serving the router themselves
  keep per-request classification.
  Matched route templates are interned, so each distinct template is
  allocated once process-wide instead of per request (#197).
- Readiness refreshes now run in a detached task, so a probe client that
  disconnects cannot cancel the refresh and starve the cache (with a
  probe timeout shorter than a slow check, readiness could never become
  ready). A failed `AlloyApp::prepare()` keeps the explicit
  configuration, so a retry reports the same error instead of silently
  loading a file or the environment. A listener task that ends on its
  own now begins shutdown instead of leaving the process reporting
  ready, and a panicked TLS reload task is now logged (reloads stop;
  serving continues on the previous material) instead of stopping
  silently. `server.request_timeout_ms` documentation now
  states that admission wait is not counted (#197).
- **Breaking:** new `server.body_read_stall_timeout_ms` (default
  `60000`, `FERRUM_ALLOY_BODY_READ_STALL_TIMEOUT_MS`): a connection
  whose handler is waiting to read a request body and receives no body
  data for the timeout is closed and counted in
  `ferrum_alloy_body_read_stall_timeouts_total`, mirroring the write
  stall timeout. Previously only the headers deadline bounded a
  trickling upload, and with `server.request_timeout_ms = 0` nothing
  did, so uploads that pause longer than a minute mid-body now
  disconnect; raise the value for such workloads. A sender that keeps
  sending, however slowly, is never cut, and time a handler spends
  before it starts reading does not count (#197).
- Request-path overhead reduced with identical output: histograms now
  record into a single bucket (integer bucket assignment) and sum to
  cumulative counts only when rendered, the metrics series and route
  label are resolved once per request instead of twice, label escaping
  borrows unless escaping is needed, request/trace/span ids are
  hex-encoded from a table without formatting calls, and shared-cache
  classification compares `Cache-Control` directives in place instead
  of allocating lowercased copies (#197).
- **Breaking:** `trust.networks` rejects IPv4-mapped IPv6 CIDRs, which
  could never match a canonicalized peer address; use the IPv4 network
  (the same rule rate-limit exempt networks already applied). A
  non-loopback `management.bind` now also warns in `check` output that
  the management listener has no TLS and its token crosses the network
  in cleartext (#197).
- Updated `rustls` in the lockfile to 0.23.46, clearing
  RUSTSEC-2026-0346 through RUSTSEC-2026-0349 (ECH fallback data
  exposure and certificate-decompression allocation bounds);
  `cargo deny check` passes again.
- `ferrum-alloy diagnose` redacts the service diagnostics credential
  (`FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or `--token-file`) from every output,
  as it already did the Edge admin credential. Previously only the Edge
  credential was redacted, so a service reflecting the credential it was
  sent into its report leaked it to the terminal and to `--write-report`
  files (#197).
- **Breaking:** `auth.jwt.jwks_url` accepts plain `http` only to the literal
  loopback addresses `127.0.0.1` and `[::1]`; `http://localhost` is refused by
  `check` and `JwtVerifier::new`, because the resolver decides where a DNS name
  connects (#181). Use `http://127.0.0.1:<port>`. An `http` JWKS is fetched
  directly, never through an environment proxy (`HTTP_PROXY`). `ferrum-alloy diagnose --url`
  likewise sends its credential over plain `http` only to a literal loopback
  address, no longer to `localhost`.
- **Breaking:** `postgres::connect` rejects `database.url` query
  parameters SQLx does not recognize, naming the parameter and never its
  value. SQLx previously logged each one with its value (libpq's
  `sslpassword` is a secret) and silently ignored it, so a typo such as
  `sslmod=verify-full` quietly dropped the requested verification (#197).
- **Breaking:** `postgres::connect` raises `sslmode=prefer` (also the default
  when neither the URL nor `PGSSLMODE` sets one) to `require` for a host that is
  not local (a Unix socket or a literal loopback address; `localhost` and
  other DNS names are not), so a remote
  database never silently falls back to plaintext (#182). An explicit
  `sslmode=disable` or `allow` to a remote host is kept and logs a warning.
- **Breaking:** operation spans are now created at the wrapped
  future's first poll instead of when `Operation::run` is called, so a
  stored-then-awaited operation's span interval starts when its work
  starts and a future dropped unpolled exports no span. The span's
  parent is the span current at first poll, not at construction: a
  future built under one span and first polled under another (or in an
  uninstrumented spawned task, where it becomes a root) changes
  parentage; instrument deferred or spawned futures with their
  intended parent. Rejected trace context now also strips the
  `baggage` header before application code runs. `alloy.db.pool_wait`
  documentation now states that repeated acquires record repeated
  values and the last value present is used, never a sum, and the
  detailed-health `latency_ms`, `age_ms`, and `uptime_seconds` fields
  are defined in `docs/measurement-semantics.md` (#197).
- `ferrum-alloy openapi export` without `--input` reads at most 16 MiB of the
  project binary's standard output, the `--input` limit. When a binary prints
  more, the export closes the pipe, kills the process it spawned (on Windows
  Cargo, not necessarily its child), and fails with exit 3 instead of
  buffering without bound (#183).
- **Breaking:** JWT verification accepts only plain JWTs. Tokens whose
  header carries `crit`, `enc`, `zip`, or `cty`, an explicit `typ` other
  than `JWT` or `at+jwt`, or an `iss` array instead of the single issuer
  string are now rejected with `401` (RFC 7515 §4.2, RFC 7519 §4.1.1,
  RFC 8725 §3.11). An `Authorize` denial detail is sanitized like other
  framework problem details before it reaches the client (#197).
- `config::Secret` holds its value in `Zeroizing<String>`: the value is
  zeroized when a `Secret` or any clone of it is dropped, and `Secret`
  implements `ZeroizeOnDrop`. `FERRUM_ALLOY_*_FILE` secret files and the TLS
  private key are read with the new `files::read_sensitive_file_bounded`,
  whose buffer is zeroizing and fully reserved before the read, so no
  reallocation or refused read leaves a plaintext copy. `zeroize` is now a non-optional
  dependency of `ferrum-alloy` (#184).
- **Breaking:** configuration validation now rejects tuning values that
  could only disable or negate a protection: `server.tls.handshake_timeout_ms`
  of `0` (which failed every TLS handshake at runtime), connection timeouts
  above 24 hours, `server.max_header_bytes` above 16 MiB, oversized body,
  header-count, and HTTP/2 stream limits, zero `http_client` timeouts and
  `database.acquire_timeout_ms`, invalid
  `http_client.propagate_trace_context_to` host rules, and out-of-range
  `auth.jwt.jwks_timeout_ms`, `jwks_max_bytes`, and `leeway_seconds`
  (mirrored in `JwtVerifier::new` and `AlloyClient::new`). Credentialed
  `auth.jwt.jwks_url` values are redacted in `redacted_toml()` and never
  printed by `JwtSettings`' `Debug` (#197).

- Complete the 2026-10 audit fixes after independent review of #177 (#166,
  #167, #171, #172). The parser-free JWKS URL check (CLI `check`) also refuses
  Punycode (`xn--`) and empty host labels, so it never accepts a URL the
  verifier rejects. The OTLP export worker owns terminal span accounting: when
  it ends, including by unwinding from an exporter panic after a timed-out
  shutdown, every span still queued is counted as a `shutdown` loss exactly
  once. Imported instance identities that start with `digest:` are digested
  too, so a literal identity never equals another identity's digest.
  Diagnostic reports preserve and warn about unknown members of observation
  `boundaries`, `interval`, `scope` and `span`. The catalog defines
  `alloy.server.preparation` and describes the header-phase anchor the importer
  uses.

- Adopt `contracts-edge-0.9.17` at `f7ba1a9b9885900d1e110f76d38e37a02eda13c8` and pin Ferrum Edge v0.9.17 at `669c574c1d1e88e84dccb26a8159939e6694644d`, with v0.9.15 as the previous supported release. Recognize the `loop_detected` `X-Gateway-Error` token and diagnostic-ref rejection phase; its diagnosis describes the received `X-Ferrum-Hops` count reaching the limit and does not claim a traversal or routing loop.
- Fix the 2026-10 audit batch (#164–#175, #178). `server.max_connections` and
  `server.max_in_flight_requests` above Tokio's `Semaphore::MAX_PERMITS` fail
  validation instead of panicking. The `auth.jwt.jwks_url` HTTPS/exact-loopback
  policy is applied to the parsed URL the verifier fetches, by both `check` and
  `JwtVerifier::new`, so lookalike loopback hosts, malformed URLs, and
  backslash-ambiguous forms such as `http://idp.example\@localhost/` are
  refused consistently. Panicking readiness callbacks fail their check with a
  sanitized message, and the management router has the same panic boundary as
  the application router. OTLP queue admission charges span status
  descriptions; a timed-out shutdown no longer counts spans the worker still
  owns as shutdown losses. `timeout_ms` is documented as a per-request timeout.
  Alloy SERVER spans export `alloy.server.preparation_ms`, which the OTLP
  importer uses to anchor the header phase at middleware entry. Diagnostic
  reports preserve and warn about unknown fields in `collection`, producers
  and `subject`; overlong OTLP instance identities become a bounded digest
  instead of colliding truncations; Alloy SERVER spans without response
  headers keep their total duration and outcome on import.
  `fill_default_annotations` fills locally referenced Path Items. Benchmark,
  qualification and compatibility docs describe the current state.

- Adopt `contracts-edge-0.9.15` at `6fb64c5dc2e014204c17609fc717d976f3b4589e` and pin Ferrum Edge v0.9.15 at `25b37395ff61bfea0f3ffd189d9011c4984fa755`. Track its `route_protocol_admission` rejection phase and reserve `X-Authenticated-Identity` from request-ID configuration.
- Adopt `contracts-edge-0.9.14` at `ddbdd845733b7046c4393ac951011dafb774db33` and pin Ferrum Edge v0.9.14 at `9bd4d5f9caa4ebe8f0ea13e76d8a6e2172eaca7d`.
- Name Ferrum Edge LLC as the copyright holder and commercial licensor in `LICENSE` (Required Notice) and `LICENSE-COMMERCIAL.md`.
- Adopt canonical `contracts-edge-0.9.13` (gateway vocabularies re-pinned to Edge v0.9.13 provenance; content otherwise unchanged).
- Fix diagnostics retention review findings (#153, item 2). Admission could
  freeze once every retained tenant held one record: records now expire after
  `diagnostics.max_age_ms` (default 15 minutes, `0` disables, otherwise 1 s to
  1 day; `FERRUM_ALLOY_DIAGNOSTICS_MAX_AGE_MS`), measured on the monotonic
  clock and removed oldest first before each admission, lookup and metrics
  scrape as `evicted_total{reason="age"}`, and an expired id gets the same
  `404` as an evicted one. A tenant holding nothing takes the oldest record of
  the tenant holding the most records while one holds two or more, by count
  even under byte pressure, and otherwise the record of the least recently
  active tenant once it is 1 second old (`diagnostics::TAKEOVER_AGE`), so
  admission never freezes, by count or by bytes, even with expiry disabled.
  One admission takes over at most two or three sole records. Reports note
  when a lookup resolved through a correlation alias, including an evicted
  local id that now names another request, and how many earlier attempts are
  no longer retained while a later one is. The `diagnostics.max_age_ms`
  validation message uses the same `within a..=b` wording as its neighbours.
  Every index shares one tenant allocation, matching the byte accounting. A
  poisoned store starts again empty (`ferrum_alloy_diagnostics_resets_total`),
  and admission gives up instead of looping if its indexes ever disagree. ADR
  0008 no longer implies a time window that did not exist.

- Document scraping the management `/metrics` endpoint with Prometheus and a
  bearer `credentials_file`, and fix Edge-version wording left stale after the
  v0.9.12 pin: the canonical vocabulary source is `contracts-edge-0.9.12`, and
  ADR 0007 marks its v0.9.9/v0.9.10 support statements as historical (#153).

- Complete the Edge v0.9.12 pin with published `contracts-edge-0.9.12` at
  `31f0a21d707795be293d15837c2f77c3d84219d8`, preserving the same 16-file scope
  and verifying its bytes against the canonical Git tree. Refresh gateway
  provenance, admin-only ETag/If-Match descriptions and diagnostic-ref provenance;
  keep gateway meanings, report schema, all diagnostic fixture payloads and the
  original v0.9.11 shared-freeze metadata unchanged. Preserve strict pairing,
  schema and fixture assertions. Re-audit published Edge `0d917701` and refresh
  the v0.9.12/v0.9.11 documentation without adopting deployment APIs or changing
  Alloy trust, telemetry, lifecycle or publishing status. The preceding bot
  head's four failed pairing jobs require fresh whole-head hosted qualification.

- Simplify the test-only benchmark health harness (#142): remove the fatal
  post-deadline H2 reuse probe, the observers, evidence capture and
  source-text tests built around it, and the health-diagnostic workflow; keep
  the bounded cancellation budget, allocator test isolation, error source
  chains and coordinator checks. The warmed cancellation regression now uses
  the matrices' 5-second window and requires all 32 measured cancellations;
  its 200 ms window could close before a slow runner completed any. Hosted
  benchmark qualification now runs weekly and on dispatch, not on pull
  requests.
- Adopt published `contracts-edge-0.9.11` at
  `390edbd5b2485af0988e02f7827fde778d76ae0a`: vendor all 16 adopted files
  byte-exact and pin every SHA-256. Record root's accepted unchanged report and
  manifest shared v1 freeze at qualified owner `81cbb410`; keep owner availability
  unreleased, historical PROPOSED descriptions and full schema parity.
  Preserve immutable consumer qualification slices and their authority limits;
  new coordinated adoption gates and #27/#28 disposition remain with root.
- Pin verified Edge v0.9.11 (`c764084b3b51c3f7ffde268c039688d35e49c553`),
  default multi-arch index
  `sha256:2476b502855940e28157858fc24008545cb3baeb3084c9610e1d4505cbe0d36e`.
  Roll the existing latest-plus-previous support window to v0.9.11/v0.9.10,
  update paired CLI assertions and only the known generated YAML headers, and
  statically re-audit existing config/MCP contracts. Fresh hosted adoption CI
  remains required; no Alloy crate publication or performance acceptance is granted.
- Correct website and shared-contract adoption references using immutable current
  sources; retain historical website and released r2 claims and pending freeze.
- Refresh the authoritative implementation ledger for #28 with current issue
  states, immutable consumer heads and hosted evidence. Record merged Anvil,
  Foundry, Nexus #519 and GitForgeOps qualification as of 2026-10-04. Record
  Nexus's final `77fdb767` head, all 11 passing checks, merge and completed merge
  push CI; earlier failed/cancelled heads remain historical failed evidence.
  Propose the owner diagnostic-report/service-manifest v1 qualification and
  coordinated freeze for root review as the next canonical release step, pending
  a matching qualified owner commit and canonical documentation/metadata with
  hosted CI. Retain released r2 annotations, pins and schema parity. Document #140's
  shared hosted benchmark preparation separately from #15/#16's external
  dedicated acceptance and #24's owner publishing decisions. Correct stale
  rule coverage, manifest agents, test-count and changelog claims; retain the
  v0.9.10/v0.9.9 Edge window, PROPOSED schemas and unpublished status.
- Anchor all CLI output writes to retained capability directory handles.
  Project and GitForgeOps trees reject internal directory links and retain
  every acquired directory across files; Edge, OpenAPI and diagnostic file
  output retain their parent through exclusive creation or temporary writing
  and replacement. Directory substitution cannot redirect writes through a
  newly planted link. Operator-selected ancestors above the named root remain
  trusted. Add command-level substitution barriers and public CLI controls;
  qualify cap-std/cap-fs-ext 4.0.3 and their published dependency checksums.
  Leave uncertain temporary entries untouched after a failed atomic write;
  preserve I/O exit codes and write context for output-parent failures.
  Addresses the remaining directory race in GHSA-68jq-pr65-chjv / #134;
  unpublished Alloy behavior, with Edge pairing and G01 contracts unchanged.
- Require a configured management bearer token for detailed health, metrics,
  and management OpenAPI/UI even on loopback. Tokenless defaults expose only
  minimal liveness/readiness probes and separately authorized diagnostics.
- Reserve diagnostic count/byte evictions before commit; reject as `fair_share`
  without losing any previous records when the candidate cannot fit admissible
  capacity. Reclaim another tenant's oldest record only if its remaining holding
  still covers the candidate tenant's projected holding; remove the global
  fallback and preserve sole records. Add variable-size, donor, replacement,
  exact-limit, mixed-pressure and concurrent/index regressions (#136).
- Compact depleted diagnostic alias owner tables geometrically, tracking
  allocation capacity across deletion tombstones. Aggregate owner-table
  capacity stays within four slots per live owner, hence per retained record,
  with constant table rounding/control overhead. Charge that owner allowance
  in the byte estimate; keep configured limits and admission policy unchanged.
  Add grow/drain/pair-refresh capacity regressions against the former index
  and variable-length owner/alias stress with bounded compaction scans
  (PR #138 / #136 / #137). The byte estimate remains distinct from process
  memory; individual rebuilds scan one alias allocation under the store lock.
- Give each frontend request immutable locally generated diagnostic ownership.
  Remote request ids and accepted traces remain correlation hints, preserving
  HTTP response ids and trace propagation; matching remote pairs and preclaims
  cannot join another local request or exhaust its 16-attempt cap. Unique local
  lookups stay tenant-scoped; reused external aliases return uniform `404` and
  expire with retained records. Gateway retries have separate reports linked
  by their real traces. Add real trusted-transport and within-request backend
  retry regressions to the existing hosted OS matrix (#137). Shared report
  schema/provenance, Edge pins and G01 are unchanged. These corrections address
  accepted residuals of GHSA-5hmr-6xjc-cpm9 and GHSA-7976-x2f2-r8fc; advisory
  disposition remains pending independent review and hosted CI. Alloy remains
  unreleased, `publish = false`, MSRV 1.94; no patched semver is asserted.
- Shut down all remaining accepted TCP connections at the drain budget through
  owned `socket2` duplicate handles, including TLS, unawaited `OnUpgrade`, and
  unpolled upgrades. Retain permits/counts until application drop, with a bounded
  final wait. Each accepted connection retains one additional socket handle;
  no per-packet overhead is added. Add real-socket regressions to the existing
  Linux/macOS/Windows hosted test matrix. These are unreleased source changes;
  Alloy remains `0.1.0`, `publish = false`, with nothing published.

- Enforce `diagnose --url`'s timeout across the complete service request,
  including response headers and the full body, so a peer cannot extend the
  deadline by periodically sending bytes.
- Add authenticated Edge diagnostic-reference lookup to `diagnose` through
  `--edge-admin-url`, an explicit trusted `--edge-observation` client capture,
  and the environment-only `FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN` credential
  (`diagnostics:read` plus namespace). Validate and bind the released G01 record
  before allowing its new record finding above `likely`; service reports,
  serialized authentication flags, and offline rereads remain unverified.
  Requests use verified TLS or direct literal-loopback HTTP, no redirects or
  environment proxies, bounded time/body, and redacted credentials. Vendor the
  ten released diagnostic-ref fixtures and pin `contracts-edge-0.9.9-r2`.
- Fix HTTP/1.1 responses being cut by `server.idle_timeout_ms` while the
  client was still downloading them. Hyper lets go of a response body as soon
  as it has taken the last chunk, so the rest of a large response could wait
  in Hyper's write buffer with no request counted in flight, and a pause of
  one idle timeout in the client's reading, or in the network, closed the
  connection and truncated the response. An HTTP/1.1 connection whose transport
  cannot take a write is no longer idle; HTTP/2 control writes, including blocked
  PING and SETTINGS acknowledgements, do not defer idle closure;
  `server.write_stall_timeout_ms` still disconnects a client that stops
  reading, now also once its response body has ended. Once the transport has
  accepted the complete response, an idle close can still occur while the
  client reads buffered bytes; that close does not truncate the response.
- Protect CLI output files from symlink redirection with exclusive creation or
  same-directory atomic replacement, and reject pre-existing symlink
  components at or beneath generated output roots, including roots written with
  trailing slashes or `/.`. The directory-handle follow-up above closes the
  check-to-open substitution gap. Ancestors above the named root remain trusted
  path context (see [security notes](docs/security.md#known-gaps)).
- Keep an upgraded (WebSocket) connection's slot with its socket: it now
  counts against `server.max_connections` and in
  `ferrum_alloy_active_connections` until the application drops it, so
  upgraded sessions can no longer outnumber the connection limit. Shutdown
  drains upgraded connections too: still open at `shutdown.drain_timeout_ms`,
  every read and write on them fails and up to 16 tasks waiting on them are
  woken, they are counted in `ferrum_alloy_force_closed_connections_total`,
  and serving waits up to one more second for the application to drop them.
  An upgraded connection held without reading or writing is now shut down
  through its independently owned TCP handle; accounting and descriptors
  remain until the application drops it. Services with many
  long-lived sessions may need a larger `max_connections`.
  Addresses GHSA-p9fc-ggvj-g423.
- Close HTTP/2 connections whose peer withholds `WINDOW_UPDATE` while Hyper
  holds part of a response chunk beyond the flow-control window and the body
  waits for the application: each response chunk handed to Hyper now counts
  as waiting until Hyper takes it for writing or drops it with a reset
  stream, so `server.write_stall_timeout_ms` applies, and a cancelled stream
  leaves nothing counted that could cut a quiet stream on the same
  connection. Addresses GHSA-8cm5-mjvm-778g.
- **Changed default:** `telemetry.request_id.accept_incoming`
  (`RequestIdConfig::accept_incoming`) now defaults to `trusted_peers`, like
  `trace_context.accept_incoming`, instead of `any`. A request id sent by a
  peer the trust classifier does not trust is replaced by a generated one,
  which handlers see and responses echo, so a caller can no longer choose the
  id its request is logged, traced, and retained for diagnostics under. Set
  `any` to keep every caller's id; configuration validation now warns about
  it. **Upgrading behind Ferrum Edge:** Edge's correlation ids are kept only
  when Edge is a trusted peer. List it in `[trust]` (`trust.identities`,
  preferred, or `trust.networks`); retries group by id only when Edge is a
  trusted peer. Setting `accept_incoming = "any"` keeps Edge's ids but files
  them as `untrusted_caller`, bound to their own traces, so Edge's untraced
  retries are split and later attempts are refused as
  `request_id_conflict`, and those records rank below any `trusted_peer` or
  `generated` records under the same id. Otherwise Alloy logs, traces, and
  retains Edge's requests under generated ids that differ from the id Edge
  echoes to its client, and retrieval by that id finds nothing.
- Diagnostic evidence retention is shared fairly between tenants: under the
  count or byte bound, a tenant evicts another tenant's oldest record only
  while that tenant holds more than it, and otherwise its own. One tenant's
  traffic can still evict another tenant's records down to an equal share,
  as a newly active tenant does, but no further. The store no longer keeps
  the slots of records evicted out of order. Its fixed per-record byte
  estimate rises from about 330 to about 770 bytes on 64-bit targets, so a
  `diagnostics.max_bytes` sized for a number of small records now holds
  about half as many (roughly 2.0–2.3 times fewer); a record with the longest
  tenant, id, and route is charged about 2 KiB.
- Retained diagnostic evidence separates generated, transport-supplied,
  and direct caller request ids and prefers generated ids in lookup. Records
  under a tenant, origin, and id must share trace identity and provenance;
  conflicts are skipped, counted, and noted in reports. Matching attempts
  remain capped at 16, with oldest-attempt eviction and tenant fair sharing.
  Reports carry `request_id_origin` on each `alloy.response` event.
- Harden the diagnostics reader against hostile reports and OTLP files.
  `parse_offline` now discards supplied findings after checking their count
  and shape, so `ParsedReport.report.findings` is always empty and findings
  come only from `analyze` (behavior change for library callers; the warning
  now says the findings were discarded). The pre-validation string-length
  walk formats a location only for an error, so long keys above a wide array
  no longer multiply its work. Parser error locations escape control
  characters and Unicode line separators in keys. OTLP `max_spans` now
  counts every span entry read, valid or not. `render_text` writes control characters other than tab and
  Unicode line separators inside report, finding, and warning values as
  escape sequences (`\n`, `\u{1b}`, `\u{2028}`, ...), so a value cannot
  forge lines or terminal controls in human-readable output, including for
  library callers. Addresses
  GHSA-wf6p-cw5w-h579, GHSA-65px-xjrq-gq7m, GHSA-7cjx-8mc3-p2gx, and
  GHSA-h3m2-gxq9-62h7.
- Reject service manifest proxy IDs when a generated upstream or plugin ID
  would exceed Ferrum Edge's 254-character resource ID limit.
- Correct `alloy.response.body.bytes`: it counts the data-frame payload bytes
  handed to Hyper after the response's inner layers ran, so with compression
  enabled it reports the compressed frame payload. It excludes HTTP framing and
  TLS overhead and is not proof the client received it.
- Correct the documented `x-ferrum-mcp` validation scope: a disabled
  document-level extension still checks top-level closed keys, the `enabled`
  boolean type, and per-operation metadata, while `endpoint`, `namespace`,
  `include`, `exclude`, `limits`, and `forward_request_headers` are validated
  only when it is enabled. Add adjacent disabled/enabled tests.
- Pin Ferrum Edge v0.9.10 and keep v0.9.9 as the previous supported release.
  Re-audit Edge #5954: MCP request charset checks and fail-closed handling for
  uninspectable or over-nested JSON-RPC batches do not change contracts Alloy
  consumes. The `X-Gateway-Error` vocabulary remains the same eight tokens,
  and the ferrum-contracts pin remains `contracts-edge-0.9.9`.
- Document the central ferrum-contracts store, vendor and pin the diagnostic-ref v1 schema, and correct the shared `[agents]` schema status.
- Interpret Ferrum Edge v0.9.9 backend attempt spans for per-attempt timing,
  connection setup, and connection reuse; preserve the v0.9.8 multiple-attempt
  behavior when attempt spans are absent. Rule `alloy.r003` (now version 3)
  compares each attempt with the service request under it, reports a service
  that outlasted its attempt as conflicting evidence, and treats an attempt
  span without both timestamps as unavailable. A buffered attempt is compared
  only when it carries `gateway.backend.connection.reused`, because Edge's
  HTTP/3 bridge to an HTTP/1.1 or HTTP/2 backend can end a buffered attempt at
  its response head.
- Rules `alloy.r001` (now version 3) and `alloy.r004` (now version 4): bumped
  for the attempt-span linkage added in #111, which changed their wording for
  every input and their output on Edge v0.9.9 input.
- Retry backoff is not attributed: Edge v0.9.9 states only that backoff falls
  between attempt spans and exports no backoff duration, so Alloy derives
  none from the gap between them. This is deferred until Edge measures it.
