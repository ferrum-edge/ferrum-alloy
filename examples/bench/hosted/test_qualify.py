"""Hosted-only tests for rejecting misleading benchmark evidence."""

import copy
import json
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch

from qualify import (CLASSIFICATION, Evidence, LOSS_REASONS, SCENARIOS,
                     instruction_profiles, stop, summarize, validate)


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


def profile_command(scenario):
    name = f"profile-{scenario}"
    return (f"target/release/alloy-bench run --scenario {scenario} --workload small "
            f"--transport h1 --seconds 1 --warmup 0.2 --concurrency 8 --rep 1 "
            f"--run-id {name} --label {CLASSIFICATION}")


def thread_profile(pid, thread, total, command):
    # Compact format-1 body with inclusive call costs and cost-free jump records,
    # using the metadata/layout captured from Valgrind 3.22 on the hosted runner.
    return (f"# callgrind format\nversion: 1\ncreator: callgrind-3.22.0\npid: {pid}\n"
            f"cmd:  {command}\npart: 1\nthread: {thread}\n\n\n"
            "desc: I1 cache: \ndesc: D1 cache: \ndesc: LL cache: \n\n"
            "desc: Timerange: Basic block 0 - 30036858\n"
            "desc: Trigger: Program termination\n\npositions: instr line\nevents: Ir\n"
            f"summary: {total}\n\n\nob=(1) target/release/alloy-bench\n"
            "fl=(1) examples/bench/src/main.rs\nfn=(1) request\n"
            f"0x10 1 {total}\ncfn=(2) response\ncalls=2 0x20 2\n* * {total * 2}\n"
            "jfi=(1)\njcnd=1/2 +1 +1 \n* * \njump=1 0x30 3 \n* * \n\n"
            f"totals: {total}\n")


class ProfileTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.out = Path(directory.name)
        self.prepare("alloy-logs-fmt", 10219, [1362883, 380453, 29128909, 32484787,
                                              32624867, 37223312, 2377941, 2159974,
                                              2485266, 2581727], 142810119)

    def prepare(self, scenario, pid, totals, collected):
        self.scenario = scenario
        self.name = f"profile-{scenario}"
        self.pid = pid
        self.totals = totals
        self.command = profile_command(scenario)
        self.base = self.out / f"{self.name}.callgrind.{pid}"
        self.base.write_bytes(b"")
        self.children = []
        for thread, total in enumerate(totals, 1):
            path = self.out / f"{self.base.name}-{thread:02d}"
            path.write_text(thread_profile(pid, thread, total, self.command))
            self.children.append(path)
        self.stderr = self.out / f"{self.name}.stderr"
        self.stderr.write_text(
            f"=={pid}== Callgrind, a call-graph generating cache profiler\n"
            f"=={pid}== Using Valgrind-3.22.0 and LibVEX; rerun with -h for copyright info\n"
            f"=={pid}== Events    : Ir\n=={pid}== Collected : {collected}\n")

    def profiles(self):
        return instruction_profiles(self.out, self.name, self.pid, self.command)

    def test_accepts_captured_empty_base_and_all_ten_threads_for_both_formatters(self):
        # Actual filenames, PIDs and per-thread/collected counts from run
        # 37214861461, artifact 11307779193. The compact body is synthetic;
        # passing this test does not substitute for a fresh hosted profile run.
        self.assertEqual(self.profiles(), list(zip(self.children, self.totals)))
        self.assertEqual(self.base.stat().st_size, 0)
        self.prepare("alloy-logs", 10230, [1393378, 399624, 27300573, 25873521,
                                          25836902, 24711615, 4683605, 4722897,
                                          4233471, 4260276], 123415862)
        self.assertEqual(self.profiles(), list(zip(self.children, self.totals)))

    def test_does_not_require_optional_base_placeholder(self):
        self.base.unlink()
        self.assertEqual(self.profiles(), list(zip(self.children, self.totals)))

    def test_placeholder_without_usable_children_is_rejected(self):
        for path in self.children:
            path.unlink()
        with self.assertRaisesRegex(ValueError, "missing main-thread"):
            self.profiles()
        self.children[0].write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "empty/truncated"):
            self.profiles()

    def test_missing_thread_is_rejected_even_with_other_valid_threads(self):
        for path in (self.children[0], self.children[5]):
            text = path.read_text()
            path.unlink()
            with self.subTest(path=path.name), self.assertRaises(ValueError):
                self.profiles()
            path.write_text(text)

    def test_nonempty_base_is_never_treated_as_a_placeholder(self):
        for text in ("\n", self.children[0].read_text()):
            self.base.write_text(text)
            with self.subTest(text=text[:20]), self.assertRaisesRegex(ValueError, "unsuffixed"):
                self.profiles()

    def test_rejects_arbitrary_extra_files_and_noncanonical_names(self):
        for suffix in ("", ".junk", ".10220", ".10220-01", ".10219-00", ".10219-1", ".10219-abc",
                       ".10219-001", ".10219-01.extra", ".10219.1-01"):
            path = self.out / f"{self.name}.callgrind{suffix}"
            path.write_bytes(b"")
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                self.profiles()
            path.unlink()
        path = self.out / f"{self.base.name}-11"
        path.mkdir()
        with self.assertRaisesRegex(ValueError, "unsupported profile file"):
            self.profiles()
        path.rmdir()
        path.symlink_to(self.children[0])
        with self.assertRaisesRegex(ValueError, "unsupported profile file"):
            self.profiles()

    def test_extra_valid_thread_cannot_inflate_profile_evidence(self):
        path = self.out / f"{self.base.name}-11"
        path.write_text(thread_profile(self.pid, 11, 1, self.command))
        with self.assertRaisesRegex(ValueError, "totals differ from profiler collection"):
            self.profiles()

    def test_rejects_malformed_or_mismatched_header_metadata(self):
        original = self.children[0].read_text()
        changes = [("# callgrind format", "# other format"), ("version: 1", "version: 2"),
                   ("creator: callgrind-3.22.0", "creator: callgrind-3.21.0"),
                   ("pid: 10219", "pid: 10220"), ("pid: 10219", "pid: invalid"),
                   ("pid: 10219", "pid: 10219\npid: 10219"),
                   ("thread: 1", "thread: 2"), ("thread: 1", "thread: 01"),
                   ("thread: 1", ""), ("part: 1", "part: 2"),
                   ("--scenario alloy-logs-fmt", "--scenario alloy-logs"),
                   ("--run-id profile-alloy-logs-fmt", "--run-id other"),
                   ("Basic block 0 - 30036858", "Basic block 1 - 30036858"),
                   ("Program termination", "Client Request"),
                   ("positions: instr line", "positions: line"),
                   ("events: Ir", "events: Ir Dr"), ("events: Ir", "events: IrExtra"),
                   ("events: Ir", "events: Ir\nevents: Ir"), ("events: Ir", "")]
        for before, after in changes:
            self.children[0].write_text(original.replace(before, after))
            with self.subTest(before=before, after=after), self.assertRaises(ValueError):
                self.profiles()

    def test_rejects_empty_truncated_malformed_and_zero_thread_evidence(self):
        original = self.children[0].read_text()
        cases = ["", original.rstrip("\n"), original[:original.index("totals:")],
                 original.replace("summary: 1362883", ""),
                 original.replace("summary: 1362883", "summary: invalid"),
                 original.replace("summary: 1362883", "summary: -1"),
                 original.replace("summary: 1362883", "summary: 1362883 1"),
                 original.replace("summary: 1362883", "summary: 1362883\nsummary: 1362883"),
                 original.replace("1362883", "0"),
                 original.replace("totals: 1362883", "totals: 1"),
                 original.replace("totals: 1362883", "totals: invalid"),
                 original.replace("0x10 1 1362883", "0x10 1 1362882"),
                 original.replace("0x10 1 1362883", "0x10 1 invalid"),
                 original.replace("0x10 1 1362883", "unknown body line"),
                 original.replace("* * 2725766", ""),
                 original.replace("0x10 1 1362883", ""),
                 original + "extra data\n"]
        for index, text in enumerate(cases):
            self.children[0].write_text(text)
            with self.subTest(case=index), self.assertRaises(ValueError):
                self.profiles()

    def test_profiler_completion_and_pid_binding_are_required(self):
        original = self.stderr.read_text()
        cases = ["", original.replace("==10219==", "==10220=="),
                 original.replace("Events    : Ir", "Events    : Ir Dr"),
                 original.replace("Events    : Ir", "Events    : IrExtra"),
                 original.replace("Collected : 142810119", "Collected : 0"),
                 original.replace("Collected : 142810119", "Collected : -1"),
                 original.replace("Collected : 142810119", "Collected : invalid"),
                 original.replace("Collected : 142810119", "Collected : 142810118"),
                 original.replace("Using Valgrind-3.22.0", "Using Valgrind-3.21.0"),
                 original + "==10219== Events    : Ir\n",
                 original + "==10219== Collected : 142810119\n",
                 original + "==10220== Collected : 1\n"]
        for index, text in enumerate(cases):
            self.stderr.write_text(text)
            with self.subTest(case=index), self.assertRaises(ValueError):
                self.profiles()
        self.stderr.write_text(original)
        for pid in (None, True, 0, -1, "10219", 10220):
            with self.subTest(pid=pid), self.assertRaises(ValueError):
                instruction_profiles(self.out, self.name, pid, self.command)

    def evidence(self, annotation_stderr="", annotation_total=None, annotation_stdout=None,
                 commit="head"):
        evidence = Evidence(self.out, "head")

        def command(name, args, timeout):
            if name == self.name:
                row = result(1, self.scenario)
                row.update(commit=commit, run_id=name, seconds=1.0, warmup_seconds=0.2,
                           concurrency=8, connections=8)
                (self.out / f"{name}.stdout").write_text(json.dumps(row) + "\n")
                return self.pid
            index = int(name.rsplit("-", 1)[1])
            total = self.totals[index] if annotation_total is None else annotation_total
            stdout = f"{total:,}  PROGRAM TOTALS\n" if annotation_stdout is None else annotation_stdout
            (self.out / f"{name}.stdout").write_text(stdout)
            (self.out / f"{name}.stderr").write_text(annotation_stderr)
            return 999

        evidence.command = Mock(side_effect=command)
        return evidence

    def test_profile_annotates_every_valid_thread_and_never_the_placeholder(self):
        evidence = self.evidence()
        evidence.stage(self.name, lambda: evidence.profile(self.scenario))
        self.assertEqual(evidence.stages[self.name]["status"], "complete")
        calls = evidence.command.call_args_list
        self.assertEqual(len(calls), 11)
        self.assertEqual(calls[0].args[2], 90)
        self.assertIn("--separate-threads=yes", calls[0].args[1])
        for index, entry in enumerate(calls[1:]):
            self.assertEqual(entry.args, (f"{self.name}-annotate-{index}",
                                         ["callgrind_annotate", "--auto=no", "--show-percs=no",
                                          str(self.children[index])], 15))

    def test_invalid_child_prevents_annotations_and_fails_the_stage(self):
        self.children[-1].write_bytes(b"")
        evidence = self.evidence()
        evidence.stage(self.name, lambda: evidence.profile(self.scenario))
        self.assertEqual(evidence.stages[self.name]["status"], "failed")
        evidence.command.assert_called_once()

    def test_annotation_timeout_fails_the_profile_stage(self):
        evidence = self.evidence()
        collect = evidence.command.side_effect

        def command(name, args, timeout):
            if name != self.name:
                raise subprocess.TimeoutExpired(args, timeout)
            return collect(name, args, timeout)

        evidence.command.side_effect = command
        evidence.stage(self.name, lambda: evidence.profile(self.scenario))
        self.assertEqual(evidence.stages[self.name]["status"], "failed")
        self.assertEqual(evidence.command.call_count, 2)

    def test_profile_preserves_source_validation_before_annotations(self):
        evidence = self.evidence(commit="other")
        evidence.stage(self.name, lambda: evidence.profile(self.scenario))
        self.assertEqual(evidence.stages[self.name]["status"], "failed")
        self.assertIn("mixed source", evidence.stages[self.name]["error"])
        evidence.command.assert_called_once()

    def test_annotation_diagnostics_or_wrong_totals_fail_the_stage(self):
        for stderr, total in [("WARNING: line malformed, ignoring\n", None), ("", 0), ("", 1)]:
            evidence = self.evidence(annotation_stderr=stderr, annotation_total=total)
            evidence.stage(self.name, lambda: evidence.profile(self.scenario))
            with self.subTest(stderr=stderr, total=total):
                self.assertEqual(evidence.stages[self.name]["status"], "failed")
                self.assertEqual(evidence.command.call_count, 2)
        evidence = self.evidence(annotation_stdout="")
        evidence.stage(self.name, lambda: evidence.profile(self.scenario))
        self.assertEqual(evidence.stages[self.name]["status"], "failed")


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
        self.assertEqual(evidence.commands[0]["pid"], 123)
        self.assertEqual(evidence.stages["timeout"]["status"], "failed")
        self.assertEqual(kill.call_args_list,
                         [call(123, signal.SIGTERM), call(123, signal.SIGKILL)])

    def test_cleanup_errors_are_not_waived(self):
        with patch("qualify.os.killpg", side_effect=PermissionError("cleanup rejected")), \
             self.assertRaises(PermissionError):
            stop(Mock(pid=123))

    def test_command_returns_and_records_owned_profiler_pid(self):
        process = Mock(pid=123)
        process.wait.return_value = 0
        with tempfile.TemporaryDirectory() as directory, \
             patch("qualify.subprocess.Popen", return_value=process), \
             patch("qualify.os.killpg"):
            evidence = Evidence(Path(directory), "head")
            self.assertEqual(evidence.command("profile", ["valgrind"], 90), 123)
        self.assertEqual(evidence.commands[0]["pid"], 123)
        self.assertEqual(evidence.commands[0]["status"], "complete")


if __name__ == "__main__":
    unittest.main()
