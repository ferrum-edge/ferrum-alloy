# ADR 0004: Serve with hyper-util directly

**Status:** Accepted (2026-09-26)

## Context

`axum::serve` is convenient, but it exposes no request-head limits, header read timeout, connection cap, HTTP/2 settings, TLS, per-connection transport identity, or bounded drain with forced close. Alloy needs all of these for production defaults and for verified gateway identity.

## Decision

`AlloyParts::serve` runs its own accept loop on `hyper_util::server::conn::auto`:

- `max_headers`, `max_buf_size`, `header_read_timeout`, HTTP/2 `max_concurrent_streams`, header list size, and keep-alive;
- a semaphore connection limit, with excess connections closed immediately, and a per-connection first-request and idle deadline that counts requests in flight, so a peer cannot hold a slot with HTTP/2 keep-alive pings alone;
- a rustls acceptor with a handshake timeout; the verified identity becomes `PeerInfo`;
- shutdown: readiness reports draining, optionally for a grace period; accepting stops; each connection gets `graceful_shutdown` (HTTP/1.1 closes after the current response, HTTP/2 sends GOAWAY); the service waits up to the drain budget, force-closes, and flushes telemetry within its own budget;
- HTTP/2 stream tasks spawned through a tracking executor rather than `TokioExecutor`, so the drain waits for handlers and cancels those left at the budget;
- per-connection cancellation tokens rather than hyper-util's `GracefulShutdown`, whose watchers subscribe lazily and would miss connections that finish a TLS handshake after shutdown starts.

`AlloyParts::router` stays a normal `axum::Router`. Applications can serve it with anything, as shown in `into_parts_router_can_be_served_by_plain_axum`.

## Consequences

- WebSocket upgrades use `serve_connection_with_upgrades`. Upgraded sessions leave Hyper's accounting; this is documented and exposed through `Lifecycle::shutdown_token`.
- Service-side HTTP/3 is not supported.
