# GUI поверх общего ядра (gpui)

> Сравнительный research 2026-08. Продуктовые решения — этот README и [карта тем](../README.md). Действующий контракт: `EngineCommand`/`EngineEvent` в `titi-engine`. Crate desktop — `titi-desktop`, не `titi-gpui`. ANSI `Component` не поднимается в ядро.

Как наложить нативный GUI (gpui от Zed) на тот же engine-контракт, который обслуживает TUI и headless. TUI, headless и GPUI — равноправные surfaces; GPUI начинается только после рабочего TUI/headless пути.

## omp

Контракт UI в omp уже разделён на два уровня — рендер-движок и интеграционный слой, что прямо пригодится для GUI:

1. **Контракт `Component` терминально-специфичен и это честно задокументировано.** `packages/tui/src/tui.ts` определяет `Component { render(width: number): readonly string[]; handleInput?(data: string): void; dispose?(): void }` — компонент возвращает готовые ANSI-строки фиксированной ширины, ссылочное равенство массива включает мемоизацию контейнеров, курсор передаётся через `CURSOR_MARKER` в отрендеренном тексте, фокус — отдельный трейт `Focusable` (источник: omp://tui).
2. **Интеграционный слой решает, куда и как монтировать UI.** `ExtensionUIContext.custom(factory, { overlay })` в `packages/coding-agent/src/modes/controllers/extension-ui-controller.ts` монтирует компонент вместо редактора или как оверлей, вызывает `dispose()`, резолвит промис через обязательный `done(result)`; в headless/RPC-режиме UI-контекст — no-op, и код обязан проверять `ctx.hasUI` (источник: omp://tui). То есть «нет UI» — уже валидный режим ядра, а не аварийная ветка.

Ближайший аналог для GUI: рендер-движок (`packages/tui`) — заменяемая деталь; контрактом верхнего уровня являются не ANSI-строки, а точки монтирования (`custom`, `renderCall`/`renderResult`, оверлеи) и режим `hasUI=false`. GUI-адаптер должен реализовать те же точки монтирования, а не эмулировать `render(width)`.

## Hermes

Hermes решает задачу «много фронтендов над одним ядром» архитектурно, через entry points:

1. **Ядро одно, точек входа пять.** Диаграмма System Overview в docs/developer-guide/architecture: `CLI (cli.py)`, `Gateway (gateway/run.py)`, `ACP (acp_adapter/)`, `Batch Runner`, `API Server` + Python library — все сходятся в `AIAgent (run_agent.py)`; презентационный код (`agent/display.py`, `hermes_cli/skin_engine.py` — спиннеры, темизация) живёт в CLI-слое, а не в ядре (источник: https://hermes-agent.nousresearch.com/docs/developer-guide/architecture).
2. **ACP-адаптер — готовый паттерн UI-адаптера над ядром.** `acp_adapter/` — ACP-сервер, подключающий то же ядро к VS Code, Zed и JetBrains: редактор получает поток событий агента через протокол, ядро не знает о хосте (источник: https://hermes-agent.nousresearch.com/docs/developer-guide/architecture, раздел Directory Structure: `acp_adapter/ # ACP server (VS Code / Zed / JetBrains)`).
3. **Desktop-приложение распространяется одним инсталлятором с CLI.** «Windows or macOS: download the Hermes Desktop installer… for a command-line only install without Hermes Desktop, run install.sh» — CLI и десктоп — два сурфейса одной сборки, не два продукта (источник: https://hermes-agent.nousresearch.com/docs/).

## Vellum

1. **Канальная архитектура: «Один ассистент, одна память, каждый канал»** — macOS, iOS, Web, Voice, Email, Telegram, Slack, Twilio перечислены как каналы одного ядра; GUI-канал (macOS/iOS/Web) не выделен в отдельный продукт, а является ещё одним адаптером поверх общей памяти и модели данных (источник: local://vellum-summary.md, раздел «Каналы»).
2. **Один кодбейз и одна модель данных на все сурфейсы.** «Managed runtime на Vellum Platform или self-hosted. Один кодбейз, одна модель данных» — состояние агента не дублируется per-канал, канал — только транспорт и рендер (источник: local://vellum-summary.md, раздел «Хостинг»).

## Решение (одно/комбо)

Сравнение кандидатов для нативного GUI-сурфейса titi:

| Критерий | gpui (Zed) | egui (+eframe) | iced |
| --- | --- | --- | --- |
| Модель | Гибрид immediate+retained, GPU-рендер, flexbox-подобный `div()` DSL | Чистый immediate mode, каждый кадр перерисовка | Elm-архитектура `update(Message)`/`view()`, wgpu |
| Вес/зрелость | ~72K SLoC, 39K загрузок/мес, 107 зависимых крейтов (lib.rs); Zed 1.0 на нём в проде | ~18 MB бинарь (hello-world), зрелый, но «интерфейсы в движении» | ~17 MB бинарь, 0.x, но на нём COSMIC от System76 |
| Licensing | Apache-2.0 | MIT OR Apache-2.0 | MIT |
| Async | Собственный executor (foreground + background), `cx.spawn`/`AsyncApp` встроены | Нет встроенного; токи привинчиваешь сам (tokio-поллинг вокруг eframe) | Встроенный: `Command`/`Subscription` поверх `iced_futures`, runtime-агностик |
| Текст/IME/a11y | IME работает (в т.ч. Windows), a11y слабая; базового text input нет — пример на 700 строк | Accessibility (Windows Narrator) работает, но IME на Windows ломается | IME на Windows не активируется, a11y нет |
| Платформы | macOS/Linux первичны; Zed сам без Windows, но gpui на Windows работает | Windows/macOS/Linux/Web | Windows/macOS/Linux (+экспериментальный web) |
| Экосистема 2026 | awesome-gpui: Waku 1.3k★, zedis 2k★, gpui-component 13.6k★, helix-gpui, tty7, официальный `create-gpui-app` | Крупнейшая社区 immediate-mode, kittest для тестов | COSMIC-экосистема |

Источники: https://gpui.rs/, https://lib.rs/crates/gpui, https://github.com/zed-industries/awesome-gpui, https://www.boringcactus.com/2025/04/13/2025-survey-of-rust-gui-libraries.html (секция GPUI: отсутствие text input, IME, a11y), http://lukaskalbertodt.github.io/2023/02/03/tauri-iced-egui-performance-comparison.html (startup/binary/input-lag), https://linuxiac.com/zed-code-editor-hits-1-0-with-gpu-accelerated-ui/.

**Выбор: gpui как GUI-адаптер над `titi-engine`.** Ядро не знает ни о crossterm, ни о gpui: surfaces шлют `EngineCommand` и читают `EngineEvent`. `titi-tui` и будущий `titi-desktop` — равноправные адаптеры. TUI-специфичный контракт `Component` остаётся внутренним делом `titi-tui`. Исторический черновик `UiEvent`/`titi-gpui` ниже — reference, не действующий план.

## Rust-маппинг

Workspace: `titi-engine`, `titi-providers`, `titi-tools`, `titi-tui`, `titi-cli` + будущий `titi-desktop`.

**titi-core — без UI-зависимостей** (никаких crossterm/gpui в Cargo.toml):

```rust
// crates/titi-core/src/ui.rs — контракт, который реализуют titi-tui и titi-gpui
pub enum UiEvent {
    MessageAppended { item: HistoryItem },
    StreamDelta { id: ItemId, text: String },
    ToolStarted { id: ItemId, name: SmolStr },
    ToolFinished { id: ItemId, result: ToolResultView },
    ViewportChanged { anchor: ScrollAnchor },
}

pub trait UiSurface: Send {
    fn push_event(&mut self, ev: UiEvent) -> anyhow::Result<()>;
    fn is_interactive(&self) -> bool;          // аналог omp ctx.hasUI
    fn on_command(&mut self, cmd: UserCommand) -> anyhow::Result<()>;
}

// headless-реализация — no-op, как у omp в RPC-режиме
pub struct HeadlessSurface;
impl UiSurface for HeadlessSurface { /* push_event: Ok(()), is_interactive: false */ }
```

**titi-gpui** — адаптер: gpui-типы `App`, `Window`, `Context<T>`, трейт `Render`, сущности `Entity<T>`; внешний крейт — gpui (crates.io, Apache-2.0) + gpui-component (таблицы, доки, text editor):

```rust
// crates/titi-gpui/src/session_view.rs
pub struct SessionView {
    core: CoreHandle,        // bridge: mpsc::Receiver<UiEvent> + ручка команд
    items: Vec<HistoryItem>,
}

impl Render for SessionView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // uniform_list по self.items — прямой аналог viewport'а TUI
        uniform_list(cx.entity().clone(), "history", self.items.len(), ...)
    }
}
```

Мост tokio → gpui-executor: `CoreHandle` держит `tokio::sync::mpsc::Receiver<UiEvent>`; gpui-сторона через `cx.spawn(async move |this, cx| { while let Some(ev) = rx.recv().await { this.update(cx, |v, cx| v.apply(ev, cx))?; cx.notify(); } })`. titi-tui остаётся как есть — его `Component` сужается до внутренней детали TUI; titi-cli собирает сурфейс по флагу (`--tui` по умолчанию, `--gui` → titi-gpui), headless-режим получает `HeadlessSurface` без единой UI-зависимости. Крейты: `tokio` (ядро), `gpui` + `gpui-component` (GUI), существующие `crossterm` (TUI).

## Definition of Done

- [ ] `cargo tree -p titi-core` не содержит gpui/crossterm/egui/iced; clippy-проверка `titi-core` на UI-зависимости проходит.
- [ ] `UiEvent`/`UiSurface`/`HeadlessSurface` определены в `titi-core/src/ui.rs`; headless-тест: запуск агента с `HeadlessSurface` не паникует и не пишет в терминал (тест в `titi-core`).
- [ ] `titi-gpui` рендерит историю сессии из того же `HistoryItem`-потока, что и `titi-tui`: один и тот же fixture-тест JSONL-транскрипта даёт эквивалентный список `HistoryItem` в обоих адаптерах (golden-тест).
- [ ] `titi-cli --gui` открывает окно gpui со стримингом ответа модели (дельты появляются в реальном времени) и полем ввода; ручная проверка на macOS.
- [ ] `titi-cli` без флагов и `--tui` работают как раньше; регресс-тесты titi-tui зелёные.
- [ ] Команда прерывания (Ctrl+C в TUI / кнопка Stop в GUI) доставляет один и тот же `UserCommand::Interrupt` в ядро — тест на обеих реализациях `UiSurface`.
- [ ] Cargo.toml workspace обновлён: член `crates/titi-gpui`, workspace.dependencies для gpui.

## Deep-dive

Подсистемные доки (2-й уровень, писать по мере проработки; пока — план):

- `docs/research/gui-gpui/gpui-internals.md` — executor, Entity/Context, key dispatch, рендер-пайплайн gpui (по docs/contexts.md, key_dispatch.md из репозитория Zed).
- `docs/research/gui-gpui/event-bridge.md` — мост tokio ↔ gpui-executor, backpressure, threading-модель, отмена.
- `docs/research/gui-gpui/component-parity.md` — соответствие точек монтирования omp (`custom`, `renderCall`/`renderResult`, overlay) ↔ gpui-аналогов (модалки, доки, inline-рендер инструментов).
- `docs/research/gui-gpui/widgets-gpui-component.md` — аудит gpui-component: text editor, таблицы, док-лейаут; что докатывать самим.
- `docs/research/gui-gpui/a11y-ime-packaging.md` — доступность, IME, сборка .app/.deb, подпись.
