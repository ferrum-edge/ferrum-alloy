"""Shared-runner benchmark regression gate. Never execute this on a local machine.

See docs/benchmarks.md#regression-gate. `collect` runs one matrix slot beside
the real Collector; `derive` turns unchanged-code calibration slots into
budgets per CPU model and checks them out of sample and against a synthetic
regression; `check` holds a fresh gate run to its CPU model's committed budgets,
and `verdict` combines the gate's slots. Every budget
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
# A model is published only if, after held-out screening, at least this
# fraction of its cell metrics keeps a limit.
COVERAGE_FLOOR = 0.5
# A CPU model gets budgets from at least this many calibration runs, so that
# each held-out check still derives its limits from four. Runs of other
# models are never held to them: ratios move more between models than within.
MIN_MODEL_RUNS = 5
# The smoke profile treats every runner as one model, to exercise the pipeline.
ANY_MODEL = "*"

PROFILES = {
    # The full matrix, at the policy's repetitions and window. Main only.
    # Every calibration slot then runs `control`, a subset with the synthetic
    # regression injected, on the same runner: the positive control for its
    # CPU model's budgets.
    "full": {"scenarios": qualify.ALL_SCENARIOS, "workloads": qualify.WORKLOADS,
             "transports": qualify.TRANSPORTS, "reps": 5, "seconds": 5, "warmup": 1,
             "timeout": 10800, "per_model": True, "coverage_floor": COVERAGE_FLOOR,
             "control": {"scenarios": ("plain", "alloy", "alloy-logs", "alloy-diagnostics"),
                         "workloads": ("small",), "transports": ("h1", "h2c"),
                         "inject_ns": 5000, "timeout": 900}},
    # Pull requests exercise the pipeline on a tiny matrix. Not evidence.
    "smoke": {"scenarios": ("plain", "alloy", "otel-collector"), "workloads": ("small",),
              "transports": ("h1", "h2c"), "reps": 3, "seconds": 1, "warmup": 0.5,
              "timeout": 600, "per_model": False, "coverage_floor": 0,
              "control": {"scenarios": ("plain", "alloy"), "workloads": ("small",),
                          "transports": ("h1",), "inject_ns": 50000, "timeout": 300}},
}
SLOT = re.compile(r"(calibration)-[0-9]{2}|(allocations)|(gate)-[0-9]{2}")


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

    Every budgeted cell must have been checked: a budget entry without data
    (which `compare` also fails on) is an error, never a verdict, even when
    another cell exceeded a limit.
    """
    code, lines, stderr = run_compare([path], budget, binary)
    over = exceeded(lines)
    entries = len(json.loads(Path(budget).read_text())["budgets"])
    budgeted = sum(1 for line in lines if line.get("budget") is not None)
    require(entries and budgeted == entries,
            f"{entries - budgeted} of {entries} budgeted cells have no data: {stderr.strip()}")
    require(code == 0 or over, f"compare failed without a budget verdict: {stderr.strip()}")
    return {"passed": code == 0, "exceeded": over, "budgeted": budgeted,
            "compare_stderr": stderr.strip()}


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


def budget_sets(calibration, per_model=True):
    """Budgets by CPU model, for each model with at least MIN_MODEL_RUNS runs.

    `calibration` is a list of (cpu_model, medians) pairs. Without
    `per_model`, every run counts as one model, ANY_MODEL.
    """
    if not per_model:
        calibration = [(ANY_MODEL, cells) for _, cells in calibration]
    sets = {}
    for model in sorted({model for model, _ in calibration}):
        runs = [cells for other, cells in calibration if other == model]
        if len(runs) >= MIN_MODEL_RUNS or not per_model:
            sets[model] = derive(runs)
    return sets


def select(sets, model):
    """The budget set a run on `model` is held to, or None: uncalibrated."""
    if model in sets:
        return model
    return ANY_MODEL if ANY_MODEL in sets else None


def cpuinfo_model(text):
    """The first `model name` of /proc/cpuinfo, as `alloy-bench` records it."""
    for line in text.splitlines():
        key, _, value = line.partition(":")
        if key.strip() == "model name":
            return value.strip()
    return None


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


def evidence_file(directory, name):
    require(isinstance(name, str) and name and Path(name).name == name,
            f"evidence file {name!r} is not a plain name")
    return directory / name


def load_slots(root, sha, profile):
    """Complete slots by name; a rerun's later attempt replaces an earlier one.

    Every field the derivation relies on is checked against the slot's own
    evidence, not trusted from `slot.json`.
    """
    control = PROFILES[profile]["control"]
    slots = {}
    for directory in sorted(path for path in root.iterdir() if path.is_dir()):
        if not (directory / "slot.json").is_file():
            continue  # A failed attempt's partial evidence; its rerun supplies the slot.
        verify_manifest(directory)
        slot = json.loads((directory / "slot.json").read_text())
        require(slot.get("schema") == SLOT_SCHEMA and slot.get("commit") == sha
                and slot.get("profile") == profile, f"foreign slot in {directory.name}")
        require(slot.get("kind") == slot_kind(slot.get("slot", "")),
                f"slot kind does not match its name in {directory.name}")
        require(type(slot.get("attempt")) is int and slot["attempt"] >= 1,
                f"invalid attempt in {directory.name}")
        raw = evidence_file(directory, slot.get("raw_file"))
        require(digest(raw) == slot["raw_sha256"], f"raw results changed in {directory.name}")
        rows = [json.loads(line) for line in raw.read_text().splitlines() if line.strip()]
        require(run_model(rows) == slot["cpu_model"], f"CPU model differs in {directory.name}")
        require({row.get("run_id") for row in rows} == {slot["run_id"]},
                f"run id differs in {directory.name}")
        slot["path"] = raw
        if slot["kind"] == "calibration":
            path = evidence_file(directory, slot.get("control_file"))
            require(digest(path) == slot["control_sha256"],
                    f"control results changed in {directory.name}")
            require(slot.get("control_injected_service_ns") == control["inject_ns"],
                    f"control injected another cost in {directory.name}")
            controls = [json.loads(line) for line in path.read_text().splitlines()
                        if line.strip()]
            require(run_model(controls) == slot["cpu_model"],
                    f"control ran on another CPU model in {directory.name}")
            slot["control_path"] = path
        previous = slots.get(slot["slot"])
        if previous is None or slot["attempt"] > previous["attempt"]:
            slots[slot["slot"]] = slot
    calibration = [slot for slot in slots.values() if slot["kind"] == "calibration"]
    require(len({slot["run_id"] for slot in calibration}) == len(calibration),
            "calibration slots share a run id")
    require(len({slot["binary_sha256"] for slot in calibration}) <= 1,
            "calibration slots ran different binaries")
    return slots


def budget_file(directory, model):
    name = "any-model" if model == ANY_MODEL else f"model-{model_slug(model)}"
    return directory / f"{name}.json"


def write_sets(directory, sets):
    directory.mkdir(parents=True, exist_ok=True)
    for model, (budget, _) in sets.items():
        write_json(budget_file(directory, model), budget)


def rule():
    return {"metrics": list(METRICS), "spread_multiplier": SPREAD_MULTIPLIER,
            "margin_floor": MARGIN_FLOOR, "margin_cap": MARGIN_CAP,
            "min_model_runs": MIN_MODEL_RUNS, "coverage_floor": COVERAGE_FLOOR,
            "held_out_screening": "a limit any held-out run exceeds is dropped"}


def write_index(directory, sets, sha):
    write_json(directory / "index.json", {
        "schema": INDEX_SCHEMA, "commit": sha, "classification": CLASSIFICATION,
        "calibration_run": f"https://github.com/{os.environ.get('GITHUB_REPOSITORY', '')}"
                           f"/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}",
        "rule": rule(),
        "models": {model: budget_file(Path("."), model).name for model in sets},
        "derivation": "derivation.json"})


def restrict(budget, cells):
    """The entries of a budget file for some cells only."""
    return {"schema": budget["schema"],
            "budgets": [entry for entry in budget["budgets"]
                        if (entry["scenario"], entry["workload"], entry["transport"]) in cells]}


def control_cells(profile):
    control = PROFILES[profile]["control"]
    return {(scenario, workload, transport) for scenario in control["scenarios"]
            if scenario != "plain" for workload in control["workloads"]
            for transport in control["transports"]}


def screened(budget, records, failures):
    """`budget` without the limits screening dropped, and how many it removed.

    `failures` maps (scenario, workload, transport, budget key) to the reason;
    a failure of a limit the full budget never had removes nothing.
    """
    keys = {key: metric for metric, (key, _) in METRICS.items()}
    entries, removed = [], 0
    for entry in budget["budgets"]:
        cell = (entry["scenario"], entry["workload"], entry["transport"])
        kept = {key: value for key, value in entry.items()
                if key not in keys or cell + (key,) not in failures}
        removed += len(entry) - len(kept)
        if any(key in kept for key in keys):
            entries.append(kept)
    for record in records:
        cell = (record["scenario"], record["workload"], record["transport"])
        for key, metric in keys.items():
            info = record["metrics"][metric]
            if cell + (key,) in failures and info.get("gated"):
                info.update(gated=False, reason=failures[cell + (key,)])
                info.pop("limit", None)
    return {"schema": budget["schema"], "budgets": entries}, removed


def limit_keys(budget):
    keys = {key for key, _ in METRICS.values()}
    return {(entry["scenario"], entry["workload"], entry["transport"], key)
            for entry in budget["budgets"] for key in entry if key in keys}


def coverage(records):
    gated = sum(1 for record in records for info in record["metrics"].values()
                if info.get("gated"))
    total = len(records) * len(METRICS)
    return {"gated": gated, "total": total, "fraction": gated / total if total else 0.0}


def evaluate(slots, profile, out, binary=BINARY, planned=()):
    """Derive budgets and qualify each CPU model's; returns the derivation report.

    For each model with MIN_MODEL_RUNS calibration runs, in order:
    1. derive its limits from all its runs;
    2. hold out each run against limits derived from the model's other runs,
       and drop every limit a held-out run exceeds (screening, recorded);
    3. require COVERAGE_FLOOR of its cell metrics to keep a limit;
    4. require every one of its runs' positive controls (the synthetic
       regression on the same runner) to exceed the screened limits.
    A model passing all of these is published; the calibration is accepted
    when at least one is.
    """
    settings = PROFILES[profile]
    calibration = sorted((slot for slot in slots.values() if slot["kind"] == "calibration"),
                         key=lambda slot: slot["slot"])
    require(calibration, "missing calibration slots")
    for slot in calibration:
        code, lines, stderr = run_compare([slot["path"]], binary=binary)
        require(code == 0, f"compare failed for {slot['slot']}: {stderr.strip()}")
        slot["medians"] = medians(lines)
    pairs = [(slot["cpu_model"], slot["medians"]) for slot in calibration]
    candidates = budget_sets(pairs, settings["per_model"])
    require(candidates, f"no CPU model has {MIN_MODEL_RUNS} calibration runs")
    cells = control_cells(profile)

    models, sets = {}, {}
    for model, (budget, records) in candidates.items():
        runs = [index for index, slot in enumerate(calibration)
                if model in (ANY_MODEL, slot["cpu_model"])]
        held_out, failures, tested = [], {}, set()
        for index in runs:
            slot = calibration[index]
            others = [pairs[other][1] for other in runs if other != index]
            loo = derive(others)
            if not loo[0]["budgets"]:
                held_out.append({"slot": slot["slot"], "passed": None,
                                 "reason": "no limit derived without this run"})
                continue
            path = budget_file(out / "leave-one-out" / slot["slot"], model)
            write_sets(path.parent, {model: loo})
            result = check_budget(slot["path"], path, binary)
            held_out.append({"slot": slot["slot"], **result})
            tested.update(limit_keys(loo[0]))
            for line in result["exceeded"]:
                cell = (line["scenario"], line["workload"], line["transport"])
                failures.update(dict.fromkeys((cell + (key,) for key in line["exceeded"]),
                                              "failed a held-out check"))
        # A limit no fold derived (its spread crossed the cap without some
        # run) was never held out, so it is not kept as if screened.
        exceeded_limits = len(failures)
        untested = limit_keys(budget) - tested - set(failures)
        failures.update(dict.fromkeys(untested, "no held-out run tested it"))
        kept, removed = screened(budget, records, failures)
        covered = coverage(records)
        state = {"runs": [calibration[index]["slot"] for index in runs],
                 "leave_one_out": held_out, "screened_limits": removed,
                 "held_out_failures": exceeded_limits, "untested_limits": len(untested),
                 "coverage": covered, "positive_controls": []}
        models[model] = state
        if covered["fraction"] < settings["coverage_floor"]:
            state["reason"] = "coverage below the floor after screening"
            continue
        subset = restrict(kept, cells)
        path = budget_file(out / "controls", model)
        write_sets(path.parent, {model: (subset, None)})
        for index in runs:
            slot = calibration[index]
            entry = {"slot": slot["slot"],
                     "injected_service_ns": slot["control_injected_service_ns"]}
            if subset["budgets"]:
                entry.update(check_budget(slot["control_path"], path, binary))
            else:
                entry.update(passed=None, reason="no control cell is budgeted")
            state["positive_controls"].append(entry)
        if not all(entry["passed"] is False for entry in state["positive_controls"]):
            state["reason"] = "a positive control was not caught"
            continue
        sets[model] = (kept, records)
    for model, state in models.items():
        state["published"] = model in sets
    write_sets(out / "budgets", sets)
    write_index(out / "budgets", sets, calibration[0]["commit"])
    report = {
        "schema": "alloy-bench-derivation/1", "classification": CLASSIFICATION,
        "profile": profile, "commit": calibration[0]["commit"], "rule": rule(),
        "planned_slots": list(planned),
        "missing_slots": sorted(set(planned) - set(slots)),
        "calibration": [{key: slot[key] for key in (
            "slot", "run_id", "cpu_model", "raw_file", "raw_sha256", "control_file",
            "control_sha256", "binary_sha256", "attempt")} for slot in calibration],
        "uncalibrated_runs": [slot["slot"] for slot in calibration
                              if select(candidates, slot["cpu_model"]) is None],
        "models": models,
        "published": sorted(sets),
        "cells": {model: records for model, (_, records) in candidates.items()},
        "accepted": bool(sets),
    }
    write_json(out / "budgets" / "derivation.json", report)
    return report


def committed_index():
    path = BUDGETS / "index.json"
    require(path.is_file(), f"no committed budgets: {path} is missing; run calibrate first")
    index = json.loads(path.read_text())
    require(index.get("schema") == INDEX_SCHEMA, "unknown budget index")
    return index


def collect(out, sha, profile_name, slot, cpuinfo=Path("/proc/cpuinfo")):
    """Run one slot. A gate slot on a CPU model without budgets runs nothing."""
    profile = PROFILES[profile_name]
    kind = slot_kind(slot)
    require(kind != "gate" or profile_name == "full", "the gate runs the full matrix only")
    record = {"schema": SLOT_SCHEMA, "slot": slot, "kind": kind, "profile": profile_name,
              "commit": sha, "attempt": int(os.environ["GITHUB_RUN_ATTEMPT"])}
    if kind == "gate":
        model = cpuinfo_model(cpuinfo.read_text())
        if model not in committed_index()["models"]:
            write_json(out / "slot.json", {**record, "cpu_model": model, "calibrated": False})
            return False
    evidence = qualify.Evidence(out, sha)
    path, rows = evidence.matrix(
        slot, profile["scenarios"], profile["workloads"], profile["transports"],
        reps=profile["reps"], counting=kind == "allocations", timeout=profile["timeout"],
        collector=True, seconds=profile["seconds"], warmup=profile["warmup"],
        label=CLASSIFICATION)
    model = run_model(rows)
    record.update(run_id=rows[0]["run_id"], cpu_model=model, calibrated=True,
                  raw_file=path.name, raw_sha256=digest(path), binary_sha256=digest(BINARY),
                  alloc_counting=kind == "allocations")
    if kind == "calibration":
        control = profile["control"]
        control_path, control_rows = evidence.matrix(
            f"{slot}-control", control["scenarios"], control["workloads"],
            control["transports"], reps=profile["reps"], timeout=control["timeout"],
            seconds=profile["seconds"], warmup=profile["warmup"],
            inject_ns=control["inject_ns"], label=CLASSIFICATION)
        require(run_model(control_rows) == model, "the control ran on another CPU model")
        record.update(control_file=control_path.name, control_sha256=digest(control_path),
                      control_injected_service_ns=control["inject_ns"])
    write_json(out / "slot.json", record)
    return True


def check(out):
    """Hold this job's fresh gate run to its CPU model's committed budgets."""
    slot = json.loads((out / "slot.json").read_text())
    require(slot["kind"] == "gate", "only a gate slot is checked against committed budgets")
    index = committed_index()
    verdict = {"schema": "alloy-bench-gate/1", "classification": CLASSIFICATION,
               "slot": slot["slot"], "attempt": slot["attempt"], "commit": slot["commit"],
               "cpu_model": slot["cpu_model"],
               "budgets_commit": index["commit"]}
    if not slot["calibrated"]:
        verdict.update(passed=None, reason="uncalibrated CPU model: no run")
    else:
        name = index["models"][slot["cpu_model"]]
        verdict.update(budget_file=name, **check_budget(out / slot["raw_file"], BUDGETS / name))
    write_json(out / "gate.json", verdict)
    return verdict


def verdict(root, planned, collected, sha, run_id, attempt):
    """The gate passes when every slot job of this attempt succeeded, every
    planned slot has this attempt's verdict, at least one ran on a calibrated
    model, and none regressed.

    Only artifacts of the current run attempt count
    (`bench-gate-<slot>-<sha>-<run id>-<attempt>`), so a verdict can never
    combine attempts: a slot that a partial rerun did not repeat, or whose
    repeated attempt left no verdict or no artifact at all, is missing. Rerun
    all jobs, or dispatch again, to retry a gate.
    """
    name = re.compile(rf"bench-gate-(.+)-{re.escape(sha)}-{re.escape(str(run_id))}-([0-9]+)")
    latest = {}
    for directory in sorted(path for path in root.iterdir() if path.is_dir()):
        match = name.fullmatch(directory.name)
        require(match is not None, f"unexpected artifact {directory.name}")
        path = directory / "gate.json"
        if int(match[2]) != attempt or not path.is_file():
            continue  # Another attempt's, or this attempt failed before checking.
        verify_manifest(directory)
        entry = json.loads(path.read_text())
        require(entry["slot"] == match[1] and entry["attempt"] == attempt,
                f"verdict does not match its artifact {directory.name}")
        latest[entry["slot"]] = entry
    verdicts = [latest[slot] for slot in sorted(latest)]
    checked = [entry for entry in verdicts if entry["passed"] is not None]
    missing = sorted(set(planned) - set(latest))
    return {"verdicts": verdicts, "checked": len(checked), "missing": missing,
            "collected": collected, "attempt": attempt,
            "passed": (collected == "success" and bool(checked) and not missing
                       and all(entry["passed"] for entry in checked))}


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
        result = check(out)
        if result["passed"] is None:
            summary(f"### Regression gate slot: no run\n\n`{result['cpu_model']}` has no "
                    "committed budgets, so this slot ran nothing.\n")
            return 0
        summary(f"### Regression gate slot: {'pass' if result['passed'] else 'REGRESSION'}\n\n"
                f"Source `{sha}` on `{result['cpu_model']}`, checked against "
                f"`{result['budget_file']}` (budgets from `{result['budgets_commit']}`). "
                f"{len(result['exceeded'])} of {result['budgeted']} budgeted cells exceeded "
                "a limit.\n")
        return 0 if result["passed"] else 1
    if command == "verdict":
        (root,) = args
        planned = json.loads(os.environ["GATE_SLOTS"])
        result = verdict(Path(root), planned, os.environ["GATE_COLLECT_RESULT"], sha,
                         os.environ["GITHUB_RUN_ID"], int(os.environ["GITHUB_RUN_ATTEMPT"]))
        write_json(out / "verdict.json", result)
        summary(f"### Regression gate: {'pass' if result['passed'] else 'FAIL'}\n\n"
                f"{result['checked']} of {len(planned)} slots ran on a calibrated CPU model; "
                f"slots without a verdict in attempt {result['attempt']}: "
                f"{', '.join(result['missing']) or 'none'}; slot jobs: "
                f"{result['collected']}. The gate needs every slot job to succeed with a "
                "verdict, at least one calibrated run, and no regression.\n")
        return 0 if result["passed"] else 1
    if command == "derive":
        (root,) = args
        planned = json.loads(os.environ.get("GATE_SLOTS", "[]"))
        report = evaluate(load_slots(Path(root), sha, profile), profile, out, planned=planned)
        lines = [f"### Budget calibration ({profile}): "
                 f"{'accepted' if report['accepted'] else 'REJECTED'}\n",
                 f"Source `{sha}`, {len(report['calibration'])} unchanged-code runs; "
                 f"missing slots: {', '.join(report['missing_slots']) or 'none'}.\n",
                 "| CPU model | Runs | Limits dropped by held-out runs | Limits kept | "
                 "Controls caught | Published |", "|---|---:|---:|---:|---:|---|"]
        for model, state in report["models"].items():
            controls = state["positive_controls"]
            caught = sum(1 for entry in controls if entry["passed"] is False)
            lines.append(f"| {model} | {len(state['runs'])} | {state['screened_limits']} | "
                         f"{state['coverage']['gated']} of {state['coverage']['total']} | "
                         f"{caught} of {len(controls)} | "
                         f"{'yes' if state['published'] else state['reason']} |")
        summary("\n".join(lines) + "\n")
        # A smoke run draws a few runners of whatever CPU models GitHub
        # assigns, so its statistics are not evidence: it must complete, and
        # its acceptance is reported, not required.
        return 0 if report["accepted"] or profile == "smoke" else 1
    raise ValueError(f"unknown command {command!r}")


if __name__ == "__main__":
    # Runner cancellation must unwind the active command's process-group cleanup.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    sys.exit(main(*sys.argv[1:]))
