# Changelog

## [Unreleased]

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
