"""Shared-runner benchmark regression gate. Never execute this on a local machine.

See docs/benchmarks.md#regression-gate. `collect` runs one matrix slot beside
the real Collector; `derive` turns unchanged-code calibration slots into
budgets per CPU model and checks them out of sample and against a synthetic
regression; `check` holds one fresh run to the committed budgets. Every budget
check goes through `alloy-bench compare --budget`, the same code CI and users
run, so the gate cannot drift from the documented pairing rules.
"""

import json
import math
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import sys

import qualify
from qualify import digest, require, write_json


BINARY = Path("target/release/alloy-bench")
BUDGETS = Path("examples/bench/budgets")
INDEX_SCHEMA = "alloy-bench-budget-index/1"
SLOT_SCHEMA = "alloy-bench-slot/1"
BUDGET_SCHEMA = "alloy-bench-budget/1"
CLASSIFICATION = ("shared GitHub-hosted; regression budgets per CPU model from "
                  "unchanged-code calibration")

# `alloy-bench compare` field -> (budget key, sign of the limit's margin).
METRICS = {"throughput_ratio": ("min_throughput_ratio", -1),
           "latency_p50_ratio": ("max_latency_p50_ratio", 1),
           "latency_p99_ratio": ("max_latency_p99_ratio", 1)}
# The derivation rule (docs/benchmarks.md#deriving-budgets). A limit sits
# SPREAD_MULTIPLIER times the widest unchanged-code deviation from the median
# calibration run, plus MARGIN_FLOOR, in log space. A metric whose margin would
# exceed MARGIN_CAP is too noisy to gate and is left unbudgeted.
SPREAD_MULTIPLIER = 3.0
MARGIN_FLOOR = 0.02
MARGIN_CAP = 0.20
# A CPU model gets its own budgets from at least this many calibration runs;
# runners of any other model are checked against the pooled budgets.
MIN_MODEL_RUNS = 3

PROFILES = {
    # The full matrix, at the policy's repetitions and window. Main only.
    "full": {"scenarios": qualify.ALL_SCENARIOS, "workloads": qualify.WORKLOADS,
             "transports": qualify.TRANSPORTS, "reps": 5, "seconds": 5, "warmup": 1,
             "timeout": 10800, "inject_ns": 5000, "min_calibration": 6},
    # Pull requests exercise the pipeline on a tiny matrix. Not evidence.
    "smoke": {"scenarios": ("plain", "alloy", "otel-collector"), "workloads": ("small",),
              "transports": ("h1", "h2c"), "reps": 3, "seconds": 1, "warmup": 0.5,
              "timeout": 600, "inject_ns": 50000, "min_calibration": 3},
}
SLOT = re.compile(r"(calibration)-[0-9]{2}|(injected)|(allocations)|(gate)")


def slot_kind(slot):
    match = SLOT.fullmatch(slot)
    require(match is not None, f"unknown slot {slot!r}")
    return next(group for group in match.groups() if group)


def model_slug(model):
    slug = re.sub(r"[^a-z0-9]+", "-", model.lower()).strip("-")
    require(slug, f"CPU model {model!r} has no usable name")
    return slug


def run_compare(paths, budget=None, binary=BINARY):
    """`alloy-bench compare`: its exit code and printed lines."""
    args = [str(binary), "compare", *(str(path) for path in paths)]
    if budget is not None:
        args.extend(["--budget", str(budget)])
    done = subprocess.run(args, capture_output=True, text=True, timeout=300, check=False)
    lines = [json.loads(line) for line in done.stdout.splitlines() if line.strip()]
    return done.returncode, lines, done.stderr


def exceeded(lines):
    return [{"scenario": line["scenario"], "workload": line["workload"],
             "transport": line["transport"], "exceeded": line["budget"]["exceeded"]}
            for line in lines if line.get("budget") and line["budget"]["exceeded"]]


def check_budget(path, budget, binary=BINARY):
    """Whether one run is within a budget file, by `compare --budget`.

    A nonzero exit with no exceeded limit is not a regression verdict (for
    example a budgeted cell without data), so it fails as an error instead.
    """
    code, lines, stderr = run_compare([path], budget, binary)
    over = exceeded(lines)
    require(code == 0 or over, f"compare failed without a budget verdict: {stderr.strip()}")
    budgeted = sum(1 for line in lines if line.get("budget") is not None)
    require(budgeted, "no cell of the run is budgeted")
    return {"passed": code == 0, "exceeded": over, "budgeted": budgeted}


def medians(lines):
    """Per-cell medians of one calibration run's `compare` output."""
    require(lines, "compare printed no cells")
    require(len({line["run_id"] for line in lines}) == 1, "medians mix invocations")
    cells = {}
    for line in lines:
        require(line["alloc_counting"] is False, "calibration must not count allocations")
        cell = (line["scenario"], line["workload"], line["transport"])
        require(cell not in cells, f"duplicate compare cell {cell}")
        cells[cell] = {metric: (line[metric] or {}).get("median") for metric in METRICS}
    return cells


def derive(runs):
    """Budgets from per-cell medians of unchanged-code runs, with their derivation.

    For each cell and metric: `center` is the median over runs, `spread` the
    largest |ln(run / center)|, and the limit sits `margin` = SPREAD_MULTIPLIER
    x spread + MARGIN_FLOOR beyond the center in log space (below it for
    throughput, above it for latency). Limits round outward. A metric that is
    unknown in any run, or whose margin exceeds MARGIN_CAP, gets no limit.
    """
    require(len(runs) >= 2, "a noise model needs at least two runs")
    cells = set(runs[0])
    require(all(set(run) == cells for run in runs), "calibration runs cover different cells")
    budgets, records = [], []
    for scenario, workload, transport in sorted(cells):
        cell = (scenario, workload, transport)
        entry = {"scenario": scenario, "workload": workload, "transport": transport,
                 "alloc_counting": False}
        record = {"scenario": scenario, "workload": workload, "transport": transport,
                  "metrics": {}}
        for metric, (key, sign) in METRICS.items():
            values = [run[cell][metric] for run in runs]
            if not all(qualify.positive(value) for value in values):
                record["metrics"][metric] = {"gated": False, "reason": "unknown in a run"}
                continue
            center = statistics.median(values)
            spread = max(abs(math.log(value / center)) for value in values)
            margin = SPREAD_MULTIPLIER * spread + MARGIN_FLOOR
            info = {"runs": len(values), "values": values, "center": center,
                    "spread": spread, "margin": margin}
            if margin > MARGIN_CAP:
                info.update(gated=False, reason="unchanged-code noise exceeds the cap")
            else:
                scaled = center * math.exp(sign * margin) * 10_000
                limit = (math.ceil(scaled) if sign > 0 else math.floor(scaled)) / 10_000
                require(limit > 0, f"nonpositive limit for {cell} {metric}")
                entry[key] = limit
                info.update(gated=True, limit=limit)
            record["metrics"][metric] = info
        if any(key in entry for key, _ in METRICS.values()):
            budgets.append(entry)
        records.append(record)
    return {"schema": BUDGET_SCHEMA, "budgets": budgets}, records


def budget_sets(calibration):
    """Pooled budgets (key None) plus one set per well-sampled CPU model.

    `calibration` is a list of (cpu_model, medians) pairs.
    """
    sets = {None: derive([cells for _, cells in calibration])}
    for model in sorted({model for model, _ in calibration}):
        runs = [cells for other, cells in calibration if other == model]
        if len(runs) >= MIN_MODEL_RUNS:
            sets[model] = derive(runs)
    return sets


def select(sets, model):
    """The budgets a run on `model` is held to: its model's, else pooled."""
    return model if model in sets else None


def run_model(rows):
    models = {(row.get("environment") or {}).get("cpu_model") for row in rows}
    require(len(models) == 1, "one run spans several CPU models")
    model = models.pop()
    require(isinstance(model, str) and model.strip(), "run has no CPU model")
    return model


def verify_manifest(directory):
    """Every file of a downloaded slot artifact matches its SHA256SUMS."""
    listed = set()
    for line in (directory / "SHA256SUMS").read_text().splitlines():
        expected, name = line.split(maxsplit=1)
        path = directory / name.removeprefix("./")
        require(path.is_file() and not path.is_symlink(), f"missing evidence file {name}")
        require(digest(path) == expected, f"evidence hash mismatch: {name}")
        listed.add(path.resolve())
    files = {path.resolve() for path in directory.rglob("*")
             if path.is_file() and path.name != "SHA256SUMS"}
    require(files == listed, f"unlisted evidence in {directory.name}")


def load_slots(root, sha, profile):
    """Complete slots by name; a rerun's later attempt replaces an earlier one."""
    slots = {}
    for directory in sorted(path for path in root.iterdir() if path.is_dir()):
        if not (directory / "slot.json").is_file():
            continue  # A failed attempt's partial evidence; its rerun supplies the slot.
        verify_manifest(directory)
        slot = json.loads((directory / "slot.json").read_text())
        require(slot.get("schema") == SLOT_SCHEMA and slot.get("commit") == sha
                and slot.get("profile") == profile, f"foreign slot in {directory.name}")
        raw = directory / slot["raw_file"]
        require(digest(raw) == slot["raw_sha256"], f"raw results changed in {directory.name}")
        slot["path"] = raw
        previous = slots.get(slot["slot"])
        if previous is None or slot["attempt"] > previous["attempt"]:
            slots[slot["slot"]] = slot
    return slots


def budget_file(directory, model):
    return directory / ("pooled.json" if model is None else f"model-{model_slug(model)}.json")


def write_sets(directory, sets):
    directory.mkdir(parents=True, exist_ok=True)
    for model, (budget, _) in sets.items():
        write_json(budget_file(directory, model), budget)


def rule():
    return {"metrics": list(METRICS), "spread_multiplier": SPREAD_MULTIPLIER,
            "margin_floor": MARGIN_FLOOR, "margin_cap": MARGIN_CAP,
            "min_model_runs": MIN_MODEL_RUNS}


def write_index(directory, sets, sha):
    write_json(directory / "index.json", {
        "schema": INDEX_SCHEMA, "commit": sha, "classification": CLASSIFICATION,
        "calibration_run": f"https://github.com/{os.environ.get('GITHUB_REPOSITORY', '')}"
                           f"/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}",
        "rule": rule(), "fallback": budget_file(Path("."), None).name,
        "models": {model: budget_file(Path("."), model).name
                   for model in sets if model is not None},
        "derivation": "derivation.json"})


def evaluate(slots, profile, out, binary=BINARY):
    """Derive budgets and check them; returns the derivation report."""
    calibration = sorted((slot for slot in slots.values() if slot["kind"] == "calibration"),
                         key=lambda slot: slot["slot"])
    require(len(calibration) >= PROFILES[profile]["min_calibration"],
            f"only {len(calibration)} complete calibration slots")
    injected = [slot for slot in slots.values() if slot["kind"] == "injected"]
    require(len(injected) == 1, "missing the synthetic-regression slot")
    for slot in calibration:
        code, lines, stderr = run_compare([slot["path"]], binary=binary)
        require(code == 0, f"compare failed for {slot['slot']}: {stderr.strip()}")
        slot["medians"] = medians(lines)
    pairs = [(slot["cpu_model"], slot["medians"]) for slot in calibration]
    sets = budget_sets(pairs)
    write_sets(out / "budgets", sets)
    write_index(out / "budgets", sets, calibration[0]["commit"])

    # Out of sample: each run against budgets derived without it.
    held_out = []
    for index, slot in enumerate(calibration):
        others = pairs[:index] + pairs[index + 1:]
        loo = budget_sets(others)
        model = select(loo, slot["cpu_model"])
        path = budget_file(out / "leave-one-out" / slot["slot"], model)
        write_sets(path.parent, {model: loo[model]})
        result = check_budget(slot["path"], path, binary)
        held_out.append({"slot": slot["slot"], "cpu_model": slot["cpu_model"],
                         "budgets": model or "pooled", **result})

    target = injected[0]
    model = select(sets, target["cpu_model"])
    regression = check_budget(target["path"], budget_file(out / "budgets", model), binary)
    false_positives = [check["slot"] for check in held_out if not check["passed"]]
    report = {
        "schema": "alloy-bench-derivation/1", "classification": CLASSIFICATION,
        "profile": profile, "commit": calibration[0]["commit"], "rule": rule(),
        "calibration": [{key: slot[key] for key in ("slot", "run_id", "cpu_model", "raw_file",
                                                    "raw_sha256", "binary_sha256", "attempt")}
                        for slot in calibration],
        "cells": {model or "pooled": records for model, (_, records) in sets.items()},
        "leave_one_out": held_out,
        "false_positives": false_positives,
        "synthetic_regression": {"slot": target["slot"], "cpu_model": target["cpu_model"],
                                 "injected_service_ns": target["injected_service_ns"],
                                 "budgets": model or "pooled", **regression},
        # Smoke runs exercise the pipeline only, so held-out failures are not
        # held against them; the synthetic regression must still be caught.
        "accepted": (not regression["passed"]
                     and (profile == "smoke" or not false_positives)),
    }
    write_json(out / "budgets" / "derivation.json", report)
    return report


def collect(out, sha, profile_name, slot):
    profile = PROFILES[profile_name]
    kind = slot_kind(slot)
    require(kind != "gate" or profile_name == "full", "the gate runs the full matrix only")
    inject = profile["inject_ns"] if kind == "injected" else 0
    evidence = qualify.Evidence(out, sha)
    path, rows = evidence.matrix(
        slot, profile["scenarios"], profile["workloads"], profile["transports"],
        reps=profile["reps"], counting=kind == "allocations", timeout=profile["timeout"],
        collector=True, seconds=profile["seconds"], warmup=profile["warmup"],
        inject_ns=inject, label=CLASSIFICATION)
    write_json(out / "slot.json", {
        "schema": SLOT_SCHEMA, "slot": slot, "kind": kind, "profile": profile_name,
        "commit": sha, "run_id": rows[0]["run_id"],
        "attempt": int(os.environ["GITHUB_RUN_ATTEMPT"]), "cpu_model": run_model(rows),
        "raw_file": path.name, "raw_sha256": digest(path), "binary_sha256": digest(BINARY),
        "injected_service_ns": inject, "alloc_counting": kind == "allocations"})


def check(out):
    """Hold this job's fresh `gate` run to the committed budgets."""
    slot = json.loads((out / "slot.json").read_text())
    require(slot["kind"] == "gate", "only a gate slot is checked against committed budgets")
    index = json.loads((BUDGETS / "index.json").read_text())
    require(index.get("schema") == INDEX_SCHEMA, "unknown budget index")
    model = slot["cpu_model"]
    name = index["models"].get(model, index["fallback"])
    result = check_budget(out / slot["raw_file"], BUDGETS / name)
    verdict = {"schema": "alloy-bench-gate/1", "classification": CLASSIFICATION,
               "commit": slot["commit"], "cpu_model": model, "budget_file": name,
               "calibrated_model": model in index["models"],
               "budgets_commit": index["commit"], **result}
    write_json(out / "gate.json", verdict)
    return verdict


def summary(text):
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as file:
        file.write(text)


def main(command, *args):
    require(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_OS") == "Linux",
            "hosted CI only")
    out = Path(os.environ["QUALIFICATION_OUT"])
    sha = os.environ["QUALIFICATION_SHA"]
    profile = os.environ["GATE_PROFILE"]
    require(profile in PROFILES, f"unknown profile {profile!r}")
    require(profile != "full" or (os.environ.get("GITHUB_EVENT_NAME") in
                                  ("workflow_dispatch", "schedule")
                                  and os.environ.get("GITHUB_REF") == "refs/heads/main"),
            "the full matrix runs only on main")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip() == sha,
            "checkout differs from immutable source")
    out.mkdir(parents=True, exist_ok=True)
    if command == "collect":
        collect(out, sha, profile, os.environ["GATE_SLOT"])
        return 0
    if command == "check":
        verdict = check(out)
        summary(f"### Regression gate: {'pass' if verdict['passed'] else 'REGRESSION'}\n\n"
                f"Source `{sha}` on `{verdict['cpu_model']}`, checked against "
                f"`{verdict['budget_file']}` (budgets from `{verdict['budgets_commit']}`). "
                f"{len(verdict['exceeded'])} of {verdict['budgeted']} budgeted cells exceeded a "
                f"limit.\n")
        return 0 if verdict["passed"] else 1
    if command == "derive":
        (root,) = args
        report = evaluate(load_slots(Path(root), sha, profile), profile, out)
        regression = report["synthetic_regression"]
        summary(f"### Budget calibration ({profile}): "
                f"{'accepted' if report['accepted'] else 'REJECTED'}\n\n"
                f"Source `{sha}`, {len(report['calibration'])} unchanged-code runs. "
                f"Leave-one-out false positives: {len(report['false_positives'])}. "
                f"Synthetic regression of {regression['injected_service_ns']} ns/request: "
                f"{len(regression['exceeded'])} of {regression['budgeted']} budgeted cells exceeded "
                f"({'caught' if not regression['passed'] else 'NOT caught'}).\n")
        return 0 if report["accepted"] else 1
    raise ValueError(f"unknown command {command!r}")


if __name__ == "__main__":
    # Runner cancellation must unwind the active command's process-group cleanup.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    sys.exit(main(*sys.argv[1:]))
