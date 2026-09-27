# ferrum-alloy

Batteries-included [Axum](https://docs.rs/axum) application toolkit with optional Ferrum Edge integration. `AlloyApp` provides typed configuration, RFC 9457 problem responses, health and readiness, limits, graceful shutdown, and request telemetry, while handlers, extractors, routers, and Tower middleware stay ordinary Axum.

Every integration is an optional Cargo feature, and none is on by default: `otel`, `tls`, `edge`, `postgres`, `openapi`, `openapi-ui`, `jwt`, `http-client`, `compression`, `cors`, `diagnostics`, and `full` for all of them.

`openapi-ui` embeds Swagger UI, which is licensed under Apache-2.0; its license and notices are in [`assets/swagger-ui/`](assets/swagger-ui/).

See the [workspace README](https://github.com/ferrum-edge/ferrum-alloy/blob/main/README.md) and [configuration](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/configuration.md).

## Status

Pre-release. This crate is not published to any registry (`publish = false`); depend on it by git revision or path, as [getting started](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/getting-started.md) describes. [Release readiness](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/release.md) lists what must happen before any release.

## License

PolyForm Noncommercial 1.0.0 ([LICENSE](LICENSE)), with commercial licensing available ([LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)). "Ferrum Alloy" and the crate and command names are working names.
