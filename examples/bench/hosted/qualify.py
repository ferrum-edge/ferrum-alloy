"""GitHub-hosted evidence collector. Never execute this on a local machine.

Uses the existing locked release binary; no performance budget or optimization.
Every command is bounded and recorded. Incomplete evidence fails the job.
"""

import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import sys
import time
import urllib.request


SCENARIOS = ("plain", "alloy", "alloy-logs", "alloy-logs-fmt")
OTEL = ("otel-sampled", "otel-unsampled", "otel-unreachable", "otel-collector")
ALL_SCENARIOS = SCENARIOS + ("alloy-diagnostics",) + OTEL
WORKLOADS = ("small", "large", "stream", "cancel")
TRANSPORTS = ("h1", "h2c", "h1-tls", "h2-tls", "h1-mtls", "h2-mtls")
LOSS_REASONS = {"queue_full", "byte_budget", "export_failed", "shutdown"}
# Exporter queue levels at the window's edges, read before shutdown.
QUEUE_FIELDS = ("spans_queued_at_start", "spans_queued_at_end", "spans_queued_bytes_at_end")
CLASSIFICATION = "shared GitHub-hosted experimental; not dedicated baseline acceptance"
# The real Collector the workflow starts beside the benchmark (collector.yaml).
COLLECTOR_CONTAINER = "alloy-bench-collector"
COLLECTOR_ENDPOINT = "http://127.0.0.1:4318/v1/traces"
COLLECTOR_HEALTH = "http://127.0.0.1:13133/"
COLLECTOR_METRICS = "http://127.0.0.1:8888/metrics"
# Receiver counters for the otlp receiver. `failed` exists only in newer
# Collectors, so it is checked when present; accepted and refused are required.
RECEIVER_COUNTERS = ("accepted", "refused", "failed")
PROMETHEUS_SAMPLE = re.compile(
    r"([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)(?:\s+-?[0-9]+)?")
PROMETHEUS_LABEL = re.compile(r'\s*([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"\s*(?:,|$)')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def positive(value):
    return type(value) in (int, float) and math.isfinite(value) and value > 0


def counter(value):
    return type(value) is int and value >= 0


def validate(rows, scenarios, workloads, transports, reps, sha, run_id, counting,
             seconds=5.0, warmup=1.0, concurrency=32, external_collector=False):
    """Reject incomplete, mixed, duplicate, erroneous, or unsupported evidence.

    With `external_collector`, `otel-collector` exported to the real Collector,
    whose receipt is checked from its own counters (`collector_accounting`);
    the result line's stub-only receipt fields must then be null, not zero.
    """
    expected = {(rep, scenario, workload, transport)
                for rep in range(1, reps + 1) for scenario in scenarios
                for workload in workloads for transport in transports}
    seen = set()
    for row in rows:
        key = (row.get("rep"), row.get("scenario"), row.get("workload"), row.get("transport"))
        require(key in expected and key not in seen, f"unexpected/duplicate cell: {key}")
        seen.add(key)
        require(row.get("schema") == "alloy-bench/1", "unknown result schema")
        require(row.get("commit") == sha and row.get("run_id") == run_id,
                "mixed source or invocation")
        require(row.get("alloc_counting") is counting, "mixed allocation modes")
        require(row.get("errors") == 0 and row.get("error_samples") == [],
                f"client errors in {key}")
        require(positive(row.get("requests")) and positive(row.get("requests_per_second")),
                f"empty/invalid measurement in {key}")
        require(row.get("seconds") == seconds and row.get("warmup_seconds") == warmup
                and row.get("concurrency") == concurrency, "different load/window")
        require(row.get("server_threads") == 4 and row.get("client_threads") == 4,
                "different runtime topology")
        h2 = row["transport"].startswith("h2")
        require(row.get("streams_per_connection") == (8 if h2 else 1)
                and row.get("connections") == (concurrency // 8 if h2 else concurrency),
                "different connection topology")
        environment = row.get("environment") or {}
        require(environment.get("os") == "linux" and environment.get("debug_build") is False,
                "requires release Linux evidence")
        require(positive(environment.get("cpus")) and environment.get("kernel")
                and positive(environment.get("started_unix_seconds")), "missing environment")
        cpu = row.get("cpu") or {}
        require(positive(cpu.get("service_ns")) and positive(cpu.get("client_ns")),
                "unsupported or empty schedstat CPU measurement")
        require(positive(cpu.get("service_us_per_request"))
                and positive(cpu.get("client_us_per_request")), "missing CPU per request")
        memory = row.get("memory") or {}
        require(memory.get("scope") == "process" and positive(memory.get("rss_bytes"))
                and positive(memory.get("peak_rss_bytes")), "unsupported RSS measurement")
        allocations = row.get("allocations")
        if counting:
            require(isinstance(allocations, dict), "missing allocation counters")
            for role in ("service", "client", "collector"):
                counts = allocations.get(role, {})
                require(counter(counts.get("calls")) and counter(counts.get("bytes")),
                        f"missing {role} allocation counters")
            require(positive(allocations["service"]["calls"]), "empty service allocation count")
            require(positive(allocations["service"].get("calls_per_request")),
                    "missing allocations per request")
        else:
            require(allocations is None, "unexpected allocation counters")
        otel = row.get("otel")
        if row["scenario"] in OTEL:
            require(isinstance(otel, dict), "missing OTEL counters")
            lost = otel.get("spans_lost", {})
            require(set(lost) == LOSS_REASONS and all(counter(v) for v in lost.values()),
                    "missing/invalid loss counters")
            require(counter(otel.get("spans_exported")), "missing export counter")
            require(all(counter(otel.get(field)) for field in QUEUE_FIELDS),
                    "missing window-edge queue counters")
            if row["scenario"] == "otel-unsampled":
                require(otel["spans_exported"] == 0 and sum(lost.values()) == 0
                        and all(otel[field] == 0 for field in QUEUE_FIELDS),
                        "unsampled scenario exported/lost/queued spans")
            elif row["scenario"] == "otel-unreachable":
                require(lost["export_failed"] > 0, "unreachable exporter not exercised")
            else:
                require(otel["spans_exported"] > 0 and lost["export_failed"] == 0,
                        "healthy exporter made no progress or failed")
            if row["scenario"] == "otel-collector" and external_collector:
                require("collector_requests" in otel and otel["collector_requests"] is None
                        and "collector_bytes" in otel and otel["collector_bytes"] is None,
                        "stub receipt counters reported for a real Collector")
            elif row["scenario"] == "otel-collector":
                require(positive(otel.get("collector_requests"))
                        and positive(otel.get("collector_bytes")), "stub received no exports")
        else:
            require(otel is None, "unexpected OTEL counters")
    require(seen == expected, f"missing {len(expected - seen)} result cells")


def distribution(values):
    return {"n": len(values), "median": statistics.median(values),
            "min": min(values), "max": max(values)}


def summarize(rows):
    """Same-invocation and same-repetition pairing only; observations, not budgets."""
    groups = {}
    for row in rows:
        key = (row["run_id"], row["commit"], row["alloc_counting"],
               row["workload"], row["transport"])
        groups.setdefault(key, []).append(row)
    summaries = []
    for key, group in groups.items():
        by_cell = {(row["rep"], row["scenario"]): row for row in group}
        plain = [row["requests_per_second"] for row in group if row["scenario"] == "plain"]
        for scenario in sorted({row["scenario"] for row in group}):
            selected = [row for row in group if row["scenario"] == scenario]
            ratios = [row["requests_per_second"] /
                      by_cell[(row["rep"], "plain")]["requests_per_second"] for row in selected]
            summary = {"run_id": key[0], "commit": key[1], "alloc_counting": key[2],
                       "workload": key[3], "transport": key[4], "scenario": scenario,
                       "throughput_ratio_to_plain": distribution(ratios),
                       "service_cpu_us_per_request": distribution(
                           [row["cpu"]["service_us_per_request"] for row in selected]),
                       "process_peak_rss_bytes": distribution(
                           [row["memory"]["peak_rss_bytes"] for row in selected]),
                       "plain_observed_spread": (max(plain) - min(plain)) /
                                                statistics.median(plain)}
            if key[2]:
                summary["service_allocations_per_request"] = distribution(
                    [row["allocations"]["service"]["calls_per_request"] for row in selected])
            if scenario == "alloy-logs":
                summary["throughput_ratio_typed_to_fmt"] = distribution(
                    [row["requests_per_second"] /
                     by_cell[(row["rep"], "alloy-logs-fmt")]["requests_per_second"]
                     for row in selected])
            summaries.append(summary)
    return summaries


def receiver_counters(text):
    """Sum the otlp receiver's span counters from Prometheus text exposition.

    Matches `otelcol_receiver_<kind>_spans` with or without `_total`, summed over
    other labels (transport, scope). A family that never appeared is absent, not
    zero; the caller decides which absences are acceptable.
    """
    counters = {}
    names = {f"otelcol_receiver_{kind}_spans{suffix}": kind
             for kind in RECEIVER_COUNTERS for suffix in ("", "_total")}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        sample = PROMETHEUS_SAMPLE.fullmatch(line)
        require(sample is not None, f"malformed Collector metric line: {line[:80]}")
        kind = names.get(sample[1])
        if kind is None:
            continue
        labels = dict(PROMETHEUS_LABEL.findall(sample[2] or ""))
        if labels.get("receiver") != "otlp":
            continue
        value = float(sample[3])
        require(math.isfinite(value) and value >= 0 and value == int(value),
                f"invalid Collector counter {sample[1]}")
        counters[kind] = counters.get(kind, 0) + int(value)
    return counters


def collector_accounting(before, after, rows):
    """Prove the real Collector accepted what the exporter reported as exported.

    `before`/`after` are `Evidence.collector_state` snapshots around one matrix.
    Every span the exporter counts as exported got a success response, and the
    receiver counts it as accepted, so accepted growth must cover the window
    exports (it also includes warm-up, retries, and shutdown drain). A refused
    or failed span, a counter reset, or a container restart fails.
    """
    for state in (before, after):
        require(state["healthy"], "Collector health check failed")
        container = state["container"]
        require(container["running"] and not container["oom_killed"]
                and container["restart_count"] == 0, "Collector container not running cleanly")
    require(before["container"]["id"] == after["container"]["id"]
            and before["container"]["started_at"] == after["container"]["started_at"],
            "Collector container restarted during the matrix")
    counts = after["receiver"]
    require("accepted" in counts and "refused" in counts,
            "Collector receiver counters missing after export")
    delta = {}
    for kind in RECEIVER_COUNTERS:
        if kind in counts:
            delta[kind] = counts[kind] - before["receiver"].get(kind, 0)
            require(delta[kind] >= 0, f"Collector {kind} counter went backwards")
    exported = sum(row["otel"]["spans_exported"] for row in rows
                   if row["scenario"] == "otel-collector")
    require(exported > 0, "no window exports to account for")
    require(delta["accepted"] >= exported,
            "Collector accepted fewer spans than the exporter reported exported")
    require(delta["refused"] == 0 and delta.get("failed", 0) == 0,
            "Collector refused or failed spans")
    return {"receiver_spans_delta": delta, "window_spans_exported": exported,
            "accepted_outside_windows": delta["accepted"] - exported}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def instruction_profile(path, pid, thread, command, creator):
    """Require one complete format-1 final thread dump, not a partial/combined dump.

    This is the Ir-only, instr+line format emitted by our fixed Callgrind options
    (observed on Valgrind 3.22). Inclusive call costs are not added to self costs.
    The hosted annotator additionally checks symbol/position interpretation.
    """
    text = path.read_text()
    require(text.endswith("\n") and "\0" not in text, f"empty/truncated profile: {path.name}")
    lines = text.splitlines()
    require(lines[0] == "# callgrind format", "unsupported profile format")
    summaries = [index for index, line in enumerate(lines) if line.startswith("summary:")]
    require(len(summaries) == 1, "missing/duplicate profile summary")
    end = summaries[0]
    header = [line.strip() for line in lines[:end] if line.strip()]
    expected = ["# callgrind format", "version: 1", f"creator: {creator}", f"pid: {pid}",
                f"cmd:  {command}", "part: 1", f"thread: {thread}",
                "desc: I1 cache:", "desc: D1 cache:", "desc: LL cache:"]
    require(header[:len(expected)] == expected, "unsupported/mismatched profile metadata")
    remaining = header[len(expected):]
    require(len(remaining) == 4
            and re.fullmatch(r"desc: Timerange: Basic block 0 - [1-9][0-9]*", remaining[0])
            and remaining[1:] == ["desc: Trigger: Program termination",
                                  "positions: instr line", "events: Ir"],
            "unsupported profile range/trigger/positions/events")
    summary = re.fullmatch(r"summary: ([1-9][0-9]*)", lines[end])
    footer = re.fullmatch(r"totals: ([1-9][0-9]*)", lines[-1])
    require(summary is not None and footer is not None, "invalid/zero summary or missing totals")
    instructions = int(summary[1])
    require(int(footer[1]) == instructions, "profile summary/totals differ")

    position = r"(?:[+-]?(?:0x[0-9a-fA-F]+|[0-9]+)|\*)"
    cost = re.compile(rf"{position}\s+{position}(?:\s+([0-9]+))?\s*")
    association = re.compile(rf"(?:calls=[1-9][0-9]*|jump=[0-9]+|jcnd=[0-9]+/[0-9]+)"
                             rf"\s+{position}\s+{position}\s*")
    symbol = re.compile(r"(?:ob|fl|fi|fe|fn|cob|cfi|cfl|cfn|jfi|jfn)=\S.*")
    self_cost = 0
    pending_call = False
    has_function = False
    for line in lines[end + 1:-1]:
        if not line.strip():
            continue
        entry = cost.fullmatch(line)
        require(not pending_call or entry is not None, "call missing profile cost line")
        if entry is not None:
            require(has_function, "profile cost missing function")
            if not pending_call:
                self_cost += int(entry[1] or "0")
            pending_call = False
        elif symbol.fullmatch(line):
            has_function = has_function or line.startswith("fn=")
        elif association.fullmatch(line):
            require(has_function, "profile association missing function")
            pending_call = line.startswith("calls=")
        else:
            raise ValueError(f"malformed profile body: {path.name}")
    require(not pending_call and self_cost == instructions, "incomplete profile self costs")
    return instructions


def instruction_profiles(out, name, pid, command):
    """Recognize PID[-%02d thread] names; ignore only the matching empty base.

    All thread files must qualify before the base placeholder can be ignored.
    Their disjoint self costs must equal the same PID's final stderr collection.
    No alternative PID, partial dump, unknown suffix, or empty child is waived.
    See docs/benchmarks.md for the supported format and completeness semantics.
    """
    require(type(pid) is int and pid > 0, "missing profiler process identity")
    messages = []
    for line in (out / f"{name}.stderr").read_text().splitlines():
        if line.startswith("=="):
            prefix = f"=={pid}== "
            require(line.startswith(prefix), "mixed/malformed profiler PID")
            messages.append(line[len(prefix):])

    def message(prefix, pattern):
        selected = [line for line in messages if line.startswith(prefix)]
        require(len(selected) == 1, f"missing/duplicate profiler {prefix}")
        match = re.fullmatch(pattern, selected[0])
        require(match is not None, f"unsupported profiler {prefix}")
        return match

    version = message("Using", r"Using Valgrind-(3\.[0-9]+\.[0-9]+) and LibVEX; "
                              r"rerun with -h for copyright info")[1]
    message("Events", r"Events\s+: Ir")
    collected = int(message("Collected", r"Collected\s+: ([1-9][0-9]*)")[1])
    base = f"{name}.callgrind.{pid}"
    paths = sorted(out.glob(f"{name}.callgrind*"))
    profiles = []
    for path in paths:
        require(path.is_file() and not path.is_symlink(), "unsupported profile file")
        if path.name == base:
            require(path.stat().st_size == 0, "nonempty unsuffixed per-thread profile")
            continue
        suffix = re.fullmatch(re.escape(base) + r"-([0-9]+)", path.name)
        require(suffix is not None, f"unexpected profile name/PID: {path.name}")
        thread = int(suffix[1])
        require(thread > 0 and suffix[1] == f"{thread:02d}", "noncanonical profile thread")
        instructions = instruction_profile(path, pid, thread, command, f"callgrind-{version}")
        profiles.append((path, instructions))
    require(profiles and any(path.name == f"{base}-01" for path, _ in profiles),
            "unsupported/missing main-thread instruction profile")
    require(sum(total for _, total in profiles) == collected,
            "incomplete thread profiles: totals differ from profiler collection")
    return profiles


def stop(process):
    """Terminate the command group and join the owned child, even after leader exit."""
    def signal_group(sig):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass  # Already exited; every other cleanup error propagates.

    signal_group(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        signal_group(signal.SIGKILL)
        process.wait(timeout=5)
    finally:
        # The leader can exit while a matrix child is still alive.
        signal_group(signal.SIGKILL)


class Evidence:
    def __init__(self, out, sha):
        self.out = out
        self.sha = sha
        self.commands = []
        self.stages = {}

    def command(self, name, args, timeout):
        record = {"name": name, "argv": [str(arg) for arg in args], "timeout_seconds": timeout,
                  "started_unix_seconds": time.time(), "status": "running"}
        self.commands.append(record)
        write_json(self.out / "commands.json", self.commands)
        env = dict(os.environ, GITHUB_SHA=self.sha)
        try:
            with (self.out / f"{name}.stdout").open("w") as stdout, \
                 (self.out / f"{name}.stderr").open("w") as stderr:
                process = subprocess.Popen(args, stdout=stdout, stderr=stderr, env=env,
                                           start_new_session=True)
                record["pid"] = process.pid
                try:
                    record["exit_code"] = process.wait(timeout=timeout)
                finally:
                    stop(process)
            require(record["exit_code"] == 0, f"{name} exited {record['exit_code']}")
            record["status"] = "complete"
        except BaseException as error:
            record["status"] = "failed"
            record["error"] = str(error)
            raise
        finally:
            record["ended_unix_seconds"] = time.time()
            write_json(self.out / "commands.json", self.commands)
        return record["pid"]

    def stage(self, name, action):
        self.stages[name] = {"status": "running"}
        self.save()
        try:
            action()
            self.stages[name] = {"status": "complete"}
        except Exception as error:
            self.stages[name] = {"status": "failed", "error": str(error)}
            print(f"{name}: {error}", file=sys.stderr)
        finally:
            self.save()

    def save(self):
        write_json(self.out / "status.json", {
            "classification": CLASSIFICATION, "commit": self.sha, "stages": self.stages,
            "run_url": f"https://github.com/{os.environ.get('GITHUB_REPOSITORY', '')}"
                       f"/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}",
            "attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
            "event": os.environ.get("GITHUB_EVENT_NAME"),
            "collector": "otel-smoke: in-process accepting stub; collector stage and full "
                         "matrix: real pinned Collector co-located on the shared runner",
            "profile": "Callgrind simulated instructions; not native CPU time",
            "dedicated_baseline": "blocked: owner-provided hosted environment required",
        })

    def collector_state(self, name, wait=0):
        """Health, receiver counters, and container identity of the real Collector.

        Raw metrics text is retained. CPU and peak memory come from the
        container's cgroup when readable, else null (unknown, not zero).
        """
        deadline = time.monotonic() + wait
        while True:
            try:
                with urllib.request.urlopen(COLLECTOR_HEALTH, timeout=5) as response:
                    healthy = response.status == 200
            except OSError:
                healthy = False
            if healthy or time.monotonic() >= deadline:
                break
            time.sleep(1)
        with urllib.request.urlopen(COLLECTOR_METRICS, timeout=10) as response:
            require(response.status == 200, "Collector metrics unavailable")
            text = response.read().decode()
        (self.out / f"{name}.prom").write_text(text)
        inspect = json.loads(subprocess.check_output(
            ["docker", "container", "inspect", COLLECTOR_CONTAINER], text=True, timeout=30))
        require(len(inspect) == 1, "ambiguous Collector container")
        state = inspect[0]["State"]
        container_id = inspect[0]["Id"]
        cgroup = Path(f"/sys/fs/cgroup/system.slice/docker-{container_id}.scope")
        cpu = None
        if (cgroup / "cpu.stat").is_file():
            for line in (cgroup / "cpu.stat").read_text().splitlines():
                if line.startswith("usage_usec "):
                    cpu = int(line.split()[1])
        peak = None
        if (cgroup / "memory.peak").is_file():
            peak = int((cgroup / "memory.peak").read_text())
        snapshot = {
            "healthy": healthy,
            "receiver": receiver_counters(text),
            "container": {"id": container_id, "image": inspect[0]["Config"]["Image"],
                          "started_at": state["StartedAt"], "running": state["Running"],
                          "oom_killed": state["OOMKilled"],
                          "restart_count": inspect[0]["RestartCount"]},
            "cgroup_cpu_usage_usec": cpu,
            "cgroup_memory_peak_bytes": peak,
        }
        write_json(self.out / f"{name}.json", snapshot)
        return snapshot

    def matrix(self, name, scenarios, workloads, transports, reps=5, counting=False, timeout=300,
               collector=False):
        run_id = f"{os.environ['GITHUB_RUN_ID']}-{os.environ['GITHUB_RUN_ATTEMPT']}-{name}"
        path = self.out / f"{name}.jsonl"
        args = ["target/release/alloy-bench", "matrix", "--scenarios", ",".join(scenarios),
                "--workloads", ",".join(workloads), "--transports", ",".join(transports),
                "--reps", str(reps), "--run-id", run_id, "--out", str(path),
                "--seconds", "5", "--warmup", "1", "--concurrency", "32", "--streams", "8",
                "--label", CLASSIFICATION]
        if counting:
            args.append("--alloc-counting")
        if collector:
            args.extend(["--collector-endpoint", COLLECTOR_ENDPOINT])
            before = self.collector_state(f"{name}-collector-before", wait=60)
        self.command(name, args, timeout)
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        validate(rows, scenarios, workloads, transports, reps, self.sha, run_id, counting,
                 external_collector=collector)
        if collector:
            after = self.collector_state(f"{name}-collector-after")
            accounting = collector_accounting(before, after, rows)
            cpu = [state["cgroup_cpu_usage_usec"] for state in (before, after)]
            write_json(self.out / f"{name}-collector.json", {
                "classification": CLASSIFICATION, "raw_file": path.name,
                "raw_sha256": digest(path), "endpoint": COLLECTOR_ENDPOINT,
                "co_located": True, **accounting,
                "collector_cpu_usec_delta": None if None in cpu else cpu[1] - cpu[0],
                "collector_memory_peak_bytes": after["cgroup_memory_peak_bytes"],
            })
        if "plain" in scenarios:
            write_json(self.out / f"{name}-summary.json", {
                "classification": CLASSIFICATION, "raw_file": path.name,
                "raw_sha256": digest(path), "plain_spread_is_noise_floor": False,
                "summaries": summarize(rows),
            })

    def profile(self, scenario):
        name = f"profile-{scenario}"
        command = ["target/release/alloy-bench", "run", "--scenario", scenario,
                   "--workload", "small", "--transport", "h1", "--seconds", "1", "--warmup", "0.2",
                   "--concurrency", "8", "--rep", "1", "--run-id", name,
                   "--label", CLASSIFICATION]
        args = ["valgrind", "--tool=callgrind", "--dump-instr=yes", "--collect-jumps=yes",
                "--separate-threads=yes", f"--callgrind-out-file={self.out}/{name}.callgrind.%p",
                *command]
        pid = self.command(name, args, 90)
        rows = [json.loads(line) for line in (self.out / f"{name}.stdout").read_text().splitlines()]
        validate(rows, [scenario], ["small"], ["h1"], 1, self.sha, name, False,
                 seconds=1.0, warmup=0.2, concurrency=8)
        profiles = instruction_profiles(self.out, name, pid, " ".join(command))
        for index, (path, instructions) in enumerate(profiles):
            annotation = f"{name}-annotate-{index}"
            self.command(annotation, ["callgrind_annotate", "--auto=no", "--show-percs=no",
                                      str(path)], 15)
            require(not (self.out / f"{annotation}.stderr").read_text().strip(),
                    "profile annotation diagnostics")
            totals = [" ".join(line.split()) for line in
                      (self.out / f"{annotation}.stdout").read_text().splitlines()
                      if "PROGRAM TOTALS" in line]
            require(totals == [f"{instructions:,} PROGRAM TOTALS"],
                    "missing/mismatched instruction annotation totals")


def main():
    require(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_OS") == "Linux",
            "hosted CI only")
    out = Path(os.environ["QUALIFICATION_OUT"])
    out.mkdir(parents=True, exist_ok=True)
    sha = os.environ["QUALIFICATION_SHA"]
    full = os.environ["QUALIFICATION_FULL"] == "1"
    require(not full or (os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
                         and os.environ.get("GITHUB_REF") == "refs/heads/main"),
            "full matrix only from main workflow_dispatch")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip() == sha,
            "checkout differs from immutable source")
    binary = Path("target/release/alloy-bench")
    before = digest(binary)
    evidence = Evidence(out, sha)
    for scenario in ("alloy-logs-fmt", "alloy-logs"):
        evidence.stage(f"profile-{scenario}", lambda scenario=scenario: evidence.profile(scenario))
    evidence.stage("comparison", lambda: evidence.matrix(
        "comparison", SCENARIOS, ["small"], ["h1"]))
    evidence.stage("allocations", lambda: evidence.matrix(
        "allocations", SCENARIOS, ["small"], ["h1"], counting=True))
    evidence.stage("otel-smoke", lambda: evidence.matrix(
        "otel-smoke", OTEL, ["small"], ["h1"], reps=1, timeout=90))
    evidence.stage("collector", lambda: evidence.matrix(
        "collector", ("plain", "alloy", "otel-collector"), ["small"], ["h1"], collector=True))
    if full:
        evidence.stage("full-matrix", lambda: evidence.matrix(
            "full-matrix", ALL_SCENARIOS, WORKLOADS, TRANSPORTS, timeout=9000, collector=True))
    evidence.stage("immutable-binary", lambda: require(digest(binary) == before, "binary changed"))
    complete = all(stage["status"] == "complete" for stage in evidence.stages.values())
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
        summary.write(f"### Benchmark preparation: {'complete' if complete else 'FAILED'}\n\n"
                      f"{CLASSIFICATION}. Source `{sha}`, binary SHA-256 `{before}`.\n\n"
                      "Raw JSONL, commands, environment, profiles, summaries, and hashes are in "
                      "the run artifact. No performance budget or improvement is asserted. "
                      "The real Collector is co-located on this shared runner; a dedicated "
                      "hosted baseline remains an external blocker.\n")
    return 0 if complete else 1


if __name__ == "__main__":
    # Runner cancellation must unwind the active command's process-group cleanup.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    sys.exit(main())
