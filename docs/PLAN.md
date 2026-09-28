# PLAN — milestones titi

Порядок = топологический по графу зависимостей (research/README.md). Каждый milestone: темы-доки + фазы из todo-конвейера.

## M0 — Foundation
- Темы: config-settings, secrets-env
- Код: workspace (готов), `titi-core::config` (слоёная резолюция, deep-merge, карантин), secrets/env.
- DoD: cargo test зелёный на слои merge + get/set/reset.

## M1 — Core Runtime
- Темы: sessions-persistence, trajectory-gepa, compaction-context, system-prompt-soul, memory-learning.
- Код: UI-независимый `titi-engine`, `EngineCommand`/`EngineEvent`, bounded provider/tool loop, сессии + персистентность, trajectory, compaction, SOUL и memory.
- Порядок: engine protocol → provider registry/credentials → TUI vertical slice → tool loop → persistence/headless. GPUI начинается только после рабочего TUI/headless пути.
- DoD: реальный prompt стримится через engine; cancel работает; transient fallback доказан тестом; permanent errors не повторяются; сессия переживает рестарт.

## M2 — Providers
- Темы: providers-streaming, toolconv, model-switching.
- Код: `titi-engine::ProviderRegistry` (descriptor + transport + credential), wire transports в `titi-providers`, engine-owned retry/fallback до visible content, mid-session switch.
- DoD: registry tests на missing credential / unknown model / fallback между transports; стриминг-тест на mock; смена модели mid-session без потери контекста.

## M3 — Tools
- Темы: tools-core, tools-advanced-cua
- Код: реестр инструментов; read/edit/write (hashline), bash (PTY, portable-pty), glob/grep (ignore+globset), fs-scan cache, ast-edit (tree-sitter), eval/notebook, web_search/browser, CUA-драйвер локальный (xcap+enigo) с крюком на удалённые.
- DoD: каждый инструмент имеет поведенческий тест; CUA делает скриншот и клик на локальной машине.

## M4 — TUI Engine
- Темы: tui-renderer
- Код: crossterm; history-batch ack, viewport diffing, Component trait, overlays, width-модель UAX#11, resize-политики, kitty graphics.
- DoD: рендер-тесты на золотых файлах; нет мерцания при стриме (визуальная проверка).

## M5 — Agent UX
- Темы: agent-ux
- Код: композер (bracketed paste, non-blocking очередь), slash-команды + автодополнение, overlay-панели, transcript-рендер, статус-линия, session switcher.
- DoD: smoke-сессия в реальном терминале: ввод, стрим, прерывание, свитч модели.

## M6 — Extensibility
- Темы: extensibility-marketplace, mcp
- Код: extensions/hooks-загрузчик, skills (SKILL.md), rulebook matching, marketplace installer, MCP-клиент (transports stdio/HTTP).
- DoD: пример-skill подхватывается и работает; MCP-сервер подключается и его тулы доступны.

## M7 — Agents & Security
- Темы: agents-hub-security
- Код: task subagents + discovery, hub-каналы, checkpoint/rewind, todo, advisor watchdog; actor identity (guardian/trusted/unknown), sandbox на тул-вызовы, креды в отдельном процессе, default deny.
- DoD: subagent работает изолированно; unknown-актор не читает память.

## M8 — Bot Network
- Темы: bot-network-soul
- Код: пер-бот домашние папки, SOUL.md identity, bot-to-bot сообщения, обмен опытом (memory/skills), экспорт/импорт души, проактивный cron, каналы (Telegram первым).
- DoD: два бота обмениваются сообщениями и переносят скилл.

## M9 — GPUI Workbench + Packaging
- Темы: gui-gpui, packaging-headless.
- Код: `titi-desktop` на Zed GPUI поверх того же `EngineCommand`/`EngineEvent`; Work, Files, Changes, Genome, Tools, Costs, Settings, Help; RPC/headless, packaging/signing.
- Запрет: никаких provider/tool/session реализаций внутри desktop crate.
- DoD: TUI, headless и GPUI управляют одной engine-сессией; RPC-клиент управляет turn; .app собирается и подписывается.

## Спринты
Собираются позже из DoD тем: спринт = подмножество задач одного milestone, закрываемое за итерацию, с общим интеграционным прогоном в конце (runbook в CONVEYOR.md).
