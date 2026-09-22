# STATE — точка возобновления конвейера titi

## Активное направление: reference product functional port

Статус: **0.1.0 работает** (чат на ratatui, headless, инструменты, сессии, Genome, память, SOUL). Встроенный каталог: OpenAI, OpenRouter, OpenCode, Anthropic; пользовательский конфиг накладывается по id, а не заменяет список. Канонический handoff: [`reference-product-port/STATE.md`](reference-product-port/STATE.md). Решения: [`reference-product-port/DECISIONS.md`](reference-product-port/DECISIONS.md). Roadmap: [`reference-product-port/README.md`](reference-product-port/README.md). Goal loop поверх reviewer-а в master (`/goal`, 2026-09-23). Следующий шаг — ручная PTY-проверка `/goal` с живой моделью. Не начинать GPUI. Tree-sitter — замена эвристик Genome, не новый граф: symbol-level уже есть.

Правило продолжения: сначала прочитать dedicated STATE, прогнать baseline, выполнить только NEXT, затем синхронизировать оба STATE-файла.

Обновляется после каждого шага. Новая сессия начинает отсюда.

## Goal loop и приватность (2026-09-23)

| Шаг | Коммит | Статус |
|-----|--------|--------|
| Goal loop: coder ↔ свежий reviewer, лимит 8 раундов, осцилляция = повтор патча, отмена паркуется | 324c6eb, 13b19d0 | done, 11 тестов |
| `AGENTS.md` проекта и метаданные скиллов в системном промпте | 09ab253, 184aace | done |
| `AGENTS.md` читается только изнутри проекта (симлинк наружу отклоняется) | cbef3b6 | done, +3 теста |
| Имя и описание скилла проходят сканер инъекций, описание ≤ 1024 байт | 75a7c6c | done, +2 теста |
| `/goal` из чата, виден в списке после `/` | b8f6619, e7cc9c6 | done |
| Алиасы ролей моделей (`modelRoles`) | b2041ce | done |
| `read`/`edit` отказывают для `.env*`, ключей, `.ssh`, cookie-баз; `grep` их пропускает; `grep`/`glob` не ходят по симлинкам | db53389 | done, +7 тестов |
| Вывод инструментов маскируется (секреты + IPv4, loopback остаётся) до модели, транскрипта и сессии | dee6960 | done, +4 теста |
| Тесты checkpoint больше не коммитят в сам checkout (отсюда три `titi checkpoint` в master от 2026-09-21) | aa1f72c | done, +1 тест |

Перенос из ветки `goal-loop`: только код, упоминания старого порта убраны.

## Артефакты Research Map — все done (2026-08-27)

README карта + граф, PLAN.md, CONVEYOR.md (с цепочкой воркеров), scripts/check-research.sh (exit 0), 20 тем-доков + 9 подсистем.

## Реализация (тодо-конвейер 68 задач)

| Узел | Статус | Примечание |
|------|--------|------------|
| M0: titi-config (слои, merge, карантин, get/set/reset) | done | 8/8 тестов |
| M0: titi-secrets (dotenv-слои + auth.db SQLite, 0600) | done | 12/12 тестов |
| M1: session JSONL+FTS (titi-core::session) | done | 13/13 тестов |
| M4: titi-tui (width/history/viewport) | done | 30/30 тестов |
| Волна 1 интеграция | done | cargo test --workspace: 63 passed |
| Волна 2: trajectory, compaction, system-prompt-soul, provider-каркас | done | в `titi-core` / `titi-engine` / `titi-soul`; старая пометка `todo` отстала |
| Волна 2 TUI: Component/overlays/keybindings/theme/kitty/resize | done | см. волну 3 TUI ниже |
| Fallback-пул titi (glm-5.3-flash + deepseek-flash, лимиты) | todo | фаза Providers |

## omp-ротация моделей (текущая сессия)

fallbackChains настроены: default = opencode-go/glm-5.3-flash → clinepass/glm-5.3 → opencode-go/deepseek-v4-flash → clinepass/deepseek-v4-flash → bai/glm-5.3-flash; revert cooldown-expiry; smol = bai/glm-5.3-flash → bai/qwen3.8-flash; task = clinepass/glm-5.3 → deepseek-v4-pro → qwen3.8-max.

## Волна 3 TUI (движок titi-tui) — 5/6

| Подсистема | Коммит | Статус |
|------------|--------|--------|
| 1. Ввод + keybindings (input.rs, keys.rs) | 8cd4138 | done |
| 2. Тема (theme/) | 837226a | done |
| 3. Kitty graphics (caps.rs, image.rs, input.rs) | 8cd4138 | done, 18+10+15 тестов |
| 4. Renderer/history/ack/resize (renderer.rs) | f416be6 | done, 16 тестов |
| 5. Agent UX: composer.rs, slash.rs, status.rs, panels.rs | d899189 | done, +62 теста |
| 6. Agent UX: overlay panels (SelectionPanel, ApprovalPanel, SessionSwitcher) + slash snapshot
| 7. Transcript markdown renderer + section visibility model + golden-тесты | b9d3b53 | done, +31 тест | | 3ad727b | done, +20 тестов |
| 8. mouse Off preset + /mouse parse | 17df97a | done, +2 теста |
| 9. Первый кадр в titi-cli: banner + status line до ready провайдера, queue ввода во время init, time-to-first-frame < 150ms | 0742f0a | done, +2 интеграционных теста (mock provider, 2s delay) |
| 10. Transcript в titi-cli: компонент Transcript (accordion-секции, /details, floating-alert backstop) | f55896f | done, +6 unit (titi-tui) + 4 интеграционных (titi-cli) |
| 11. Mouse drag-select: Selection model, SGR-mouse decoding (Drag/ScrollUp/Down), selection background, SGR-mouse integration test, /mouse + --mouse пресеты, персист display.mouse_tracking | 86255c0 | done, +9 unit (selection) + 3 input (Drag/Scroll) + 3 интеграционных (mouse_selection) + 3 App/config (titi-cli) |
| 12. Bracketed paste + overlay-панели (DoD): `Event::Paste` — вставка одним блоком, inline-collapse >6 строк, `[Image #N]` аттачменты; `Ctrl+X` session switcher, `/model`+`Ctrl+M` model picker, approval-gated close сессии (удаление JSONL только по Yes), Esc везде cancel-без-удаления | f4f4836 | done, +1 unit (composer, счётчик аттачментов) + 10 интеграционных (overlays_paste) |

titi-cli: FirstFrame core (banner, StatusLine Starting→Ready, очередь ввода, замер ttff) + App (FirstFrame + Transcript + theme) + бинарник на crossterm (raw mode, alternate screen, event loop). Transcript: секции thinking/tools expanded, subagents collapsed, activity hidden; `/details <section> <mode>`; backstop-алерт при all_hidden. Реализовано на std threads, БЕЗ tokio/ratatui — хватило crossterm.
titi-tui: 392 тестов (374 unit + 18 integration); titi-cli: 35 интеграционных; workspace: 603. Сборка 0 warnings; новые файлы clippy-clean.

## Slash DoD (2026-08-28)

- [x] Builtin names reserved (help, details, model, sessions, mouse)
- [x] `/unknown-xyz` → Passthrough (LLM as text)
- [x] `$1`/`$ARGUMENTS` expand in file templates (unit-tested in slash.rs)
- [x] Floating autocomplete panel (CompletionPanel, 6 unit + 11 integration tests)
- [x] Tab inserts highlighted command name, Esc hides, arrows navigate
- [x] Enter dispatches via Route::Builtin / Expanded / Passthrough
- [x] Snapshot test for route + complete (slash_snapshot.rs, 2 passed)
- [x] TopCenter anchor added to overlay::Anchor for floating panel placement
- [x] PTY-smoke: `/m` → panel shows `/model  Switch the active model`, Enter → model picker opens

3. ✅ Slash-команды (DoD): реестр `SlashRegistry` (builtin резерв, file-команды с `$1`/`$@`/`$ARGUMENTS`, Passthrough для `/unknown`); floating `CompletionPanel` (non-modal: Tab вставляет имя, Esc прячет, стрелки двигают подсветку); Enter диспатчит через `Route` (Builtin: help/details/model/sessions/mouse; Expanded: submit развёрнутого шаблона; Passthrough: submit как текст). Снапшот-тест route+complete; 6 unit (панель) + 11 интеграционных (titi-cli). `Anchor::TopCenter` добавлен для floating-размещения.
4. ✅ Очередь (DoD): `App::push_queued`/`stream_queue_len`/`pull_last_queued`/`queue_highlighted`/`clear_highlight` поверх `Composer::queue`; `Alt+Up` (crossterm `KeyCode::Up` + `KeyModifiers::ALT`) вытаскивает последнее сообщение в редактор с подсветкой (inverse video), Esc снимает подсветку не удаляя текст; LIFO-порядок; 3 unit (composer) + 5 интеграционных (queue.rs). PTY-smoke: Alt+Up/Esc на пустой очереди — no-op, без паники. Коммит `376d1dd`.
5. ✅ UI-фикс frame-anchor (`3ce63d4`): `render()` делал `Clear(All)` без `MoveTo(0,0)` — после первого кадра курсор оставался внизу, каждый ре-рендер рисовал кадр с низа экрана (UI «плавал»/дрейфовал по вертикали при наборе и resize). Комментарий «Move cursor to top» был, самой команды — не было никогда. Фикс: `execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))`. Доказано PTY: старый бинарь `\x1b[2J` без cursor-home, новый `\x1b[2J\x1b[1;1H`; кадр стабилен при keystrokes и resize 60→100→50. Workspace 603 passed.
6. PTY-smoke: full terminal test — kitty+resize+input, drag-select selection background, `/mouse` switching.
7. Каждый шаг обновляет этот файл — точка возобновления.


## OMP TUI 1:1 chrome (2026-08-29)

Не Hermes. Слот dark = `titanium`. Продуктовый статус — default preset:

Left: `pi > model > path > git` (mode/collab/pr/context/cost скрыты пока пустые)
Right: `session_name`
Separator: `powerline-thin` (`>` в unicode)

Composer shape **box** (OMP default): статус в верхней `boxRound` границе `╭── … ──╮`, промпт слит с нижней `╰─ {input}{CURSOR_MARKER} ─╯`. Slash complete — inner rows внутри бокса, не TopCenter на баннере. Модалки BottomCenter.

CLI больше не делает `Clear(All)`: `Renderer::draw` + `extract_cursor` паркует курсор на маркере.

`app.*`: followUp Ctrl+Q/Ctrl+Enter, dequeue Alt+Up/Shift+Up, model.select Alt+M, session.switch Ctrl+X, `/pause` `/hotkeys` `/help` `/switch`, `/mouse on|toggle`. Plan Alt+Shift+P, hub Alt+A, live Ctrl+L, history Ctrl+R, retry Alt+R, display.reset Alt+L, external editor Ctrl+G, clipboard copy/paste.

Slash: `$1`/`$2`/`$@`/`$ARGUMENTS`/`$@[start:length]`; capability catalog with `_shadowed` (native 100 > omp-plugins 90 > claude 80 …). OSC 5522 ingest on paste (image/* → `[Image #N]`). Git porcelain `*N +N ?N` cached on HEAD/index mtime. PTY 80×20: box composer, slash inner-rows, Ctrl+X Sessions, `/model` Enter → picker.

Type-to-filter: `SelectionPanel` fuzzy (substring then subsequence); overlay printable keys + Backspace reach the picker. CLI `paint` offers `FramePlan.history` for transcript overflow and acks after `Renderer::draw`. Stub `app.*` chords now toggle plan/live badges, open hub/history overlays, OSC 52 copy, `$VISUAL`/`$EDITOR`, retry last prompt, `reset_display`.

Compact model picker: `SelectList` window (~40% of terminal, title inset in `boxRound` top border) sits above the composer so 80×20 keeps `Model`. Type-to-filter still fuzzy + Enter.

Renderer 1:1: `draw` serializes history remainder + viewport in one write; rebuild/reset fold ED2+ED3 into that write (no mid-frame flush). CLI resize default is `Rebuild` (`PI_TUI_RESIZE_SCROLLBACK` override). Last-resort `truncate_to_width` + SGR close on every drawn row. `detectColorMode`: after WT_SESSION/COLORTERM, `TERM ∈ {dumb, linux, ""}` is 256-color.

Custom themes: `{agentDir}/themes` (`$TITI_AGENT_DIR` else `$PI_CODING_AGENT_DIR` else `~/.titi/agent/themes`, named profile under `~/.titi/profiles/<name>/agent/themes`). Built-in names still win.

Appearance: OSC 11 BT.601 luma (`< 0.5` → dark) → `COLORFGBG` (`bg < 8` → dark) → macOS `defaults` on Zellij-darwin only → dark. Auto-dark = `titanium`, auto-light = `light`. First paint uses COLORFGBG (`init_auto`); live OSC 11 / Mode 2031 go through `classify_appearance_bytes` → `App::ingest_probe_reply` (2031 = re-query, not luma). CLI enables Mode 2031, queries OSC 11 at start, and re-queries on FocusGained. Crossterm still drops raw OSC from `Event::read`.

STT: `app.stt.toggle` has empty default keys (hold Space is the gesture). Space-hold matches omp (`SPACE_REPEAT_MAX_GAP_MS=120`, jitter 18ms/0.35, mechanical run 2, release 250ms). `stt.enabled` defaults false; mic/ASR worker is stubbed (`Idle`/`Recording`). CLI polls the release timeout on the 50ms tick.

Ещё не 1:1: OSC 5522 as a first-class terminal event (crossterm only exposes bracketed paste); live Agent Hub broker IPC (roster overlay exists; inject via `App::set_hub_peers`); Kitty Unicode placeholders; `ctx.ui.custom` mounts; real STT mic/ASR.
