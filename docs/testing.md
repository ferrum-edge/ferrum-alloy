# Testing

Hosted CI (`.github/workflows/ci.yml`) is the source of truth. This page covers the fuzz and property tests for untrusted input, and the browser smoke test of the OpenAPI documentation UI. The example-based tests are listed in [implementation-status.md](implementation-status.md).

## Property tests

The property tests use [`proptest`](https://crates.io/crates/proptest) (a dev-dependency only, with the `std` feature and no process forking). They run with the ordinary `cargo test` in the `test` job on Linux, macOS, and Windows.

CI sets `PROPTEST_RNG_SEED` in `.github/workflows/ci.yml`, so every run draws the same cases and a CI failure reproduces locally with the same seed. To explore other cases, run the tests locally with the variable unset (a random seed) or set to another number, for example `PROPTEST_RNG_SEED=7 cargo test -p ferrum-alloy --test config_properties`. Raise `PROPTEST_CASES` (default 256) for a longer run.

| File | Invariant |
|---|---|
| `crates/ferrum-alloy-diagnostics/tests/properties.rs` | For mutated contract fixtures (verified-trust claims, forged collection verification, scaled and negative values, unavailable values, shifted intervals), offline input is always downgraded to unverified and never yields `confirmed`. Every finding lists `does_not_prove` and cites only observations that exist. Parsing, `analyze`, and `render_text` are deterministic. |
| `crates/ferrum-alloy-diagnostics/tests/otlp_import.rs` | For generated OTLP/JSON traces (Edge and Alloy spans across several services, absent, negative, and measured durations, repeated span ids, skewed clocks) and for default and tightened report limits, every successful `otlp::import` serializes to a report that `parse_offline` accepts under the same limits, and anything else fails with `ReportRejected` or `ConflictingSpans`. The report names a service only when every service-scoped observation names the same one. Conversely, the same spans with distinct ids on one clock, well inside the default limits, always import. |
| `crates/ferrum-alloy-telemetry/tests/properties.rs` | `parse_traceparent` accepts and rejects exactly what Ferrum Edge v0.9.8's parser does (unchanged since v0.9.7) (a transcription of Edge's parser, plus the cases from Edge's own parser tests). `RequestId::parse` matches Edge's `correlation_id` rule. `tracestate` normalization is stable. None of the header parsers panics on any string. |
| `crates/ferrum-alloy-telemetry/tests/properties.rs` | Body lifecycle: for any sequence of data frames, trailers, and errors, stopped after any number of polls and then dropped, and for `HEAD`, `204`, and `101` responses, the finalizer runs exactly once, nothing is left in flight, and the recorded outcome (`completed`, `error`, `cancelled`, `not_sent`, `upgraded`) is the one the sequence implies. |
| `crates/ferrum-alloy/tests/config_properties.rs` | A value in a wrong-typed field, an invalid address, an unknown variant, an unknown quoted key, a TOML syntax error, or an invalid `FERRUM_ALLOY_*` variable never appears, even when it holds a newline, a backtick, or `, expected `, in the error's `Display` or `Debug` output. A secret set through the environment never appears in `redacted_toml()` or `Debug`. |

A failing case is shrunk and printed by `proptest`, which also records its seed in a `proptest-regressions/<test file>.txt` file next to the test. Commit that file with the fix: `proptest` replays the recorded cases first on every later run, whatever the seed. Also keep the shrunk input as an example-based regression test next to the related tests.

## Browser smoke test

The `openapi-ui-browser` CI job (Ubuntu) proves that the OpenAPI documentation UI (feature `openapi-ui`) works under its Content-Security-Policy in a real browser, which the header checks in `crates/ferrum-alloy/tests/openapi_ui.rs` cannot. It builds `examples/openapi-ui`, a small service whose `alloy.toml` serves the UI at `/docs` on the application listener (`127.0.0.1:18080`, with `openapi.public`) and on the management listener (`127.0.0.1:19090`, behind a throwaway token the job sets in `FERRUM_ALLOY_MANAGEMENT_TOKEN`), starts it, and waits for it to be ready. It then runs `ci/browser-smoke/`, a [Playwright](https://playwright.dev) project pinned to an exact version with a committed `package-lock.json` and installed with `npm ci --ignore-scripts`. The tests drive the runner's preinstalled Google Chrome (`channel: "chrome"`), so no browser is downloaded.

For each listener, `ci/browser-smoke/tests/openapi-ui.spec.ts`:

- loads `/docs` and waits until Swagger UI shows the document's title, both operations, and the schema section, with the stylesheet applied, and expands an operation;
- fails on any `securitypolicyviolation` event (recorded by a script registered before navigation), any console error or uncaught exception, any failed request, and any request to another origin (`data:` URIs are inline content);
- checks that the page and each asset it loaded carry exactly the policy defined in `crates/ferrum-alloy/src/openapi_ui.rs`;
- as a negative control, injects an inline `<script>` and requires the browser to block it and report one `script-src` violation, so a clean run cannot pass vacuously.

On the management listener it also checks that the page, a script asset and the OpenAPI document are refused with `401` without the token or with a wrong one, and served with it. The job uploads the screenshots, the Playwright report and failure traces, and the server log as the `openapi-ui-browser-smoke` artifact, on success and on failure.

To run it locally, start `cargo run -p example-openapi-ui` with `FERRUM_ALLOY_MANAGEMENT_TOKEN` set to at least 32 characters, then run `npm ci --ignore-scripts && npm test` in `ci/browser-smoke/` with the same variable. Google Chrome must be installed.

## Fuzz targets

`fuzz/` is a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) crate. It is its own workspace and is excluded from the main one, so workspace builds and the MSRV check never see it. It needs a nightly toolchain.

`fuzz/Cargo.lock` is committed. Dependencies shared with the workspace are at the same versions as in the workspace `Cargo.lock`; only `libfuzzer-sys`, `arbitrary`, and `jobserver` are added. The `dependencies` CI job runs `cargo deny --locked --manifest-path fuzz/Cargo.toml check` against it. When the workspace lockfile changes a shared dependency, update the fuzz lockfile to match: run `cargo update --manifest-path fuzz/Cargo.toml -p <crate> --precise <version>` for each changed crate, and review the diff.

| Target | Parser | Seed corpus | Checks beyond "does not crash" |
|---|---|---|---|
| `traceparent` | `parse_traceparent`, `validate_tracestate` | `fuzz/seeds/traceparent` (cases from Edge's parser tests) | Accepted values round-trip through `to_header_value`; normalized `tracestate` is stable |
| `request_id` | `RequestId::parse` | `fuzz/seeds/request_id` | Accepted ids are kept verbatim and within Edge's alphabet and length |
| `diagnostic_report` | `parse_offline`, then `analyze` and `render_text` | `contracts/fixtures/reports` | No `confirmed` finding, `does_not_prove` is never empty, rules and rendering are deterministic |
| `otlp_import` | `otlp::import`, `otlp::trace_ids` | `contracts/fixtures/otlp` | As for `diagnostic_report` |
| `config_file` | `config::load_from` | `contracts/fixtures/manifests`, `fuzz/seeds/config_file` | Errors never repeat the canary value the seeds place in keys and values. `fuzz/dicts/config_file.dict` supplies the text serde puts around a supplied value, such as `, expected ` and a newline |

### CI smoke run

The `fuzz-smoke` CI job builds every target with a pinned nightly and runs each for 60 seconds, starting from the seed corpus and with the target's dictionary from `fuzz/dicts/` when there is one. It first checks that `fuzz/Cargo.lock` is complete: if cargo would change it, the job fails and prints the difference. If a target fails, the job uploads the crashing inputs as the `fuzz-artifacts` artifact.

### Longer runs

Longer runs are manual:

```bash
cargo install cargo-fuzz --locked
cargo +nightly fuzz run diagnostic_report fuzz/corpus/diagnostic_report contracts/fixtures/reports -- -max_total_time=3600
cargo +nightly fuzz run config_file fuzz/corpus/config_file fuzz/seeds/config_file -- -dict=fuzz/dicts/config_file.dict -max_total_time=3600
```

Use the seed directory from the table for other targets. `fuzz/corpus/` and `fuzz/artifacts/` are ignored by git.

### When a target finds a crash

1. Reproduce it with `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>`.
2. Minimize it with `cargo +nightly fuzz tmin <target> <crash-file>`.
3. Fix the root cause and add the minimized input as an example-based regression test in the crate's `tests/` directory, for example in `crates/ferrum-alloy-diagnostics/tests/bounds_and_hostile_input.rs`.
