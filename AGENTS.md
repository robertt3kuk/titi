# AGENTS — entry point for coding agents

titi is a terminal coding agent written in Rust: one loop, one session, one
set of tools. The same engine drives a full-screen TUI and a headless JSONL
interface; a native GPUI window comes later. Public repo `robertt3kuk/titi`
with two owners (Beka and nvimq); everyone works straight on `master`.

omp, Codex, Cline, grok, and Antigravity load this file on their own. Claude
Code loads it through `CLAUDE.md` (`@AGENTS.md`), Gemini CLI through
`.gemini/settings.json`. Do not copy these rules into per-agent files.

## Canon

- How one task runs: `docs/CONVEYOR.md` (take it from STATE → research →
  contracts → tests → code → CI → record). After 3 failed attempts on the
  same artifact, stop and ask the user.
- Milestones: `docs/PLAN.md`. Where to resume: `docs/research/STATE.md`.
  Research map: `docs/research/README.md`.
- Project brain (health, tech debt, risks): `docs/BRAIN.md`. Read it before
  substantial work. Index of all docs: `docs/README.md`.
- Commits and push: `docs/COMMITS.md`.

## Stack and layout

- Rust edition 2024, resolver 3, stable toolchain (`rust-toolchain.toml`).
- `crates/` holds the workspace members (see `Cargo.toml`):
  - `titi-engine` — the UI-free runtime: `EngineCommand`/`EngineEvent`, the
    turn loop, subagents, the reviewer. Surfaces never call a model directly.
  - `titi-core` sessions and trajectory · `titi-config` layered settings ·
    `titi-secrets` .env and auth store · `titi-providers` HTTP/SSE transports ·
    `titi-tools` read/write/edit/glob/grep/bash plus approval tiers ·
    `titi-genome` ranked repo map · `titi-memory` stores and embeddings ·
    `titi-soul` system prompt and SOUL.md · `titi-tui` ratatui renderer ·
    `titi-cli` the `titi` binary (screen, headless, keys, slash commands).
- Agent state, keys, sessions, and memory live in `~/.titi/agent`, never in
  the repo.

## Verify

CI (`.github/workflows/ci.yml`) is the source of truth, so nothing needs to
compile locally:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets     # informational, not denied
cargo test --workspace --locked
gh run list --repo robertt3kuk/titi --branch master --limit 5
```

One exception: check TUI changes by hand as well, in a real terminal with
`cargo run -p titi-cli`, and record the result in `docs/QA_STATUS.md` (skill
`tui-smoke`). A live model turn needs a provider key that the user sets with
`titi --set-key`. Never ask for the key or handle it yourself.

To map the code without compiling, run `titi-map <path> <N>` (skill
`genome-map`).

## Code style

- Formatting: default rustfmt. Lints: the workspace `[lints]`. Do not add new
  lints, gates, or `-D warnings` without asking.
- Errors: strict. Every fallible API returns a typed error (`thiserror`) and
  validates its inputs. No `unwrap`/`expect`/`panic!` outside tests.
- Comments: only for a *why* that the code does not make obvious.
- Abstractions: nothing premature. Up to about 3 repeats of the same code is
  fine.
- Refactoring: improving nearby code is welcome, but in its own commit, never
  mixed into a feature or fix.
- New code starts with a behavioural test (CONVEYOR step 4). If a change
  breaks an existing test, decide whether the test or the code is wrong and
  explain why in the commit body. Never weaken a test just to make it pass.
- Adding a crate: justify it and check RustSec and maintenance status first
  (skill `dependency-update`).

## Rules for the agent

- Non-trivial task: propose a plan and wait for approval. Small edits: just
  do them.
- Ambiguous request: ask when a wrong guess would be costly to undo.
  Otherwise take the most likely reading and say which one you took.
- Commit and push each focused change straight to `origin master` (skill
  `commit`). Never `--force`, `reset --hard`, or `clean -fd`. On a
  non-fast-forward push: `git fetch && git rebase origin/master`. If the
  rebase conflicts, stop and report.
- Secrets never go into the repo, logs, or tests (tests use `sk-test` and
  `example.invalid`).
- Before calling a task done, run skill `code-review`.
- When a procedure of 3+ steps shows up for the second time, turn it into a
  project skill in `.agents/skills/`: create it, commit it as
  `docs(skills)`, and mention it in your report (skill `skill-authoring`).

### Multi-agent operating model

Roles follow CONVEYOR (Scout / Worker / Reviewer / Integrator). The lead
agent is the supervisor; its playbook is skill `supervisor-review`.
- Workers commit locally; only the supervisor pushes accepted work.
- Workers report to the supervisor after every subtask (in omp: `hub send`).
  Results live in files; messages carry only the paths.
- The supervisor checks the artifacts themselves (diff, tests, CI), not the
  worker's summary. Wrong work goes back to the same worker with file:line
  feedback and is not accepted until it is fixed. After 3 rounds, ask the
  user.
- Only one agent writes a given file at a time. The supervisor hands out
  access to shared files (`Cargo.toml`, `Cargo.lock`, `chat.rs`, the docs
  canon) one agent at a time.

## Boundaries and known risks

- Empryo is a closed, untrusted binary: never vendor it, run it, or download
  it. Re-implement its ideas in open Rust only.
- PLAN gates GPUI (M9) and the MCP/skills/hooks runtime (M6). Do not start
  them early.
- nvimq's local branch `goal-loop` conflicts with `master` in
  `crates/titi-cli/src/chat.rs`; merging it needs a manual resolution.

## Setup metadata

Generated 2026-09-23 by init-project-programming. Skill hash
`24b37a0e3f4400c3ed71c27dd26e83ebdc837849`, manifest hash
`ce8ccb8af6b3b7fcf534530d1ed78fadd0f33169`, computed over `Cargo.toml`,
`crates/*/Cargo.toml`, `rust-toolchain.toml`, and `.github/workflows/ci.yml`.
Questionnaire answers: `.agents/state/init-project-programming.json`.
