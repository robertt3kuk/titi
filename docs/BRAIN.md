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

Audited 2026-10-08 at `3aa6774` (2026-10-05) by two readers:
[`audits/2026-10-08-general.md`](audits/2026-10-08-general.md) (areas 1, 2,
5–10) and [`audits/2026-10-08-security.md`](audits/2026-10-08-security.md)
(areas 3–4); every number below comes from
[`audits/2026-10-08-facts.md`](audits/2026-10-08-facts.md). The three audit
files are committed in `8848055`. HEAD moved while this was synthesized
(`3aa6774` → `af985e1`): all five of the fixes that were landing in parallel
are committed — `899c89a`, `af985e1`, `e7c48e1` + `fe1abf2`, `8b33ed9` (whose
bearer header needed `d24f5b1`) and `cbb8abd` — and their rows are struck
through below.

| Area | State | Evidence |
| --- | --- | --- |
| Version / phase | `0.1.0`; README still says "Phase 3–4 · tools, providers, agents — in progress" | `README.md:9` |
| Tests | **1805 passed / 0 failed**, summed from the 68 `test result:` lines of the CI `test` job; 11 crates | facts §3, §5 |
| CI | `fmt`, `clippy` (informational, never denied), `test --locked`, `tui-smoke`; HEAD `3aa6774` green in run `37267636467` | `.github/workflows/ci.yml`; facts §3–4 |
| Tree | clean and in sync with `origin/master`; 83 commits since 2026-10-02 (12 / 49 / 16 / 6 across four days) | facts §1–2 |
| Comment markers | 0 `TODO`/`FIXME`/`XXX` across `crates/**/*.rs` | facts §5 |
| Dependency security | **UNVERIFIED** — `cargo audit` is not installed, so *no* advisory was checked. This is a hole in the audit, not a clean bill | facts §6 |
| Live model turn | needs a user-provided key in `~/.titi/agent` (`titi --set-key`) | `README.md` |
| AGENTS.md rules | the engine boundary and the approval tiers hold; the "no `unwrap`/`expect`/`panic!` outside tests" rule is a convention, not a lint, in 8 of 11 crates | area 2 below |

## Stack and architecture

See `AGENTS.md` → Stack and layout. Rule: surfaces send `EngineCommand` and
paint `EngineEvent`; they never call a model.

## Status per module

Rows are the ten audit areas, reconciled against the code (see *Audit history*
for what was rejected). Pointers are into `crates/`.

| Area | State | Evidence |
| --- | --- | --- |
| 1. Engine boundary | holds | `titi-tui/Cargo.toml:1-25` carries no provider/HTTP dependency; `titi-cli/src/main.rs:334` calls `chat::run`; `chat.rs:28,770` consume only `EngineCommand`/`EngineEvent`; `engine.rs:9` builds config from `titi_engine` types |
| 2. Errors | typed errors exist, enforcement does not | `titi-engine/src/lib.rs` exports typed errors; ~97 fallible `src` APIs return `Result<…, String>`; `[lints] workspace = true` sits in only 3 of 11 manifests, so `unwrap_used`/`expect_used` never run on the other 8; ≈36 `unwrap`/`expect`/`panic!` outside test modules, ~19 of them behind the module-wide allow at `titi-genome/src/parse.rs:3` |
| 3. Secrets | OK at rest, leaking on 2 of 4 egresses | mode 0600 on `auth.db` (`titi-secrets/src/store.rs:226-229`) and masked `Debug`/errors; but `titi-engine/src/difftrack.rs:279-288` drops only `.env`/`*.pem`/`*.key`, `titi-memory/src/redact.rs:349-370` missed quoted and `_`-suffixed names, and `titi-engine/src/tool_loop.rs:167-171` records raw args |
| 4. Tools and approval | OK | one gate at `titi-engine/src/tool_loop.rs:320-350`; the jail is real (`titi-tools/src/fs.rs:26-63`, symlinks never followed); `yolo` skips only the prompt, not the jail; the settings guard is prefix- and case-aware (`titi-tools/src/settings.rs:159-165`); bash confinement is cwd-only by design (Exec tier) |
| 5. Providers | retry semantics correct, watchdogs and cancel broken | only 429/5xx are retried and 401/unknown-model never (`titi-providers/src/wire.rs:949-965`), only before visible content (`titi-engine/src/runtime.rs:1933-2020`); but `transport.rs:190-212` declares timeouts nothing reads, `http.rs:53-55` builds a client with no timeout, the ordinary read is a bare `body.next().await` (`wire.rs:776`), and `RequestCtx::aborted` is never read outside tests |
| 6. Persistence | unsafe on crash and on concurrent access | no `sync_all` anywhere in `titi-core` plus three in-place truncating rewrites (`session/store.rs:102-107,215,262,281`); `titi-config/src/config_file.rs:176-185` runs the guarded write after a failed acquire and then unlinks the lock file; `session/store.rs:434-438` skips any unparsable line; no `PRAGMA user_version` in any store |
| 7. TUI | width, resize and kitty fallbacks good; a panic loses its own message | `titi-tui/src/width.rs:6,138-235` (unicode-width), `chat.rs:3214-3223` (resize invalidates), `chat.rs:3362-3370` (`Screen::Drop` restores terminal state); there is no `std::panic::set_hook` in the workspace |
| 8. Tests | broad, but partly aimed at unreachable code | 11/11 crates have tests and 1805 pass; `titi_cli::app::App` is unreachable from the binary yet carries five integration test files; the default test chat used the real `~/.titi/agent`; `tests/first_frame.rs:58,79-82` and three more files assert wall-clock |
| 9. Dependencies | 5 unused or over-declared direct deps; duplicates transitive only | `titi-tui/Cargo.toml:8,13`, `titi-tools/Cargo.toml:17`, `titi-core/Cargo.toml:10`, `titi-cli/Cargo.toml:25` vs `:29`; 21 duplicate transitive roots; RustSec unverified |
| 10. Docs drift | AGENTS.md ↔ CI aligned; README ↔ code drifted | `AGENTS.md:44-47` matches `ci.yml:24,34,42,51`; `README.md:99,174,303,378` vs `chat.rs:1275-1288,3512-3527`; `README.md:205-208` weaker than CI; `README.md:9` still claims 1279 passed |

## Known tech debt

Severities are the audit's, reconciled here (critical = data loss, secret leak
or security hole; high = wrong behaviour on a main path).

| Severity | Item | Notes |
| --- | --- | --- |
| critical | ~~A test deletes the developer's real provider key — `titi-cli/src/chat.rs:10969` builds its chat with the bare helper, so `/logout openai` reaches `~/.titi/agent/auth.db`~~ fixed 2026-10-08 in 899c89a (the bare helper now builds on a temp dir, and a test pins that it never points at the real agent dir) | data loss when running `cargo test -p titi-cli`; kept for one more cycle |
| critical | ~~The `git diff HEAD` block leaks credential files the read tools refuse — `titi-engine/src/difftrack.rs:279-288` (+ `runtime.rs:179-209,1816`): a modified `id_rsa` or service-account JSON reaches the provider~~ fixed 2026-10-08 in af985e1 (one `SensitivePolicy` for both paths, a redact pass per section, angle-bracket paths dropped and headers sanitised) | automatic secret egress; kept for one more cycle |
| high | ~~Tool/output masking misses quoted and `_`-suffixed key names — `titi-memory/src/redact.rs:349-370`~~ fixed 2026-10-08 in 8b33ed9, and the bearer header's prefilter in d24f5b1 (the pattern matched only a lowercase `bearer`, so a real `Authorization: Bearer …` never reached it) | `{"api_key": …}`, `client_secret:`, `AWS_SECRET_ACCESS_KEY=`, `Authorization: Bearer …`; kept for one more cycle |
| high | Error lints are inert in 8 of 11 crates — `Cargo.toml:38-43`; only `titi-engine`, `titi-providers` and `titi-genome` opt in | `titi-cli/tests/login_oauth.rs:416,430` (`unsafe env::set_var`) must be rewritten first, since `unsafe_code = "forbid"` cannot be allowed back; clippy still runs without `-D warnings` on purpose |
| high | Session files are written without `fsync` and rewritten in place — `titi-core/src/session/store.rs:102-107,215,262,281` | reuse the tmp + `sync_all` + rename pattern of `titi-memory/src/store.rs:291-304` |
| high | ~~The `fd-lock` helper runs the guarded write after a failed acquire, then unlinks the lock — `titi-config/src/config_file.rs:176-185`~~ fixed 2026-10-08 in cbb8abd (the body runs only under a held lock, a refused lock is an error to the caller, and the lock file survives as the rendezvous point) | advisory lock in name only; kept for one more cycle |
| high | A stalled provider hangs the turn and `Cancel` cannot unblock it — `titi-providers/src/transport.rs:190-212`, `http.rs:53-55`, `wire.rs:770-777` | `TransportError::Stalled` is produced by tests only; `RequestCtx::aborted` is never read in production |
| high | `titi_cli::app::App` (2492 lines) is unreachable from the binary and kept alive by five integration test files | decide: wire it in or delete it with the test files |
| medium | Raw tool arguments are persisted unmasked and the file mode is not set — `titi-engine/src/tool_loop.rs:167-171`, `titi-core/src/trajectory.rs:99-107` | the result is masked two blocks later; `write` is the only content tool without a `policy` field (`titi-tools/src/fs.rs:431-437`) |
| medium | A cloned repository's `.env` outranks the user's own credential layers — `titi-secrets/src/env.rs:37-48` | every other protected setting reads past the project layer |
| medium | The README slash table is wrong in three ways — `README.md:99,303` document a `/skillful` that exists nowhere; `README.md:174,378` deny the council slash command that `chat.rs:1275` dispatches; `/genome` and `/git` are missing | |
| medium | Stringly-typed errors and discarded writes at the CLI seam — ~97 `Result<…, String>`; `titi-cli/src/session_log.rs:25` returns `Option<Self>`; `headless.rs:198,227,252,292,314,336` drop write failures; `headless.rs:218,270,329` can emit a blank JSONL line | |
| medium | A retry can replay thinking deltas — `titi-providers/src/stream.rs:140-148`, `titi-engine/src/runtime.rs:2002-2005` | no answer text is duplicated |
| medium | `fallback_chain` / `fallback_cooldown` are dead config — `runtime.rs:489-491,530-533`, `titi-config/src/fallback.rs:46-49` | `FallbackChain::select` is called from tests only |
| medium | A panic while the screen is owned erases its own message — no `set_hook`; `chat.rs:3362-3370` | the exit path restores terminal state correctly |
| medium | ~~The default test chat is not isolated — `chat.rs:539`, bare helper `chat.rs:7295-7299`; 136 tests build on it~~ fixed 2026-10-08 in 899c89a (same change as the critical test row) | in-flight at the time of the audit |
| medium | Wall-clock assertions that can flake — `tests/first_frame.rs:58,79-82`, `titi-tools/src/pipe.rs:306`, `pty.rs:423`, `titi-engine/tests/tools.rs:953` | `:79-82` only re-measures the fixture's own 2 s sleep |
| medium | The headless JSONL wire protocol is serde-tested on one sample — `tests/headless.rs:8-20` | neither `EngineCommand` nor `EngineEvent` is `#[non_exhaustive]` |
| low | Five direct dependencies are unused or over-declared — `titi-tui/Cargo.toml:8,13`, `titi-tools/Cargo.toml:17`, `titi-core/Cargo.toml:10`, `titi-cli/Cargo.toml:25` | `tempfile` is declared in both `[dependencies]` and `[dev-dependencies]` |
| low | A blanket `#![allow(clippy::expect_used)]` hides a caller-supplied pattern — `titi-genome/src/parse.rs:3,553` | scope the allow to the `LazyLock` items |
| low | The lenient JSONL reader launders mid-file corruption into missing entries — `titi-core/src/session/store.rs:434-438` | be lenient only on the last line |
| low | No schema version in either SQLite store or the session entry — `session/index.rs:71-88`, `titi-secrets/src/store.rs:219-234`, `titi-memory/src/index.rs:106-108` | set `user_version` |
| low | The README's local verify commands are weaker than CI — `README.md:205-208,409-412` | copy the AGENTS.md block |
| low | The engine boundary is guarded by a manifest substring test — `titi-tui/tests/dependency_rule.rs:14-27` | cannot catch a transitive path to a provider |
| low | `write` can plant code that runs later — `titi-tools/src/fs.rs:469`; `.git` sits inside the jail | refuse `.git/` for write/edit |
| low | Checkpoint commits ignore the `SensitivePolicy` — `titi-cli/src/git_checkpoint.rs:17-40` | a staged `.env` gets committed |
| low | ~~The `<diff>` frame around the snapshot is not sanitised — `runtime.rs:204-209`, `difftrack.rs:152-158,168-183`~~ fixed 2026-10-08 in af985e1 (`sanitize_headers` plus angle-bracket paths dropped) | prompt injection only |
| low | ~~`titi genome check` reports false positives on a clean tree — 13 `syntax-error` + 19 `unresolved-import` (facts §7)~~ fixed 2026-10-08 in e7c48e1 + fe1abf2 (documented in b1fe30a); a re-run reports 10 `ambiguous-symbol` and neither of the two codes, and since 4c2f465 only a real problem exits non-zero: those ten are informational and the gate exits 0 on a clean tree | the remaining 10 are same-name types in two crates each (`AgentState`, `Entry`, `Role`, `VERSION`, …); whether the checker should call those ambiguous is not settled here |
| low | The mask consumes the credential's key name, so a masked line can end with a dangling quote — `titi-memory/src/redact.rs:349-370` | `"password": "…"` → `[redacted]"`; preserving the key needs a per-pattern capture template (Rust's regex has no lookbehind) |
| low | The README header test count is stale — `README.md:9` says 1279 passed (2026-09-23); CI says 1805 | |
| low | STATE.md mixes Russian history and English notes | readability only |
| medium | ~~`goal-loop` branch (goal loop, AGENTS.md injection, skills list, modelRoles) not in `master`~~ fixed 2026-09-23 in 324c6eb (goal loop shipped on `master` as `/goal`) | kept for one more cycle |

## Known risks

- Empryo is a closed, untrusted binary; never vendor, run, or download it.
- A stale clone can hold commits that differ from `origin/master` only by
  hash; re-sync with fetch + rebase, never force-push over it.
- Three secret egresses were guarded by content heuristics rather than one
  policy (the `git diff` block, masked tool output, the trajectory file); the
  diff egress now shares the tools' `SensitivePolicy` and redacts (`af985e1`),
  but tool output and the trajectory file still depend on the mask patterns
  alone — and the trajectory records raw args before masking.
- Dependency advisories are unchecked: no `cargo audit` was available, so
  RustSec status is unknown, not clean.
- `titi genome check` exits 0 on a clean tree with 10 informational
  `ambiguous-symbol` lines (`4c2f465`), so it is usable as a gate; the ten are
  same-name types in two crates each, which the checker does not yet call noise.

## Audit history

| Date | Auditor | Summary |
| --- | --- | --- |
| 2026-10-08 | `audit-general` (areas 1, 2, 5–10) + `audit-security` (areas 3–4), facts from `2026-10-08-facts.md` | First full audit at `3aa6774`: CI green (1805/0), boundary, tools and retry semantics hold; 2 critical (a test deletes the real stored key; the `git diff` block leaks credential files to the provider) raised from the auditors' `high` by the definitions, 6 high, one dead 2492-line `app.rs`; no critical in the general half. Rejections carried: "no picture fallback on non-kitty terminals" is the documented contract, not a defect, and the `titi-tui/src/theme/tests.rs` occurrences are a `#[cfg(test)]` module; one measurement corrected — the facts file's 451 `unwrap` + 507 `expect` are raw `src` counts with inline test modules included (outside them ≈36, ~19 behind the `genome/parse.rs` allow), and RustSec is unverified because `cargo audit` is absent. Both criticals were fixed the same day (`899c89a`, `af985e1`); one fix landed with a test its own pattern could not satisfy (`8b33ed9`), unseen because the range was unpushed, and pinned to the real contract in `98d40bb` |
