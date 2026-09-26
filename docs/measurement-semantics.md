# Measurement semantics

Every timing Alloy exposes, whether as a span attribute, metric, log field, `Server-Timing` value, or diagnostic observation, is defined here. The definitions give start, end, scope, unit, clock domain, availability, and what the measurement does **not** cover. `crates/ferrum-alloy-diagnostics/src/catalog.rs` mirrors this document. A measurement not listed here is not interpreted by diagnosis rules.

## Principles

1. **Local monotonic durations.** Durations come from `std::time::Instant` in the process that measured them. Wall-clock timestamps (span start/end) are for correlation and display. They are never subtracted across hosts, so Alloy never reports one-way network latency.
2. **Headers are not completion.** A service future returning a response only means response *headers* exist. Body completion is a separate, later event.
3. **Handed to Hyper is not delivered.** "Completed" means the body yielded its final frame to Hyper. It does not prove the remote client received the bytes.
4. **Only instrumented boundaries are named.** Alloy does not report "handler time", "serialization time", "database time", or "queue time" unless that exact boundary is instrumented.
5. **Nested and concurrent intervals are never summed.** A database call inside a handler is already inside the handler's elapsed time. Exclusive time is computed only from validated same-clock nested intervals. Otherwise Alloy reports the hierarchy and the enclosing durations.
6. **Unknown is not zero.** Missing values carry an availability state instead of `0`.

## Availability states

| State | Meaning |
|---|---|
| `measured` | A value exists. |
| `unavailable` | The producer supports the measurement but has no value for this request. For example, Edge exports `-1` for `backend_ttfb` on a rejected request. |
| `unsupported` | The producer version cannot measure it. For example, Edge v0.9.7 has no per-attempt timing. |
| `not_applicable` | The phase did not happen. For example, a reused connection has no TLS handshake. |
| `not_sampled` | The trace was not sampled. |
| `export_pending` | Recorded but not yet exported. |
| `dropped` | Lost before export (queue full, byte budget, export failure). It is also counted in `ferrum_alloy_telemetry_spans_lost_total`. |
| `unknown` | Nothing is known. |

A missing span is `unknown`. It is never proof that the service was not reached or that a packet was lost.

## Alloy service measurements

All are measured by `ferrum-alloy-telemetry`'s layer in the service process, with a monotonic clock, scoped to one HTTP request (one stream on HTTP/2) as seen by that process.

| Name | Start | End | Unit | Exposed as | Limitations |
|---|---|---|---|---|---|
| `alloy.server.time_to_headers` | The outermost Alloy telemetry layer is called (`alloy.middleware_entry`) | The inner service returns response headers to Hyper (`alloy.response_headers_produced`) | ms (span), s (metric) | `alloy.server.time_to_headers_ms`; `ferrum_alloy_server_time_to_headers_seconds`; `Server-Timing: alloy;dur=` (opt-in) | Excludes accept, TLS handshake, HTTP/2 stream setup, request-head parsing, and any kernel or Hyper queueing before the service was called. Includes everything inside the layer: admission wait, request-body reads performed before responding, handler work. |
| `alloy.server.body_duration` | Headers produced | Body finalized: final frame handed to Hyper, body error, or drop (`alloy.response_body_finalized`) | ms | `alloy.server.body_duration_ms` | Frames handed to Hyper, not bytes received by the client. Flow control and client read speed affect it. |
| `alloy.server.duration` | Middleware entry | Body finalized | ms / s | `alloy.server.duration_ms`; `http_server_request_duration_seconds` | Covers streaming. The OpenTelemetry span for the request ends at the same point, because the span is entered during body polls and stamped at finalization. |
| `alloy.admission.wait` | Admission layer entered | Permit granted or refused | ms | `alloy.admission.wait_ms` | Only when `server.max_in_flight_requests > 0`. The permit is released at response headers; streaming bodies do not hold it. |
| `alloy.response.body.bytes` | — | — | bytes | span attribute | Data frame bytes handed to Hyper (not on the wire, not compressed size). |

### Body outcomes

Every request finalizes **exactly once**, with one of these outcomes:

| Outcome | When |
|---|---|
| `completed` | The final frame was handed to Hyper (including trailers), or the body was empty. |
| `error` | The body stream returned an error. |
| `cancelled` | The body was dropped before its end (client disconnect, gateway abandon, reset, shutdown). |
| `not_sent` | The protocol forbids a body (HEAD, 1xx, 204, 304). Hyper discards it; this is not a cancellation. |
| `upgraded` | `101 Switching Protocols`. The upgraded (WebSocket) session is **not** instrumented. |
| `cancelled_before_headers` | The request future was dropped before any response existed. |
| `service_error` | The inner service returned an error instead of a response. |

### Explicitly instrumented operations

| Name | Start | End | Notes |
|---|---|---|---|
| `alloy.operation.duration` | First poll of the wrapped future | Its completion, or drop (`outcome=cancelled`) | Created with `ferrum_alloy_telemetry::operation::Operation`. A database operation's duration is **application-observed call time**: pool wait, network, driver, and server work. It is **not** database server execution time. |
| `alloy.db.pool_wait` | `pool.acquire()` called | Connection obtained or failed | Recorded by `ferrum_alloy::postgres::acquire` on the current operation span. Separate from the query. |
| Outbound HTTP (`http.client.request` span, `alloy.operation.duration_ms`) | Request sent | Response **headers** received | Reading the response body happens afterwards and is outside the span. |

Dropping a future cancels cooperative local work only. It cannot guarantee that a remote database statement or HTTP side effect was cancelled.

## Ferrum Edge measurements (v0.9.7)

These come from Edge's own spans and logs. Alloy imports them without reinterpreting them. See [edge-contract-inventory.md](edge-contract-inventory.md) for sources.

| Name | Start | End | Scope | Limitations |
|---|---|---|---|---|
| `edge.request.total` (`gateway.latency.total_ms`) | Edge handler entry | Transaction summary; body completion for streamed responses | Whole request, all attempts | — |
| `edge.backend.time_to_headers` (`gateway.latency.backend_ttfb_ms`) | First backend dispatch | Response headers available | **All attempts and retry backoff** | For **buffered** responses (`gateway.response.streamed = false`), it equals the full backend exchange including the body. `-1` means unknown, and Alloy records it as `unavailable`. |
| `edge.backend.total` (`gateway.latency.backend_total_ms`) | First backend dispatch | Body fully buffered | Buffered responses only | Omitted when streaming. |
| `edge.plugin_execution` | — | — | Cumulative plugin time | Not an interval; never subtracted from anything. |
| Connection acquisition / DNS / TCP / TLS setup | — | — | — | **Unsupported** in v0.9.7. |
| Per-attempt response-header wait | — | — | — | **Unsupported** in v0.9.7. |

HTTP/2 connection setup would be connection-scoped. If Edge ever exports it, Alloy must not charge it to each multiplexed stream; it should be linked as connection-level evidence. A pooled request has no setup phase and is `not_applicable`, not zero.

## Comparing gateway and service measurements

Diagnosis rule `alloy.r003` subtracts a service measurement from a gateway measurement only when all of these hold:

1. **Linkage.** The Alloy SERVER span's parent is the Edge SERVER span. This comes from explicit span ids, never timestamps.
2. **Single attempt reached the service.** Exactly one Alloy SERVER span is linked. With more than one, Alloy reports `alloy.gateway.multiple_service_attempts` and makes no comparison.
3. **Matching boundaries.**
   - Streamed responses: Edge `backend_ttfb` against Alloy `time_to_headers`.
   - Buffered responses: Edge `backend_ttfb` (which includes the body) against Alloy `duration`.
   - Unknown buffering mode: no comparison (`alloy.gateway.timings_not_comparable`).
4. **Both values measured.**

The result is an **unattributed residual**, never "network latency". It can include:

- connection setup;
- retry attempts and backoff that never reached the service;
- request transfer, TLS, and queueing before Alloy middleware;
- intermediaries;
- response header transfer (streamed) or body transfer and flow control (buffered).

Because Edge v0.9.7 records no attempt identity, a residual is at most `likely`.

A **negative** residual is not clamped to zero. It is reported as `conflicting_evidence` (`alloy.evidence.service_exceeds_gateway`), and the comparison is suppressed.

Default reporting thresholds are ≥ 50 ms and ≥ 20 % of the gateway measurement (`rules::Thresholds`).

## Dominance of an instrumented operation

Rule `alloy.r002` reports the single largest instrumented operation that descends from a service span when it takes ≥ 50 % of that span's time to headers, with a floor of 5 ms. It never adds operations together. Nesting is validated by the parent-span chain plus same-instance wall-clock intervals (1 ms slack). Without interval proof, the finding stays `likely` and records the missing evidence. An operation longer than an enclosing interval it claims to be inside is `conflicting_evidence`.

## Clock skew

Wall-clock intervals are compared only within one producer instance (`service.instance.id`). Durations are never derived from timestamps of different hosts. The diagnostics parser rejects reports whose intervals span more than 24 hours.

## Metric bucket bounds

`http_server_request_duration_seconds` and `ferrum_alloy_server_time_to_headers_seconds` use the OpenTelemetry recommended buckets: 5 ms, 10 ms, 25 ms, 50 ms, 75 ms, 100 ms, 250 ms, 500 ms, 750 ms, 1 s, 2.5 s, 5 s, 7.5 s, 10 s.

Labels are method (the nine standard methods or `_OTHER`), route template, and status code, with these fixed labels:

- `__unmatched__`: the router fallback ran.
- `__not_routed__`: a response was produced before routing.
- `0`: no response existed.
- `__overflow__`: the series cap (default 2000) was reached.
