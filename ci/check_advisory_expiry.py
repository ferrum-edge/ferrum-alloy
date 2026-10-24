#!/usr/bin/env python3
"""Enforce the review deadline on every advisory ignore in deny.toml.

cargo-deny reads only `id` and `reason` from an `[advisories] ignore` entry, so
the `[expires:YYYY-MM-DD]` token the reason carries is inert to it. This
checker gives the token teeth: every ignore must be a table with a non-empty
`id` and a `reason` that carries exactly one well-formed `[expires:...]` date,
and that date must not have passed. The day itself is still valid; the entry
fails from the next day on. Dates are compared in UTC.

Run it before cargo-deny in the PR dependency-policy job and in the scheduled
advisories workflow:

    python3 -I ci/check_advisory_expiry.py [--config deny.toml] [--today YYYY-MM-DD]

Exit status: 0 when every ignore is valid, 1 when any is missing, malformed,
or expired, 2 when deny.toml cannot be read or parsed.
"""

from __future__ import annotations

import argparse
import datetime as dt
import re
import sys
import tomllib
from pathlib import Path

EXPIRES_TOKEN = re.compile(r"\[expires:([^\]]*)\]")
ISO_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def check(config: dict, today: dt.date) -> tuple[list[str], list[str]]:
    """Return (ok_lines, failures) for every advisory ignore in `config`."""
    ok: list[str] = []
    failures: list[str] = []
    advisories = config.get("advisories", {})
    if not isinstance(advisories, dict):
        return ok, ["[advisories] must be a table"]
    ignores = advisories.get("ignore", [])
    if not isinstance(ignores, list):
        return ok, ["[advisories] ignore must be an array"]

    for index, entry in enumerate(ignores):
        label = f"ignore[{index}]"
        if not isinstance(entry, dict):
            failures.append(
                f"{label}: {entry!r} must be a table with `id` and a `reason` carrying "
                "[expires:YYYY-MM-DD]"
            )
            continue
        advisory = entry.get("id")
        if not isinstance(advisory, str) or not advisory.strip():
            failures.append(f"{label}: missing `id`")
            continue
        label = advisory
        reason = entry.get("reason")
        if not isinstance(reason, str) or not reason.strip():
            failures.append(f"{label}: missing `reason` with an [expires:YYYY-MM-DD] token")
            continue
        tokens = EXPIRES_TOKEN.findall(reason)
        if not tokens:
            failures.append(f"{label}: `reason` has no [expires:YYYY-MM-DD] token")
            continue
        if len(tokens) > 1:
            failures.append(f"{label}: `reason` has {len(tokens)} [expires:] tokens; use one")
            continue
        raw = tokens[0].strip()
        if not ISO_DATE.fullmatch(raw):
            failures.append(f"{label}: malformed expiry {raw!r}; use [expires:YYYY-MM-DD]")
            continue
        try:
            expires = dt.date.fromisoformat(raw)
        except ValueError:
            failures.append(f"{label}: invalid expiry date {raw!r}")
            continue
        if expires < today:
            failures.append(
                f"{label}: expired on {expires.isoformat()}; re-review the exception and "
                "renew or remove it"
            )
            continue
        ok.append(f"ok: {label} valid through {expires.isoformat()}")
    return ok, failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", type=Path, default=Path("deny.toml"))
    parser.add_argument(
        "--today",
        type=dt.date.fromisoformat,
        default=dt.datetime.now(dt.timezone.utc).date(),
        help="evaluation date (YYYY-MM-DD; for tests)",
    )
    args = parser.parse_args(argv)
    try:
        config = tomllib.loads(args.config.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as exc:
        print(f"cannot read {args.config}: {exc}", file=sys.stderr)
        return 2
    ok, failures = check(config, args.today)
    for line in ok:
        print(line)
    for line in failures:
        print(f"error: {line}", file=sys.stderr)
    if failures:
        return 1
    print("Every advisory ignore carries a valid, unexpired [expires:] date.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
