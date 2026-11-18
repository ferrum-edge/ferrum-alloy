"""Tests that the Edge main drift workflow runs ci.yml's Edge configuration steps.

.github/workflows/edge-main-drift.yml copies the three steps of ci.yml's
"Generated Edge configuration validates" job and runs them against the signed
Ferrum Edge `main` image (ferrum-edge/ferrum-contracts#76). A copy that drifts
from ci.yml would warn about the wrong thing, so the scripts must stay equal.

Run: python3 -I -m unittest discover -s ci/tests -p 'test_*.py' -v
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CI = REPO / ".github/workflows/ci.yml"
DRIFT = REPO / ".github/workflows/edge-main-drift.yml"

STEP_START = re.compile(r"^      - ", re.MULTILINE)
JOB_START = re.compile(r"^  [a-z][a-z0-9-]*:\n", re.MULTILINE)
RUN_BLOCK = re.compile(r"^        run: \|\n((?:(?: {10}.*)?\n)+)", re.MULTILINE)
COPIED = ("ferrum-edge validate (", "Edge ", "POST /api-specs (")
IDENTITY = (
    "--certificate-identity https://github.com/ferrum-edge/ferrum-edge/"
    ".github/workflows/main-latest-image.yml@refs/heads/main"
)


def job(text: str, name: str) -> str:
    start = text.index(f"\n  {name}:\n") + 1
    rest = text[start + 1 :]
    following = JOB_START.search(rest)
    return text[start:] if following is None else text[start : start + 1 + following.start()]


def steps(job_text: str) -> list[str]:
    starts = [match.start() for match in STEP_START.finditer(job_text)]
    return [job_text[a:b] for a, b in zip(starts, starts[1:] + [len(job_text)])]


def copied_scripts(job_text: str) -> list[str]:
    scripts = []
    for step in steps(job_text):
        name = step.split("\n", 1)[0].removeprefix("      - name: ")
        if name.startswith(COPIED):
            block = RUN_BLOCK.search(step)
            assert block is not None, name
            scripts.append(block.group(1).rstrip())
    return scripts


class EdgeMainDriftTests(unittest.TestCase):
    def setUp(self):
        self.ci = CI.read_text(encoding="utf-8")
        self.drift = DRIFT.read_text(encoding="utf-8")

    def test_runs_ci_edge_configuration_steps_verbatim(self):
        pinned = copied_scripts(job(self.ci, "edge-config"))
        main = copied_scripts(job(self.drift, "check"))
        self.assertEqual(len(pinned), 3)
        self.assertEqual(main, pinned)

    def test_only_the_issue_job_writes_and_never_for_pull_requests(self):
        check = job(self.drift, "check")
        issue = job(self.drift, "issue")
        self.assertIn("permissions: {}\n", self.drift)
        self.assertIn("    permissions:\n      contents: read\n", check)
        self.assertNotIn(": write", check)
        self.assertIn("      issues: write", issue)
        self.assertNotIn("uses:", issue)
        self.assertIn("github.event_name != 'pull_request'", issue)
        self.assertIn(
            "github.ref == format('refs/heads/{0}', github.event.repository.default_branch)",
            issue,
        )
        self.assertNotIn("secrets.", self.drift)

    def test_a_check_that_could_not_run_still_reaches_the_issue(self):
        # The report is written even when resolving the image failed, and the
        # issue job runs for any finished check, so a broken pipeline is filed
        # as `error` rather than going silent; only a clean run closes it.
        report = next(step for step in steps(job(self.drift, "check")) if "- name: Write the report" in step)
        self.assertIn("        if: ${{ !cancelled() }}\n", report)
        self.assertIn("outcome=error", report)
        issue = job(self.drift, "issue")
        self.assertNotIn("needs.check.result == 'success'", issue)
        self.assertIn("needs.check.result != 'cancelled'", issue)
        self.assertIn("drift | error)", issue)
        clean = issue[issue.index("            clean)") :]
        self.assertIn("gh issue close", clean)
        self.assertEqual(issue.count("gh issue close"), 1)
        # Only an issue the workflow wrote, found by its marker on every page.
        self.assertIn("--paginate", issue)
        self.assertIn('startswith("<!-- edge-main-drift -->")', issue)

    def test_runs_only_the_verified_edge_main_image(self):
        check = job(self.drift, "check")
        self.assertIn(IDENTITY, check)
        self.assertNotIn("matrix.", check)
        self.assertNotIn("ferrumedge/ferrum-edge", self.drift)
        for step in steps(check):
            if step.split("\n", 1)[0].removeprefix("      - name: ").startswith(COPIED):
                self.assertIn("EDGE_IMAGE: ${{ steps.edge.outputs.image }}", step)
                self.assertIn("continue-on-error: true", step)


if __name__ == "__main__":
    unittest.main()
