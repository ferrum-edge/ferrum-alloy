"""Hosted-only tests for budget derivation and the regression gate."""

import json
import math
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import gate
from qualify import digest


CELLS = [("alloy", "small", "h1"), ("alloy", "small", "h2c"), ("otel-collector", "small", "h1")]


def compare_line(run_id, cell, throughput, p50=1.5, p99=2.0):
    scenario, workload, transport = cell
    summary = lambda value: None if value is None else {
        "n": 5, "median": value, "min": value, "max": value}
    return {"schema": "alloy-bench-compare/1", "run_id": run_id, "commit": "head",
            "alloc_counting": False, "scenario": scenario, "workload": workload,
            "transport": transport, "pairs": 5, "reps": [1, 2, 3, 4, 5],
            "throughput_ratio": summary(throughput), "latency_p50_ratio": summary(p50),
            "latency_p99_ratio": summary(p99), "plain_requests_per_second": summary(1e5),
            "budget": None}


def fake_compare(paths, budget=None, binary=None):
    """`alloy-bench compare` over files that already hold compare lines."""
    lines = [json.loads(line) for path in paths
             for line in Path(path).read_text().splitlines()]
    if budget is None:
        return 0, lines, ""
    budgets = json.loads(Path(budget).read_text())["budgets"]
    failed = False
    for line in lines:
        for entry in budgets:
            if (entry["scenario"], entry["workload"], entry["transport"]) != (
                    line["scenario"], line["workload"], line["transport"]):
                continue
            over = []
            if "min_throughput_ratio" in entry and \
                    line["throughput_ratio"]["median"] < entry["min_throughput_ratio"]:
                over.append("min_throughput_ratio")
            for metric, key in (("latency_p50_ratio", "max_latency_p50_ratio"),
                                ("latency_p99_ratio", "max_latency_p99_ratio")):
                if key in entry and (line[metric] is None
                                     or line[metric]["median"] > entry[key]):
                    over.append(key)
            line["budget"] = {"exceeded": over}
            failed = failed or bool(over)
    return (1 if failed else 0), lines, ""


def run_cells(throughputs):
    return {cell: {"throughput_ratio": value, "latency_p50_ratio": 1.5,
                   "latency_p99_ratio": 2.0} for cell, value in zip(CELLS, throughputs)}


class DerivationTests(unittest.TestCase):
    def test_limits_sit_a_noise_margin_beyond_the_median_run_and_round_outward(self):
        runs = [run_cells([value] * 3) for value in (0.60, 0.61, 0.62)]
        budget, records = gate.derive(runs)
        self.assertEqual(budget["schema"], "alloy-bench-budget/1")
        info = records[0]["metrics"]["throughput_ratio"]
        spread = math.log(0.61 / 0.60)
        self.assertAlmostEqual(info["center"], 0.61)
        self.assertAlmostEqual(info["spread"], spread)
        margin = gate.SPREAD_MULTIPLIER * spread + gate.MARGIN_FLOOR
        self.assertAlmostEqual(info["margin"], margin)
        expected = math.floor(0.61 * math.exp(-margin) * 10_000) / 10_000
        self.assertEqual(budget["budgets"][0]["min_throughput_ratio"], expected)
        self.assertLessEqual(expected, 0.61 * math.exp(-margin))
        # Latency limits are maxima and round up; identical runs leave the floor.
        p50 = budget["budgets"][0]["max_latency_p50_ratio"]
        self.assertEqual(p50, math.ceil(1.5 * math.exp(gate.MARGIN_FLOOR) * 10_000) / 10_000)
        self.assertGreaterEqual(p50, 1.5 * math.exp(gate.MARGIN_FLOOR))
        self.assertIs(budget["budgets"][0]["alloc_counting"], False)

    def test_noisy_or_unknown_metrics_are_left_unbudgeted_not_loosened(self):
        runs = [run_cells([0.6, 0.6, 0.6]) for _ in range(3)]
        runs[0][CELLS[1]]["throughput_ratio"] = 0.3
        runs[1][CELLS[2]]["latency_p99_ratio"] = None
        budget, records = gate.derive(runs)
        by_cell = {(entry["scenario"], entry["workload"], entry["transport"]): entry
                   for entry in budget["budgets"]}
        self.assertNotIn("min_throughput_ratio", by_cell[CELLS[1]])
        self.assertIn("max_latency_p50_ratio", by_cell[CELLS[1]])
        self.assertNotIn("max_latency_p99_ratio", by_cell[CELLS[2]])
        self.assertEqual(records[1]["metrics"]["throughput_ratio"]["reason"],
                         "unchanged-code noise exceeds the cap")
        self.assertEqual(records[2]["metrics"]["latency_p99_ratio"]["reason"],
                         "unknown in a run")

    def test_runs_must_cover_the_same_cells_and_be_more_than_one(self):
        with self.assertRaises(ValueError):
            gate.derive([run_cells([0.6] * 3)])
        short = run_cells([0.6] * 3)
        del short[CELLS[0]]
        with self.assertRaises(ValueError):
            gate.derive([run_cells([0.6] * 3), short])

    def test_only_well_sampled_models_get_budgets_and_others_are_uncalibrated(self):
        calibration = [("A", run_cells([0.66] * 3))] * 5 + [("B", run_cells([0.61] * 3))] * 4
        sets = gate.budget_sets(calibration)
        self.assertEqual(set(sets), {"A"})
        self.assertEqual(gate.select(sets, "A"), "A")
        self.assertIsNone(gate.select(sets, "B"))
        self.assertIsNone(gate.select(sets, "never seen"))
        # The smoke profile treats every runner as one model.
        pooled = gate.budget_sets(calibration, per_model=False)
        self.assertEqual(set(pooled), {gate.ANY_MODEL})
        self.assertEqual(gate.select(pooled, "B"), gate.ANY_MODEL)
        self.assertEqual(gate.budget_file(Path("."), gate.ANY_MODEL).name, "any-model.json")

    def test_cpu_model_matches_what_the_harness_records(self):
        text = "processor\t: 0\nvendor_id\t: x\nmodel name\t: AMD EPYC 7763 64-Core Processor\n" \
               "processor\t: 1\nmodel name\t: other\n"
        self.assertEqual(gate.cpuinfo_model(text), "AMD EPYC 7763 64-Core Processor")
        self.assertIsNone(gate.cpuinfo_model("processor\t: 0\n"))

    def test_medians_come_from_one_uncounted_invocation(self):
        lines = [compare_line("r", cell, 0.6) for cell in CELLS]
        cells = gate.medians(lines)
        self.assertEqual(cells[CELLS[0]]["throughput_ratio"], 0.6)
        lines[2]["latency_p99_ratio"] = None
        self.assertIsNone(gate.medians(lines)[CELLS[2]]["latency_p99_ratio"])
        for broken in ([], lines[:1] + [compare_line("other", CELLS[1], 0.6)],
                       lines[:1] + lines[:1],
                       [dict(lines[0], alloc_counting=True)]):
            with self.subTest(broken=broken), self.assertRaises(ValueError):
                gate.medians(broken)

    def test_slot_names_and_model_slugs(self):
        self.assertEqual(gate.slot_kind("calibration-07"), "calibration")
        self.assertEqual(gate.slot_kind("gate-03"), "gate")
        self.assertEqual(gate.slot_kind("allocations"), "allocations")
        for slot in ("calibration-7", "calibration", "gate", "injected-01", "gate-1", ""):
            with self.subTest(slot=slot), self.assertRaises(ValueError):
                gate.slot_kind(slot)
        self.assertEqual(gate.model_slug("Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz"),
                         "intel-r-xeon-r-platinum-8370c-cpu-2-80ghz")
        with self.assertRaises(ValueError):
            gate.model_slug("()")


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.run = self.root / "run.jsonl"
        self.run.write_text("".join(json.dumps(compare_line("r", cell, 0.6)) + "\n"
                                    for cell in CELLS))
        self.budget = self.root / "budget.json"

    def tearDown(self):
        self.directory.cleanup()

    def write_budget(self, minimum):
        entries = [{"scenario": s, "workload": w, "transport": t, "alloc_counting": False,
                    "min_throughput_ratio": minimum} for s, w, t in CELLS]
        self.budget.write_text(json.dumps({"schema": "alloy-bench-budget/1",
                                           "budgets": entries}))

    def test_passes_within_and_fails_beyond_limits_through_compare(self):
        with patch("gate.run_compare", fake_compare):
            self.write_budget(0.5)
            self.assertEqual(gate.check_budget(self.run, self.budget),
                             {"passed": True, "exceeded": [], "budgeted": 3})
            self.write_budget(0.7)
            result = gate.check_budget(self.run, self.budget)
        self.assertFalse(result["passed"])
        self.assertEqual(len(result["exceeded"]), 3)
        self.assertEqual(result["exceeded"][0]["exceeded"], ["min_throughput_ratio"])

    def test_a_compare_error_or_an_unbudgeted_run_is_not_a_verdict(self):
        failing = lambda *args, **kwargs: (1, [], "budget matched no paired line")
        with patch("gate.run_compare", failing), self.assertRaisesRegex(ValueError, "verdict"):
            gate.check_budget(self.run, self.budget)
        unbudgeted = lambda *args, **kwargs: (0, [compare_line("r", CELLS[0], 0.6)], "")
        with patch("gate.run_compare", unbudgeted), self.assertRaisesRegex(ValueError, "budgeted"):
            gate.check_budget(self.run, self.budget)

    def gate_budgets(self):
        budgets = self.root / "budgets"
        budgets.mkdir()
        self.write_budget(0.7)
        (budgets / "model-a.json").write_text(self.budget.read_text())
        self.write_budget(0.5)
        (budgets / "model-b.json").write_text(self.budget.read_text())
        (budgets / "index.json").write_text(json.dumps({
            "schema": gate.INDEX_SCHEMA, "commit": "calibrated",
            "models": {"A": "model-a.json", "B": "model-b.json"}}))
        return budgets

    def test_gate_slots_check_their_models_budgets_and_skip_uncalibrated_models(self):
        budgets = self.gate_budgets()
        verdicts = {}
        for model in ("A", "B", "C"):
            out = self.root / f"bench-gate-{model}"
            out.mkdir()
            (out / "run.jsonl").write_text(self.run.read_text())
            (out / "slot.json").write_text(json.dumps({
                "slot": f"gate-{model}", "attempt": 1, "kind": "gate", "commit": "head",
                "cpu_model": model,
                "calibrated": model != "C", "raw_file": "run.jsonl"}))
            with patch("gate.run_compare", fake_compare), patch("gate.BUDGETS", budgets):
                verdicts[model] = gate.check(out)
            (out / "SHA256SUMS").write_text("".join(
                f"{digest(path)}  ./{path.name}\n" for path in sorted(out.iterdir())))
        self.assertFalse(verdicts["A"]["passed"])
        self.assertEqual(verdicts["A"]["budget_file"], "model-a.json")
        self.assertTrue(verdicts["B"]["passed"])
        self.assertIsNone(verdicts["C"]["passed"])
        self.assertNotIn("budget_file", verdicts["C"])
        overall = gate.verdict(self.root)
        self.assertEqual(overall["checked"], 2)
        self.assertFalse(overall["passed"])
        (self.root / "bench-gate-A" / "gate.json").write_text('{"passed": true}')
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            gate.verdict(self.root)

    def test_verdict_needs_a_calibrated_slot(self):
        for name, passed in (("one", None), ("two", True)):
            directory = self.root / name
            directory.mkdir()
            (directory / "gate.json").write_text(
                json.dumps({"slot": name, "attempt": 1, "passed": passed}))
            (directory / "SHA256SUMS").write_text(
                f"{digest(directory / 'gate.json')}  ./gate.json\n")
            result = gate.verdict(self.root)
        self.assertTrue(result["passed"])
        # A rerun attempt of slot "two" that landed on an uncalibrated model.
        rerun = self.root / "two-rerun"
        rerun.mkdir()
        (rerun / "gate.json").write_text(json.dumps({"slot": "two", "attempt": 2, "passed": None}))
        (rerun / "SHA256SUMS").write_text(f"{digest(rerun / 'gate.json')}  ./gate.json\n")
        result = gate.verdict(self.root)
        self.assertEqual([entry["attempt"] for entry in result["verdicts"]], [1, 2])
        self.assertFalse(result["passed"])

    def test_gate_collect_runs_nothing_on_an_uncalibrated_model(self):
        budgets = self.gate_budgets()
        cpuinfo = self.root / "cpuinfo"
        cpuinfo.write_text("model name\t: C\n")
        out = self.root / "out"
        out.mkdir()
        with patch("gate.BUDGETS", budgets), \
             patch.dict("gate.os.environ", {"GITHUB_RUN_ATTEMPT": "1"}), \
             patch("gate.qualify.Evidence") as evidence:
            self.assertFalse(gate.collect(out, "head", "full", "gate-01", cpuinfo))
        evidence.assert_not_called()
        slot = json.loads((out / "slot.json").read_text())
        self.assertEqual((slot["cpu_model"], slot["calibrated"]), ("C", False))
        with self.assertRaisesRegex(ValueError, "full matrix"):
            gate.collect(out, "head", "smoke", "gate-01", cpuinfo)


def write_slot(root, slot, throughputs, model="A", attempt=1, profile="full", control=0.45):
    """A calibration slot: its compare lines, and its positive control's."""
    directory = root / f"bench-gate-{slot}-{attempt}"
    directory.mkdir()
    raw = directory / f"{slot}.jsonl"
    raw.write_text("".join(json.dumps(compare_line(f"{slot}-{attempt}", cell, value)) + "\n"
                           for cell, value in zip(CELLS, throughputs)))
    controls = directory / f"{slot}-control.jsonl"
    controls.write_text("".join(json.dumps(compare_line(f"{slot}-c", cell, control)) + "\n"
                                for cell in CELLS))
    (directory / "slot.json").write_text(json.dumps({
        "schema": gate.SLOT_SCHEMA, "slot": slot, "kind": gate.slot_kind(slot),
        "profile": profile, "commit": "head", "run_id": f"{slot}-{attempt}",
        "attempt": attempt, "cpu_model": model, "raw_file": raw.name,
        "raw_sha256": digest(raw), "control_file": controls.name,
        "control_sha256": digest(controls), "control_injected_service_ns": 5000,
        "binary_sha256": "binary", "alloc_counting": False}))
    (directory / "SHA256SUMS").write_text("".join(
        f"{digest(path)}  ./{path.name}\n" for path in sorted(directory.iterdir())
        if path.name != "SHA256SUMS"))
    return directory


class EvaluationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.slots = self.root / "slots"
        self.slots.mkdir()
        self.out = self.root / "out"

    def tearDown(self):
        self.directory.cleanup()

    def evaluate(self, profile="full"):
        with patch("gate.run_compare", fake_compare):
            return gate.evaluate(gate.load_slots(self.slots, "head", profile), profile, self.out)

    def calibrate(self, values, profile="full", model="A", first=1, control=0.45):
        for index, value in enumerate(values, first):
            write_slot(self.slots, f"calibration-{index:02d}", [value] * 3, model=model,
                       profile=profile, control=control)

    def test_publishes_a_quiet_model_whose_controls_catch_the_synthetic_regression(self):
        self.calibrate([0.600, 0.603, 0.597, 0.601, 0.599, 0.602])
        self.calibrate([0.65, 0.66], model="B", first=7)
        report = self.evaluate()
        self.assertTrue(report["accepted"])
        self.assertEqual((report["candidates"], report["published"]), (["A"], ["A"]))
        self.assertEqual(report["false_positives"], [])
        held_out = {check["slot"]: check for check in report["leave_one_out"]}
        self.assertEqual([held_out[f"calibration-{i:02d}"]["budgets"] for i in range(1, 7)],
                         ["A"] * 6)
        self.assertIsNone(held_out["calibration-07"]["passed"])
        controls = report["positive_controls"]
        self.assertEqual(len(controls), 6)  # Only model A's runs; B is not a candidate.
        for control in controls:
            # Only the control's cells are budgeted for it: alloy small h1 and h2c.
            self.assertEqual((control["passed"], control["budgeted"]), (False, 2))
        index = json.loads((self.out / "budgets" / "index.json").read_text())
        self.assertEqual(index["models"], {"A": "model-a.json"})
        self.assertEqual(index["rule"]["min_model_runs"], gate.MIN_MODEL_RUNS)
        for name in ("model-a.json", "derivation.json"):
            self.assertTrue((self.out / "budgets" / name).is_file(), name)
        self.assertFalse((self.out / "budgets" / "model-b.json").exists())

    def test_a_model_whose_budgets_miss_one_control_is_not_published(self):
        self.calibrate([0.600, 0.601, 0.600, 0.601, 0.600])
        self.calibrate([0.700, 0.701, 0.700, 0.701], model="B", first=6)
        write_slot(self.slots, "calibration-10", [0.700] * 3, model="B", control=0.69)
        report = self.evaluate()
        self.assertEqual(report["candidates"], ["A", "B"])
        self.assertEqual(report["published"], ["A"])
        self.assertTrue(report["accepted"])
        missed = [c for c in report["positive_controls"] if c["passed"]]
        self.assertEqual([c["slot"] for c in missed], ["calibration-10"])
        # B's runs are not held out against budgets that were never published.
        held_out = {check["slot"]: check for check in report["leave_one_out"]}
        self.assertIsNone(held_out["calibration-06"]["passed"])
        self.assertEqual(json.loads((self.out / "budgets" / "index.json").read_text())["models"],
                         {"A": "model-a.json"})

    def test_held_out_limits_come_only_from_the_same_models_runs(self):
        self.calibrate([0.600, 0.601, 0.600, 0.601, 0.600])
        self.calibrate([0.700, 0.701, 0.700, 0.701, 0.700], model="B", first=6, control=0.5)
        report = self.evaluate()
        self.assertEqual(report["false_positives"], [])
        self.assertEqual(report["published"], ["A", "B"])

    def test_rejects_a_held_out_false_positive(self):
        self.calibrate([0.600, 0.601, 0.600, 0.601, 0.600, 0.585])
        report = self.evaluate()
        self.assertFalse(report["accepted"])
        self.assertEqual(report["false_positives"], ["calibration-06"])

    def test_rejects_when_no_model_catches_the_synthetic_regression(self):
        self.calibrate([0.6] * 5, control=0.6)
        report = self.evaluate()
        self.assertEqual(report["published"], [])
        self.assertFalse(report["accepted"])

    def test_a_control_without_budgeted_cells_cannot_publish_its_model(self):
        values = [0.600, 0.601, 0.600, 0.601, 0.600]
        for index, value in enumerate(values, 1):
            directory = write_slot(self.slots, f"calibration-{index:02d}", [value] * 3)
        with patch("gate.control_cells", lambda profile: {("alloy", "large", "h1")}):
            report = self.evaluate()
        self.assertEqual(report["positive_controls"][0]["reason"], "no control cell is budgeted")
        self.assertFalse(report["accepted"])
        self.assertTrue(directory.is_dir())

    def test_smoke_pools_models_and_does_not_require_held_out_passes(self):
        self.calibrate([0.600, 0.601], profile="smoke")
        self.calibrate([0.585], profile="smoke", model="B", first=3, control=0.3)
        report = self.evaluate("smoke")
        self.assertTrue(report["false_positives"])
        self.assertEqual(report["published"], [gate.ANY_MODEL])
        self.assertTrue(report["accepted"])

    def test_needs_a_model_with_enough_runs_and_one_profile(self):
        self.calibrate([0.6] * 4)
        with self.assertRaisesRegex(ValueError, "calibration runs"):
            self.evaluate()
        write_slot(self.slots, "calibration-06", [0.6] * 3, profile="smoke")
        with self.assertRaisesRegex(ValueError, "foreign"):
            self.evaluate()  # Mixed profiles are foreign evidence.
        with patch("gate.run_compare", fake_compare), self.assertRaisesRegex(ValueError, "slots"):
            gate.evaluate({}, "full", self.out)

    def test_rerun_attempts_replace_earlier_ones_and_partial_evidence_is_ignored(self):
        self.calibrate([0.6] * 6)
        write_slot(self.slots, "calibration-07", [0.6] * 3)
        write_slot(self.slots, "calibration-07", [0.5] * 3, attempt=2)
        (self.slots / "bench-gate-calibration-01-2").mkdir()  # failed, no slot.json
        slots = gate.load_slots(self.slots, "head", "full")
        self.assertEqual(slots["calibration-07"]["attempt"], 2)
        self.assertEqual(slots["calibration-01"]["attempt"], 1)

    def test_calibration_collects_the_matrix_then_its_control_on_the_same_runner(self):
        calls = []

        class Evidence:
            def __init__(self, out, sha):
                self.out = out

            def matrix(self, name, scenarios, workloads, transports, **options):
                calls.append((name, scenarios, options))
                path = self.out / f"{name}.jsonl"
                path.write_text("rows\n")
                model = "B" if name.endswith("control") and len(calls) > 2 else "A"
                rows = [{"run_id": f"run-{name}", "environment": {"cpu_model": model}}]
                return path, rows

        out = self.root / "collect"
        out.mkdir()
        with patch("gate.qualify.Evidence", Evidence), patch("gate.digest", lambda path: "h"), \
             patch.dict("gate.os.environ", {"GITHUB_RUN_ATTEMPT": "2"}):
            self.assertTrue(gate.collect(out, "head", "full", "calibration-01"))
            slot = json.loads((out / "slot.json").read_text())
            with self.assertRaisesRegex(ValueError, "another CPU model"):
                gate.collect(out, "head", "full", "calibration-02")
        (name, scenarios, options), (control, control_scenarios, control_options) = calls[:2]
        self.assertEqual((name, control), ("calibration-01", "calibration-01-control"))
        self.assertEqual(scenarios, gate.PROFILES["full"]["scenarios"])
        self.assertTrue(options["collector"])
        self.assertNotIn("inject_ns", options)
        self.assertEqual(control_options["inject_ns"], 5000)
        self.assertEqual(control_scenarios, ("plain", "alloy", "alloy-logs", "alloy-diagnostics"))
        self.assertEqual((slot["attempt"], slot["cpu_model"], slot["control_file"]),
                         (2, "A", "calibration-01-control.jsonl"))

    def test_a_changed_control_file_fails(self):
        directory = write_slot(self.slots, "calibration-01", [0.6] * 3)
        control = directory / "calibration-01-control.jsonl"
        control.write_text("changed\n")
        (directory / "SHA256SUMS").write_text("".join(
            f"{digest(path)}  ./{path.name}\n" for path in sorted(directory.iterdir())
            if path.name != "SHA256SUMS"))
        with self.assertRaisesRegex(ValueError, "control results changed"):
            gate.load_slots(self.slots, "head", "full")

    def test_tampered_unlisted_or_foreign_evidence_fails(self):
        directory = write_slot(self.slots, "calibration-01", [0.6] * 3)
        (directory / "calibration-01.jsonl").write_text("changed\n")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            gate.load_slots(self.slots, "head", "full")
        directory = write_slot(self.root, "calibration-02", [0.6] * 3)
        (directory / "extra.txt").write_text("unlisted")
        with self.assertRaisesRegex(ValueError, "unlisted"):
            gate.verify_manifest(directory)
        foreign = self.root / "foreign"
        foreign.mkdir()
        write_slot(foreign, "calibration-03", [0.6] * 3)
        with self.assertRaisesRegex(ValueError, "foreign"):
            gate.load_slots(foreign, "other commit", "full")


if __name__ == "__main__":
    unittest.main()
