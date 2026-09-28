# Benchmarks

The harness in `examples/bench` measures the relative cost of Alloy's layers against a plain hyper server, across server stacks, workloads, and transports. It writes one machine-readable JSON line per run, so that regression budgets can later be computed from committed raw data.

The only results recorded so far are local, same-host measurements from one shared machine on one day (see [Results](#results-2026-09-26-previous-harness)). They are not capacity numbers. **No regression budget exists, and nothing is enforced in CI**; see [Regression budgets](#regression-budgets-not-yet-enforced).

## The matrix

A run measures one *cell*: one scenario, one workload, and one transport.

| Scenario | Server |
|---|---|
| `plain` | hyper-util's automatic HTTP/1.1 + HTTP/2 connection builder (what `axum::serve` uses) serving the same router, with `TCP_NODELAY` as Alloy sets it, and rustls for TLS. No Alloy. |
| `alloy` | `AlloyApp` defaults (Alloy's server loop, limits, admission, request id, route labels, metrics, telemetry layer); subscriber with no layers |
| `alloy-logs` | As `alloy`, with Alloy's JSON log layer at `info` writing to a sink. Every result on this page predates that layer and was measured with tracing-subscriber's JSON `fmt` layer instead (see *Record batching* and *Alloy JSON layer*). |
| `alloy-logs-fmt` | As `alloy-logs`, formatted by tracing-subscriber's JSON `fmt` layer with the same line layout. Added with the Alloy JSON layer and not yet measured. |
| `alloy-diagnostics` | Authorized diagnostic mode: as `alloy`, with a `DiagnosticsAuthorizer` installed and every request tagged with a tenant, so each request's evidence is retained in the bounded ring (default bounds, so steady-state runs evict on every request). The management router is built but not served, and retrieval itself is not measured. Added with diagnostic retrieval (#13) and not yet measured. |
| `otel-sampled` | OpenTelemetry bridge, sampling ratio 1.0, exporter that discards batches in process |
| `otel-unsampled` | OpenTelemetry bridge, sampling ratio 0.0 |
| `otel-unreachable` | Sampling ratio 1.0, OTLP/HTTP to a closed port (200 ms timeout, no retries) |
| `otel-collector` | Sampling ratio 1.0, OTLP/HTTP to a healthy collector: by default an in-process stub that answers every export with `200 OK` without decoding it, or a real Collector given with `--collector-endpoint` |

| Workload | Request and response |
|---|---|
| `small` | `GET /small`: a 60-byte JSON object |
| `large` | `GET /large`: 64 KiB with a known length |
| `stream` | `GET /stream`: 64 frames of 1 KiB with no known length (chunked on HTTP/1.1), so every frame passes through the body wrappers |
| `cancel` | `GET /stream-long` (4,096 frames of 1 KiB); the client drops the response after the first data frame. On HTTP/1.1 that closes the connection (with a reset, as an aborting client's would, instead of leaving it in `TIME_WAIT`), so every request pays for a new connection; on HTTP/2 it resets only the stream. |

| Transport | Protocol |
|---|---|
| `h1` | HTTP/1.1 over TCP, keep-alive |
| `h2c` | HTTP/2 with prior knowledge over TCP |
| `h1-tls`, `h2-tls` | HTTP/1.1 or HTTP/2 (by ALPN) over TLS |
| `h1-mtls`, `h2-mtls` | As above, with a client certificate the server requires and verifies |

TLS uses a throwaway CA, server certificate, and client certificate generated for every run.

Not in the matrix yet:

- **Behind Ferrum Edge.** `examples/edge-observability` runs Alloy behind Edge for correctness; the harness does not measure it.
- **A real Collector by default.** The stub measures OTLP encoding and export over HTTP, not a Collector's processing. Pass `--collector-endpoint` to export to a real one.

## Running it

Build in release mode, then either measure one cell or run a matrix:

```bash
cargo build --release -p example-bench

# One cell, one JSON line on stdout.
target/release/alloy-bench run --scenario alloy --workload small --transport h2c

# Every cell, 5 interleaved repetitions, appended to a file.
target/release/alloy-bench matrix --reps 5 --label "$(hostname) $(git rev-parse --short HEAD)" \
  --out examples/bench/results/$(date +%F)-<host>.jsonl

# A subset; --dry-run prints the order and a lower bound on the duration.
target/release/alloy-bench matrix --scenarios plain,alloy,otel-collector \
  --workloads small,stream --transports h1,h2c,h1-mtls --dry-run
```

Options for both commands:

| Option | Default | Meaning |
|---|---|---|
| `--seconds N` | 5 | Measurement window |
| `--warmup N` | 1 | Load before the window starts; not counted |
| `--concurrency N` | 32 | Requests in flight: one connection each on HTTP/1.1 |
| `--streams N` | 8 | Streams per HTTP/2 connection, so HTTP/2 uses `concurrency / streams` connections; `concurrency` must be a multiple |
| `--alloc-counting` | off | Count allocations; see below |
| `--label TEXT` | none | Recorded as `environment.label`; use it for the host and commit |
| `--run-id ID` | random | Recorded as `run_id`. `matrix` generates one (a version 4 UUID) and passes it to every run, so the lines of one matrix share it |
| `--collector-endpoint URL` | stub | OTLP/HTTP traces URL for `otel-collector` |

`matrix` takes `--scenarios`, `--workloads`, and `--transports` (each `all` or a comma-separated list, default `all`), `--reps` (default 5), and `--out` (default stdout). The full matrix has 8 × 4 × 6 = 192 cells, so 5 repetitions at the defaults take about 96 minutes.

How a run is made:

- **Process.** Server and client run in one process, on separate 4-worker Tokio runtimes, over loopback. `matrix` starts a fresh process for every run, because a process installs one global subscriber and a fresh heap keeps runs independent.
- **Ordering.** Every repetition runs every cell once, and each repetition rotates the order by one position, so no cell always runs first or last. Neighbors stay the same, so a slow cell affects the same next cell in every repetition.
- **Load.** The client is closed-loop: each worker sends its next request when the previous one finishes. Connections are opened before the warm-up, and a run fails if they cannot be.
- **Window.** Only requests that start and finish inside the measurement window count. Throughput is those requests divided by the window.

## Result format

Each run prints one JSON object on one line. The format is versioned by `schema` (currently `alloy-bench/1`); fields may be added within a version, but a field that changes meaning or is removed bumps it. Unknown values are `null`, never zero.

`alloy-bench/1` is not published yet: no committed result uses it, and nothing consumes it. Until results are committed under it, a scenario's meaning may still change within the version. In particular, `alloy-logs` now measures Alloy's `JsonLayer`; before the unpublished `alloy-bench/1` schema is published, this change in meaning does not require a schema bump. Use `alloy-logs-fmt` to measure tracing-subscriber's JSON formatter for comparison, and compare `alloy-logs` lines only when they come from the same commit.

| Field | Meaning |
|---|---|
| `schema`, `rep` | Format version; repetition index (set by `matrix`, else `null`) |
| `run_id` | Identifier of the invocation: every line of one `matrix` shares it, and a lone `run` gets its own |
| `commit` | `GITHUB_SHA` when it is set (GitHub Actions), else `null`; for local runs, put the commit in `--label` |
| `scenario`, `workload`, `transport` | The cell |
| `protocol`, `tls`, `mtls` | `http/1.1` or `h2`, and transport security |
| `concurrency`, `connections`, `streams_per_connection` | Offered load |
| `server_threads`, `client_threads` | Tokio worker threads of each runtime |
| `warmup_seconds`, `seconds` | Warm-up and measurement window |
| `alloc_counting` | Whether allocations were counted; see below |
| `requests`, `errors` | Requests completed in the window, and failed ones |
| `error_samples` | The messages of the first 5 client errors of the run (including failed reconnections), so a failing run says why; empty when `errors` is 0 |
| `connects` | Connections opened during the whole run, including warm-up and the initial ones |
| `body_bytes` | Response body bytes read in the window |
| `requests_per_second` | `requests / seconds` |
| `latency_us` | `p50`, `p90`, `p99`, `p999`, `max` of request latency in µs, from sending the request to reading the whole body (for `cancel`, the first data frame); nearest-rank, so `pN` is the value at 1-based rank ⌈N/100 × requests⌉ |
| `cpu` | On-CPU time in the window by thread role (`service_ns`, `client_ns`, `collector_ns`) and per request (`service_us_per_request`, `client_us_per_request`) |
| `memory` | `peak_rss_bytes` and `rss_bytes` of the whole process at the end of the run (`scope: "process"`) |
| `allocations` | With `--alloc-counting`: allocation `calls` and `bytes` in the window, in total and per request, for each role |
| `otel` | OpenTelemetry scenarios: `spans_exported` and `spans_lost` by reason in the window; for the stub collector, `collector_requests` and `collector_bytes` it received |
| `environment` | `label`, `os`, `arch`, `cpus`, `cpu_model`, `cpu_governor`, `kernel`, `load1_start`, `debug_build`, `bench_version`, `started_unix_seconds` |

What these measure, and what they do not:

- **Thread roles.** Client threads (`bench-client`) and collector threads (`bench-collector`) are named; every other thread is the service's, including the OpenTelemetry export thread and the main thread's idle time. `service_us_per_request` is therefore the cost of serving a request, exporting its span, and running the process around it.
- **CPU time** comes from `/proc/self/task/*/schedstat`, so it is Linux only; elsewhere `cpu` is `null`. It is summed per thread between the start and the end of the window. A thread that exits inside the window is missing, so the value is a lower bound.
- **Memory** comes from `/proc/self/status` (Linux only). It covers the whole process, client included, and the peak includes the warm-up. Compare it between cells, not as the service's footprint.
- **Allocations** are counted by a global allocator in the benchmark binary only, attributed to the role of the allocating thread. Counting adds shared atomic operations to every allocation, which lowers throughput, so it is off by default and **a counting run's throughput and latency must not be compared with a non-counting run's**. Measure allocations in a separate pass.
- **Span loss** is read from Alloy's own counters (`ferrum_alloy_telemetry_spans_lost_total`, `..._spans_exported_total`). Spans still queued when the window ends are in neither count. How `otel-unreachable` loses spans depends on the platform: where a connection to a closed loopback port is refused at once (Linux, macOS), exports fail and spans are lost as `export_failed`; where the connection attempt hangs instead (Windows can), exports may time out and spans may be lost by another reason, such as a full queue.

## Regression budgets (not yet enforced)

Budgets are the goal of #15, but they need a dedicated host first. Same-host numbers from a shared machine move with background load by more than the regressions a budget should catch. In the [discarded run](#discarded-run), `plain` fell from 178k to 77k requests/s between repetitions on unchanged code. Hosted CI runners are shared virtual machines with variable neighbors and CPU frequency, so a budget checked there would either fail at random or have to be too loose to mean anything.

The plan, which stays open in #15:

1. Run the full matrix on a dedicated or isolated Linux host, with pinned CPU frequency (the `performance` governor, which `environment.cpu_governor` records), at least 5 interleaved repetitions, and commit the raw JSON lines under `examples/bench/results/`.
2. From that baseline, derive each cell's budget as a ratio to `plain` in the same cell and repetition, not as absolute requests per second, with the noise floor measured on `plain` itself.
3. Add a scheduled job on that host that runs the matrix and fails when a ratio moves by more than the noise floor.

A budget computed from result lines must use only lines with `errors == 0`, because a run with errors measured something other than the cell (fast failures inflate throughput), and must group lines by `run_id` before pairing a cell with `plain`, so that ratios never mix repetitions from different invocations, hosts, or commits. Only compare lines with the same `alloc_counting`.

Until then, no number is quoted without its environment and repetition count, and the harness has no CI job. Its code compiles in the workspace build, and its unit tests and end-to-end tests (every scenario as a separate `alloy-bench run` process, and a one-cell `matrix` whose output is parsed) run in the `test` job with windows of 0.2 seconds. They check that the harness works, not how fast anything is.

## Results (2026-09-26, previous harness)

These results predate this matrix. They were measured with the previous version of the harness, in which `plain` was `axum::serve` without `TCP_NODELAY`, the client was hyper-util's pooled client, and each result line named its scenario in Rust case (`Plain`, `AlloyLogs`) inside an object that the external runner wrapped with `rep` and `load1`. The command was:

```bash
target/release/alloy-bench --scenario <scenario> --payload small --connections 32 --seconds 5 --warmup 1
```

- **Environment:**
  - Apple M4 (10 cores), 16 GiB, macOS 26.6.1;
  - Rust 1.98.1, release profile;
  - source is commit `232a9a7` plus the working-tree changes committed with this document.
- **Background load:** the machine was shared with other work. The 1-minute load average was 9 to 15 during the runs, and it is recorded with every result.
- **Ordering:** the six scenarios were interleaved within each of 5 repetitions, so background load affects them similarly.

HTTP/1.1, small payload, 32 connections. Each value is the median of 5 runs, and no run had errors.

| Scenario | Requests/s | Range | Relative to `plain` | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| `plain` | 177,114 | 175,565 – 178,403 | 1.00 | 176 µs | 336 µs |
| `alloy` | 132,845 | 130,135 – 133,736 | 0.75 (0.73 – 0.76) | 235 µs | 461 µs |
| `alloy-logs` | 60,563 | 58,588 – 61,690 | 0.34 (0.33 – 0.35) | 520 µs | 1,034 µs |
| `otel-sampled` | 112,029 | 110,707 – 113,943 | 0.63 (0.62 – 0.65) | 279 µs | 528 µs |
| `otel-unsampled` | 126,031 | 121,323 – 127,364 | 0.71 (0.69 – 0.72) | 250 µs | 480 µs |
| `otel-unreachable` | 113,013 | 110,049 – 114,030 | 0.64 (0.62 – 0.65) | 280 µs | 521 µs |

The "Relative to `plain`" range pairs each run with the `plain` run of the same repetition.

The raw results, one JSON line per run, are in `examples/bench/results/2026-09-26-macos-m4-interleaved.jsonl`.

## Reading the results

- On a handler that does no work, Alloy's default stack adds about 60 µs at p50 and costs about a quarter of peak throughput. A handler doing real I/O would dilute this, but this run did not measure one.
- **JSON access logging was the dominant cost**, more than the rest of the stack combined, even though its output goes to a sink. tracing-subscriber's JSON formatter re-parses and re-serializes all of a span's fields on every `Span::record` call, and the request span was recorded field by field. The batching change below addresses this.
- An **unreachable collector costs the same as a healthy (discarding) exporter**. Export failures stay off the request path. The previous harness did not record how many spans the bounded queue dropped; the current one reports it as `otel.spans_lost`.
- Unsampled OpenTelemetry still costs about 5% relative to `alloy`, for the bridge and the sampling decision.

## Record batching

The request span's fields are now recorded in one `record_all!` call per phase (request start, response headers, finalization) instead of one `record` call per field. Before the change a request made about 10 such calls with default settings; after it, 3 (plus none for disabled optional fields).

The results table above predates this change, and its `alloy-logs` row used tracing-subscriber's default JSON layout. The comparison below instead uses the layout of Alloy's own `init::fmt_layer`: flattened events, current span, no span list.

**Method:**

- The binary was built before and after the change from the same tree.
- Runs alternated before and after within each scenario, for 5 repetitions, with 32 connections, 5 s per run, HTTP/1.1, and the small payload.
- The 1-minute load average was **11 to 22** during these runs, higher than for the table above. Absolute numbers are therefore lower, and only the paired ratios are meaningful.

| Scenario | Before (median req/s) | After (median req/s) | After / before, median (range) | p50 before → after |
|---|---:|---:|---:|---:|
| `plain` (unchanged code: noise floor) | 129,996 | 125,127 | 0.95 (0.91 – 1.08) | 223 → 226 µs |
| `alloy` | 86,274 | 93,324 | 1.07 (0.85 – 1.32) | 334 → 311 µs |
| `alloy-logs` | 47,999 | 57,148 | **1.43 (1.15 – 1.55)** | 643 → 502 µs |
| `otel-sampled` | 61,720 | 68,467 | 1.10 (0.92 – 1.13) | 440 → 400 µs |

- **JSON logging:** throughput improved by more than the noise floor in every paired run.
- **Other scenarios:** the changes are within the noise measured on unchanged code, so the only supported claim is that they did not regress.
- **Raw data:** `examples/bench/results/2026-09-26-macos-m4-record-batching-ab.jsonl`.

## Alloy JSON layer

`init::fmt_layer` now formats `json` logs with Alloy's own layer, `ferrum_alloy_telemetry::json::JsonLayer`, instead of tracing-subscriber's JSON `fmt` layer. The line layout is unchanged; see [configuration](configuration.md#logging).

**Why it should be cheaper:** tracing-subscriber keeps a span's fields as one JSON string. Each `record` call parses that string into a map, adds the new values, and serializes the whole map again. Each event parses the current span's string once more to embed it. Alloy's layer keeps every span field as its rendered JSON value in a span extension, so recording renders only the new values and an event copies stored bytes. Each event is rendered once, into a reused per-thread buffer, and written with one `write_all`. For the request span this removes three parse-and-reserialize passes per request (one per `record_all!` phase) and one parse for every event logged inside it, including the access event.

**Expected effect:** a smaller gap between `alloy-logs` and `alloy`. This is an expectation from the removed work, **not a measurement**. No profile or benchmark was run for this change.

**How to measure it:** both formatters are in the same binary, so one build is enough. Interleave `alloy-logs-fmt` (before) with `alloy-logs` (after) within each repetition, and include `plain` as the noise floor, with the method used under *Record batching*. Profile both scenarios before quoting where the remaining time goes.

## Discarded run

An earlier sequential run went through all scenarios in order, 3 times, for 10 seconds each, then ran large-payload and h2c variants once. Its load average was around 17 on 10 cores. `plain` fell from 178k to 77k requests/s between the first and third repetitions, and single runs contradicted each other; for example, `otel-sampled` with the large payload appeared faster than `plain`.

That data is kept in `examples/bench/results/2026-09-26-macos-m4-discarded-high-load.jsonl` for transparency and must not be quoted. Large-payload and h2c results therefore have **no valid measurement** yet.

## Not measured

- The matrix above, on any host: large payloads, streaming, cancellation, h2c, TLS, mTLS, a healthy collector, CPU time, memory, and allocations have **no valid measurement** yet. The harness covers them; the runs wait for a dedicated host.
- Authorized diagnostic mode (`alloy-diagnostics`, #13): the scenario exists; no run has measured it.
- Behavior behind Ferrum Edge.
- Linux, or any dedicated benchmarking host.
- Long-duration stability and tail latency beyond p99.9.
