# titi

A terminal coding agent in Rust. It follows the [reference product](https://reference-product.com) product model — one loop, one session, one set of tools — without Electron. The same engine drives a full-screen terminal and a headless JSONL interface. A native GPUI window comes later.

[English](#english) · [Русский](#русский)

| Version | License | Phase | As of | Tests |
| --- | --- | --- | --- | --- |
| `0.1.0` | [MIT](LICENSE) | E3 · Genome, in progress | 2026-09-22 | 935 passed |

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

Built in: OpenAI (`openai`, model `gpt-4.1`, env `OPENAI_API_KEY`), OpenRouter (`openrouter`, wire `openai/gpt-4.1`, env `OPENROUTER_API_KEY`), OpenCode (`opencode-go`, `glm-5.3-flash` and `deepseek-v4-flash`, env `OPENCODE_API_KEY`), and Anthropic. A project config overlays this list by id. It does not delete the others. `titi --set-key <id>` stores the key for that provider. The first model that has a key is the one that runs; a machine with no keys still starts on `openai/gpt-4.1` and the request fails in the open.

One turn, no screen:

```bash
titi --prompt "read Cargo.toml and tell me the version"
titi --headless --approval yolo
```

`--headless` reads `{"v":1,"command":…}` frames from stdin and writes events to stdout. The first line is `{"ready":true,"protocol":1}`.

### In the terminal

| Key or command | What it does |
| --- | --- |
| Enter | Sends the turn. During a turn it steers, it does not start a second one |
| Ctrl+C | Stops the running turn. When idle, press it again within 2 seconds to quit |
| Ctrl+D | Quits when the input line is empty |
| `y` / `n` | Approves or refuses a write or a shell command |
| `/model` | Switches to the next model that has a key. `/model <id>` picks one |
| `/checkpoint` | Records a rewind point, and the git HEAD when the index is clean |
| `/checkpoints` | Lists this session's rewind points |
| `/rewind` | Cuts the session back to the newest point. `/rewind 2` picks one |
| `/recap` | Prints what the session did: turns, tools, files, problems |
| `/context` | What fills the context window now: system prompt, project rules, skills, recalled memory, genome map, history, tool specs, each with its estimated tokens and share. The numbers are estimates, not provider counts |
| `/compact` | Folds the history now instead of waiting for the threshold. `/compact auth` keeps the folded lines that mention `auth` in the digest |
| `/pause` | Holds the composer and stops the running turn. `/pause` again resumes |
| `/help` | Lists these commands |
| `/login` | `/login` lists providers. `/login openai` asks for the key and stores it masked. `/login openai <key>` stores it in one step |
| `/logout` | Forgets the stored key for a provider. An environment variable is left alone |
| `/keys` | Which providers have a key: env, stored, or none. The key itself is never shown |
| Up / Down | Move through the command list. It opens only when `/` is the start of the line |
| Tab | Fills the highlighted command. Enter on a prefix runs it. Esc clears the slash |

The screen is a ratatui chat: model and session on top, the transcript in the middle, one input line at the bottom. `--mouse` is still accepted so older commands do not fail; this screen does not track the mouse.

In Kitty or Ghostty, a local photo named in the transcript is drawn in place: png, jpeg, gif, bmp, or ico, including a markdown image. The pixels are sent once. Other terminals leave the path as text. `TITI_NO_KITTY_PLACEHOLDERS=1` turns the pictures off.

Tool approval: `--approval always-ask|write|yolo`. The default is `write` — reads pass, writes and the shell ask. Headless has no approval panel, so the mode has to be set explicitly or a write waits for an answer that never comes.

### How it fits

A surface never calls the model itself. It sends `EngineCommand` and paints `EngineEvent`. Retries and model switches happen only before the first visible token. A `401` and an unknown model are not retried.

```text
TUI / headless
      │  EngineCommand / EngineEvent
      ▼
 titi-engine          turn, tools, cancel, compaction, agents
      │
      ├── titi-providers     HTTP/SSE: OpenAI, Anthropic, Gemini
      ├── titi-tools         read  write  edit  glob  grep  bash
      ├── titi-genome        file and symbol graph → a prompt fragment
      ├── titi-memory        what to recall on this turn
      └── titi-soul          SOUL.md and personality
```

| Crate | Role |
| --- | --- |
| `titi-cli` | The `titi` binary: screen, headless, keys |
| `titi-tui` | Frame, composer, panels, themes. It does not know the model |
| `titi-engine` | Protocol, loop, provider registry, subagents |
| `titi-providers` | Transport and stream decoding |
| `titi-tools` | Tools and the `read` / `write` / `exec` tiers |
| `titi-genome` | Walk the repo, PageRank, project into a system message |
| `titi-core` | JSONL sessions, search, trajectory |
| `titi-memory` | Memory index: full-text search and local embeddings |
| `titi-soul` | Identity slot, scanned before it enters the prompt |
| `titi-config` | Layered settings |
| `titi-secrets` | `.env` and `auth.db` |

Agent state lives in `~/.titi/agent`. A named profile uses `~/.titi/profiles/<name>/agent`. `TITI_AGENT_DIR` overrides both. Sessions, memory, and keys are not written into the repository. The only project file is `.titi/config.yml`, and it must not contain a key.

Print the repository map on its own:

```bash
cargo run -p titi-genome --example map -- . 40
```

`TITI_NO_GENOME=1` leaves the map out of the prompt.

### What is already here

The engine, streaming, model switching, tools jailed to the current directory, approval for dangerous calls, sessions that restore and rewind, compaction of a long context, subagents that can only read unless asked otherwise, Genome across eleven languages, memory, SOUL, and local photos in Kitty and Ghostty.

### What is not

A desktop window (E5). Skills, hooks, and MCP. A dollar cost. A tree-sitter parser in place of the current heuristics.

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

Встроены: OpenAI (`openai`, модель `gpt-4.1`, env `OPENAI_API_KEY`), OpenRouter (`openrouter`, в запросе `openai/gpt-4.1`, env `OPENROUTER_API_KEY`), OpenCode (`opencode-go`, `glm-5.3-flash` и `deepseek-v4-flash`, env `OPENCODE_API_KEY`) и Anthropic. Конфиг проекта накладывается на этот список по id и не удаляет остальные. `titi --set-key <id>` кладёт ключ этого провайдера. Запускается первая модель, у которой ключ уже есть. Если ключей нет, остаётся `openai/gpt-4.1`, и запрос падает открыто.

Один ход без экрана:

```bash
titi --prompt "прочитай Cargo.toml и скажи версию"
titi --headless --approval yolo
```

`--headless` читает со stdin кадры `{"v":1,"command":…}` и пишет события в stdout. Первая строка — `{"ready":true,"protocol":1}`.

### В терминале

| Клавиша или команда | Что делает |
| --- | --- |
| Enter | Отправляет ход. Во время хода это steering, а не второй ход |
| Ctrl+C | Останавливает активный ход. В покое второе нажатие за 2 секунды выходит |
| Ctrl+D | Выход, если строка ввода пустая |
| `y` / `n` | Разрешить или отказать записи и shell |
| `/model` | Следующая модель, у которой есть ключ. `/model <id>` выбирает конкретную |
| `/checkpoint` | Точка отката и git HEAD, если индекс чистый |
| `/checkpoints` | Список точек отката этой сессии |
| `/rewind` | Откат к последней точке. `/rewind 2` выбирает номер |
| `/recap` | Что было в сессии: ходы, инструменты, файлы, ошибки |
| `/context` | Что сейчас занимает окно: системный промпт, правила проекта, скиллы, вспомненная память, карта репозитория, история, описания инструментов — у каждого оценка в токенах и доля. Это оценка, а не счёт провайдера |
| `/compact` | Сворачивает историю сразу, не дожидаясь порога. `/compact auth` оставляет в дайджесте свёрнутые строки, где упомянут `auth` |
| `/pause` | Держит ввод и останавливает ход. Ещё раз `/pause` продолжает |
| `/help` | Список этих команд |
| `/login` | `/login` показывает провайдеров. `/login openai` просит ключ и прячет его. `/login openai <ключ>` сохраняет сразу |
| `/logout` | Забывает сохранённый ключ провайдера. Переменную окружения не трогает |
| `/keys` | У кого есть ключ: env, сохранён или нет. Сам ключ не показывается |
| Up / Down | Список команд. Он открывается, только если `/` стоит в начале строки |
| Tab | Подставляет выбранную команду. Enter на префиксе выполняет её. Esc стирает слэш |

Экран — чат на ratatui: сверху модель и сессия, посередине разговор, снизу одна строка ввода. `--mouse` по-прежнему принимается, чтобы старые команды не падали; этот экран мышь не отслеживает.

В Kitty и Ghostty локальное фото, названное в разговоре, рисуется на месте: png, jpeg, gif, bmp или ico, в том числе картинка из markdown. Пиксели уходят один раз. В остальных терминалах остаётся путь. `TITI_NO_KITTY_PLACEHOLDERS=1` выключает картинки.

Подтверждение инструментов: `--approval always-ask|write|yolo`. По умолчанию `write` — чтение проходит само, запись и shell спрашивают. У headless нет панели подтверждения, поэтому режим надо задать явно, иначе запись будет ждать ответа, которого не будет.

### Как устроено

Поверхность не зовёт модель сама. Она шлёт `EngineCommand` и рисует `EngineEvent`. Повторы и смена модели — только до первого видимого токена. `401` и неизвестная модель не повторяются.

```text
TUI / headless
      │  EngineCommand / EngineEvent
      ▼
 titi-engine          ход, инструменты, отмена, компакция, агенты
      │
      ├── titi-providers     HTTP/SSE: OpenAI, Anthropic, Gemini
      ├── titi-tools         read  write  edit  glob  grep  bash
      ├── titi-genome        граф файлов и символов → кусок промпта
      ├── titi-memory        что вспомнить в этот ход
      └── titi-soul          SOUL.md и личность
```

| Крейт | Зачем |
| --- | --- |
| `titi-cli` | Бинарь `titi`: экран, headless, ключи |
| `titi-tui` | Кадр, композер, панели, темы. Про модель не знает |
| `titi-engine` | Протокол, цикл, реестр провайдеров, субагенты |
| `titi-providers` | Транспорт и разбор потока |
| `titi-tools` | Инструменты и уровни `read` / `write` / `exec` |
| `titi-genome` | Обход репозитория, PageRank, проекция в системное сообщение |
| `titi-core` | Сессии JSONL, поиск, траектория |
| `titi-memory` | Индекс памяти: полнотекст и локальные эмбеддинги |
| `titi-soul` | Слот идентичности, проверка до входа в промпт |
| `titi-config` | Слои настроек |
| `titi-secrets` | `.env` и `auth.db` |

Состояние агента живёт в `~/.titi/agent`. Именованный профиль — `~/.titi/profiles/<имя>/agent`. Оба перекрывает `TITI_AGENT_DIR`. Сессии, память и ключи в репозиторий не пишутся. В проекте остаётся только `.titi/config.yml`, и ключа в нём быть не должно.

Карту репозитория можно напечатать отдельно:

```bash
cargo run -p titi-genome --example map -- . 40
```

`TITI_NO_GENOME=1` убирает карту из промпта.

### Что уже есть

Движок, стриминг, смена модели, инструменты в пределах текущего каталога, подтверждение опасных вызовов, сессии с восстановлением и откатом, компакция длинного контекста, субагенты (по умолчанию только чтение), Genome по одиннадцати языкам, память, SOUL и локальные фото в Kitty и Ghostty.

### Чего ещё нет

Настольного окна (E5). Скиллов, хуков и MCP. Стоимости в долларах. Парсера tree-sitter вместо текущих эвристик.

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
