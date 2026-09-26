# ADR 0003: Finalize request accounting at body completion, exactly once

**Status:** Accepted (2026-09-26)

## Context

A Tower service future resolving with a response proves only that headers exist. Server-sent events, downloads, and streamed gRPC keep running afterwards. Bodies can:

- end normally, including with trailers;
- be empty;
- error;
- be dropped when a client disconnects;
- never be polled at all, for HEAD, 204, and 304 responses.

`tower-http`'s trace layer reports its own events, but Alloy needs one authoritative finalization that metrics, spans, and diagnosis all share.

## Decision

- A single `Finalizer` owns each request's accounting. It moves from the response future into `InstrumentedBody` and finalizes exactly once, through an idempotent `finish` plus `Drop`.
- Outcomes: `completed`, `error`, `cancelled`, `not_sent` (protocol forbids a body), `upgraded` (101), `cancelled_before_headers`, and `service_error`.
- Bodies that are already at end-of-stream, or that the protocol forbids, are finalized when headers are produced. Hyper never polls those, so they would otherwise be misreported as cancelled on drop.
- The request span is entered only inside synchronous `poll` calls, never across `.await`, and is stamped at finalization. The exported OpenTelemetry span therefore ends when the body ends.
- Time to headers and body duration are separate measurements (see [measurement-semantics.md](../measurement-semantics.md)).

## Consequences

- In-flight gauges include streaming responses.
- Upgraded sessions are explicitly out of scope and are labeled `upgraded`.
- Tested in `crates/ferrum-alloy-telemetry/tests/lifecycle.rs` (every outcome, and exactly-once under concurrency), in `otel_export.rs` (span end after the body), and in the e2e stream check.
