"""Tests that the repository tracks no generated Python bytecode.

Run: python3 -I -m unittest discover -s ci/tests -p 'test_*.py' -v
"""

from __future__ import annotations

import subprocess
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


class TrackedFilesTests(unittest.TestCase):
    def test_no_python_bytecode_is_tracked(self):
        # Running these tests locally writes ci/tests/__pycache__/*.pyc. The
        # bytecode is interpreter-specific and records the author's checkout
        # path, so it must never be committed (.gitignore also excludes it).
        listed = subprocess.run(
            ["git", "-C", str(REPO), "ls-files", "-z"],
            check=True,
            capture_output=True,
        ).stdout.decode("utf-8")
        bytecode = [
            path
            for path in listed.split("\0")
            if path.endswith((".pyc", ".pyo")) or "__pycache__/" in path
        ]
        self.assertEqual(bytecode, [])


if __name__ == "__main__":
    unittest.main()
