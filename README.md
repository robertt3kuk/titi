# titi

A terminal coding agent in Rust. It follows the [reference product](https://reference-product.com) product model — one loop, one session, one set of tools — without Electron. The same engine drives a full-screen terminal and a headless JSONL interface. A native GPUI window comes later.

[English](#english) · [Русский](#русский)

| Version | License | Phase | As of | Tests |
| --- | --- | --- | --- | --- |
| `0.1.0` | [MIT](LICENSE) | Phase 3–4 · tools, providers, agents — in progress | 2026-09-23 | 1279 passed |

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

Without a key the turn fails in the open: the provider requires a credential. Settings stack from lowest to highest: built-in defaults, `~/.titi/agent`, `<project>/.titi/config.yml`, then environment variables.

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

`--headless` reads `{"v":1,"command":…}` frames from stdin and writes events to stdout. The first line is `{"ready":true,"protocol":1}`.

`--goal` runs the coder/reviewer loop without a screen and exits with the code CI reads: `0` when the reviewer passes it, `1` on a partial verdict, `3` on anything else, a missing verdict included. Events still go to stdout as JSONL; the report line goes to stderr, where a shell script can read it without parsing the stream.

`--mode agent|plan|duck` picks what a session may reach. A mode is not advice the model may ignore — it decides which tools are registered at all. `agent` registers everything: read, write, exec. `plan` registers only the read-only tools, so the turn answers with a plan instead of a change. `duck` registers no filesystem or shell tool at all and sends no repository map: a repo-blind partner to talk something through. `/plan`, `/duck`, and `/done` switch mid-session.

### In the terminal

| Key or command | What it does |
| --- | --- |
| Enter | Sends the turn. During a turn it steers, it does not start a second one |
| Ctrl+C | Stops the running turn. When idle, press it again within 2 seconds to quit |
| Ctrl+D | Quits when the input line is empty |
| `y` / `n` | Approves or refuses a write or a shell command |
| `/model` | Switches to the next model in the list. `/model <id>` picks one by id or by its short name |
| `/switch` | Fuzzy search over the same list: `/switch opus`, `/switch anthropic/claude-sonnet-4-5`. `/switch @review:high` resolves a model role from the settings |
| `/usage` | Tokens for this turn and for the session, prompt and completion apart |
| `/budget` | Caps what the session may spend: `/budget 200k`, `/budget 1.5m`, `/budget off`. The cap is in tokens; a cap in money is refused, because nothing here knows a price |
| `/settings` | Every resolved setting with the layer it came from |
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
| `/hub` | Shows or hides the roster of the local hub |
| `/join` | Joins the local hub, `/join <name>` under a name of your own; the default is the session id |
| `/leave` | Leaves the hub, which unregisters this peer |
| `/skillful` | Toggles skillful mode for this session |
| `/btw` | Sends a message that is not recorded in the history; during a turn it steers instead of starting one |
| `/pause` | Holds the composer and stops the running turn. `/pause` again resumes |
| `/help` | Lists these commands |
| Up / Down | Move through the command list. It opens only when `/` is the start of the line |
| Tab | Fills the highlighted command. Enter on a prefix runs it. Esc clears the slash |

The picker also offers the skills it found in `<agent_dir>/skills/`, `.agents/skills/`, and `.titi/skills/`. A `/name` that is not a command and is a skill expands that `SKILL.md` into the prompt, after the same screening the metadata gets. A skill's scripts are never executed.

The screen is a ratatui chat: model and session on top — plus a `plan` or `duck` badge when the engine confirms that mode — the transcript in the middle, one input line at the bottom. `--mouse` is still accepted so older commands do not fail; this screen does not track the mouse.

In Kitty or Ghostty, a local photo named in the transcript is drawn in place: png, jpeg, gif, bmp, or ico, including a markdown image. The pixels are sent once. Other terminals leave the path as text. `TITI_NO_KITTY_PLACEHOLDERS=1` turns the pictures off.

Tool approval: `--approval always-ask|write|yolo`. The default is `write` — reads pass, writes and the shell ask. Headless has no approval panel, so the mode has to be set explicitly or a write waits for an answer that never comes.

### The tools

`read`, `write`, `edit`, `hashline_edit`, `glob`, `grep`, `bash`, `memory`, `settings`.

`hashline_edit` pins the lines it replaces by the six-hex anchor of the text that was read. A line that changed since the read is refused as a stale read instead of being overwritten from a picture of the file that is no longer true.

`bash` runs through a pipe by default. `pty: true` runs it under a real terminal, so the command sees a tty and behaves the way it would in your own shell — colour, progress, paging. A pty run is bounded on three axes: `timeout_secs` (1 to 3600, 300 by default), a 64 KiB cap on captured output, and an interrupt the surface can raise. Zero means "no deadline" nowhere in titi.

`settings` lets the agent read and write configuration. `approval_mode`, everything under `privacy.`, and any key ending in `.key` are refused: loosening approval or reading out a credential is a human decision.

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

The engine also seats a council: two to four briefs answer the same question on models and efforts of their own, none of them seeing the others, and one fold separates what they agree on from what they do not, naming the member that holds each dissent. It is an engine command today (`RunCouncil`), with no slash command in front of it yet.

`titi-core` can seal a session export into a share package: ChaCha20-Poly1305 through the audited RustCrypto crate, header authenticated as associated data so a host cannot relabel one session as another, and the key returned separately instead of being stored with the package. Nothing hands one out from the screen yet.

Print the repository map on its own:

```bash
cargo run -p titi-genome --example map -- . 40
```

`TITI_NO_GENOME=1` leaves the map out of the prompt. Genome indexes twelve languages. Rust, TypeScript, TSX, JavaScript, and Python are parsed with tree-sitter grammars, so the symbols come from the syntax tree; Go, Java, C, C++, C#, Ruby, Kotlin, Swift, and PHP stay on pattern parsers, and so does import resolution everywhere — an import path is a module specifier, not a declaration, and resolving it needs the repo's file set rather than a tree.

### What is already here

The engine, streaming, model switching and a fallback chain across providers, tools jailed to the current directory, approval for dangerous calls, hashline edits that refuse a stale read, `bash` on a real pty with a timeout, agent / plan / duck modes, sessions that restore, fork, export, and rewind, compaction of a long context, a coder-and-reviewer goal loop with an exit code CI can read, background loops and a token budget, a toolless advisor, a local hub, subagents that can only read unless asked otherwise, Genome over twelve languages with tree-sitter for Rust, TypeScript, and Python, memory, SOUL, skills that are discovered and expanded by name, prompt caching on Anthropic, and local photos in Kitty and Ghostty.

### What is not

A desktop window on GPUI (M9). MCP, hooks, and a skills runtime that runs anything: a skill is found and its text is expanded, never executed (M6). A price table — everything here is counted in tokens, and `/budget $10` is refused instead of converted at a made-up rate.

### Safe to publish

Keys, sessions, memory, and `SOUL.md` stay in `~/.titi/agent`, outside this tree. `.gitignore` also refuses `.env`, `*.db`, private keys, `.reference-product/`, and `.tmp_*`. The working tree and the git history were scanned for API keys, GitHub tokens, AWS keys, private-key blocks, and JWTs: none are committed. Tests use placeholders such as `sk-test`.

The handoff notes a public endpoint (`https://opencode.ai/zen/go/v1`) and model ids. It does not contain the key.

### Continue the work

The resume point is [`docs/research/STATE.md`](docs/research/STATE.md); the research map is [`docs/research/README.md`](docs/research/README.md). The task cycle is [`docs/CONVEYOR.md`](docs/CONVEYOR.md) and the milestones are [`docs/PLAN.md`](docs/PLAN.md). Decisions live in the theme docs under `docs/research/`.

```bash
cargo fmt --check
cargo test -p titi-engine
cargo clippy -p titi-engine --all-targets
cargo test --workspace
```

CI runs fmt, clippy (informational), the workspace tests, and a smoke of the real binary on a pty (`scripts/tui-smoke.py`): the ready frame, the `/` listing, `/help`, Ctrl+D, then the terminal restore sequences. No provider key is used and no model turn runs; every wait is bounded, so a hang fails the job instead of hanging it.

---

## Русский

Терминальный агент на Rust. Продуктовая модель [reference product](https://reference-product.com): один цикл, одна сессия, одни инструменты — без Electron. Тот же движок обслуживает полноэкранный терминал и headless JSONL. Нативное окно на GPUI ещё впереди.

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

Без ключа ход падает открыто: провайдер требует credential. Настройки складываются снизу вверх: встроенные значения, `~/.titi/agent`, `<проект>/.titi/config.yml`, затем переменные окружения.

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

`--headless` читает со stdin кадры `{"v":1,"command":…}` и пишет события в stdout. Первая строка — `{"ready":true,"protocol":1}`.

`--goal` гоняет цикл «кодер + ревьюер» без экрана и выходит с кодом, который читает CI: `0`, если ревьюер принял, `1` при частичном вердикте, `3` во всех остальных случаях, включая отсутствие вердикта. События по-прежнему уходят в stdout как JSONL; строка отчёта — в stderr, чтобы её мог прочитать shell-скрипт, не разбирая поток.

`--mode agent|plan|duck` выбирает, до чего сессия вообще дотянется. Режим — не совет, который модель может проигнорировать: он решает, какие инструменты зарегистрированы. `agent` — всё: чтение, запись, shell. `plan` — только читающие инструменты, поэтому ход отвечает планом, а не правкой. `duck` не регистрирует ни файловых, ни shell-инструментов и не шлёт карту репозитория: это собеседник, слепой к репозиторию. Переключение на ходу — `/plan`, `/duck`, `/done`.

### В терминале

| Клавиша или команда | Что делает |
| --- | --- |
| Enter | Отправляет ход. Во время хода это steering, а не второй ход |
| Ctrl+C | Останавливает активный ход. В покое второе нажатие за 2 секунды выходит |
| Ctrl+D | Выход, если строка ввода пустая |
| `y` / `n` | Разрешить или отказать записи и shell |
| `/model` | Следующая модель в списке. `/model <id>` выбирает по id или по короткому имени |
| `/switch` | Нечёткий поиск по тому же списку: `/switch opus`, `/switch anthropic/claude-sonnet-4-5`. `/switch @review:high` разворачивает роль модели из настроек |
| `/usage` | Токены за ход и за сессию, prompt и completion отдельно |
| `/budget` | Ограничивает трату сессии: `/budget 200k`, `/budget 1.5m`, `/budget off`. Лимит в токенах; лимит в деньгах отклоняется — прайса здесь никто не знает |
| `/settings` | Все разрешённые настройки и слой, из которого пришла каждая |
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
| `/hub` | Показывает или прячет ростер локального хаба |
| `/join` | Подключает к локальному хабу, `/join <имя>` — под своим именем; по умолчанию это id сессии |
| `/leave` | Выходит из хаба и снимает этого участника с ростера |
| `/skillful` | Переключает skillful-режим сессии |
| `/btw` | Сообщение, которое не пишется в историю; во время хода это steering, а не новый ход |
| `/pause` | Держит ввод и останавливает ход. Ещё раз `/pause` продолжает |
| `/help` | Список этих команд |
| Up / Down | Список команд. Он открывается, только если `/` стоит в начале строки |
| Tab | Подставляет выбранную команду. Enter на префиксе выполняет её. Esc стирает слэш |

Тот же список предлагает найденные скиллы — из `<agent_dir>/skills/`, `.agents/skills/` и `.titi/skills/`. `/name`, который не команда, а скилл, разворачивает его `SKILL.md` в промпт после той же проверки, что проходят метаданные. Скрипты скилла не выполняются никогда.

Экран — чат на ratatui: сверху модель и сессия, плюс значок `plan` или `duck`, когда движок подтвердил этот режим, посередине разговор, снизу одна строка ввода. `--mouse` по-прежнему принимается, чтобы старые команды не падали; этот экран мышь не отслеживает.

В Kitty и Ghostty локальное фото, названное в разговоре, рисуется на месте: png, jpeg, gif, bmp или ico, в том числе картинка из markdown. Пиксели уходят один раз. В остальных терминалах остаётся путь. `TITI_NO_KITTY_PLACEHOLDERS=1` выключает картинки.

Подтверждение инструментов: `--approval always-ask|write|yolo`. По умолчанию `write` — чтение проходит само, запись и shell спрашивают. У headless нет панели подтверждения, поэтому режим надо задать явно, иначе запись будет ждать ответа, которого не будет.

### Инструменты

`read`, `write`, `edit`, `hashline_edit`, `glob`, `grep`, `bash`, `memory`, `settings`.

`hashline_edit` закрепляет заменяемые строки шестизначным hex-якорем того текста, который был прочитан. Строка, изменившаяся после чтения, отклоняется как устаревшее чтение, а не переписывается по снимку файла, который уже неправда.

`bash` по умолчанию работает через пайп. `pty: true` запускает команду под настоящим терминалом: она видит tty и ведёт себя так, как в вашей собственной оболочке — цвет, прогресс, пейджер. Запуск на pty ограничен по трём осям: `timeout_secs` (от 1 до 3600, по умолчанию 300), 64 KiB захваченного вывода и прерывание, которое может поднять поверхность. Ноль нигде в titi не означает «без дедлайна».

`settings` даёт агенту читать и писать конфигурацию. `approval_mode`, всё под `privacy.` и любой ключ, кончающийся на `.key`, запрещены: ослабить подтверждение или достать credential — решение человека.

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

В движке есть и совет: от двух до четырёх брифов отвечают на один вопрос, каждый на своей модели и со своим усилием, и никто из них не видит остальных; свёртка отделяет то, в чём они сходятся, от того, в чём нет, и называет автора каждого расхождения. Сегодня это команда движка (`RunCouncil`), слэш-команды перед ней пока нет.

`titi-core` умеет запечатать выгрузку сессии в share-пакет: ChaCha20-Poly1305 через аудированный крейт RustCrypto, заголовок аутентифицирован как associated data, поэтому хранилище не переклеит одну сессию под другую, а ключ возвращается отдельно и рядом с пакетом не лежит. С экрана его пока никто не выдаёт.

Карту репозитория можно напечатать отдельно:

```bash
cargo run -p titi-genome --example map -- . 40
```

`TITI_NO_GENOME=1` убирает карту из промпта. Genome индексирует двенадцать языков. Rust, TypeScript, TSX, JavaScript и Python разбираются грамматиками tree-sitter, поэтому символы берутся из синтаксического дерева; Go, Java, C, C++, C#, Ruby, Kotlin, Swift и PHP остаются на шаблонных парсерах — как и разрешение импортов везде: путь импорта это спецификатор модуля, а не объявление, и для него нужен список файлов репозитория, а не дерево.

### Что уже есть

Движок, стриминг, смена модели и цепочка фолбэков между провайдерами, инструменты в пределах текущего каталога, подтверждение опасных вызовов, hashline-правки, которые отказывают на устаревшем чтении, `bash` на настоящем pty с таймаутом, режимы agent / plan / duck, сессии с восстановлением, форком, выгрузкой и откатом, компакция длинного контекста, цикл «кодер и ревьюер» с кодом выхода для CI, фоновые циклы и бюджет в токенах, советчик без инструментов, локальный хаб, субагенты (по умолчанию только чтение), Genome по двенадцати языкам с tree-sitter для Rust, TypeScript и Python, память, SOUL, скиллы, которые находятся и разворачиваются по имени, кэш промпта у Anthropic и локальные фото в Kitty и Ghostty.

### Чего ещё нет

Настольного окна на GPUI (M9). MCP, хуков и рантайма скиллов, который что-то запускает: скилл находится, его текст разворачивается, но не исполняется (M6). Прайс-таблицы — здесь всё считается в токенах, и `/budget $10` отклоняется, а не пересчитывается по выдуманной ставке.

### Можно публиковать

Ключи, сессии, память и `SOUL.md` остаются в `~/.titi/agent`, вне этого дерева. `.gitignore` также не пускает `.env`, `*.db`, приватные ключи, `.reference-product/` и `.tmp_*`. Рабочее дерево и история git проверены на API-ключи, токены GitHub, ключи AWS, блоки приватных ключей и JWT: в коммитах их нет. В тестах стоят заглушки вроде `sk-test`.

В стейте записан публичный endpoint (`https://opencode.ai/zen/go/v1`) и идентификаторы моделей. Самого ключа там нет.

### Продолжить работу

Точка возобновления — [`docs/research/STATE.md`](docs/research/STATE.md); карта ресерча — [`docs/research/README.md`](docs/research/README.md). Цикл задачи — [`docs/CONVEYOR.md`](docs/CONVEYOR.md), майлстоуны — [`docs/PLAN.md`](docs/PLAN.md). Решения живут в доках тем под `docs/research/`.

```bash
cargo fmt --check
cargo test -p titi-engine
cargo clippy -p titi-engine --all-targets
cargo test --workspace
```

CI гоняет fmt, clippy (информационно), тесты воркспейса и smoke настоящего бинаря на pty (`scripts/tui-smoke.py`): кадр готовности, список по `/`, `/help`, Ctrl+D и последовательности восстановления терминала. Ключ провайдера не используется, ход модели не запускается; каждое ожидание ограничено, поэтому зависание роняет джобу, а не висит в ней.
