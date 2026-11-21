# ferrum-alloy-diagnostics

Versioned, language-neutral diagnostic evidence schema for Ferrum Alloy, with a bounded parser, OTLP/JSON import, and deterministic diagnosis rules. It depends only on serde and has no HTTP or async dependencies. Rules use no network and no AI, and every finding lists what it does not prove.

See [measurement semantics](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/measurement-semantics.md).

## Status

Released from Git only, starting with the tag `v0.1.0`. This crate is not published to any registry (`publish = false`); depend on it by git tag, revision, or path, as [getting started](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/getting-started.md) describes. [Release readiness](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/release.md) lists what every release must satisfy.

## License

PolyForm Noncommercial 1.0.0 ([LICENSE](LICENSE)), with commercial licensing available ([LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)). "Ferrum Alloy" and the crate and command names are working names.
