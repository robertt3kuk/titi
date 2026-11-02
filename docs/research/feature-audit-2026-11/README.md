# titi feature audit — "have we taken every feature we actually need?"

Repo state read: `master` @ `878c0a6` (2026-10-21), 852 commits, ~163k lines of Rust under `crates/*/src` in 12 crates. Audit date 2026-11-02. Research only: nothing edited, Empryo read through its public docs only, no product run except `titi --version` (to prove it does not exist) and `omp --version` (18.14.1).

Evidence conventions: `path:line` = read in code on this tree. "Doc says" = repo doc, not verified by code. `[INFERENCE]` = reasoned, not observed. Reference-product facts come from the official docs pages named by the three research subagents (Claude Code `code.claude.com/docs`, Codex `learn.chatgpt.com/docs`, Gemini CLI `geminicli.com/docs`, opencode `opencode.ai/docs`, Aider `aider.chat/docs`, omp `omp://` docs of the installed 18.14.1 + its GitHub README, Empryo `empryo.com/docs/*.md`). Where a docs page was not found/readable the cell is `?`, not `○`.

---

## 1. Verdict

**No — but what is missing is a short, specific list, not a broad one.** On the interactive core titi has taken nearly everything the references have, and on context engineering it is ahead of all of them: the Atlas (ranked map + per-prompt lens + `atlas` tool + co-change), role router, cache-break diagnostics, goal/review/council loops, hashline edits, compaction with result masking, local traces, `@file`/draft stash/palette, skills + file commands + subagent definitions + Claude-compatible hooks + MCP stdio/HTTP with repo-consent, ~100 themes, vim composer, notifications. (Only Aider and Empryo have a repo map at all; Claude Code, Codex, Gemini CLI, omp and opencode do not.)

What titi has **not** taken is the "trust and reach" layer every mainstream agent now ships, and three things the docs say are done are broken or dead in the code:

1. **Safe autonomy.** No OS sandbox of `bash` (`pipe.rs:382` runs `sh -c`, nothing wraps it; zero hits for seatbelt/landlock/bwrap in `crates/`), no network policy for bash, approval is tier-only (`titi-tools/src/lib.rs:84-102`) with a bare y/n prompt (`keys.rs:1167-1193`): no allow/deny rules, no "approve for session", no accept-edits mode — in the default `write` mode **every edit and every shell command prompts**. And file undo does not work for its main use (see Contradiction C1).
2. **Multimodal and web.** No image input at all (`ChatMessage.content` is a plain string, `titi-providers/src/transport.rs:41-63`); `fetch` returns the raw body (no HTML→markdown); `web_search` needs a hand-set endpoint+key env pair (`web.rs:131`).
3. **Reach and distribution.** No release pipeline, installer, update path or even `titi --version` (`main.rs:259` → "unknown option"); no Windows (`titi-core/src/hub.rs:21` imports `std::os::unix::net` unconditionally; CI is ubuntu only, `ci.yml:17-64`); no ACP/IDE bridge (only a symbols LSP server, `atlas_cmd.rs:76`); headless `--prompt` takes a text argument only (no stdin, no `--output-format`, no budget/turn caps, no `--model`/`--resume <id>`); no GitHub Action; MCP has no OAuth.
4. **Verification depth.** Syntax-only post-edit diagnostics (no LSP client); no formatter hook.

The Atlas/eval work is not a gap, but the **A/B eval that proves it** (atlas-v2 step 9) and the `scripts/eval.py` baseline against a real model have not been run (agent-practices #10 status line, atlas-v2 README:539). That is a measurement gap, not a feature gap.

Policy risk to decide, not a feature: titi ships "Anthropic (Claude Pro/Max)" OAuth login (`titi-providers/src/oauth/provider.rs:79`, QA_STATUS row "OAuth login"). Anthropic's published position is that subscriptions may not be used through third-party harnesses (The Register 2026-02-20, <https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/>; opencode's docs repeat it, <https://opencode.ai/docs/providers>). See §4.

---

## 2. Capability matrix

Legend — titi column: **have / partial / missing**, then the evidence. Reference columns: ● documented, ◐ partial/experimental/via plugin, ○ not found in the docs read, ? page not readable/unverified. CC = Claude Code, Cx = Codex CLI, Gem = Gemini CLI, Aid = Aider, omp = oh-my-pi 18.14.1, Emp = Empryo, oc = opencode (added: it is the closest open-source analogue).

### 2.1 Core loop and models

| Capability | titi (verified in code) | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Streaming tool loop, mid-turn steering, cancel | **have** — `tool_loop.rs`, Enter steers (README:73), Ctrl+C `keys.rs:1186` | ● | ● | ◐ | ◐ | ● | ● | ● |
| Wire families: OpenAI chat, Responses, Anthropic, Gemini | **have** — `titi-providers/src/{openai,anthropic,gemini}.rs`, `ApiKind` `transport.rs:20-38` | ◐ | ◐ | ◐ | ● | ● | ● | ● |
| Built-in provider catalog | **partial** — 9 providers `engine.rs:13-102`; **no Gemini/Google descriptor** though the transport exists; built-in models carry no price (`NO_PRICE_MODELS` `engine.rs:284`) and only 3 declare a window (`engine.rs:108,116,140,230`) | ● | ◐ | ◐ | ● | ● (60+) | ● | ● (75+) |
| Custom OpenAI-compatible providers / local Ollama, LM Studio | **have** — `providers:` in config; `engine.rs:86-101`; background model discovery | ◐ | ● | ◐ | ● | ● | ● | ● |
| Cloud-platform providers (Bedrock, Vertex, Azure) | **missing** — no SigV4/ADC code | ● | ● | ● | ● | ● | ◐ | ● |
| Subscription OAuth | **partial** — Anthropic and ChatGPT-Codex only (`oauth/provider.rs:79,125`); Anthropic one is policy-risky | ● | ● | ● | ○ | ● | ● | ◐ |
| Fallback chain on transient failure | **have** — before first visible token, 401/unknown model not retried (README:50) | ● | ◐ | ◐ | ○ | ● | ● | ? |
| Per-role model routing (`explore`, `review`, `compact`, `namer`…) | **have** — `titi-engine/src/router.rs`, `/router` | ◐ | ◐ | ◐ | ● (weak/editor) | ● | ● | ◐ |
| Reasoning-effort control on every wire family | **have** — CHANGELOG:54, `effort.rs` | ● | ● | ◐ | ● | ● | ● | ● |
| Prompt-cache control | **partial** — Anthropic breakpoints `wire.rs:474,619,970` + break diagnostics (`cache_watch.rs`); no 1 h TTL, no keep-alive ping, no OpenAI `prompt_cache_key`, no Gemini `cachedContent` (grep: none) | ● | ? | ◐ | ● | ● | ● | ◐ |
| Context-window accounting / 1M models | **partial/bug** — window is set once from the first model (`engine.rs:1006-1019`, default 128k `runtime.rs:1000`); `SwitchModel` never updates it (`runtime.rs:1777-1802`); no 1M opt-in header | ● | ● | ? | ◐ | ● | ◐ | ? |
| Token and money budgets | **partial** — `/budget` tokens and `$`; but no built-in price ⇒ `$` caps are refused out of the box (`engine.rs:284`, BRAIN:108); no headless cap flag | ● (`--max-budget-usd`) | ◐ | ○ | ○ | ● (`--max-cost`, exit 3) | ? | ○ |

### 2.2 Context and memory

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Project rules file (AGENTS.md hierarchy) | **partial** — `AGENTS.md` / `.titi/AGENTS.md` only (`project_context.rs:52-59`) | ● | ● | ● | ◐ | ● | ● | ● |
| CLAUDE.md / GEMINI.md fallback, `@path` imports | **missing in practice** — a complete multi-provider loader exists (`titi-core/src/context_files.rs:135-175`) but no code calls it (`grep context_files` → only a comment, `project_context.rs:8`) | ● | ◐ | ● | ○ | ● | ● | ● |
| Just-in-time subdirectory rules (loaded when a tool touches the dir) | **missing** (practice #21, `[INFERENCE]` in doc; grep confirms only static load) | ● | ◐ | ● | ○ | ◐ | ? | ? |
| Cross-session memory with recall | **have** — `titi-memory`, `/memory`, recall ranked by prompt (CHANGELOG:66) | ● | ● | ◐ | ○ | ● | ● | ○ |
| Compaction (summary + result masking, forced on overflow) | **have** — `titi-engine/src/compaction/`, CHANGELOG:38 | ● | ● | ● | ● | ● | ● | ● |
| Ranked repo map / task-conditioned lens / graph tool | **have — titi's edge** — `titi-atlas` (13 grammars, PageRank, co-change, `atlas` tool) | ○ | ○ | ○ | ● | ○ | ● | ○ |

### 2.3 Tools and editing

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| read / write / edit / glob / grep / bash, jailed fs tools | **have** — `fs.rs` (`jail_path`), `.git` write refusal `fs.rs:69-93` | ● | ● | ● | ◐ | ● | ● | ● |
| Hash-anchored edits | **have** — `hashline.rs:220` | ○ | ○ | ○ | ○ | ● | ◐ | ○ |
| `apply_patch` format | **missing** (practice #24 `Do`) | ○ | ● | ○ | ◐ (udiff) | ● | ○ | ● |
| Structural/AST edit as a model tool | **missing** — `ast_edit` is a library only (agent-practices #25; omp-parity #27 gating) | ○ | ○ | ○ | ○ | ● | ● | ○ |
| Notebook read/edit | **missing** | ● | ○ | ? | ? | ● | ? | ? |
| **Image input (attach, paste, vision)** | **missing** — `ChatMessage.content: SmolStr` (`transport.rs:41-63`), no image part in any wire builder (`wire.rs` grep), MCP images degrade to `[image]` (`titi-mcp/src/client.rs:892`); only *output* of local photos in Kitty (`titi-tui/src/image.rs`) | ● | ● | ● | ● | ● | ● | ● |
| PDF / other binary readers | **missing** (`read` "names a binary file", README:164) | ● | ○ | ● | ○ | ● | ? | ? |
| Web fetch (HTML→markdown, extraction, cache) | **partial** — raw body GET, 8 MiB cap, strong SSRF guard, redirect-hop judging (`web.rs:254-267`, BRAIN:102); no HTML conversion | ● | ○ | ● | ● (`/web`) | ● | ● | ● |
| Web search | **partial** — single generic endpoint from env `TITI_SEARCH_ENDPOINT`+`…API_KEY` (`web.rs:131`); registered even when unconfigured (`web.rs:395-402`, perf README:210); no provider-native/hosted search | ● | ● | ● | ○ | ● (23 providers) | ● | ◐ |
| Background shell + job table | **have** — auto-background 60 s (`pipe.rs:40-45`), `/jobs` | ● | ◐ | ● | ○ | ● | ● | ◐ |
| Pty shell, `ask` tool, `todo` tool | **have** — `pty.rs`, `ask.rs`, `todo.rs` | ● | ◐ | ● | ○ | ● | ● | ● |
| `project` (detect and run lint/test/typecheck/build) | **have** — `project.rs:802` | ◐ | ○ | ○ | ◐ (`--test-cmd`) | ○ | ● | ○ |
| Git helpers | **partial** — read-only `git` + `git_commit`; push/reset/checkout/rebase refused (`git.rs:1-12,36-45`); no commit-message generation, no PR | ● | ◐ | ◐ | ● (auto-commit, `/undo`) | ● (`github` tool) | ● | ● |
| LSP client (diagnostics, definition, refs, rename) | **missing** — only syntax diagnostics (`syntax_note.rs`); `titi atlas lsp` is a *server* of symbols | ● | ○ | ○ | ○ | ● | ● | ● |
| Format-on-write | **missing** (a `PostToolUse` hook can emulate) | ◐ | ○ | ○ | ◐ | ● | ◐ | ● |
| Browser / computer-use tool | **missing** — PLAN M3 item, nothing in tree | ◐ | ◐ | ◐ | ○ | ● | ● | ○ |
| Code-execution kernel (`eval`) | **missing** (omp-parity #27) | ○ | ○ | ○ | ○ | ● | ◐ | ○ |
| MCP resources / prompts / tool-search deferral | **missing** (`titi-mcp` is tools-only; perf README:185 proposal only) | ● | ● | ● | ○ | ● | ● | ◐ |

### 2.4 Verification, review, planning

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Turn-end verify command (`verify.command`) + one-shot feedback | **have** — `verify.rs`, CHANGELOG:42 | ◐ (hooks) | ◐ | ◐ | ● (auto-lint/test) | ◐ | ◐ | ◐ |
| Clean-context reviewer + verdict + CI exit codes | **have** — `/review`, `titi --review` exits 0/3/4/5, `--goal` 0/1/3 | ● | ● | ○ | ○ | ● | ● | ○ |
| Goal loop (coder+reviewer rounds) | **have** — `/goal`, `goal.rs` | ● | ● | ○ | ○ | ● | ● | ○ |
| Plan mode / read-only mode | **have** — `--mode plan` registers only read tools (README:67) | ● | ● | ● | ● (ask) | ● | ● | ● |
| Eval harness with a real-model baseline | **partial** — `scripts/eval.py` + 5 tasks, no baseline run (practices #10); atlas A/B step 9 not done | n/a | n/a | n/a | ● (leaderboard) | ◐ (`omp bench`) | ? | n/a |

### 2.5 Safety, permissions, sandbox

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Approval modes | **partial** — `always-ask / write / yolo` only (`titi-tools/src/lib.rs:84-102`); `write` auto-approves **Read tier only**, so edits prompt too; no "accept edits, ask shell" mode; set only by `--approval` (`main.rs:135-146`) | ● (6 modes) | ● | ● (auto_edit) | ◐ | ● | ● | ● |
| **Allow/deny rules per tool/command/path** (`Bash(git status)`, `edit(src/**)`) | **missing** — gate is `force_approval || !approval_mode.auto_approves(tier)` (`tool_loop.rs:777`); `tools.approval.*` appears only in settings *tests* (`settings.rs:1358-1401`), no reader | ● | ● (execpolicy) | ● (TOML policy) | ○ | ● (`tools.approval.<tool>`, `bash.patterns`) | ◐ | ● |
| Approve once / for session / deny-with-message | **missing** — `y`/Enter allow, `n`/Esc deny only (`keys.rs:1172-1185`); note **Enter approves** | ● | ● | ● | ○ | ● | ● | ● |
| Forced prompt on critical commands (`rm -rf /`, `curl|sh`) | **missing** — every bash is Exec tier, no content check (only the foreground-server interceptor `intercept.rs:1-40`) | ● | ● | ◐ | ○ | ● | ● | ◐ |
| **OS sandbox for bash (seatbelt / bwrap / landlock)** | **missing** — `sh -c` with `current_dir` (`pipe.rs:382`, `pty.rs:224`), "not path-jailed" (ARCHITECTURE.md:430-431); practices #37 says the same | ● | ● | ● | ○ | ○ | ? | ? |
| Network policy for shell (domain allowlist / off by default) | **missing** (fetch has SSRF guards; bash has none) | ● | ● | ◐ | ○ | ○ | ? | ○ |
| Env scrubbing for child processes | **partial** — hooks get an allow-listed env (`pipe.rs:395`); `bash` inherits the full environment | ◐ | ◐ | ● | ○ | ◐ | ? | ? |
| Secret/sensitive-file policy + output redaction | **have** — `SensitivePolicy`, `redact.rs`, BRAIN rows 3, 4 | ◐ | ◐ | ● | ○ | ● | ● | ◐ |
| Project-code consent (hooks, MCP servers from a cloned repo) | **have** — trust panel, hashes (`hooks_trust.rs`, `mcp_setup.rs`, CHANGELOG:58) | ● | ● | ● | ○ | ● | ◐ | ○ |
| Prompt-injection scan of rules/skills/agent bodies | **have** — `titi_soul::scan` | ○ | ○ | ○ | ○ | ? | ? | ○ |
| Subagent privilege model | **partial** — read-only by default; the only writer mode is `agent_writes`→`Yolo` (`runtime.rs:1568-1573`) and it has **no user-facing setter** (`agent_writes` only appears at `runtime.rs:909,996,1568,1626`) | ● | ● | ● | n/a | ● | ● | ● |

### 2.6 Sessions and history

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Resume newest / pick / search sessions | **have** — `-c`, `/sessions`, Ctrl+X, `session.autoResume`; sessions record `cwd` (`session.rs:82-93`) | ● | ● | ● | ◐ | ● | ● | ● |
| Resume by id / name from the CLI | **missing** — no `--resume`/`--session` flag (`main.rs` flag list) | ● | ● | ● | ○ | ● | ● | ● |
| Fork / branch tree | **have** — `/fork`, `/tree` | ● | ● | ◐ | ○ | ● | ● | ● |
| **File-level undo / checkpoint rewind** | **partial — broken for its main case** — `/checkpoint` pins HEAD when nothing is staged (`git_checkpoint.rs:46-50`), or commits staged changes onto the user's current branch (`:66-76`); `/rewind` restores with `git reset --hard` **only on a clean tree** (`:84-94`), so after the agent has edited files the transcript rewinds and the files do not (`session_fs.rs:479-482` "git not restored"). Not automatic per turn. | ● (per prompt) | ○ | ● (shadow repo) | ● (`/undo`) | ○ | ● (private refs) | ● (`/undo`) |
| Export | **have** — `/export` md/jsonl | ● | ● | ● | ◐ | ● | ● | ● |
| Shareable link / redacted share | **missing** — sealed share package exists (`titi-core/src/share.rs`, lib.rs:16) but nothing calls it (README:222) | ○ | ○ | ◐ | ○ | ● | ◐ | ● |
| Import other agents' sessions/config | **missing** | ● | ● | ○ | ○ | ● | ● | ◐ |

### 2.7 TUI and UX

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Full-screen TUI, markdown/tables/math/mermaid, mouse, resize-safe | **have** — `titi-tui`, QA_STATUS rows | ● | ● | ● | ◐ | ● | ● | ● |
| Vim composer | **have (subset)** — `vim.rs`, GAP #9 lists what is not ported | ● | ● | ● | ● | ● | ◐ | ○ |
| Themes | **have** — ~100 embedded (`theme/builtin.rs:3-8`), auto light/dark | ● | ● | ● | ◐ | ● | ● | ● |
| Status line presets | **have** — presets/gauge; **user script status line missing** | ● (script) | ● | ◐ | ○ | ● | ● | ○ |
| Notifications (OSC 777/BEL, terminal progress, title) | **have** — GAP #3–5 | ● | ● | ● | ● | ● | ● | ● |
| Command palette, `@file`, history browser, draft stash | **have** — CHANGELOG:86-104 | ● | ● | ● | ◐ | ● | ● | ● |
| External-editor edit of the draft (Ctrl+G) | **missing** — declared in `titi-tui/src/keybindings.rs:343-347`, no consumer anywhere | ● | ● | ● | ● | ● | ● (vim panel) | ● |
| Custom keybindings file | **missing** — table in `keybindings.rs` is unconsumed; BRAIN:98 names it a queue item | ● | ● | ○ | ◐ | ● | ◐ | ● |
| Output styles / personalities | **partial** — `SOUL.md` only | ● | ◐ | ○ | ○ | ◐ | ◐ | ○ |
| Voice input | **missing** — `space_hold.rs` is dead on purpose, "titi's STT is a stub" (BRAIN:98) | ● | ○ | ◐ | ● | ● | ● | ○ |
| Accessibility (screen-reader/plain mode, `NO_COLOR`, reduced motion) | **missing** — grep `NO_COLOR|reduce.?motion|screen.?reader`: no hits in prod code | ● | ○ | ● | ○ | ◐ (colour-blind) | ◐ (`/motion`) | ○ |

### 2.8 Extensibility

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Skills (`SKILL.md`, foreign dirs) | **have** — `skills.rs`, `skill_tool.rs` | ● | ● | ● | ○ | ● | ● | ● |
| File slash commands | **have** — `commands.rs`, `.claude/commands` read | ● | ◐ | ● | ○ | ● | ○ | ● |
| Subagent definitions (`.claude/agents`) | **have** — `agent_defs.rs` | ● | ● | ● | ○ | ● | ● | ● |
| Hooks | **partial** — 5 events (`titi-config/src/hooks.rs:41-47`: Pre/PostToolUse, UserPromptSubmit, Stop, SessionStart), command type only; CC has 33 events and 5 handler types; missing `PostToolUseFailure`, `SessionEnd`, `PreCompact`, `SubagentStop`, `Notification`, `PermissionRequest` | ● | ● | ● | ○ | ● | ● | ◐ |
| MCP over stdio + Streamable HTTP with consent | **have** — `titi-mcp` 2,675 lines; CHANGELOG:46-48 | ● | ● | ● | ○ | ● | ● | ● |
| **MCP OAuth** (401 discovery, PKCE, token store) | **missing** — no oauth in `titi-mcp`; design L4c "not started" | ● | ● | ● | ○ | ● | ● | ● |
| `titi mcp add/list/login` CLI | **missing** (JSON files + `/mcp` panel only) | ● | ● | ● | ○ | ● | ◐ | ● |
| Plugins / marketplace | **missing** — design L6 "not started" | ● | ● | ● (extensions) | ○ | ● | ○ | ◐ |
| titi as an MCP server | **missing** | ● | ○ (removed) | ○ | ○ | ? | ● (docs MCP) | ○ |

### 2.9 Headless, automation, CI, IDE

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Full-duplex headless protocol (JSONL, ready frame) | **have** — `--headless`, `headless.rs`, protocol pinned by `tests/protocol.rs` | ● | ● (app-server) | ◐ | ○ | ● (rpc) | ◐ | ● (serve) |
| One-shot prompt mode | **partial** — `--prompt <text>` only (`main.rs:151-157`); no stdin piping; stdout = event JSONL, reply on stderr; exit 0/1 | ● | ● (`exec`) | ● | ● (`-m`) | ● | ● | ● |
| `--output-format text/json/stream-json`, `--json-schema`/output schema | **missing** | ● | ● | ● | ○ | ● | ● | ◐ |
| `--model`, `--resume`, `--max-turns`, `--max-cost`, `--timeout`, allowed-tools flags | **missing** (only `--approval`, `--mode`, `--theme`, `-c`) | ● | ● | ● | ◐ | ● | ● | ● |
| CI gates by exit code | **have** (`--goal`, `--review`) | ◐ | ◐ | ◐ (0/1/42/53) | ◐ | ◐ | ● (0–4,130) | ○ |
| GitHub Action / Slack / GitLab | **missing** | ● | ● | ● | ○ | ○ | ○ | ● |
| SDK / app-server for embedding | **missing** (protocol is documented in code only) | ● | ● | ? | ◐ (unofficial) | ● | ○ | ● |
| **ACP server / IDE bridge** | **missing** — `atlas lsp` only (`atlas_cmd.rs:76`) | ◐ (VS Code/JetBrains ext) | ● | ● | ○ | ● | ● | ● |
| Scheduled / cron tasks | **partial** — `/loop` timer lives in the engine and dies with the process | ● | ● | ○ | ○ | ◐ | ● | ○ |

### 2.10 Multi-agent

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Parallel read-only subagents, live strip | **have** — `agent_tool.rs`, pinned agents | ● | ● | ● | ○ | ● | ● | ● |
| Parallel *writing* agents with isolation (worktrees) | **missing** — see subagent row above; `worktree` appears only in `.git` guards (`fs.rs:74`) | ● | ● | ◐ | ○ | ● | ● | ◐ |
| Local peer hub, council, advisor, graph | **have** — `hub.rs` (unix socket 0600), `council.rs` | ◐ | ◐ | ○ | ○ | ● | ● | ○ |
| Cloud / remote agents, phone control | **missing — out of scope** | ● | ● | ○ | ○ | ○ | ● | ◐ |

### 2.11 Observability and cost

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| `/usage`, `/context`, `/cache`, turn footer, per-model usage | **have** | ● | ● | ● | ◐ | ● | ● | ◐ |
| Local span traces + `titi trace` | **have** — `titi-core/src/trace.rs`, CHANGELOG:116 | ○ | ○ | ◐ | ○ | ● | ◐ | ○ |
| OTLP export | **missing** | ● | ● | ● | ○ | ● | ? | ○ |
| Telemetry-free by default | **have** — no analytics code found | ◐ | ◐ | ◐ | ◐ (opt-in) | ◐ | ? | ◐ |
| Crash report | **partial** — panic hook restores the terminal and prints one line (`main.rs` `install_panic_report`, BRAIN row 7); `/diagnose` prints a paste block; nothing written to disk | ◐ | ◐ | ◐ | ○ | ? | ? | ◐ |

### 2.12 Packaging, distribution, platforms

| Capability | titi | CC | Cx | Gem | Aid | omp | Emp | oc |
|---|---|---|---|---|---|---|---|---|
| Install channel | **missing** — README "Run" is `cargo run -p titi-cli`; `.github/workflows/` holds only `ci.yml`; no release job, no binaries, no brew/install script | ● (script, brew, winget, apt…) | ● | ● (npm, brew) | ● (pip, script) | ● | ● | ● |
| `--version`, update check, self-update | **missing** — `./target/debug/titi --version` → `unknown option --version`; nothing named update in `main.rs` | ● (auto) | ● | ● | ● | ● (`omp update`) | ? | ● |
| Signed / notarised release | **missing** | ● | ? | ? | ? | ● (macOS in CI) | ? | ? |
| Shell completions, `doctor` | **partial** — `/diagnose` yes; completions no | ● | ● | ◐ | ● | ● | ◐ | ◐ |
| macOS | **have (dev platform), no CI** — CI is `ubuntu-latest` ×5 (`ci.yml:17,27,37,56,64`); QA rows are manual macOS PTY runs | ● | ● | ● | ● | ● | ● | ● |
| Linux | **have** — CI platform | ● | ● | ● | ● | ● | ● | ● |
| **Windows** | **missing** — `titi-core/src/hub.rs:21` unconditional `use std::os::unix::net` [INFERENCE: does not compile on Windows]; shells spawn `sh` (`pipe.rs:382`, `pty.rs:224`); unix-only process groups with `cfg(not(unix))` stubs in four places only | ● (native) | ● (native sandbox) | ● | ● | ● (native) | ● | ◐ (WSL recommended; v2 native) |

---

## 3. Take next (ranked) and deliberately not taking

Size: **S** ≲ 300 LOC / one crate, **M** 300–1,500 LOC or two crates, **L** > 1,500 LOC or a new subsystem `[INFERENCE, not measured]`. Ranked by (user value × frequency) ÷ cost, with defects first because they are cheap and silently hurt trust.

### Take next — top 15

**1. Make file undo real (per-turn snapshot + `/undo`)** — **M**
Why: the single most-requested safety net for an agent that edits your tree; every comparable product (CC per-prompt checkpoints, Gemini shadow repo, Empryo `refs/empryo/`, opencode/Aider `/undo`) has it. In titi the feature exists, is documented and passes QA on a clean tree, but fails when it matters (C1 below), and the "checkpoint" can write a commit onto the user's branch.
Evidence: `git_checkpoint.rs:46-50,66-76,84-94`; `session_fs.rs:479-482`; nothing takes a snapshot automatically (`grep -n "checkpoint_session" crates/titi-cli/src` → only the `/checkpoint` command path).
How: before each turn that will edit (first write/edit/hashline/bash-with-write), snapshot the tree into a private ref or detached tree object using a temp index (`GIT_INDEX_FILE`, `git write-tree`/`commit-tree`, `refs/titi/<session>/<n>`) — never touching the user's index, branch or HEAD; `/undo` and `/rewind` restore **only paths the agent changed** (diff the snapshot against the tree) and refuse paths the user also changed since. Non-git dirs: copy-on-write backup of agent-touched files (the `touched` set already exists, `tool_loop.rs` `TouchedSet`). Copy: Empryo private refs, Gemini `/restore`; skip CC's "bash edits are not tracked" limitation by diffing the tree.

**2. Permission rules + approve-for-session + accept-edits mode + a persistent approval setting** — **M**
Why: today every edit and every shell command prompts in the default mode, and the only relief is `--approval yolo` (all-or-nothing, flag-only). Approval fatigue is the top real-world complaint (CC best-practices, practices #38). Needed by anyone who runs more than a toy session.
Evidence: `titi-tools/src/lib.rs:84-102`; `tool_loop.rs:777`; `keys.rs:1167-1193`; `tools.approval.*` unconsumed (`settings.rs:1358-1401`); practices #38 `Do:`.
How: `permissions: {allow: ["bash(cargo test*)","edit(src/**)"], ask: [...], deny: ["bash(rm -rf*)","read(.env)"]}` evaluated in the same function that holds the gate (deny wins; a hook can still only tighten, as `hooks.rs:11-13` already states); prompt gets `a` = approve for this session (and `A` = write the rule to the user config, never the project file); a `acceptEdits` mode (writes auto, exec/network ask); a critical-command denylist that forces a prompt even in yolo (omp's override). User config layer only for loosening, like `privacy.allow` (README:30). Copy: Claude `Tool(specifier)` syntax (already half-parsed for agent definitions, design L5 "As built") + omp's `bash.patterns`. Skip Gemini's tiered TOML engine for now.

**3. Read the rule files other agents write (CLAUDE.md, GEMINI.md, `@imports`, subdirectory JIT)** — **S**
Why: a repo that has only `CLAUDE.md` gets **no** project rules in titi; this is the first thing a user trying titi on their existing project will hit. The code is already written and unused.
Evidence: `project_context.rs:52-59` vs `titi-core/src/context_files.rs:135-175` (unreferenced; `lib.rs:6`).
How: call `context_files` from `project_context::render`, keep the injection scan, add `@path` expansion (omp: depth 5; Gemini; CC), and (second step) append a subdirectory's `AGENTS.md` to the first tool result that touches that directory (practices #21 `Do`, prefix-stable).

**4. Make headless a real CI interface** — **S–M**
Why: CI/scripting is where an agent gets adopted beyond one person; the competition's contract is `exec`/`-p` + stdin + `--output-format` + budgets + stable exit codes.
Evidence: `main.rs:151-157` (`--prompt` takes an argument only), no `--model/--resume/--version` flags (`main.rs` flag chain), only `--goal`/`--review` have documented exit codes (README:65, USAGE).
How: `titi exec` (alias of `--prompt`): prompt from arg or stdin, `--output-format text|json|stream-json`, `--model`, `--resume <id|last>`, `--max-turns`, `--max-cost`, `--timeout`, `--allow/--deny` rules (reuses #2), `--output-schema` later; document exit codes (0 ok, 1 error, 2 usage, 3 fail, 4 partial, 130 interrupt — Empryo's set, `docs/surfaces/headless.md`). Add `--version` in the same change.

**5. Model catalog truth: per-model window, window on `/model` switch, shipped prices, a Gemini descriptor** — **S**
Why: compaction thresholds, the context gauge and `$` budgets are wrong or unusable out of the box; switching from a 128k model to a 1M one (or back) keeps the old window; `/budget $2` is refused for every built-in.
Evidence: `engine.rs:1006-1019`, `runtime.rs:1777-1802`, `runtime.rs:1000`, `engine.rs:284` (all 26 built-ins unpriced), no Gemini descriptor in `engine.rs:13-102`.
How: resolve window per model through the registry on each turn (the registry already has `primary_context_window`, `registry.rs:194`); ship dated prices with a source link in the descriptor (BRAIN:108 refuses to guess — the owner must allow sourcing them from provider docs); add `google` + `gemini-*` descriptors; add the Anthropic 1M beta header opt-in.

**6. Release engineering: binaries, installer, `--version`, `titi update`, macOS CI** — **M**
Why: today nobody but a Rust developer can install titi. PLAN M9 "packaging/headless" is the right home; nothing exists.
Evidence: only `.github/workflows/ci.yml`; README "Run: cargo run -p titi-cli"; `main.rs:259`.
How: GitHub release workflow (macOS arm64/x64, Linux x64/arm64 musl; `strip=symbols`, `lto=thin` already set in `Cargo.toml`), SHA-256 file, `install.sh`, Homebrew tap, `titi update --check` (read-only network call, opt-out), macOS ad-hoc sign then notarise (omp's CI recipe is documented in `docs/research/packaging-headless/README.md`), add a `macos-latest` test job (QA is on macOS today, CI never builds there).

**7. OS sandbox for `bash` (macOS seatbelt, Linux bubblewrap), then auto-approve inside it** — **L** (macOS first = M)
Why: it is what turns "ask every time" into "safe by construction" — Anthropic measured 84% fewer prompts; Claude Code, Codex and Gemini all ship it; titi's own docs call bash "not path-jailed". Prompt-injection through `fetch`, MCP or a cloned repo currently lands in an unconfined `sh`.
Evidence: grep for seatbelt/sandbox-exec/landlock/bwrap in `crates/` → no hits; `pipe.rs:382`; ARCHITECTURE.md:430; practices #37 (`Do:`).
How: spawn through `sandbox-exec -p <profile>` (macOS) / `bwrap` (Linux) when found; writes = workspace + `$TMPDIR` + `.git` read-only (reuse `refuse_git_dir` policy), reads = everything except `privacy.sensitive` paths, network = off by default with a domain allowlist through a loopback proxy as step two; the `unsafe_code = forbid` rule is fine (external binaries, no FFI); sandbox on ⇒ `bash` auto-approves under `write` mode; escape hatch = ask. No new dependency needed for step one (check `dependency-update` skill before adding any). Copy: Codex/Claude design (open source per their docs); fall back to today's behaviour with a visible "unsandboxed" badge where unavailable. Windows: not in scope.

**8. Image input (paste / `@image.png` / `read image`) + vision capability flag** — **M–L**
Why: screenshots of a bug or a design are routine in 2026; every reference product supports it; titi cannot even receive an MCP image. GAP.md:208 left "how titi attaches images" unmeasured — answer: it does not.
Evidence: `transport.rs:41-63`; `wire.rs` has no image block; `titi-mcp/src/client.rs:892` turns images into `[image]`.
How: `ChatMessage.content` → parts (`#[serde(default)]` so old sessions load, as `tool_call_id` was added), `vision: true` in `ModelCompat`, builders for the four families, `read` returns an image part for png/jpeg/gif/webp (auto-resize ≤ 2000 px, 5 MiB, opencode's numbers), composer paste/clipboard (`pbpaste -Prefer png`/`osascript`, `wl-paste`, `xclip`), dropped path detection; strip for models without vision and say so. Persist as artifact files, not inline base64, in the session JSONL.

**9. Web fetch/search that a model can actually use** — **M**
Why: docs lookups are a daily need; raw HTML into context wastes tokens, and a search tool that needs two env vars and is advertised to the model when unconfigured costs ~215 bytes per request for a tool that always answers "no provider".
Evidence: `web.rs:254-267,395-402`, perf README:210.
How: HTML→markdown (pure-Rust readability extraction, check `dependency-update`), keep the SSRF guard; `fetch` optional `prompt`/summarise-through-`explore` role; search: provider-native server tools where the wire family has them (Anthropic `web_search`, OpenAI hosted search, Gemini grounding — Claude, Codex, Gemini all do this) plus a keyless fallback; do not register `web_search` when nothing is configured.

**10. Git workflow commands: `/commit`, `/pr`, commit-message generation** — **S**
Why: commit + PR is the end of almost every session; Aider's auto-commit and Claude's `/commit-push-pr` are daily features; titi's `git` tool forbids push/checkout by design and has no PR path.
Evidence: `git.rs:36-45` (forbidden verbs), no `gh` usage anywhere (`grep -rniE "\bgh\b|pull request"` → skills text only).
How: no new tool — ship bundled file commands/skills (`/commit`, `/pr`, `/review-pr`) that use `git_commit`, `bash` and `gh`, with the `commit` role in `modelRoles` writing the message from the staged diff; approval stays per command. (The mechanism exists: `commands.rs`, skills.)

**11. ACP server (`titi acp`)** — **M**
Why: one adapter gives Zed, JetBrains (ACP registry), Neovim (CodeCompanion/Avante) and VS Code (via extensions that speak ACP) without writing a plugin per editor; omp, Empryo, opencode, Gemini all ship it; Codex and Claude are listed in Zed's registry. titi's engine is already UI-free (`EngineCommand`/`EngineEvent`), so it is a protocol mapping, not a rewrite.
Evidence: no ACP anywhere (`grep -ri "acp"` → research docs for other products only).
How: new `titi-cli` mode next to `--headless`; map prompt/cancel/permission request/diff to ACP JSON-RPC; route approvals through `session/request_permission`; file reads through the client when it offers unsaved buffers. Defer VS Code/JetBrains native extensions.

**12. MCP OAuth + `titi mcp add/login`** — **L**
Why: the interesting remote MCP servers (issue trackers, docs, observability) require OAuth; CC, Codex, Gemini, opencode and omp all do the browser flow; without it HTTP MCP is limited to static bearer headers.
Evidence: `titi-mcp` has no OAuth; design L4c status "not started" (design-2026-10.md:394,413), ~1,200 LOC estimate there; titi already has PKCE/loopback/device code for provider logins (`titi-providers/src/oauth/{pkce,callback,device}.rs`) to reuse.
How: RFC 9728 discovery + PKCE + loopback redirect + token store in `titi-secrets`, refuse non-HTTPS token endpoints; `/mcp login <server>`; then MCP resources/prompts (as `@` mentions and `/mcp__srv__prompt`) and tool-search deferral (perf README:185).

**13. LSP diagnostics after edits (and a read-only `lsp` tool)** — **L**
Why: type errors are what agents ship broken; titi's syntax check catches only parse errors; CC, omp, Empryo and opencode report LSP diagnostics after every write. Aider/Empryo-style tree-sitter is not a substitute.
Evidence: `syntax_note.rs`, Empryo-parity #18 "partial", #22 "gated: needs LSP client".
How: lazy-start per-language servers found on `PATH` (no auto-install in step one), `didChange` from the edit tools, append `diagnostics: +N −M` next to the existing `syntax:` line; tool ops `diagnostics`, `definition`, `references`, `rename` later. Copy opencode's "opt-in, lazy, never download" posture first; omp's broker/mux is overkill.

**14. Writer subagents behind a switch + git-worktree isolation** — **M**
Why: the README promises "subagents that can only read unless asked otherwise" but nobody can ask; parallel implementation work (the headline multi-agent use in CC, Codex, omp, Empryo) is impossible. Needs worktrees to be safe because per-file claims (`claims.rs`) do not stop semantic conflicts.
Evidence: `runtime.rs:909,996,1568-1573,1626`; no setter in `titi-cli`/`titi-config`; practices #36.
How: `agent.writes: true` (user layer only) + `agent` call parameter `isolation: worktree` creating `git worktree add` under the agent dir, merged back by the parent (diff shown, approval required); subagents in a writer mode inherit the parent's permission rules (#2) instead of `Yolo`.

**15. Windows support and a CI matrix** — **L** (decision first)
Why: Windows is a large share of developers and every reference product has native Windows; it is also the platform where an "unknown option --version" kind of first impression is fatal. It is a port, not a flag.
Evidence: `hub.rs:21`; `pipe.rs:382`, `pty.rs:224`; CI is ubuntu only.
How: first add `windows-latest` as an allowed-failure job to see the real error list; gate the hub behind `cfg(unix)` (named pipe later); choose the shell (`pwsh`/`cmd`/Git Bash detection like opencode's `OPENCODE_GIT_BASH_PATH`); process-tree kill via job objects (needs `unsafe` or a crate — the workspace forbids `unsafe`, so a crate decision under `dependency-update`); ConPTY via `portable-pty` (already a dependency). If the owner does not want Windows, say so in the README rather than leave it implicit.

Also worth doing, not in the top 15 (all S–M): hooks parity (six more events + `http` handler); `--screen-reader`/`NO_COLOR`/reduced-motion; OTLP export of the span tree titi already writes; shell completions; user-script status line; Ctrl+G external editor; `titi doctor`; import of CLAUDE/Codex config (`/import`); `apply_patch` for OpenAI models **only if** the eval harness shows edit-failure retries (practices #24); the atlas-v2 step-9 A/B and a real-model baseline for `scripts/eval.py` (without these, every context-engineering claim is unmeasured end to end).

### Deliberately not taking (with reason)

| Item | Reason |
|---|---|
| Desktop/GPUI, phone/watch apps, web UI, remote control, "another computer" mode | Owner dropped GPUI 2026-11-02; terminal-only. Remove PLAN M9's GPUI half. |
| Cloud/hosted agents (Codex Cloud, Claude on the web, teleport, scheduled cloud tasks, Slack/Linear delegation) | Needs a hosted service; no sign titi wants to run infrastructure. `/loop` + `exec` + a GitHub Action cover the CI need. |
| Claude **subscription** OAuth (Anthropic Pro/Max login) | Not a feature to add but one to **retire or gate**: Anthropic's stated policy is no third-party harnesses on subscriptions (The Register 2026-02-20; opencode docs). Keep API-key Anthropic; keep ChatGPT-Codex login (OpenAI permits it in opencode/omp, `[INFERENCE]` from their shipping it). Owner decision. |
| Bedrock/Vertex/Azure native auth (SigV4, ADC) | Large, enterprise-only; users can reach those through an OpenAI-compatible gateway via `providers:`. Revisit on demand. |
| Enterprise managed policy (admin tiers, MDM, `requirements.toml`, SSO gateway) | Single-user/open-source product. Trust store + user-layer-only loosening already covers the threat model. |
| Classifier-based "auto mode" (CC) | Puts a model call on the approval path; rules + sandbox (#2, #7) give most of the benefit deterministically. Revisit after #7. |
| Plugin marketplace (design L6) | Skills, commands, agents, hooks and MCP already load from `.claude/`/`.agents/` layouts, so users bring their own; building a marketplace/lock/rollback system (~2,000 LOC) before anyone asks is premature. Keep in design; take only `/import` first. |
| Voice input, TTS, pets, radio, i18n | Low frequency; platform-coupled (PortAudio/whisper); `space_hold.rs` already parked (BRAIN:98). |
| Embedded Neovim, Tern surface, generate_image, IDA, security_scan, Python/JS `eval` kernel, DAP debugger | omp/Empryo niche tools; large; no stated demand (omp-parity #27, empryo-parity "Not taken"). |
| Snapcompact, TTSR, magic keywords, workflowz, advisor watchdog files | omp-specific, unmeasured; titi already has compaction + advisor + goal loop. |
| Aider-style whole/udiff/architect edit formats, repo-map-in-chat file adding | Practices #24/#49: do not wrap edits in alternate formats without eval evidence; coder+review+role routing already cover architect/editor. |
| Semantic/vector search tool | Practices #17: grep + Atlas lens beats it on code until evals say otherwise. |
| `bot network` / Telegram channels (PLAN M8) | Not a terminal coding-agent feature; `grep telegram` → nothing in tree. Recommend demoting M8 out of the plan or into an explicit non-goal. |
| Session import from other agents | Convenient, rarely decisive; fold into a later `/import`. |

---

## 4. Contradictions: repo docs vs code

**C1 — `/checkpoint` + `/rewind` "undoes code" (README:88-90, ARCHITECTURE, QA_STATUS row "pass" 2026-10-03) — only on a clean tree.**
`git_checkpoint.rs:46-50`: nothing staged ⇒ the "snapshot" is just `HEAD`. `:84-94`: `restore` runs `git reset --hard` and refuses unless `git status --porcelain` is empty. The agent's own edits make the tree dirty, so rewinding after any edit yields `· git not restored: workspace has uncommitted changes` (`session_fs.rs:479-482`) while the transcript has already rewound — history and files disagree. With staged files, `:66-76` runs `git commit --no-verify` on the user's current branch. README says only "records the git HEAD when the index is clean".

**C2 — README:238 "What is not: MCP, hooks, and a skills runtime…" vs reality.** Hooks (`titi-engine/src/hooks.rs`, `titi-config/src/hooks.rs`), MCP stdio and HTTP (`titi-mcp`, CHANGELOG:46-48) and skills (`skill_tool.rs`) ship; AGENTS.md:114 and PLAN.md:36 say M6 was opened 2026-10-21. ARCHITECTURE.md:16 still calls `titi-mcp` "stdio" only. README:9 "Phase 3–4" and "1907 passed (2026-10-08)" vs BRAIN:58 "2055 (2026-10-09)". CHANGELOG:5 header says "Unreleased — 2026-10-08" while containing 2026-10-21 work.

**C3 — README:162 lists nine tools ("read, write, edit, hashline_edit, glob, grep, bash, todo, memory, settings"); the registered set also has `fetch`, `web_search`, `git`, `git_commit`, `diagnose`, `project`, `ask`, `agent`, `skill`, `atlas`** (`git.rs:82,184`, `web.rs:256,360`, `project.rs:802`, `ask.rs:226`, `atlas_tool.rs`, `skill_tool.rs`; practices #23 counts ~18).

**C4 — README:37-46 provider table vs code.** The table omits `openai-codex` (`engine.rs:55-62`, 15 models) and `anthropic/claude-sonnet-5-5` (`engine.rs:147`); README:188 diagram says providers speak "OpenAI, Anthropic, Gemini" but no Gemini provider descriptor ships (`engine.rs:13-102`) — Gemini works only if the user declares one.

**C5 — README:176 / settings guard: "approval (`approval_mode`, `tools.approval*`) … refused to read as well as write" — implies these are real settings.** Nothing reads them: approval comes only from `--approval` (`main.rs:135-146`); `tools.approval.*` exists only in settings tests (`settings.rs:1358-1401`). They are protected names for knobs that do not exist. Related: README:234 "approval for dangerous calls" suggests classification; the gate is tier-only (`lib.rs:94-101`) and every `bash` is Exec.

**C6 — README:234 "subagents that can only read unless asked otherwise"** — there is no way to ask: `agent_writes` has no setter outside `EngineConfig` defaults/tests (`runtime.rs:909,996`; none in `titi-cli`/`titi-config`). When set it makes the child `Yolo` (`runtime.rs:1568-1573`), bypassing the parent's approval mode.

**C7 — `titi_core::context_files` (multi-provider rule discovery with CLAUDE.md/GEMINI.md/`.opencode`, `@`-imports) and `project_context.rs:8` "matches `titi_core::context_files`".** The engine never calls it; the live loader reads only AGENTS.md (`project_context.rs:52-59`). The comment is false; the module is dead code that looks like a feature.

**C8 — omp-parity/GAP.md:90 (#12) "the workspace is not in `SessionMeta` yet"** vs `session.rs:82-93` (`SessionMeta.cwd`, schema v2 in BRAIN:56). GAP.md:209 "omp's editor keybinding names already match titi's (`keybindings.rs:32+`), so `tui.editor.*` is not a gap" — that table is not consumed by the screen (`/hotkeys` reads its own table, `keys.rs:153-161`; BRAIN:98 lists keybinding customization as a queue item), and `app.editor.external` (Ctrl+G) has no handler. GAP.md:208 left image attachment "unmeasured": it is absent.

**C9 — agent-practices/README.md bodies below updated status lines.** #39 and #40 bodies still read "missing, gated by PLAN M6" directly above "Status … have"; #22 still says only `.titi/skills` is scanned; #37 and #38 are still correct (missing/partial). A reader scanning bodies gets the wrong picture.

**C10 — PLAN.md milestones read as a status list but have no status marks.** M3 DoD ("CUA makes a screenshot and a click", eval/notebook, browser, ast-edit as tools), M7 ("sandbox on tool calls, credentials in a separate process, default deny, actor identity") and M8 (bot network, Telegram) are not started; M9 still names GPUI. AGENTS.md:4-5 and README.md:3 still say "a native GPUI window comes later" (another worker is removing it).

**C11 — README:222 vs AGENTS/PLAN "share".** Consistent (nothing hands out the share package), but `titi-core/src/lib.rs:16` re-exports `share_session`/`seal_export` with no caller: another built-but-unwired module like C7.

**C12 — QA_STATUS "Local photos (kitty/ghostty): unverified" (line 38) vs README:156 documenting the feature as working.** No manual verification on record for a documented capability.

---

## 5. What I could not verify

- Behaviour of titi at runtime beyond `--version` and flag parsing: this audit read code; it did not build or drive the TUI (AGENTS.md: CI is the source of truth; no builds were run).
- Whether Windows compilation fails beyond `hub.rs:21` (I did not cross-compile).
- Reference-product cells marked `?`: pages 404/unreadable for opencode (`/docs/share`, `/docs/sandbox`, desktop), Codex global CLI flag table (JS component), Gemini 1M-context and SDK, Empryo sandbox absence (absence in docs is not proof).
- LOC sizes are my estimates except where a repo design doc gives one (MCP OAuth ≈ 1,200 LOC, design-2026-10.md:444).
- Anthropic's third-party-harness policy was checked through the press report and opencode's docs, not through Anthropic's current legal pages.
