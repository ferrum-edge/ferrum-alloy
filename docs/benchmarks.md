# Benchmarks

These are local, same-host measurements from one machine on one day. They show the relative cost of Alloy's layers on a trivial handler. They are not capacity numbers, and nothing here is enforced as a regression budget.

## How they were run

```bash
cargo build --release -p example-bench
target/release/alloy-bench --scenario <scenario> --payload small --connections 32 --seconds 5 --warmup 1
```

- **Harness:** `examples/bench/src/main.rs`. Server and client run in one process on separate 4-worker Tokio runtimes, connected over loopback with HTTP/1.1 keep-alive. The client runs 32 concurrent request loops.
- **Handler:** `GET /small` returns a 60-byte JSON object. No I/O, so any per-request overhead is magnified relative to a real service.
- **Scenarios:**

| Scenario | Server |
|---|---|
| `plain` | `axum::serve` with the same router, no Alloy |
| `alloy` | `AlloyApp` defaults (Alloy's server loop, limits, admission, request id, route labels, metrics, telemetry layer); subscriber with no layers |
| `alloy-logs` | As `alloy`, with a JSON `fmt` subscriber at `info` writing to a sink |
| `otel-sampled` | OpenTelemetry bridge, sampling ratio 1.0, exporter that discards batches |
| `otel-unsampled` | OpenTelemetry bridge, sampling ratio 0.0 |
| `otel-unreachable` | Sampling ratio 1.0, OTLP/HTTP to a closed port (200 ms timeout, no retries) |

- **Environment:**
  - Apple M4 (10 cores), 16 GiB, macOS 26.6.1;
  - Rust 1.98.1, release profile;
  - source is commit `232a9a7` plus the working-tree changes committed with this document.
- **Background load:** the machine was shared with other work. The 1-minute load average was 9 to 15 during the runs, and it is recorded with every result.
- **Ordering:** the six scenarios were interleaved within each of 5 repetitions, so background load affects them similarly.

## Results

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
- **JSON access logging is the dominant cost**, more than the rest of the stack combined, even though its output goes to a sink. This is a performance finding worth investigating: span-field formatting on every request is the likely cause, but no profile was taken.
- An **unreachable collector costs the same as a healthy (discarding) exporter**. Export failures stay off the request path. The harness did not record how many spans the bounded queue dropped.
- Unsampled OpenTelemetry still costs about 5% relative to `alloy`, for the bridge and the sampling decision.

## Discarded run

An earlier sequential run went through all scenarios in order, 3 times, for 10 seconds each, then ran large-payload and h2c variants once. Its load average was around 17 on 10 cores. `plain` fell from 178k to 77k requests/s between the first and third repetitions, and single runs contradicted each other; for example, `otel-sampled` with the large payload appeared faster than `plain`.

That data is kept in `examples/bench/results/2026-09-26-macos-m4-discarded-high-load.jsonl` for transparency and must not be quoted. Large-payload and h2c results therefore have **no valid measurement** yet.

## Not measured

- CPU time per request, memory, and allocation counts.
- Export to a real, healthy Collector.
- Large payloads, h2c, TLS, and mTLS.
- Behavior behind Ferrum Edge.
- Linux, or any dedicated benchmarking host.
- Long-duration stability and tail latency beyond p99.
