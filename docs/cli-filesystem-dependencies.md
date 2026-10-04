# CLI filesystem dependency qualification

Issue #134 / GHSA-68jq-pr65-chjv uses the safe `cap-std` and `cap-fs-ext`
4.0.3 APIs. Only the CLI depends on them. Workspace `unsafe_code = "forbid"`,
MSRV 1.94, default/optional integration boundaries, and Linux/macOS/Windows
support remain in effect. This is unpublished development work, not a claim
about a patched release. Edge pairing and G01 contracts are unchanged.

## Static provenance

The following published archives were downloaded from `static.crates.io`,
unpacked with `tar`, and inspected as source without executing their code or
build scripts. Each SHA-256 was calculated with `shasum -a 256` and matched
the corresponding non-yanked version's `cksum` in the official sparse index
at `https://index.crates.io`. The crates.io JSON API returned HTTP 403, so
the registry index and published archives supplied the metadata instead.

| Archive | SHA-256 |
|---|---|
| [cap-std 4.0.3](https://static.crates.io/crates/cap-std/cap-std-4.0.3.crate) | `c1ec78e242cfa2cfe276807ac2ecc00315a6c97786977414bcd1c3963b6c91b8` |
| [cap-fs-ext 4.0.3](https://static.crates.io/crates/cap-fs-ext/cap-fs-ext-4.0.3.crate) | `56ff379b70af8e08307a8f65e7040c7301cb4a572538ade16b4984f0da77847f` |
| [cap-primitives 4.0.3](https://static.crates.io/crates/cap-primitives/cap-primitives-4.0.3.crate) | `8b5f74729fd2f44701d1a8eb47e906cdb3ccd9ec0f02baad85a744b791940b18` |
| [ambient-authority 0.0.2](https://static.crates.io/crates/ambient-authority/ambient-authority-0.0.2.crate) | `e9d4ee0d472d1cd2e28c97dfa124b3d8d992e10eb0a035f33f5d12e3a177ba3b` |
| [fs-set-times 0.20.3](https://static.crates.io/crates/fs-set-times/fs-set-times-0.20.3.crate) | `94e7099f6313ecacbe1256e8ff9d617b75d1bcb16a6fddef94866d225a01a14a` |
| [io-extras 0.19.0](https://static.crates.io/crates/io-extras/io-extras-0.19.0.crate) | `20fd6de4ccfcc187e38bc21cfa543cb5a302cb86a8b114eb7f0bf0dc9f8ac00f` |
| [io-lifetimes 2.0.4](https://static.crates.io/crates/io-lifetimes/io-lifetimes-2.0.4.crate) | `06432fb54d3be7964ecd3649233cddf80db2832f47fec34c01f65b3d9d774983` |
| [io-lifetimes 3.0.1](https://static.crates.io/crates/io-lifetimes/io-lifetimes-3.0.1.crate) | `2f0fb0570afe1fed943c5c3d4102d5358592d8625fda6a0007fdbe65a92fba96` |
| [maybe-owned 0.3.4](https://static.crates.io/crates/maybe-owned/maybe-owned-0.3.4.crate) | `4facc753ae494aeb6e3c22f839b158aebd4f9270f55cd3c79906c45476c47ab4` |
| [rustix-linux-procfs 0.1.1](https://static.crates.io/crates/rustix-linux-procfs/rustix-linux-procfs-0.1.1.crate) | `2fc84bf7e9aa16c4f2c758f27412dc9841341e16aa682d9c7ac308fe3ee12056` |
| [winx 0.36.4](https://static.crates.io/crates/winx/winx-0.36.4.crate) | `3f3fd376f71958b862e7afb20cfe5a22830e1963462f3a17f49d82a6c1d1f42d` |

## API and platform assessment

The published cap-std README explicitly supports Linux, macOS and Windows.
The inspected interfaces are `Dir::open_ambient_dir`, `DirExt::open_dir_nofollow`,
`Dir::create_dir`, `Dir::entries`, `Dir::open_with`, `Dir::symlink_metadata`,
`Dir::rename`, `Dir::remove_file`, and owned `File` methods. No Alloy unsafe code or raw descriptor aliases are needed.

Every untrusted directory open gets exactly one normal component. The ordinary
`Dir::open_dir` API permits confined symlinks and is deliberately not used.
Unix opens use no-follow flags and descriptor-relative operations. On Windows,
cap-primitives `open_unchecked.rs` / `create_file_at_w.rs` use a root directory
handle and `FILE_OPEN_REPARSE_POINT`, then check the opened object's metadata
to enforce no-follow. `dir_utils.rs` and `oflags.rs` exclude `FILE_SHARE_DELETE`
on directory handles. Windows rename/removal implementations derive paths
from these locked handles; retaining the entire acquired chain prevents an
internal directory substitution while those operations run. Tests must verify
that Windows denies a rename while held and allows it after the command drops
the handles. Unix tests actually move the directory and install a symlink while
the writer waits, then verify writes and renames still use the original object.

The three cap crates have no `rust-version` field in their published manifests.
Their [v4.0.3 CI definition](https://github.com/bytecodealliance/cap-std/blob/v4.0.3/.github/workflows/main.yml)
specifies MSRV 1.70 and native Linux/macOS/Windows jobs. This is upstream
configuration evidence, not a claim that those jobs or Alloy's new graph passed.
Published manifests declare Rust 1.70 for io-extras/io-lifetimes 3, and 1.63 for
io-lifetimes 2, rustix-linux-procfs and winx. Ambient-authority, fs-set-times and
maybe-owned do not declare an MSRV. Their source/manifests were inspected;
Alloy's exact locked graph still requires its hosted MSRV 1.94 gate.

All new licenses are allowed by the existing policy: Apache-2.0 with the LLVM
exception, Apache-2.0 or MIT. No license/advisory exceptions were added.

## Manual lockfile graph and required hosted gates

The lockfile was edited from the published manifests/index, without running
Cargo. The normal and platform dependency edges added are:

- cap-fs-ext → cap-primitives, cap-std, io-lifetimes 3, windows-sys 0.61.2.
- cap-std → cap-primitives, io-extras, io-lifetimes 3, rustix.
- cap-primitives → ambient-authority, fs-set-times, io-extras, io-lifetimes 3,
  ipnet, maybe-owned, rustix, rustix-linux-procfs, windows-sys 0.61.2, winx.
- fs-set-times → io-lifetimes 2, rustix, windows-sys 0.52.0.
- io-extras → io-lifetimes 3, windows-sys 0.52.0.
- rustix-linux-procfs → once_cell, rustix.
- winx → bitflags, windows-sys 0.52.0.
- ambient-authority, both io-lifetimes versions and maybe-owned have no active
  normal dependency edges under the selected features.

The existing bitflags, ipnet, once_cell, rustix and Windows packages satisfy
these requirements and retain their versions/checksums. cap-fs-ext enables only
`std`; cap-std defaults are empty; optional UTF-8/async integrations and
io-lifetimes `close` are not enabled. Unix rustix filesystem/process/termios/time
features add no packages beyond those already locked. The existing runtime
socket2 dependency and its owned-handle guard remain unchanged.

No local format, compile, lint, test, MSRV or cargo-deny command was executed.
Before merge, the existing GitHub-hosted gates must pass with `--locked`,
including formatting, both clippy configurations, both feature test suites on
Linux/macOS/Windows, MSRV 1.94, dependency/license auditing and generated-project
checks. Static inspection and `git diff --check` alone do not prove compilation
or platform behavior.
