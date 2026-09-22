# BRAIN — titi project state

> The code is the source of truth, not past reports. Agents read this before
> substantial work. It is updated only through `docs/SYNTHESIS_PROMPT.md`
> after an audit (`docs/AUDIT_PROMPT.md`, skill `brain-audit`). Day-to-day task
> status lives in `docs/research/STATE.md`, not here.

## What this is and why

A terminal coding agent in Rust that follows a reference product's model
(one loop, one session, one tool set) without Electron. One UI-free engine
(`titi-engine`) serves the TUI and headless JSONL; a GPUI desktop comes later
(PLAN M9). Built by two owners pushing straight to `master`.

## Current state — honest scorecard

As of 2026-09-23, from README, STATE.md and CI. Not yet audited.

| Area | State | Evidence |
| --- | --- | --- |
| Version / phase | `0.1.0`, E3 · Genome in progress | README |
| Tests | ~935 passing on CI | README table; CI run on `77dbbc9` green |
| CI | fmt, clippy (informational), test on GitHub Actions | `.github/workflows/ci.yml` |
| Surfaces | ratatui chat + headless JSONL work; GPUI not started (gated) | README, PLAN |
| Providers | OpenAI, OpenRouter, OpenCode, Anthropic built in | README |
| Live model turn | needs a user-provided key in `~/.titi/agent` | README |

## Stack and architecture

See `AGENTS.md` → Stack and layout. Rule: surfaces send `EngineCommand` and
paint `EngineEvent`; they never call a model.

## Status per module

Unknown until the first audit; see `docs/research/STATE.md` for per-task status.

## Known tech debt

| Severity | Item | Notes |
| --- | --- | --- |
| medium | `goal-loop` branch (goal loop, AGENTS.md injection, skills list, modelRoles) not in `master` | conflicts with `crates/titi-cli/src/chat.rs` |
| low | clippy runs without `-D warnings`, workspace `[lints]` not on every crate | deliberate: no new gates without asking |
| low | STATE.md mixes Russian history and English notes | readability only |

## Known risks

- Empryo is a closed, untrusted binary; never vendor, run, or download it.
- A stale clone can hold commits that differ from `origin/master` only by
  hash; re-sync with fetch + rebase, never force-push over it.

## Audit history

| Date | Auditor | Summary |
| --- | --- | --- |
| — | — | no audit yet |
