# Changelog

## [Unreleased]

- Refresh the authoritative implementation ledger for #28 with current issue
  states, immutable consumer heads and hosted evidence. Record merged Anvil,
  Foundry and GitForgeOps qualification; keep Nexus #519 and the canonical
  diagnostic-report/service-manifest record and freeze pending. Document #140's
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
