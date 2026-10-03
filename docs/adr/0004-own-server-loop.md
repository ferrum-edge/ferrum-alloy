# ADR 0004: Serve with hyper-util directly

**Status:** Accepted (2026-09-26)

## Context

`axum::serve` is convenient, but it exposes no request-head limits, header read timeout, connection cap, HTTP/2 settings, TLS, per-connection transport identity, or bounded drain with forced close. Alloy needs all of these for production defaults and for verified gateway identity.

## Decision

`AlloyParts::serve` runs its own accept loop on `hyper_util::server::conn::auto`:

- `max_headers`, `max_buf_size`, `header_read_timeout`, HTTP/2 `max_concurrent_streams`, header list size, and keep-alive;
- a semaphore connection limit, with excess connections closed immediately, and a per-connection first-request and idle deadline that counts requests in flight, response data written after a response body ends, and writes the transport cannot take yet, so a peer cannot hold a slot with HTTP/2 keep-alive pings alone;
- a per-connection write stall deadline: the transport records response data written (HTTP/2 `DATA` frames only) and whether a write is blocked, and response bodies record when they wait on the connection and, on HTTP/2, each chunk they hand to Hyper, wrapped so that it counts as held until Hyper takes it for writing or drops it (exact per stream, resets included), so a peer that takes no response data, by flow control or a zero TCP window, cannot hold a slot either, even while Hyper holds part of a chunk beyond the window and the body waits for the application;
- a rustls acceptor with a handshake timeout; the verified identity becomes `PeerInfo`;
- shutdown: readiness reports draining, optionally for a grace period; accepting stops; each connection gets `graceful_shutdown` (HTTP/1.1 closes after the current response, HTTP/2 sends GOAWAY); the service waits up to the drain budget, force-closes, and flushes telemetry within its own budget;
- the connection slot (semaphore permit, `active_connections` gauge, and the drain's count of open sockets) is owned by the transport, not the connection task, so it stays with the socket that Hyper hands over for an upgrade; at the drain budget every transport fails its reads and writes and wakes the tasks waiting on them, which closes upgraded sessions the application still reads or writes;
- HTTP/2 stream tasks spawned through a tracking executor rather than `TokioExecutor`, so the drain waits for handlers and cancels those left at the budget;
- per-connection cancellation tokens rather than hyper-util's `GracefulShutdown`, whose watchers subscribe lazily and would miss connections that finish a TLS handshake after shutdown starts.

`AlloyParts::router` stays a normal `axum::Router`. Applications can serve it with anything, as shown in `into_parts_router_can_be_served_by_plain_axum`.

## Consequences

- WebSocket upgrades use `serve_connection_with_upgrades`. Upgraded sessions leave Hyper's accounting but not Alloy's: they count against `max_connections` until the application drops them and are drained and force-closed with the other connections. Applications close them gracefully by watching `Lifecycle::shutdown_token`.
- Service-side HTTP/3 is not supported.
