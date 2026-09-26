# ADR 0002: Trust comes from verified transport identity, never headers

**Status:** Accepted (2026-09-26)

## Context

A service behind Ferrum Edge wants to honor the gateway's `traceparent`, request id, and authenticated consumer identity. Any client that reaches the service directly can send the same headers.

Edge v0.9.7 provides no signed context. It can present a client certificate, possibly an X.509-SVID, to backends (`backend_tls_client_cert_path`). A static `trusted = true` switch or forwarded headers would let any direct caller impersonate the gateway.

## Decision

- A `TrustClassifier` decides trust from request **extensions** set by the transport: `PeerInfo` (remote address plus verified TLS identity) or axum's `ConnectInfo`. The default classifier trusts nobody.
- `TrustedPeers` matches exact SPIFFE ids or `dns:` SANs from certificates rustls verified, or configured source networks (a trusted termination boundary). Networks that match everything are rejected.
- Trust gates propagation metadata (trace context, request ids, `Server-Timing`). Edge-asserted identity (`x-consumer-*`) additionally requires a **verified identity**, never network trust, plus explicit opt-in. Unverified copies are removed before handlers run.
- `gateway_required` rejects requests without a verified identity, except configured health paths.
- No ad hoc signatures. If signed context becomes necessary, it needs a written specification first: established algorithms, key rotation, audience, request and tenant binding, freshness, and replay handling.

## Consequences

- The first supported deployment is Edge with a client SVID and Alloy with rustls client verification. `examples/edge-observability` and the CI end-to-end job exercise exactly that.
- Sidecar TLS termination must use `trust.networks` and guarantee network isolation.
- Existing applications that terminate TLS themselves can populate `PeerInfo` using `TlsPeer::from_verified_leaf` (telemetry feature `x509`).
