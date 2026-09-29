# Модернизация UI: живой чат-экран titi

Статус: **исследование + список доработок, кода не меняли**. Реализация — по
пунктам из «Короткого списка» (§4), каждый пункт отдельным коммитом.

Тема — визуальный слой живого экрана (`./target/debug/titi`, обычный чат), а не
движок истории/viewport (он описан в [tui-renderer/](tui-renderer/README.md)) и
не slash-пайплайн (он описан в [agent-ux/README.md](README.md)).

Обозначения ниже:

- `pitui src/X` = `/Users/workie/.bun/install/global/node_modules/@oh-my-pi/pi-tui/src/X`
- `agent src/X` = `/Users/workie/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src/X`
- `chat.rs:N` = `crates/titi-cli/src/chat.rs:N`

Обе омп-ссылки — исходники omp на этой машине (единственный читаемый эталон;
инвентарь остальных TUI — §2.3).

## 1. Что titi рисует сегодня (кадры, а не мнения)

### 1.1 Стенд

Кадры сняты **с настоящего бинарника** `./target/debug/titi` через PTY и
эмулятор экрана: python `pty` +
`TIOCSWINSZ`, вывод прогоняется через `pyte.Screen(cols, rows)`, дальше
печатается `screen.display`. Это тот же способ, которым уже снят `QA_STATUS.md`
(там он назван «PTY … (pyte screen)»).

Оговорка про сборку: первый блок кадров снят на бинарнике 14:13, дальше
`target/debug/titi` был **пересобран другим потоком работы** (16:38; в дереве
правятся только `crates/titi-providers/src/{openai,wire}.rs`). `chat.rs` при
этом чист, а контрольный прогон на текущем бинарнике даёт те же кадры
(empty-state и середину хода), поэтому кадры ниже отражают текущий код экрана.

- размеры: **80×20** и **120×30**, `TERM=xterm-256color`, `COLORTERM=truecolor`;
- idle-кадр на «пустой» агентской папке (`TITI_AGENT_DIR=/tmp/titi-throwaway`) и
  idle-кадр на реальной `~/.titi/agent` (там в шапке видно модель);
- остальные кадры — на одноразовой `TITI_AGENT_DIR` с `config.yml`, который
  объявляет **локального провайдера**: `providers: [{id: mock,
  api: openai-completions, base_url: http://127.0.0.1:8798/v1,
  credential_required: false}]` и `models: [{id: mock/scripted, provider: mock,
  wire_model: scripted, context_window: 200000}]`. На `127.0.0.1:8798` поднят
  скриптовый OpenAI-совместимый SSE-сервер, который отдаёт нужную сцену
  (`read` — вызов тула `read`, затем длинный markdown-ответ; `edit` — тул `edit`;
  `write` — тул `write` под `--approval always-ask`; `text` — просто ответ).
  Так кадры хода/тула/approval — **настоящие кадры настоящего TUI**, без
  обращения к платному провайдеру и без записи в реальную сессию.
- креденшелы не печатались и не трогались; список провайдеров (`--list-keys`)
  использован только как факт «ключ есть», `--login` не запускался.

### 1.2 Пустой экран (idle), 80×20

```
 titi  ready  plan                               openai/gpt-4.1  000000008db2
   · mode: plan
…
╭──────────────────────────────────────────────────────────────────────────────╮
│ › ask titi…                                                                  │
│                                                                              │
╰────────────────── enter sends  ·  /model  ·  ctrl-c quits ───────────────────╯
```

(пустая агентская папка без ключей: каталог отдал `openai/gpt-4.1`; на реальной
папке в шапке стоит `openai-codex/gpt-5.5` — см. §1.3)

### 1.3 Пустой экран (idle), 120×30, реальная агентская папка

```
 titi  ready  plan                                                                 openai-codex/gpt-5.5  000000011984
   · mode: plan
…
╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│ › ask titi…                                                                                                          │
│                                                                                                                      │
╰────────────────────────────────────── enter sends  ·  /model  ·  ctrl-c quits ───────────────────────────────────────╯
```

Строка 1 — это вся «шапка»: ` titi` + состояние + режим + (модель, контекст %,
id сессии) справа. Строка 2 — первая строка транскрипта (заметка `· mode:
plan`). Между транскриптом и композером на idle **нет** статусной строки
(`work_row` возвращает `None`, `chat.rs:3967-3971`), поэтому пустой экран —
это ~14 пустых строк 80×20.

### 1.4 Экран-заставка (пустой транскрипт, agent-режим), 80×20 и 120×30

```
 titi  ready                                      mock/scripted  00000000e255
…
                                      titi
                             say what you want done
                enter  send      /model  switch      ctrl-c  quit
…
╭──────────────────────────────────────────────────────────────────────────────╮
│ › ask titi…                                                                  │
╰────────────────── enter sends  ·  /model  ·  ctrl-c quits ───────────────────╯
```

120×30 — то же самое, центрированное по 120 колонкам. Ни версии, ни cwd, ни
модели, ни подсказок; анимации нет; три строки текста (`empty_state`,
`chat.rs:4031-4052`). Стоит заметить: «заставка» показывается **только при
пустом транскрипте**, а в `--mode plan` транскрипт непустой (заметка про
режим), поэтому заставку там не видно вообще.

### 1.5 Композер: набор текста, 80×20

```
╭──────────────────────────────────────────────────────────────────────────────╮
│ › make the composer look modern▍                                             │
│                                                                              │
╰────────────────── enter sends  ·  /model  ·  ctrl-c quits ───────────────────╯
```

Каретка — глиф `▍` (`chat.rs:4561`), не настоящий курсор терминала. Ввод
однострочный: `fit_tail` показывает только хвост строки; высота бокса —
`Constraint::Length(4)` (`chat.rs:3767`), то есть многострочный ввод не
раскрывает бокс.

### 1.6 Список slash-команд (`/`) и `/help`, 80×20 и 120×30

```
 titi  ready  plan                         openai-codex/gpt-5.5  000000011984
   · mode: plan
 ▶ /checkpoint   record a rewind point
   /checkpoints  list rewind points
   /compact      fold the history now, optionally around a focus
   /context      what fills the context window
   /goal         run coder and reviewer until the goal passes
   /help         list these commands
   /keys         which providers have a key or a sign-in
   /usage        show token usage
   /login        sign in to a provider, or store a key
   /logout       forget a stored key
   /model        switch model
   /pause        hold input and stop the turn
   /memory       list, search, or forget memories
   … 32 more below
╭──────────────────────────────────────────────────────────────────────────────╮
│ › /▍                                                                         │
╰────────────────── enter sends  ·  /model  ·  ctrl-c quits ───────────────────╯
```

Список **без рамки и без заголовка**, прямо в транскрипте; подсветка выбора —
`▶` (`picker_panel`, `chat.rs:3696-3737`), обрезка — текстом `… 32 more below`.
`/help` печатает такой же плоский текст в транскрипт (120×30):

```
 titi  ready  plan                                                                 openai-codex/gpt-5.5  000000011984
   · /model  switch model
   · /pause  hold input and stop the turn
   · /memory  list, search, or forget memories
   · /advisor  a toolless second opinion on this conversation
…
   · /diagnose  a diagnostics block to paste into a bug report
```

`/settings` — тоже заметка в транскрипте, тем же глифом `·`:

```
   · models.0.context_window = 200000 (agent)
     models.0.id = "mock/scripted" (agent)
     providers.0.api = "openai-completions" (agent)
…
```

### 1.7 Пикер модели, 80×20 и 120×30

```
 titi  ready  plan                         openai-codex/gpt-5.5  000000011984
   · mode: plan
 models · 28
  ▾ openai-codex  15
 ▶ openai-codex/gpt-5.5  ·openai-codex  oauth  ✓ current
   openai-codex/gpt-5.6  ·openai-codex  oauth
…
   … 23 more below
╭──────────────────────────────────────────────────────────────────────────────╮
│ › ask titi…                                                                  │
╰──────────── ↑↓ move  ·  enter switches  ·  esc clears or closes ─────────────╯
```

Группы — строки-заголовки `▾ provider  N`, выбор — `▶` + жирный, текущая модель —
бейдж `✓ current`. Рамки/титула/скроллбара нет; список «занимает» строки
транскрипта и старую заставку (видно на 120×30: заставка осталась выше
списка).

### 1.8 Ход: статусная строка, тул, стрим, ответ (80×20)

Начало хода (`--mode plan`, длина ответа — на весь экран, поэтому транскрипт
уезжает вверх):

```
 titi  working  plan                       openai-codex/gpt-5.5  000000011984
   · mode: plan

  you  │ Use the read tool on docs/README.md, then answer in Russian with an
       │ H2 heading, a three-item bullet list, and a short fenced rust code
       │ block. Ten lines max.
…
 ⠙ waiting for the first token · 0.1s
╭──────────────────────────────────────────────────────────────────────────────╮
│ › steer this turn…                                                           │
╰─────────────────────── enter steers  ·  ctrl-c stops ────────────────────────╯
```

Через 0.5 с в шапке появляется процент контекста (`3%`), и правый блок
**сдвигается влево** — вот первая строка кадра до (`t=0.44s`) и первая строка
кадра после (`t=0.54s`), дословно; это джиттер шапки:

```
 titi  working  plan                   openai-codex/gpt-5.5  000000011984
 titi  working  plan                   openai-codex/gpt-5.5  3%  000000011984
```

Тул выполнился, пошёл стрим (обратите внимание: `##`, `-`, ```` ``` ````, `>` —
сырые символы, ни один markdown-элемент не отрисован):

````
 titi  working  plan                          mock/scripted  1%  00000000c462
   · mode: plan

  you  │ Use the read tool on docs/README.md, then answer with markdown.
   ▸ read
   ✓ # docs — index - `BRAIN.md` — project health, tech debt, risks (updated
     only via audit). - `AUDIT_PROMPT.md` / `SYNTHESI…

  titi │ ## Что в репозитории
       │
       │ Коротко: рабочее пространство из десяти crate-ов, один цикл, один
       │ сеанс.
       │
       │ - `titi-engine` — цикл хода и команды движка
       │ - `titi-tui` — рендерер и тема
       │ - `titi-cli` — бинарник, экран, клавиши
       │
       │ Пример из `docs/README.md`:
       │
       │ ```rust
       │ let plan = app.plan_frame(input, height);
       │ renderer.draw(plan)?;
       │ ```
       │
       │ > Правило: одна тема — одна папка в `docs/research/`.
       │
       │ Остальное — в `AGENTS.md`.

 ⠹ streaming · 3.1s · 335 chars
╭──────────────────────────────────────────────────────────────────────────────╮
│ › steer this turn…                                                           │
╰──────────────────── 1%  ·  enter steers  ·  ctrl-c stops ────────────────────╯
````

Конец хода (та же сцена, 120×30): состояние вернулось в `ready`, статус-строка
исчезла, но подпись композера по-прежнему несёт `1%`:

```
 titi  ready                                                                          mock/scripted  1%  00000000fe34
…
╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│ › ask titi…                                                                                                          │
╰─────────────────────────────────── 1%  ·  enter sends  ·  /model  ·  ctrl-c quits ───────────────────────────────────╯
```

### 1.9 Чип тула и его результат: `read` и `edit`

`read` (80×20 и 120×30) — две строки, «превью результата» одной плоской строкой
с обрезкой:

```
   ▸ read
   ✓ # docs — index - `BRAIN.md` — project health, tech debt, risks (updated
     only via audit). - `AUDIT_PROMPT.md` / `SYNTHESI…
```

`edit` (скретч-репозиторий в `/tmp`, `--approval yolo`, 120×30) — **изменённый
файл не показан вообще**, только слово:

```
 titi  ready                                                                          mock/scripted  0%  00000000d974
  you  │ add a doc comment to the add function
   ▸ edit
   ✓ edited

  titi │ Готово: изменил `src/lib.rs`.
```

### 1.10 Approval, 80×20 (и отказ)

```
 titi  needs you                              mock/scripted  0%  00000000dd18
  you  │ write the probe note
   ▸ write
 ⚠ needs you · write
╭──────────────────────────────────────────────────────────────────────────────╮
│ write   y allow    n refuse                                                  │
│                                                                              │
╰──────────────────────── 0%  ·  y allow  ·  n refuse ─────────────────────────╯
```

Промпт **подменяет содержимое композера**: на экране только имя тула и два
слова; аргументов (`notes/probe.txt`, содержимое), причины и origin нет.
После `n` — ошибка тем же чипом:

```
   ▸ write
   ✕ tool error  tool invocation denied
```

### 1.11 Очередь/steer во время стрима, 80×20

```
 titi  working                                mock/scripted  2%  00000000e255
       │ Остальное — в `AGENTS.md`.
  you  │ and also list the crates          ← поставлено в очередь, ждёт хода
  titi │ ## Что в репозитории
…
 ⠏ streaming · 2.0s · 197 chars
╭──────────────────────────────────────────────────────────────────────────────╮
│ › steer this turn…                                                           │
╰──────────────────── 2%  ·  enter steers  ·  ctrl-c stops ────────────────────╯
```

Поставленное в очередь сообщение выглядит как обычный блок `you`, без пометки
«ждёт»; `Alt+Up` вернул его в редактор, и после этого в шапке на одной строке
оказался мусор из тела очереди (`Что в`) — воспроизводилось один раз, отдельным
пунктом ниже не считаю, но это симптом: очередь не имеет собственного
представления.

### 1.12 Палитра, которая реально уходит в терминал (сырые SGR)

Из сырых байтов одного кадра хода (только `38;2;…`/`48;2;…`, счётчики):

```
fg:  255;64;84   (accent, 92 раза)     255;236;234 (text, 76)   132;90;96 (dim, 8)
     92;42;50    (border, 7)           196;150;154 (muted, 3)   255;176;176 (gold, 1)
bg:  18;8;10     (page, 156)           36;16;20    (card, 31)
```

Это ровно `Ink::titanium()` (`chat.rs:3829-3858`) — «красная» палитра,
зашитая в живой экран. Тема (`titi-tui`, 60 fg-токенов, `crates/titi-tui/src/theme/schema.rs:21-82`)
на этом экране не участвует: в `chat.rs` **нет ни одного упоминания `Theme`**
(`grep -n "Theme" crates/titi-cli/src/chat.rs` → пусто), а `crates/titi-tui/src/theme/builtin.rs:9-11`
объявляет 100 тем (dark/light + 98 в `themes/defaults/`), включая `titanium.json`
с акцентом `#00b4ff`.

### 1.13 Что снять не удалось

- **Severity/thinking-кадр**: скриптовый сервер не отдавал reasoning-дельты —
  фаза `WorkPhase::Thinking` в кадре не поймана (в коде она есть, `chat.rs:4000-4008`).
- **Многострочный ввод в композере**: ввод остаётся однострочным (см. §1.5),
  отдельного кадра «бокс вырос» не существует by design.
- **Ctrl+X (session switcher) на живом экране**: нажатие не даёт видимого
  эффекта ни в 80×20, ни в 120×30; в `chat.rs` нет ни одного упоминания
  `app.session.switch` (диспатч этих чордов живёт только в `crates/titi-cli/src/app.rs:1423`).
  Кадр пикера сессий поэтому не снят.

## 2. Эталоны

### 2.1 omp `pitui` (движок): палитра, рамки, отступы, движение

**Палитра — семантические токены, а не цвета в коде.** `pitui theme/schema.ts:5-23`
задаёт `ThemeJson`, `pitui theme/schema.ts:26-85` — union `ThemeColor` (~60
токенов: `accent`, `border`, `borderAccent`, `borderMuted`, `success`, `error`,
`warning`, `muted`, `dim`, `thinkingText`, `toolTitle`, `toolOutput`,
`mdHeading`, `mdLink`, `mdCode`, `mdCodeBlock`, `mdCodeBlockBorder`, `mdQuote`,
`mdQuoteBorder`, `mdHr`, `mdListBullet`, `toolDiffAdded/Removed/Context`,
`syntax*`, `thinkingOff…Max`, `bashMode`, `pythonMode`, `statusLine*`), плюс
фоновые токены `pitui theme/schema.ts:132-140` (`selectedBg`, `userMessageBg`,
`customMessageBg`, `toolPendingBg`, `toolSuccessBg`, `toolErrorBg`, `statusLineBg`).
Значения — в JSON (`pitui theme/dark.json:5-88`), например:

```
"blue": "#178fb9",       "gray": "#777d88",        "accent": "#febc38",
"selectedBg": "#31363f", "toolPendingBg": "#1d2129",
"toolSuccessBg": "#161a1f","toolErrorBg": "#291d1d",
"statusLineBg": "#121212","statusLineModel": "#d787af", "statusLinePath": "#00afaf",
"mdCode": "#e5c1ff",     "mdCodeBlock": "#9CDCFE", "mdHeading": "#febc38",
"toolDiffAdded": "green" (#89d281), "toolDiffRemoved": "red" (#fc3a4b),
```

то есть бордеры (`border → blue`), подсветка выбора (`selectedBg`), три
фона тулов по статусу, отдельная палитра markdown/диффов и отдельные цвета
сегментов статусной строки. Тот же набор токенов уже есть у titi
(`crates/titi-tui/src/theme/schema.rs:21-95`) — он просто не подключён к
живому экрану.

**Рамки.** Глифы: `pitui theme/symbols.ts:420-439` — `boxRound` (`╭╮╰╯─│`) и
`boxSharp` (`┌┐└┘┼┬┴├┤`); скруглённые углы используют острые тройники, потому
что круглых юникод-вариантов нет (`pitui theme/theme-class.ts:523-539`).
Титул врезается **в саму верхнюю линию**: `pitui chrome/overlay-box.ts:18-30` —
`box.topLeft + horizontal` + `bold(fg(color,"accent", " title "))` + fill +
`topRight`; отступ содержимого — ровно одна колонка с каждой стороны
(`pitui chrome/overlay-box.ts:32-40`). Общий `Box` по умолчанию
`paddingX = 1, paddingY = 1` (`pitui components/box.ts:56-60`).

**Отступы.** Внутри рамки — один пробел у вертикали; вертикальные пустые
строки добавляет только `Box.paddingY` (`pitui components/box.ts:188-205`),
никаких «глобальных» пустых строк между блоками движок не вставляет. То есть
воздух задаётся явными `Spacer`/padding, а не «на глаз».

**Композер (box-стиль).** `pitui components/composer/box.ts:1-9`:

> The classic omp composer: rounded frame, status line embedded in the top
> border, and the last content row merged into the bottom border (`╰─ text … ─╯`),
> keeping a one-line prompt at two rows total.

Плюс: статус в верхней рамке (`statusAttachment: "top-border"`), `paddingX` по
умолчанию 2, скроллбар ввода **заменяет правую вертикаль** (`█` в диапазоне
большого пальца, `│` вне его, `pitui components/composer/box.ts:92-95`).

**Курсор.** В рендер вставляется нулевой ширины маркер (`CURSOR_MARKER`), по
которому TUI ставит аппаратный курсор, а рядом — видимая клетка курсора
(`pitui components/input.ts:525-529`, `pitui components/editor.ts:1355-1368`);
`pitui theme/tui-adapters.ts:172-180` — `inputCursor: preset === "ascii" ? "|" : "▏"`.

**Спиннеры.** Два независимых набора кадров: `status: ["⣾","⣽","⣻","⢿","⡿","⣟","⣯","⣷"]`,
`activity: ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]` (`pitui theme/symbols.ts:1469-1478`);
шаг 80 мс (`pitui components/loader.ts:5-7` — `SPINNER_ADVANCE_MS = 80`), рендер
30 fps; кадр берётся из `Math.floor(nowMs / SPINNER_ADVANCE_MS)`
(`pitui status-line/segments.ts:192-195`).

**Скроллбар.** `pitui components/scroll-view.ts:18-30` — `auto|always|never`,
`auto` занимает колонку только при переполнении; в строке рисуется `thumb` или
`track` (`pitui components/scroll-view.ts:488-496`), по умолчанию это `│` и `█`
(`pitui components/scroll-view.ts:15-16`), цвета задаёт вызывающий
(`track → muted`, `thumb → accent`).

**Выбор в списках.** `pitui components/select-list.ts:452-454` — курсор
`❯` (или `>` в ascii-пресете, `pitui theme/symbols.ts:403`); у невыбранных
строк место курсора заполнено пробелами; выбранная строка целиком
перекрашивается `selectedText`, а не только глиф
(`pitui components/select-list.ts:389-432`); наведение мыши — отдельный
`hovered`-стиль (`pitui components/select-list.ts:52,357-364`).

**Диффы.** `pitui chrome/diff.ts:104-113`:

> Context lines: dim/gray; Removed lines: red, with inverse on changed tokens;
> Added lines: green, with inverse on changed tokens.

Гуттер номеров строк фиксирован на 3 цифры, чтобы потоковый рендер совпадал
байт-в-байт с финальным (`pitui chrome/diff.ts:117-123`), внутристрочные
изменения — через `inverse` (`pitui chrome/diff.ts:59-99`). Дифф подключается в
чип тула (`pitui chat/tool-execution.ts:1330-1332`).

**Markdown.** Есть полноценный рендерер `pitui components/markdown.ts` (3804 строки,
`marked`): H1 = подчёркнутый жирный, H3+ — префикс `### `, фенсы
рисуются как `codeBlockBorder("```lang")` + тело + закрывающая линия
(`pitui components/markdown.ts:3039-3048`), списки — `- ` / `1. ` с
продолжением по ширине маркера (`pitui components/markdown.ts:3392-3397`),
цитаты — `quoteBorder` в 2 колонки (`pitui components/markdown.ts:3138-3160`),
таблицы, hr, mermaid, LaTeX.

### 2.2 omp `agent` (продуктовый экран): композиция, сегменты, движение

**Композиция.** `agent src/modes/interactive-mode.ts:1405-1411` создаёт
`chatContainer` (транскрипт), `statusContainer`, `todoContainer`,
`subagentContainer`, `errorBannerContainer`; `:1468-1470` — `editorContainer`;
`:1706` — `composer.setStatusComponent(this.statusLine)`;
`:1729-1746` — `composer.setRuntimeChildren([...])` в порядке
«транскрипт → HUD'ы → Working-лоадер → вложения → редактор → статусная строка»
(статусная строка — **последний** ребёнок кадра).

**Статусная строка.** Пресет по умолчанию (`pitui status-line/presets.ts:5-17`):

```
leftSegments:  ["pi", "vim", "model", "mode", "collab", "stream", "path", "git", "pr", "context_pct", "cost"]
rightSegments: ["session_name"]
separator:     "powerline-thin"
```

Разделитель — powerline-глифы (`pitui status-line/separators.ts:20-27`), а у
`plain`-раскладок — точка `·` (`pitui status-line/component.ts:2623-2626`); фон
берётся из `statusLineBg`, у `plain-full`/`plain-left` фон снимается совсем
(`pitui status-line/component.ts:2629-2637`). Пять раскладок описаны в
`pitui status-line/component.ts:2586-2600`.

**Пустой экран.** `pitui prompt/welcome.ts:326-378` — бокс с колонкой
«Welcome back!» + ASCII-логотип `PI_LOGO` (`pitui prompt/welcome.ts:520`) +
имя модели и провайдера, и правой колонкой недавних сессий; под боксом —
случайная подсказка из `tips.txt`, `Tip:`-метка и радужное свечение для
«новых» подсказок (`pitui prompt/welcome.ts:459-471`). Заставка один раз
проигрывает интро (`INTRO_MS = 3000`, `pitui prompt/welcome.ts:617-623`) и
затем «уходит в нативный scrollback» (`pitui prompt/welcome.ts:205-239`).

**Сообщения.** Пользователь — «бабл» с фоном `userMessageBg`
(`pitui chat/user-message.ts:73-135`); ассистент — обычный markdown без
ролевого заголовка (`pitui chat/assistant-message.ts:5,212,646`), с
переиспользованием `Markdown`-инстансов и публикацией «замороженного
префикса» во время стрима (`pitui chat/assistant-message.ts:68-78,205-212,704-790`).

**Чипы тулов.** Иконки статусов — отдельные глифы: `status.success "✔"`,
`status.error "✘"`, `status.running "⟳"`, `status.pending "⏳"`,
`status.warning "⚠"` (`pitui theme/symbols.ts:391-399`), выбор по статусу —
`pitui render/render-utils.ts:277-300`; активный кадр спиннера подставляется
туда же. Фон чипа — по статусу: `toolPendingBg` / `toolErrorBg` /
`toolSuccessBg` (`pitui chat/tool-execution.ts:963-967`), у отменённого вызова
нейтральный pending-тинт, а не error. Есть многофайловые pending-превью с
подписью `… N more files pending…` (`pitui chat/tool-execution.ts:1118-1141`) и
плавный reveal аргументов write/edit на 30 fps
(`agent src/modes/controllers/tool-args-reveal.ts:479-486`).

**Approval.** Текст промпта — `Allow tool: <name>` + `Origin: MCP server tool` +
`Reason: …` + детали из `formatApprovalDetails(args)`
(`agent src/tools/approval.ts:364-388`); ответ — не `y/n`, а селект:
`agent src/extensibility/extensions/wrapper.ts:401` — `choice = await
uiContext.select(safetyPrompt, ["Approve", "Deny"])`.

**Пикеры/панели.** `OverlayPanel` — «Rounded-box container for inline overlays
(selectors, run panels) … content is inset two columns on each side»
(`pitui chrome/overlay-box.ts:250-300`); якоря оверлеев (`center`, `bottom-center`
и т.д.) — `pitui tui.ts:333-343`; `SelectList` подключает `ScrollView` с
`scrollbar: "auto"` (`pitui components/select-list.ts:337-340`) и статус-строку
`No matching items` / `No items` (`pitui components/select-list.ts:265-270`).

**Движение.** «Working…» — `DEFAULT_WORKING_MESSAGE = "Working…"`
(`agent src/modes/interactive-mode.ts:428`) через `shimmerText`
(`agent src/modes/interactive-mode.ts:449-457`); бренд плавно меняет цвет между
idle и working (`BRAND_FADE_MS = 450` —
`pitui status-line/component.ts:62`, применение — `:1137-1157`); todo-строка прочёркивается
покадрово (`pitui chat/tool-execution.ts:319-320,674-681`).

### 2.3 Инвентарь других TUI на этой машине

Проверено (read-only): `/opt/homebrew/lib/node_modules` (`@openai/codex@0.146.0`,
`@xai-official/grok@1.0.44`, `pi-*`, `context-mode`, `freebuff`),
`/Users/workie/.bun/install/global/node_modules` (`@oh-my-pi/*`, `@earendil-works/*`,
`@anthropic-ai` (бинарник claude-code + SDK), `@cline` (бандл), `@opencode*`
(бинарник), `@mariozechner/clipboard` (Rust, не рендерер)),
`which -a crush codex gemini opencode claude grok` и `~/.cargo/bin`.

Вердикт: **читаемого исходника рендеринга у claude-code, codex, opencode, grok и
cline на диске нет** — только закрытые бинарники и бандлы: Mach-O
`~/.local/share/claude/versions/2.1.177` (215 МБ), `~/.codex/packages/standalone/current/bin/codex`
(248 МБ), `@opencode/cli/bin/opencode.exe` (176 МБ), `@openai/codex` (310 МБ
платформенных бинарников), `@cline` — бандл. Читаемы:
(1) omp — `@oh-my-pi/pi-tui/src`, `@oh-my-pi/pi-coding-agent/src` (наш эталон);
(2) тот же код на шаг впереди — `@earendil-works/pi-tui/dist` (читаемый JS + `.d.ts`,
не минифицирован: `dist/components/select-list.js`, `dist/components/editor.js`,
`dist/components/markdown.js`) и `@earendil-works/pi-coding-agent/examples/extensions/*`
(`working-indicator.ts`, `status-line.ts`, `message-renderer.ts`);
(3) `pi-subagents/src/tui/{render,fleet,fleet-status,fleet-transcript}.ts` —
самый крупный «не-omp» рендер чата на машине; (4) `pi-mcp-adapter/mcp-panel.ts`
(панель с собственным `PanelTheme`); (5) `@juicesharp/rpiv-ask-user-question`
(`view/components/preview/preview-box-renderer.ts`, `tab-bar.ts`,
`option-list-view.ts` с `MAX_VISIBLE_OPTIONS = 10`); (6) `@juicesharp/rpiv-todo`
(оверлей над редактором); (7) `@llblab/pi-telegram/lib/status.ts` + `docs/ui-style.md`.

Практический вывод: **альтернативных эталонов для «как выглядит современный
агентский TUI» на машине нет** — все основные агентские CLI закрыты. Поэтому
список в §3 опирается на omp (исходники + кадры выше) и на наши собственные
ранее зафиксированные решения (§2.4). Из «не-omp» стоит прочитать
`pi-subagents/src/tui/fleet-status.ts` и `pi-mcp-adapter/mcp-panel.ts`
при работе над статусной строкой и панелями, соответственно; отдельным
эталоном их не беру.

### 2.4 Что в titi уже есть, но живой экран не использует

| Есть в крейте | Где | Живой экран (`chat.rs`) |
| --- | --- | --- |
| 100 тем, 60 fg-токенов + 7 bg | `crates/titi-tui/src/theme/{schema,builtin,loader}.rs` | не используется (своя палитра `Ink`) |
| markdown-рендер | `crates/titi-tui/src/markdown.rs:151` | не вызывается |
| сборка статусной строки сегментами | `crates/titi-tui/src/status_bar.rs:78` (left/right, git-кеш) | своя шапка + `work_row` |
| композер-бокс с вставкой статуса в рамку | `crates/titi-tui/src/composer.rs:243` | своя отрисовка `composer()` |
| панели `SelectionPanel` (титул в рамке), `SessionSwitcher`, `CompletionPanel` | `crates/titi-tui/src/panels.rs:56,321,503` | плоский `picker_panel` |
| стек оверлеев с якорями | `crates/titi-tui/src/overlay.rs:30-160` | нет оверлеев вообще |
| таблицы глифов, спиннер-кадры | `crates/titi-tui/src/theme/symbols_data.rs` | 10 зашитых брайль-кадров в `chat.rs` |

Причина расхождения: живой экран — это `chat::run` → `draw()` на
`ratatui::Terminal` с `Ink::titanium()` (`chat.rs:3748-3750`), а весь слой темы
подключён к параллельному типу `App` (`crates/titi-cli/src/app.rs`), который
используют тесты и другие поверхности, но не `chat::run`.

## 3. Список доработок

Формат пункта: (a) что не так сейчас — кадр или код; (b) что делает эталон —
`file:line`; (c) что менять в titi; (d) объём и риск; (e) как режется на коммит.
Цены: S — <½ дня, M — ½–2 дня, L — >2 дней. Риск — риск для поведения (не для
вида): «чисто вид» = меняются только глифы/цвета/раскладка при том же наборе
фактов на экране.

### 1. Палитра: живой экран не пользуется темой

**(a)** Весь цвет экрана — из `Ink::titanium()` (`chat.rs:3829-3858`), «Dark
red»-палитра в коде; сырые SGR это подтверждают (§1.12: `255;64;84` accent,
`18;8;10` page). В `chat.rs` нет ни одного упоминания `Theme`, поэтому выбор
темы (100 встроенных тем, `crates/titi-tui/src/theme/builtin.rs:9-11`,
`themes/defaults/titanium.json` с `"accent": "electricBlue" (#00b4ff)`) на
живой экран не влияет. Имя `Ink::titanium()` при этом **не совпадает** с
темой `titanium` — это разные палитры с одним именем.

**(b)** omp держит цвета только в токенах (`pitui theme/schema.ts:26-85`,
значения — `pitui theme/dark.json:5-88`) и красит ими всё: бордеры
(`border → blue`), фоны чипов по статусу (`toolPendingBg/toolSuccessBg/toolErrorBg`),
markdown (`mdHeading/mdCode/mdQuote…`), диффы (`toolDiffAdded/Removed/Context`),
сегменты статусной строки (`statusLine*`).

**(c)** `Ink` заменить на `Arc<Theme>` в `Chat` (поле рядом с существующими,
`struct Chat` — `chat.rs:233-310`), `draw()` → `theme.fg(ThemeColor::X, …)`; `Ink::page()`
заменить на `ThemeBg` (там, где нужен фон). Источник: тот же, что у `App` —
`titi_tui::theme::loader`/`resolve_appearance` (`crates/titi-tui/src/theme/appearance.rs:221`,
`AUTO_DARK_THEME = "titanium"`), плюс `--theme`/`/theme` при желании.
11 мест `Ink::titanium()` (одно рабочее `chat.rs:3750`, остальные —
в тестах, `chat.rs:5806-8001`) меняются механически.

**(d)** M; риск — **чисто вид**, но дифф большой (все вызовы цвета), и надо
сохранить существующий контраст для `dim`-текста на тёмном фоне.
**(e)** Один коммит `refactor(chat): take every colour from the theme, drop Ink`.

### 2. Контраст и различимость семантических цветов

**(a)** В `Ink` три «сигнальных» цвета почти неразличимы: `amber
255,120,128` vs `red 255,96,112` (обе — светло-красные), `green 125,211,168`
единственный холодный; `line 92,42,50` (бордер композера на idle) на фоне
`18,8,10` даёт контраст **1.72:1** (посчитано по относительной яркости
WCAG 2.1) — рамка почти не видна (§1.2, §1.5). Различие
«успех/ошибка/ожидание» на экране держится на глифе (`✓`/`✕`/`▸`), не на
цвете.

**(b)** omp: `success #89d281`, `error #fc3a4b`, `warning #e4c00f`,
`muted #777d88`, `dim #5f6673` (`pitui theme/dark.json:6-12`);
`syntaxComment #6A9955`, `syntaxKeyword #569CD6`, `syntaxString #CE9178` и т.д. —
насыщенности специально разнесены по тону; бордеры — не фон-в-фон, а
`blue #178fb9` (`pitui theme/dark.json:22`).

**(c)** После п.1 — значения токенов в `crates/titi-tui/themes/*.json`; отдельный
проход по `titanium.json` (accent `#00b4ff`, `statusLine*`, `tool*Bg`), чтобы
`dim`/`border` читались на `#0f1216`. Предлагаемые значения — **предложения**,
не решение: например `border → #2a3038` уже есть в теме (`subtleGray`),
`amber → #ffb347`, `red → #ff4757` тоже уже в `titanium.json` (`warningAmber`,
`alertRed`).

**(d)** S; чисто вид (правка JSON-палитры + golden-тесты темы
`crates/titi-tui/src/theme/tests.rs`).
**(e)** Один коммит `style(theme): raise border/dim contrast in titanium`.

### 3. Границы и отступы: один бокс на весь экран

**(a)** На живом экране есть ровно одна рамка — композер (§1.2). Между
элементами транскрипта нет ни гаттеров, ни паддингов: `speech()` печатает
`"  "` + `{name:<4}` + `" │ "` (то есть `  you  │ ` — 9 клеток; `chat.rs:4407-4437`),
продолжения — 7 пробелов + `│ `; чип — `   ▸ `, заметка — `   · `; пустая строка ставится «между сообщениями» (`chat.rs:4115-4120`),
но не между тулом и ответом. Обратите внимание на §1.9: чип тула и текст
ответа визуально одного уровня, отличаются только префиксом.

**(b)** omp: у рамки титул врезан в верхнюю линию и отступ ровно 1 колонка
(`pitui chrome/overlay-box.ts:18-40`), `Box` — `paddingX/Y = 1`
(`pitui components/box.ts:56-60`), пустые строки добавляет только явный
padding (`pitui components/box.ts:188-205`); панели — «content is inset two
columns on each side» (`pitui chrome/overlay-box.ts:250-300`).

**(c)** Ввести один источник геометрии: константы гаттера/инсета в `chat.rs`
(сейчас размазаны как `"  "` + `{name:<4}` + `" │ "`, `"       │ "`, `"   ▸ "`)
и функции `gutter(kind)`, `inset()`; затем — рамка вокруг чипа тула (только
при `expanded`) и пустая строка между блоками разных ролей. Заодно убрать зашитые отступы
`picker_panel` (`format!("  {text}")` у заголовка, `format!("   {text}")` у
строки) и `hidden_line` в пользу той же геометрии.

**(d)** M; чисто вид, но трогает все тесты, которые сравнивают кадры как текст
(`chat.rs:7872-8001`, `tests/transcript.rs`).
**(e)** Один коммит `refactor(chat): one gutter and inset rule for the transcript`.

### 4. Шапка (masthead): состав и джиттер

**(a)** Строка 1 — единственное место с «фактами о сессии»: ` titi` + состояние
+ режим + loops + `модель  контекст%  id-сессии` (`masthead`, `chat.rs:3881-3951`).
Проблемы: (1) **в правом сегменте — id сессии**, самое бесполезное из
возможного; (2) при появлении `3%` правый блок съезжает влево на 3 колонки
(§1.8) — экран «дёргается» в середине хода; (3) нет ни пути, ни git-ветки, ни
стоимости/расхода, ни бейджей, хотя всё это уже собирается в
`crates/titi-tui/src/status_bar.rs:115-241`; (4) усечение жёстко запрограммировано
(сначала id, потом модель, `chat.rs:3930-3945`) — при `3%` порядок ломается.

**(b)** omp: левая группа `pi vim model mode collab stream path git pr
context_pct cost`, правая — `session_name` (`pitui status-line/presets.ts:5-17`),
разделители — powerline (`pitui status-line/separators.ts:20-27`), у plain —
`·` (`pitui status-line/component.ts:2623-2626`); сегменты строятся из живых
данных (git-кеш по mtime — `crates/titi-tui/src/status_bar.rs:242-245` и
`:307-386` уже так умеет).

**(c)** `masthead` переписать через `titi_tui::status_bar::StatusSnapshot`
(или его расширение): левая группа — `titi` + режим + путь + git; правая —
имя сессии (не id) + модель + контекст; **резервировать ширину заранее**, чтобы
появление `%` не двигало сегменты (передавать `Option<u8>` и рендерить на месте
с фиксированной шириной, как это делает `live_snapshot`).

**(d)** M; риск — **почти чисто вид**, но тест «ширина строки не меняется на
кадрах спиннера» (`agent-ux/README.md`, DoD статус-строки) надо усилить до
«не меняется и при появлении `%`».
**(e)** Один коммит `feat(chat): masthead as left/right segments with reserved widths`.

### 5. Статусная строка над композером: содержание и вид

**(a)** `work_row` (`chat.rs:3967-4016`) есть и работает (§1.8), но: (1) кадр
спиннера — один из 10 брайль-глифов, и для `Tool` подменяется на `⚙`
(`chat.rs:3995-4023`), то есть «работает» и «стримит» выглядят одинаково и
не отличимы от `waiting`; (2) у тула нет аргументов — только имя; (3) строка
обрезается с `…` (комментарий в `chat.rs:4018-4020`) — при узком терминале
пропадает как раз счётчик; (4) нет ни стоимости, ни счётчика субагентов, ни
заморозки таймера после хода; (5) `waiting for the first token` — длинно, а
строка живёт в `ink.accent` (красный = «ошибка» в голове пользователя).

**(b)** omp: `status`- и `activity`-наборы кадров разделены
(`pitui theme/symbols.ts:1469-1478`), шаг 80 мс (`pitui components/loader.ts:5-7`),
кадр из номера тика (`pitui status-line/segments.ts:192-195`), «Working…»
шиммерится (`agent src/modes/interactive-mode.ts:428,449-457`), бренд плавно
меняет цвет между idle и working (`BRAND_FADE_MS = 450` —
`pitui status-line/component.ts:62`).

**(c)** В `work_row`: три явных визуальных состояния (`activity`-спиннер для
ожидания/стрима, `status.running` для тула, `warning` для approval),
`шорт-текст · таймер` без обрезки (сначала сокращать текст, потом резать),
для тула — имя + главный аргумент (`read docs/README.md`).

**(d)** S–M; чисто вид + одна новая строка данных (аргумент тула), риск низкий.
**(e)** Один коммит `feat(chat): distinct spinner, tool args and shorter facts in the work row`.

### 6. Композер: каретка, рост под многострочный ввод, подсказки

**(a)** `composer()` (`chat.rs:4491-4565`): каретка — глиф `▍`
(`chat.rs:4561`), высота жёстко 4 строки (`chat.rs:3767`) при том, что
многострочный ввод свернули до хвоста (`fit_tail`); подпись живёт в нижней
рамке (`title_bottom`, `chat.rs:4507-4509`) и подменяется по контексту
(`composer_caption`, `chat.rs:4567-4598`); при длинном вводе нет скроллбара;
подсказка `enter sends  ·  /model  ·  ctrl-c quits` дублирует то, что уже
видно в заставке.

**(b)** omp: настоящий курсор — маркер + видимая клетка
(`pitui components/input.ts:525-529`, `pitui components/editor.ts:1355-1368`,
`inputCursor "▏"` — `pitui theme/tui-adapters.ts:172-180`); бокс-стиль держит
статус в верхней рамке и **сливает последнюю строку с нижней рамкой**, обещая
«one-line prompt at two rows total» (`pitui components/composer/box.ts:1-9`);
при переполнении правая вертикаль становится скроллбаром (`█`/`│`,
`pitui components/composer/box.ts:92-95`); подсказки форматируются как
`accent(key) + dim italic(label)` (`pitui prompt/composer-hints.ts:68-73`).

**(c)** `composer()`: (1) поставить реальный курсор — либо через
`CURSOR_MARKER`-эквивалент, который уже умеет вынимать titi-tui
(`titi_tui::cursor::extract_cursor` — `crates/titi-tui/src/cursor.rs:18`,
вызывается из `crates/titi-tui/src/renderer.rs:135`), либо
инверсией клетки на позиции каретки; (2) высоту композера сделать
`1..=6` от числа строк ввода (это уже изменение раскладки `draw`, а не только
вида); (3) вынести подсказку в отдельную строку над рамкой, а в рамке оставить
только контекст-процент; (4) скроллбар ввода.

**(d)** M; пункт (1) и (3) — чисто вид, (2) и (4) — поведение (нужны PTY-кадры и
тесты раскладки).
**(e)** Два коммита: `style(chat): real cursor and hint row in the composer`,
затем `feat(chat): grow the composer to the input and show its scrollbar`.

### 7. Чипы тулов: структура вместо одной плоской строки

**(a)** `tool_chip` (`chat.rs:4439-4458`) сводит всё к трём вариантам
`("▸", "✓", "✕")`, текст — «превью», которое приходит из движка; результат
`read` — одна плоская строка с переносом и `…` (§1.9); нет ни аргументов, ни
разворачивания/сворачивания, ни счётчика строк.

**(b)** omp: иконка по статусу (`✔ ✘ ⟳ ⏳ ⚠`, `pitui theme/symbols.ts:391-399`),
фон по статусу (`pitui chat/tool-execution.ts:963-967`), отменённый вызов —
нейтральный pending, а не error, многофайловые превью с
`… N more files pending…` (`pitui chat/tool-execution.ts:1118-1141`), подсказка
разворачивания `formatExpandHint` (`pitui render/render-utils.ts:306-310`,
`[<key>: Expand]` в `dim`),
плавный reveal аргументов (`agent src/modes/controllers/tool-args-reveal.ts:479-486`).

**(c)** `tool_chip` → структурный тип `ToolChip { name, arg, state, rows:
Rows|Expanded }`; заголовок `▸ read  docs/README.md`, тело — первые N строк
выхода, хвост `… +N lines`; `Ctrl+O` (`app.details.toggleAll`, уже
задекларирован в `crates/titi-tui/src/keybindings.rs:441-447`) — раскрытие.
Данные: движок уже шлёт `ToolStarted/ToolFinished` с `name`/`output`
(`chat.rs:842`), аргументы надо донести (проверить, что они есть в событии;
если нет — добавить в `EngineEvent`, отдельный коммит).

**(d)** L; задевает движок/события → не чисто вид, нужен PTY-кадр и тесты.
**(e)** Три коммита: `feat(engine): carry tool arguments in the tool events` →
`feat(chat): structured tool chips with an arg and an output tail` →
`feat(chat): ctrl-o expands every tool chip`.

### 8. Диффы: `edit`/`write` ничего не показывают

**(a)** `✓ edited` — и всё (§1.9). Ни строк, ни номеров, ни `+/-`; даже не
видно, какой файл.

**(b)** omp: `renderDiff` с контекстом `dim`, удалением `red`, добавлением
`green` и `inverse` на изменённых токенах (`pitui chrome/diff.ts:104-113`),
внутристрочно (`:59-99`), гуттер из 3 цифр (`:117-123`), подключён в чип
(`pitui chat/tool-execution.ts:1330-1332`), инструментальные обёртки
(`pitui tools/edit.ts:537,956,1231-1262`).

**(c)** Добавить в `titi-tui` модуль `diff.rs` (`render_diff(text, theme, width)`
по образцу `chrome/diff.ts`) — он там чистый и без зависимости от `ratatui`;
в `chat.rs` — новый `LineKind::Diff` и рендер в чипе `edit`/`write`.
В `titi-tui` уже есть токены `ToolDiffAdded/Removed/Context`
(`crates/titi-tui/src/theme/schema.rs:48-50`) и `Syntax*`.

**(d)** M; `titi-tui/diff.rs` — чисто вид (новый код + unit-тесты), подключение
в `chat.rs` — вид + один новый вид строки транскрипта.
**(e)** Два коммита: `feat(tui): a diff renderer with gutter numbers` →
`feat(chat): show edit and write diffs in the tool chip`.

### 9. Markdown в ответе ассистента

**(a)** Markdown не рендерится вообще: `LineKind::Assistant => speech(...)`
(`chat.rs:4314-4332`) отдаёт сырой текст; в кадре §1.8 видны литеральные `##`,
`- `, ```` ```rust ````, `> `. При этом `titi_tui::markdown::render_markdown`
(`crates/titi-tui/src/markdown.rs:151`) существует и используется **только** в
`App` (`crates/titi-cli/src/app.rs:1188`), а `Chat` о нём не знает.

**(b)** omp: полноценный `components/markdown.ts` — заголовки без `#`
(H1 — подчёркнутый жирный, H3+ — `###`-префикс, `pitui components/markdown.ts:2964-2992`),
фенсы с рамкой-линией и подсветкой (`:3039-3048`, `:2677-2719`), списки с
выравниванием продолжения по ширине маркера (`:3392-3397`), цитаты с бордером
(`:3138-3160`), таблицы (`:2956-2960`), hr (`:3129-3132`), inline
`bold/italic/code` (`:3258-3277`).

**(c)** Подключить `render_markdown` в `message_rows` для `LineKind::Assistant`
и почистить сам рендер: убрать `#`-префикс у заголовков
(`markdown.rs:191-201`), оформить код-блок рамкой/фоном по
`MdCodeBlockBorder` (`markdown.rs:281-292`), добавить `_italic_` и
`` `code` ``-стили в `style_inline` (`markdown.rs:301-394`), поддержать таблицы.

**(d)** M–L; сама разметка внутри titi-tui — чисто вид (golden-тесты), но
подключение меняет вид каждой строки ответа → нужны PTY-кадры.
**(e)** Три коммита: `feat(tui): markdown headings, code frames and inline styles` →
`feat(chat): render assistant replies as markdown` →
`feat(tui): tables and quotes in the markdown renderer`.

### 10. Блоки ролей: `you │` / `titi │` без фона и без времени

**(a)** Реплики — «спич» с гаттером и вертикальной чертой
(`chat.rs:4407-4437`): пользователь и ассистент отличаются только цветом
подписи (`you` — `gold`, `titi` — `accent`, оба из `Ink`), нет фона, нет метки
времени, нет отличия «ожидает» от «отправлено».

**(b)** omp: пользователь — бабл с фоном `userMessageBg` и отдельным
`userMessageText` (`pitui chat/user-message.ts:73-135`,
`pitui theme/dark.json:13,31`); ассистент — markdown без ролевого заголовка
(`pitui chat/assistant-message.ts:212,646`); заголовков `User:`/`Assistant:` в
omp нет вовсе (это зафиксировано в нашем же разборе
[agent-ux/README.md](README.md), §3).

**(c)** В `speech()` — подложка `ThemeBg::UserMessageBg` под блок пользователя
(хелперы уже есть: `Theme::bg_fill` — `crates/titi-tui/src/theme/mod.rs:342`,
`Selection::apply_background` — `crates/titi-tui/src/selection.rs:68`) и подпись роли в
`dim`; ассистенту — ничего лишнего.

**(d)** S; чисто вид (нужна аккуратная отрисовка фона от строки до строки, иначе
фон обрывается на концах абзацев).
**(e)** Один коммит `style(chat): a background band for the user block`.

### 11. Пустое состояние

**(a)** Три центрированные строки: `titi`, `say what you want done`,
`enter send · /model switch · ctrl-c quit` (`empty_state`, `chat.rs:4031-4052`).
Нет версии, cwd, модели, последних сессий, подсказок; нет анимации; в
`--mode plan` заставка не показывается вообще (§1.4).

**(b)** omp: бокс с «Welcome back!», логотипом и моделью/провайдером слева и
списком недавних сессий справа (`pitui prompt/welcome.ts:326-378`), подсказка
под боксом с меткой `Tip:` (`:459-471`), логотип `PI_LOGO` (`:520`), одноразовое
интро 3000 мс (`:617-623`) и уход заставки в scrollback после интро (`:205-239`).

**(c)** `empty_state` → компонент: рамка `boxRound`, слева `titi v…` + модель +
cwd, справа — 4 последние сессии (данные есть: `list_sessions_from` —
`crates/titi-cli/src/app.rs:2109`), под рамкой — одна подсказка из списка
(подсказки можно взять из `tips.txt`-идеи), и показывать заставку всегда, пока
транскрипт пуст (в том числе в plan-режиме — заметку про режим перенести в
шапку, в сегмент `mode`).

**(d)** M; в основном вид, но меняет условие показа заставки → поведение на один
тест.
**(e)** Два коммита: `feat(chat): a welcome box with version, model and recent sessions` →
`feat(chat): show the welcome box whenever the transcript is empty`.

### 12. Пикеры и панели: рамка, титул, скроллбар

**(a)** Пикер модели (`picker_panel`, `chat.rs:3696-3737`) — плоский список в
области транскрипта: нет рамки, нет заголовка-в-линии, нет скроллбара, о
переполнении сообщает текст `… 23 more below` (`hidden_line`, `chat.rs:3741-3746`),
подсвечен только глиф `▶` + жирный. Заголовок `models · 28` — обычная строка в
`dim`. Селектор сессий на живом экране **недоступен** (§1.13).

**(b)** omp: `OverlayPanel` — рамка с титулом в верхней линии, контент с инсетом
2 колонки (`pitui chrome/overlay-box.ts:18-40,250-300`); `SelectList` целиком
красит выбранную строку (`pitui components/select-list.ts:389-432`) и держит
скроллбар `auto` (`:337-340`), статус-строка `No matching items` (`:265-270`);
якоря — `bottom-center` и т.д. (`pitui tui.ts:333-343`).

**(c)** Использовать `crates/titi-tui/src/panels.rs:56` (`SelectionPanel` — титул
уже врезается в рамку) вместо самодельного `picker_panel`, и `overlay.rs:30-160`
для якоря над композером вместо врезки в транскрипт; скроллбар — новый виджет
в `titi-tui` по образцу `scroll-view` (`pitui components/scroll-view.ts:15-30,488-496`);
селектор сессий подключить в `chat.rs::on_key` (сейчас чорд живёт только в
`app.rs:1423`).

**(d)** M; `SelectionPanel` уже покрыт тестами (`panels.rs:822-900`) → замена —
риск средний (меняется область отрисовки), скроллбар — чисто вид.
**(e)** Три коммита: `refactor(chat): draw the picker with SelectionPanel` →
`feat(tui): a scrollbar widget for panel lists` →
`feat(chat): Ctrl+X session switcher on the live screen`.

### 13. Оверлеи и approval: показать, о чём спрашивают

**(a)** Approval подменяет содержимое композера одной строкой
`write   y allow    n refuse` (`chat.rs:4514-4518`), в статус-строке
`⚠ needs you · write` (`chat.rs:3977-3985`); **аргументов нет**: не видно ни
пути, ни содержимого, ни причины (§1.10). Стек оверлеев
(`crates/titi-tui/src/overlay.rs`) на живом экране не используется вообще.

**(b)** omp: `Allow tool: <name>` + `Origin: MCP server tool` + `Reason: …` +
`formatApprovalDetails(args)` (`agent src/tools/approval.ts:364-388`), ответ —
селект `Approve`/`Deny` (а не `y/n`, `agent
src/extensibility/extensions/wrapper.ts:401`), поверх экрана — модальная
панель, а не подмена композера.

**(c)** `chat.rs`: при `approval.is_some()` рендерить `OverlayPanel` над
композером с титулом `⚠ write`, строками деталей (путь/дифф/содержимое) и
селектом `▶ Approve  ·  Deny` с подтверждением `Enter`; `y`/`n` оставить как
алиасы. Детали: `ToolFinished`/`ToolStarted` + аргументы из п.7 (для `write` —
первые строки содержимого, для `bash` — сама команда).

**(d)** M; поведение (новый оверлей, новая клавиша) → PTY-кадры + тесты.
**(e)** Два коммита: `feat(chat): approval as an overlay panel with details` →
`feat(chat): approve with enter, keep y/n as aliases`.

### 14. Ошибки и заметки не отличаются от тулов

**(a)** Одна функция `chip()` рисует ошибки (`✕`), заметки (`·`) и тулы (`▸`) —
различие только в глифе и цвете (`chat.rs:4314-4332,4467-4489`). Кадр §1.10:
`✕ tool error  tool invocation denied`. Ошибки не отделены от контента, у них
нет ни заголовка, ни предложения, что делать.

**(b)** omp: `status.error "✘"`, `status.warning "⚠"`, `status.info "ⓘ"`
(`pitui theme/symbols.ts:391-399`), фон ошибки — `toolErrorBg` (#291d1d,
`pitui theme/dark.json:41`), есть отдельный `errorBannerContainer` в композиции
экрана (`agent src/modes/interactive-mode.ts:1417`).

**(c)** Разделить `LineKind::Error` (красный блок с фоном и заголовком
`✕ bash failed`), `LineKind::Note` (dim, без жирного) и `LineKind::Tool`;
ошибку тула оставить **внутри** чипа этого тула, а не отдельной строкой.

**(d)** S; чисто вид.
**(e)** Один коммит `style(chat): separate error blocks from notes and tool chips`.

### 15. Движение: спиннеры, шиммер, интро

**(a)** `spinner_frame` — 10 брайль-кадров, `SPINNER_PERIOD`
(`chat.rs:63-64` — кадры и период **50 мс**, `chat.rs:3871-3873` — выбор
кадра); «Working» — слово, не анимировано; переход состояния не анимирован;
логотип/заставка статичны. При этом кадры глифов и таблицы
символов уже есть в `crates/titi-tui/src/theme/symbols_data.rs`, а разделение
`status`/`activity` — в omp (`pitui theme/symbols.ts:1469-1478`).

**(b)** omp: два набора кадров и шаг 80 мс (`pitui theme/symbols.ts:1469-1478`,
`pitui components/loader.ts:5-7`), шиммер «Working…»
(`agent src/modes/interactive-mode.ts:449-457`), плавная смена цвета бренда
450 мс, прочерчивание todo (`pitui chat/tool-execution.ts:674-681`), интро
заставки 3000 мс (`pitui prompt/welcome.ts:617-623`).

**(c)** Взять кадры/шаг из `titi-tui` (один источник вместо зашитой константы),
добавить `status`-набор для тула; шиммер и интро — только если п.11 (заставка)
и п.5 уже приняты; интро включать по `appearance`-политике и гасить при
`TERM=dumb`/отсутствии truecolor.

**(d)** S (кадры) / M (шиммер+интро); кадры — чисто вид, шиммер/интро —
поведение (таймеры, лишние кадры, риск на слабых терминалах).
**(e)** Два коммита: `refactor(chat): take spinner frames from the theme symbols` →
`feat(chat): shimmer the working row`.

### 16. Скролл транскрипта: нет индикатора

**(a)** Транскрипт скроллится (`chat.rs:480-521`), но на экране нет ни полосы,
ни `… N more above` (в отличие от пикера, где `hidden_line` есть). Пользователь
не знает, что выше что-то есть.

**(b)** omp: одна колонка `track`/`thumb`, `auto` — только при переполнении
(`pitui components/scroll-view.ts:18-30,488-496`), цвета — `muted`/`accent`.

**(c)** Новый виджет `titi-tui` (общий для транскрипта и панелей, см. п.12) +
одна колонка справа в области транскрипта в `draw()`.

**(d)** S–M; чисто вид (добавляет колонку → перенос текста на 1 символ уже,
проверить golden'ы).
**(e)** Один коммит (общий со п.12, если делать подряд): `feat(tui): a scrollbar widget`.

## 4. Короткий список (порядок лендинга, по одному коммиту на пункт)

Столбец «риск»: **вид** — меняются глифы/цвета/раскладка при том же наборе
фактов, тестам достаточно golden'ов; **экран** — трогает живой `chat.rs`
(нужны PTY-кадры и тесты поведения).

| # | Пункт (§3) | Что даёт | Объём | Риск | Коммит |
| --- | --- | --- | --- | --- | --- |
| 1 | 1 (палитра) | Убирает «красный» самодельный `Ink`; живой экран начинает слушать тему — без этого бессмысленны все темы и п.2 | M | вид, но `chat.rs` | `refactor(chat): take every colour from the theme, drop Ink` |
| 2 | 9 (markdown) | Главный видимый дефект: `##`, `-`, ```` ``` ```` сейчас литеральные | M | вид (+ `markdown.rs`) | `feat(tui): markdown headings, code frames and inline styles` + `feat(chat): render assistant replies as markdown` |
| 3 | 8 (диффы) | `✓ edited` → настоящий дифф; самый заметный «нет информации» на экране | M | вид (новый модуль + вид строки) | `feat(tui): a diff renderer with gutter numbers` + `feat(chat): show edit and write diffs in the tool chip` |
| 4 | 4 (шапка) | Правая группа перестаёт дёргаться, id сессии уходит, появляются путь/git | M | экран | `feat(chat): masthead as left/right segments with reserved widths` |
| 5 | 5 (статусная строка) | Три различимых состояния + имя тула с аргументом | S | экран (узкий) | `feat(chat): distinct spinner, tool args and shorter facts in the work row` |
| 6 | 3 (геометрия) + 10 (фон юзера) | Один гаттер/инсет, воздух между блоками, фон под репликой пользователя | M | вид (много golden'ов) | `refactor(chat): one gutter and inset rule for the transcript` + `style(chat): a background band for the user block` |
| 7 | 12 (панели) + 16 (скроллбар) | Пикер получает рамку/титул/скроллбар; `Ctrl+X` работает | M | экран | `refactor(chat): draw the picker with SelectionPanel` + `feat(tui): a scrollbar widget for panel lists` + `feat(chat): Ctrl+X session switcher on the live screen` |
| 8 | 11 (заставка) | Первый экран перестаёт быть тремя строками текста | M | экран (условие показа) | `feat(chat): a welcome box with version, model and recent sessions` |

Тот же список словами (в этом порядке и лендится, по одному коммиту):

1. **Тема на живом экране** — `crates/titi-cli/src/chat.rs` (`Ink`, `draw`),
   `crates/titi-tui/src/theme/**` только на чтение → `refactor(chat): take every
   colour from the theme, drop Ink`.
2. **Markdown ответа** — `crates/titi-tui/src/markdown.rs` (заголовки, рамка
   кода, inline) + `chat.rs` (`message_rows`) → `feat(tui): markdown headings,
   code frames and inline styles`, `feat(chat): render assistant replies as
   markdown`.
3. **Диффы** — новый `crates/titi-tui/src/diff.rs` + `chat.rs` (вид строки,
   чип `edit`/`write`) → `feat(tui): a diff renderer with gutter numbers`,
   `feat(chat): show edit and write diffs in the tool chip`.
4. **Шапка** — `chat.rs` (`masthead`, `draw`) → `feat(chat): masthead as
   left/right segments with reserved widths`.
5. **Статусная строка** — `chat.rs` (`work_row`, `spinner_frame`) →
   `feat(chat): distinct spinner, tool args and shorter facts in the work row`.
6. **Геометрия + фон реплики пользователя** — `chat.rs` (`speech`, `chip`,
   `draw`) → `refactor(chat): one gutter and inset rule for the transcript`,
   `style(chat): a background band for the user block`.
7. **Панели и скроллбар** — `crates/titi-tui/src/panels.rs` (переиспользовать
   `SelectionPanel`), новый виджет скроллбара, `chat.rs` (`picker_panel`,
   `on_key` для `Ctrl+X`) → `refactor(chat): draw the picker with
   SelectionPanel`, `feat(tui): a scrollbar widget for panel lists`,
   `feat(chat): Ctrl+X session switcher on the live screen`.
8. **Заставка** — `chat.rs` (`empty_state`, условие показа в `draw`) →
   `feat(chat): a welcome box with version, model and recent sessions`.

Разделение по риску — явно:

- **Чисто titi-tui, без поведения** (низкий риск, покрывается golden/unit-тестами
  крейта): шаг 2 в части `markdown.rs`; шаг 3 в части `diff.rs`; шаг 7 в части
  нового виджета скроллбара и `panels.rs`; правки палитры в
  `crates/titi-tui/themes/*.json`; кадры спиннера из `theme/symbols_data.rs`.
- **Задевает живой экран `chat.rs`** (нужны PTY-кадры в реальном терминале и
  тесты поведения/раскладки): шаги 1, 4, 5, 6, 8 целиком; в шаге 2 —
  подключение markdown; в шаге 3 — новый вид строки; в шаге 7 — замена
  `picker_panel` и чорд `Ctrl+X`.
- **Задевает движок** (вне короткого списка): только п.7 §3 (аргументы тула в
  `EngineEvent`) — поэтому структурные чипы и approval-оверлей (п.13) отложены.

Что **не** попадает в короткий список и почему: п.7 (структурные чипы тулов —
тянет за собой `EngineEvent` с аргументами, это отдельная работа по движку) и
п.13 (approval-оверлей — зависит от п.7, иначе деталей не из чего взять);
п.14 и п.15 — дешёвые, но малозаметные, их логично приклеить к ближайшему
коммиту по палитре/чипам, а не вести отдельно.

Порядок выбран так, чтобы каждый следующий шаг не приходилось переделывать:
палитра → контент (markdown, диффы) → верх и низ экрана (шапка, работа,
композер) → геометрия → панели → заставка → движение.

## 5. Что не проверено

- Не снят кадр фазы `thinking` (reasoning) — скриптовый провайдер её не отдаёт
  (`WorkPhase::Thinking`, `chat.rs:4000-4008` существует, но на экране не
  наблюдалась).
- Кадры хода/тула/approval сняты на локальном скриптовом провайдере, а не на
  реальной подписке: в момент съёмки живой путь вызова инструмента на подписке
  ChatGPT не отвечал (дефект декодера Responses; починен параллельным потоком —
  запись в `docs/research/STATE.md`), поэтому кадры «тул выполняется» на живом
  ключе получить было нечем. Вординг движка (`tool done  <preview>`) — из
  настоящего движка, но «настоящий» ответ модели в этих кадрах не участвует.
- Селектор сессий (`Ctrl+X`) на живом экране не открылся; вывод «его нет в
  `chat.rs`» сделан по коду (`app.rs:1423` — единственный диспатч), а не по
  кадру.
- Advanced-темы из `themes/defaults/` (98 штук) визуально не проверялись: они
  всё равно не доходят до живого экрана (§3.1), проверять нечего.
- Предложенные значения цветов в п.2 — **предложения**, не измеренный
  результат; контраст надо перемерить после п.1.
- Часть кадров снята до пересборки бинарника другим потоком работы (§1.1);
  контрольный прогон на текущем бинарнике совпал, но если `chat.rs` изменится,
  кадры надо переснять — сам стенд (скриптовый SSE-сервер + `pyte`) для этого
  воспроизводим по §1.1.
