# Release readiness

Nothing is published. Every crate is `publish = false`, and stays so until the owner explicitly approves a release. This page records what CI already checks, the checklist a release commit must satisfy, and the owner decisions that must be made before the first publish.

## What CI checks now

The `Package dry run` job in `.github/workflows/ci.yml` runs on every pull request and every push to `main`. `cargo package` only builds `.crate` archives under `target/package/`; it never uploads anything.

1. **Package.** `cargo package --locked --no-verify --exclude-lockfile` for the five crates: `ferrum-alloy-diagnostics`, `ferrum-alloy-telemetry`, `ferrum-alloy-edge`, `ferrum-alloy`, and `ferrum-alloy-cli`. The examples are not packaged.
2. **Packaged files.** Every archive contains `Cargo.toml`, `README.md`, `LICENSE`, and `LICENSE-COMMERCIAL.md`, and the license files match the repository's byte for byte. The CLI archive contains every committed file under `crates/ferrum-alloy-cli/templates/`, including `templates/base/.github/`, and the `ferrum-alloy` archive every committed file under `crates/ferrum-alloy/assets/`.
3. **Packaged sources build on their own.** The job unpacks the archives into a new workspace outside the repository, with a `[patch.crates-io]` entry that points each crate at its unpacked archive, and runs `cargo check` with all features and with default features. It fails if any `ferrum-alloy*` crate resolves from a registry instead of the unpacked archives. This catches a file a crate needs but does not package, such as a template compiled in with `include_str!`.

### Why `--no-verify --exclude-lockfile`

The crates depend on each other, and none of them is in a registry. `cargo package` writes a `Cargo.lock` into each archive and, unless `--no-verify` is given, builds each archive; both resolve the sibling crates. Packaging with a lockfile or with verification needs a registry the crates may be published to, and `publish = false` rules that out: Cargo resolves workspace siblings through a temporary local registry, but it does not add `publish = false` packages to it, so it would look for them on crates.io and fail (or, if someone published a crate with the same name, resolve to that crate). `--exclude-lockfile` and `--no-verify` skip both resolutions. Step 3 then does the verification that `--no-verify` skips, with an explicit patch that can never resolve to a registry crate.

Once publishing is approved and `publish = false` is removed, `cargo package --workspace` resolves the siblings through its local registry, and the job can drop both flags.

### Package contents

Each crate declares an `include` list in its `Cargo.toml`: `src/`, its `README.md`, and the two license files; the CLI also includes `templates/`, and `ferrum-alloy` includes `assets/`, the vendored documentation UI (feature `openapi-ui`) with its Apache-2.0 `LICENSE`, `NOTICE`, bundled notices, and hash manifest. Tests are not packaged, because they read fixtures from outside the crate (`contracts/`, `docs/`, the examples). The `LICENSE` and `LICENSE-COMMERCIAL.md` files in each crate directory are symbolic links to the repository's, and Cargo packages their contents.

The internal workspace dependencies carry `version = "0.1.0"` next to `path`, so that the packaged manifests contain a registry dependency on each sibling. Builds in this repository still use the path.

## Release checklist

A release commit must satisfy every item. Record the evidence in the release notes.

- [ ] **Owner approval.** The owner has approved this release, and the [owner decisions](#open-owner-decisions) below are recorded.
- [ ] **Version.** All five crates share `workspace.package.version`. Bump it and the `version` of every internal entry in `[workspace.dependencies]` together. Before 1.0, a breaking change bumps the minor version, and any other change bumps the patch version.
- [ ] **Changelog.** A changelog entry lists the user-visible changes, including changes to configuration (`docs/configuration.md`), the diagnostic evidence schema, and Edge contracts. The repository has no changelog yet; the first release creates one.
- [ ] **MSRV.** The release notes state the MSRV (`rust-version`, currently 1.94, set by sqlx 0.9), and the `MSRV (1.94)` job passes on the release commit. An MSRV increase is called out as a change.
- [ ] **Compatibility.** `docs/compatibility.md` and `docs/compatibility.json` describe the tested matrix on the release commit: the Edge support window (the latest Edge release and the previous one), the pinned images, and the component versions.
- [ ] **Benchmark baseline.** A benchmark run for the release commit is recorded as [benchmarks](benchmarks.md) describes, with its host and limits. No regression budget exists yet (#15), so the baseline is evidence, not a gate.
- [ ] **CI evidence.** Every job in `.github/workflows/ci.yml` passed on the release commit itself, including `Package dry run`, both `Ferrum Edge end-to-end` slots, `Generated projects`, `PostgreSQL integration`, `Dependency policy`, `MSRV`, and `Fuzz smoke`. Link the run in the release notes.
- [ ] **Generated projects.** `ferrum-alloy new` writes a git dependency on Alloy (`--alloy-git`, `--alloy-branch`, `--alloy-tag`, or `--alloy-rev`). Decide whether a published release switches starters to a registry dependency, and update the templates and the generator tests if so.
- [ ] **License policy.** Removing `publish = false` makes `cargo deny check` evaluate the five crates' own `PolyForm-Noncommercial-1.0.0` license, because `[licenses.private] ignore` in `deny.toml` only skips `publish = false` crates. In the same release PR, add a license exception for each of the five crates, or set `[licenses.private] registries` to the chosen registry.
- [ ] **Publish.** Package and publish from a Linux or macOS checkout (`core.symlinks=true`) of the exact commit whose `Package dry run` passed; a Windows checkout turns the symlinked license files into 13-byte text files. Check that each packaged `LICENSE` matches the root file before `cargo publish`. Only after all of the above: remove `publish = false` (or restrict it to the chosen registry), drop `--no-verify --exclude-lockfile` from the package job, and publish in dependency order: diagnostics, telemetry, edge, `ferrum-alloy`, the CLI.

## Open owner decisions

These are not decided by this repository's code and block any publish:

- **Crate and command names, and registry availability.** The names `ferrum-alloy`, `ferrum-alloy-telemetry`, `ferrum-alloy-edge`, `ferrum-alloy-diagnostics`, and `ferrum-alloy-cli`, and the `ferrum-alloy` command, are working names. On 2026-09-27 the crates.io API returned 404 for all five crate names. That is not a reservation, and it must be checked again before a publish.
- **Trademark.** Availability of "Ferrum Alloy" (and of the crate and command names) has not been checked.
- **Registry and license.** Whether crates licensed PolyForm Noncommercial 1.0.0 are published to crates.io at all, or to another registry, or not at all. The manifests declare `license = "PolyForm-Noncommercial-1.0.0"` (an SPDX identifier) and ship `LICENSE-COMMERCIAL.md` alongside; an SPDX expression cannot name the commercial license, so decide whether that is how the dual licensing is presented.
- **License expression for vendored code.** `ferrum-alloy` packages Swagger UI (`assets/swagger-ui/`, Apache-2.0 with bundled MIT, BSD-3-Clause, and DOMPurify code). When publishing, its `license` may need to become `PolyForm-Noncommercial-1.0.0 AND Apache-2.0 AND MIT AND BSD-3-Clause` so the manifest does not claim the vendored files are PolyForm-licensed.
- **Product homepage URL.** The manifests declare only `repository`. Decide whether the crates get a separate `homepage`, and which URL.
- **Publisher.** Who owns the registry entries and how publishing is authenticated.
