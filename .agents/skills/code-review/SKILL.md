---
name: code-review
description: titi project self-review checklist for a titi change before calling it done — error handling, tests, engine boundary, secrets, commit hygiene. Use before reporting a task complete, or when asked to review a diff.
---

# code-review

Run against `git diff` (or `git diff origin/master...HEAD`). Fix what fails,
then report what you checked.

## Correctness and style (AGENTS.md → Code style)
- [ ] Every fallible API returns a typed error (`thiserror`); inputs validated.
- [ ] No `unwrap`/`expect`/`panic!`/`todo!` outside `#[cfg(test)]` and `tests/`.
- [ ] Comments explain only a non-obvious why; no narration.
- [ ] No premature abstraction; no new trait/generic for a single user.
- [ ] Adjacent refactors sit in their own commit, not mixed in.
- [ ] rustfmt-clean shape; no new lints or `#[allow]` without reason.

## Tests
- [ ] New behaviour has a test that fails without the change.
- [ ] Changed existing tests: the commit body says why the old contract was wrong.
- [ ] Tests need no network and no real keys (`sk-test`, `example.invalid`).

## Architecture
- [ ] Surfaces (`titi-cli`, `titi-tui`) talk to the engine only via
      `EngineCommand`/`EngineEvent`.
- [ ] No GPUI or MCP/hooks runtime work (gated by PLAN).
- [ ] Nothing from the Empryo binary vendored or invoked.

## Safety
- [ ] No secret values in code, logs, fixtures, session or memory writes.
- [ ] New dependency → went through skill `dependency-update`.
- [ ] TUI change → skill `tui-smoke` done and `docs/QA_STATUS.md` updated.

## Record
- [ ] `docs/research/STATE.md` status updated if this was a STATE task.
- [ ] Commit(s) follow skill `commit`.
