# titi — product review: what is missing

Read-only survey, 2026-10-08. Base: `3c1bc9e` **plus the working tree as it stood** — a sibling wave was
editing `chat.rs`, `engine.rs`, `main.rs`, `session_fs.rs`, `titi-config/settings.rs` and
`titi-tools/pipe.rs` while this was written, and it closed four of the gaps below. Those four rows are
marked **in flight**, are not recommended, and are the reason a few status marks in `omp-parity/GAP.md`
went stale; they landed the same day as `17af487` + `f00ee88` (auto background for `bash`), `da28518`
(bare exit), `1a159cf` (double Escape) and `4ca14d6` (`--continue` and `session.autoResume`), with
`cfa35ce` pinning the status-line golden behind them. `e58ad4c` closed this survey's own first
recommendation (§4) the same day; read both against those commits, not against the tree. `chat.rs` grew
~200 lines during this survey (17,154 at the last look); symbols are the stable pointer, line numbers are
of the moment.

Gates (`PLAN.md`): nothing below needs a paid API, a network service or vendored omp code. M6
(MCP/skills/hooks) and M9 (GPUI) items are not recommended. No program was run — see the appendix.

## 1. The one-minute table

| gap | who feels it | cost | value ÷ cost | gate |
|---|---|---|---|---|
| Approving a network call shows no URL: `fetch`/`web_search` (and `settings`) are the only tools with no `describe()`, so the prompt reads `fetch   y allow    n refuse` | anyone whose turn fetches | `titi-tools/src/web.rs` (+ 3 lines in `settings.rs`) (S) | high ÷ S | ok |
| No search over past sessions, though the FTS5 index exists and is populated on every append | a returning user ("what did we decide about X?") | `chat.rs` (S; `titi-core` API exists) | high ÷ S | ok |
| Sessions record no workspace, so `--continue` and the Ctrl+X switcher are agent-dir-wide | anyone with two projects | `titi-core/src/session/*` + `session_fs.rs` (M, schema + migration) | medium ÷ M | ok |
| No money anywhere: `/budget $2` is refused with "no price table", and no turn or session states a cost | a metered key (opencode-go, openrouter, bai) | `registry.rs`, `engine.rs`, `status.rs`, `chat.rs` (M) | medium ÷ M | ok (static table) |
| 3 of the 16 keys the screen answers are advertised, and there is no `/hotkeys` | a new user, first minute | `chat.rs` (S) | medium ÷ S | ok |
| The session tree is stored (`parent_id`, leaf, `fork`) but only a flat picker shows it; no `/tree` | anyone comparing two approaches | `chat.rs` (S) | medium ÷ S | ok |
| The transcript section model died with `transcript.rs`: no `/details`, no per-section visibility, no fold divider — one note line on compaction | a user after the first compaction | `chat.rs` (+ `titi-tui`) (M) | medium ÷ M | ok |
| Long paste: the 6-line collapse died with `composer.rs`; the whole body is inserted and sent, the composer shows its tail | anyone pasting a stack trace | `chat.rs` (S) | medium ÷ S | ok |
| No `ask` tool — the agent cannot put options to the person | an ambiguous turn | `titi-tools` + `tool_loop.rs` + `chat.rs` (M–L) | medium ÷ L | ok |
| `read` output is unnumbered and never rendered as markdown (omp defaults both off too) | a user reading a long read | `titi-tools/src/fs.rs` (S) | low ÷ S | ok |
| The turn footer always shows time + tokens + cache (omp has three switches) | a user who wants a quiet transcript | `chat.rs` + `titi-config` (S) | low ÷ S | ok |
| **In flight, not counted:** session resume (`--continue`/`session.autoResume`), bare `exit`, Esc-Esc → `/rewind`, auto-background bash | | working tree | — | — |

`chat.rs` is the bottleneck for every UI item left: 17.2k lines, one writer, and the only file most of
the rows above can land in. That is a finding of its own, not a footnote.

## 2. Corrected inventory (GAP.md, re-checked against the tree)

GAP's omp side is sound: every settings id and line number I re-read (`autoResume :30`, `tui.tight :560`,
`tui.vimMode :774`, `display.pinnedAgents :591`, `smoothStreaming :622`, `hideToolActivity :634`,
`collapseCompacted :693`, `paste.largeMenuThreshold :1012`, `readLineNumbers/defaultLimit/renderMarkdown
tools/settings.ts:149,161,180`) still matches. What went stale is the **titi** half, because
`crates/titi-tui/{composer,transcript,renderer,history,viewport,input,cursor,focus,slash,hub,overlay}.rs`
were deleted in `1ff6889` and `titi-genome/src/parse.rs` became `lang/`. Per entry:

| entry | GAP says | now (2026-10-08) | evidence |
|---|---|---|---|
| 6 paste menu | partly — collapse already in `composer.rs:68,132-159` | **worse than "partly": still absent, and the collapse it was credited with is gone.** `Chat::paste` inserts the whole body; `composer_view`/`fit_tail` show the tail of one row; nothing in the live tree mentions a paste marker. New home `chat.rs` | `grep -rn 'Paste\b' crates/titi-cli/src` → login prompts only |
| 9 vim | absent, entry point `composer.rs`, mode chip in `render_box_composer` | **still absent.** Home is `chat.rs` (`map_key`, `on_key`) — i.e. a modal editor inside the serialized ~17k-line file | `grep -rli vim crates/` → one doc comment in `status_bar.rs:17` |
| 15 tight | absent, `composer.rs` + `renderer.rs` padding constant | **still absent.** The padding is `chat.rs` (`Padding::horizontal(1)` in `composer`); the `caps.rs` "tight" hit GAP cited is an unrelated comment | `grep -rn 'Padding::horizontal' crates/titi-cli/src/chat.rs` → 1 |
| 18 read prefs | partly — ranges in, no numbering, no markdown render | **unchanged and smaller than it reads:** omp's `readLineNumbers`/`read.renderMarkdown` default **off**, `read.defaultLimit` 300 (titi 2000). Home `titi-tools/src/fs.rs` | `tools/settings.ts:149,161,180`; `fs.rs` `READ_MAX_LINES` |
| 19 transcript prefs | partly — `/details` visibility exists | **still absent, and the "partly" is gone:** `/details` and the whole section model died with `transcript.rs` (`grep -rn details crates/titi-cli/src` → none). What exists is one `folded N earlier messages` note. New home `chat.rs` | `chat.rs` `EngineEvent::Compacted` arm |
| 12 auto-resume | absent, needs a flag + config read | **in flight (uncommitted):** `--continue`/`-c`, `session.autoResume`, `launch_session`, `newest_session` are in the working tree. GAP's cost was also wrong — the store already had `resume_latest`/`restore_latest` | working-tree diff; `settings.rs` `SESSION_AUTO_RESUME_KEY` |
| 13 bare exit | absent | **in flight:** `/exit`, `/quit`, `quit`/`q`, second-Enter confirm; `--help` already documents it | working-tree diff, `main.rs` usage |
| 14 double-Escape | partly — Esc-Esc backs out of pickers only | **in flight:** `Chat::escape` on an empty composer arms a second press into `/rewind` | working-tree diff, `chat.rs escape` |
| 20 auto-background bash | absent, home `pipe.rs` + engine | **in flight:** `pipe.rs` gained `BACKGROUND_AFTER` (60 s), `TITI_BASH_BACKGROUND_MS`, a `Background` hand-off; the settings key is still to come | working-tree diff, `pipe.rs` |
| 17 pinned agents | partly — hub roster overlay in `titi-tui/hub.rs` | **partly, pointer dead:** the widget went with `hub.rs`; the live `/hub` panel is `chat.rs` + `panels.rs` box drawing, still a toggle, nothing pinned above the composer | `chat.rs` `hub_open`, `/hub` |
| 23 session tree | partly — `/fork` exists, needs parent links | **better than it reads:** the tree is *stored* (`entry.rs` `parent_id`, `store.rs` `fork`, `walk`), only the view is missing | `crates/titi-core/src/session/entry.rs`, `store.rs` |
| 1 footer toggles | partly ported | **correct as written** — the remaining cost is the three `display.*` switches, in `chat.rs` + `titi-config` | `titi-tui/src/status.rs` `TurnFooter` |
| 2,3,4,5,7,8,10,11 | ported | **confirmed ported** (tables, title, notify, OSC 9;4, emoji, LaTeX, presets, tok/s) | `markdown.rs`, `title.rs`, `caps.rs`, `emoji.rs`, `latex.rs`, `status_bar.rs`, `status.rs` |
| 16,21,22,24,25,26,27 | absent | **confirmed absent** (shimmer, `ask`, reactions, mermaid, auto-graph, spelling, `eval` tool) | `grep -rli` per name: 0 hits |

Also stale, not in GAP: the `[Image #N]` paste-attachment counter and the OSC 5522 ingest went with
`composer.rs`; what is left is a path inside a message being drawn as a kitty photo (`chat.rs`
`prepare_photo`, `image_paths`).

## 3. Beyond omp parity (grounded in this codebase)

| need | titi today | smallest honest addition |
|---|---|---|
| Resume the last session | has (in flight) — and titi's is *unconditional* where omp's `autoResume` is opt-in and cwd-scoped (`settings.ts:30`, `session-listing.ts:953`) | scope by workspace (see the missed-workspace row) |
| Branch/tree view | stored: `entry.rs` `parent_id`, `store.rs` `fork`/`walk`; `/fork` exists; no view | `/tree` in `chat.rs` over `store.load` — file only, no schema work |
| Search across sessions | index and query exist and are live (`index.rs:162`, `store.rs:294`, populated at `store.rs:118`); **zero callers** | `/sessions <query>` — a new command (the name is already in the source comments), over the surface the data layer is waiting for |
| Per-project scope | `SessionMeta` carries title/bot/source, no cwd; `session_fs.rs` says so in prose | add `cwd` to the index + one migration; then scope listing, resume, search |
| Cost / usage control | token cap `/budget` (`runtime.rs` budget); no price table, and `/budget $2` says so verbatim | price on `ModelDescriptor` (`registry.rs:34`), `$` in the footer and `/usage`; `/budget $2` becomes possible |
| What a turn cost | tokens, cached share, wall time, cache-miss flag (`status.rs` `TurnFooter`) | the money part above; a subscription user gets nothing (omp shows credits there — `segments.ts:813`; titi has no credit notion) |
| Safety: what you are approving | every tool gives a `describe()` line except three: `fetch`, `web_search` and `settings` — so the one outbound channel is approved blind, and a config write is approved blind too | `describe()` on `FetchTool`/`WebSearchTool` (`web.rs`) and on `SettingsTool` (`settings.rs`) |
| Safety: what a tool can reach | `fetch` checks the scheme only — no private-address guard, 5 redirects followed; `write` jail, `.git` refusal, `.env`/key refusal and output masking all exist | refuse cloud-metadata targets (no legitimate use); note redirect re-check as a separate, larger piece |
| Safety: surprises | `git_commit` (Write tier) can commit to the repo; `settings` writes config inside a per-segment guard | already approval-gated; a `/diagnose` line naming the write tier would be enough |
| Discoverability | `/help` lists 41 commands, one line each; the welcome shows 3 chords and **one** of 20 tips per session | `/hotkeys` listing the 17 live keys, generated from the same table |
| Discoverability, drift | the source's own comments call the session switcher `/sessions` (about six places) but **no such command exists**; typing it prints `unknown command /sessions` | the command #2 adds; then make the comments name Ctrl+X |
| Docs / tests as surface | `ARCHITECTURE.md` 303 L, `QA_STATUS.md` 55 L of manual ledger, README states a measured test count + CI run id; 1775 `#[test]`/`#[tokio::test]` in 166 files (grep, not a run) | no `CHANGELOG.md`; README line 3 and 226 link `https://reference-product.com` — a placeholder that will 404 in the first paragraph a stranger reads |

## 4. Recommend next three

**1. Network approvals name the call, and metadata targets are refused** — `crates/titi-tools/src/web.rs`
(+ three lines in `settings.rs`). The project already fixed this class for `bash` (an approval that named
only the tool was a blind yes). Every tool declares a `describe()` line except `fetch`, `web_search` and
`settings`, so `ToolStarted.detail` is `None` and the prompt falls back to the tool name — the outbound
channel is the one approved blind. User sees `fetch https://docs.rs/serde   y allow    n refuse`, and
`fetch http://169.254.169.254/…` fails with a sentence instead of fetching. Test: `describe()` returns
the masked URL form; a metadata host is refused without a socket being opened; the loopback mocks the
file already uses keep passing (loopback stays allowed — a dev server is a legitimate fetch).

**2. Find an old session from inside the chat: `/sessions <query>`** — `crates/titi-cli/src/chat.rs`
(+ read-only `titi-core`; `SessionStore::search` and the FTS index are already there and populated).
This is a *new* command: the switcher is Ctrl+X only, and the bare `/sessions` the comments promise
prints `unknown command`. User sees `sessions · 3 hits · "prompt cache"` over rows naming session, time
and the matching line; Enter switches to that session; a bare `/sessions` shows the same list Ctrl+X
shows, so the comments become true. Test: two temp sessions with distinct text, assert `/sessions kafka`
names the right one and not the other, and that no hits says so; one PTY smoke per `QA_STATUS.md`.

**3. Money: what the turn and the session cost** — `crates/titi-engine/src/registry.rs` (a `cost` field),
`crates/titi-cli/src/engine.rs` (the built-in table), `crates/titi-tui/src/status.rs` (footer + usage
row), `crates/titi-cli/src/chat.rs` (two call sites; one of them is currently owned by another worker).
User sees `1.4s · 3.4k prompt (2.9k cached) · 250 out · $0.004` and `session $0.83` in `/usage`. Test:
`TurnFooter::row()` prints the figure for a priced descriptor and **omits** the part when the descriptor
has no price (a keyless local model must not print `$0.000`); an assertion that every built-in model id
has a price or is on an explicit no-price list, so the table cannot rot silently.

**Ownership:** #1 is parallel to everything. #2 and #3 both end in `chat.rs`, and #3 also touches
`engine.rs`, which a worker holds today — treat them as **one workstream** (land #2's command, then #3's
call sites), not two. If three *parallel* workers are wanted, the third slot is the `/tree` view or the
paste-collapse restore — both also `chat.rs`, which is the point: the serializer is the constraint.

## 5. Deliberately not recommended

- **Everything M6/M9 gated** (skills/hooks/MCP/extensions runtime, GPUI Workbench) — `PLAN.md`.
- **In flight today** — resume/`--continue`, bare exit, Esc-Esc, auto-background bash; recommending them
  would duplicate a landed-in-an-hour wave.
- **Vim modal editing (#9)** — a state machine plus operators/motions in the one serialized file; the
  users who want it will say so louder than the cost justifies now.
- **Mermaid as ASCII (#24), auto-graph (#25)** — large, and both want the same picture-rendering budget
  as the image stack, which is unmeasured here.
- **Spelling/autocomplete (#26)** — platform-coupled (macOS dictionary) and the model-based path
  downloads weights: a network dependency, i.e. against the gate.
- **`eval` tool (#27)** — kernel lifecycle + isolation, and it drags in the workpool/subagent runtime.
- **omp's `/stats` dashboard** — it launches a local HTTP server; use the terminal, not a service.
- **`find.enabled`** — auto-enables only on omp's native judge model; a paid provider, against the gate.
- **`snapcompact.*`, `worktree.*`, `share.*`, `hindsight`, `loop.*`** — titi has an equivalent
  (`compaction.rs`, git checkpoints, `titi-core/src/share.rs`, `titi-memory`, `/loop`) or no matching need.
- **`read` line numbers (#18)** — omp ships it off by default and titi's transcript already reads fine;
  the one thing worth having there is the money figure, not the gutter.

## 6. Evidence appendix

Every titi claim above is a grep/report or a file read; every omp claim is a source read. Nothing was
executed (no `cargo`, no `titi`, no `omp`), so all omp statements are source-level, and the workspace
test count is a grep of test attributes, not a green run.

titi (all relative to the repo root):

```
ls crates/titi-tui/src                      # 18 modules; composer/transcript/renderer/hub/slash/… gone
git show --stat 1ff6889                     # the deletion: 5755 lines, 11 tui modules + tests
grep -rn 'Paste\b' crates/titi-cli/src      # paste collapse gone with composer.rs
grep -n 'Padding::horizontal' crates/titi-cli/src/chat.rs
grep -rn details crates/titi-cli/src        # no /details anywhere
grep -rn 'EngineEvent::Compacted' crates/titi-cli/src/chat.rs
grep -rn 'describe(' crates/titi-engine/src/tool_loop.rs   # detail ← handler.describe(&args)
grep -n 'fn describe' crates/titi-tools/src/*.rs   # every tool but fetch/web_search/settings has one
grep -n 'scheme()' -A6 crates/titi-tools/src/web.rs          # scheme check only
grep -n 'pub fn search' crates/titi-core/src/session/{index,store}.rs
grep -rn '\.search(' crates/ --include=*.rs    # no user-facing caller: the two hits are titi-memory and the tool itself
grep -n 'pub struct ModelDescriptor' -A12 crates/titi-engine/src/registry.rs
grep -n 'pub const .*KEY' crates/titi-config/src/settings.rs    # 12 keys, incl. session.autoResume
grep -n 'const WELCOME_CHORDS' -A1 crates/titi-cli/src/chat.rs  # 3 chords
grep -n 'const WELCOME_TIPS' -A25 crates/titi-cli/src/chat.rs   # 20 tips
awk 'NR>=4627 && NR<=4792 && /^        name: "/' crates/titi-cli/src/chat.rs   # 41 commands
grep -n '"sessions"' crates/titi-cli/src/chat.rs    # none — the switcher is Ctrl+X only
grep -n 'unknown command /' crates/titi-cli/src/chat.rs
grep -n 'fn parse_budget' -A4 crates/titi-cli/src/chat.rs   # '$' → BudgetArgError::Money
grep -c '#\[test\]\|#\[tokio::test\]' crates/ (per file)        # 1775 attributes in 166 files
git status --short; git diff --stat          # the in-flight wave
```

omp (`~/.bun/install/global/node_modules/@oh-my-pi/`, read only — never run):

```
pi-coding-agent/src/modes/settings.ts:30         autoResume ("the most recent session in the current directory")
pi-coding-agent/src/modes/settings.ts:560,774,591,622,634,693,1012   tight/vim/pinned/smooth/tools/collapse/paste
pi-coding-agent/src/tools/settings.ts:149,161,180,278   readLineNumbers(false)/defaultLimit(300)/renderMarkdown(false)/toolResultPreview
pi-coding-agent/src/tools/ask.ts:101             the ask tool
pi-coding-agent/src/exec/settings.ts:108,250     bash.autoBackground.enabled / thresholdMs
pi-coding-agent/src/tools/bash-interceptor.ts:1-5,119   cat/grep/find → read/grep/glob
pi-coding-agent/src/tools/approval.ts:366        formatApprovalPrompt; per-tool formatApprovalDetails in write.ts:473, bash.ts:584
pi-catalog/src/models.ts:151                     calculateUsageCost; pi-ai/src/usage.ts:222-223 costUsd
pi-tui/src/status-line/segments.ts:784-820       cost segment; :813 usingSubscription
pi-coding-agent/src/session/session-stats.ts:149 aggregate message/token/cost stats
pi-coding-agent/src/session/session-listing.ts:938,953  resolveResumableSession; session dir computed from cwd
pi-coding-agent/src/slash-commands/builtin-session.ts:385,467,496,255  /usage, /changelog, /hotkeys, /session pin
```

Not verified / limits of this review: the in-flight wave's final shape (it was moving during the survey,
so its four rows are read from the working-tree diff, not from a commit); the image stack's real
capability (unmeasured — kitty paths are live, but no budget/limit comparison was attempted); omp's vim,
mermaid and chart implementations (their settings ids were read, not their code, so their cost figures
are GAP.md's, not mine); and whether any of these features would in fact be *used* — that needs a live
session, which no one here has run.
