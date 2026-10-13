---
name: muse-agents
description: Dispatch and orchestrate local Muse Wonder Full subagents via the standalone Muse Code muse CLI harness for ferrum-alloy issue/PR work — implementer, fix-round, and shepherd modes, with low/medium/high/xhigh/max/ultra reasoning effort, worktree isolation, and the review loop. Use when the user asks Claude to spawn Muse/Muse Wonder Full agents on issues, PRs, review findings, or red CI.
---

# Muse Wonder Full agents

Act as the orchestrator. Read and follow the shared workflow in
[the canonical skill](../../../.agents/skills/muse-agents/SKILL.md), interpreting its Codex
orchestrator role as your Claude orchestrator role. Use the shared launcher and references;
do not duplicate them in this directory.

```bash
<ABS_REPO>/.agents/skills/muse-agents/scripts/dispatch-agent.sh \
  --worktree <ABS_WORKER_WORKTREE> \
  --prompt-file <ABS_PROMPT_FILE> \
  --effort <low|medium|high|xhigh|max|ultra>
```

`--effort` passes straight through as Muse's `--reasoning-effort` (default `high`; `ultra` only
when explicitly requested). Muse Wonder Full has no fast tier; there is no `--fast` flag.

Read the canonical skill before dispatch for preflight, isolation, prompt construction, failure
handling, and verification. For implementer mode, read
[agent-brief.md](../../../.agents/skills/muse-agents/references/agent-brief.md).
For fix-round or shepherd mode, also read
[continuation-brief.md](../../../.agents/skills/muse-agents/references/continuation-brief.md).
Resolve all worker paths to absolute paths. Run each worker in its own background or long-lived
execution session and retain that session's identity. Never recursively invoke this skill from a
dispatched worker.
A worker that exits claiming it is "waiting on monitor" has ended its turn; treat every exit as a
completion and verify its state.

## Remote CI validation

Local cargo runs are permitted in this repository, and hosted CI for the exact pushed head remains
the source of truth. Before every commit, run the checks listed in `AGENTS.md`: `cargo fmt --all`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace --all-features`. When templates change, also run
`cargo test -p ferrum-alloy-cli --test cli -- --ignored`; when dependencies change, run
`cargo deny check`. Docker-based checks (the Edge end-to-end stack and PostgreSQL) and the browser
smoke test run only in CI. Do not run benchmarks or fuzzing unless the controller assigns them.
`git diff --check` is always allowed.

Use remote CI results for the exact pushed head SHA as build/test confirmation. Inspect failed
job logs, fix the demonstrated failure, push the change, and use the next CI run to confirm it.
Pending, skipped, unavailable, or earlier-head checks are not evidence that the change passed.
Keep adding or updating relevant tests; remote CI executes them.

The controller owns post-push CI monitoring unless the worker is explicitly assigned a CI repair
or shepherd round. A worker assigned to exit after pushing must report the head SHA and CI status
as pending or unverified and exit; the controller continues the CI-driven fix loop. Never report
build/test success without matching remote evidence.

Include this validation policy (the local `AGENTS.md` checks before each commit, with remote CI as
the source of truth) in every dispatch prompt, including continuation prompts and any permitted
nested delegation.
