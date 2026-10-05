# AI-agent tools

Ferrum Edge v0.9.12 and v0.9.11 can publish the operations of an OpenAPI document as MCP tools. When `POST /api-specs` receives a document carrying the `x-ferrum-mcp` extension, it generates a proxy-scoped `mcp_gateway`. That gateway runs each `tools/call` as an ordinary HTTP request to the proxy's own backend. Ferrum Alloy lets you declare, next to each handler, which operations agents may call, and `ferrum-alloy openapi export` writes those declarations into the exported document.

The contract is Edge's: `docs/api_specs.md` ("`x-ferrum-mcp` (optional)") and `docs/plugins.md` ("OpenAPI bridge (generated tools)"), implemented in `src/admin/api_specs/extractor.rs` and `src/plugins/mcp_openapi_bridge.rs`. The extractor and bridge are byte-identical between published v0.9.11 and v0.9.12 (`0d917701b63ef38210c49df830f48cf0457cbc7d`); v0.9.12 adds an admin deployment-recovery section to the API-spec documentation without changing the exported extension shape. The extension first shipped in v0.9.9; unsupported v0.9.8 ignores it, so a document is still accepted there but publishes no tools. The manifest's `[agents]` section belongs to the EXISTING/implemented shared `ferrum.service_manifest` v1 freeze published in `contracts-edge-0.9.11`; Alloy itself remains unreleased. New pin/Edge-pairing qualification awaits hosted CI.

## Safety guidance

An exposed operation lets an AI agent act for whichever user or consumer is calling the MCP endpoint. Choose what to expose accordingly.

- **Expose only what an agent should be able to do on a user's behalf.** Exposure is a product decision, made separately for each handler. Internal, administrative, and bulk operations should stay hidden.
- **Start with read-only operations.** A `GET` that returns the caller's own data is a good first tool. Add state-changing tools once you have seen how agents use the read-only ones.
- **State-changing and destructive operations need explicit opt-in.** When the selection comes from `[agents]` and `AgentTool`, the export never exposes an operation that nobody declared. A hand-written document-level `x-ferrum-mcp: true` (or an object without `include`) is different: Edge then publishes every `GET` operation, declared or not. The export accepts that, because it is valid for Edge, but prints a warning that names the undeclared operations. A `POST`, `PUT`, `PATCH`, or `DELETE` operation becomes a tool only through its own `AgentTool::expose()`, or by being named in `include` by `operationId`. Being selected by a tag alone is refused.
- **Keep annotations honest.** The export writes the method's default hints: `readOnlyHint` for `GET`, `destructiveHint` for `DELETE`, and `idempotentHint` for `PUT`. Agents may use these hints to decide whether to ask the user first, so override a default only to state more risk, never less. For example, a `POST` that deletes data should set `destructive(true)`. The export refuses `read_only(true)` on a `POST`, `PUT`, `PATCH`, or `DELETE` operation, and `destructive(false)` on a `DELETE`. The hints are hints: authorization stays with the gateway and the service.
- **Write the description for an agent.** Agents choose tools by their descriptions. An exposed operation needs one; the export refuses an operation without it.

### Policy scope

From Edge's contract: a bridged call reaches the backend as, say, `DELETE /shop/orders/7`. Everything the proxy decides from the request line sees the client's `POST` to the MCP endpoint instead:

- the `allowed_methods` 405 filter;
- plugin triggers on `match.method` or `match.path`;
- WAF path, method, and query rules;
- path-keyed authorization and rate limits.

A route-level rule that protects `DELETE /shop/orders/{id}` does **not** protect the tool bridged onto it.

Edge closes part of this gap itself. An operation whose method the proxy's `allowed_methods` does not allow is refused at import, and the gateway refuses such a call at runtime. `allowed_methods` must also allow `POST`, the method of the MCP endpoint. Beyond that, restrict bridged operations in the generated gateway, not in route-level policy. Leave them out of the selection, or embed an `mcp_gateway` in `x-ferrum-plugins` whose `policy` denies, hides, or group-gates tools by their public names (`orders.get_order`). The service must still authorize every request it receives: a bridged call carries the gateway's consumer identity like any other request, but it forwards none of the client's credentials (`Authorization` and `Cookie` are dropped).

## Declaring a tool

With the `openapi` feature, add `AgentTool` to an operation's `extensions`:

```rust
use ferrum_alloy::agents::AgentTool;

/// Fetch an order.
#[utoipa::path(
    get,
    path = "/orders/{id}",
    params(("id" = u64, Path, description = "Order identifier")),
    responses((status = 200, description = "The order", body = Order)),
    extensions(("x-ferrum-mcp" = json!(AgentTool::expose()
        .description("Fetch one of the caller's orders by its identifier"))))
)]
async fn get_order(Path(id): Path<u64>) -> Json<Order> { /* ... */ }
```

`json!(...)` is utoipa's attribute syntax and takes any expression that serializes. Code that builds operations without the macro can use `AgentTool::extensions()`.

| Builder | `x-ferrum-mcp` key | Default (Edge) |
|---|---|---|
| `AgentTool::expose()` / `AgentTool::hide()` | `expose: true` / `false` | not exposed |
| `.name(..)` | `name` (1–128 characters of `A-Za-z0-9_.-`) | the `operationId` (utoipa: the handler's function name) |
| `.title(..)` | `title` | the operation summary |
| `.description(..)` | `description` | the operation description, then its summary |
| `.read_only(..)`, `.destructive(..)`, `.idempotent(..)`, `.open_world(..)` | `annotations.readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint` | from the method, as above |

A tool's public name is the namespace, a dot, and its name (`orders.get_order`), as Edge's `policy.tools` keys and per-consumer grants address it.

## Publishing: `[agents]` in the service manifest

```toml
[agents]
enabled = true                 # default false
# endpoint_path = "/shop/mcp"  # default: Edge's {public_path}/mcp
# namespace = "orders"         # default: service.name
```

`ferrum-alloy openapi export --manifest ferrum-service.toml` then does the following:

1. It writes the document-level `x-ferrum-mcp` extension: `enabled`, the namespace, the endpoint when one is set, and an `include` that lists exactly the operations declared with `expose: true`. Without that `include`, Edge would publish every `GET` operation; with it, only declared handlers become tools. With `enabled = false`, the export writes `x-ferrum-mcp: false`. Without an `[agents]` section, the document-level extension is whatever the code wrote, or nothing. A document that already sets one while the manifest has `[agents]` is refused, so the extension has one source.
2. It adds the method's default annotations to each exposed operation, keeping any hint the code set, so the reviewed document shows what agents will see.
3. It checks the result (see below). On a problem it exits with code 3 and writes nothing. A selection that publishes undeclared `GET` operations is printed as a warning and does not fail. `--check` runs the same checks before comparing, so a problem is reported as invalid input (exit 3), not as drift (exit 4).

`endpoint_path` must be below `api.public_path`. The publisher (Nexus, an operator) supplies `x-ferrum-proxy` with `listen_path` set to `api.public_path`, as for any exported document, and replaces the root `servers` with `[{"url": "/"}]`. Edge builds each tool's path from the listen path, then the server URL, then the Paths key, so keeping the exported `servers` (the public path again) would double the prefix: `/shop/shop/orders`. Nexus and CI's `edge-config` step both replace it.

## What the export checks

The checks mirror Edge's admission of `x-ferrum-mcp`, plus the agent-safety rules above. With the document-level extension enabled, Edge's selection is applied, and every selected operation must:

- have an `operationId`;
- have a description (`x-ferrum-mcp.description`, the operation description, or its summary);
- take a JSON request body, if it takes one (`application/json` or `+json`). Edge refuses a required non-JSON body, and it silently drops an optional one, which an agent then cannot send;
- not be `HEAD`, `OPTIONS`, or `TRACE`, however it is selected (`expose: true`, `include` by `operationId`, or by tag);
- not set hints that state less risk than its method: `readOnlyHint: true` on a `POST`, `PUT`, `PATCH`, or `DELETE`, or `destructiveHint: false` on a `DELETE`;
- if it changes state, be selected explicitly (`expose: true`, or named by `operationId` in `include`), not by a tag alone;
- produce a valid tool name that no other operation produces.

Also:

- At least one operation, and at most 256, must be selected.
- `x-ferrum-mcp` cannot be combined with `x-ferrum-validate`, and the endpoint must not overlap a selected operation's path.
- Every `x-ferrum-mcp` value must use Edge's closed keys, with the right types. An annotation `title` is at most 8 KiB, as Edge allows. When the document-level extension is disabled, only its top-level closed keys, the `enabled` boolean type, and each operation's `x-ferrum-mcp` metadata are checked; the disabled document's other fields (`endpoint`, `namespace`, `include`, `exclude`, `limits`, `forward_request_headers`) are not. Those subtrees are validated once the extension is enabled. A typo in a top-level key therefore fails before the extension is turned on, while a wrong-typed `namespace` or `limits` value in a disabled document is caught only when it is enabled.

Edge checks more at import than the export does, including reserved header parameters, cookie parameters, parameter styles, and `allowed_methods`. CI submits an exported document to the real `POST /api-specs` on every supported Edge release: the `edge-config` job submits `contracts/fixtures/openapi/orders-api.openapi.json`, which includes an undeclared `GET` that must stay unpublished.

## Template

`ferrum-alloy new NAME --with openapi` exposes one read-only operation, `GET /items/{id}`, and enables `[agents]` in `ferrum-service.toml`. The generated test `only_get_item_is_offered_to_agents` keeps it that way.
