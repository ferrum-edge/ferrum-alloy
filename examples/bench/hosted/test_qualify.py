"""Hosted-only tests for rejecting misleading benchmark evidence."""

import copy
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch

from qualify import Evidence, LOSS_REASONS, SCENARIOS, stop, summarize, validate


def result(rep, scenario):
    return {"schema": "alloy-bench/1", "rep": rep, "scenario": scenario,
            "workload": "small", "transport": "h1", "commit": "head", "run_id": "run",
            "alloc_counting": False, "allocations": None, "requests": 100,
            "requests_per_second": 20.0, "errors": 0, "error_samples": [],
            "seconds": 5.0, "warmup_seconds": 1.0, "concurrency": 32,
            "connections": 32, "streams_per_connection": 1,
            "server_threads": 4, "client_threads": 4,
            "cpu": {"service_ns": 100, "client_ns": 100,
                    "service_us_per_request": 0.001, "client_us_per_request": 0.001},
            "memory": {"scope": "process", "peak_rss_bytes": 2048, "rss_bytes": 1024},
            "environment": {"os": "linux", "debug_build": False, "kernel": "test",
                            "cpus": 4, "started_unix_seconds": 1}, "otel": None}


def check(rows, scenarios=SCENARIOS, counting=False):
    validate(rows, scenarios, ["small"], ["h1"], 5, "head", "run", counting)


class ValidationTests(unittest.TestCase):
    def setUp(self):
        self.rows = [result(rep, scenario) for rep in range(1, 6) for scenario in SCENARIOS]

    def test_requires_complete_unique_repetitions(self):
        check(self.rows)
        for rows in (self.rows[:-1], self.rows + [self.rows[0]], []):
            with self.assertRaises(ValueError):
                check(rows)

    def test_rejects_errors_mixed_sources_and_unsupported_probes(self):
        for key, value in [("errors", 1), ("error_samples", ["connection reset"]),
                           ("commit", "other"), ("run_id", "other"),
                           ("alloc_counting", True), ("seconds", 0.2),
                           ("requests", 0), ("requests_per_second", float("nan")),
                           ("cpu", None), ("memory", None)]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                rows = copy.deepcopy(self.rows)
                rows[0][key] = value
                check(rows)
        self.rows[0]["cpu"]["service_ns"] = 0
        with self.assertRaisesRegex(ValueError, "schedstat"):
            check(self.rows)

    def test_allocation_pass_requires_real_counters(self):
        for row in self.rows:
            row["alloc_counting"] = True
            row["allocations"] = {role: {"calls": 100, "bytes": 1000, "calls_per_request": 1}
                                  for role in ("service", "client", "collector")}
        check(self.rows, counting=True)
        self.rows[0]["allocations"]["service"]["calls"] = 0
        with self.assertRaises(ValueError):
            check(self.rows, counting=True)

    def test_collector_requires_exports_and_receipt_not_just_no_client_errors(self):
        rows = [result(rep, "otel-collector") for rep in range(1, 6)]
        for row in rows:
            row["otel"] = {"spans_exported": 10,
                           "spans_lost": dict.fromkeys(LOSS_REASONS, 0),
                           "collector_requests": 1, "collector_bytes": 100}
        check(rows, scenarios=["otel-collector"])
        for field in ("spans_exported", "collector_requests", "collector_bytes"):
            broken = copy.deepcopy(rows)
            broken[0]["otel"][field] = 0
            with self.subTest(field=field), self.assertRaises(ValueError):
                check(broken, scenarios=["otel-collector"])
        rows[0]["otel"]["spans_lost"]["export_failed"] = 1
        with self.assertRaises(ValueError):
            check(rows, scenarios=["otel-collector"])

    def test_unreachable_must_exercise_failure_and_preserve_drop_reason(self):
        rows = [result(rep, "otel-unreachable") for rep in range(1, 6)]
        for row in rows:
            row["otel"] = {"spans_exported": 0, "spans_lost": dict.fromkeys(LOSS_REASONS, 0)}
            row["otel"]["spans_lost"]["export_failed"] = 10
        check(rows, scenarios=["otel-unreachable"])
        rows[0]["otel"]["spans_lost"]["export_failed"] = 0
        with self.assertRaises(ValueError):
            check(rows, scenarios=["otel-unreachable"])

    def test_pairs_same_repetition_and_separates_allocation_modes_and_invocations(self):
        for row in self.rows:
            row["requests_per_second"] = 10.0 * row["rep"]
            if row["scenario"] == "alloy-logs":
                row["requests_per_second"] *= 2
        check(self.rows)
        extra = copy.deepcopy(self.rows)
        for row in extra:
            row["run_id"] = "allocation-run"
            row["alloc_counting"] = True
            row["allocations"] = {"service": {"calls_per_request": 1}}
            row["requests_per_second"] *= 10
        summaries = summarize(self.rows + extra)
        typed = [row for row in summaries if row["scenario"] == "alloy-logs"]
        self.assertEqual(len(typed), 2)
        for row in typed:
            self.assertEqual(row["throughput_ratio_to_plain"],
                             {"n": 5, "median": 2, "min": 2, "max": 2})
            self.assertEqual(row["throughput_ratio_typed_to_fmt"]["median"], 2)

    def test_stage_failure_is_recorded_and_not_waived(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Evidence(Path(directory), "head")
            evidence.stage("broken", lambda: check([]))
            self.assertEqual(evidence.stages["broken"]["status"], "failed")
            self.assertIn("missing", evidence.stages["broken"]["error"])

    def test_cleanup_signals_descendants_even_when_group_leader_exited(self):
        process = Mock(pid=123)
        process.wait.return_value = 0
        with patch("qualify.os.killpg") as kill:
            stop(process)
        self.assertEqual(kill.call_args_list,
                         [call(123, signal.SIGTERM), call(123, signal.SIGKILL)])
        process.wait.assert_called_once_with(timeout=5)

    def test_command_timeout_is_failed_and_group_is_cleaned(self):
        process = Mock(pid=123)
        process.wait.side_effect = [subprocess.TimeoutExpired("bench", 1), 0]
        with tempfile.TemporaryDirectory() as directory, \
             patch("qualify.subprocess.Popen", return_value=process), \
             patch("qualify.os.killpg") as kill:
            evidence = Evidence(Path(directory), "head")
            evidence.stage("timeout", lambda: evidence.command("bench", ["bench"], 1))
        self.assertEqual(evidence.commands[0]["status"], "failed")
        self.assertEqual(evidence.stages["timeout"]["status"], "failed")
        self.assertEqual(kill.call_args_list,
                         [call(123, signal.SIGTERM), call(123, signal.SIGKILL)])

    def test_cleanup_errors_are_not_waived(self):
        with patch("qualify.os.killpg", side_effect=PermissionError("cleanup rejected")), \
             self.assertRaises(PermissionError):
            stop(Mock(pid=123))


if __name__ == "__main__":
    unittest.main()
