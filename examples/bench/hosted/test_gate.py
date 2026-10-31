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

    def test_well_sampled_models_get_their_own_budgets_others_use_pooled(self):
        calibration = [("A", run_cells([0.66] * 3))] * 3 + [("B", run_cells([0.61] * 3))] * 2
        sets = gate.budget_sets(calibration)
        self.assertEqual(set(sets), {None, "A"})
        self.assertEqual(gate.select(sets, "A"), "A")
        self.assertIsNone(gate.select(sets, "B"))
        self.assertIsNone(gate.select(sets, "never seen"))
        # Pooled limits include the spread between models.
        pooled = sets[None][1][0]["metrics"]["throughput_ratio"]
        model = sets["A"][1][0]["metrics"]["throughput_ratio"]
        self.assertGreater(pooled["margin"], model["margin"])

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
        for slot in ("injected", "allocations", "gate"):
            self.assertEqual(gate.slot_kind(slot), slot)
        for slot in ("calibration-7", "calibration", "gate-1", ""):
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

    def test_gate_check_selects_the_runners_model_or_the_pooled_fallback(self):
        out = self.root / "out"
        budgets = self.root / "budgets"
        out.mkdir()
        budgets.mkdir()
        (out / "run.jsonl").write_text(self.run.read_text())
        self.write_budget(0.7)
        (budgets / "model-a.json").write_text(self.budget.read_text())
        self.write_budget(0.5)
        (budgets / "pooled.json").write_text(self.budget.read_text())
        (budgets / "index.json").write_text(json.dumps({
            "schema": gate.INDEX_SCHEMA, "commit": "calibrated", "models": {"A": "model-a.json"},
            "fallback": "pooled.json"}))
        verdicts = {}
        for model in ("A", "B"):
            (out / "slot.json").write_text(json.dumps({
                "kind": "gate", "commit": "head", "cpu_model": model, "raw_file": "run.jsonl"}))
            with patch("gate.run_compare", fake_compare), patch("gate.BUDGETS", budgets):
                verdicts[model] = gate.check(out)
        self.assertFalse(verdicts["A"]["passed"])
        self.assertTrue(verdicts["A"]["calibrated_model"])
        self.assertTrue(verdicts["B"]["passed"])
        self.assertEqual(verdicts["B"]["budget_file"], "pooled.json")
        self.assertFalse(verdicts["B"]["calibrated_model"])
        self.assertEqual(json.loads((out / "gate.json").read_text())["cpu_model"], "B")


def write_slot(root, slot, throughputs, model="A", attempt=1, kind=None, profile="full"):
    directory = root / f"bench-gate-{slot}-{attempt}"
    directory.mkdir()
    raw = directory / f"{slot}.jsonl"
    raw.write_text("".join(json.dumps(compare_line(f"{slot}-{attempt}", cell, value)) + "\n"
                           for cell, value in zip(CELLS, throughputs)))
    (directory / "slot.json").write_text(json.dumps({
        "schema": gate.SLOT_SCHEMA, "slot": slot, "kind": kind or gate.slot_kind(slot),
        "profile": profile, "commit": "head", "run_id": f"{slot}-{attempt}",
        "attempt": attempt, "cpu_model": model, "raw_file": raw.name,
        "raw_sha256": digest(raw), "binary_sha256": "binary",
        "injected_service_ns": 5000 if slot == "injected" else 0, "alloc_counting": False}))
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

    def calibrate(self, values, profile="full"):
        for index, value in enumerate(values, 1):
            write_slot(self.slots, f"calibration-{index:02d}", [value] * 3, profile=profile)

    def test_accepts_quiet_calibration_that_catches_the_synthetic_regression(self):
        self.calibrate([0.600, 0.603, 0.597, 0.601, 0.599, 0.602])
        write_slot(self.slots, "injected", [0.50] * 3)
        report = self.evaluate()
        self.assertTrue(report["accepted"])
        self.assertEqual(report["false_positives"], [])
        self.assertEqual([check["budgets"] for check in report["leave_one_out"]], ["A"] * 6)
        regression = report["synthetic_regression"]
        self.assertFalse(regression["passed"])
        self.assertEqual(len(regression["exceeded"]), 3)
        index = json.loads((self.out / "budgets" / "index.json").read_text())
        self.assertEqual(index["models"], {"A": "model-a.json"})
        self.assertEqual(index["fallback"], "pooled.json")
        self.assertEqual(index["rule"]["spread_multiplier"], gate.SPREAD_MULTIPLIER)
        for name in ("model-a.json", "pooled.json", "derivation.json"):
            self.assertTrue((self.out / "budgets" / name).is_file(), name)

    def test_rejects_a_held_out_false_positive_or_an_uncaught_regression(self):
        self.calibrate([0.600, 0.601, 0.600, 0.601, 0.600, 0.560])
        write_slot(self.slots, "injected", [0.59] * 3)
        report = self.evaluate()
        self.assertFalse(report["accepted"])
        self.assertEqual(report["false_positives"], ["calibration-06"])
        self.assertTrue(report["synthetic_regression"]["passed"])

    def test_smoke_reports_but_does_not_require_held_out_passes(self):
        self.calibrate([0.600, 0.601, 0.585], profile="smoke")
        write_slot(self.slots, "injected", [0.30] * 3, profile="smoke")
        report = self.evaluate("smoke")
        self.assertTrue(report["false_positives"])
        self.assertTrue(report["accepted"])

    def test_needs_enough_calibration_and_one_injected_slot(self):
        self.calibrate([0.6] * 5)
        write_slot(self.slots, "injected", [0.5] * 3)
        with self.assertRaisesRegex(ValueError, "calibration slots"):
            self.evaluate()
        write_slot(self.slots, "calibration-06", [0.6] * 3, profile="smoke")
        with self.assertRaisesRegex(ValueError, "foreign"):
            self.evaluate()  # Mixed profiles are foreign evidence.

    def test_rerun_attempts_replace_earlier_ones_and_partial_evidence_is_ignored(self):
        self.calibrate([0.6] * 6)
        write_slot(self.slots, "injected", [0.6] * 3)
        write_slot(self.slots, "injected", [0.5] * 3, attempt=2)
        (self.slots / "bench-gate-calibration-01-2").mkdir()  # failed, no slot.json
        slots = gate.load_slots(self.slots, "head", "full")
        self.assertEqual(slots["injected"]["attempt"], 2)
        self.assertEqual(slots["calibration-01"]["attempt"], 1)

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
