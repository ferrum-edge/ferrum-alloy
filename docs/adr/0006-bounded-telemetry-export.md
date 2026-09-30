# ADR 0006: Bounded span processor with exact loss accounting

**Status:** Accepted (2026-09-26)

## Context

The OpenTelemetry SDK's `BatchSpanProcessor` (0.33) bounds its queue, but its drop counter is private, and it has no byte budget. The OTLP exporter's implicit blocking HTTP client depends on how reqwest's TLS features unify across the application. Enabling Alloy's `http-client` or `jwt` feature made it panic at construction, a failure the workspace-wide all-features test found.

## Decision

- `BoundedSpanProcessor`:
  - a span-count queue and an estimated byte budget, where spans that do not fit are dropped and counted by reason (`queue_full`, `byte_budget`, `export_failed`, `shutdown`);
  - one dedicated export thread with tracing disabled and OpenTelemetry telemetry suppressed, so the exporter cannot instrument itself recursively;
  - bounded flush and shutdown, handled in queue order with spans and resource updates. A flush exports the spans queued before it. Nothing queued behind a flush or shutdown, such as another flush, a resource update, or a second shutdown, is discarded with its acknowledgement; a shutdown still handles at most one queue's worth of later messages, and callers after that get `AlreadyShutdown` at once.
- The OTLP/HTTP (protobuf) exporter is built on that thread with an explicitly configured blocking reqwest client: rustls, the `ring` provider, platform roots, no redirects. It uses a retry budget (`max_export_retries`) and a request size cap.
- Sampling is parent-based over a ratio. Only accepted remote parents keep their decision.
- Nothing is installed globally by library constructors. `AlloyApp` installs a subscriber only in `TelemetryInit::Auto` and only when none exists. If the application owns a subscriber and OTLP export is enabled without an OpenTelemetry layer, startup fails instead of silently losing traces.

## Consequences

- Loss is visible in `/metrics` as `ferrum_alloy_telemetry_spans_lost_total{reason}` and `ferrum_alloy_telemetry_spans_exported_total`.
- OpenTelemetry Rust's tracing API and SDK are documented upstream as beta. Versions are pinned and recorded in [compatibility.md](../compatibility.md).
