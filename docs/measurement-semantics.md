# Measurement semantics

Every timing Alloy exposes, whether as a span attribute, metric, log field, `Server-Timing` value, or diagnostic observation, is defined here. The definitions give start, end, scope, unit, clock domain, availability, and what the measurement does **not** cover. `crates/ferrum-alloy-diagnostics/src/catalog.rs` mirrors this document. A measurement not listed here is not interpreted by diagnosis rules.

## Principles

1. **Local monotonic durations.** Durations come from `std::time::Instant` in the process that measured them. Wall-clock timestamps (span start/end) are for correlation and display. They are never subtracted across hosts, so Alloy never reports one-way network latency.
2. **Headers are not completion.** A service future returning a response only means response *headers* exist. Body completion is a separate, later event.
3. **Handed to Hyper is not delivered.** "Completed" means the body yielded its final frame to Hyper. It does not prove the remote client received the bytes.
4. **Only instrumented boundaries are named.** Alloy does not report "handler time", "serialization time", "database time", or "queue time" unless that exact boundary is instrumented.
5. **Nested and concurrent intervals are never summed.** A database call inside a handler is already inside the handler's elapsed time. Exclusive time is computed only from validated same-clock nested intervals. Otherwise Alloy reports the hierarchy and the enclosing durations.
6. **Unknown is not zero.** Missing values carry an availability state instead of `0`.

## Connection timeout boundaries

Response body finalization, transport progress, and client receipt are separate
boundaries. Hyper can release a finished body while its final bytes still wait in
Hyper's write buffer. On HTTP/1.1, a blocked transport write keeps that connection
from being idle; the write stall timeout bounds the wait instead. On HTTP/2,
only response DATA writes count as progress; blocked control writes, including
PING and SETTINGS acknowledgements, do not defer idle closure after the response
body has ended.

A completed transport write means the transport accepted those bytes, which can
still be buffered in the kernel or on the client. Once the body has ended and
every write has completed, the connection can legitimately close by the idle
timeout while the client is still consuming buffered response data. An idle close
counter alone therefore does not prove truncation. Compare the body actually
received with its declared length, and distinguish time spent with a blocked
server write from time spent reading data the server has already written. These
boundaries do not add a client-delivery timing to Alloy's measurements.

## Availability states

| State | Meaning |
|---|---|
| `measured` | A value exists. |
| `unavailable` | The producer supports the measurement but has no value for this request. For example, Edge exports `-1` for `backend_ttfb` on a rejected request. |
| `unsupported` | The producer version cannot measure it. For example, legacy Edge v0.9.8 has no per-attempt timing. |
| `not_applicable` | The phase did not happen. For example, a reused connection has no TLS handshake. |
| `not_sampled` | The trace was not sampled. |
| `export_pending` | Recorded but not yet exported. |
| `dropped` | Lost before export (queue full, byte budget, export failure). It is also counted in `ferrum_alloy_telemetry_spans_lost_total`. |
| `unknown` | Nothing is known. |

A missing span is `unknown`. It is never proof that the service was not reached or that a packet was lost.

## Duration evidence

A diagnosis rule uses an observation as a duration only when all of these hold (`Observation::duration_ms`):

- its kind is `measurement` (not `event` or an unrecognized kind);
- its availability is `measured`;
- its unit is `us`, `ms`, or `s`;
- its value is finite and not negative, both as reported and after conversion to milliseconds. A negative zero counts as zero and never renders as `-0.0`.

Any other observation is kept in the report but never enters a subtraction, dominance comparison, or streaming comparison. A negative value of a measurement in a known unit (a duration, size, or count) is reported as `conflicting_evidence` (`alloy.evidence.negative_measurement`) and is never clamped to zero or used to derive another timing. A value in an unrecognized unit is not interpreted, so its sign is not judged.

## Span linkage

Span ids are unique only within one trace, so diagnosis identifies a span by its trace id and span id together. A parent span id names a span in the child's own trace. Two observations are linked only through explicit parent span ids within one trace. Supported Ferrum Edge v0.9.11 and v0.9.10 hand the service a CLIENT span per backend attempt as its parent, so a service span is linked to a gateway request when its parent is the Edge SERVER span or an Edge attempt span (`edge.backend.attempt`) whose parent is that SERVER span. Only one attempt hop is followed, both for linking service spans and for attributing degraded evidence. A service span whose attempt span was not exported (Edge drops spans when its export buffer is full) is not linked. Observations from different traces are never joined, even when their span ids match. An observation without a span is never linked to a span. The one exception is weaker than a link: when exactly one gateway request has no service telemetry, rule `alloy.r004` cites unsampled, dropped, or unexported evidence without a span, or from that request's trace, on that request as a possible explanation. Degraded evidence it cannot attribute, including evidence from another trace, is cited once on `alloy.telemetry.degraded_evidence_unlinked`.

## Alloy service measurements

All are measured by `ferrum-alloy-telemetry`'s layer in the service process, with a monotonic clock, scoped to one HTTP request (one stream on HTTP/2) as seen by that process.

| Name | Start | End | Unit | Exposed as | Limitations |
|---|---|---|---|---|---|
| `alloy.server.time_to_headers` | The outermost Alloy telemetry layer is called (`alloy.middleware_entry`) | The inner service returns response headers to Hyper (`alloy.response_headers_produced`) | ms (span), s (metric) | `alloy.server.time_to_headers_ms`; `ferrum_alloy_server_time_to_headers_seconds`; `Server-Timing: alloy;dur=` (opt-in) | Excludes accept, TLS handshake, HTTP/2 stream setup, request-head parsing, and any kernel or Hyper queueing before the service was called. Includes everything inside the layer: admission wait, request-body reads performed before responding, handler work. The request's OpenTelemetry span starts at middleware entry but ends at body finalization, so the span end is **not** the headers boundary. Diagnosis derives the header-phase interval as span start plus this duration (see [Dominance](#dominance-of-an-instrumented-operation)). |
| `alloy.server.body_duration` | Headers produced | Body finalized: final frame handed to Hyper, body error, or drop (`alloy.response_body_finalized`) | ms | `alloy.server.body_duration_ms` | Frames handed to Hyper, not bytes received by the client. Flow control and client read speed affect it. |
| `alloy.server.duration` | Middleware entry | Body finalized | ms / s | `alloy.server.duration_ms`; `http_server_request_duration_seconds` | Covers streaming. The OpenTelemetry span for the request ends at the same point, because the span is entered during body polls and stamped at finalization. |
| `alloy.admission.wait` | Admission layer entered | Permit granted or refused | ms | `alloy.admission.wait_ms` | Only when `server.max_in_flight_requests > 0`. The permit is released at response headers; streaming bodies do not hold it. |
| `alloy.response.body.bytes` | — | — | bytes | span attribute | Data-frame payload bytes handed to Hyper after the response's inner layers ran. Alloy's telemetry layer is outermost, so when compression is enabled its compression layer sits inside it and this counts the compressed frame payload. It excludes HTTP framing (chunked encoding, headers) and TLS overhead, and is not proof the client received it. |

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

## Ferrum Edge measurements (supported v0.9.11 and v0.9.10; legacy v0.9.8 input)

These come from Edge's own spans and logs. Alloy interprets only the documented attempt and timing fields. See [edge-contract-inventory.md](edge-contract-inventory.md) for sources.

| Name | Start | End | Scope | Limitations |
|---|---|---|---|---|
| `edge.request.total` (`gateway.latency.total_ms`) | Edge handler entry | Transaction summary; body completion for streamed responses | Whole request, all attempts | — |
| `edge.backend.time_to_headers` (`gateway.latency.backend_ttfb_ms`) | First backend dispatch | Response headers available | **All attempts and retry backoff** | For **buffered** responses (`gateway.response.streamed = false`), it equals the full backend exchange including the body. `-1` means unknown, and Alloy records it as `unavailable`. |
| `edge.backend.total` (`gateway.latency.backend_total_ms`) | First backend dispatch | Body fully buffered | Buffered responses only | Omitted when streaming. |
| `edge.plugin_execution` | — | — | Cumulative plugin time | Not an interval; never subtracted from anything. |
| `edge.backend.attempt.duration` (`CLIENT` span duration) | Attempt dispatch (for the first HTTP/1.1 or HTTP/2 attempt, the handoff of its prepared request) | The attempt's outcome: the response head when the response streams, the complete response when it is buffered, or the failure | One backend attempt | Derived from an Edge v0.9.9 attempt span's start and end timestamps. When either timestamp is missing or the end precedes the start, it is `unavailable`, never zero, and the exported timestamps are kept as `span.start_unix_nano` and `span.end_unix_nano`. Includes the attempt's own connection setup; does not include other attempts or the time between attempts. A buffered attempt on Edge's HTTP/3 bridge to an HTTP/1.1 or HTTP/2 backend usually ends at its response head instead (see below). Unsupported in v0.9.8. |
| `edge.backend.connection.setup` (`gateway.backend.connection.setup_ms`) | Connection setup starts | A new connection is established | Connection setup performed by one attempt | Exported only when Edge's direct HTTP/2 or gRPC pools observe a completed setup. An absent value is unknown; `gateway.backend.connection.reused = true` means setup is not applicable to that attempt. Unsupported in v0.9.8. |
| `edge.backend.connection.dns` (`gateway.backend.connection.dns_ms`), `.tcp_connect` (`gateway.backend.connection.tcp_connect_ms`), `.tls_handshake` (`gateway.backend.connection.tls_handshake_ms`) | Respective setup phase begins | Respective phase ends | Connection setup performed by one attempt | Optional component durations emitted by Edge's direct HTTP/2 and gRPC pools. Each phase is a component of setup and must not be added to the whole setup duration. Unsupported in v0.9.8. |
| `edge.backend.connection_reused` (`gateway.backend.connection.reused`) | Connection selected for attempt | — | One backend attempt | Event carrying `reused=true` or `false`; it is not a duration. Edge may omit it when the pool did not report reuse or setup. Unsupported in v0.9.8. |

The attempt span's full duration overlaps its connection setup and the backend SERVER span. Those intervals are nested, so they are never summed. Connection setup is attributed to the attempt that established the connection; a reused connection has no setup phase. Connection attributes come only from what an Edge pool measures: the direct HTTP/2 and gRPC pools report them, while the bundled HTTP/1.1 client, the HTTP/3 pool, the HBONE and mesh-mTLS pools, and an attempt that joined a connection another request was setting up omit them. Their absence is unknown, not zero.

**Buffered attempts on the HTTP/3 bridge.** Edge v0.9.9 documents one exception to the attempt's end boundary. On the HTTP/3 frontend's bridge to an HTTP/1.1 or HTTP/2 backend, a buffered attempt through Edge's bundled HTTP client usually ends at its response head, the point its retry is decided at, unless its body is collected inside the attempt so a failure while reading it can be retried; a mesh-tagged attempt ends with its complete response. Edge exports no frontend protocol, so Alloy cannot recognize that bridge directly. Only the direct HTTP/2 and gRPC pools report `gateway.backend.connection.reused`, and the bridge's attempts use neither, so Alloy treats a buffered attempt as ending with its complete response only when it carries that attribute. Rule `alloy.r003` does not compare any other buffered attempt with its service: a single linked service falls back to the gateway aggregate comparison, and with several linked services the attempt is listed as not compared.

**Retry backoff is not attributed.** The attempt's `gateway.backend.retry_reason` attribute identifies a retry. Edge v0.9.9 states only that retry backoff falls between attempt spans; it exports no backoff duration. The gap between sibling attempt span intervals is an elapsed inter-attempt interval that can include backoff, target selection, and gateway-local waits before the next dispatch, so Alloy neither reports it as a backoff duration nor subtracts it. Backoff attribution is deferred until Edge exports a backoff measurement.

## Gateway diagnostic reference (Ferrum Edge v0.9.9 and later)

Ferrum Edge v0.9.9 can stamp an opaque `X-Ferrum-Diagnostic-Ref` on the error responses the gateway authors. It is an identifier, not a timing: it has no start, end, unit, or clock domain, and it is never used in a duration, subtraction, or comparison. Alloy records it as one field, defined here and in `catalog.rs` (`EDGE_DIAGNOSTIC_REF_HEADER`, `EDGE_DIAGNOSTIC_REF_PATTERN`, `is_edge_diagnostic_ref`).

| Field | Recorded as | Producer | Grammar | Availability |
|---|---|---|---|---|
| Gateway diagnostic reference | A `client.response_header` event (kind `event`, leg `client_to_gateway`) with `header` = `X-Ferrum-Diagnostic-Ref` (matched case-insensitively) and `value` = the reference | Whoever observed the response: a client, a person, or the `edge-e2e` driver. Edge's `otel_tracing` puts it on no span, so the OTLP importer never records one. | `fd1_` and 32 lowercase hex digits, or `fd2_`, an 8-digit lowercase hex replica id, `_`, and 32 lowercase hex digits (`^(fd1_[0-9a-f]{32}\|fd2_[0-9a-f]{8}_[0-9a-f]{32})$`, from the pinned `gateway-headers.json`) | Only from Edge v0.9.9 and later with `FERRUM_DIAGNOSTIC_REFS=errors` (responses with a gateway `X-Gateway-Error` token) or `all` (also plugin rejections, gateway policy refusals, and routing `404`s). The default is `off`, and v0.9.8 never sends it. |

What it does not cover:

- **Absence is unknown.** A response without a reference is not evidence that the backend authored it: references may be off, the release may predate them, or a plugin may have replayed a stored response.
- **It embeds nothing.** The reference is 128 random bits (plus a random replica id for `fd2_`). It names no cause, route, backend, tenant, or time.
- **It is not authenticated.** Edge v0.9.9 removes any copy a backend or plugin sets, but the header names no Edge version and any server can send one. Rule `alloy.r007` therefore reports a well-formed reference as `alloy.edge.diagnostic_ref`, at most `likely`, and a malformed one as `alloy.edge.diagnostic_ref_malformed`, `unknown`, with no meaning inferred. Neither raises any other finding.
- **Resolution is explicit and separately bound.** `diagnose --edge-admin-url` with `--edge-observation` and `FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN` calls the released admin lookup. The separate trusted capture binds reference, replica, status, token including absence, protocol, namespace, and `created_at` inside the request window (at most 300 seconds; no implicit clock-skew expansion). `expires_at` cannot precede the window's end. RFC 3339 times are used only for binding, never cross-host duration subtraction. `detail.duration_bucket` remains a coarse label; it is never converted into a measured duration. `detail.backend_dispatch`, error classes and route-timeout phases cite the gateway's recorded view, not independent causal proof. Only the new record finding (`alloy.r008`, keys in `catalog::EDGE_RECORD_EVIDENCE_KEYS`) may be confirmed, through the private authenticated CLI path of [ADR 0009](adr/0009-authenticated-edge-diagnostic-lookup.md). Missing detail remains missing. Unknown vocabulary caps at likely or is refused; service reports, timing comparisons, and offline rereads gain no authority.

## Comparing gateway and service measurements

Diagnosis rule `alloy.r003` subtracts a service measurement from a gateway measurement only when all of these hold:

1. **Linkage.** The Alloy SERVER span's parent is the Edge SERVER span (v0.9.8), or an Edge attempt span whose parent is the Edge SERVER span (v0.9.9), in the same trace. This comes from explicit trace and span ids, never timestamps.
2. **Attempt linkage.** With Edge v0.9.9 attempt spans, each service SERVER span is matched to the CLIENT span it names as parent, and r003 compares that attempt's duration with the matching service measurement. This holds even when Edge's aggregate `backend_ttfb` is unknown (`-1`). With several linked services, `alloy.gateway.multiple_service_attempts` lists one residual per compared attempt and names each attempt it did not compare, and why. With Edge v0.9.8 input, attempt spans are absent: one linked service is compared with the gateway aggregate, while multiple linked services produce `alloy.gateway.multiple_service_attempts` with no invented per-attempt breakdown.
3. **Matching boundaries.**
   - A streamed attempt ends at the response head and is compared with Alloy `time_to_headers`.
   - A buffered attempt ends with the complete response and is compared with Alloy `duration`, but only when it carries the `gateway.backend.connection.reused` attribute (see [Buffered attempts on the HTTP/3 bridge](#ferrum-edge-measurements-v099-and-v098)). Otherwise a single linked service falls back to the aggregate comparison and lists the missing end boundary as missing evidence.
   - Without an attempt span, streamed responses use Edge `backend_ttfb` against Alloy `time_to_headers`.
   - Without an attempt span, buffered responses use Edge `backend_ttfb` (which includes the body) against Alloy `duration`.
   - Unknown buffering mode: no comparison (`alloy.gateway.timings_not_comparable`). With several attempts, each one is listed as not compared and the buffering mode as missing evidence.
4. **Both values are usable durations** (see [Duration evidence](#duration-evidence)). A matching service measurement without a usable duration, or an attempt span without a usable duration, yields `alloy.gateway.timings_not_comparable` and no difference.

The result is an **unattributed residual**, never "network latency". For a v0.9.9 attempt comparison it is local to that attempt and can include:

- connection setup;
- request transfer, TLS, and queueing before Alloy middleware;
- intermediaries;
- response header transfer (streamed) or body transfer and flow control (buffered).

Other attempts and retry backoff lie outside an attempt span, so they are not alternatives for an attempt comparison. For a gateway aggregate comparison, the residual may also include time spent in retry attempts or between attempts. Explicit setup and reuse evidence are cited on the attempt that emitted them. An attempt comparison lists `gateway connection setup timing` as missing evidence unless the attempt reported a measured setup or `reused = true`, because Edge omits connection attributes on several pools.

The residual depends on both measurements, so `confirmed` requires a verified collection path and verified provenance for both gateway and service measurements. Offline OTLP input remains at most `likely`. When v0.9.8 has no attempt identity, `missing_evidence` names it; a v0.9.9 attempt comparison cites the attempt number when emitted.

A **negative** residual, aggregate or per attempt, is not clamped to zero. It is reported as `conflicting_evidence` (`alloy.evidence.service_exceeds_gateway`), and the comparison is suppressed. With several attempts, that attempt is left out of the `alloy.gateway.multiple_service_attempts` comparison, which says so.

Default reporting thresholds are ≥ 50 ms and ≥ 20 % of the gateway measurement (`rules::Thresholds`).

## Dominance of an instrumented operation

Rule `alloy.r002` reports the single largest instrumented operation that descends from a service span, through parent span ids in the same trace, when it takes ≥ 50 % of that span's time to headers. Services whose time to headers is under 5 ms are skipped. It never adds operations together.

The comparison is limited to the **header phase**. Diagnostic imports give `alloy.server.time_to_headers` an interval that starts at the Alloy SERVER span's start (middleware entry) and ends at that start plus `alloy.server.time_to_headers_ms`. This adds a local duration to one timestamp of the same span; no two timestamps are subtracted. The interval never ends at the span's end, because the span stays open through the response body. If the duration is missing, or ends more than 1 ms after the span ends, the header-phase interval is unknown and none is made up.

Descendants of the service span (parent-span chain) are placed against that interval using same-instance wall-clock intervals only (`service.instance.id`, 1 ms slack):

| Placement | Treatment |
|---|---|
| Inside the header phase | Compared. The finding is `likely` from offline evidence (`confirmed` only from verified producers over a verified collection path). An operation that reports a longer duration than the time to headers enclosing it is `conflicting_evidence` (`alloy.evidence.operation_exceeds_enclosing`). |
| Starts at or after the end of the header phase (for example, work done while the body streams) | Not compared. It cannot explain time that had already passed before it started. |
| Starts before the end of the header phase but does not fit inside it | Not compared. Part of its duration belongs to the body, and the rule does not split durations. |
| Unknown: no header-phase interval, no operation interval, or different or unnamed instances | Compared only when no operation is placed inside the header phase. The finding is `unknown`, lists the missing interval evidence, and states in `does_not_prove` that the operation may have run during the response body. |

## Clock skew

Wall-clock intervals are compared only within one producer instance (`service.instance.id`). Durations are never derived from timestamps of different hosts. The diagnostics parser rejects reports whose intervals span more than 24 hours.

## Metric bucket bounds

`http_server_request_duration_seconds` and `ferrum_alloy_server_time_to_headers_seconds` use the OpenTelemetry recommended buckets: 5 ms, 10 ms, 25 ms, 50 ms, 75 ms, 100 ms, 250 ms, 500 ms, 750 ms, 1 s, 2.5 s, 5 s, 7.5 s, 10 s.

Labels are method (the nine standard methods or `_OTHER`), route template, and status code, with these fixed labels:

- `__unmatched__`: the router fallback ran.
- `__not_routed__`: a response was produced before routing.
- `0`: no response existed.
- `__overflow__`: the series cap (default 2000) was reached.
