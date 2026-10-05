"""Hosted CI only: verify the fork node and freeze this feature graph identity."""

import hashlib
import json
import os
from pathlib import Path
import sys


if os.environ.get("GITHUB_ACTIONS") != "true":
    raise ValueError("protocol graph recording is hosted-CI-only")
directory = Path(os.environ["ALLOY_BENCH_DIAGNOSTIC_DIR"])
path = directory / sys.argv[1]
graph = json.loads(path.read_text())
nodes = [package for package in graph["packages"] if package["name"] == "h2"]
if len(nodes) != 1 or nodes[0]["version"] != "0.4.19" or nodes[0]["source"] is not None:
    raise ValueError("experiment did not resolve exactly one local h2 0.4.19 fork")
if Path(nodes[0]["manifest_path"]).resolve().parent != Path(os.environ["ALLOY_PROTOCOL_H2"]):
    raise ValueError("unexpected h2 fork source")
digest = hashlib.sha256(path.read_bytes()).hexdigest()
with Path(os.environ["GITHUB_ENV"]).open("a") as output:
    output.write(f"ALLOY_BENCH_GRAPH_SHA256={digest}\n")
with (directory / "source.txt").open("a") as output:
    output.write(f"{path.name}_sha256={digest}\n")
