"""Hosted CI only: prepare an exact, isolated numerical observer experiment.

Never run this script on an implementer's machine. The caller exports the exact
source head with git archive into RUNNER_TEMP; no global cache is modified.
"""

import hashlib
import os
from pathlib import Path
import re
import sys
import tarfile
import urllib.request


ARCHIVE_SHA256 = "ef8e5e5a340588f4452631496976cf8636d4a7ecf600239fdc27615d2530bc16"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_utf8(path):
    # No locale decoding, BOM stripping or universal-newline translation.
    return path.read_bytes().decode("utf-8")


def verify(root, manifest):
    for line in read_utf8(manifest).splitlines():
        expected, name = line.split("  ", 1)
        path = root / name
        if path.resolve().is_relative_to(root.resolve()) and digest(path) == expected:
            continue
        raise ValueError(f"exact source checksum mismatch: {name}")


def apply_exact(root, patch):
    # Deliberately no search, offsets or fuzz: every hunk uses its declared
    # original line and matches the complete old/context text byte for byte.
    lines = read_utf8(patch).splitlines(keepends=True)
    index = 0
    while index < len(lines):
        if not lines[index].startswith("--- "):
            raise ValueError("expected unified diff file header")
        old_name = lines[index][4:].strip()
        new_name = lines[index + 1][4:].strip()
        if not lines[index + 1].startswith("+++ b/"):
            raise ValueError("invalid target header")
        path = root / new_name[2:]
        if not path.resolve().is_relative_to(root.resolve()):
            raise ValueError("patch path outside experiment")
        if old_name != "/dev/null" and old_name != "a/" + new_name[2:]:
            raise ValueError("renames are not allowed")
        original = read_utf8(path).splitlines(keepends=True) if old_name != "/dev/null" else []
        if old_name == "/dev/null" and path.exists():
            raise ValueError("new patch target already exists")
        result = []
        cursor = 0
        index += 2
        while index < len(lines) and lines[index].startswith("@@ "):
            header = re.fullmatch(
                r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@\n", lines[index]
            )
            if header is None:
                raise ValueError("invalid hunk header")
            old_start, old_count, new_start, new_count = (
                int(value) if value is not None else 1 for value in header.groups()
            )
            start = old_start - 1 if old_count else old_start
            if start < cursor:
                raise ValueError("overlapping hunks")
            result.extend(original[cursor:start])
            new_position = new_start - 1 if new_count else new_start
            if len(result) != new_position:
                raise ValueError("new hunk line position mismatch")
            before = []
            after = []
            index += 1
            while index < len(lines) and lines[index][:1] in (" ", "+", "-"):
                if lines[index].startswith("--- "):
                    break
                kind, text = lines[index][0], lines[index][1:]
                if kind in (" ", "-"):
                    before.append(text)
                if kind in (" ", "+"):
                    after.append(text)
                index += 1
            if len(before) != old_count or len(after) != new_count:
                raise ValueError("hunk count mismatch")
            if original[start : start + old_count] != before:
                raise ValueError(f"exact hunk mismatch: {new_name}:{old_start}")
            result.extend(after)
            cursor = start + old_count
        result.extend(original[cursor:])
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes("".join(result).encode("utf-8"))


def add_dependency(path):
    text = read_utf8(path)
    anchor = "[dependencies]\n"
    if text.count(anchor) != 1:
        raise ValueError("unexpected manifest dependencies")
    path.write_bytes(text.replace(anchor, anchor + 'h2 = "=0.4.19"\n').encode("utf-8"))


def expected_lock(text):
    blocks = text.split("[[package]]\n")
    for index, block in enumerate(blocks):
        if block.startswith('name = "h2"\n'):
            pin = (
                'version = "0.4.19"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                f'checksum = "{ARCHIVE_SHA256}"\n'
            )
            if block.count(pin) != 1:
                raise ValueError("ordinary lock pin differs from reviewed archive")
            blocks[index] = block.replace(pin, 'version = "0.4.19"\n')
        elif block.startswith(('name = "example-bench"\n', 'name = "ferrum-alloy"\n')):
            head, dependencies = block.split("dependencies = [\n", 1)
            items, tail = dependencies.split("]\n", 1)
            items = sorted(items.splitlines() + [' "h2",'])
            blocks[index] = head + "dependencies = [\n" + "\n".join(items) + "\n]\n" + tail
    return "[[package]]\n".join(blocks)


workspace = Path(sys.argv[1]).resolve()
if os.environ.get("GITHUB_ACTIONS") != "true":
    raise ValueError("protocol preparation is hosted-CI-only")
if not workspace.is_relative_to(Path(os.environ["RUNNER_TEMP"]).resolve()):
    raise ValueError("experiment workspace must be inside RUNNER_TEMP")
diagnostics = workspace / "examples/bench/diagnostics"
archive = workspace.parent / "h2-0.4.19.crate"
urllib.request.urlretrieve("https://static.crates.io/crates/h2/h2-0.4.19.crate", archive)
if digest(archive) != ARCHIVE_SHA256:
    raise ValueError("published h2 archive checksum mismatch")
with tarfile.open(archive) as packed:
    packed.extractall(workspace.parent, filter="data")
h2 = workspace.parent / "h2-0.4.19"
verify(h2, diagnostics / "h2-original.sha256")
verify(workspace, diagnostics / "alloy-original.sha256")
apply_exact(h2, diagnostics / "h2-0.4.19.patch")
apply_exact(workspace, diagnostics / "observer.patch")
verify(h2, diagnostics / "h2-patched.sha256")
verify(workspace, diagnostics / "alloy-patched.sha256")
add_dependency(workspace / "crates/ferrum-alloy/Cargo.toml")
add_dependency(workspace / "examples/bench/Cargo.toml")
manifest = workspace / "Cargo.toml"
manifest.write_bytes(
    manifest.read_bytes()
    + f'\n[patch.crates-io]\nh2 = {{ path = "{h2.as_posix()}" }}\n'.encode("utf-8")
)
(workspace.parent / "expected-protocol.lock").write_bytes(
    expected_lock(read_utf8(workspace / "Cargo.lock")).encode("utf-8")
)
with Path(os.environ["GITHUB_ENV"]).open("ab") as output:
    for name, value in {
        "ALLOY_PROTOCOL_WORKSPACE": workspace.as_posix(),
        "ALLOY_PROTOCOL_H2": h2.as_posix(),
        "ALLOY_BENCH_H2_PATCH_SHA256": digest(diagnostics / "h2-0.4.19.patch"),
        "ALLOY_BENCH_OBSERVER_PATCH_SHA256": digest(diagnostics / "observer.patch"),
    }.items():
        output.write(f"{name}={value}\n".encode("utf-8"))
