# Упаковка и headless-поверхности

> Сравнительный research 2026-08. Продуктовые решения — этот README и [карта тем](../README.md). Headless/RPC сериализует тот же `EngineCommand`/`EngineEvent`, что TUI и будущий GPUI.

Тема: CLI parity (паритет интерактивного и headless-поверхностей), установка/подпись macOS, install-id, user-facing packages, RPC mode, SDK, collab.

## omp

1. **Единая CLI-поверхность с default-командой.** `omp` вызывается как `omp [command] [flags] [messages...]`; если первый не-флаговый аргумент — не зарегистрированная подкоманда, происходит маршрутизация в default-команду `launch`, а аргументы становятся начальным промптом (`omp "fix the build"`). Headless — это не отдельный бинарь, а флаги той же поверхности: `--print/-p` (обработать промпт, стрим в stdout, выход без TUI) и `--mode text|json|rpc|acp|rpc-ui`. Все ~40 подкоманд зарегистрированы в `packages/coding-agent/src/cli-commands.ts`; `omp --help` показывает только user-facing подмножество. Источник: omp://cli-reference.md.
2. **install-id — стабильная инсталляционная идентичность.** `getInstallId()` из `packages/utils/src/dirs.ts` возвращает UUID v4 (`crypto.randomUUID()`), персистится в `~/.omp/install-id` (mode `0o600`, trailing `\n`) относительно базового config-root независимо от профиля — один ID на инсталляцию, не на профиль. Запись через `open(O_WRONLY|O_CREAT|O_EXCL, 0o600)`: при гонке двух процессов проигравший получает `EEXIST` и перечитывает файл победителя; мусор в существующем файле предварительно `unlink`-ается. Потребители: OpenAI Codex `installationId`, Claude-совместимый `device_id` (scoped по account UUID), отчёты usage в auth-broker, auto-QA grievances (`installId`). Значение трактуется как opaque, PII не содержит. Источник: omp://install-id.md.
3. **Подпись/notarization macOS — CI-pipeline с auto-skip.** Бинарь собирается и ad-hoc подписывается на раннере, затем `scripts/ci-macos-sign.sh` в матрице `release_binary_darwin` импортирует Developer ID cert во временный keychain, переподписывает с `--options runtime --timestamp` и entitlements (`com.apple.security.cs.allow-jit`, `allow-unsigned-executable-memory`, `disable-library-validation` — обязателен, т.к. omp `dlopen()`-ит извлечённые нативные аддоны `pi_natives.<triple>.node` с чужим Team ID), гоняет `--version` и `--smoke-test` под новой подписью, затем `notarytool submit --wait`. Шаг auto-skip без всех пяти `APPLE_*` секретов. Голый Mach-O нельзя застейплить (`stapler` — только `.app`/`.pkg`/`.dmg`): ticket достаётся онлайн; `curl | sh` и Homebrew formula не ставят quarantine-бит и Gatekeeper не консультируется, а браузерный download/кэск требует онлайн-lookup — для offline-дистрибутива нужен notarized `.pkg`/`.dmg`. Источник: omp://macos-signing-notarization.md.
4. **User-facing packages — политика корневых доков.** Правило: корневые доки покрывают package-local CLI, дашборды и bench-раннеры, доступные пользователю напрямую или через `omp`; внутренние крейты — исключены. Примеры: `omp stats` (`@oh-my-pi/omp-stats`, порт 3847, аггрегаты в `~/.omp/stats.db` из сессий `~/.omp/agent/sessions/`), `robomp` (Python-сервис GitHub-триажа, поднимает `omp --mode rpc` сессию на issue), `omp browser-relay` (loopback + `--token`), `collab-web`, `mnemopi`, `snapcompact`, `omptype`, `metaharness`. README/манифесты остаются источником истины по флагам, корневые доки — точка обнаружения со ссылками на пути исходников. Источник: omp://user-facing-packages.md.
5. **RPC mode — NDJSON-протокол поверх stdio с версионированием.** Запуск `omp --mode rpc`: ready-кадр `{type:"ready", protocolVersion:1, supportedProtocolVersions:[1,2], maxFrameBytes:1048576, maxReassembledFrameBytes:67108864}`, затем клиент шлёт `negotiate_protocol`. Физический stdout-кадр v1 ограничен 1 MiB; v2 эмитит oversized-объекты losslessly последовательностью `rpc_chunk` (base64-сегменты с `chunkId/index/count/byteLength`, строгая валидация и лимит reassembly). Команды канонизированы в `rpc-types.ts` (`prompt`, `steer`, `follow_up`, `abort`, `get_state`, `set_host_tools`, `set_host_uri_schemes`, `get_messages_page` с курсорами и machine-readable кодами `session_busy`/`stale_cursor` и др.). Ключевая семантика: `prompt`/`abort_and_prompt` акуются немедленно — завершение только по `agent_end` c `isTerminal !== false` или `prompt_result`/`data.agentInvoked: false` для local-only. Хост может регистрировать host-tools (callback `host_tool_call`/`host_tool_result`) и host-URI-схемы (`host_uri_request`). Источник: omp://rpc.md.
6. **SDK — in-process embedding.** `@oh-my-pi/pi-coding-agent`: `createAgentSession()` с принципом «provide to override, omit to discover» (cwd, agentDir, authStorage, ModelRegistry, Settings, SessionManager, skills/rules/extensions/MCP/LSP). `SessionManager.create()` — файловый `.jsonl` (resume/fork), `SessionManager.inMemory()` — без персистенции. `AuthStorage.getApiKey` резолвится по 7-шаговому приоритету (runtime override → models.yml → OAuth → /login → env → broker → custom resolver). Для нескольких сессий в одном процессе — приватный `AgentRegistry` на сессию (дефолтный глобальный пускает один `"Main"` на поколение). Стартовые оптимизации: `fetch.preconnect(model.baseUrl)` параллельно с загрузкой расширений (экономит 100–300 мс) и условный LSP warmup только для интерактивных сессий с выключенным `lsp.lazy`. Источник: omp://sdk.md.
7. **Collab — E2E-шифрование и hub-топология.** `/collab` печатает join-ссылку `<roomId>.<key>`: 48 байт base64url = 32-байтный AES-256-GCM room key + 16-байтный write token; view-only ссылка — голые 32 байта ключа. Каждый payload (entries, events, state, prompts) шифруется AES-256-GCM до сокета; relay видит только room id, счётчики соединений, шифротекст и 4-байтный routing prefix. Хост авторитарен, гости не пирятся; кадры: `welcome`/`snapshot-chunk`, `entry` (дублируются в реплику `~/.omp/collab/<roomId>.jsonl` через обычный `/resume`-механизм — поэтому `/dump` и ctrl+o нативны), `event`, `state`, `bus`, `agents`, `ui-request`. Гость с полным токеном может промптить/прерывать/управлять subagent'ами; хост верифицирует write token при join. Прод-релей (Go, content-blind) не распространяется для self-hosting; для дева есть `packages/collab-web/scripts/local-relay.ts` (ws://localhost:7466). Настройки: `collab.relayUrl` (дефолт `wss://my.omp.sh`), `collab.webUrl`, `collab.displayName`. Источник: omp://collab.md.

## Hermes

1. **Установка: shell-инсталлер с двумя layout'ами и авто-детекцией метода.** `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash` (или Desktop-инсталлер на macOS/Windows). Per-user: код в `~/.hermes/hermes-agent/`, бинарь — symlink `~/.local/bin/hermes`, данные в `~/.hermes/`; root-mode — FHS-раскладка `/usr/local/lib/hermes-agent/` + `/usr/local/bin/hermes` для общих машин. Инсталлер сам ставит uv/Python 3.11/Node 22/ripgrep/ffmpeg, поддерживает `--skip-browser` и `--skip-computer-use` для headless-инсталляций без Chromium; `hermes doctor` диагностирует; метод установки (git/Docker/NixOS) детектируется по раскладке без env-переменных. Источник: https://hermes-agent.nousresearch.com/docs/getting-started/installation.
2. **Headless-поверхности: три протокола вместо одного `--mode rpc`.** Hermes явно не имеет `--mode rpc`; вместо него: (а) **ACP** — `hermes acp`, JSON-RPC over stdio для IDE (VS Code/Zed/JetBrains); (б) **TUI gateway JSON-RPC** — `tui_gateway/server.py`, stdio или WebSocket, полный каталог методов (`prompt.submit`, `session.steer`, `session.compress`, `approval.respond`, `command.dispatch`...), с явной таблицей маппинга Pi-RPC команд (`prompt`→`prompt.submit`, `abort`→`session.interrupt`, `get_state`→`session.status`); (в) **OpenAI-compatible API server** — `gateway/platforms/api_server.py`: `POST /v1/chat/completions` (SSE), stateful `/v1/responses`, асинхронные `/v1/runs` (202 + run_id) с `/approval`, `/steer` (только в статусе `running`, иначе `409 run_not_accepting_steer`; недоставленный steer возвращается как `pending_steer` в `run.completed`), `/stop`, `GET /v1/capabilities`. Все три драйвят один `AIAgent` core. Источник: https://hermes-agent.nousresearch.com/docs/developer-guide/programmatic-integration.
3. **Desktop-приложение — тот же агент, нативная упаковка.** Нативное приложение (Electron 20+, macOS/Windows/Linux, включая нативный Wayland-клиент) вокруг того же ядра: тот же конфиг, ключи, сессии, скиллы, память; запускается `hermes desktop`. Обновления: фоновая проверка + one-click update, при нескольких update-таргетах сначала бэкенды, потом само приложение (обновление клиента перезапускает app); после апдейта бэкенда приложение перепроверяет свою версию и предупреждает о stale-сборке. Источник: https://hermes-agent.nousresearch.com/docs/user-guide/desktop.
4. **Batch-режим для headless-траекторий.** Отдельная страница «Batch Processing: Generate agent trajectories at scale — parallel processing, checkpointing, and toolset distributions» — пакетная генерация траекторий агента для обучения/оценки без интерактива. Источник: https://hermes-agent.nousresearch.com/docs/llms.txt (индекс; страница /docs/user-guide/features/batch-processing).

## Vellum

Тема упаковки, CLI parity, подписи и RPC-протоколов **не покрывается**: в материале нет ни слова про CLI-поверхность, headless-режимы, дистрибуцию или подпись бинарей. Ближайшие аналоги:

1. **Каналы вместо headless-режимов.** macOS, iOS, Web, Voice, Email, Telegram, Slack, Twilio — «один ассистент, одна память, каждый канал». Т.е. интеграционная поверхность строится как набор канальных адаптеров над одним ядром (аналог omp SDK/RPC + Hermes gateway), а не как CLI-флаги. Источник: local://vellum-summary.md.
2. **Хостинг: managed или self-hosted, один кодбейз.** «Managed runtime на Vellum Platform или self-hosted. Один кодбейз, одна модель данных» — дистрибуция решается не упаковкой бинарей, а двумя deployment-режимами одного рантайма. Источник: local://vellum-summary.md.
3. Косвенно релевантна безопасность: «Учётные данные живут в отдельном процессе и никогда не попадают в модель» — принцип, который надо соблюдать при дизайне install-id/секретов и headless-режимов (секреты не должны утекать в JSONL-стрим). Источник: local://vellum-summary.md.

## Решение (одно/комбо)

Комбо, за основу берётся модель omp (единая CLI с default-командой + один флаг `--mode text|json|rpc`), дополненная HTTP-поверхностью из Hermes и канальной идеей Vellum. Обоснование: (1) единая CLI-поверхность с маршрутизацией в default-команду даёт CLI-parity «из коробки» — headless не может разойтись с интерактивом, потому что это буквально один и тот же парсер и тот же сессионный код, только с другим output-режимом; это самый дешёвый в поддержке вариант (один парсер clap, одна поверхность доков). (2) RPC делаем NDJSON-over-stdio с ready-кадром и версионируемым протоколом (v1 без chunking, v2 с `rpc_chunk`) — это проверенный omp-контракт, дешёвый в Rust (линии stdio + serde) и не требующий HTTP-стека для локальных хостов; HTTP API (OpenAI-совместимый, по образцу Hermes `/v1/runs`) откладываем в отдельный крейт, чтобы не тащить axum в базовую поверхность. (3) SDK — это in-process Rust API в `titi-core` (builder-фасад `create_session()`), а не отдельный продукт: библиотека и CLI собираются из одного ядра, дублирования нет. (4) Упаковка: cargo-dist + Homebrew formula + curl-скрипт; подпись macOS — hardened runtime + notarization в CI с auto-skip по секретам, но без omp-энтитлайментов JIT (у чистого Rust-бинаря их нет) — только `disable-library-validation`, если появятся dlopen-плагины. (5) Collab в MVP не делаем; закладываем только E2E-модель (AES-GCM room key + write token) в deep-dive, чтобы транспорт не пришлось переделывать. install-id переносим один-в-один (UUID-файл `0o600`, O_EXCL, базовый config-root вне профилей) — это 50 строк и закрывает будущую телеметрию/совместимость.

## Rust-маппинг

**Крейты workspace:**

- `titi-cli` — парсер и маршрутизация: clap-derive `Cli { command: Option<Command>, mode: OutputMode, print: bool, ... }` с default-командой (динамическая диспетчеризация: нераспознанный первый аргумент → launch). `OutputMode { Text, Json, Rpc /*, Acp */ }`. Здесь же install-id CLI и подкоманды уровня `omp` (models, update, stats и т.д.).
- `titi-core` — ядро сессий: типы `Session`, `SessionManager`, `Settings`, `AgentRegistry`; здесь живёт **SDK-фасад**:
  ```rust
  pub struct SessionOptions { cwd: PathBuf, settings: Settings, tools: ToolSet, ... }
  pub async fn create_session(opts: SessionOptions) -> Result<(SessionHandle, ModelFallback)>;
  impl SessionHandle {
      pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent>;
      pub async fn prompt(&self, msg: impl Into<String>, q: QueuePolicy) -> Result<PromptAck>;
      pub async fn dispose(&self);
  }
  ```
- `titi-providers` — провайдеры; в тему попадает только то, что RPC/SDK-события (`message_update`, токены/с) агрегируются отсюда.
- `titi-tools` — реестр инструментов; host-tools из RPC-режима внедряются как обычные tool-фабрики:
  ```rust
  pub trait Tool: Send + Sync { fn name(&self) -> &str; async fn call(&self, args: Value) -> ToolResult; }
  pub fn register_host_tools(reg: &mut ToolRegistry, defs: Vec<HostToolDef>, cb: HostCallback);
  ```
- `titi-tui` — интерактивный рендер; для parity важно: TUI потребляет те же `SessionEvent`, что и json/rpc-режимы.
- **Новый `titi-rpc`** — NDJSON-фрейминг и протокол:
  ```rust
  pub struct ReadyFrame { protocol_version: u32, supported: Vec<u32>, max_frame_bytes: usize, max_reassembled: usize }
  pub enum RpcFrame { Response(...), Event(SessionEvent), UiRequest(...), HostToolCall(...), Chunk(ChunkMeta, Vec<u8>) }
  pub async fn serve_rpc(session: SessionHandle, input: impl AsyncRead, output: impl AsyncWrite) -> Result<()>;
  pub struct RpcFrameDecoder; // v2 reassembly с валидацией chunkId/index/count/byteLength
  ```
- **Новый `titi-collab`** (позже) — E2E-клиент: `RoomKey::from_link(&str) -> Result<(RoomId, Aes256Key, Option<WriteToken>)>`, sealing `aes-gcm`, WS-клиент/сервер релея.
- **Новый `titi-dist`** — xtask-крейт для сборки/подписи (`cargo xtask dist`, вызов `codesign`/`notarytool` через std::process, генерация `.pkg`).

**Ключевые решения:** install-id в `titi-core::dirs`:
```rust
pub fn install_id(base_config_root: &Path) -> InstallId; // OnceLock-кэш, O_EXCL create 0o600, EEXIST → перечитать
```
CLI parity обеспечивается контрактом: каждый `SessionEvent` имеет JSON-представление, TUI и `--mode json` потребляют один и тот же поток.

**Внешние крейты:** `clap` (CLI + completions), `serde`/`serde_json` (протокол), `tokio` (stdio async, broadcast-события), `uuid` (v4, feature `v4`), `aes-gcm` + `rand`/`rand_core` (collab E2E), `tokio-tungstenite` (collab relay, позже), `dirs` (config-root). Для упаковки — не крейты, а инструменты: `cargo-dist` (релизные артефакты, Homebrew-таск), shell-скрипты `codesign`/`notarytool`/`xcrun stapler` в CI (аналог `ci-macos-sign.sh`), `cargo-binstall`/`cargo install` как аналог `curl | sh` для разработчиков.

## Definition of Done

- [ ] `titi -p "prompt"` обрабатывает промпт, стримит результат в stdout и завершает процесс с кодом 0; `titi --mode json` эмитит NDJSON-события, потребляемые интеграционным тестом (минимум `message_update`, `agent_end` с `isTerminal`).
- [ ] `titi --mode rpc`: тест по stdio проверяет ready-кадр (поля `protocolVersion`, `supportedProtocolVersions`, `maxFrameBytes`), `negotiate_protocol` → v2, reassembly oversized-кадра из `rpc_chunk` и отклонение битой последовательности (`interleaved`).
- [ ] Тест гонки install-id: два процесса одновременно зовут `install_id()` ровно один создаёт `~/.titi/install-id` с mode `0o600`, оба получают одинаковый UUID; мусорный файл предварительно удаляется.
- [ ] CLI parity зафиксирована golden-тестом: список подкоманд и флагов, доступных в `--help`, совпадает с эталонным файлом; headless-режим (`-p`) принимает любой флаг launch-поверхности.
- [ ] Host-tools: интеграционный тест регистрирует `set_host_tools`, агент вызывает `host_tool_call`, хост отвечает `host_tool_result`, ответ попадает в модель; `isError: true` поверхяется как ошибка тула.
- [ ] macOS-джоб в CI собирает arm64-бинарь, подписывает (`--options runtime --timestamp`), нотаризует при наличии секретов и проверяет `codesign --verify --strict` + smoke-запуск; без секретов — ad-hoc и зелёный пайплайн (auto-skip).
- [ ] Документация user-facing пакетов: каждый крейт с пользовательским CLI имеет корневой doc-файл по политике include/exclude (аналог omp://user-facing-packages.md).
- [ ] Collab deep-dive: спека ссылок (48-байтный full / 32-байтный view-only) и запрет промпта для view-only покрыты юнит-тестами парсера и тестом токена (без сети).

## Deep-dive

План подсистем-доков (пишутся отдельными задачами, сейчас — план):

- `docs/research/packaging-headless/cli-surface-parity.md` — полная спецификация CLI: таблица подкоманд/флагов, правила маршрутизации default-команды, `@file`-attach, `--`-экранирование, golden-схема parity-теста.
- `docs/research/packaging-headless/rpc-protocol.md` — NDJSON-контракт titi-rpc: ready-кадр, версионирование, v2-чанкинг, каталог команд, host-tools/host-URI субпротоколы, модель ошибок и paged messages.
- `docs/research/packaging-headless/sdk-embed.md` — in-process фасад `create_session()`: discovery-дефолты, приоритет резолва ключей, жизненный цикл dispose, мульти-сессии через AgentRegistry.
- `docs/research/packaging-headless/macos-packaging.md` — подпись/notarization/стейплинг для Rust: чем отличается от Bun-бинаря (нет JIT-энтитлайментов), .pkg/.dmg для quarantine-каналов, cargo-dist + CI-секреты.
- `docs/research/packaging-headless/install-id.md` — хранение/гонки/потребители install-id в titi.
- `docs/research/packaging-headless/collab-relay.md` — E2E-модель collab: формат ссылок, AES-256-GCM кадры, hub-топология, минимальный content-blind relay.
- `docs/research/packaging-headless/dist-channels.md` — каналы дистрибуции: curl-скрипт, Homebrew, cargo-binstall; что требует notarization, а что нет.
