"""Tests for ci/check_advisory_expiry.py.

Run: python3 -I -m unittest discover -s ci/tests -p 'test_*.py' -v
"""

from __future__ import annotations

import datetime as dt
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_advisory_expiry.py"
REPO_DENY = Path(__file__).resolve().parents[2] / "deny.toml"
TODAY = dt.date(2026, 11, 30)


def run(deny_toml: str, today: str = TODAY.isoformat()) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as directory:
        config = Path(directory) / "deny.toml"
        config.write_text(deny_toml, encoding="utf-8")
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), "--config", str(config), "--today", today],
            check=False,
            capture_output=True,
            text=True,
        )


def deny_with(*entries: str) -> str:
    return "[advisories]\nignore = [\n" + "".join(f"    {e},\n" for e in entries) + "]\n"


class AdvisoryExpiryTests(unittest.TestCase):
    def test_deadline_before_at_and_after(self):
        entry = deny_with('{ id = "RUSTSEC-2099-0001", reason = "why [expires:2026-11-30]" }')
        self.assertEqual(run(entry, "2026-11-29").returncode, 0)
        at = run(entry, "2026-11-30")
        self.assertEqual(at.returncode, 0, at.stderr)
        self.assertIn("valid through 2026-11-30", at.stdout)
        after = run(entry, "2026-12-01")
        self.assertEqual(after.returncode, 1)
        self.assertIn("RUSTSEC-2099-0001: expired on 2026-11-30", after.stderr)

    def test_missing_token_reason_or_id_fails(self):
        cases = {
            "no token": '{ id = "RUSTSEC-2099-0002", reason = "no deadline" }',
            "no reason": '{ id = "RUSTSEC-2099-0003" }',
            "string form": '"RUSTSEC-2099-0004"',
            "no id": '{ reason = "why [expires:2026-12-31]" }',
        }
        for name, entry in cases.items():
            with self.subTest(name=name):
                result = run(deny_with(entry))
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("error:", result.stderr)

    def test_malformed_or_duplicate_tokens_fail(self):
        cases = {
            "slashes": "[expires:2026/12/31]",
            "impossible": "[expires:2026-02-30]",
            "short": "[expires:2026-1-5]",
            "empty": "[expires:]",
            "two tokens": "[expires:2026-12-31] [expires:2027-01-31]",
            "unterminated": "[expires:2026-12-31",
            "valid then dangling": "[expires:2027-12-31] earlier cutoff [expires:2026-11-01",
            "dangling then valid": "cutoff [expires:2026-11-01 then [expires:2027-12-31]",
        }
        for name, token in cases.items():
            with self.subTest(name=name):
                entry = deny_with(f'{{ id = "RUSTSEC-2099-0005", reason = "why {token}" }}')
                result = run(entry)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("RUSTSEC-2099-0005", result.stderr)

    def test_one_bad_entry_fails_the_whole_file(self):
        entries = deny_with(
            '{ id = "RUSTSEC-2099-0006", reason = "fine [expires:2026-12-31]" }',
            '{ id = "RUSTSEC-2099-0007", reason = "stale [expires:2026-11-01]" }',
        )
        result = run(entries)
        self.assertEqual(result.returncode, 1)
        self.assertIn("ok: RUSTSEC-2099-0006", result.stdout)
        self.assertIn("RUSTSEC-2099-0007: expired", result.stderr)

    def test_no_ignores_passes_and_unreadable_config_is_an_error(self):
        self.assertEqual(run("[advisories]\nversion = 2\n").returncode, 0)
        self.assertEqual(run("").returncode, 0)
        self.assertEqual(run("[advisories\n").returncode, 2)

    def test_no_exceptions_at_all_is_valid(self):
        # Removing the last ignore (an empty list, or no `ignore` key) must not
        # break CI.
        self.assertEqual(run("[advisories]\nversion = 2\nignore = []\n").returncode, 0)
        self.assertEqual(run("[advisories]\nversion = 2\nyanked = \"deny\"\n").returncode, 0)

    def test_repository_deny_toml_passes_the_checker_structurally(self):
        # Every ignore the repository has (possibly none) carries one deadline.
        # Dates are checked against the far past so this test never expires.
        result = subprocess.run(
            [sys.executable, "-I", str(SCRIPT), "--config", str(REPO_DENY), "--today", "2000-01-01"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(REPO_DENY.read_text(encoding="utf-8"))
        for entry in config.get("advisories", {}).get("ignore", []):
            with self.subTest(id=entry.get("id") if isinstance(entry, dict) else entry):
                self.assertIsInstance(entry, dict)


if __name__ == "__main__":
    unittest.main()
