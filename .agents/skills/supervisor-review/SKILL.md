---
name: supervisor-review
description: Playbook for the lead agent in titi that dispatches workers and accepts or rejects their output — verify artifacts not reports, return wrong work to the same worker, serialize shared files. Use when orchestrating subagents or reviewing a subagent's result.
---

# supervisor-review

You are the Integrator from `docs/CONVEYOR.md`. Workers (Scout, Worker,
Reviewer) report to you; you decide what lands on `master`.

## Dispatch
- Each task states: goal, result format, files the worker owns, sources to
  read, and the DoD from the theme doc. Vague tasks duplicate work.
- 1 worker for a simple node, 2–4 for a medium one, cap ~6 per wave.
- Workers commit locally and do not push. Only you push accepted work
  (skill `commit`), so nothing lands on `master` before review.
- File ownership: one writer per file at a time. Shared files (`Cargo.toml`,
  `Cargo.lock`, `crates/titi-cli/src/chat.rs`, `docs/research/STATE.md`,
  `AGENTS.md`) — you grant access one worker at a time and release it when
  that worker reports.

## omp mapping (when omp is the harness)
- Spawn with `task`: implementation → `task`/`grok-builder` (@coding);
  review → `reviewer` (@review); a contested call → `second-opinion`
  (another model family, read-only); secrets/auth/injection → `security`.
- Coordination through `hub`: `send` to a worker by exact roster id,
  `jobs` for a status snapshot, `wait` only when fully blocked.
  Messages are plain prose; artifacts go by `local://`/file path.
- Other harnesses: use their subagent + message primitive the same way.

## Status cadence
- Workers `send` after every subtask: what changed (paths), what is left,
  blockers. No status → ask once via `send`, don't poll with shell tools.

## Accepting work — check the artifact, not the summary
1. Read the diff yourself. Does it match the task and file ownership?
2. Tests exist for the new contract and would fail without the change.
3. Run skill `code-review` against the diff.
4. Before pushing: the reviewer agent's verdict with file:line evidence.
   After pushing: CI green on that commit (skill `ci-and-tests`); red →
   fix forward with a focused commit.
5. "completed" from a worker means it stopped, not that it is correct.

## Rejecting work
- Send it back to the same worker with concrete file:line feedback and the
  failed check. Do not accept partial fixes.
- Disagreement between worker and reviewer → `second-opinion`, then decide.
- 3 rework rounds on the same artifact → stop and escalate to the user with
  what was tried (CONVEYOR stop rule).

## Integrate
- Land accepted work as focused commits (skill `commit`), update
  `docs/research/STATE.md` right after each status change.
