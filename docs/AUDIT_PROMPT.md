# AUDIT_PROMPT — request a full audit of titi

Paste everything below into any capable model that can read the repo
(or run it as skill `brain-audit`). Output goes to
`docs/audits/<YYYY-MM-DD>-<auditor>.md`, then through `SYNTHESIS_PROMPT.md`.

---

You are auditing **titi**, a terminal coding agent in Rust (edition 2024,
workspace in `crates/`). Read `AGENTS.md` first, then the code. Trust the code
over any doc, including `docs/BRAIN.md` and `docs/research/STATE.md`.

Check, with file:line evidence for every claim:

1. **Engine boundary** — no surface (`titi-cli`, `titi-tui`) calls a provider
   directly; all goes through `EngineCommand`/`EngineEvent`.
2. **Errors** — typed errors at fallible APIs; any `unwrap`/`expect`/`panic!`
   outside tests; errors swallowed or stringly-typed.
3. **Secrets** — keys only via `titi-secrets` / `~/.titi/agent`; nothing
   logged, persisted to sessions/memory, or committed; memory masking works.
4. **Tools and approval** — write/bash tools gated by approval tiers;
   `--approval yolo` scope; path traversal outside the workspace.
5. **Providers** — retry/fallback only before the first visible token; 401 and
   unknown model never retried; cancel unblocks everything.
6. **Persistence** — session/SQLite schema changes, corruption handling,
   concurrent access (`fd-lock`).
7. **TUI** — width handling (UAX#11), resize, terminal state restored on
   panic/exit, kitty graphics fallbacks.
8. **Tests** — gaps against each crate's contract; tests that assert nothing;
   flaky timing.
9. **Dependencies** — unused crates, RustSec advisories, duplicated versions.
10. **Docs drift** — AGENTS.md commands vs `ci.yml`; README claims vs code.

Output format:

```
## Summary (5 lines max)
## Scorecard: area | state | evidence
## Findings: [critical|high|medium|low] title — file:line — why — fix
## Tech debt not worth fixing now
## Unverified: what you could not check and why
```

Never invent numbers. Mark anything you did not verify as an estimate.
