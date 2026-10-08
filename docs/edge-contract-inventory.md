# Ferrum Edge contract inventory

This document lists every Ferrum Edge header, attribute, endpoint, error token, and timing field Ferrum Alloy relies on, emits, or deliberately does not rely on. Each entry gives its source, producer, consumer, trust rules, lifecycle boundary, protocol coverage, tests, and status. Every item was checked against Edge source; nothing is listed as a contract on the strength of an example or design note alone.

**Status values**

| Status | Meaning |
|---|---|
| **EXISTING** | Present in the Edge release below and verified in its source. For Alloy-owned items, implemented in this repository. |
| **PROPOSED** | Not implemented by its would-be producer or consumer. Historical PROPOSED descriptions/fixture comments are retained for exact pairing; current shared v1 status is EXISTING/implemented in canonical `x-contract`. |
| **UNAVAILABLE** | Not produced by supported Edge v0.9.15 (the contract baseline) or v0.9.14. Alloy must not assume it and reports it as missing evidence. Historical release-specific absences are marked separately. |

## Revisions inspected

| Repository | Revision | Role |
|---|---|---|
| ferrum-edge/ferrum-edge | `v0.9.15` = `25b37395ff61bfea0f3ffd189d9011c4984fa755` (2026-10-08) | **Contract baseline.** Published default multi-arch index (`ferrumedge/ferrum-edge@sha256:29b468df…436eaca3`); static re-audit below, fresh Alloy adoption CI pending. |
| ferrum-edge/ferrum-edge | `v0.9.14` = `9bd4d5f9caa4ebe8f0ea13e76d8a6e2172eaca7d` (2026-10-07) | Previous release under the latest-plus-previous policy (`ferrumedge/ferrum-edge@sha256:15442f1b…0da5e3f8`); static re-audit below, fresh Alloy adoption CI pending. |
| ferrum-edge/ferrum-edge | `v0.9.13` = `9b83115de7ec23ab51ec4feae6bed65e596db425` (2026-10-06) | Older, unsupported release (`ferrumedge/ferrum-edge@sha256:6caa0987…05862e50`); its historical re-audit is retained below. |
| ferrum-edge/ferrum-edge | `v0.9.11` = `c764084b3b51c3f7ffde268c039688d35e49c553` (2026-10-04) | Older, unsupported release under the previous support window (`ferrumedge/ferrum-edge@sha256:2476b502…0d36e`); its historical re-audit is retained below. |
| ferrum-edge/ferrum-edge | `v0.9.10` = `ee040d5e3281fde424aa65f5b18004852c5b53b0` (2026-10-01) | Older, unsupported release (`ferrumedge/ferrum-edge@sha256:430d6a7d…c7dd4cc`). Historical unmarked Edge file:line references below remain from v0.9.10 unless another revision is named. |
| ferrum-edge/ferrum-edge | `v0.9.9` = `234717ce41965cd1e2b5c6c761a25475c5d7628c` (2026-10-01) | Older, unsupported release retained for historical source/qualification references. Line references marked "v0.9.9" are from this revision. |
| ferrum-edge/ferrum-edge | `v0.9.8` = `e27f2109216352c3fe9e67a7014611f3f66daa91` (2026-09-27) | Older, unsupported release retained as historical comparison for telemetry and security behavior. |
| ferrum-edge/ferrum-edge | `05997cee91bb4e1fa3dd1e64506b1512c7b16b8a` (main, 2026-09-24) | Unmarked historical line references below were spot-checked at v0.9.9 where their files were unchanged. |
| ferrum-edge/ferrum-contracts | `contracts-edge-0.9.15` = `6fb64c5dc2e014204c17609fc717d976f3b4589e` | Published canonical tag adopted byte-exact for the same 16 selected vocabularies, schemas and fixtures (`contracts/ferrum-contracts/PIN`). |
| ferrum-edge/ferrum-contracts | `contracts-edge-0.9.11` = `390edbd5b2485af0988e02f7827fde778d76ae0a` | Historical pin and original accepted shared v1 freeze; its unchanged report metadata remains in the current canonical bytes. |
| ferrum-edge/ferrum-contracts | `contracts-edge-0.9.9-r2` = `591c73a3f965fdab440c3a76b2707accdf491ba5` | Historical qualified consumer pin; original released bytes and annotations remain immutable. |
| ferrum-edge/ferrum-anvil | `075890f9418718113d5d83da4074c6b00b92bde9` (origin/main) | `DiagnosticFinding` schema, Edge v0.9.7 outcome catalog, G01 proposal. |
| ferrum-edge/ferrum-foundry | `e2f60b1eb0e881b3302ee7416df2d085f757dec4` | Pinned-Edge approach (`docs/compatibility.json`). No diagnostic UI. |
| ferrum-edge/ferrum-nexus | `b803a95cf4afd012d3cbb93324f5beac937b20c0` | OpenAPI 3.x publication through Edge `/api-specs`. |
| ferrum-edge/ferrum-edge-git-forge-ops | `fa56bd790e848544fdc9fea1a1ac3024d514a3b6` | `kind`/`spec` resource format. |
| ferrum-edge/ferrumedge | `0b796393b63e12a9fd643430446620dc23f140a8` | Historical website snapshot; its inspected source did not mention Alloy. |
| ferrum-edge/ferrumedge | `51f3709f1d3f754519dcc0ce5e018716f44dc272` | Current Alloy page: [immutable source](https://github.com/ferrum-edge/ferrumedge/blob/51f3709f1d3f754519dcc0ce5e018716f44dc272/alloy.html); it labels Alloy pre-release and unpublished, with source tested against Edge v0.9.10/v0.9.9. |

The historical consumer revisions above support the inventory's source references. Consumer qualification as of 2026-10-04 is recorded at immutable Anvil #312, Foundry #540, Nexus #519 and GitForgeOps #461 heads in the [ledger](implementation-status.md#cross-repository-dependencies). Root accepted the unchanged shared v1 freeze at qualified owner 81cbb; canonical publication is complete. The [owner/adoption record](shared-contract-qualification.md) preserves those qualified slices and the remaining fresh adoption gates without relabeling historical r2 evidence.

**v0.9.15 static re-audit (2026-10-08).** Compared the published tags
`v0.9.14..v0.9.15` after checking the intervening `v0.9.13..v0.9.14` step
below. Edge references are from the named tag. This is source evidence, not
execution of Alloy's new head.

1. **Changed files.** The checklist's five core files change in
   `src/config/types.rs`, `src/plugins/otel_tracing.rs`, and
   `src/proxy/headers.rs`; `src/retry.rs` and
   `src/plugins/correlation_id.rs` are unchanged. The change in `config/types.rs`
   adds helper methods for composition validation and proxy associations, not
   fields to Edge export. Re-read changed entries and all broader plugin,
   admission, and header behavior under items 3 to 9.
2. **`src/retry.rs`: no vocabulary change.** The eight `X-Gateway-Error`
   tokens and their mappings are unchanged. The `contracts-edge-0.9.15`
   gateway-errors vocabulary confirms the same eight tokens and records that
   `route_protocol_admission` maps to no token. Alloy's edge contract,
   diagnostics catalog, and `alloy.r007` remain paired; no new
   `does_not_prove` claim is needed. Version-neutral meanings remain unchanged.
   The PIN and all 16 selected files match the canonical tag; the two gateway
   vocabularies were copied byte-for-byte and their hashes recalculated.
3. **`src/proxy/headers.rs`: gateway-owned headers and overwrites.** Edge now
   treats exact `X-Authenticated-Identity` as a gateway assertion in addition
   to the `x-consumer-*` namespace, with case-insensitive and underscore-folded
   matching. It also confines client `Connection` nominations at ingress so
   nominations cannot remove later gateway assertions or plugin headers; the
   backend boundary still strips nominated hop-by-hop fields. The set of
   gateway request headers overwritten from forwarding metadata is unchanged.
   Alloy does not read `X-Authenticated-Identity` as a Consumer identity and
   reserves the name from request-ID configuration.
4. **`src/plugins/otel_tracing.rs`: no telemetry contract change.** The new
   Lightstep secret-reference validation and resolution do not change span
   kinds, `gateway.*` attributes, `traceparent` parsing, or timing fields.
5. **`src/plugins/correlation_id.rs`: unchanged.** The accepted request-ID
   grammar and exported correlation configuration are unchanged.
6. **`src/config/types.rs` and allowed keys.** Edge export fields and the
   `otel_tracing` allowed configuration keys are unchanged; the config changes
   add internal helper methods only.
7. **Plugin ordering and rejection phases.** Edge #6090 adds
   `route_protocol_admission` before plugins and upstream dispatch when the
   selected protocol cannot run the route's auth/admission policy. It belongs
   in `catalog::EDGE_PRE_UPSTREAM_PHASES`; Alloy now records it as likely
   admission evidence with regression coverage. No `X-Gateway-Error` token is
   assigned to this phase.
8. **Reserved `x-consumer-*` handling and external identity.** The entire
   `x-consumer-*` namespace remains gateway-owned. Edge #6088 adds
   `X-Authenticated-Identity` for external auth without a mapped Consumer; it
   is not emitted with `X-Consumer-Username` and is not proof of a mapped
   Consumer. No Alloy probe or diagnosis rule reads it. The telemetry layer
   rejects it as a request-ID header so it cannot be repurposed as correlation
   data.
9. **Previously unavailable entries.** No new Alloy-consumed header,
   diagnostic endpoint, span attribute, timing field, or evidence authority
   appeared. The new Edge identity header remains outside Alloy's trusted
   consumer identity contract.

**v0.9.14 static re-audit (2026-10-08).** Compared the published tags
`v0.9.13..v0.9.14` after checking the intervening `v0.9.12..v0.9.13` step
below. Edge references are from the named tag. This is source evidence, not
execution of Alloy's new head.

1. **Changed files.** The checklist's five core files change only in
   `src/plugins/otel_tracing.rs` (+5), `src/plugins/correlation_id.rs` (+4),
   and `src/retry.rs` (+64/-4). The broader plugin changes in `src/plugins/mod.rs`
   are reviewed under items 7 and 8. `src/config/types.rs`,
   `src/proxy/headers.rs`, and `docs/plugin_execution_order.md` are byte-identical.
2. **`src/retry.rs`: no vocabulary change.** Edge #6028/#6042 classify typed
   backend HTTP/2 resets and buffered read errors more accurately, including
   `ProtocolError` for backend resets with a reason other than `NO_ERROR`.
   `HTTP_OBSERVABILITY_ERROR_CLASSES` remains the same eight tokens; the
   `contracts-edge-0.9.14` gateway-errors vocabulary confirms no `ErrorClass`
   value or token was added. `contract::GATEWAY_ERROR_TOKENS`,
   `catalog::EDGE_GATEWAY_ERROR_TOKENS`, and rule `alloy.r007` remain paired;
   no new `does_not_prove` claim is required. Existing explanations remain
   version-neutral. The PIN and all 16 adopted files were already updated in
   this PR and match the canonical `contracts-edge-0.9.14` tag.
3. **`src/proxy/headers.rs`: unchanged.** Backend-response stripping and
   Edge-owned request-header overwrites are unchanged.
4. **`src/plugins/otel_tracing.rs`: declarations only.** The change declares
   `traceparent` and `tracestate` among request headers the plugin may modify;
   span kinds, attributes, meanings, trace-context parsing, and timing
   boundaries are unchanged. No timing field was added.
5. **`src/plugins/correlation_id.rs`: declarations only.** The change reports
   the configured request-id header as mutable. Its accepted grammar and
   exported configuration are unchanged.
6. **`src/config/types.rs` and allowed keys: unchanged.** Edge export's fields,
   bounds, and `otel_tracing` allowed configuration keys did not change.
7. **Plugin ordering and rejection phases.** `docs/plugin_execution_order.md`
   is unchanged. No `rejection_phase` value was added in v0.9.14, and no new
   phase belongs in `catalog::EDGE_PRE_UPSTREAM_PHASES`.
8. **Reserved `x-consumer-*` names.** The namespace stripping behavior is
   unchanged. The `src/plugins/mod.rs` changes make built-in plugin trust depend
   on concrete registered types, but do not change which consumer assertion
   headers Edge strips.
9. **Previously unavailable entries.** No new Alloy-consumed header,
   diagnostic endpoint, span attribute, timing field, or evidence authority
   appeared. The HTTP/2 classification changes do not add an exported contract.

**v0.9.13 static re-audit (2026-10-08).** Compared the published tags
`v0.9.12..v0.9.13`; this records the intervening step omitted from the v0.9.13
adoption. Edge references are from the named tag. This is source evidence, not
execution of Alloy's new head.

1. **Changed files.** Among the checklist surfaces, `src/plugins/mod.rs`
   (+60/-3), `src/plugins/mesh_route_dispatch.rs` (+166/-2),
   `src/plugins/otel_tracing.rs` (+5), and `src/plugins/correlation_id.rs`
   (+4) change. `src/retry.rs`, `src/config/types.rs`,
   `src/proxy/headers.rs`, and `docs/plugin_execution_order.md` are unchanged.
2. **`src/retry.rs`: unchanged vocabulary.** The eight `X-Gateway-Error`
   tokens and class mapping are unchanged. No token was added, so the Alloy
   constants, `alloy.r007` explanations and `does_not_prove` limits remain
   paired with the pinned vocabulary.
3. **`src/proxy/headers.rs`: unchanged.** Backend-response stripping and
   gateway-owned request-header overwrites are unchanged.
4. **`src/plugins/otel_tracing.rs`: declarations only.** `traceparent` and
   `tracestate` are declared as mutable request headers; span kinds, attributes,
   trace-context parsing, and timing boundaries are unchanged.
5. **`src/plugins/correlation_id.rs`: declarations only.** The configured
   request-id header is declared mutable; its accepted grammar and exported
   configuration are unchanged.
6. **`src/config/types.rs` and allowed keys: unchanged.** No generated Edge
   configuration field or `otel_tracing` allowed key changed.
7. **Plugin ordering and rejection phases.** Plugin execution order is
   unchanged. Edge #6024 introduces `route_request_timeout_early_upload`, a
   request deadline rejection before an upstream attempt. Alloy's R001 rule
   already treats catalogued pre-upstream phases as gateway admission evidence;
   `catalog::EDGE_PRE_UPSTREAM_PHASES` now includes this value, with a regression
   check that it produces the pre-upstream finding.
8. **Reserved `x-consumer-*` names.** The gateway-owned namespace behavior is
   unchanged. The new early route-total preview operates on request inputs and
   does not alter reserved-header stripping.
9. **Previously unavailable entries.** The early upload deadline adds the
   rejection phase above, but no new header, telemetry attribute, diagnostic
   endpoint, or authority. Other Alloy-consumed unavailable entries remain
   unavailable.

**v0.9.12 static re-audit (2026-10-05).** Compared the actual published
`v0.9.11..v0.9.12` source archives and the [immutable release diff](https://github.com/ferrum-edge/ferrum-edge/compare/c764084b3b51c3f7ffde268c039688d35e49c553...0d917701b63ef38210c49df830f48cf0457cbc7d).
This is source and byte-integrity evidence, not execution of Alloy's new head.

1. **Changed files.** Edge #6012 adds the opt-in admin `deployment-v1` snapshot,
   dependency-fenced partial mutations and explicit durable/live/recovery
   acknowledgements. Admin handlers, database stores and OpenAPI documentation
   change; Alloy's data-plane, telemetry and file-mode config sources do not.
2. **`src/retry.rs`.** Byte-identical to v0.9.11. Re-read all eight coarse tokens,
   class mapping and rejection tokens. Canonical gateway-errors changes only
   release/source provenance. `GATEWAY_ERROR_TOKENS`, the complete catalog and
   `PINNED_GATEWAY_ERROR_MEANINGS` remain paired; version-neutral explanations
   and every `does_not_prove` limit stay unchanged.
3. **`src/proxy/headers.rs`.** Byte-identical. Rechecked the reserved assertion
   namespace and the three gateway-owned diagnostic response headers. Backend
   copies remain stripped; transport identity remains Alloy's trust authority.
4. **`src/plugins/otel_tracing.rs` and `attempt_spans.rs`.** Both byte-identical.
   SERVER/CLIENT parentage, client echo, attributes, allowed keys and body/attempt
   completion boundaries are unchanged. No timing or backoff measurement is added.
5. **`src/plugins/correlation_id.rs`.** Byte-identical. Request-id grammar and
   exported `x-request-id` configuration are unchanged.
6. **`src/config/types.rs` and health probes.** Byte-identical. Existing resource
   fields, ID/path bounds, backend TLS and active-health settings remain. Alloy
   emits no new field; YAML payloads are unchanged outside release/source comments.
7. **Plugin ordering and MCP.** `src/plugins/mod.rs`,
   `docs/plugin_execution_order.md`, the API-spec extractor and MCP bridge are
   byte-identical. `docs/api_specs.md` only adds the deployment-recovery section;
   the exported `x-ferrum-mcp` shape and rejection-phase catalog are unchanged.
8. **Gateway vocabulary and G01.** Reviewed every canonical header's meaning and
   availability. Only admin ETag/If-Match descriptions gain the deployment profile;
   the three released gateway diagnostic headers and reference grammar are unchanged.
   `src/diagnostic_ref.rs` is byte-identical; its canonical schema changes only
   `x-contract` provenance, and all ten lookup fixtures are byte-identical.
9. **Previously unavailable items and authority.** No new Alloy-consumed route,
   attempt or diagnostics-request header, signed context, reference on a span,
   retry-backoff duration or schema URL appears. Alloy does not call
   `GET /deployment-snapshot` or its conditional mutation routes, retain their
   secret-complete evidence, or vendor their schemas/fixtures. G01 lookup,
   namespace authorization, secret privacy and confirmation limits remain intact.

All 16 adopted files were downloaded from the immutable canonical commit;
their Git blob IDs match its complete published tree and SHA-256 values are
recorded in PIN. The report schema and all 12 diagnostic fixtures are unchanged;
the report's `coordinated_release.contracts_tag` still records the original
`contracts-edge-0.9.11` freeze at owner 81cbb, separately from this adoption pin.
The [v0.9.12 canonical release](https://github.com/ferrum-edge/ferrum-contracts/releases/tag/contracts-edge-0.9.12)
was published at 13:58:38 UTC on 2026-10-05 after its sole main PUSH
[Validate contracts run 37320780987](https://github.com/ferrum-edge/ferrum-contracts/actions/runs/37320780987)
succeeded; that repository has no release workflow. Tagged pending-publication
wording remains immutable historical text. Root's upstream Edge distribution
verification is recorded in [compatibility.md](compatibility.md); neither upstream
qualification nor the old bot head's passing Edge jobs qualifies this new Alloy head.

**v0.9.11 static re-audit (2026-10-04, historical).** Compared the actual published tags
`v0.9.10..v0.9.11`, with v0.9.11 resolved to the full source SHA above. This is
source evidence; the new Alloy pairing still requires hosted CI on its exact commit.

1. **Changed files.** The release changes transport/auth-lifetime internals, TLS
   source admission and admin backup/verification/egress APIs. Existing Alloy
   generated fields, MCP extension keys, gateway assertions and telemetry names
   remain present. No new contract is assumed by Alloy.
2. **`src/retry.rs`.** The eight gateway tokens, their meanings and class-to-token
   table are unchanged. New internal hyper classification and authorization-expiry
   handling add no vocabulary value. Canonical gateway-errors changes only release
   provenance; local tokens and version-neutral diagnostic explanations remain paired.
3. **`src/proxy/headers.rs`.** Changes reserve map capacity and compare unchanged
   repeated values before parsing names. Gateway-owned request/response strip sets,
   including `x-consumer-*` and diagnostic headers, retain their contracts.
4. **`src/plugins/otel_tracing.rs` and `attempt_spans.rs`.** Exporter atomic calls
   change from `fetch_update` to `try_update`; attributes, allowed config keys and
   boundaries are unchanged. Attempt-span source is byte-identical. No timing field
   or backoff measurement is added; unknown remains unknown.
5. **`src/plugins/correlation_id.rs`.** Byte-identical; request-id grammar and the
   exported `x-request-id` configuration are unchanged.
6. **`src/config/types.rs`.** Existing resource fields/ID bounds remain. TLS admission
   now rejects explicit source selectors inconsistent with cert/key/CA material kind
   (Edge #5959). Alloy's fixtures use ordinary file paths, not conflicting selectors;
   no generated field or fixture payload changes. Full real-validator CI stays required.
7. **Plugin ordering and MCP.** `docs/plugin_execution_order.md`, the API-spec
   documentation, `src/admin/api_specs/extractor.rs` and `mcp_openapi_bridge.rs`
   are byte-identical. Plugin trait changes are lint annotations; rejection phases
   and the exported MCP shape are unchanged. Agent-tool admission CI is retained.
8. **Reserved headers and G01.** Reserved namespace handling and reference grammar
   remain. `src/diagnostic_ref.rs`, the diagnostic-ref schema and all ten lookup
   fixtures are byte-identical. The canonical header vocabulary adds admin-only
   ETag/If-Match entries, not a new Alloy diagnostic or identity header.
9. **Previously unavailable items.** No route/attempt/diagnostic-request header,
   signed context, diagnostic reference on a span, retry-backoff measurement or
   schema URL is introduced. New admin snapshot/verification/egress surfaces are
   outside Alloy's explicit G01 lookup and grant no new confirmation authority.

Canonical gateway vocabularies are copied from `contracts-edge-0.9.15`, never
edited locally. The report's `x-contract` records EXISTING/implemented at owner
81cbb with unreleased availability; every historical PROPOSED description outside
`$id`/`x-contract` remains exactly paired. Tagged pending-publication text is the
source's pre-publication record, superseded by the actual GitHub release.

**v0.9.10 re-audit (2026-10-01).** The checklist in [compatibility.md](compatibility.md#re-audit-checklist), run against `v0.9.9..v0.9.10`. The only source changes are Edge #5954 in MCP prompt-shield and MCP gateway handling. Edge references are `v0.9.10` file:line unless marked.

1. **Changed files.** `src/plugins/otel_tracing.rs`, `src/plugins/correlation_id.rs`, `src/config/types.rs`, `src/proxy/headers.rs`, `src/retry.rs`, `src/plugins/mod.rs`, and the configuration/export and plugin-ordering sources named by the checklist are unchanged. The #5954 changes are in `src/plugins/ai_prompt_shield.rs`, `src/plugins/mcp_gateway.rs`, and `src/plugins/utils/mcp_jsonrpc.rs`; they refuse non-UTF-8 charsets and fail closed when MCP JSON-RPC bodies cannot be safely inspected. Alloy's consumed header, identity, request-id, tracing, telemetry, error-token, generated configuration, and exported `x-ferrum-mcp` extension-shape contracts are unaffected; Edge applies stricter runtime checks to MCP requests.
2. **`src/retry.rs`: no vocabulary change.** The `X-Gateway-Error` tokens and class mapping are unchanged. v0.9.10 adds no tokens, so `contract::GATEWAY_ERROR_TOKENS`, `catalog::EDGE_GATEWAY_ERROR_TOKENS`, and rule `alloy.r007` need no change. The vendored gateway vocabularies are byte-identical in the adopted `contracts-edge-0.9.9-r2` pin; no vocabulary semantics changed.
3. **`src/proxy/headers.rs`: unchanged.** Backend response stripping and gateway-owned request-header handling are unchanged.
4. **`src/plugins/otel_tracing.rs`: unchanged.** Span kinds, `gateway.*` attributes, `traceparent` parsing, and tracing configuration are unchanged; no timing field was added.
5. **`src/plugins/correlation_id.rs`: unchanged.** The accepted request-id grammar and configuration are unchanged.
6. **`src/config/types.rs` and `otel_tracing` allowed keys: unchanged.** The new MCP request checks add no exported configuration field.
7. **Plugin execution ordering and rejection phases: unchanged.** No `rejection_phase` value or Alloy catalog entry changed.
8. **Reserved `x-consumer-*` handling: unchanged.** The gateway-owned request namespace behavior is unchanged.
9. **Previously unavailable entries: unchanged.** The MCP hardening adds no header, telemetry attribute, endpoint consumed by Alloy, or other Alloy-consumed contract.

The preceding **v0.9.9 re-audit (2026-10-01)** compared `v0.9.8..v0.9.9`; its historical results follow. References in that entry describe the release behavior at that time, not current support status. The contract descriptions below retain their historical source offsets; the current v0.9.12 comparison above rechecks the surfaces Alloy consumes. Older release comparisons explain compatibility with earlier telemetry or behavior.

1. **Changed files.** All five listed files changed: `src/config/types.rs` (+293), `src/proxy/headers.rs` (+107), `src/plugins/otel_tracing.rs` (+48, plus the new `src/plugins/otel_tracing/attempt_spans.rs`), `src/plugins/correlation_id.rs` (+11), and `src/retry.rs` (+1/-1). Every entry backed by them was re-read (items 2 to 6).
2. **`src/retry.rs`: no vocabulary change.** The only change makes `rustls_error_from_chain` `pub(crate)` (`:1002`). The eight `X-Gateway-Error` tokens (`:212-223`, `HTTP_OBSERVABILITY_ERROR_CLASSES` `:235-244`) and the class mapping (`x_gateway_error_token_for_class`, `:310`) are unchanged. The contracts pin moved to `contracts-edge-0.9.9`. Its `gateway-errors.json` changes only provenance and drops the main-branch note, with identical token meanings, so `catalog::EDGE_GATEWAY_ERROR_TOKENS` and `PINNED_GATEWAY_ERROR_MEANINGS` stand. Its `gateway-headers.json` lists `X-Ferrum-Diagnostic-Ref` as a released gateway diagnostic header, which Alloy mirrors as `contract::DIAGNOSTIC_REF` (item 9). The diagnostic-report schema and Finding fixtures are byte-identical.
3. **`src/proxy/headers.rs`.**
   - `X-Ferrum-Diagnostic-Ref` joins `GATEWAY_OWNED_DIAGNOSTIC_RESPONSE_HEADERS` (`:842-856`), so a backend copy is stripped at every backend response boundary, as `X-Gateway-Error` already is (§3).
   - The whole `x-consumer-*` request namespace is gateway-owned (Edge #5880). Matching is ASCII case-insensitive and treats `_` as `-` (`is_consumer_assertion_header`, `:129-143`). Client copies are dropped at ingress, before any plugin (`src/plugins/mod.rs:6553-6556`, `:6600-6609`), and from the raw merge base (`strip_reserved_gateway_assertion_headers`, `:618-628`). This closes the §1 `x-consumer-*` gap and §9 item 2 for v0.9.9 only; v0.9.8 still strips just the two names. Alloy needs no change: it trusts only `x-consumer-username` and `x-consumer-custom-id`, only from a verified identity, and no Alloy test assumed other names were stripped.
   - The request headers Edge overwrites (forwarding headers, `X-Forwarded-For`) are unchanged.
4. **`src/plugins/otel_tracing.rs` and `attempt_spans.rs`.**
   - Span kinds: with an exporting instance and a sampled request, every backend attempt on an instrumented dispatch path exports one CLIENT span, a child of the Edge SERVER span (Edge #5864, #5867, #5875). Those paths are the HTTP/1.1 and HTTP/2 backend loop, the native gRPC loop, the HTTP/3 paths and bridges, and WebSocket upgrades. The backend receives that span as its `traceparent` parent (`BackendAttemptTrace::begin`, `attempt_spans.rs:252-300`), so the service's SERVER span nests under the attempt. Uninstrumented paths keep the SERVER span's `traceparent` and export no attempt span.
   - The `traceparent` echoed to the client still names the SERVER span (`after_proxy`, `otel_tracing.rs:1043-1050`).
   - A request dropped mid-attempt exports the attempt in flight with `error.type = cancelled` once the backend had it (`attempt_spans.rs:404-411`), so a client disconnect alone does not leave a service span under an unexported parent. An attempt span can still go missing: Edge drops it, with a rate-limited warning, when its export buffer is full (`export_with`, `attempt_spans.rs:88-112`), and an export lost on the way to the collector has the same effect. A service span under a missing attempt span is not linked to the gateway request; rule `alloy.r004` lists the unexported attempt span as a possible explanation.
   - `traceparent` parsing (`parse_traceparent`, `:710`; v0.9.8 `:675`), the SERVER span's `gateway.*` attributes (`:2466-2467` onward), the resource attributes (`:2688`), and `ALLOWED_CONFIG_KEYS` (`:78`) are unchanged.
   - Alloy imports each attempt as an `edge.backend.attempt` event that carries its span link and attempt number, and links a service span to the gateway request through at most one attempt (§4). It also interprets the attempt span's duration (`edge.backend.attempt.duration`) and the five connection attributes (`attempt_spans.rs:712-731`), each scoped to its attempt, as defined in `measurement-semantics.md` and the catalog. Rule `alloy.r003` compares each attempt with the service request under it. The `edge-e2e` job saw this shape on v0.9.9 (run 36829511173: Alloy SERVER span → Edge CLIENT span with `gateway.backend.attempt=1` → Edge SERVER span, and one attempt span for each of the two retry attempts).
5. **`src/plugins/correlation_id.rs`: request-id grammar unchanged.** `is_valid_correlation_id` (`:218-223`) and the 256-byte limit (`:268`) are the same, so `ferrum-alloy-telemetry`'s mirror stands. New: `header_name` may not name an `x-consumer-*` header (`:175-180`). Export configures `x-request-id`.
6. **`src/config/types.rs`.**
   - `Proxy` (`:2776`; v0.9.8 `:2646`) gains `allow_path_parameters` (`:2850`, default `false`) and `websocket_permessage_deflate` (`:3189`, default `strip`).
   - `Upstream` (`:1910`), `PluginConfig` (`:3247`), `GatewayConfig` (`:3398`), and `RetryConfig` (`:2398`) gain no fields.
   - Admission refuses a literal `listen_path` that contains `;` unless the proxy opts in (`listen_path_requires_path_parameters`, `:3928`; checked at `:4316` and `:8268`). It also refuses empty segments and dot segments with a `;` parameter (`src/policy_path.rs` rules 5, 9, and 10; GHSA-fcqw-793q-wg5x, GHSA-5mrg-vq2h-6j3w).
   - **Decision:** `edge export` emits neither new field. Both defaults are what an Alloy service wants: `;` path parameters stay refused before routing, and WebSocket compression stays stripped. Both supported releases accept the generated fields, and unsupported v0.9.8's `deny_unknown_fields` would reject either new field. Instead, manifest validation now refuses `;`, `%`, `\`, and `.`/`..` segments in `api.public_path`, so export cannot produce a `listen_path` that a supported release refuses. The same check applies to `api.service_base_path` and `health.path`, where it is stricter than Edge requires. The `edge-config` job validates the generated fixture on both supported releases.
7. **`rejection_phase`.** `docs/plugin_execution_order.md` names the same phases, so `catalog::EDGE_PRE_UPSTREAM_PHASES` is unchanged. v0.9.9's diagnostic-reference labels (`src/diagnostic_ref.rs:200-255`) add three phases, none a pre-upstream transaction-log value Alloy reads:
   - `h1_framing_unverified` (`src/proxy/mod.rs:32110`) and `client_trust_withdrawn` (`:32182`) are recorded only in the diagnostic reference;
   - `websocket_permessage_deflate` (`:16275`) refuses a backend's answer after the attempt.
8. **`src/plugins/mod.rs` reserved names.** The whole `x-consumer-*` namespace in v0.9.9 (item 3); `x-geo-country` and `x-path-param-*` are unchanged (`:6553-6556`).
9. **UNAVAILABLE entries.**
   - Now produced by v0.9.9: the diagnostic reference (§3, §6) and per-attempt CLIENT spans with pool connection timings (§4). Alloy relies on neither for a timing or a confidence: it records a reference a client observed but does not resolve it (the reference is off by default), and attempt spans are used only for linkage.
   - Still unavailable: a route id header, an attempt header to the backend, a diagnostics-request header, a signed gateway context, and `schema_url` (no matches in `src/` at v0.9.9).

The remaining EXISTING entries in `src/proxy/mod.rs`, `src/plugins/mod.rs`, `src/health_check.rs`, and `src/admin/mod.rs` were spot-checked at v0.9.9: the functions and strings they cite are present. The e2e and `edge-config` jobs cover them on both releases.

## 1. Request metadata Edge sends to the service

| Item | Status | Source (Edge) | Producer → consumer | Trust rules | Lifecycle | Protocols | Tests |
|---|---|---|---|---|---|---|---|
| `traceparent` | EXISTING | v0.9.9 `src/plugins/otel_tracing.rs:1013-1041` (`before_proxy`), parse `:710`; per-attempt value `src/plugins/otel_tracing/attempt_spans.rs:252-300`. v0.9.8 `otel_tracing.rs:978-998`, parse `:675-711` | `otel_tracing` plugin → Alloy telemetry layer | Edge removes every case variant and inserts its own value. The parent id is the **Edge SERVER span** in v0.9.8, and in v0.9.9 the **Edge CLIENT span of the attempt** on instrumented dispatch paths (re-audit item 4); flags are only `00`/`01`. Alloy accepts it only from a trusted peer (default `trusted_peers`). Receiving it never authenticates anything. | Request head, once per attempt. **In v0.9.8 every retry attempt carries the same value** (`src/proxy/mod.rs` `proxy_to_backend_retry` reuses headers); in v0.9.9 each instrumented attempt carries its own span id. | HTTP/1.1 and HTTP/2 to Alloy. The e2e run used HTTP/2 via ALPN. | Edge: `tests/unit/plugins/otel_tracing_tests.rs` (`…propagates_existing_traceparent`, `…before_proxy_replaces_all_caller_trace_context_casings`, `…untrusted_parent_creates_fresh_root`). Alloy: `crates/ferrum-alloy-telemetry/tests/context_and_trust.rs`, `crates/ferrum-alloy/tests/gateway_mtls.rs`, `edge-e2e` (including the retry case: two Alloy SERVER spans under the same Edge SERVER span, each through its own attempt span on v0.9.9) |
| `tracestate` | EXISTING | `otel_tracing.rs:924`, `:941-944` | Edge → Alloy | Forwarded verbatim only when Edge trusted the caller's context (`trace_context_trust: trusted`); otherwise dropped. Edge adds no member. Alloy validates the W3C grammar (≤32 members, ≤512 bytes) and propagates only accepted values. | Request head | HTTP/1.1, HTTP/2 | Edge: `…preserves_tracestate`, `…invalid_parent_drops_tracestate`. Alloy: `tracestate_validation_follows_the_w3c_grammar` |
| `baggage` | EXISTING (pass-through) | Not touched by `otel_tracing`. Mesh egress strip only: `src/proxy/mod.rs:32881-32894` | Client → Edge → service | Untrusted. Alloy never parses it and never forwards it; `http_client` strips it. | Request head | all | Alloy: `trace_context_goes_only_to_listed_hosts` |
| `x-request-id` | EXISTING (when the `correlation_id` plugin is attached) | v0.9.9 `src/plugins/correlation_id.rs:218-223` (grammar), `:268` (256-byte limit); unchanged from v0.9.8 apart from the `x-consumer-*` `header_name` refusal (`:175-180`) | `correlation_id` → Alloy | Edge keeps a client value of at most 256 bytes of `[A-Za-z0-9._-]`, otherwise generates a UUIDv4. Alloy applies the same rule and reuses Edge's id when Edge is a trusted peer (`request_id.accept_incoming` defaults to `trusted_peers`). It is a correlation aid, not a credential. Without the plugin Edge sends no request id, and Alloy generates one. | Request head; Edge echoes it on responses and rejects | all | Edge: `tests/unit/plugins/correlation_id_tests.rs`. Alloy: `valid_request_ids_are_kept_and_echoed`, `invalid_or_oversized_request_ids_are_replaced`, `edge-e2e` ("Alloy used the gateway's request id") |
| `x-consumer-username` | EXISTING | Doc `docs/plugins.md:288-297`. Built `src/plugins/mod.rs:6516-6560`, injected `src/proxy/mod.rs:16606-16645` (v0.9.9 `refresh_backend_gateway_assertion_headers`, `:17154`). Client copies stripped: v0.9.9 the whole namespace (`src/plugins/mod.rs:6553-6556`, `:6600-6609`; `src/proxy/headers.rs:618-628`); v0.9.8 this name only (`src/plugins/mod.rs:6363-6368`) | Edge auth plugins → Alloy Edge adapter (`GatewayContext`) | Accepted only when the peer is a **verified mTLS identity** in `trust.identities` and `edge.accept_consumer_identity = true`. Otherwise removed before handlers run. Network-boundary trust never authorizes it. Authentication by Edge; authorization stays in the application. | Request head | all | Alloy: `crates/ferrum-alloy-edge/tests/policy.rs`, `gateway_mtls.rs` |
| `x-consumer-custom-id` | EXISTING | same as above | same | same | same | same | same |
| `x-consumer-*` (other names) | EXISTING in v0.9.9 (gateway-owned namespace); gap in v0.9.8 | v0.9.9: `is_consumer_assertion_header` (`src/proxy/headers.rs:129-143`, case-insensitive, `_` equals `-`), dropped at ingress (`src/plugins/mod.rs:6600-6609`) and on every outbound path (Edge #5880). v0.9.8: only the two exact names are reserved on the plain HTTP path (`src/plugins/mod.rs:6363-6368`) | client → service (v0.9.8 only) | Behind v0.9.8 a client can send, for example, `X-Consumer-Role` through Edge; behind v0.9.9 it cannot. Alloy trusts no `x-consumer-*` name other than the two above, and those only from a verified identity, whichever release is in front. | — | v0.9.9: HTTP/1.1, HTTP/2, HTTP/3, gRPC, WebSocket. v0.9.8: HTTP | — |
| `X-Forwarded-For` | EXISTING | `src/proxy/mod.rs:4884-4927` (`build_xff_value`) | Edge → Alloy | Regenerated. An untrusted chain is dropped unless the peer is in `FERRUM_TRUSTED_PROXIES`, so the rightmost hop is written by Edge. Alloy exposes it only as `GatewayContext.client_address` from a verified identity, and never uses it for trust decisions. | Request head | all | Edge: `tests/functional/functional_forwarded_via_headers_test.rs`. Alloy: `verified_gateway_identity_is_handed_off`, `forwarded_headers_never_establish_trust` |
| `X-Forwarded-Proto`, `X-Forwarded-Host` | EXISTING | `src/proxy/mod.rs:42075-42086`, `src/proxy/headers.rs:127-170` | Edge → service | Overwritten by Edge. Alloy does not consume them. | Request head | all | Edge functional tests |
| `Forwarded` | EXISTING (opt-in) | `FERRUM_ADD_FORWARDED_HEADER`, default `false` (`src/config/env_config.rs:4869`) | Edge → service | When disabled, **a client's value passes through**. Alloy ignores it. | Request head | all | Edge functional tests |
| `Via` | EXISTING | `src/proxy/mod.rs:9782-9791`, default on (`env_config.rs:4867`) | Edge → service | Appended; spoofable. Not an Edge marker for Alloy. | Request and response | all | — |
| `X-Real-IP`, `X-Forwarded-Port` | EXISTING | `src/proxy/headers.rs:180-183` | client → service | Not generated. `X-Forwarded-Port` is not stripped. Alloy ignores both. | Request head | all | — |
| `x-geo-country`, `x-path-param-*` | EXISTING | `src/plugins/mod.rs:6142-6146` | Edge → service | Stripped from clients, injected by Edge. Alloy does not consume them yet. | Request head | all | — |
| Route id header (`x-ferrum-route-id` or similar) | **UNAVAILABLE** | none (`grep x-ferrum` finds only internal and admin names) | — | Alloy uses its own route template. | — | — | — |
| Attempt number or attempt id to the backend | **UNAVAILABLE** as a header | none | — | Alloy reports it as missing evidence. In v0.9.8 retries appear as sibling Alloy SERVER spans under one Edge SERVER span; in v0.9.9 each is under its own attempt span (§4), whose number scopes that attempt's evidence. | — | — | Alloy rule `alloy.gateway.multiple_service_attempts` |
| Diagnostics-request header | **UNAVAILABLE** | none | — | — | — | — | — |
| Signed gateway context | **UNAVAILABLE** | none | — | Alloy relies on mTLS identity instead. It does not add ad hoc signatures. | — | — | — |

## 2. Gateway identity toward the service

| Item | Status | Source | Notes | Tests |
|---|---|---|---|---|
| Backend client certificate: `backend_tls_client_cert_path` / `backend_tls_client_key_path` (proxy, upstream, or global `FERRUM_BACKEND_TLS_CLIENT_CERT_PATH`) | EXISTING | `src/config/types.rs:2720-2724` (proxy), `:1928-1931` (upstream); `docs/backend_mtls.md:24-35` | With `upstream_id` set, the upstream's TLS settings win. Pointing the paths at SPIFFE SVID files presents an SVID. Edge does not present an SVID automatically. | e2e: Edge presents `spiffe://ferrum.demo/ns/edge/sa/gateway` and Alloy verifies it (`peer.trust=verified_identity`) |
| Server verification: `backend_tls_verify_server_cert` (default true), `backend_tls_server_ca_cert_path` | EXISTING | `types.rs:2728-2732`, `:1934-1937` | A custom CA replaces public roots. | e2e (Alloy's demo CA) |
| Active health probes present the backend client certificate | EXISTING | `src/health_check.rs:2729-2750`, `:3661-3683` | So `client_auth = required` works with Edge health checks. | Generated upstream in the e2e stack |
| `X-Authenticated-Identity` | EXISTING in v0.9.15; not consumed by Alloy | `contracts-edge-0.9.15` `vocabularies/gateway-headers.json`; `src/proxy/headers.rs` (`is_consumer_assertion_header`), Edge #6088 | External authenticated identity or display claim, emitted only when the auth flow has no mapped Consumer. It is not emitted with `X-Consumer-Username`; client copies are removed, and the header is forbidden as a configured mutation destination. | Alloy does not accept it in `GatewayContext`; telemetry rejects it as a configured request-ID header. |

## 3. Edge responses and error signals

| Item | Status | Source | Semantics | Trust | Alloy use |
|---|---|---|---|---|---|
| `X-Gateway-Error` | EXISTING | `src/retry.rs:212-244` (tokens), `:310-318` (class mapping), unchanged through v0.9.15 from v0.9.9 and v0.9.8; backend copies stripped by v0.9.9 `src/proxy/headers.rs:809`, `:852-870` (v0.9.8 `:754-757`, `:777-816`) | Closed vocabulary, on gateway-authored 5xx only: `connection_failure`, `backend_timeout`, `backend_error`, `circuit_breaker_open`, `overload`, `config_stale`, `concurrency_limit`, and `request_timeout` (a route's total request deadline expired before any backend held the request). `backend_timeout` denotes a gateway backend or route timeout; releases before v0.9.8 also used it for route deadlines that expired before dispatch. The header alone does not prove whether the service received the request. | Both supported releases strip backend-supplied copies at every backend response boundary; releases before v0.9.8 do not on every path (Anvil `catalog/ferrum/ferrum-edge-0.9.7/outcomes.json`). The header is unauthenticated and names no Edge version, so it stays unverified evidence. | Diagnosis rule `alloy.r007` caps confidence at `likely`, keeps the broader pre-v0.9.8 meaning of `backend_timeout` because a header names no release, and lists what each token does not prove. For example, `connection_failure` does not prove a DNS failure. An unknown token yields `unknown`. `edge-e2e` checks `connection_failure` for a refused backend connection on both supported releases. |
| `X-Ferrum-Diagnostic-Ref` | EXISTING since v0.9.9, retained in v0.9.15/v0.9.14 (off by default); UNAVAILABLE in legacy v0.9.8 | v0.9.9 `src/diagnostic_ref.rs:82` (`DIAGNOSTIC_REF_HEADER`), `:96-122` (`fd1_`/`fd2_` forms), stamped as the last step before the client response head (`stamp_response_headers`, `:1672`); backend and plugin copies stripped (`src/proxy/headers.rs:838-870`); enabled by `FERRUM_DIAGNOSTIC_REFS` (`src/config/env_config.rs:4574`, default `off`). `docs/error_classification.md` "Gateway diagnostic references", `docs/admin_api.md` "Diagnostic References". Pinned grammar: `contracts-edge-0.9.9` `vocabularies/gateway-headers.json` (`values.pattern`) | An opaque reference, `fd1_<32 lowercase hex>`, or `fd2_<8 lowercase hex replica id>_<32 lowercase hex>` with `FERRUM_DIAGNOSTIC_REF_REPLICA_TAG=true`. It embeds nothing. `errors` mode stamps every HTTP/1.1, HTTP/2, and HTTP/3 response that carries the gateway's own `X-Gateway-Error` token; `all` mode also stamps plugin rejections, gateway policy refusals, and routing `404`s. A backend's own response, relayed or replayed by a plugin, never carries one. | Gateway-owned whatever the setting, but unauthenticated: the header names no Edge version, and any server can send one. It is a pointer to authenticated evidence, never evidence itself. | Recorded from a response a client observed as a `client.response_header` event (`measurement-semantics.md`, `catalog::EDGE_DIAGNOSTIC_REF_HEADER`); Edge puts it on no span (§4). Rule `alloy.r007` reports a well-formed reference as `alloy.edge.diagnostic_ref` (`likely`, naming the lookup in `confirm_with`) and anything else as `alloy.edge.diagnostic_ref_malformed` (`unknown`, never offered for lookup); neither changes another finding. `catalog::is_edge_diagnostic_ref` implements the pinned grammar, which the pairing test compares. `edge-e2e` turns references on (`FERRUM_DIAGNOSTIC_REFS=errors`) and checks on both supported releases that the refused connection's `502` carries a well-formed reference that diagnosis records. |
| `X-Ferrum-Diagnostic-Owner-Replica` | EXISTING in v0.9.9 (admin lookup only) | v0.9.9 `src/diagnostic_ref.rs:127` | On a lookup `404` for an `fd2_` reference another replica minted, names the owner replica, only for a caller whose token passed the scope and namespace checks. | Admin response header. | Not followed: the explicit ADR 0009 lookup never uses an owner hint for automatic routing or retry. |
| `X-Gateway-Upstream-Status: degraded` | EXISTING | v0.9.9 `src/proxy/mod.rs:42523`, `:42625`; v0.9.8 `:41018`, `:41120` | The all-unhealthy fallback target was used. | Stripped from backend responses in both supported releases (same list as `X-Gateway-Error`) | Not interpreted yet |
| Gateway error bodies `{"error":"…"}` | EXISTING | `src/proxy/mod.rs:25817`, `:48013-48040` | Plain JSON, not Problem Details. | — | Not parsed. Body text is weak evidence (Anvil convention). |
| `traceparent` echoed to the client | EXISTING | v0.9.9 `otel_tracing.rs:1043-1050` (`after_proxy`); v0.9.8 `:1000-1014` | The Edge SERVER span. In v0.9.8 this is the value Edge sent upstream; in v0.9.9 the backend receives the attempt's CLIENT span instead. | — | `edge-e2e` checks it names the Edge SERVER span, and that the Alloy span is under it, directly or through one attempt span. |
| `Server-Timing` | EXISTING (untouched) | No references in `src/`, `tests/`, `docs/` | Edge neither emits nor strips it. It is not in the backend-response strip set (`src/proxy/headers.rs:745-757`). | — | Alloy's opt-in `Server-Timing` would reach clients unchanged. **Not tested through Edge.** |

## 4. Edge telemetry (OTLP) Alloy interprets

Edge exports **OTLP/HTTP JSON only** (`otel_tracing.rs:2165-2173`), hand-written without an OTel SDK, and, in v0.9.8, only **SERVER** spans (`:140-173`), with no CLIENT spans for upstream attempts and no per-retry spans. v0.9.9 adds one CLIENT span per backend attempt (row below). The span ends at transaction summary time, which for streamed responses is body completion (`src/proxy/deferred_log.rs`).

| Attribute | Status | Source | Meaning | Alloy handling |
|---|---|---|---|---|
| `gateway.latency.total_ms` | EXISTING | `otel_tracing.rs:2419` | Handler entry until summary; refreshed at body completion for streamed responses. | `edge.request.total` |
| `gateway.latency.backend_ttfb_ms` | EXISTING | `:2420` | Backend dispatch start until response headers, **across every retry attempt and backoff**. For buffered responses it **equals the full backend exchange** (`src/proxy/mod.rs:39286-39292`). Always exported; `-1` means unknown. | `edge.backend.time_to_headers`. A negative value becomes `unavailable`, never zero. |
| `gateway.latency.backend_total_ms` | EXISTING | `:2448-2452` | Buffered responses only. Omitted when unknown. | `edge.backend.total` |
| `gateway.latency.processing_ms`, `gateway.overhead_ms`, `gateway.plugin_execution_ms` | EXISTING | `:2424-2458` | Derived (`src/plugins/mod.rs:8234-8251`). | `edge.plugin_execution` only |
| Diagnostic reference on a span | **UNAVAILABLE** | none: v0.9.9 `otel_tracing.rs` and `attempt_spans.rs` export no reference attribute | Edge stamps the reference only on the client response (§3). | The OTLP importer records no reference; only a client-observed header does. |
| `gateway.response.streamed` | EXISTING | `:2538-2559` | Whether the response streamed. | Selects which Alloy measurement is comparable (rule `alloy.r003`). |
| `gateway.error.class` | EXISTING | `:2538-2559`, classes `src/retry.rs:23-173` | Typed gateway failure class (19 values). | `edge.gateway_error` event |
| `gateway.proxy.id` | EXISTING | `:2466` | Proxy id. | Attribute only |
| `http.route` | EXISTING, **different meaning** | `:3482-3516` | Holds the **proxy name**, not a path template. | Never compared with Alloy's `http.route`. |
| `http.response.status_code` | EXISTING | `:2433` | Final status. | `edge.response` event |
| Resource `telemetry.sdk.name = "ferrum-edge"`, scope `ferrum-edge` | EXISTING | `:2639-2659` | Producer identification. | Used by the OTLP importer, which marks provenance `unverified`. |
| Sampling: `root_sampling`, `root_sampling_ratio`; parent-based only for trusted callers | EXISTING | `:3088-3149`, `:3433-3442` | An untrusted caller's sampled flag has no effect. | Matches Alloy's policy. |
| CLIENT span per backend attempt: `gateway.backend.attempt`, `http.request.resend_count`, `gateway.backend.retry_reason`, `server.address`/`server.port`, `http.response.status_code` or `error.type` | EXISTING in v0.9.9 | v0.9.9 `attempt_spans.rs:679-751` (`otlp_attempt_span`) | One span per backend attempt, parent = the Edge SERVER span; the backend's `traceparent` names it. | `edge.backend.attempt` event: its span link, which links the service span under it to the gateway request (rules follow at most one attempt hop), with `gateway.backend.attempt` as the attempt scope and `gateway.backend.retry_reason` as the `retry_reason` attribute. `server.address`, `server.port`, `http.request.resend_count`, the status, and `error.type` are not interpreted. |
| Attempt CLIENT span start and end | EXISTING in v0.9.9 | `attempt_spans.rs:679-681`, `:744-745` (end = start + the attempt's monotonic duration) | From dispatch until the outcome is known: the response head when streamed, the complete response when buffered, except that a buffered attempt on the HTTP/3 frontend's bridge to an HTTP/1.1 or HTTP/2 backend usually ends at its response head (`docs/plugins.md` "Backend attempt spans"). Retry backoff falls between attempt spans. | `edge.backend.attempt.duration`, scoped to the attempt; `unavailable` when a timestamp is missing or the end precedes the start. A buffered attempt is compared with the service only when it reports `gateway.backend.connection.reused`, which the bridge's attempts never do. No backoff duration is derived. |
| `gateway.backend.connection.reused` | EXISTING in v0.9.9 (direct HTTP/2 and gRPC pools only) | `attempt_spans.rs:712-717`; set by `src/proxy/http2_pool.rs:1120`, `src/proxy/grpc_proxy.rs:1159` | `true` when the attempt rode a pooled connection it did not open, `false` when it set one up, including a failed setup. Omitted by the bundled HTTP/1.1 client, the HTTP/3 pool, the HBONE and mesh-mTLS pools, and an attempt that joined a connection another request was setting up. | `edge.backend.connection_reused` event (`reused` attribute), scoped to the attempt. `true` makes that attempt's setup `not_applicable`. Its presence marks a buffered attempt's end as the complete response. |
| `gateway.backend.connection.setup_ms` | EXISTING in v0.9.9 (direct HTTP/2 and gRPC pools only) | `attempt_spans.rs:719`, `:728-731` | Whole connection establishment by this attempt, DNS through the HTTP/2 handshake. | `edge.backend.connection.setup`, scoped to the attempt. Absent is unknown, never zero; diagnosis then lists `gateway connection setup timing` as missing unless the connection was reused. |
| `gateway.backend.connection.dns_ms` | EXISTING in v0.9.9 (when the pool timed the phase) | `attempt_spans.rs:720`, `:728-731` | DNS resolution for the connection the attempt set up. | `edge.backend.connection.dns`, scoped to the attempt. A component of setup, never added to it. |
| `gateway.backend.connection.tcp_connect_ms` | EXISTING in v0.9.9 (when the pool timed the phase) | `attempt_spans.rs:721-724`, `:728-731` | TCP connect for the connection the attempt set up. | `edge.backend.connection.tcp_connect`, scoped to the attempt. A component of setup, never added to it. |
| `gateway.backend.connection.tls_handshake_ms` | EXISTING in v0.9.9 (when the pool timed the phase) | `attempt_spans.rs:725-731` | TLS handshake for the connection the attempt set up. | `edge.backend.connection.tls_handshake`, scoped to the attempt. A component of setup, never added to it. |
| Semantic-convention version / `schema_url` | **UNAVAILABLE** | none | Names match the stable HTTP conventions. | — |
| Per-attempt duration, connection setup (DNS/TCP/TLS), connection reuse flag | **UNAVAILABLE** in v0.9.8; EXISTING in v0.9.9 on attempt CLIENT spans (rows above) | v0.9.8: `final_backend_dispatch_elapsed` exists internally but is not exported (`src/proxy/mod.rs:39993`). | — | For v0.9.8 input, reported as `missing_evidence` in diagnosis and never inferred. v0.9.9 input is interpreted per the rows above. |
| Retry backoff duration | **UNAVAILABLE** in both releases | v0.9.9 `docs/plugins.md` "Backend attempt spans": backoff falls between attempt spans | — | Deferred: Alloy derives no backoff from the gap between attempt spans, which can also hold target selection and gateway-local waits. |

## 5. Edge transaction logs

These fields exist in Edge access logs (`TransactionSummary`, `src/plugins/mod.rs:7893-8123`). Alloy reads them only when an operator supplies them in a diagnostic report; there is no automated collection.

| Field | Status | Meaning |
|---|---|---|
| `latency_total_ms`, `latency_backend_ttfb_ms`, `latency_backend_total_ms` | EXISTING | As in §4; `-1` means unknown. |
| `metadata.rejection_phase` | EXISTING | The phase that rejected before upstream: `authenticate`, `authorize`, `before_proxy`, `on_request_received`, `circuit_breaker_open`, … Maps to Alloy's `edge.request.rejected` (`phase`). Plugin identity is not recorded. |
| `error_class`, `body_error_class` | EXISTING | Typed classes (`src/retry.rs:151-173`) |
| Warning log `"Retrying backend request"` with `attempt` | EXISTING | v0.9.9 `src/proxy/mod.rs:40744` (v0.9.8 `:39282`). Log only; not in transaction summaries. v0.9.9 also records each attempt on its CLIENT span (§4). |

## 6. Endpoints

| Endpoint | Status | Notes |
|---|---|---|
| Edge active health check `GET {http_path}` (default `/health`, healthy `[200, 302]`) | EXISTING | `src/config/types.rs:1463-1526`, `src/health_check.rs:85-92`, `:3242-3277`. Alloy's export sets `http_path` to the manifest's `health.path` (typically `/readyz`) and `healthy_status_codes: [200]`. |
| Edge admin `POST/PUT/GET/DELETE /api-specs` | EXISTING | `src/admin/mod.rs:3673-3716`, `docs/api_specs.md`. Not available in file mode. **Alloy never calls it.** `ferrum-alloy openapi export` produces the artifact that operators or Nexus publish. CI's `edge-config` job submits the exported agent-tool fixture to it (database mode, SQLite) to prove Edge accepts what the export writes (§7). |
| `x-ferrum-mcp` on `POST /api-specs`: OpenAPI operations as MCP tools through a generated `mcp_gateway` | EXISTING since v0.9.9, retained in v0.9.15/v0.9.14 (Edge #5930); ignored by legacy v0.9.8 | v0.9.9 `docs/api_specs.md` ("`x-ferrum-mcp` (optional)"), `docs/plugins.md` ("OpenAPI bridge"); `src/admin/api_specs/extractor.rs` (`X_FERRUM_MCP_KEYS`, `X_FERRUM_MCP_OPERATION_KEYS`, `mcp_operation_selected`, `extract_mcp_bridge_operations`, `auto_inject_mcp_gateway`), `src/plugins/mcp_openapi_bridge.rs` (`ANNOTATION_KEYS`, `default_annotations`, `MAX_BRIDGE_OPERATIONS` = 256). Alloy writes it (`ferrum-alloy openapi export`) and checks it against these rules (`ferrum_alloy_edge::agents`); see [agent-tools.md](agent-tools.md). |
| G01 authenticated diagnostic lookup: `GET /diagnostics/v1/refs/{ref}` on the admin listener, `diagnostics:read` scope plus an `ns` claim | EXISTING in v0.9.9 (Edge #5767, #5845, #5846, #5868); UNAVAILABLE in v0.9.8 | Released v0.9.9 `src/admin/mod.rs:2796`, `docs/admin_api.md` “Diagnostic References”, `openapi.yaml` `DiagnosticRefLookup`, and `src/diagnostic_ref.rs`. Schema and ten selected fixtures vendored byte-exact from `contracts-edge-0.9.15`; schema provenance is refreshed, with wire constraints and fixture payloads unchanged from r2. Alloy calls it only with explicit `--edge-admin-url`, environment credential, and a separate trusted `--edge-observation`; binding and the limited confirmation path are defined in ADR 0009. `200` may have null detail; `401`/`403` refuse credentials/scope/namespace; `404` collapses malformed/unknown/expired/evicted/wrong-namespace/wrong-replica/disabled references (an owner hint is not an automatic routing instruction); `429` rate limits. No redirects or automatic retries, verified HTTPS or direct literal-loopback HTTP, no environment proxies, 2s connect/5s whole-request and 64 KiB body bounds. Only a bound authenticated record with known vocabulary can confirm its own recorded facts; service reports and timing findings remain unverified. |
| Alloy `/livez`, `/readyz` (application and management listeners) | EXISTING (Alloy) | Status only, `no-store` |
| Alloy management `/health`, `/metrics`, `/openapi.json` | EXISTING (Alloy) | Configured bearer token always required; tokenless detailed routes deny; loopback bind by default |

## 7. Configuration schema Alloy generates

The unchanged resource shape of `ferrum-alloy edge export`, statically rechecked against v0.9.15/v0.9.14 above, originated with fields that exist in the `deny_unknown_fields` resources of both Edge v0.9.9 (`src/config/types.rs`: `Proxy` 2776, `Upstream` 1910, `PluginConfig` 3247, `GatewayConfig` 3398) and v0.9.8 (`Proxy` 2646, `Upstream` 1843, `PluginConfig` 3101, `GatewayConfig` 3252), and GitForgeOps's `kind`/`spec` wrapper (`src/config/strict.rs:321-332`). v0.9.9 adds two `Proxy` fields that export deliberately leaves at their defaults (re-audit item 6): `allow_path_parameters` (`false`) and `websocket_permessage_deflate` (`strip`). Manifest paths cannot contain `;`, `%`, `\`, or `.`/`..` segments, so no generated `listen_path` needs the opt-in.

Edge v0.9.15 and v0.9.14 validate resource IDs in `src/config/types.rs`: IDs are 1–254 bytes, start with an ASCII alphanumeric, and then contain only ASCII alphanumerics, `.`, `_`, or `-`. Manifest validation applies that limit to each generated ID selected by the manifest: `-upstream` for health checks, `-correlation-id` for the correlation plugin, and `-otel-tracing` for OTLP tracing.

| Generated item | Status | Validation |
|---|---|---|
| Proxy: `listen_path`, `backend_scheme` (never `backend_protocol`), `strip_listen_path`, `backend_path`, `backend_*_timeout_ms`, `upstream_id`, `plugins`, `labels`, `backend_tls_*` | EXISTING | CI: `ferrum-edge validate -m file` is required on v0.9.15 and v0.9.14 (new adoption pairing pending; historical v0.9.10/v0.9.9 passed) for `contracts/fixtures/manifests/plain-http.edge.yaml` and for the e2e TLS config. The e2e stack serves traffic with it. |
| Upstream with `health_checks.active` and backend TLS on the upstream | EXISTING | same |
| `correlation_id` plugin config (`header_name`, `echo_downstream`) | EXISTING | same |
| `otel_tracing` plugin config (`endpoint`, `service_name`, `trace_context_trust: untrusted`, `include_url_path: false`, optional `root_sampling`/`root_sampling_ratio`) | EXISTING | same; keys checked against `ALLOWED_CONFIG_KEYS` (`otel_tracing.rs:63-81`) |
| End-to-end stack only (`gen-e2e-edge-config`, not `edge export`): proxy `backend_host`, `backend_port`, and `retry` (`max_retries`, `retryable_status_codes`, `retryable_methods`, `retry_on_connect_failure`); Edge environment `FERRUM_POOL_WARMUP_ENABLED`, `FERRUM_POOL_HTTP2_CONNECTIONS_PER_HOST`, and `FERRUM_DIAGNOSTIC_REFS=errors` (supported by v0.9.15 and v0.9.14) | EXISTING | `ferrum-edge validate` and real traffic in the `edge-e2e` job on v0.9.15 and v0.9.14; fresh adoption qualification pending |
| Service manifest `ferrum.service_manifest` v1 | **EXISTING**/implemented shared v1 | Alloy-defined; Foundry #540 and Nexus #519 have merged, qualified authenticated previews as of 2026-10-04. GitForgeOps qualifies Alloy-generated resource trees, not a manifest JSON preview. Root accepted the unchanged freeze at qualified owner 81cbb, published in `contracts-edge-0.9.11`; the [adoption candidate](shared-contract-qualification.md) requires fresh hosted checks; see the [consumer evidence ledger](implementation-status.md#cross-repository-dependencies). |
| Service manifest `[agents]` (`enabled`, `endpoint_path`, `namespace`), read by `openapi export` | Included in shared schema as of ferrum-contracts #8 (`contracts-edge-0.9.9-r2`) | Alloy's own parser accepts manifests with or without it. Alloy does not vendor the service-manifest schema (§9). |
| OpenAPI document-level `x-ferrum-mcp` (`enabled`, `endpoint.path`, `namespace`, `include.operations`) and per-operation `x-ferrum-mcp` (`expose`, `name`, `title`, `description`, `annotations` with `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) | EXISTING since v0.9.9, retained in v0.9.15/v0.9.14 | CI: `edge-config` submits `contracts/fixtures/openapi/orders-api.openapi.json` to the real `POST /api-specs` on both supported releases. Each generates an `mcp_gateway` with exactly the declared tools, endpoint, and namespace. Legacy v0.9.8 accepted the document and ignored the extension. |

## 8. Alloy-owned telemetry (produced by this repository)

| Item | Status | Meaning |
|---|---|---|
| Alloy SERVER span, scope `ferrum-alloy-telemetry` | EXISTING | Parent = the accepted remote parent (behind Edge, the Edge SERVER span on v0.9.8 and the Edge attempt CLIENT span on v0.9.9), otherwise a new root. |
| `alloy.trace.parent` | EXISTING | `accepted_remote`, `root`, `rerooted_untrusted`, `rerooted_invalid`, or `ignored_by_policy` |
| `alloy.peer.trust` | EXISTING | `verified_identity`, `network_boundary`, or `untrusted` |
| `alloy.request_id` | EXISTING | Validated request id |
| `alloy.server.time_to_headers_ms`, `alloy.server.body_duration_ms`, `alloy.server.duration_ms` | EXISTING | See [measurement-semantics.md](measurement-semantics.md) |
| `alloy.response.body.outcome`, `alloy.response.body.bytes`, `alloy.response.upgraded` | EXISTING | Body finalization |
| `alloy.admission.wait_ms` | EXISTING | Admission wait when enabled |
| Operation spans with `alloy.operation.duration_ms`, `alloy.operation.kind`, `alloy.db.pool_wait_ms` | EXISTING | Explicitly instrumented operations |
| Diagnostic report `ferrum.diagnostic_report` v1 | EXISTING/implemented shared v1; vendored and pinned from published `contracts-edge-0.9.15` with owner-unreleased availability | Alloy's Finding remains a superset of Anvil's `DiagnosticFinding`. Alloy validates the pinned fixtures; Anvil #312 adds a merged, qualified, bounded read-only importer with canonical fixtures and an actual hosted Alloy golden. Reported provenance/confidence remain unverified claims. Root accepted the unchanged v1 freeze at qualified owner 81cbb; new adoption qualification remains pending; see the [ledger](implementation-status.md#cross-repository-dependencies). |

## 9. Cross-repository dependencies

Released producer contracts and qualified consumer slices are distinguished below. Root accepted the unchanged shared v1 freeze separately from the immutable consumer qualifications; immutable heads, hosted runs and remaining steps are in the [implementation ledger](implementation-status.md#cross-repository-dependencies).

1. **Edge**: per-attempt timing, connection-setup and reuse evidence, and the G01 authenticated diagnostic reference. They supply per-attempt observations and the bound G01 record; timing attribution remains unverified. v0.9.9 exports per-attempt CLIENT spans with attempt numbers and pool connection timings, and the G01 reference (off by default); v0.9.8 has none of them. Alloy interprets the attempt spans' durations and connection fields as defined in `measurement-semantics.md` and the catalog; they are unverified evidence, and Edge exports no retry backoff duration. It records a G01 reference a client observed (§3) and, with explicit client observation and admin access, resolves it under ADR 0009. Confirmation applies only to the bound record's facts; no timing attribution is promoted.
2. **Edge**: stripping every client-supplied `x-consumer-*` header, not only the two exact names. Done in v0.9.9 (Edge #5880) and unchanged through v0.9.12; still open in unsupported v0.9.8.
3. **Anvil**: #312 is merged and qualified for read-only diagnostic preview, including `gateway_telemetry` / `service_telemetry` claims, strict native bounds, redaction and exact golden timestamp text. Import does not authenticate evidence or run Anvil diagnosis; assessment remains unverified/unknown.
4. **Foundry / Nexus / GitForgeOps**: Foundry #540's authenticated manifest preview and presentation-boundary ADR are merged and qualified; diagnostic presentation remains future work. Nexus #519's authenticated, redacted manifest preview and unreleased subset work are merged and qualified at [`77fdb767`](https://github.com/ferrum-edge/ferrum-nexus/tree/77fdb767ec8ef04e88f13df9fb291bc77fbd0344); all 11 final-head checks and both PR workflows passed, and #446 is closed. GitForgeOps #461 validates two real CLI-generated resource trees through its consumer loader, assembler and released gateway validator; that is resource consumption, not direct manifest JSON adoption, production apply or human release acceptance. Alloy #27 stays open for fresh coordinated pin adoption qualification and root disposition; owner 81cbb qualification, root's unchanged-wire freeze and canonical publication are complete in the [owner/adoption record](shared-contract-qualification.md).
   **ferrum-contracts**: the shared `service-manifest` v1 schema (EXISTING/implemented, not vendored here) has a closed top level and retains the optional `[agents]` section introduced in #8/r2. The [published adoption record](https://github.com/ferrum-edge/ferrum-contracts/blob/390edbd5b2485af0988e02f7827fde778d76ae0a/docs/adoption.md) preserves four qualified consumer slices and qualified owner 81cbb. Current status is canonical `x-contract`; copied historical PROPOSED report descriptions and prepared/pending source wording remain unchanged. This owner pin adoption and coordinated consumer pins need their own hosted qualification.
   **Nexus**: publishes OpenAPI documents through `POST /api-specs`, adding `x-ferrum-proxy` and a root server base. Its `routes` enforcement also adds `x-ferrum-validate`, which Edge v0.9.9 refuses to combine with `x-ferrum-mcp`. Publishing an exported document with agent tools through Nexus therefore uses its docs-only mode. [#519](https://github.com/ferrum-edge/ferrum-nexus/pull/519) merged MCP subset support and closed [#446](https://github.com/ferrum-edge/ferrum-nexus/issues/446) on 2026-10-04; manifest preview alone does not publish tools.
5. **Website**: an Alloy page exists at [immutable website source](https://github.com/ferrum-edge/ferrumedge/blob/51f3709f1d3f754519dcc0ce5e018716f44dc272/alloy.html). It labels Alloy **Pre-release**, says it is not on crates.io, and describes source tested with Edge v0.9.10 and v0.9.9; this source pairing is not a published Alloy release. The inspected `0b796393b63e12a9fd643430446620dc23f140a8` website snapshot remains historical and did not mention Alloy.
