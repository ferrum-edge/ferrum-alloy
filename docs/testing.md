# Testing

Hosted CI (`.github/workflows/ci.yml`) is the source of truth. This page covers the fuzz and property tests for untrusted input. The example-based tests are listed in [implementation-status.md](implementation-status.md).

## Property tests

The property tests use [`proptest`](https://crates.io/crates/proptest) (a dev-dependency only, with the `std` feature and no process forking). They run with the ordinary `cargo test` in the `test` job on Linux, macOS, and Windows.

| File | Invariant |
|---|---|
| `crates/ferrum-alloy-diagnostics/tests/properties.rs` | For mutated contract fixtures (verified-trust claims, forged collection verification, scaled and negative values, unavailable values, shifted intervals), offline input is always downgraded to unverified and never yields `confirmed`. Every finding lists `does_not_prove` and cites only observations that exist. Parsing, `analyze`, and `render_text` are deterministic. |
| `crates/ferrum-alloy-telemetry/tests/properties.rs` | `parse_traceparent` accepts and rejects exactly what Ferrum Edge v0.9.7's parser does (a transcription of Edge's parser, plus the cases from Edge's own parser tests). `RequestId::parse` matches Edge's `correlation_id` rule. `tracestate` normalization is stable. None of the header parsers panics on any string. |
| `crates/ferrum-alloy-telemetry/tests/properties.rs` | Body lifecycle: for any sequence of data frames, trailers, and errors, stopped after any number of polls and then dropped, and for `HEAD`, `204`, and `101` responses, the finalizer runs exactly once, nothing is left in flight, and the recorded outcome (`completed`, `error`, `cancelled`, `not_sent`, `upgraded`) is the one the sequence implies. |
| `crates/ferrum-alloy/tests/config_properties.rs` | A value in a wrong-typed field, an invalid address, an unknown variant, an unknown quoted key, a TOML syntax error, or an invalid `FERRUM_ALLOY_*` variable never appears in the error's `Display` or `Debug` output. A secret set through the environment never appears in `redacted_toml()` or `Debug`. |

A failing case is shrunk and printed by `proptest`. Keep it as an example-based regression test next to the related tests.

## Fuzz targets

`fuzz/` is a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) crate. It is its own workspace and is excluded from the main one, so workspace builds, the MSRV check, and `cargo deny` never see it. It needs a nightly toolchain.

| Target | Parser | Seed corpus | Checks beyond "does not crash" |
|---|---|---|---|
| `traceparent` | `parse_traceparent`, `validate_tracestate` | `fuzz/seeds/traceparent` (cases from Edge's parser tests) | Accepted values round-trip through `to_header_value`; normalized `tracestate` is stable |
| `request_id` | `RequestId::parse` | `fuzz/seeds/request_id` | Accepted ids are kept verbatim and within Edge's alphabet and length |
| `diagnostic_report` | `parse_offline`, then `analyze` and `render_text` | `contracts/fixtures/reports` | No `confirmed` finding, `does_not_prove` is never empty, rules and rendering are deterministic |
| `otlp_import` | `otlp::import`, `otlp::trace_ids` | `contracts/fixtures/otlp` | As for `diagnostic_report` |
| `config_file` | `config::load_from` | `contracts/fixtures/manifests`, `fuzz/seeds/config_file` | Errors never repeat the canary value the seeds place in keys and values |

### CI smoke run

The `fuzz-smoke` CI job builds every target with a pinned nightly and runs each for 60 seconds, starting from the seed corpus. The fuzz crate has no committed lockfile: the job copies the workspace `Cargo.lock` into `fuzz/` first, so shared dependencies keep the versions `cargo deny` audits. If a target fails, the job uploads the crashing inputs as the `fuzz-artifacts` artifact.

### Longer runs

Longer runs are manual:

```bash
cargo install cargo-fuzz --locked
cp Cargo.lock fuzz/Cargo.lock
cargo +nightly fuzz run diagnostic_report fuzz/corpus/diagnostic_report contracts/fixtures/reports -- -max_total_time=3600
```

Use the seed directory from the table for other targets. `fuzz/corpus/` and `fuzz/artifacts/` are ignored by git.

### When a target finds a crash

1. Reproduce it with `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>`.
2. Minimize it with `cargo +nightly fuzz tmin <target> <crash-file>`.
3. Fix the root cause and add the minimized input as an example-based regression test in the crate's `tests/` directory, for example in `crates/ferrum-alloy-diagnostics/tests/bounds_and_hostile_input.rs`.
