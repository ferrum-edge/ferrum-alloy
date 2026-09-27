# ferrum-alloy-telemetry

Independently usable Tower/Axum request instrumentation for Ferrum Alloy: request ids, W3C trace context accepted only from trusted transport peers, route-template metrics, response-lifecycle accounting that ends when the response body ends, and an optional OpenTelemetry bridge with bounded OTLP export.

It does not depend on `ferrum-alloy`, so an existing Axum application can add only this layer. Library code never installs a global subscriber or provider; the `subscriber` feature adds helpers that the application calls explicitly.

See [measurement semantics](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/measurement-semantics.md) and [getting started](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/getting-started.md).

## Status

Pre-release. This crate is not published to any registry (`publish = false`); depend on it by git revision or path, as [getting started](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/getting-started.md) describes. [Release readiness](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/release.md) lists what must happen before any release.

## License

PolyForm Noncommercial 1.0.0 ([LICENSE](LICENSE)), with commercial licensing available ([LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)). "Ferrum Alloy" and the crate and command names are working names.
