# Architecture decision records

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-workspace-and-dependency-boundaries.md) | Workspace crates and acyclic dependency boundaries | Accepted |
| [0002](0002-transport-trust-and-gateway-identity.md) | Trust comes from verified transport identity, never headers | Accepted |
| [0003](0003-response-lifecycle-accounting.md) | Finalize request accounting at body completion, exactly once | Accepted |
| [0004](0004-own-server-loop.md) | Serve with hyper-util directly instead of `axum::serve` | Accepted |
| [0005](0005-diagnostic-evidence-model.md) | Versioned, language-neutral evidence with deterministic rules | Accepted |
| [0006](0006-bounded-telemetry-export.md) | Bounded span processor with exact loss accounting | Accepted |
| [0007](0007-real-gateway-integration-testing.md) | Prove Edge integration with a pinned real gateway, not mocks | Accepted |
| [0008](0008-tenant-scoped-diagnostic-retrieval.md) | Tenant-scoped diagnostic retrieval from a running service | Accepted |
| [0009](0009-authenticated-edge-diagnostic-lookup.md) | Authenticated Edge record bound to an explicit trusted client capture | Accepted |
