# ferrum-alloy-cli

The `ferrum-alloy` command:

- `new`: starter projects that compile and pass their own tests. The project templates are compiled into the binary.
- `check`: configuration validation.
- `openapi export`: with drift detection, and the AI-agent tool metadata (`x-ferrum-mcp`) Ferrum Edge reads, checked against Edge's rules ([AI-agent tools](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/agent-tools.md)).
- `edge export`: Ferrum Edge file-mode YAML or a GitForgeOps tree.
- `diagnose`: deterministic explanations of evidence from files, or a service report fetched with `--url` (`FERRUM_ALLOY_DIAGNOSTICS_TOKEN` or `--token-file`). An explicit `--edge-admin-url` and separate trusted `--edge-observation` capture resolve a released Edge G01 record using only `FERRUM_ALLOY_EDGE_DIAGNOSTICS_TOKEN`; only its bound record finding may be confirmed (ADR 0009). Credentials never come from arguments.
- `version`: the tool version, the diagnostic-report and service-manifest schema versions, and the pinned Edge contract release and source commit (`--format human`, the default, or `json`).

See [getting started](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/getting-started.md).

## Status

Released from Git only, starting with the tag `v0.1.0`. This crate is not published to any registry (`publish = false`); install the `ferrum-alloy` command from Git with `cargo install --locked --git https://github.com/ferrum-edge/ferrum-alloy --tag v0.1.0 ferrum-alloy-cli`, as [getting started](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/getting-started.md) describes. [Release readiness](https://github.com/ferrum-edge/ferrum-alloy/blob/main/docs/release.md) lists what every release must satisfy.

## License

PolyForm Noncommercial 1.0.0 ([LICENSE](LICENSE)), with commercial licensing available ([LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)). "Ferrum Alloy" and the crate and command names are working names.
