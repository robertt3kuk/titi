# titi

A terminal coding agent in Rust. It follows the product model of its reference product, omp — one loop, one session, one set of tools — without Electron. The same engine drives a full-screen terminal and a headless JSONL interface. A native GPUI window comes later.

[English](#english) · [Русский](#русский)

| Version | License | Phase | As of | Tests |
| --- | --- | --- | --- | --- |
| `0.1.0` | [MIT](LICENSE) | Phase 3–4 · tools, providers, agents — in progress | 2026-10-08 | 1907 passed (CI run 37840563719) |

---

## English

### Run

Rust 1.85 or newer (edition 2024).

```bash
cargo run -p titi-cli
```

A provider key belongs in the agent directory, never in the repository:

```bash
titi --set-key opencode-go "$KEY"
titi --list-keys
```

Without a key the turn fails in the open: the provider requires a credential. Settings stack from lowest to highest: built-in defaults, `~/.titi/agent`, `<project>/.titi/config.yml`, then environment variables. The project file cannot declare `providers` or `models`, and cannot loosen `privacy.allow` or `privacy.maskIps`: a provider entry names where your key is sent, so only your own layers decide it.

A model you declare can say what it costs, in the same dollars per million tokens every provider publishes: `price: { input: 3, output: 15, cachedInput: 0.3 }`. That is the rate `/budget $` and the `· $x` footer meter; a model with no `price` is unpriced — which is not free — and a money cap over it is refused by name rather than guessed. A rate nobody can read is refused the same way, naming the key (`input`, `output`, `cachedInput`), and the model loads unpriced rather than at zero.


Built in:

| Provider | Models | Key |
| --- | --- | --- |
| `openai` | `gpt-4.1` | `OPENAI_API_KEY` |
| `openrouter` | `gpt-4.1` (wire `openai/gpt-4.1`) | `OPENROUTER_API_KEY` |
| `opencode-go` | `glm-5.3-flash`, `deepseek-v4-flash` | `OPENCODE_API_KEY` |
| `anthropic` | `claude-sonnet-4-5` | `ANTHROPIC_API_KEY` |
| `clinepass` | `glm-5.3`, `deepseek-v4-flash`, `deepseek-v4-pro` | `CLINE_API_KEY` |
| `bai` | `glm-5.3-flash`, `qwen3.8-flash`, `qwen3.8-max` | `BAI_API_KEY` |
| `ollama` | whatever the server has loaded | none, `127.0.0.1:11434` |
| `lmstudio` | whatever the server has loaded | none, `127.0.0.1:1234` |

`clinepass` and `bai` are cheap OpenAI-compatible gateways, so they reuse the same transport and add no provider branch. The two local servers ship no model list at all: nothing is contacted at start, and a background listing asks only the providers that need no key, so an absent server costs a failed request rather than a failed start. What it answers joins the `/model` list when it arrives, after the startup order, so rows never jump under the cursor.

A project config overlays this list by id. It does not delete the others. `titi --set-key <id>` stores the key for that provider. The first model that has a key is the one that runs; a machine with no keys still starts on `openai/gpt-4.1` and the request fails in the open. A turn that fails before its first visible token walks the rest of the list in order and says so (`model fallback: a → b`); a `401` and an unknown model are not retried.

One turn, no screen:

```bash
titi --prompt "read Cargo.toml and tell me the version"
titi --headless --approval yolo
titi --headless --goal "cargo test -p titi-core passes"
titi --mode plan
```

`--continue` / `-c` resumes the newest session in this agent directory instead of starting blank; with nothing to resume, it starts fresh and says so. `session.autoResume` set in a config layer (the project file wins) resumes the newest session at every launch; it is off unless set.

`--headless` reads `{"v":1,"command":…}` frames from stdin and writes events to stdout. The first line is `{"ready":true,"protocol":1}`. Not every command answers with an event — `Steer`, `RestoreHistory`, `Cancel` with no turn, and `ApproveTool` are quiet, so a client must not block on a reply to them — and closing stdin ends the run.

`--goal` runs the coder/reviewer loop without a screen and exits with the code CI reads: `0` when the reviewer passes it, `1` on a partial verdict, `3` on anything else, a missing verdict included. Events still go to stdout as JSONL; the report line goes to stderr, where a shell script can read it without parsing the stream.

`--mode agent|plan|duck` picks what a session may reach. A mode is not advice the model may ignore — it decides which tools are registered at all. `agent` registers everything: read, write, exec. `plan` registers only the read-only tools, so the turn answers with a plan instead of a change. `duck` registers no filesystem or shell tool at all and sends no repository map: a repo-blind partner to talk something through. `/plan`, `/duck`, and `/done` switch mid-session.

### In the terminal

| Key or command | What it does |
| --- | --- |
| Enter | Sends the turn. During a turn it steers, it does not start a second one |
| Ctrl+C | Stops the running turn. When idle, press it again within 2 seconds to quit |
| Ctrl+D | Quits when the input line is empty |
| `exit` / `quit` / `q` | Leaves the chat: at once before the first turn, and on a second Enter once it has one |
| `y` / `n` | Approves or refuses a write or a shell command |
| `/model` | Switches to the next model in the list. `/model <id>` picks one by id or by its short name |
| `/switch` | Fuzzy search over the same list: `/switch opus`, `/switch anthropic/claude-sonnet-4-5`. `/switch @review:high` resolves a model role from the settings |
| `/usage` | Tokens for this turn and for the session, prompt and completion apart — as the provider counted them, estimated only when it reports none, with the part served from the prompt cache named |
| `/budget` | Caps what the session may spend: `/budget 200k`, `/budget 1.5m`, `/budget $2`, `/budget off`. Tokens or money (exact micro-dollars, charged at each round's model price by the engine's own ledger; a model with no price says so rather than pretending) |
| `/settings` | Every resolved setting with the layer it came from |
| `/theme` | Chooses a palette; bare `/theme` opens the picker and remembers the choice |
| `/keys` · `/whoami` | Which providers have a key: env, stored, or none. The key itself is never shown |
| `/login` | `/login` lists providers. `/login openai` asks for the key and stores it masked. `/login openai <key>` stores it in one step |
| `/logout` | Forgets the stored key for a provider. An environment variable is left alone |
| `/checkpoint` | Records a rewind point, and the git HEAD when the index is clean |
| `/checkpoints` | Lists this session's rewind points |
| `/rewind` | Cuts the session back to the newest point. `/rewind 2` picks one |
| `/recap` | Prints what the session did: turns, tools, files, problems |
| `/fork` | Starts a new session from this one, keeping what has happened so far |
| `/export` | Writes the transcript to `<session-id>.md` in the current directory. `/export <path>` chooses where, and a path ending in `.jsonl` exports JSONL instead of markdown |
| `/context` | What fills the context window now: system prompt, project rules, skills, recalled memory, genome map, history, tool specs, each with its estimated tokens and share. The numbers are estimates, not provider counts |
| `/compact` | Folds the history now instead of waiting for the threshold. `/compact auth` keeps the folded lines that mention `auth` in the digest |
| `/memory` | `/memory` or `/memory list` shows what is remembered. `/memory search <query>` looks it up, `/memory forget <id>` drops one |
| `/plan` | Plan mode: only read-only tools are registered, so the next turns read the repository and change nothing |
| `/duck` | Duck mode: repo-blind and toolless, for talking a problem through |
| `/done` | Leaves plan or duck mode and acts again |
| `/goal` | Runs the coder and the reviewer until the goal passes or the round cap stops it |
| `/loop` | Repeats a prompt in the background: `/loop 5m <prompt>`, intervals `90s`, `5m`, `2h`. The engine owns the timer, so closing the screen does not kill it |
| `/jobs` | Lists the background loops. `/jobs cancel <id>` stops one |
| `/advisor` | A toolless second opinion on this conversation, `/advisor <question>` on something specific. It is not a turn: nothing it says is acted on |
| `/council` | Puts a question to a council of independent briefs on models and efforts of their own; one fold names who dissents |
| `/graph` | Runs the orchestrator graph: the council decides, the goal loop works |
| `/git` | Shows git status or diff, read-only |
| `/genome` | Manages the prompt map: `/genome on`, `/genome off`, `/genome limit <n>` |
| `/diagnose` | Prints a diagnostics block to paste into a bug report |
| `←` `→` / `alt+←` `alt+→` / `home` `end` | Move the caret: a character, a word, the ends of the draft. `ctrl+a` / `ctrl+e` are the ends too |
| `delete` / `ctrl+u` | Delete what is after the caret, and everything before it |
| `alt+backspace` / `ctrl+w` | Delete the word before the caret |
| `alt+f` | Cycles what `/tree` shows: every entry, everything but the tool traffic, only yours |
| `alt+a` | Walks the view through the live agents and back to the turn |
| `/hotkeys` | Every key the screen answers, grouped, from the same table the screen reads |
| `/tree` | The session's stored entries as a tree, every branch, the path to the leaf marked. Enter branches there; `alt+f` filters |
| `/sessions [query]` | Bare, the list Ctrl+X shows. With a query, search over past sessions: each row is the line that matched, dated by that line |
| `/changelog [full\|last n]` | What changed in the build you are running, from the notes embedded in it |
| `/budget $2` | A cap in money as well as in tokens |
| `/statusline` | Chooses the status line preset (default, minimal, compact, full, ascii). Bare `/statusline` states the preset in force and lists them all |
| `/mouse` | Mouse reporting: off, wheel, buttons, all (drag selects, release copies) |
| `/hub` | Shows or hides the roster of the local hub |
| `/join` | Joins the local hub, `/join <name>` under a name of your own; the default is the session id |
| `/leave` | Leaves the hub, which unregisters this peer |
| `/btw` | Sends a message that is not recorded in the history; during a turn it steers instead of starting one |
| `/pause` | Holds the composer and stops the running turn. `/pause` again resumes |
| `/help` | Lists these commands |
| Up / Down | Move through the command list. It opens only when `/` is the start of the line |
| Tab | Fills the highlighted command. Enter on a prefix runs it. Esc clears the slash |

A `paste.menuThreshold` (default 100, `0` turns it off) long paste is offered a way to reach the model instead of pasting a wall of text: attach it as a fenced block, as a file under `.titi/pastes/`, or leave the `[Paste #N · …]` marker every shorter paste gets. Dropping `statusLine.separator` (`powerline`, `powerline-thin`, `slash`, `pipe`, `block`, `none`, `ascii`) changes the glyph between the status line's segments; `statusLine.sessionAccent` (unset = off) puts the theme's accent on the idle editor border and on the rest of the context gauge; `statusLine.transparent` (unset = off) leaves that row's background to the terminal. `display.turnFooter.time`, `.tokens` and `.cacheMiss` (each unset = on) mute one part of the dim row under a finished answer.

Other keys this build reads, each unset = off unless said otherwise: `editor.vim` turns the composer into vim's two modes (Esc leaves Insert for Normal, which has `h l w b e 0 ^ $` with counts, `x X s`, the `d`/`c` operators with a motion, `dd`, `D`, `cc`, `C`, `S`, and `i a I A` back; enter sends in either mode, and `i`/`esc` are the whole of it if you never leave Insert). `display.pinnedAgents` (`off|collapsed|full`, unset = `collapsed`) pins the live agents above the composer — one row each, with `display.subagentLivePreview` (unset = off) adding what each is doing — and `alt+a` walks their panes. `display.smoothStreaming` reveals a streamed answer at a readable rate instead of in the provider's bursts, and `tui.tight` drops one cell of horizontal padding from the composer, the status row and the panels. `treeFilterMode` (`default|no-tools|user-only`) names the filter `/tree` opens in, and `startup.changelog` (unset = on) says one line when the build changed since your last run, pointing at `/changelog`. `session.autoResume` (unset = off) reopens the newest session in the agent directory at every launch.

A mermaid `flowchart`/`graph` fence is drawn as a diagram (top-down and left-right, labels, chains, `subgraph`’s nodes without its box; an unsupported fence keeps its source) unless `tui.renderMermaid` is off — unset is on, omp’s default.

A GFM table whose numbers read as one measure gets a bar chart under it: labels left, bars scaled so the largest fills the field, each value as written at its own bar's end. Only for 4–12 rows, a first column of labels rather than an index, one column whose every cell is one quantity in one unit (`12`, `1,400 tok/s`, `45%`, `$250`, `2.5k`), no negatives, and only when the bars say more than the numbers already do (nine values, or a three-fold spread). Anything else — a range, a date, a cell with a second number, a pane too narrow for a bar field — leaves the table exactly as it was. omp draws this (`tui.autoGraph`, default `always`) as SVG on a graphics terminal; titi draws it in text.


The model a session starts on can be named: `modelRoles:` / `  default: openai/gpt-4.1` — unset is the first available model, which is what it always was, and a pinned id that is not available starts there anyway and says so once, by name.

A dotted key may be written flat: `editor.vim: true` at the top level is the same setting as `editor:\n  vim: true`, which is what `/settings` prints, so a line copied out of it works. Where a file writes both, the nested one wins. `ask` is the tool the model uses to put a question to you: options, several if it says so, or your own words — answered in the panel above the composer. `agent` spawns a subagent (up to four in one call) whose lifecycle the pinned strip shows.

The picker also offers the skills it found in `<agent_dir>/skills/`, `.agents/skills/`, and `.titi/skills/`. A `/name` that is not a command and is a skill expands that `SKILL.md` into the prompt, after the same screening the metadata gets. A skill's scripts are never executed.

The screen is a ratatui chat: model and session on top — plus a `plan` or `duck` badge when the engine confirms that mode — the transcript in the middle, one input line at the bottom. `/mouse` turns mouse reporting over (`off`, `wheel`, `buttons`, `all`; a drag selects, releasing inside the transcript copies), and its choice is remembered for the next run.

The same settings style as `genome.enabled` and `genome.limit` covers the screen's own switches: `statusLine.preset` (`default|minimal|compact|full|ascii`, `default` when unset) chooses which row of the preset table the top line paints, and `statusLine.contextLine` (`off|percentage|embedded`, **default off** — the default frame stays byte-identical until a turn reports a context window for the gauge to fill) lets the gap between the status line's groups double as a context gauge. `notify.completion`, `notify.error` and `notify.ask` (each unset = on) raise a desktop notification when a turn finishes, fails or stops on an approval — OSC 777 where the terminal speaks it, a bell fallback where it does not; `terminal.progress` (unset = on) raises the terminal's own OSC 9;4 progress bar for a running turn; `composer.tokenRate` (unset = on) shows the generation rate on the working row, an estimate marked `~`, not a provider count.

In Kitty or Ghostty, a local photo named in the transcript is drawn in place: png, jpeg, gif, bmp, or ico, including a markdown image. The pixels are sent once. Other terminals leave the path as text. `TITI_NO_KITTY_PLACEHOLDERS=1` turns the pictures off.

Tool approval: `--approval always-ask|write|yolo`. The default is `write` — reads pass, writes and the shell ask. Headless has no approval panel, so the mode has to be set explicitly or a write waits for an answer that never comes.

### The tools

`read`, `write`, `edit`, `hashline_edit`, `glob`, `grep`, `bash`, `todo`, `memory`, `settings`.

`read` takes an optional `offset` (the first line, 1-based) and `limit` (how many lines); a range that leaves lines out starts with `[lines A-B of N]`, so a large file is read in pieces. One read answers at most 30,000 characters, and 2,000 lines without a `limit`; a longer file comes back as its first part under `[lines 1-N of M; read on from offset N+1]` instead of being cut in the middle. Reading a directory lists it; a binary file is named as one.

`glob` matches real glob patterns: `*` and `?` stay in one directory, `**/` crosses any number, `[a-z]` and `{rs,toml}` work, and a pattern without `/` names a file in any directory. `grep` takes a regular expression, with `ignore_case` and a `glob` file filter, and `path` may name one file (a path outside the workspace or one that does not exist is an error); a hit on a very long line shows the 300 characters around the match. Both skip `target`, `.git` and `node_modules`, list in path order, and cap the answer (1,000 paths, 500 matching lines) with a count of the rest.

`edit` replaces text that occurs exactly once — an ambiguous `old_string` is refused with its count — or every occurrence with `replace_all`; a CRLF file is matched from the LF text a model writes and keeps its own line endings and byte-order mark. When `old_string` is nowhere as written but fits exactly one run of lines once indentation, trailing spaces and typographic quotes are set aside, that run is edited, the new lines take the file's indentation, and the answer says the match was loose.

`hashline_edit` pins the lines it replaces by the six-hex anchor of the text that was read. A line that changed since the read is refused as a stale read instead of being overwritten from a picture of the file that is no longer true.

`bash` runs through a pipe by default. `pty: true` runs it under a real terminal, so the command sees a tty and behaves the way it would in your own shell — colour, progress, paging. Either way a run is bounded: by `timeout_secs` (1 to 3600, 300 by default), by Ctrl+C, which stops the command a cancelled turn was waiting on, and by a cap on what it keeps (64 KiB on a pty; the first and last 64 KiB of each stream on a pipe, since a log puts its error last). A stop takes the command's whole process group with it, and a failure starts with its `exit N`. The output is what a terminal would show — colour codes and window titles removed, `\r` progress bars reduced to their last state. Zero means "no deadline" nowhere in titi. A dev server, a watcher or `tail -f` in the foreground is refused before it runs, with the backgrounded form to use instead (`cmd > log 2>&1 &`) and `timeout 30 cmd` to check only that it starts. A command that outlives the background threshold — the `bash.autoBackground.thresholdMs` setting, in milliseconds, 60 000 by default, refused when it is 0 or past one hour; `TITI_BASH_BACKGROUND_MS` overrides it when set — is handed over as a background job instead of holding the turn. Every tool result, whatever the tool, is capped at 40,000 characters before the model sees it: past that it keeps the head and the tail around a note saying how much was left out.

`todo` is the agent's checklist for multi-step work: `write` sets the whole list, `update` sets one item's status by its number, `view` shows it. At most one item is in progress, a list holds up to 50 items of up to 200 characters, and the list lasts as long as the process.

`settings` lets the agent read and write configuration. Approval (`approval_mode`, `tools.approval*`), `privacy`, the provider catalog (`providers`, `models`) — their roots, everything under them, and any parent such as `tools` — and any credential-looking leaf (`key`, `apiKey`, `api_key`, `token`, `secret`, `password`) are refused, to read as well as to write: loosening approval, redirecting a key, or reading out a credential is a human decision.

### How it fits

A surface never calls the model itself. It sends `EngineCommand` and paints `EngineEvent`. Retries and model switches happen only before the first visible token. A `401` and an unknown model are not retried.

```text
TUI / headless
      │  EngineCommand / EngineEvent
      ▼
 titi-engine          turn, tools, cancel, compaction, modes, agents, council, goal loop
      │
      ├── titi-providers     HTTP/SSE: OpenAI, Anthropic, Gemini; fallback chain
      ├── titi-tools         read  write  edit  hashline  glob  grep  bash  settings
      ├── titi-genome        file and symbol graph → a prompt fragment
      ├── titi-memory        what to recall on this turn
      ├── titi-core          sessions, trajectory, the local hub broker
      └── titi-soul          SOUL.md and personality
```

The system prompt reaches every family, not only the OpenAI-compatible ones: a leading system message is lifted into Anthropic's top-level `system` array and into Gemini's `systemInstruction`, and a compaction digest sitting mid-history travels as a user turn instead of vanishing.

On Anthropic the request is also built for the prompt cache. Tool specs go out sorted by name, so the prefix is byte-stable across processes instead of following a hash map's seed; every message travels as a one-element block array, so its bytes do not change as it ages into history; and `cache_control` breakpoints sit on the last tool, on the system block, and on the newest message — four at most, which is the protocol limit.

| Crate | Role |
| --- | --- |
| `titi-cli` | The `titi` binary: screen, headless, keys, modes, hub client |
| `titi-tui` | Frame, composer, panels, themes. It does not know the model |
| `titi-engine` | Protocol, loop, provider registry, modes, subagents, council, goal loop, background jobs |
| `titi-providers` | Transport, stream decoding, prompt-cache layout, fallback chain |
| `titi-tools` | Tools and the `read` / `write` / `exec` tiers, pty bash, hashline edits |
| `titi-genome` | Walk the repo, tree-sitter symbols, PageRank, project into a system message |
| `titi-core` | JSONL sessions, search, trajectory, the hub broker, the share package |
| `titi-memory` | Memory index: full-text search and local embeddings |
| `titi-soul` | Identity slot, scanned before it enters the prompt |
| `titi-config` | Layered settings |
| `titi-secrets` | `.env` and `auth.db`, which holds more than one account per provider |

Agent state lives in `~/.titi/agent`. A named profile uses `~/.titi/profiles/<name>/agent`. `TITI_AGENT_DIR` overrides both. Sessions, memory, and keys are not written into the repository. The only project file is `.titi/config.yml`, and it must not contain a key.

### Several agents

Sessions on one machine talk through a local hub: one broker per agent directory listens on `<agent_dir>/hub.sock`, created with mode `0600`, and validates a sender against the id its connection joined under, so one client cannot speak as another. `/join` puts this session on the roster, `/hub` shows it, `/leave` steps off. Nothing waits on that socket: a broker that is slow, gone, or never started costs the chat loop nothing, and "no hub broker running" is an ordinary answer rather than an error.

The engine also seats a council: two to four briefs answer the same question on models and efforts of their own, none of them seeing the others, and one fold separates what they agree on from what they do not, naming the member that holds each dissent. `/council` puts a question to it from the chat; `/graph` runs it behind the goal loop.

`titi-core` can seal a session export into a share package: ChaCha20-Poly1305 through the audited RustCrypto crate, header authenticated as associated data so a host cannot relabel one session as another, and the key returned separately instead of being stored with the package. Nothing hands one out from the screen yet.

Print the repository map on its own:

```bash
cargo run -p titi-genome --example map -- . 40
```

`TITI_NO_GENOME=1` leaves the map out of the prompt. `genome.enabled` in the agent or project config turns the map on or off, `genome.limit` (1 to 64, default 24) caps the files it projects; the project file wins, and `TITI_NO_GENOME=1` forces one run off without touching either. A turn also carries a `<diff>` block: the working tree's changes next to the genome map, so the model sees its own edits since the turn began. Genome indexes thirteen languages, and every one of them is read from a syntax tree: a tree-sitter grammar sits on each row (`crates/titi-genome/src/lang/mod.rs`), so the symbols in Rust, TypeScript, TSX, JavaScript, Python, Go, Java, C, C++, C#, Ruby, Kotlin, Swift and PHP come from the parse, not from patterns. `titi genome capabilities` prints the roster — level, extensions, and one honest note per language — and the LSP handshake carries the same table.

### What is already here

The engine, streaming, model switching and a fallback chain across providers, tools jailed to the current directory, approval for dangerous calls, hashline edits that refuse a stale read, `bash` with a deadline that Ctrl+C can cut short, on a real pty when asked, agent / plan / duck modes, sessions that restore, fork, export, and rewind, compaction of a long context, a coder-and-reviewer goal loop with an exit code CI can read, background loops and a token budget, a toolless advisor, a local hub, subagents that can only read unless asked otherwise, Genome over thirteen languages, every one read from a syntax tree, memory, SOUL, skills that are discovered and expanded by name, prompt caching on Anthropic, and local photos in Kitty and Ghostty.

### What is not

A desktop window on GPUI (M9). MCP, hooks, and a skills runtime that runs anything: a skill is found and its text is expanded, never executed (M6). A shipped price table: no built-in model carries a price, so a money cap over one is refused by name rather than converted at a rate nobody wrote down. A model you declare may carry its own `price` (see above), and then the cap is enforced against it.

### Safe to publish

Keys, sessions, memory, and `SOUL.md` stay in `~/.titi/agent`, outside this tree. `.gitignore` also refuses `.env`, `*.db`, private keys, `.reference-product/`, and `.tmp_*`. The working tree and the git history were scanned for API keys, GitHub tokens, AWS keys, private-key blocks, and JWTs: none are committed. Tests use placeholders such as `sk-test`.

The handoff notes a public endpoint (`https://opencode.ai/zen/go/v1`) and model ids. It does not contain the key.

### Continue the work

The resume point is [`docs/research/STATE.md`](docs/research/STATE.md); the research map is [`docs/research/README.md`](docs/research/README.md). The task cycle is [`docs/CONVEYOR.md`](docs/CONVEYOR.md) and the milestones are [`docs/PLAN.md`](docs/PLAN.md). Decisions live in the theme docs under `docs/research/`.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets     # informational, not denied
cargo test --workspace --locked
gh run list --repo robertt3kuk/titi --branch master --limit 5
```

CI runs fmt, clippy (informational), the workspace tests, and a smoke of the real binary on a pty (`scripts/tui-smoke.py`): the ready frame, the `/` listing, `/help`, Ctrl+D, then the terminal restore sequences. No provider key is used and no model turn runs; every wait is bounded, so a hang fails the job instead of hanging it.

---

## Русский

Терминальный агент на Rust. Продуктовая модель его reference product, omp: один цикл, одна сессия, одни инструменты — без Electron. Тот же движок обслуживает полноэкранный терминал и headless JSONL. Нативное окно на GPUI ещё впереди.

### Запуск

Нужен Rust 1.85+ (edition 2024).

```bash
cargo run -p titi-cli
```

Ключ провайдера кладётся в каталог агента, не в репозиторий:

```bash
titi --set-key opencode-go "$KEY"
titi --list-keys
```

Без ключа ход падает открыто: провайдер требует credential. Настройки складываются снизу вверх: встроенные значения, `~/.titi/agent`, `<проект>/.titi/config.yml`, затем переменные окружения. Проектный файл не может объявить `providers` или `models` и не может ослабить `privacy.allow` или `privacy.maskIps`: запись провайдера называет, куда уйдёт ваш ключ, поэтому решают только ваши собственные слои.

Объявленная вами модель может назвать свою цену — в тех же долларах за миллион токенов, что публикует провайдер: `price: { input: 3, output: 15, cachedInput: 0.3 }`. Именно эту ставку считают `/budget $` и строка `· $x`; модель без `price` — не «бесплатная», а без цены, и денежный лимит по ней отклоняется с названием модели, а не угадывается. Нечитаемая ставка отклоняется так же, с названием ключа (`input`, `output`, `cachedInput`), и модель загружается без цены, а не с нулевой.


Встроены:

| Провайдер | Модели | Ключ |
| --- | --- | --- |
| `openai` | `gpt-4.1` | `OPENAI_API_KEY` |
| `openrouter` | `gpt-4.1` (в запросе `openai/gpt-4.1`) | `OPENROUTER_API_KEY` |
| `opencode-go` | `glm-5.3-flash`, `deepseek-v4-flash` | `OPENCODE_API_KEY` |
| `anthropic` | `claude-sonnet-4-5` | `ANTHROPIC_API_KEY` |
| `clinepass` | `glm-5.3`, `deepseek-v4-flash`, `deepseek-v4-pro` | `CLINE_API_KEY` |
| `bai` | `glm-5.3-flash`, `qwen3.8-flash`, `qwen3.8-max` | `BAI_API_KEY` |
| `ollama` | то, что загружено на сервере | не нужен, `127.0.0.1:11434` |
| `lmstudio` | то, что загружено на сервере | не нужен, `127.0.0.1:1234` |

`clinepass` и `bai` — дешёвые OpenAI-совместимые шлюзы, поэтому они переиспользуют тот же транспорт и не добавляют ветку провайдера. У двух локальных серверов встроенного списка моделей нет вовсе: на старте никто никуда не ходит, а фоновое перечисление спрашивает только тех, кому не нужен ключ, так что выключенный сервер стоит упавшего запроса, а не упавшего старта. Что он ответил, попадает в список `/model` уже после стартового порядка, поэтому строки не прыгают под курсором.

Конфиг проекта накладывается на этот список по id и не удаляет остальные. `titi --set-key <id>` кладёт ключ этого провайдера. Запускается первая модель, у которой ключ уже есть. Если ключей нет, остаётся `openai/gpt-4.1`, и запрос падает открыто. Ход, упавший до первого видимого токена, проходит остаток списка по порядку и говорит об этом (`model fallback: a → b`); `401` и неизвестная модель не повторяются.

Один ход без экрана:

```bash
titi --prompt "прочитай Cargo.toml и скажи версию"
titi --headless --approval yolo
titi --headless --goal "cargo test -p titi-core проходит"
titi --mode plan
```

`--continue` / `-c` открывает последнюю сессию этого каталога вместо чистого старта; если открывать нечего, стартует с чистого листа и об этом говорит. `session.autoResume`, выставленный в слое конфига (проектный файл сильнее), делает то же при каждом запуске; без выставления выключен.

`--headless` читает со stdin кадры `{"v":1,"command":…}` и пишет события в stdout. Первая строка — `{"ready":true,"protocol":1}`. Отвечает событием не каждая команда — `Steer`, `RestoreHistory`, `Cancel` без активного хода и `ApproveTool` молчат, поэтому клиент не должен ждать от них ответа, — а закрытие stdin завершает прогон.

`--goal` гоняет цикл «кодер + ревьюер» без экрана и выходит с кодом, который читает CI: `0`, если ревьюер принял, `1` при частичном вердикте, `3` во всех остальных случаях, включая отсутствие вердикта. События по-прежнему уходят в stdout как JSONL; строка отчёта — в stderr, чтобы её мог прочитать shell-скрипт, не разбирая поток.

`--mode agent|plan|duck` выбирает, до чего сессия вообще дотянется. Режим — не совет, который модель может проигнорировать: он решает, какие инструменты зарегистрированы. `agent` — всё: чтение, запись, shell. `plan` — только читающие инструменты, поэтому ход отвечает планом, а не правкой. `duck` не регистрирует ни файловых, ни shell-инструментов и не шлёт карту репозитория: это собеседник, слепой к репозиторию. Переключение на ходу — `/plan`, `/duck`, `/done`.

### В терминале

| Клавиша или команда | Что делает |
| --- | --- |
| Enter | Отправляет ход. Во время хода это steering, а не второй ход |
| Ctrl+C | Останавливает активный ход. В покое второе нажатие за 2 секунды выходит |
| Ctrl+D | Выход, если строка ввода пустая |
| `exit` / `quit` / `q` | Уход из чата: сразу до первого хода, а после — по второму Enter |
| `y` / `n` | Разрешить или отказать записи и shell |
| `/model` | Следующая модель в списке. `/model <id>` выбирает по id или по короткому имени |
| `/switch` | Нечёткий поиск по тому же списку: `/switch opus`, `/switch anthropic/claude-sonnet-4-5`. `/switch @review:high` разворачивает роль модели из настроек |
| `/usage` | Токены за ход и за сессию, prompt и completion отдельно — как их посчитал провайдер; оценка, только если он не сообщает; часть из кэша промпта названа отдельно |
| `/budget` | Ограничивает трату сессии: `/budget 200k`, `/budget 1.5m`, `/budget $2`, `/budget off`. Токены или деньги (точные микро-доллары, по цене модели каждого раунда из леджера самого движка; модель без цены говорит об этом, а не делает вид) |
| `/settings` | Все разрешённые настройки и слой, из которого пришла каждая |
| `/theme` | Выбирает палитру; голый `/theme` открывает выбор и запоминает его |
| `/keys` · `/whoami` | У кого есть ключ: env, сохранён или нет. Сам ключ не показывается |
| `/login` | `/login` показывает провайдеров. `/login openai` просит ключ и прячет его. `/login openai <ключ>` сохраняет сразу |
| `/logout` | Забывает сохранённый ключ провайдера. Переменную окружения не трогает |
| `/checkpoint` | Точка отката и git HEAD, если индекс чистый |
| `/checkpoints` | Список точек отката этой сессии |
| `/rewind` | Откат к последней точке. `/rewind 2` выбирает номер |
| `/recap` | Что было в сессии: ходы, инструменты, файлы, ошибки |
| `/fork` | Начинает новую сессию от текущей, сохраняя всё, что уже было |
| `/export` | Выгружает разговор в `<id-сессии>.md` в текущем каталоге. `/export <путь>` выбирает куда, а путь на `.jsonl` выгружает JSONL вместо markdown |
| `/context` | Что сейчас занимает окно: системный промпт, правила проекта, скиллы, вспомненная память, карта репозитория, история, описания инструментов — у каждого оценка в токенах и доля. Это оценка, а не счёт провайдера |
| `/compact` | Сворачивает историю сразу, не дожидаясь порога. `/compact auth` оставляет в дайджесте свёрнутые строки, где упомянут `auth` |
| `/memory` | `/memory` или `/memory list` показывает, что запомнено. `/memory search <запрос>` ищет, `/memory forget <id>` удаляет запись |
| `/plan` | Режим плана: зарегистрированы только читающие инструменты, поэтому следующие ходы читают репозиторий и ничего не меняют |
| `/duck` | Режим утки: слепой к репозиторию и без инструментов, чтобы проговорить задачу |
| `/done` | Выходит из режима плана или утки и снова действует |
| `/goal` | Гоняет кодера и ревьюера, пока цель не пройдена или пока не кончились круги |
| `/loop` | Повторяет промпт в фоне: `/loop 5m <промпт>`, интервалы `90s`, `5m`, `2h`. Таймер живёт в движке, поэтому закрытый экран его не убивает |
| `/jobs` | Список фоновых циклов. `/jobs cancel <id>` останавливает один |
| `/advisor` | Второе мнение по этому разговору, без инструментов; `/advisor <вопрос>` — про конкретное. Это не ход: сказанное им не исполняется |
| `←` `→` / `alt+←` `alt+→` / `home` `end` | Двигают каретку: на символ, на слово, к краям черновика. `ctrl+a` / `ctrl+e` — тоже края |
| `delete` / `ctrl+u` | Удаляют то, что после каретки, и всё, что до неё |
| `alt+backspace` / `ctrl+w` | Удаляют слово перед кареткой |
| `alt+f` | Перебирает, что показывает `/tree`: всё, всё кроме вызовов инструментов, только ваше |
| `alt+a` | Ведёт вид по живым агентам и обратно к ходу |
| `/hotkeys` | Все клавиши, которые отвечает экран, по группам — из той же таблицы, что читает сам экран |
| `/tree` | Записи сессии как дерево, все ветки, путь к листу помечен. Enter ветвится там; `alt+f` фильтрует |
| `/sessions [запрос]` | Без аргумента — список, который даёт Ctrl+X. С запросом — поиск по прошлым сессиям: строка — это совпавшая реплика, и время у неё её собственное |
| `/changelog [full\|last n]` | Что изменилось в сборке, которую вы запустили, — по заметкам, вшитым в неё |
| `/budget $2` | Денежное ограничение, а не только в токенах |
| `/hub` | Показывает или прячет ростер локального хаба |
| `/join` | Подключает к локальному хабу, `/join <имя>` — под своим именем; по умолчанию это id сессии |
| `/leave` | Выходит из хаба и снимает этого участника с ростера |
| `/council` | Отдаёт вопрос совету независимых брифов, каждый на своей модели и со своим усилием; свёртка называет несогласных |
| `/graph` | Гоняет оркестратор-граф: совет решает, цикл цели работает |
| `/git` | Показывает git status или diff, только чтение |
| `/genome` | Управляет картой промпта: `/genome on`, `/genome off`, `/genome limit <n>` |
| `/diagnose` | Печатает блок диагностики, чтобы вставить в баг-репорт |
| `/btw` | Сообщение, которое не пишется в историю; во время хода это steering, а не новый ход |
| `/pause` | Держит ввод и останавливает ход. Ещё раз `/pause` продолжает |
| `/help` | Список этих команд |
| Up / Down | Список команд. Он открывается, только если `/` стоит в начале строки |
| Tab | Подставляет выбранную команду. Enter на префиксе выполняет её. Esc стирает слэш |

Тот же список предлагает найденные скиллы — из `<agent_dir>/skills/`, `.agents/skills/` и `.titi/skills/`. `/name`, который не команда, а скилл, разворачивает его `SKILL.md` в промпт после той же проверки, что проходят метаданные. Скрипты скилла не выполняются никогда.

Экран — чат на ratatui: сверху модель и сессия, плюс значок `plan` или `duck`, когда движок подтвердил этот режим, посередине разговор, снизу одна строка ввода. `--mouse` по-прежнему принимается, чтобы старые команды не падали; этот экран мышь не отслеживает.

Вставка длиной от `paste.menuThreshold` (по умолчанию 100, `0` выключает) получает выбор, как дойти до модели: блоком в ограде, файлом в `.titi/pastes/` или обычным маркером `[Paste #N · …]`. `statusLine.separator` (`powerline`, `powerline-thin`, `slash`, `pipe`, `block`, `none`, `ascii`) меняет глиф между сегментами строки состояния; `statusLine.sessionAccent` (не задан = выключено) отдаёт акцент темы бездействующей рамке редактора и остатку контекстной шкалы; `statusLine.transparent` (не задан = выключено) оставляет фон этой строки терминалу. `display.turnFooter.time`, `.tokens` и `.cacheMiss` (каждый не задан = включено) гасят по одной части тусклой строки под законченным ответом.

Остальные ключи этой сборки, не задан = выключено, если не сказано иначе: `editor.vim` включает в композере два режима vim (Esc уводит из Insert в Normal, где есть `h l w b e 0 ^ $` со счётом, `x X s`, операторы `d`/`c` с движением, `dd`, `D`, `cc`, `C`, `S` и возврат по `i a I A`; Enter отправляет в любом режиме). `display.pinnedAgents` (`off|collapsed|full`, не задан = `collapsed`) прикрепляет живых агентов над композером — по строке на каждого, а `display.subagentLivePreview` (не задан = выключено) добавляет, чем он занят; `alt+a` ведёт по их панелям. `display.smoothStreaming` показывает потоковый ответ с читаемой скоростью, а не всплесками провайдера, `tui.tight` убирает по одной ячейке горизонтального отступа у композера, строки состояния и панелей. `treeFilterMode` (`default|no-tools|user-only`) задаёт фильтр, с которым открывается `/tree`, а `startup.changelog` (не задан = включено) говорит одну строку, когда сборка сменилась с прошлого запуска, и указывает на `/changelog`; `session.autoResume` (не задан = выключено) открывает свежайшую сессию агентского каталога при каждом запуске.

Фенс mermaid `flowchart`/`graph` рисуется диаграммой (сверху вниз и слева направо, подписи, цепочки, узлы `subgraph` без его рамки; неподдержанный фенс остаётся исходником), если `tui.renderMermaid` не выключен — не задан значит включено, как в omp.

Таблица GFM, числа которой читаются как одна мера, получает под собой столбиковую диаграмму: подписи слева, столбики в масштабе самого длинного, значение — у конца своего столбика. Только для 4–12 строк, первого столбца подписей (не индекса), одного столбца, где каждая ячейка — одна величина в одной единице (`12`, `1,400 tok/s`, `45%`, `$250`, `2.5k`), без отрицательных значений — и только если столбики говорят больше, чем сами числа (девять значений или трёхкратный разброс). Всё прочее — диапазон, дата, ячейка со вторым числом, слишком узкая панель — оставляет таблицу ровно как была. omp рисует это (`tui.autoGraph`, по умолчанию `always`) как SVG на графическом терминале; titi рисует текстом.


Модель, с которой начинается сессия, можно назвать: `modelRoles:` / `  default: openai/gpt-4.1` — не задано значит первая доступная модель, как и было, а закреплённый id, который недоступен, начинает там же и говорит об этом один раз, по имени.

Ключ с точкой можно писать плоско: `editor.vim: true` в корне — та же настройка, что вложенная, то есть то, что печатает `/settings`, поэтому скопированная оттуда строка работает. Если в файле есть оба написания, побеждает вложенное. Инструмент `ask` — это вопрос модели к вам: варианты, несколько если она так сказала, или свои слова; отвечают в панели над композером. `agent` запускает субагента (до четырёх за один вызов), и его жизненный цикл виден в прикреплённой строке.

В Kitty и Ghostty локальное фото, названное в разговоре, рисуется на месте: png, jpeg, gif, bmp или ico, в том числе картинка из markdown. Пиксели уходят один раз. В остальных терминалах остаётся путь. `TITI_NO_KITTY_PLACEHOLDERS=1` выключает картинки.

Подтверждение инструментов: `--approval always-ask|write|yolo`. По умолчанию `write` — чтение проходит само, запись и shell спрашивают. У headless нет панели подтверждения, поэтому режим надо задать явно, иначе запись будет ждать ответа, которого не будет.

### Инструменты

`read`, `write`, `edit`, `hashline_edit`, `glob`, `grep`, `bash`, `todo`, `memory`, `settings`.

`read` принимает необязательные `offset` (первая строка, с единицы) и `limit` (сколько строк); диапазон, в который попал не весь файл, начинается с `[lines A-B of N]`, так что большой файл читается частями. Один `read` отдаёт не больше 30 000 символов и 2000 строк без `limit`; файл длиннее приходит первой частью с `[lines 1-N of M; read on from offset N+1]`, а не с вырезанной серединой. `read` каталога показывает его содержимое, бинарный файл называется бинарным.

`glob` понимает настоящие glob-шаблоны: `*` и `?` не выходят за каталог, `**/` проходит любое их число, работают `[a-z]` и `{rs,toml}`, а шаблон без `/` ищет имя файла в любом каталоге. `grep` принимает регулярное выражение, `ignore_case` и фильтр файлов `glob`, а `path` может указывать на один файл (путь вне workspace или несуществующий — ошибка); совпадение в очень длинной строке показывается 300 символами вокруг него. Оба пропускают `target`, `.git` и `node_modules`, выдают пути по порядку и обрезают ответ (1000 путей, 500 совпавших строк), называя число остальных.

`edit` заменяет текст, который встречается ровно один раз — неоднозначный `old_string` отвергается с числом совпадений, — или все вхождения при `replace_all`; файл с CRLF находится по LF-тексту, который пишет модель, и сохраняет свои концы строк и BOM. Если `old_string` нет в файле как написано, но он ровно в одном месте совпадает построчно без учёта отступов, пробелов в конце и типографских кавычек, правится это место, новые строки получают отступы файла, а ответ говорит, что совпадение было нестрогим.

`hashline_edit` закрепляет заменяемые строки шестизначным hex-якорем того текста, который был прочитан. Строка, изменившаяся после чтения, отклоняется как устаревшее чтение, а не переписывается по снимку файла, который уже неправда.

`bash` по умолчанию работает через пайп. `pty: true` запускает команду под настоящим терминалом: она видит tty и ведёт себя так, как в вашей собственной оболочке — цвет, прогресс, пейджер. В обоих случаях запуск ограничен: `timeout_secs` (от 1 до 3600, по умолчанию 300), Ctrl+C, который останавливает команду, на которой стоял отменённый ход, и объёмом того, что сохраняется (64 KiB на pty; первые и последние 64 KiB каждого потока на пайпе — ошибка в логе обычно в конце). Остановка забирает всю группу процессов команды, а неудача начинается с `exit N`. Вывод — то, что показал бы терминал: без цветовых кодов и заголовков окна, а `\r`-прогресс-бары сведены к последнему состоянию. Ноль нигде в titi не означает «без дедлайна». Dev-сервер, вотчер или `tail -f` на переднем плане отклоняются до запуска, с подсказкой, как запустить в фоне (`cmd > log 2>&1 &`), и `timeout 30 cmd`, чтобы только проверить, что он стартует. Команда, которая переживает порог фона — настройка `bash.autoBackground.thresholdMs`, в миллисекундах, по умолчанию 60 000, отвергается при 0 или больше часа; `TITI_BASH_BACKGROUND_MS` перекрывает её, если выставлена, — передаётся как фоновое задание, а не держит ход. Результат любого инструмента обрезается до 40 000 символов, прежде чем его увидит модель: сверх этого остаются начало и конец, а между ними — пометка, сколько выпало.

`todo` — чеклист агента для многошаговой работы: `write` задаёт весь список, `update` меняет статус одного пункта по номеру, `view` показывает его. В работе не больше одного пункта, в списке до 50 пунктов по 200 символов, и живёт он столько же, сколько процесс.

`settings` даёт агенту читать и писать конфигурацию. Подтверждение (`approval_mode`, `tools.approval*`), `privacy`, каталог провайдеров (`providers`, `models`) — их корни, всё под ними и любой родитель вроде `tools` — и любой лист, похожий на credential (`key`, `apiKey`, `api_key`, `token`, `secret`, `password`), запрещены и на чтение, и на запись: ослабить подтверждение, перенаправить ключ или достать credential — решение человека.

### Как устроено

Поверхность не зовёт модель сама. Она шлёт `EngineCommand` и рисует `EngineEvent`. Повторы и смена модели — только до первого видимого токена. `401` и неизвестная модель не повторяются.

```text
TUI / headless
      │  EngineCommand / EngineEvent
      ▼
 titi-engine          ход, инструменты, отмена, компакция, режимы, агенты, совет, цикл цели
      │
      ├── titi-providers     HTTP/SSE: OpenAI, Anthropic, Gemini; цепочка фолбэков
      ├── titi-tools         read  write  edit  hashline  glob  grep  bash  settings
      ├── titi-genome        граф файлов и символов → кусок промпта
      ├── titi-memory        что вспомнить в этот ход
      ├── titi-core          сессии, траектория, брокер локального хаба
      └── titi-soul          SOUL.md и личность
```

Системный промпт доезжает до всех семейств, а не только до OpenAI-совместимых: ведущее системное сообщение поднимается в верхнеуровневый массив `system` у Anthropic и в `systemInstruction` у Gemini, а дайджест компакции в середине истории уезжает пользовательским ходом, а не исчезает.

У Anthropic запрос вдобавок собран под кэш промпта. Описания инструментов уходят отсортированными по имени, поэтому префикс стабилен побайтово между процессами, а не следует за seed'ом хеш-таблицы; каждое сообщение едет массивом из одного блока, поэтому его байты не меняются, когда оно стареет в историю; точки `cache_control` стоят на последнем инструменте, на системном блоке и на самом свежем сообщении — максимум четыре, столько разрешает протокол.

| Крейт | Зачем |
| --- | --- |
| `titi-cli` | Бинарь `titi`: экран, headless, ключи, режимы, клиент хаба |
| `titi-tui` | Кадр, композер, панели, темы. Про модель не знает |
| `titi-engine` | Протокол, цикл, реестр провайдеров, режимы, субагенты, совет, цикл цели, фоновые задания |
| `titi-providers` | Транспорт, разбор потока, раскладка под кэш промпта, цепочка фолбэков |
| `titi-tools` | Инструменты и уровни `read` / `write` / `exec`, bash на pty, hashline-правки |
| `titi-genome` | Обход репозитория, символы через tree-sitter, PageRank, проекция в системное сообщение |
| `titi-core` | Сессии JSONL, поиск, траектория, брокер хаба, share-пакет |
| `titi-memory` | Индекс памяти: полнотекст и локальные эмбеддинги |
| `titi-soul` | Слот идентичности, проверка до входа в промпт |
| `titi-config` | Слои настроек |
| `titi-secrets` | `.env` и `auth.db`, где у провайдера может быть больше одного аккаунта |

Состояние агента живёт в `~/.titi/agent`. Именованный профиль — `~/.titi/profiles/<имя>/agent`. Оба перекрывает `TITI_AGENT_DIR`. Сессии, память и ключи в репозиторий не пишутся. В проекте остаётся только `.titi/config.yml`, и ключа в нём быть не должно.

### Несколько агентов

Сессии на одной машине общаются через локальный хаб: на каталог агента приходится один брокер, он слушает `<agent_dir>/hub.sock` с правами `0600` и сверяет отправителя с тем id, под которым подключение вошло, так что один клиент не может говорить за другого. `/join` ставит сессию в ростер, `/hub` его показывает, `/leave` уводит. Никто этот сокет не ждёт: медленный, пропавший или вовсе не запущенный брокер ничего не стоит циклу чата, а «брокер не запущен» — обычный ответ, а не ошибка.

В движке есть и совет: от двух до четырёх брифов отвечают на один вопрос, каждый на своей модели и со своим усилием, и никто из них не видит остальных; свёртка отделяет то, в чём они сходятся, от того, в чём нет, и называет автора каждого расхождения. `/council` отдаёт ему вопрос из чата; `/graph` гоняет его за циклом цели.

`titi-core` умеет запечатать выгрузку сессии в share-пакет: ChaCha20-Poly1305 через аудированный крейт RustCrypto, заголовок аутентифицирован как associated data, поэтому хранилище не переклеит одну сессию под другую, а ключ возвращается отдельно и рядом с пакетом не лежит. С экрана его пока никто не выдаёт.

Карту репозитория можно напечатать отдельно:

```bash
cargo run -p titi-genome --example map -- . 40
```

`TITI_NO_GENOME=1` убирает карту из промпта. `genome.enabled` в агентском или проектном конфиге включает и выключает карту, `genome.limit` (от 1 до 64, по умолчанию 24) ограничивает число файлов в проекции; проектный файл решает, а `TITI_NO_GENOME=1` выключает на один прогон, не трогая ни то, ни другое. Каждый ход несёт и блок `<diff>`: изменения рабочего дерева рядом с картой репозитория, чтобы модель видела собственные правки. Genome индексирует двенадцать языков. Rust, TypeScript, TSX, JavaScript и Python разбираются грамматиками tree-sitter, поэтому символы берутся из синтаксического дерева; Go, Java, C, C++, C#, Ruby, Kotlin, Swift и PHP остаются на шаблонных парсерах — как и разрешение импортов везде: путь импорта это спецификатор модуля, а не объявление, и для него нужен список файлов репозитория, а не дерево.

### Что уже есть

Движок, стриминг, смена модели и цепочка фолбэков между провайдерами, инструменты в пределах текущего каталога, подтверждение опасных вызовов, hashline-правки, которые отказывают на устаревшем чтении, `bash` с дедлайном, который прерывает Ctrl+C, и на настоящем pty по запросу, режимы agent / plan / duck, сессии с восстановлением, форком, выгрузкой и откатом, компакция длинного контекста, цикл «кодер и ревьюер» с кодом выхода для CI, фоновые циклы и бюджет в токенах, советчик без инструментов, локальный хаб, субагенты (по умолчанию только чтение), Genome по двенадцати языкам с tree-sitter для Rust, TypeScript и Python, память, SOUL, скиллы, которые находятся и разворачиваются по имени, кэш промпта у Anthropic и локальные фото в Kitty и Ghostty.

### Чего ещё нет

Настольного окна на GPUI (M9). MCP, хуков и рантайма скиллов, который что-то запускает: скилл находится, его текст разворачивается, но не исполняется (M6). Прайс-таблицы в поставке: ни одна встроенная модель не несёт цены, поэтому денежный лимит по ней отклоняется с названием модели, а не пересчитывается по ставке, которую никто не писал. Объявленная вами модель может нести свою `price` (см. выше), и тогда лимит по ней соблюдается.

### Можно публиковать

Ключи, сессии, память и `SOUL.md` остаются в `~/.titi/agent`, вне этого дерева. `.gitignore` также не пускает `.env`, `*.db`, приватные ключи, `.reference-product/` и `.tmp_*`. Рабочее дерево и история git проверены на API-ключи, токены GitHub, ключи AWS, блоки приватных ключей и JWT: в коммитах их нет. В тестах стоят заглушки вроде `sk-test`.

В стейте записан публичный endpoint (`https://opencode.ai/zen/go/v1`) и идентификаторы моделей. Самого ключа там нет.

### Продолжить работу

Точка возобновления — [`docs/research/STATE.md`](docs/research/STATE.md); карта ресерча — [`docs/research/README.md`](docs/research/README.md). Цикл задачи — [`docs/CONVEYOR.md`](docs/CONVEYOR.md), майлстоуны — [`docs/PLAN.md`](docs/PLAN.md). Решения живут в доках тем под `docs/research/`.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets     # информационно, не ошибка
cargo test --workspace --locked
gh run list --repo robertt3kuk/titi --branch master --limit 5
```

CI гоняет fmt, clippy (информационно), тесты воркспейса и smoke настоящего бинаря на pty (`scripts/tui-smoke.py`): кадр готовности, список по `/`, `/help`, Ctrl+D и последовательности восстановления терминала. Ключ провайдера не используется, ход модели не запускается; каждое ожидание ограничено, поэтому зависание роняет джобу, а не висит в ней.
