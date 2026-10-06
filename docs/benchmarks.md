# Benchmarks

The harness in `examples/bench` measures the relative cost of Alloy's layers against a plain hyper server, across server stacks, workloads, and transports. It writes one machine-readable JSON line per run, so that regression budgets can later be computed from committed raw data.

Recorded evidence includes historical shared-machine measurements from 2026-09-26 (see [Results](#results-2026-09-26-previous-harness)) and [bounded shared hosted preparation from 2026-10-04](#hosted-evidence-2026-10-04). Neither supplies capacity numbers or a dedicated baseline. **No performance regression budget exists.** The [hosted qualification workflow](#hosted-qualification-preparation) checks evidence completeness and collects experimental profiles and comparisons; it does not fulfill the dedicated-host acceptance criteria in [#15](https://github.com/ferrum-edge/ferrum-alloy/issues/15).

## The matrix

A run measures one *cell*: one scenario, one workload, and one transport.

| Scenario | Server |
|---|---|
| `plain` | hyper-util's automatic HTTP/1.1 + HTTP/2 connection builder (what `axum::serve` uses) serving the same router, with `TCP_NODELAY` as Alloy sets it, and rustls for TLS. No Alloy. |
| `alloy` | `AlloyApp` defaults (Alloy's server loop, limits, admission, request id, route labels, metrics, telemetry layer); subscriber with no layers |
| `alloy-logs` | As `alloy`, with Alloy's JSON log layer at `info` writing to a sink. The historical 2026-09-26 results predate that layer and used tracing-subscriber's JSON `fmt` layer instead; the hosted 2026-10-04 sample uses the typed layer. |
| `alloy-logs-fmt` | As `alloy-logs`, formatted by tracing-subscriber's JSON `fmt` layer with the same line layout. Profiled and compared in the bounded shared hosted sample; dedicated before/after acceptance remains open. |
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
| `cancel` | `GET /stream-long` (4,096 frames of 1 KiB); the client drops the response after the first nonempty incoming data frame, which may be a partial application chunk. On HTTP/1.1 that closes the connection (with a reset, as an aborting client's would, instead of leaving it in `TIME_WAIT`), so every request pays for a new connection; on HTTP/2 it resets the stream, with reuse subject to reset retention and protocol guards. |

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

The commands below describe the harness interface for a GitHub-hosted CI step. Qualification, profiling, formatting, builds, and tests for this work run only in hosted CI. The automated workflow builds the release binary once before collecting evidence:

```bash
cargo build --release -p example-bench

# One cell, one JSON line on stdout.
target/release/alloy-bench run --scenario alloy --workload small --transport h2c

# Every cell, 5 interleaved repetitions, appended to a hosted artifact.
target/release/alloy-bench matrix --reps 5 --label "shared hosted experimental" \
  --out "$RUNNER_TEMP/full-matrix.jsonl"

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

`matrix` takes `--scenarios`, `--workloads`, and `--transports` (each `all` or a comma-separated list, default `all`), `--reps` (default 5), and `--out` (default stdout). `Scenario::ALL` lists nine scenarios: the full matrix has 9 × 4 × 6 = 216 cells and 1,080 runs at five repetitions. The default 1-second warm-up plus 5-second window therefore takes **at least 108 minutes**, excluding process startup, connection preparation, draining, probes, and teardown.

How a run is made:

- **Process.** Server and client run in one process, on separate 4-worker Tokio runtimes, over loopback. `matrix` starts a fresh process for every run, because a process installs one global subscriber and a fresh heap keeps runs independent.
- **Ordering.** Every repetition runs every cell once, and each repetition rotates the order by one position, so no cell always runs first or last. Neighbors stay the same, so a slow cell affects the same next cell in every repetition.
- **Load.** The client is closed-loop: each worker sends its next request when the previous one finishes. Before the requested warm-up starts, every worker completes one exchange of the selected workload (including reading the first data frame and dropping the response for `cancel`). This preparation proves that the worker, connection, and server can serve the workload; a bound listener or an H2 dispatcher reporting `ready` alone does not. Preparation failures fail the run. Preparation requests, bytes, and latencies are excluded from measurement; `connects` still includes every connection opened, including the replacements after HTTP/1.1 preparation cancellations.
- **Window.** After the requested warm-up duration, workers stop starting warm-up requests and finish their in-flight exchanges, including reading the first frame and dropping the response for `cancel`. Every worker then parks between requests with a usable connection (reconnecting after HTTP/1.1 cancellation). Only once all workers have reached this boundary does the coordinator take the baseline probe and release the requested measurement window. Worker termination during preparation, warm-up, or boundary admission fails the run with its cause; remaining workers are aborted and joined before returning. Draining and reconnecting are outside the window; they do not extend its duration. Only requests that start and finish inside the window count. Throughput is those requests divided by the window.

## Result format

Each run prints one JSON object on one line. The format is versioned by `schema` (currently `alloy-bench/1`); fields may be added within a version, but a field that changes meaning or is removed bumps it. Unknown values are `null`, never zero.

`alloy-bench/1` is not published yet: no committed baseline uses it. The hosted qualification validator consumes its experimental result lines. Until baseline results are committed under it, a scenario's meaning may still change within the version. In particular, `alloy-logs` now measures Alloy's `JsonLayer`; before the unpublished `alloy-bench/1` schema is published, this change in meaning does not require a schema bump. Use `alloy-logs-fmt` to measure tracing-subscriber's JSON formatter for comparison, and compare `alloy-logs` lines only when they come from the same commit.

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
| `error_samples` | The messages and available error source chains of the first 5 client errors of the run (including failed reconnections), so a failing run says why; empty when `errors` is 0 |
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

The plan, which stays open in #15 and requires the [external unblock checklist](#external-unblock-checklist):

1. Run the full matrix on an owner-provided dedicated or isolated **GitHub-hosted** Linux environment, with verified CPU isolation and frequency controls, at least 5 interleaved repetitions, and commit the raw JSON lines under `examples/bench/results/`. A reported `performance` governor alone does not prove a pinned frequency or isolated physical CPU.
2. From that baseline, derive each cell's budget as a ratio to `plain` in the same cell and repetition, not as absolute requests per second, with the noise floor measured on `plain` itself.
3. Add a scheduled job on that host that runs the matrix and fails when a ratio moves by more than the noise floor.

A budget computed from result lines must use only lines with `errors == 0`, because a run with errors measured something other than the cell (fast failures inflate throughput), and must group lines by `run_id` before pairing a cell with `plain`, so that ratios never mix repetitions from different invocations, hosts, or commits. Only compare lines with the same `alloc_counting`.

Until then, no number is quoted without its artifact, source commit, environment, and repetition count. The harness compiles in the workspace build. Its real-service unit health matrices use a 1-second warm-up and a fixed 5-second window for every workload and transport on both plain and Alloy servers. Cancellation health has a finite per-worker exchange budget below the pinned client's reset-retention capacity (see [Functional health tests](#functional-health-tests)). Other workloads retain continuous closed-loop load. The warmed cancellation regression uses a 100 ms warm-up with the same 5-second window. Separate boundary/probe and full-body regressions retain their 200 ms windows; the end-to-end tests (every scenario as a separate `alloy-bench run` process, and a one-cell `matrix` whose output is parsed) also retain 0.2-second windows. These are functional checks, not performance budgets or dedicated-host acceptance.

## Hosted qualification preparation

[Hosted benchmark qualification](../.github/workflows/newbenchmark-only.yml) runs the bounded preparation workload weekly on `main`, and the full matrix on manual dispatch. It is not a pull request check. It checks out the exact `main` commit and builds one optimized binary with debug symbols using the pinned toolchain and `Cargo.lock`. That binary supplies every profile and comparison in the artifact. Existing workspace CI still checks formatting, linting, and tests; qualification additionally runs the harness tests and the compiled `json_log` schema/snapshot tests without updating snapshots.

The fixed weekly workload is bounded by a 60-minute job timeout and a 15-minute collection step:

- Callgrind profiles of `alloy-logs-fmt` and `alloy-logs`, each bounded to 90 seconds of wall time, with a 1-second measurement window, 0.2-second warm-up, eight concurrent HTTP/1.1 requests, and the `small` workload. Raw per-thread instruction/call graph dumps and function annotations are retained. These are one profile per formatter, including startup, preparation, warm-up, client work, and teardown. They locate candidate costs in unchanged code before any future optimization; they do not establish native elapsed-time costs or statistically repeated improvements. Callgrind changes scheduling and simulates instructions; see its [manual](https://valgrind.org/docs/manual/cl-manual.html).
- Five interleaved repetitions of `plain`, `alloy`, `alloy-logs`, and `alloy-logs-fmt`, with `small`, `h1`, 32 concurrent requests, the default 1-second warm-up and 5-second window: 20 runs. The existing matrix driver rotates by one cell per repetition. Neighbors remain correlated, so this ordering does not eliminate drift or constitute a measured noise floor.
- The same 20 runs in a separate allocation-counting pass. Only ratios within that pass are paired; its throughput is never compared with the ordinary pass.
- One smoke run of each OTEL scenario with the same load/window. Sampled exporters must make progress; unsampled must export/drop no spans; the unreachable exporter must report `export_failed`; the accepting stub must receive requests and bytes without export failures. Queue/byte-budget/shutdown losses remain raw facts, with no invented drop rate or performance threshold. These single runs check exercised behavior, not stability or real Collector health.

The Linux probes capture service/client on-CPU time, process RSS (including the client and warm-up peak), and allocation counts only in the counting pass. Span counters cover the measurement window, excluding spans still queued at its end; their totals need not equal completed requests. The stub returns an empty successful OTLP response without decoding spans. Its receipt checks establish only stub/exporter progress. The harness currently records no real Collector receiver/processor health, external Collector CPU/RSS, end-of-run queue backlog, or complete delivery accounting.

`examples/bench/hosted/qualify.py` validates all expected cells and repetitions, source/run identifiers, release environment, allocation mode, offered load, nonzero requests, zero client errors, and supported CPU/RSS counters. Missing or zero CPU probes, missing RSS, missing/empty profiles, profiler incompatibility, timeout, nonzero subprocess exits, and validation failures fail qualification. There is no `continue-on-error` and no waived gate. Commands run in process groups; timeout/cancellation terminates the group, including matrix children, and joins the directly owned child. Cleanup errors also fail. Runner termination can prevent final artifact upload; a missing artifact or `running` stage is incomplete evidence.

The supported profile layout is Callgrind format version 1 with the fixed options above: one final `part: 1` dump per thread, `positions: instr line`, and exactly `events: Ir`. Valgrind appends a thread ID to [separate-thread filenames](https://valgrind.org/docs/manual/cl-manual.html#cl-manual.options); the captured Valgrind 3.22 output uses `profile-<scenario>.callgrind.<PID>-<thread>` with at least two thread digits (`-01`, `-02`, ..., `-10`). The validator binds that canonical name to the owned profiler process PID and the dump's `pid`, `thread`, exact benchmark command, and creator version matching stderr. It requires the format marker, cache descriptions, a basic-block range starting at zero, and `Program termination` trigger. Partial/combined dumps, other metadata layouts, unknown suffixes/PIDs, symlinks, and extra profile files fail closed.

Valgrind 3.22 also creates a zero-byte unsuffixed `profile-<scenario>.callgrind.<PID>` placeholder. Only this matching base may be ignored, and only after all numbered thread files qualify, including the main thread (`-01`). Every child must be nonempty and complete, with one positive integer summary, a final matching positive `totals` footer, and a valid instruction/call/jump body whose self costs equal that total. Inclusive call costs are excluded from the sum; see the [Callgrind format](https://valgrind.org/docs/manual/cl-format.html). The sum across all children must match the same PID's positive final `Collected` count with exact `Ir` events in stderr, so a missing child cannot pass on the strength of another thread. A nonempty base or any empty/malformed child fails. Every qualified child must then complete its bounded annotation with no stderr diagnostics and matching instruction totals. Raw placeholders and rejected files remain in the artifact. These completeness checks do not establish native CPU costs or a performance gain; the completed preparation run is recorded below.

The run artifact `alloy-benchmark-<head SHA>-<run ID>-<attempt>` contains raw JSONL, stdout/stderr, exact commands and limits, stage status, source and lock/toolchain hashes, binary hash, runner image/OS/kernel/CPU/load/tool versions, raw profiles, summaries, and a `SHA256SUMS` manifest. Artifacts expire after 30 days; preserve them before citing results. Summaries pair throughput with `plain` and typed logs with fmt logs in the same repetition, invocation, workload, transport, commit, and allocation mode. They report median/range and `plain`'s observed spread, explicitly **not a noise floor**, budget, significance test, historical before/after comparison, or claimed improvement. Interpret them together with every raw row's environment.

On `main`, the workflow's input-free `workflow_dispatch` can collect the full 216-cell matrix with five repetitions (1,080 runs), plus the preparation passes above. Both the workflow and driver reject full execution from other refs. The full job is bounded to 210 minutes; full collection to 180 minutes, and the full matrix subprocess to 150 minutes. Dispatch is an owner/root action; this implementation does not launch it. It still uses a standard shared GitHub-hosted runner and an accepting stub, so its artifact is **experimental**, even when complete. It cannot satisfy #15's dedicated baseline, committed raw data, derived budgets, or regression enforcement, or #16's demonstrated improvement beyond measured noise. The bounded preparation evidence below does not change those acceptance limits.

The workflow has only `contents: read`, SHA-pinned actions, no persisted checkout credentials, caches, secrets, `pull_request_target`, or reusable untrusted workflow calls. It neither provisions infrastructure nor changes account settings. A dedicated acceptance workflow must be a separately reviewed main-only change.

## Hosted evidence (2026-10-04)

[PR #140](https://github.com/ferrum-edge/ferrum-alloy/pull/140) merged at `d7ddb3688e058ec3cc2e17d166a801aa0037b5b1`. Its reviewed producer head was [`d1baa65e03f5000d7da1acfc3b2712f72ae72510`](https://github.com/ferrum-edge/ferrum-alloy/tree/d1baa65e03f5000d7da1acfc3b2712f72ae72510): all 19 checks across two workflows passed, including [producer run 37216954327](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37216954327). The separate [main push CI 37218434984](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37218434984) passed on the merge commit. These are completed source checks, not a claim about later commits.

Artifact [`11308638664`](https://github.com/ferrum-edge/ferrum-alloy/actions/runs/37216954327/artifacts/11308638664), named `alloy-benchmark-d1baa65e03f5000d7da1acfc3b2712f72ae72510-37216954327-1`, retains the actual source, input/lock/toolchain and binary hashes, environment, commands, raw output and `SHA256SUMS`. Its integrity manifest was checked during the static ledger refresh. The environment is a **shared Ubuntu 24.04 GitHub VM**, x86_64, four visible CPUs, Intel Xeon Platinum 8573C; this is not evidence of pinned frequency or dedicated physical CPUs. The artifact contains:

- One Callgrind profile per existing formatter (`alloy-logs`, `alloy-logs-fmt`), each with ten final per-thread dumps, matching summary/footer and profiler totals, plus annotations. Callgrind reports simulated instructions, including preparation/client/teardown work, not a native production CPU baseline.
- 20 ordinary comparison records and 20 separate allocation-counting records: four scenarios (`plain`, `alloy`, `alloy-logs`, `alloy-logs-fmt`), five interleaved repetitions each, `small` / `h1`. On-CPU/RSS observations and allocation counters retain their environment and mode; counting and ordinary timings must not be mixed.
- Four OTEL smoke records. `otel-collector` used the in-process accepting stub, not a real Collector.

This is **experimental preparation**. It establishes neither a qualified noise floor nor the full 216-cell/1,080-observation acceptance matrix, real Collector health, historical before/after production gains or regression budgets. Production logging was unchanged by #140. Issues [#15](https://github.com/ferrum-edge/ferrum-alloy/issues/15#issuecomment-5982335298) and [#16](https://github.com/ferrum-edge/ferrum-alloy/issues/16#issuecomment-5982335463) remain open and externally blocked for dedicated acceptance. Artifacts expire after 30 days; root must preserve the evidence before it expires. The [external unblock checklist](#external-unblock-checklist) names the owner-dependent steps.

## Functional health tests

The harness's unit tests check that every cell works; they are not performance evidence.

- **Real-service matrices.** `plain_serves_every_workload_over_every_transport` and `alloy_serves_every_workload_over_every_transport` run every workload over all six transports against the real servers, with four workers, two streams per H2 connection, a 1-second warm-up and a fixed 5-second window. Each cell must complete work with nonzero latencies and zero errors, with exact body accounting and report labels. A 200 ms window was too short for a shared debug-build runner to complete work reliably.
- **Bounded cancellation.** Unlimited first-frame cancellations exceed pinned h2 0.4.19's 50 retained local resets per connection (`src/proto/mod.rs`); DATA that then arrives for a forgotten stream consumes the separate library-reset limit and ends the connection with `ENHANCE_YOUR_CALM` / `too_many_internal_resets`. Cancellation cells therefore give each worker a budget of eight exchanges in warm-up and eight in measurement, so two workers per connection need at most `2 × (1 + 8 + 8) = 34` retained resets, including preparation. Exactly 32 measured cancellations and no H2 reconnects are required. A budgeted run's client driver fails after 15 seconds instead of hanging the test binary. `late_data_requires_retained_reset_state_for_http2_reuse` pins the reset-retention behavior on the wire. Continuous CLI load is unchanged: a sustained cancellation benchmark can still reach these limits.
- **Warmed cancellation.** `cancellation_remains_healthy_after_normal_warmup` uses the same budget with a 100 ms warm-up (#143), which can end with workers mid-exchange, and the matrices' 5-second window. It requires exactly 32 measured cancellations. Its former 200 ms window measured none when a slow shared runner spent it on h2-mtls cancellations. The causal `http1_cancellation_reconnects_and_http2_does_not` fixture gates first DATA frames instead of relying on the window, and checks HTTP/1 reconnects and HTTP/2 reuse on all six transports.
- **Partial frames.** Hyper's HTTP/1 decoder can return part of a declared chunk, so a cancellation reads between one byte and 1 KiB.

The budgeted workers previously sent one more request on each retained H2 sender after the measurement deadline. The 15-second timeout in [#142](https://github.com/ferrum-edge/ferrum-alloy/issues/142) first occurred on the commit that added that test-only probe, and its one instrumented recurrence stalled waiting for the probe's response headers. The probe was removed, together with the test-only observers built to investigate it.

## External unblock checklist

The owner/root must provide these concrete inputs and evidence before dedicated baseline acceptance or a budget can be proposed. All execution remains in GitHub-hosted CI; no local or self-hosted repository execution is an alternative:

1. **Hosted execution environment:** an already provisioned GitHub-hosted Linux runner label/group, image/version, x86_64 CPU model/count and memory, execution allowance for the full matrix, and provider evidence of exclusive/isolated CPU allocation and frequency controls. Record affinity, governor, frequency/turbo policy, competing load, and how controls are verified throughout each run. Standard `ubuntu-24.04`, a larger VM label, or a `performance` governor alone is insufficient. If GitHub cannot supply these controls, record the acceptance criterion as blocked; do not label shared-VM observations dedicated or invent an isolation guarantee. This change does not provision paid runners or modify runner/account settings.
2. **Real Collector:** an owner-approved OTLP/HTTP `/v1/traces` URL reachable from that hosted environment, without embedding credentials in PRs/artifacts; a release-tagged, digest-pinned Collector image; the exact receiver/processor/exporter configuration and resource limits; and a durable output or receiver metrics proving accepted spans. The existing demo pin is `otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1`, a candidate to verify in hosted CI, not a provisioned qualification endpoint. Readiness alone is insufficient: retain before/after accepted/refused counts, logs, export responses/partial-success facts, backlog/drain behavior, and health observations during all repetitions. The benchmark's external-Collector request/byte fields are currently `null`; do not substitute stub counters for real evidence.
3. **Main-only workflow review:** pin the runner and Collector configuration in a new reviewed change with fixed source selection, no arbitrary ref/endpoint dispatch input, finite timeouts, complete cleanup, raw artifact hashes, and minimum permissions. Keep untrusted PR execution on the standard ephemeral runner. The full matrix must pass `--collector-endpoint` to the approved real endpoint; the current experimental dispatch deliberately uses the stub.
4. **Baseline and noise evidence:** archive and commit the complete raw results under `examples/bench/results/` with immutable source and binary/lock hashes, environment/control evidence, all 216 cells, at least five interleaved repetitions, zero client errors, and Collector health/drop facts. Collect repeated unchanged-code controls to derive the noise model; the five observed `plain` values on a shared runner are insufficient. Record allocation passes separately. Derive per-cell ratios and proposed budgets from that evidence, then review a failing regression gate against that baseline; do not choose arbitrary hard limits.
5. **Logging follow-up:** inspect both hosted profiles, identify a supported cost hypothesis, preserve the existing compiled JSON schema snapshot, and only then propose a production change. A same-binary typed/fmt comparison evaluates the existing implementation; any future before/after claim requires immutable before and after sources, a dedicated environment, paired interleaved repetitions, and measured unchanged-code noise. Keep [#16](https://github.com/ferrum-edge/ferrum-alloy/issues/16) open until that evidence supports an improvement.

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

**Expected effect:** a smaller gap between `alloy-logs` and `alloy`. The original layer change had no accompanying profile or benchmark. The later [hosted preparation](#hosted-evidence-2026-10-04) profiles and compares both existing implementations at one head; it does not establish a historical before/after gain or satisfy #16.

**How to measure it:** both formatters are in the same binary, so one build is enough. The hosted preparation above interleaves `alloy-logs-fmt` and `alloy-logs` within each repetition, with `plain` and `alloy` controls, and profiles both formatters. This compares the existing implementations at one immutable head; it is not a historical before/after experiment. `plain`'s observed variation on the shared runner is not a qualified noise floor. Inspect the profile artifacts before attributing remaining costs or making any further logging change.

## Discarded run

An earlier sequential run went through all scenarios in order, 3 times, for 10 seconds each, then ran large-payload and h2c variants once. Its load average was around 17 on 10 cores. `plain` fell from 178k to 77k requests/s between the first and third repetitions, and single runs contradicted each other; for example, `otel-sampled` with the large payload appeared faster than `plain`.

That data is kept in `examples/bench/results/2026-09-26-macos-m4-discarded-high-load.jsonl` for transparency and must not be quoted. Large-payload and h2c results therefore have **no valid measurement** yet.

## Not measured

- The complete matrix on a dedicated host: large payloads, streaming, cancellation, h2c, TLS, mTLS and a real healthy Collector still lack accepted baseline measurements. The bounded hosted sample supplies experimental small HTTP/1.1 on-CPU/RSS and allocation observations only; full acceptance waits for the external unblock checklist.
- Authorized diagnostic mode (`alloy-diagnostics`, #13): the scenario exists; no run has measured it.
- Behavior behind Ferrum Edge.
- A dedicated Linux benchmarking host. The existing Linux sample is a shared Ubuntu 24.04 VM.
- Long-duration stability and tail latency beyond p99.9.
