# STATE — точка возобновления конвейера titi

## Активное направление: reference product functional port

Статус: **0.1.0 работает** (чат на ratatui, headless, инструменты, сессии, Genome, память, SOUL). Встроенный каталог: OpenAI, OpenRouter, OpenCode, Anthropic; пользовательский конфиг накладывается по id, а не заменяет список. Канон направления — этот файл плюс [карта тем](README.md); каталог `reference-product-port/`, на который ссылались старые доки, в репозитории отсутствует, ссылки в research-доках исправлены. Goal loop поверх reviewer-а в master (`/goal`, 2026-09-23). Следующий шаг — ручная PTY-проверка `/goal` с живой моделью. Не начинать GPUI. Tree-sitter — замена эвристик Genome, не новый граф: symbol-level уже есть.

Правило продолжения: сначала прочитать этот STATE и док темы, прогнать baseline, выполнить только NEXT, затем обновить STATE.

Обновляется после каждого шага. Новая сессия начинает отсюда.

## Вывод bash, нестрогий edit, grep и учёт токенов (2026-10-04, третья волна, отлендилось в master)

- вывод `bash` (`eefe9f7`): модели уходило всё, что команда писала для терминала — цветовые коды, заголовки окна, сотни перерисовок `\r`-прогресс-бара (`pty: true`, а на пайпе — `curl`, `pip`, `git`); `ansi::plain` проигрывает вывод как терминал (`\r`, backspace, `ESC[K`, `ESC[G`) и оставляет текст, который остался бы на экране, без escape-последовательностей; на обоих путях, включая таймаут и прерывание. Из нативного минимизатора omp взята часть, не зависящая от команды;
- `edit` (`d39e9f0`): если `old_string` нет как написано, он сравнивается построчно без окружающих пробелов, с одним пробелом вместо серий и ASCII вместо типографских кавычек, тире и пробелов; ровно одно совпадение — правится, новые строки получают отступы файла (отступ вызова → отступ файла, самый длинный префикс, вложенность сохраняется: 2→4 пробела, пробелы→табы), ответ говорит, что совпадение нестрогое; два места — отказ с числом; токены не игнорируются (`a=b` ≠ `a = b`); точное совпадение по-прежнему первое. По мотивам fuzzy-match omp, без порога — всё или ничего;
- `grep`: путь вне workspace или несуществующий — ошибка, а не молчаливый поиск по всему workspace (`5296d5a`); совпадение в длинной строке (минифицированный бандл) — 300 символов вокруг первого совпадения с `…` (`9fe307d`);
- учёт токенов: ход, отменённый или упавший (кэп раундов, провайдер), теперь сообщает `TurnUsage` за оплаченные раунды — раньше `/usage` их терял, а бюджет считал (`5ae3e6c`; тело `run_turn` перенесено в async-блок, поэтому дифф переотступом, `git diff -w` — суть); `TurnUsage.cached_tokens` (serde default) и `/usage` — `1200 prompt (1000 cached)` (`daa04e4`).

- `read` (`f6c8769`, `549b339`): один ответ — не больше 30 000 символов (ниже кэпа движка) и 2000 строк без `limit`; длинный файл приходит первой частью с `[lines 1-N of M; read on from offset N+1]`, а не головой и хвостом с вырезанной серединой; `read` каталога — список (каталоги с `/`), бинарник — `not UTF-8 text (N bytes)` вместо сообщения декодера (из кэша, так что и у `hashline_edit`). Проверено вживую: 5000 строк → 8942 символа модели.

Вживую (PTY 100×30 + `scripts/fake-provider.py`, новые слова `color shell`, `loose edit`, usage с `cached_tokens: 60`): `✓ fetched ok` вместо escape-кашки; нестрогий edit с диффом; после отменённого хода `Session: 500 (300 cached) / 50` — его раунд учтён; `sleep 600` после Ctrl+C не остался. Записано в `docs/QA_STATUS.md`.

## Порт из omp, границы bash и заставка (2026-10-03, вторая волна, отлендилось в master)

Четыре воркера в отдельных worktree (супервизор смотрел диффы и тесты, а не отчёты, и черри-пикал по одному) плюс правки супервизора. Идеи взяты из omp 18.5.1 (MIT) и переписаны на Rust, кода оттуда нет.

- заставка (`3aae036`, `8d51599`, `6681a9e`): вместо рамки — чёрно-белый локап TITI как у omp: знак `██████ ██████` с полублочным вордмарком `titi` и версией справа, по центру, без рамки; все клетки знака — чистые серые, вычисленные от яркости страницы (на тёмной 0.96, на светлой 0.08), градиент по диагонали как у omp; под ним теглайн, блок фактов, аккорды и `Tip: …` (20 правдивых подсказок, выбор — хэш id сессии); на старте по знаку проходит блик (1.5 с, ease-out), после чего кадр равен покою; деградация по высоте — подсказка → сессии → каталог → теглайн → пустые строки → модель, ниже — одна строка `titi v0.1.0`;
- `todo` (`ef1dffc`, `b384265`, рендер `d3bfe78`): чеклист агента — `write` (весь список), `update` (статус по номеру строки), `view`; в работе не больше одного пункта (старт другого возвращает прежний в pending), 50 пунктов по 200 символов, read-tier (живёт в plan), в duck и у субагентов нет, на перезапуск не сохраняется; чип `todo 1/3 · Fix the parser`, под ним строки чеклиста;
- `read`/`glob`/`grep` (`c0e0108`, `118efd5`, `fd771b2`, `f98315c`): `offset`/`limit` с заголовком `[lines A-B of N]`; настоящие glob-шаблоны (`**/`, классы, `{a,b}`) через `regex` (уже был в lock-дереве, RustSec чист); grep по регулярке с `ignore_case` и фильтром `glob`, порядок по путям, кэпы 1000 путей / 500 строк со счётом остатка; `path` на файл теперь ищет в этом файле, а не возвращает пусто;
- реальные токены (`c1b9e55`, `c151d84`, `c77e5c3`, `c44631f`): `StreamEvent::Usage(TokenUsage)` из всех четырёх семей (Chat Completions, Responses/Codex, Anthropic с кэшем, Gemini с thoughts), Chat Completions просит `stream_options.include_usage`, помпа держит `Done` до 2.5 с ради хвостового чанка с usage, движок берёт счёт провайдера и оценку — только когда счёта нет; `/budget` больше не пишет «estimated» (`3db0dc7`);
- границы `bash` (`ec2abdd`, `7d05ab5`, `762f782`, `829e2a6`): путь по пайпу шёл через `Command::output` без дедлайна, без прерывания, без кэпа и ждал закрытия пайпов — `npm run dev`, `--watch`, `sleep 999`, даже `server &` вешали ход навсегда; теперь `pipe.rs` — дедлайн `timeout_secs` (300 по умолчанию) на обоих путях, своя группа процессов и SIGTERM→SIGKILL всей группе (сигнал шлёт `sh`, потому что `unsafe` запрещён), первые и последние 64 KiB каждого потока, ответ по выходу shell, а не по закрытию пайпов, `exit N` в ошибке. Ctrl+C не доходил до команды вообще: `Interrupt` был, но его никто не поднимал — теперь он в `EngineConfig`, Cancel/Shutdown его поднимают, новый ход опускает, а счётчик поднятий не даёт команде пропустить отмену, если флаг опущен до её опроса. Из интерсептора omp (у них выключен по умолчанию) взяты только правила для команд, которые не завершаются сами: dev-серверы, вотчеры, `tail -f`, `docker compose up` без `-d` на переднем плане отклоняются сразу с подсказкой `cmd > log 2>&1 &` и `timeout 30 cmd`; фон, кавычки, комментарии и heredoc не считаются. Отказ решается до подтверждения (`ToolHandler::refusal`, `68ad58b`): пользователя не просят одобрить команду, которая всё равно не запустится.

Проверено вживую (PTY + `scripts/fake-provider.py`, новые ключевые слова `forever`, `dev server`, `make todo`), записано в `docs/QA_STATUS.md`: заставка в 80×24/60×20/100×30, 84 оттенка — все серые; `npm run dev` отклонён мгновенно; Ctrl+C на `sleep 600` — `command interrupted` сразу и ни одного `sleep` в `ps`; чеклист строками; `/usage` — `200 prompt + 20 completion` от провайдера. База: workspace 1742 passed, 0 failed (было 1664); clippy — новых предупреждений вне тестов нет.

Открыто (что из этого закрыто — в секции выше): правила omp «cat/grep/find → read/grep/glob» не портированы (у omp выключены по умолчанию).

## Полировка по живому PTY-прогону (2026-10-03, отлендилось в master `751dbc1`)

Метод: реальный бинарник в PTY (pyte) против скриптового OpenAI-совместимого сервера на loopback — настоящие HTTP-ходы без ключа и без живой модели. Инструменты в репозитории: `scripts/fake-provider.py` (ответ по ключевому слову: `run bash`, `slow`, `edit readme`, `fail429`, … — список в docstring; каждое тело запроса пишется в `requests.jsonl`) и `scripts/pty-drive.py` (сценарий из клавиш/ожиданий/ресайзов → кадры строками); процедура — в скилле `tui-smoke`, раздел «A real turn without a key» (`c9a2c34`, `cd37f45`). Прогон и записан в `docs/QA_STATUS.md` (`8f4f57d`): шесть строк, которые никто не проверял, теперь `pass`.

Что прогон нашёл и что исправлено (по коммиту на правку, каждая с поведенческим тестом, который падал до неё):

- безопасность: каталог провайдеров читался и из проектного `.titi/config.yml`, то есть клонированный репозиторий мог переопределить `openai.base_url` и получить ключ первым же запросом — теперь `providers`/`models` только из пользовательских слоёв (`7da590f`); инструмент `settings` пропускал `privacy` целиком, `providers`, `tools` (родитель `tools.approval`), листья `apiKey`/`token`, пустые сегменты и чужой scope — guard стал посегментным (`d58fac1`);
- чат: подтверждение инструмента было слепым (`bash   y allow    n refuse` без команды) — теперь описание вызова в промпте, строке статуса и чипе (`2272996`); текст раунда после инструмента приклеивался над чипом (`766e1a1`); стир, набранный под финальным ответом, терялся и всплывал после следующего промпта — теперь отвечается следующим ходом, а Cancel возвращает его в композер (`199c347`); многострочная вставка сплющивалась в одну строку — переносы и табы сохраняются, композер показывает `↵` (`261d436`); перезапуск показывал заставку, хотя модель получала прошлый разговор — теперь он на экране (`4c2d7e8`); фолбэк и ручное переключение модели различаются (`979a460`); `/keys` больше не пишет `no key` провайдерам, которым ключ не нужен (`b5a5bc3`);
- движок: `TurnUsage` считал только последний раунд хода (`c4cfd56`); транзиентные ретраи шли без паузы — теперь бэкофф 500 мс ×2 до 8 с с отменой по слайсам (`69beedd`); «all configured models are unavailable» теперь называет последнюю модель и её ошибку (`38625ed`); вызов инструмента, убранного режимом, отвечает `bash is withheld here; only read tools are offered`, а не `unknown tool` (`d8f92ef`); результат любого инструмента режется до 40 000 символов (голова + хвост + пометка), иначе один `seq 1 20000` съедал 85% окна (`9e1876c`);
- провайдеры: ошибки транспорта читаются фразой — `rejected (HTTP 401): …`, без `Some(401)` (`7cb7f60`);
- инструменты: `edit` отказывает неоднозначному `old_string` (или `replace_all`), находит CRLF-файл по LF-тексту и сохраняет CRLF/BOM, отвергает пустой и пустой-по-смыслу edit (`751dbc1`); unwrap в `titi-memory` убран (`06bd358`);
- CLI: device-флоу `--login --device` печатает device-инструкции (`2441cd1`); `--replay` воспроизводит стримленный ответ целиком, а не первую дельту (`18befc4`); `--help` описывает пикер моделей (`2e93618`).

База после: workspace 1664 passed, 0 failed, 1 ignored; CI на master зелёный (run #77 на `9e1876c`: fmt, clippy, test, tui-smoke). Clippy без ошибок; +7 предупреждений — `unwrap`/`expect` в тестовых файлах, где эта идиома уже повсюду.

Открыто (за владельцем): app.rs и стек оверлеев `titi-tui` по-прежнему недостижимы из бинарника; решение A/B по кэшу промпта (`prompt-cache.md`); история промптов по стрелке вверх (сейчас стрелки скроллят транскрипт); BRAIN.md устарел (скоркарта от 2026-09-23) — обновляется только циклом `brain-audit`.

## Карта репозитория (Genome): маркер, счётчик и лимит (2026-10-02, отлендилось)

Восемь коммитов по карте в промпте, по одному на правку:

- `ae1aa96` fix(genome): ignore method calls when counting symbol uses — вызовы методов и std-квалифицированные пути больше не считаются использованием символа: `path.join` и `Path::join` — не использование уникального `join`, а `hub::join` — по-прежнему использование;
- `9437b5d` fix(genome): mark recently modified files as recent — маркер `[RECENT]` вместо `[NEW]`: «недавно изменён», а не «недавно создан», окно те же 48 часов (`NEW_WINDOW`, `crates/titi-genome/src/project.rs:6`);
- `ba6343b` fix(genome): drop unused exports from the prompt map — в промпте не больше четырёх экспортов на файл (`PROMPT_EXPORTS`), экспорт с нулём пользователей не показывается, ранжирование файлов не изменилось;
- `5038c0f` feat(cli): read genome.limit from settings — лимит файлов из настройки `genome.limit` (1..=64, умолчание 24, проектный `.titi/config.yml` побеждает);
- `05548f8` docs(cli): document genome.limit and TITI_NO_GENOME — `--help` называет и настройку, и переменную;
- `e253ffa` docs(genome): drop the citation of a missing spec, `5829ada` style(genome): rustfmt the symbol filter, `da47710` docs(agents): point genome mapping at the crate example — доки и стиль.

Измерения в [prompt-cache.md](prompt-cache.md) сняты, когда маркер назывался `[NEW]`; строка маркера теперь `[RECENT]`, окно не менялось, поэтому тиканье осталось. `docs/ARCHITECTURE.md` описывает карту уже с `[RECENT]`, четырьмя экспортами и `genome.limit`.

## Genome: проверка, LSP и живой дифф (2026-10-02, отлендилось)

Карта в промпте перестала быть только картой; семь коммитов, по одному на правку:

- `4eef588` feat(cli): turn the genome on and off from the terminal — `titi genome` идёт до движка и ключа не требует: голый печатает четыре строки статуса (вкл/выкл, причина — `setting`, `TITI_NO_GENOME` или `default` для незаданного ключа, действующий cap 1..=64 иначе 24, путь к agent-конфигу), `on`/`off` пишут булево в agent-конфиг, `limit <n>` — cap после проверки диапазона (0, 65 и не-число отвергаются с кодом 2 и без записи), а `/genome` в чате повторяет те же глаголы тем же форматтером (`engine::genome_note`), чтобы поверхности не разъезжались;
- `6291441` feat(genome): check the index and serve a stdio LSP — `titi genome check` печатает по строке на диагностику (`path:line: code: message`), на чистом дереве `genome: clean` и выход 0, любая диагностика — выход 1; `titi genome lsp` поднимает stdio-LSP без сокета: `initialize`, `shutdown`, `textDocument/documentSymbol`, `textDocument/definition`, `textDocument/references`, `textDocument/diagnostic`, кадры — `Content-Length` (`crates/titi-genome/src/lsp.rs`), из новых зависимостей только `serde_json`;
- `9b20f79` fix(genome): an unreadable root is an error, not an empty index — индекс, который не построился, винит себя (`genome: check failed (…)`, выход 1), а не отдаёт мнимо чистое дерево;
- `79319d0` feat(cli): serve genome check and lsp — `check` работает локально и из чата, а `/genome lsp` в чате только называет терминальную команду (`crates/titi-cli/src/chat.rs`);
- `c7813b9` feat(engine): show the working-tree diff to the turn — ход несёт блок `<diff>` с `git diff HEAD` сразу после `<genome>`: до 8 файлов, 120 строк, 6000 байт (`crates/titi-engine/src/difftrack.rs`), файлы с секретными именами и куски с секрето-образными присваиваниями выбрасываются целиком, пути диффа вливаются в touched-набор, который читает карта, а duck-режим не получает ни карты, ни диффа;
- `4ae8574` chore(genome): depend on serde_json for LSP framing и `88630fb` docs(config): finish the genome.limit doc — зависимость и доки.

Проверено в этом раунде: `cargo test -p titi-cli --test privacy` 14, `--test genome` 13, и весь набор на этом коммите — см. гейты в отчёте раунда. `docs/ARCHITECTURE.md` описывает оба глагола и блок `<diff>`.

## OAuth-вход в провайдера (2026-09-28, живой вход Codex пройден)

Задача владельца: «исправить логин и oauth, чтобы OAuth был как в omp». Ресерч закрыт: [providers-streaming/oauth-login.md](providers-streaming/oauth-login.md) — точные authorize/token-endpoint'ы, scopes, callback, PKCE, refresh-skew, идентичность и инференс-заголовки Anthropic/Codex, разобранные по исходникам omp (`@oh-my-pi/pi-catalog`, `@oh-my-pi/pi-ai`, MIT). Ответ на «забрать из их кода»: забраны дескрипторы (client-id, URL, scopes, beta-заголовки — факты протокола, MIT с атрибуцией), движок — нет (TypeScript/Bun + KDL-компиляция); порт — таблица констант Rust и свой поток поверх неё.

Сделано в `c3a57ef`: `titi-providers::oauth` (PKCE S256 из 96 байт → base64url, callback на `TcpListener` с разбором `code`/`state`/`error`, 404 на чужой путь, `port_fallback #false` у Codex, обмен кода JSON у Anthropic и form у Codex, идентичность Anthropic через bootstrap-запрос и Codex через claim'ы JWT, refresh, device-grant Codex; base64url и percent-encoding свои, из зависимостей только `sha2`); `auth.db` v3 с `refresh_token`/идентичностью и миграцией из v2; провод по `CredKind` (`Authorization: Bearer` + Claude Code отпечаток у Anthropic, `chatgpt-account-id`/`openai-beta` у Codex); движок — свеп `refresh_due` по skew перед ходом (терминальный отказ refresh удаляет строку, транзиентная ошибка оставляет); поверхность — `titi --login [provider]` (и `--login --device <id>`), `/login <provider>` в чате с печатью URL и вставкой кода, `/keys` с видом и остатком жизни.

Тесты на момент коммита: `titi-providers` 151, `titi-secrets` 24, `titi-engine` 231, `titi-cli` 266; workspace 1464 passed, 0 failed. Смоук без сети и ключей: `titi --login` печатает `anthropic  Anthropic (Claude Pro/Max)` и `openai-codex  ChatGPT Plus/Pro (Codex Subscription)` и выходит 0; `titi --login nonexistent` печатает `unknown oauth provider nonexistent` и выходит 2; `titi --help` называет `--login`.

Живой прогон владельца (2026-09-28): `titi --login openai-codex` в настоящем браузере довёл вход в ChatGPT Plus/Pro до конца и записал подписку в реальный `~/.titi/agent` (`--list-keys` → `openai-codex  (oauth, 239h 59m left)`), и ход модели на `openai-codex/gpt-5.5` в TUI (PTY 80×20) ответил `42` без ошибок. Живой вход Claude не проверен: подписки на этой машине нет, `--login anthropic` доходит до callback, печатает URL и отвергает любой подставленный код как `invalid_grant` — это отсутствие подписки, а не дефект кода.

Прогон вскрыл три дефекта, не исполнявшихся ни одним тестом (их правит `092be78`):

- массив `tools` для `OpenAiResponses` отправлялся чатовой формой (`{"type":"function","function":{…}}`, `openai_tools_wire`), и подписочный бэкенд Codex отвечал `400 {"error":{"message":"Missing required parameter: 'tools[0].name'"}}`; инструменты есть в каждом режиме (plan читает, duck ищет), поэтому ни один ход на токене не проходил. Теперь у Responses своя плоская форма `responses_tools_wire` (`name` рядом с `type`), у чатовых эндпоинтов остаётся `openai_tools_wire` — две семьи не могут делить один сериализатор (`crates/titi-providers/src/wire.rs`);
- loopback-callback слушал только одну адресную семью: `TcpListener::bind(("localhost", port))` брал первый адрес и держал лишь `[::1]:54545`, так что `curl 127.0.0.1:54545/callback?…` получал `curl exit 7`. Дескриптор называет хост, а не семью, и какую выберет браузер — не знать, поэтому теперь на одном порту слушают все семьи, которые разрешает хост, первый редирект снимает остальные, а фолбэк порта двигает семьи вместе и `port_fallback: false` у Codex остаётся ошибкой конфигурации (`crates/titi-providers/src/oauth/callback.rs`);
- голый `/login` печатал ростер провайдеров и оставлял выбор в памяти. Теперь это пикер подписок — строка на провайдера и метод (`browser`, `device code` там, где дескриптор его объявляет), `/login <provider> [key|device]` — грамматика, device-флоу доступен из чата, композер в нём не просит код, а провайдер без API-ключа отвергает inline-ключ (`crates/titi-cli/src/{chat.rs,login.rs,main.rs}`).

Заодно не-2xx теперь несёт собственную формулировку провайдера (`error.message`/`code` или голое `message`; тело читается лимитированно и по таймауту, одна строка, креды маскируются, текст режется) вместо `upstream status N` (`crates/titi-providers/src/wire.rs`) — именно это сделало тот `400` читаемым.

`SwitchModel` починен (`459f7dc`): команда меняла модель и не эмитила события движка (`crates/titi-engine/src/runtime.rs:766`), поэтому headless-JSONL клиент не мог переключить модель перед ходом, а `crates/titi-cli/src/headless.rs:99` ждал терминального события и висел — `printf … | titi --headless` стоял до таймаута клиента. Теперь движок отвечает `EngineEvent::ModelSwitched { turn_id: Option<TurnId>, from, to }`: standalone-переключение шлёт `None` (у переключения вне хода нет хода, который можно назвать), фолбэк посреди хода — `Some(id)`, и вариант остался один, чтобы поверхность, которой нужна только пара `from`/`to`, читала оба случая одинаково; arm отвечает и когда модель не меняется, чтобы клиент, ждущий исхода, не висел на no-op. `headless::run` читает stdin своим потоком, качает события движка, пока ждёт следующий кадр, и завершает прогон на EOF, когда в полёте нет джоба; неотвечающие команды (`Steer`, `RestoreHistory`, `Cancel` без хода, `ApproveTool`) описаны в доках модуля и README.

Пикер моделей и живой признак хода (`1aba3fa`, `crates/titi-cli/src/chat.rs`): голые `/model` и `/switch` больше не крутят модель по кругу, а открывают браузер — строки сгруппированы по провайдеру, роли (`modelRoles`) идут первыми, текущая модель помечена и прокручена в вид, ввод фильтрует по подпоследовательности, показывая запрос и счёт, длинный каталог окнится строками `… N more`, а строка несёт то, что о модели известно: окно контекста, если дескриптор его объявляет, и метку креда провайдера. Подтверждение переключения теперь ровно одно — обработчик `ModelSwitched` (он же присваивает `self.model = to`): командная и пикерная стороны свои заметки не печатают, потому что раньше отказ движка всё равно объявлялся, а состоявшееся переключение печаталось дважды. Мастхед во время хода показывает только слово `working` — глиф и секунды ушли в строку прогресса над композером (`d590578`, абзац ниже), — а на узком экране правая половина сначала теряет метку сессии и только потом режет модель по месту, с многоточием (обрезанный id сессии всё ещё называет файл, `open -codex/gpt-5.5` — ничего). `/keys`, `/diagnose` и метка в пикере читают кред одним местом (`Credential::of`: окружение, потом хранилище), вывод не изменился. Проверено в PTY (80×20, 60×20, 120×30, pyte), не глазами: `docs/QA_STATUS.md`.

Строка прогресса над композером (`d590578`, `crates/titi-cli/src/chat.rs`): фазы хода видно там, где человек только что напечатал промпт, а не в мастхеде на другом конце экрана. `WorkPhase` — это `Waiting` (с отправки промпта и со старта хода), `Streaming` (первая текстовая дельта; дальше между раундами фаза остаётся `Streaming`, чтобы раунд после инструмента не читался как первое ожидание), `Thinking` (дельта рассуждения) и `Tool { call_id, name, since }` — со своими часами, и снять строку может только тот вызов, что ею владеет: поздний `ToolFinished` чужого вызова не трогает часы работающего. Рисуется это как `⠋ waiting for the first token · 3.2s`, `⠋ streaming · 4.1s · 320 chars`, `⠋ thinking · …`, `⚙ bash · 1.2s`, `⚠ needs you · bash`. Правило «один факт — один раз»: мастхед оставил себе слово состояния, режим и модель, а глиф и секунды отдал строке — две строки, анимирующие одни часы, только мешают заметить застой. Строки нет, когда ничего не идёт: `Constraint::Length(u16::from(status.is_some()))` даёт ноль, а виджет не рисуется, поэтому простой экран остаётся построчно прежним (проверено вживую в PTY 80×20 на временном agent-каталоге, без сети: мастхед ` titi  ready  plan … bai/glm-5.3-flash  00000001780c` — без глифа и секунд, композер — последние четыре строки 16–19). Предел: живой кадр с инструментом (`⚙ bash · 1.2s`) в TUI ещё не снят — инструментальный ход на подписке починен абзацем ниже (`c2280f1`), и после него кадр снимаем, — поэтому фазу инструмента пока пинят только тесты через `TestBackend`; в `docs/QA_STATUS.md` строка `partial`.

Модернизация чат-экрана по плану `agent-ux/ui-modernization.md` §4 отлендилась целиком — восемь шагов: 1) цвета берутся с темы, самодельный `Ink` удалён (`ecaf37a`); 2) markdown ответа — рендерер в `titi-tui` (`c397449`) и провод в чат (`af44ab1`); 3) диффы тулов — рендерер с гуттером (`ad3f8b4`), `detail` у `edit`/`write` (`7b4bce2`) и чип, который его рисует (`6c3dfac`); 4) шапка из левой и правой групп с зарезервированными слотами (`68e7786`); 5) строка прогресса над композером с фазами `WorkPhase` (`d590578`) и описанием вызова в ней (`df9498f`, `432afca`); 6) одна геометрия отступов и фон под репликой пользователя (`7acec68`, `87d6f70`) плюс поверхность для пресета по умолчанию (`0c42577`); 7) панели и скроллбар (`b569a98`, `4e713db`, `f520e8b`) и переключатель сессий по `Ctrl+X` вместе с починкой лога сессии (`10fef55`); 8) заставка (`636c207`) и правило «факт целиком или не вовсе» в ней (`d0b2ecb`). База на `552323d`: workspace 1625 passed, 0 failed, 1 ignored (62 таргета), снято в detached-worktree — в той самой среде, которая и поймала обрезанный факт в заставке. Выбор темы к этому добавился отдельно (`761137e`, `2f97a29`, `d115eeb`): в настройках две ключа, `theme.dark` и `theme.light` (`crates/titi-config/src/settings.rs`), `--theme <name>` выбирает палитру на прогон и отвергает незнакомое имя с кодом 2, `/theme` открывает пикер (`╭─ themes · 101 ─…╮` с `▶ auto  ✓ current`) и помнит выбор в том слоте, который выбрала сама терминальная площадка по фону терминала; `auto` остаётся умолчанием и означает отсутствие выбора, а не палитру.

Пределы честные: фон под репликой пользователя на пресете по умолчанию поднят (`552323d`: `userMessageBg` = `deepTitanium` `#323841` вместо `brushedTitanium` `#151820`) — теперь это 1.59:1 против страницы (`#323841` на `#0f1216`) при 9.39:1 для текста на нём, а против карточки композера 1.13:1, то есть порядок намеренный — страница (0.0059 по люме), карточка (0.0289), фон реплики (0.0389), самый сильный слой под словами пользователя, но различать эти две поверхности по одной яркости всё ещё не стоит; ещё шесть shipped-пресетов схлопывают `userMessageBg` в `statusLineBg` (dark-gruvbox, dark-poimandres, light-poimandres, light-tokyo-night, obsidian, onyx), а девять дают двум из трёх цветов строки прогресса один и тот же токен (пять — `accent`==`toolTitle`, четыре — `accent`==`warning`) — это выбор самих палитр, не дефект кода. Дифф-деталь (`ToolResult.detail`) в лог сессии сознательно не пишется: канал один (`EngineEvent::ToolFinished.detail`), а персистентность — отдельная работа, и её место — формат сессии в `titi-core`.

Инструментальный ход на подписке ChatGPT починен (`c2280f1`): промпт с намерением вызвать инструмент не давал ни одного вызова — `TurnStarted → ContextUsage → TurnUsage(completion_tokens 0) → TurnFinished{stop}`. Причина в декодере Responses (`crates/titi-providers/src/openai.rs`): блок инструмента открывался только по кадру дельты аргументов и требовал в нём `name`, а бэкенд Codex называет вызов один раз — в `response.output_item.added`, — и кадры аргументов у него не несут ни имени, ни call id, только `item_id`; поэтому имя всегда было пустым, блок не открывался, а все фрагменты аргументов молча выбрасывались. Теперь блок открывается по `output_item.added`, фрагменты `response.function_call_arguments.delta` дописываются в блок по `item_id` (или `output_index`, или в вызов в работе), а закрывается он на `…arguments.done`/`output_item.done`; целые аргументы, пришедшие только в done-кадре, тоже дают вызов, и старый порядок first-party (имя в дельте) продолжает работать. Вторая ошибка того же класса — в `crates/titi-providers/src/wire.rs`: реплей ассистентских вызовов шёл в чатовой форме, и бэкенд отказал обоими половинами буквально — `Invalid value: 'tool'. Supported values are: 'assistant', 'system', 'developer', and 'user'` и `No tool call found for function call output with call_id call_…`; теперь `responses_input_wire` шлёт вызовы как `function_call`, а результаты как `function_call_output`, паруя их по `call_id`. Живой прогон на подписке (`--headless --mode plan`, тот же промпт про `Cargo.toml`): `ToolStarted{name:"read"}`, `ToolFinished` с содержимым файла, ответ `The package version value is 0.1.0.` и `TurnUsage(completion_tokens 10)`. Отсюда следует, что причина, по которой в `docs/QA_STATUS.md` не было живого кадра `⚙ bash · 1.2s`, снята — кадр теперь можно снять, и строку там стоит перепроверить.

Остаётся одно наблюдение (до сегодняшнего дня, не регрессия):

- `crates/titi-cli/src/app.rs` и стек оверлеев `titi-tui` недостижимы из продакшн-бинарника: он запускает `chat::run` (`crates/titi-cli/src/main.rs:292`).

Открытые недочёты (строки по `1aba3fa`):

- `crates/titi-cli/src/app.rs:930-932` печатает `model fallback: {from} → {to}` и для standalone-переключения, где слово «fallback» неверно (сегодня эта поверхность из продакшн-бинарника не достижима);
- `titi --login --device` печатает браузерную формулировку вместо device-инструкций: `on_auth` (`crates/titi-cli/src/login.rs:173`) и `auth_notice` (`:190`) всегда дают «Open this URL in your browser:» и «Paste the authorization code…», и `run_login_device` (`:230`) берёт тот же `TerminalUi::new()` (`:238`). Заготовленный патч лежит в `/tmp/titi-device-wording-parked.patch` и перезагрузку не переживёт, поэтому он же словами: у `TerminalUi` (`:131`) появляется поле `device: bool`; `TerminalUi::new()` зовёт `Self::for_flow(false)`, новый `TerminalUi::device()` — `Self::for_flow(true)`, а `for_flow(device: bool)` держит общий код читающего stdin потока; `on_auth` при `device` печатает `device_auth_notice`, иначе прежний `auth_notice`, и в обоих случаях зовёт `open_browser(url)`; `device_auth_notice` = «Open this URL on your other device:» + `osc8_link(url)` + пустая строка + `instructions` — URL остаётся одной OSC 8-ссылкой (`titi_tui::caps::osc8_link`), промпта вставки нет, а `instructions` несут `Enter code: …`; `run_login_device` берёт `TerminalUi::device()`. Тесты: хелпер `assert_single_osc8_link` (в заметке ровно одна пара open/close и URL внутри), `the_browser_notice_asks_for_a_pasted_code`, `the_device_notice_carries_the_code_without_a_paste_prompt` (в device-заметке есть `Enter code: …` и нет промпта вставки).

Тесты после правок (локально, `--locked`): `titi-cli` 276, `titi-providers` 162, `titi-engine` 231, `titi-secrets` 24; workspace 1485 passed, 0 failed; `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` и `cargo check --workspace --all-targets` чисты (у clippy только прежние `unwrap`/`expect` в тестовых модулях).

Ссылка входа теперь кликабельна и не боится переноса: authorize-URL печатается как OSC 8-гиперссылка, а каждое ребро завёрнутого URL несёт URL целиком, поэтому обрыв строки больше не оставляет нечего кликать (раньше жестянка рвала URL посреди токена и вешала на него свои отступы). В TUI это `paint_links` (`crates/titi-cli/src/chat.rs`): span не может нести управляющие символы — ratatui выкидывает их, пока заполняет буфер, — поэтому ESC-последовательности пишутся прямо в ячейки уже нарисованного ребра с `CellDiffOption::ForcedWidth(1)`, а `wrap_url` режет URL по рёбрам без потери байта; в `titi --login` тот же URL оборачивает `caps::osc8_link` (`crates/titi-cli/src/login.rs`, `crates/titi-tui/src/caps.rs`), тоже `3feab86`. Предел честный: проверено на байтовом уровне PTY (80×20, 6 рёбер по 456-символьному URL, каждое с полным URL как target, видимый текст сходится в URL байт в байт; и `/login`, и `--login`), клик мышью в GUI-терминале не проверялся; терминал без OSC 8 покажет просто весь URL текстом.

Две косметики, которые эта правка не тронула: мастхед (`crates/titi-cli/src/chat.rs:3138-3158`) режет id модели посимвольно без многоточия — живой кадр показал `open -codex/gpt-5.5`; `titi --login` в device-флоу печатает браузерную формулировку (`crates/titi-cli/src/login.rs:173-196`) вместо device-инструкций.

| Шаг | Статус | Где |
|-----|--------|-----|
| Ресерч omp (движки oauth-code/device/refresh, дескрипторы, хранилище, CLI-UX, инференс-заголовки) | done | [oauth-login.md](providers-streaming/oauth-login.md) |
| План среза (крейты, типы, тесты, DoD) | done | `oauth-login.md`, секции «Rust-маппинг» и «Definition of Done» |
| Реализация: `titi-providers::oauth` (PKCE, callback, обмен, refresh, дескрипторы anthropic/openai-codex) | done, `c3a57ef` | `crates/titi-providers/src/oauth/{mod,pkce,callback,encode,device,provider}.rs` |
| Хранилище v3: `refresh_token`, идентичность, миграция из v2 | done, `c3a57ef` | `crates/titi-secrets/src/store.rs` |
| Провод: авторизация по `CredKind` (Anthropic OAuth → `Authorization: Bearer` + `anthropic-beta` + отпечаток Claude Code; Codex → `chatgpt-account-id` + `openai-beta` и тело подписочного бэкенда) | done, `c3a57ef` | `crates/titi-providers/src/wire.rs` |
| Свеп refresh по skew перед ходом, карантин отвергнутого refresh-токена | done, `c3a57ef` | `crates/titi-engine/src/registry.rs` |
| Поверхность: `--login [provider]`, `--login --device <id>`, `/login <provider>` с callback+вставкой кода, `/keys` c oauth | done, `c3a57ef` | `crates/titi-cli/src/{main.rs,chat.rs,secrets.rs,login.rs}` |
| Живая проверка владельцем: вход Codex (браузер + подписка) | done | `docs/QA_STATUS.md` |
| Живой ход на подписке: `openai-codex/gpt-5.5` ответил, плоская форма tools у Responses | done, `092be78` | `crates/titi-providers/src/wire.rs` |
| Callback слушает все семьи loopback на одном порту | done, `092be78` | `crates/titi-providers/src/oauth/callback.rs` |
| Голый `/login` открывает пикер подписок, device-флоу из чата, inline-ключ у провайдера без API-ключа отвергнут | done, `092be78` | `crates/titi-cli/src/{chat.rs,login.rs,main.rs}` |
| Не-2xx несёт сообщение провайдера вместо `upstream status N` | done, `092be78` | `crates/titi-providers/src/wire.rs` |
| Живая проверка владельцем: вход Claude (браузер + подписка) | blocked (нет подписки) | `docs/QA_STATUS.md` |

NEXT: остаётся нерешённым одно наблюдение выше (`app.rs` + оверлеи `titi-tui` недостижимы из бинарника) и два открытых недочёта (`model fallback` для standalone-переключения в `crates/titi-cli/src/app.rs:930-932`; браузерная формулировка в device-флоу `titi --login --device` — `crates/titi-cli/src/login.rs:173`/`:190`/`:238`, патч в `/tmp/titi-device-wording-parked.patch`); модернизация чат-экрана по `docs/research/agent-ux/ui-modernization.md` §4 отлендилась целиком (восемь шагов, хеши — в абзаце выше), а после `c2280f1` можно снять живой кадр `⚙ bash · 1.2s` и перепроверить строку в `docs/QA_STATUS.md`. Из живых проверок нужны подписка/время: вход Claude владельцем и свеп refresh на живом токене. Кода срез не требует: device-flow и Codex-инференс (`/backend-api/codex/responses`) уже в master, вне его остались auth-broker (`titi creds serve`), мульти-аккаунты с бэкоффом и импорт кред Claude Code/Codex CLI.

## Ход, история и промпт до провайдера (2026-09-23, вторая половина)

| Что | Коммит | Статус |
|-----|--------|--------|
| Системный промпт вообще не доезжал до Anthropic и Gemini: движок шлёт его головным `Role::System`, обе семьи несут его верхнеуровневым полем, а `WireRequest::system` в продакшене не заполнял никто. Модель работала без identity, `AGENTS.md`, скиллов и карты репозитория; сжатая сессия теряла ещё и дайджест компакции. OpenAI не задет | 7190897 | done, +3 теста |
| Законченный ход не возвращался в следующий запрос: ход N+1 не видел хода N. История стала append-only, эпоха истории защищает от гонки с `/rewind` | b214c1f | done, +3 теста |
| Промпты в очереди за отменённым ходом залипали и выстреливали позже, отвечая на заброшенный вопрос. Возвращаются через `EngineEvent::PromptReturned` в композер, если он пуст, иначе в транскрипт | 70557cf, 7e44bca | done, +3 теста |
| Отменённый ход успевал открыть ещё один оплаченный запрос к провайдеру | a8820fb | done, +1 тест |
| Вызовы и результаты инструментов не переживали перезапуск: `store.rs` их не писал. Теперь пишутся; окно восстановления режет по границе раунда, а битый раунд (отмена посреди инструмента) лечится на месте, а не отрезает всю историю после себя | 80e752d, 2b5dd11, 7d126e5 | done, +4 теста |
| Вставка скилла из сообщения: `/` в композере предлагает команды и скиллы, выбор вставляет `/<имя>` в любом месте строки, движок разворачивает токен в тело `SKILL.md` (headless тоже). Тело проходит сканер инъекций, кап 32 KiB, отказ виден через новое нетерминальное `EngineEvent::Notice`. Третий корень поиска — `.agents/skills`, где скиллы проекта и лежат | 9b52269, 954c0d3, 8188323 | done, +14 тестов |
| `expect()` в продакшене (`compat.rs::clamp_effort`) — единственное нарушение AGENTS среди 240 предупреждений clippy, остальные в тестовых модулях | 417abc7 | done |
| Тест авто-темы падал по расписанию: утверждал о процессно-глобальном реестре, общем для всего интеграционного бинарника | eaaf622 | done |

Исследование кэша промпта: [`prompt-cache.md`](prompt-cache.md) (6ef69fa). Ключевой замер — рендер карты репозитория с пустым и непустым списком тронутых файлов совпадает ровно одной строкой, поэтому любой `read` ломает префикс на следующем ходу. Решение A (заморозить системный промпт на сессию) или B (разрезать на стабильную и переменную части) — за владельцем, код не начат.

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

## Audit follow-ups — 2026-10-08 (todo queue)

Source: `docs/audits/2026-10-08-general.md` (areas 1, 2, 5–10),
`docs/audits/2026-10-08-security.md` (areas 3–4); numbers from
`docs/audits/2026-10-08-facts.md` (HEAD `3aa6774`, CI green, 1805 passed /
0 failed). Synthesis and severities: `docs/BRAIN.md`. Pointers are the ones
the audits gave; nothing here is re-verified by this list.

**Five fixes were already in flight while the audit ran — all five have since
landed, so none of them needs starting again.** The destructive test and the
unisolated test chat are in `899c89a`, the diff-block credential leak and its
`<diff>` frame in `af985e1`, the genome checker's `syntax-error` /
`unresolved-import` false positives in `e7c48e1` + `fe1abf2`, the masking gaps
in `8b33ed9` (with the bearer-header prefilter in `d24f5b1` and the test pin in
`98d40bb`), and the file-lock helper in `cbb8abd`.

- [x] `critical` ~~a test deletes the developer's real stored provider key —
  `crates/titi-cli/src/chat.rs:10969` (`/logout openai` → real
  `~/.titi/agent/auth.db`; `chat.rs:539` + bare helper `chat.rs:7295-7299`)~~
  fixed 2026-10-08 in 899c89a.
- [x] `critical` ~~the `git diff HEAD` block leaks credential files the read
  tools refuse — `crates/titi-engine/src/difftrack.rs:279-288`,
  `runtime.rs:179-209,1816`~~ fixed 2026-10-08 in af985e1.
- [x] `high` ~~tool-output masking misses quoted and `_`-suffixed key names —
  `crates/titi-memory/src/redact.rs:349-370`~~ fixed 2026-10-08 in 8b33ed9,
  with the bearer-header prefilter in d24f5b1 and the test pinned in 98d40bb.
- [x] `high` ~~error lints are inert in 8 of 11 crates — `Cargo.toml:38-43`;
  blocked by `crates/titi-cli/tests/login_oauth.rs:416,430` (`unsafe
  env::set_var` under `unsafe_code = "forbid"`)~~ fixed 2026-10-08 in 9b9ef2f
  (the workspace lints now apply to every member) after 13e4fa1 dropped the
  unsafe env hand-off that blocked it.
- [x] `high` ~~session files are written without `fsync` and rewritten in
  place — `crates/titi-core/src/session/store.rs:102-107,215,262,281`~~ fixed
  2026-10-08 in e4771e8 (a synced temp file), with the trajectory tail repaired
  before appending in 88386ac and its torn tail told from a damaged line in
  e3906be; `export_to_file` is the one in-place write left (row below).
- [x] `high` ~~the `fd-lock` helper runs the guarded write after a failed acquire
  and unlinks the lock — `crates/titi-config/src/config_file.rs:176-185`~~
  fixed 2026-10-08 in cbb8abd.
- [x] `high` ~~a stalled provider hangs the turn and `Cancel` cannot unblock it
  — `crates/titi-providers/src/transport.rs:190-212`, `http.rs:53-55`,
  `wire.rs:770-777`, `runtime.rs:2159-2165`~~ fixed 2026-10-08 in 3d179d2 (the
  http client gets a timeout), 46fa805 (the declared first-event and idle
  windows are applied and `Stalled` is produced in production) and 0169bbb (a
  cancel ends a silent read).
- [x] `high` ~~`titi_cli::app::App` (2492 lines) is unreachable from the binary
  and kept alive by five integration test files~~ fixed 2026-10-09, delete
  chosen over wire-in: phase 1 moved the live helpers to
  `session_fs.rs`/`themes.rs` (7ace4a6, 27dc55c, 54 call sites repointed), and
  phase 2 deleted the struct (`9364bb4`, `app.rs` 2129 lines), the eleven
  `titi-tui` modules only it used (`1ff6889`, 5315 lines) and the seven test
  files that only drove it (`4f13833`). `selection.rs` and `space_hold.rs`
  were kept on purpose and are ported (item below); the memo records what was
  recoverable and where.
- [x] `medium` ~~raw tool arguments are persisted unmasked —
  `crates/titi-engine/src/tool_loop.rs:167-171`,
  `crates/titi-core/src/trajectory.rs:99-107`~~ fixed 2026-10-08 in 907f821
  (masked before the trajectory sees them, and the file locked down).
- [x] `medium` ~~a cloned repository's `.env` outranks the user's credential
  layers — `crates/titi-secrets/src/env.rs:37-48`~~ fixed 2026-10-08 in
  d85614c.
- [ ] `medium` stringly-typed errors and discarded writes at the CLI seam —
  `crates/titi-cli/src/session_log.rs:25`, `headless.rs:198,227,252,292,314,336`,
  `headless.rs:218,270,329`.
- [x] `medium` ~~the retry can replay thinking deltas —
  `crates/titi-providers/src/stream.rs:140-148`,
  `crates/titi-engine/src/runtime.rs:2002-2005`~~ fixed 2026-10-09 in
  8ba2604: `StreamEvent::is_visible_output` puts answer text, tool arguments
  and reasoning on the visible side of the retry gate — the surface has
  already painted them, so a second attempt would stream its own reasoning
  into the same block and the transcript would read the previous attempt's
  thinking twice.
- [x] `medium` ~~`fallback_chain` / `fallback_cooldown` are dead config —
  `crates/titi-engine/src/runtime.rs:489-491,530-533`,
  `crates/titi-config/src/fallback.rs:46-49`~~ fixed 2026-10-09 in 25235e5:
  both files deleted outright (−732 lines) rather than documented, because
  the settings were parsed into a value no code read — per-role ordered
  chains and a cooldown-based revert to the primary never affected routing.
  The routing that does work stays: `fallback_models`, which `titi-cli` fills
  from its resolved model list, is exercised by
  `falls_back_after_transient_budget`; `fallback.chains` is now simply an
  unrecognized key.
- [x] `medium` ~~a panic in the alternate screen erases its own message — no
  `set_hook`; `crates/titi-cli/src/chat.rs:3362-3370`~~ fixed 2026-10-09 in
  d913076: a hook installed before the screen opens restores what
  `Screen::Drop` restores (raw mode, the alternate screen, the cursor, focus
  reporting) and then prints one plain line to stderr, so the message
  survives; the hook neither exits nor swallows the panic, which still
  unwinds to 101. Verified on a PTY (below).
- [x] `medium` ~~the default test chat is not isolated —
  `crates/titi-cli/src/chat.rs:539,7295-7299`~~ fixed 2026-10-08 in 899c89a.
- [ ] `medium` wall-clock assertions that can flake —
  `crates/titi-cli/tests/first_frame.rs:58,79-82`,
  `crates/titi-tools/src/pipe.rs:306`, `pty.rs:423`. The fourth file the
  audit named, `crates/titi-engine/tests/tools.rs:953`, is gate-driven since
  2cce9f3: the cancel test waits on the trap the turn actually sets instead
  of on a sleep, so a slow machine cannot fail it. A fifth instance flaked on
  CI and is fixed: `chat.rs`'s `the_frame_paints_each_state_with_its_own_token`
  rendered twice with a needle carrying the clock and the spinner — `59ceebf`
  (2026-10-09), which strengthens the assertion rather than relaxing it.
- [x] `medium` ~~the headless JSONL protocol is serde-tested on one sample —
  `crates/titi-cli/tests/headless.rs:8-20`~~ fixed 2026-10-09 in 6e97627:
  `EngineCommand` and `EngineEvent` are `#[non_exhaustive]` now, and
  `crates/titi-engine/tests/protocol.rs` builds one value per variant,
  serializes it against a hand-written JSON fixture and decodes the fixture
  back, so a renamed, dropped or reordered field fails there; both optional
  shapes are pinned (`detail` absent vs present, `Consult.question` as `Some`
  and `None`).
- [x] `medium` ~~the README slash table is wrong in three ways —
  `README.md:99,174,303,378` (`/skillful` does not exist; `/council` and
  `/graph` do)~~ fixed 2026-10-08 in 268a159, which took the header test count
  and the verify block with it.
- [x] `medium` ~~nine tree-sitter grammars are justified and ABI-14 verified but
  not added — go, java, c/c++, c#, kotlin, php, ruby and swift; every one must
  emit ABI 14, the range the workspace's `tree-sitter 0.24` accepts, because
  newer releases emit ABI 15 and would need a core bump
  (`crates/titi-genome/Cargo.toml:12-14`, guarded by
  `grammars_match_the_core_abi`)~~ all nine added 2026-10-09 in 9c81d63 (Go),
  ea73eec (Java), 583f27c (C and C++), a43ac34 (Kotlin), 2e46e36 (C#), 918604a
  (Ruby), ddee4be (PHP) and d658a45 (Swift), each with its grammar crate, its
  `Cargo.lock` entry and its `tests/lang_<lang>.rs` in the same commit; d81a6cf
  dropped the pattern helpers no language uses and f4c5799 documents what each
  level names.
- [ ] `low` the workspace lints surface **13** `unwrap`/`expect` warning headers
  as of 2026-10-09 (`cargo clippy --workspace --all-targets`, measured on this
  tree). The rule the item asked for is in place: all eleven crates carry a
  crate-level `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`
  (`b7396f9` config, `23ea652` core, `0e71d27` providers, `00908d8` tools,
  `483806c` soul, `a8b7cc2` secrets, `ebaaf15` memory, `22614cf` genome,
  `ad76f7c` engine, then `b63140a`, `3315395` and `e0be096` for cli and tui),
  which took the count from 1304/1477 by silencing the test-only noise. The two
  test files that sweep had missed are fixed too, in `3118e96`
  (`crates/titi-cli/tests/continue.rs`, whose old blanket
  `#![allow(clippy::unwrap_used)]` stood at line 11, and
  `crates/titi-engine/tests/protocol.rs`, which had **no header line at all** —
  cite it as "no header", there is no line to name; its four headers are gone
  with it. What remains, by file, is what a fix pass would take:
  `titi-genome/src/shared.rs` 5, `titi-memory/src/redact.rs` 4,
  `titi-genome/src/refs.rs` 3 (the kept regex `.expect`s at
  `src/refs.rs:260-264`, the invariant-impossible case `22614cf` argued for)
  and `titi-tui/src/theme/mod.rs` 1.
  **All five in `shared.rs` are deliberate**: they are the poisoned-lock panics
  documented at `shared.rs:59-65` ("a parser panic is a bug in this crate, not a
  condition to paper over with a possibly torn graph") — the `"genome lock
  poisoned"` expects at `:88`, `:102` and `:136` and the `"genome stats lock
  poisoned"` pair at `:120`/`:124` that arrived with `last_stats` in phase 1/2 —
  so do not "fix" them into a silent empty graph.
- [x] `medium` ~~`selection.rs` and `space_hold.rs` are kept while nothing
  calls them~~ ported into `chat.rs` 2026-10-09 in 92c08c8, with the status
  `app-rs-decision.md` keeps: `selection.rs` is live again (a press anchors,
  a drag moves the far corner, a release copies, the frame paints the region
  with `Selection::apply_background` and the text comes from
  `Selection::text` — every part of the module has a caller), and
  `space_hold.rs` is half live: `delete_before_cursor` is behind
  alt+backspace / ctrl+w in the composer, while `SpaceHold` itself — the
  push-to-talk detector — stays dead on purpose, because a hold-space key
  that opened a microphone nothing listens to would be a key that does
  nothing; whoever lands STT wires it to the live composer in one commit or
  deletes it in one. History search is ported with them; hub revive/stop, the
  details accordion and keybinding customization remain queue items,
  recoverable from git history at `65a2f8b`.
- [ ] `low` five direct dependencies are unused or over-declared —
  `crates/titi-tui/Cargo.toml:8,13`, `crates/titi-tools/Cargo.toml:17`,
  `crates/titi-core/Cargo.toml:10`, `crates/titi-cli/Cargo.toml:25` vs `:29`.
- [x] `low` ~~a module-wide `#![allow(clippy::expect_used)]` hides a
  caller-supplied pattern — `crates/titi-genome/src/lang/mod.rs:17` (the file
  the audit named, `src/parse.rs:3,553`, became `lang/` in `714824d`)~~ fixed
  2026-10-09 in `1cf904a`: the four regex literals now call one
  `support::literal_regex`, which is the single place a literal pattern is
  compiled and the single **item-level** allow, with the why on the allow line
  (a pattern written in this source has no input that can make it fail). The
  invariant is held by a test that parses a file through each module holding a
  pattern.
- [x] `low` ~~the lenient JSONL reader launders mid-file corruption into
  missing entries — `crates/titi-core/src/session/store.rs:434-438`~~ fixed
  2026-10-08 in ffb447d (a corrupt middle line is surfaced, not dropped).
- [ ] `low` no schema version in either SQLite store or the session entry —
  `crates/titi-core/src/session/index.rs:71-88`,
  `crates/titi-secrets/src/store.rs:219-234`,
  `crates/titi-memory/src/index.rs:106-108`; the session index is stamped
  since c8971a7, the two SQLite stores are not.
- [ ] `low` the README's local verify commands are weaker than CI —
  `README.md:205-208,409-412`.
- [ ] `low` the engine boundary is guarded by a manifest substring test —
  `crates/titi-tui/tests/dependency_rule.rs:14-27`.
- [x] `low` ~~`write` can plant code that runs later (`titi-tools/src/fs.rs:469`;
  `.git` is inside the jail)~~ the `.git` half fixed 2026-10-09 in b494564:
  `refuse_git_dir` rejects any write, edit or hashline path carrying a `.git`
  component inside the jail and names it, because hooks, `config` or
  `fsmonitor` there are executed by the next git command the user runs.
  Reading is untouched, and a `.git` that is a plain file (a linked
  worktree's `gitdir:` pointer) is not mistaken for the directory.
- [x] `low` ~~checkpoint commits ignore the `SensitivePolicy`~~ fixed
  2026-10-09 in ca0f406: `snapshot_with_policy` reads the same policy the
  runtime tools read and refuses to commit when a staged path is blocked,
  naming the file and the remedy (`git restore --staged <file>`, or
  allow-list it if it really is not a credential). It refuses rather than
  unstaging in the user's place — silently rewriting the index under the
  cover of a "checkpoint" would drop the file the user believes is saved —
  and when settings cannot load it falls back to the built-in list, never to
  nothing. The tests assert on the commit tree, not on log text.
- [x] `low` ~~the `<diff>` frame around the snapshot is not sanitised —
  `crates/titi-engine/src/runtime.rs:204-209`,
  `crates/titi-engine/src/difftrack.rs:152-158,168-183`~~ fixed 2026-10-08 in
  af985e1.
- [x] `low` ~~`titi genome check` reports false positives on a clean tree —
  `docs/audits/2026-10-08-facts.md` §7 (13 `syntax-error` + 19
  `unresolved-import`)~~ fixed 2026-10-08 in e7c48e1 + fe1abf2; a re-run
  reports 10 `ambiguous-symbol` lines and, since 4c2f465, exits 0 on a clean
  tree (13 before the `App` deletion removed three colliding definitions:
  `Entry`, `AgentKind` and `AgentStatus`); 3624fe8 moved the per-language capability lines out of the findings.
- [ ] `low` `export_to_file` in `crates/titi-core` is the one in-place write the
  durability pass left alone.
- [x] `low` ~~the README header test count is stale — `README.md:9` (`1279
  passed`) against 1805 on CI~~ fixed 2026-10-09 in 18997b6: the cell names
  the run it quotes (`1774 passed (CI run 37780191533)`), summed from that
  run's `test result:` lines, so it is a dated measurement rather than a
  claim about the newest run.

**Product-review follow-ups (2026-10-08).** Source:
`docs/research/product-review.md` — a read-only survey at `3c1bc9e` plus the
wave above, with its own ranked cost table; these are the items it recommends
next, and the two worker notes it could not see yet.

- [x] `medium` ~~the paste collapse and the transcript's section model are
  gone — **a regression from our own `App` deletion, not a feature never
  built**~~ both restored 2026-10-09 in `chat.rs`: `3f86696` brings the
  collapse back with the lost module's own threshold (`6` lines, restored
  rather than chosen — the old `App` passed six) and a one-line
  `[Paste #1 · 8 lines]` marker whose body is swapped in at submit, so the
  model reads the wall while the transcript echoes the marker; `21bc0fb`
  brings the section model back as `/details <section> <mode>` over the
  module's four names, with its DoD defaults (thinking, tools and subagents)
  and one departure — activity stays expanded, because on this surface those
  lines are the answers `/usage`, `/context`, `/jobs` and `/recap` give — plus
  a `▸ folded 14 turns · 22k tokens` divider, collapsed by default.
  Honest notes: the marker's form is new on purpose (one line, no `/` in it,
  so command completion still works on the draft, and a substituted body is
  never scanned again), and `Screen::open` now asks for **bracketed paste** —
  it never did, so no terminal marked a paste and the collapse could not have
  fired even before the deletion. That part was a live bug, not a regression.
- [x] `medium` ~~`/sessions <query>`: the FTS5 index and `SessionStore::search`
  exist, are populated on every append, and have **no caller**~~ the caller
  landed 2026-10-09 in `6d7a89e` (`chat.rs`): a bare `/sessions` opens the same
  rows, title and switch Ctrl+X opens, and `/sessions <query>` offers one row
  per matching entry — the session's title (or its id) and the line that
  matched — narrowing as it is typed (a character re-runs, a backspace takes
  one back, the first Esc clears to the whole list, the second closes) on the
  picker chrome the model, theme and history browsers already use.
  Honest limits: it reaches only what the index holds; both the list and the
  search are **capped at `SESSION_HITS_MAX` = 40** rows; the row's time is the
  session's last write (read from its file, so a hit row and a list row cannot
  disagree), not the hit's own time — `SearchHit` carries only
  `session_id`/`entry_id`/`text`, which is why `SearchHit.ts` is queued below;
  and the search is deliberately unscoped (no bot, no workspace filter), the
  only way a session written before sessions recorded a workspace is findable.
- [ ] `medium` no money anywhere: `/budget $2` is refused with "no price
  table", and neither the turn footer nor `/usage` states a cost. A static
  price on `ModelDescriptor` (`titi-engine/src/registry.rs`), the `$` figure in
  `titi-tui/src/status.rs`'s `TurnFooter` and in `/usage`, and the part omitted
  when a descriptor has no price — a keyless local model must not print
  `$0.000`.
- [x] `medium` ~~sessions record no workspace: `SessionMeta` carries title, bot
  and source but no cwd, so `--continue` and the switcher are agent-directory
  wide~~ landed 2026-10-09 in three commits: `ca61791` adds
  `SessionMeta::cwd`, persisted as `sessions.cwd` and read back by
  `session_meta`; `1fe4bc0` adds `sessions_in(Some(root))` and the same filter
  beside `search`'s bot one, both inside the query's own statement so the
  index narrows; `a77d12f` makes the resume rule `newest_session_in(agent_dir,
  workspace)` and passes the directory titi runs in, so `--continue` resumes
  the row Ctrl+X would have offered rather than the session created last.
  The three details that make it honest: `cwd` is `Option<String>` because
  every older session has none, and `None` means **unknown, never "not
  yours"** — nothing may treat it as a filter result; the v1→v2 migration
  probes `PRAGMA table_info(sessions)` before `ALTER TABLE … ADD COLUMN cwd`
  and stamps `user_version = 2`; and the resume falls back in three branches —
  the scoped pick when the index has one, the newest session anywhere when
  this workspace has nothing recorded, and the newest anywhere when the index
  cannot be read at all — so an old session is never hidden by a filter it
  predates. The store also records the process working directory when its
  caller names no workspace, which is what makes the field live in production
  rather than a value only callers could fill.
- [x] `low` ~~`bash.autoBackground.thresholdMs` is not a setting yet~~ landed
  2026-10-09 in `16753dd`: the key is declared in `titi-config` beside the
  others and read in `titi-cli::engine`, with precedence **environment over
  setting over the 60 000 ms default** — the engine passes it through
  `titi_tools::background_after_with`, which keeps the environment check first,
  so a value already exported for a machine or a test is never overridden by a
  config layer.
- [x] `low` ~~no `/tree` view of a tree that is already stored~~ landed
  2026-10-09 in `141700f`: `/tree` draws the store's entries as the tree they
  are — one row per entry, indented by depth, the path to the leaf marked `•`
  and the leaf named `✓ current`, the title counting what is off the path — and
  Enter moves the leaf to the row under the cursor and replays that path
  (`session_fs::branch_at` forks the store's leaf, walks the new path, and the
  `RestoreHistory` a rewind already sends replaces the engine's history). The
  entries left behind stay in the store, which is what makes it a branch and
  not a rewind. Deliberately not done: omp's `/tree` also filters entries and
  summarises the branch you leave.
- [x] `low` ~~no `/hotkeys`: the screen answers 16 keys and advertises 3, and
  `/help` lists 41 commands one line each. Generate the list from the same table
  the keys are read from, so the two cannot drift.~~ fixed 2026-10-09 in
  `7a7e7bc`: `/hotkeys` prints the five groups (composer, lists & pickers,
  transcript & mouse, turn control, session) from `HOTKEYS` in
  `crates/titi-cli/src/keys.rs`, beside the `on_key` that answers those keys,
  and the slash list advertises it beside `/help`;
  `every_key_the_screen_answers_is_named_in_the_hotkeys_listing` drives every key
  `map_key` can produce through every state `on_key` branches on, so a binding
  missing from the listing fails the suite rather than going unadvertised.
- [x] `low` ~~`chat.rs` is this project's serializer: 17.2k lines, one writer,
  and every UI item above ends in it — which is why they cannot be
  parallelized~~ split 2026-10-09, the review's structural finding taken first:
  `1d4f354` moved the transcript renderer out (1570 lines), `a5779d1` the
  pickers (2103), `9b49145` the key path (583) and `24e51ef` the composer (237),
  each into its own module with `lib.rs` declaring it — about 4,900 lines out.
  `chat.rs` is 14,991 lines after that, still the largest file in the tree, so
  the split is a start rather than a finished story: what remains there is the
  session, the loop and the binding core, and splitting those is its own item.
- [x] `low` ~~the README's first paragraph links the placeholder
  `https://reference-product.com` (`README.md:3` and the verify block), which
  404s for the first stranger who reads it, and there is no `CHANGELOG.md` at
  all~~ fixed 2026-10-09 in `240db13`: the placeholder link is gone from the
  README and `CHANGELOG.md` exists — newest-first, user-facing, in prose about
  what a user sees.

**Follow-ups named by this wave (2026-10-09).**

- [x] `low` ~~the engine's one-line adoption of `SharedGenome`~~ landed
  2026-10-09 in `a4716a6`: `crates/titi-engine/src/runtime.rs` holds
  `genome: SharedGenome` at `:780` (the doc on the field says the tool loop
  writes it and the turn-start walk re-reads it), so the snapshot type phase 0
  built has a production reader. `runtime.rs:117`'s
  `type GenomeIndex = Arc<tokio::sync::Mutex<Option<Genome>>>` is gone.
- [ ] `medium` Taimyr **phase 3 — the watcher and its debounce — is the only
  phase left**, and it stays blocked on the owner's word about `notify` (per
  `docs/research/genome-incremental.md` §4 and §6): adding the dependency goes
  through the `dependency-update` procedure and its health is **unverified
  offline**, which is why nothing was added. Everything else landed on
  2026-10-09 — phase 1 (`eac8a9c`, `3a7aafa`, `b4b1401`), phase 2 (`a4716a6`,
  `aede977`), phase 4 (`5d8d9a7` the background indexer, `dcd962e` the
  `sent`/`applied` pending, `4f455ec` the retried window, `ef304a9` the bounded
  turn wait) and phase 5 (`0a5a8ea`, the LSP's buffer overlay) — and phase 3 now
  has a worker to feed rather than a design to invent.
- [x] `low` ~~a `micro_usd` on `SetBudget` plus a cost accumulated from
  `TurnUsage` in `Runtime::over_budget`~~ landed 2026-10-09, and it is the
  review's "so `/budget $2` becomes possible" actually delivered:
  `36d3aef` added `SetMoneyBudget { micro_usd }` as a **sibling** of
  `SetBudget` rather than a field beside its tokens — a wire-compatibility
  choice (a struct variant cannot gain a field without breaking every
  construction site) that also says the truth that the two bounds are
  independent — with `MoneyBudgetUpdated`, `MoneyBudgetExceeded` and
  `MoneyBudgetUnpriced { model }` (an unpriced model is not a free one);
  `89d8ec0` keeps the ledger from the same per-turn figure a footer shows
  (`ModelPrice::cost_micro_usd` over the turn's own counts, one arithmetic in
  one place), takes the price from the resolver (`TransportResolver::price`,
  implemented by `ProviderRegistry` from the descriptors a surface already
  reads, defaulted to `None` so a resolver that only maps ids is honestly
  unpriced), and enforces it in `over_budget` beside the token cap — either
  one reached stops the next turn and hands back what was queued behind it,
  each reporting its own event once; `be0211f` reads `/budget $2` as digits
  into micro-dollars (`$0.50` is 500 000, never a float) and refuses a figure
  finer than a millionth, a sign, or a typo in either unit by naming which,
  with `/budget off` lifting both bounds in one intent.
- [x] `low` ~~subagents have no genome handle: `tool_agent.rs:199-202` passes
  `None` where the tool loop passes its `SharedGenome`~~ landed 2026-10-09 in
  `6cdb2af`: `ToolAgentRunner` carries the session's handle (`with_genome`) and
  hands its publish point to `execute_tools`, exactly as the runtime does for
  the main loop, so a file a subagent writes is folded in as that call returns
  instead of at the next turn's walk. The runtime builds the index before the
  runner — the same cold start, just earlier. `None` stays the honest value for
  a caller with no root: the write still happens, only the fold is skipped.
- [ ] `low` a deleted import target does not re-parse its importers: removing a
  file drops its symbols and edges, and an importer that named it keeps the
  import it had. The next turn's walk fixes it, not the change itself.
- [ ] `low` `SearchHit.ts` for exact session-hit times: `SearchHit` carries
  `session_id`/`entry_id`/`text` and no timestamp, so a hit row shows the
  session's last write — which is what keeps a hit row and a list row from
  disagreeing. A timestamp on the hit would let the row say when the matching
  line was written instead.
- [x] `low` ~~the five production `unwrap`/`expect` sites in `chat.rs` the lints
  worker listed (`2113`, `2164`, `5338`, `8665`, `8726`)~~ they no longer
  surface: `chat.rs` contributes none of the 11 headers the measurement now
  finds, so whatever became of them, nothing in that file is behind an
  unscoped allow any more.
- [x] `medium` ~~wire `AskRequested` into the chat, so the held `ask` commits can
  land~~ landed 2026-10-09 in `c505bd7`: the panel follows the approval prompt's
  interaction (the question is the title, the choices are the rows, arrows move
  and Enter takes a row, Space ticks on a multi-choice question and Enter sends
  the set with an empty set refused, Esc answers `Cancelled`, Ctrl+C interrupts
  the turn as over an approval, and a printable character answers in the
  composer). `d52f1cf` (the tool and the engine's `SessionAsk`), `ebf8fc6` (the
  headless answer path) and `015f8ed` (its test's mode bit) are on `master` with
  it.

**Landed in the wave that followed (2026-10-08 → 10-09), beyond this queue.**
Genome per language: `714824d` split the scanner into one module per language
behind a single table, `2b49fed` reports what each language's index rests on,
`3624fe8` keeps capability out of the check's findings, `91148a1` prints the
roster, `520321a` documents the table, and one commit per language carries its
declarations and placements — `4ce6027` Go, `d445f15` C/C++, `e4b4295` C#,
`4f972da` Swift, `a59084a` Python, `32fc554` TypeScript, `133c7c0` Java,
`683d05f` Kotlin, `d3a18b9` PHP, `b83b13b` Ruby. Features: GFM tables in the
transcript (`ae1f35c`, goldens `39bae94`, input caps `b45b07b`, the cut-cell
style fix `57b0454`), the turn footer with usage (`9430689`) and the run state
in the terminal title (`1a2ecc3`), emoji in the live composer (`faa2de9`
crate-side, `effcf3e` wired, `28394b1` backspace). `4134736` dropped four
unused dependency edges.

The grammar wave (2026-10-09) finished that genome work: every language row now
reads its declarations from a syntax tree instead of patterns, so all 13
languages are `Full` and `Level::Heuristic` has no instance left — it stays as
the name of the state a `grammar: None` row describes, pinned by
`crates/titi-genome/src/lang/mod.rs:508-522`. Terminal chrome landed with it:
notifications on a finished, failed or asking turn (`notify.completion`,
`notify.error`, `notify.ask`, f8ea043), the terminal's own progress for a
running turn (`terminal.progress`, e7cd8ae), and the generation rate on the
working row (`composer.tokenRate`, 64bc233 — the characters the row already
counts ÷ 4 over a rolling 4 s window, printed with a `~` because it is an
estimate, `titi-tui/src/status.rs:182-235`). Honest limits: the C row claims
`.h` and retries the C++ grammar when the C parse is broken (`titi genome
capabilities` says so), `export_to_file` is still a plain `fs::write`, and the
lints' `unwrap`/`expect` warning headers stood at 1304 when this was written
  2026-10-09 (1392 before the `App` deletion), a count that follows the tests
  rather than a fact to trust.

The `App` deletion wave (2026-10-09) closed the audit's last structural item.
Phase 1 had moved the live helpers out; phase 2 deleted the stack — `app.rs`
(2129 lines), the eleven `titi-tui` modules only it used (5315 lines) and the
seven test files that only drove it (`dispatch`, `engine_events`,
`overlays_paste`, `queue`, `slash_completion`, `transcript`,
`slash_snapshot`). `cli/src` went 22201 → 20071 and `tui/src` 22171 → 16607 in
those commits (at this head 20157 and 19468, the later feature work adding to
both); `tests/mouse_preset.rs` is new because the helper it covers outlived
the stack. Terminal chrome: the status line is painted from a five-entry
preset table (Default, Minimal, Compact, Full, Ascii) chosen by
`statusLine.preset` or `/statusline <preset>` (validated before anything is
written), with the context gauge filling the gap between the groups in accent
and its label anchored at the right so `9%` → `10%` shifts nothing;
`statusLine.contextLine` defaults to off, so the default frame stays
byte-identical to before. LaTeX landed as a subset rendered to Unicode —
154 symbol commands, 17 big operators, 87 function names, accents and
`\frac`/`\sqrt`, inline and display (`crates/titi-tui/src/latex.rs`) — with
the contract that nothing is ever deleted: an unknown command prints verbatim,
a multi-character script keeps its meaning, an inline fraction stays flat; and
7d83722 lets a maths-only answer take the markdown path at all.


The ports-and-guards wave (2026-10-09) finished the `App` deletion's queue. The
three behaviours the deletion parked are back in the live chat, in one commit
(`92c08c8`): mouse selection and copy (`selection.rs` is live again — a press
anchors, a drag moves the far corner, a release copies through
`pbcopy`/`wl-copy`/`xclip` when one is on `PATH` and OSC 52 otherwise, and the
frame paints the region with `Selection::apply_background`), the appearance
re-probe on a focus gain (focus reporting goes on with the screen; the OSC 11
query's answer is reassembled byte for byte out of the key events it arrives as
— a reply's `BEL` comes back as ctrl-`g` — inside a 300 ms window, and nothing
of it reaches the composer), and the prompt-history browser (Ctrl+R, or ↑ at an
empty composer: the prompts *this session* carried, newest first, read from the
session's own store through `session_fs::session_history`, filtered as you
type; Enter puts one in the composer and starts nothing, Esc leaves the draft
alone). The composer also gained the word delete it lacked — alt+backspace /
ctrl+w through `titi_tui::space_hold::delete_before_cursor` — while `SpaceHold`
itself, the push-to-talk detector, stays dead on purpose: a hold-space key that
opened a microphone nothing listens to would be a key that does nothing, and
whoever lands STT either wires it to the live composer in one commit or deletes
it in one.

Four fixes landed alongside them: a panic now reports itself instead of being
erased by the alternate-screen restore (`d913076`; verified on a PTY —
`./target/debug/titi --panic-test` prints `panic: deliberate panic for the
panic-report check  crates/titi-cli/src/main.rs:57:9` and exits 101), the write
jail refuses a `.git` component for write, edit and hashline (`b494564`), a
checkpoint refuses to commit a staged path the `SensitivePolicy` blocks and
names the remedy (`ca0f406`), and the engine's cancel test waits on the trap
the turn sets instead of on a sleep (`2cce9f3`). Engine correctness: a retry no
longer replays reasoning (`8ba2604`, `StreamEvent::is_visible_output`),
`fallback_chain`/`fallback_cooldown` are deleted rather than documented
(`25235e5`, −732 lines, because per-role chains and a cooldown-based revert
never routed anything), and `EngineCommand`/`EngineEvent` are
`#[non_exhaustive]` with every variant pinned against hand-written JSON
fixtures (`6e97627`).

Lints: nine crates now scope `unwrap_used`/`expect_used` to production code
with a crate-level `#![cfg_attr(test, allow(...))]` (b7396f9 … ad76f7c), which
took the `unwrap`/`expect` warning headers from 1304 to 435 (`cargo clippy
--workspace --all-targets`; 1477 → 535 `warning:` lines). `titi-cli` and
`titi-tui` are not scoped yet and carry most of what is left. The test suite
went 1774 → 1784 passed in the same wave, which is not only growth: deleting
`titi-providers/src/fallback.rs` and `titi-config/src/fallback.rs` took 20
tests with it (14 + 6).

Process, learned the hard way: three agents staged into one git index, which
produced two commits whose subject named one crate and whose stat listed
another and swept a docs fix into a peer's commit; `.agents/skills/commit/
SKILL.md` now carries a `mkdir`-based commit lock taken before staging and
released after the commit, explicit-path staging and the report-don't-rewrite
rule (`408c44e`). The two mislabeled commits were reconciled with an
interactive rebase — one subject reworded, one commit split — that left
`HEAD^{tree}` byte-identical, so the rewrite moved messages, not content.

The review-and-ergonomics wave (2026-10-09) closed this survey's first
recommendation and the four parity gaps it found in flight. Safety: every tool
now names the call it is about to make — `fetch` was the one outbound channel
approved blind, so its `describe()` renders the masked URL (`fetch
https://docs.rs/serde`; a query string's secrets are redacted before the length
bound) and `settings` names the key it reads or writes (`e58ad4c`); a cloud
metadata target is refused before a socket is opened, the whole link-local block
included, with loopback left allowed because a dev server is a legitimate fetch.

`bash` gained the auto-background behaviour GAP listed as absent (`17af487`,
`f00ee88`): a command still running after `BACKGROUND_AFTER` — 60 s, or
`TITI_BASH_BACKGROUND_MS`, or `EngineConfig::background_after` for a surface that
wants its own — comes back at once as `moved to the background as bg-N · the
output will arrive when it finishes` and joins the existing job table instead of
a second registry, so `/jobs` lists it, `/jobs cancel bg-N` stops its process
group, and its output reaches the session as a follow-up turn, masked and capped
exactly like a tool result. No new protocol variant: a command runs once, on no
timer, so it gets no `JobStarted`. Four cancel behaviours, each tested: a
**turn** cancel leaves the handed-over command alone, a command under the
threshold is killed as before, `/jobs cancel` only signals (the thread waiting
reports the end, so no surface is ever told a job stopped while it still runs),
and the engine's drain cancels them at shutdown so none outlives the session
that would report it.

Ergonomics: a bare `exit`, `quit` or `q` — matched whole, so `exit code` is an
ordinary prompt — leaves, with a second Enter inside the existing `QUIT_WINDOW`
confirming it when the session holds a conversation (`da28518`); a second Escape
on an empty composer calls the same cut `/rewind` calls, so the chord and the
command cannot drift (`1a159cf`); and `--continue`/`-c` or `session.autoResume`
reopens the newest session (`4ca14d6`). Honest limit: the resume is
agent-directory-wide, because sessions record no workspace.

`docs/research/product-review.md` is the wave's survey — a read-only read of the
tree at `3c1bc9e` plus these commits, with the omp-inventory pointers corrected
and a ranked gap table; it is dated, and the queue above carries what it
recommends next. The test suite went 1784 → 1812 passed, and the
`unwrap`/`expect` warning headers 435 → 441 (`cargo clippy --workspace
--all-targets`; 535 → 555 `warning:` lines): the new code adds production sites,
while the two unscoped crates still hold most of the number.

The workspace-and-Taimyr wave (2026-10-09) finished the review's first list and
started Taimyr. Restorations, all in `chat.rs`: the paste collapse is back with
the lost module's own threshold — six lines, restored rather than chosen, since
the old `App` passed six — as a one-line `[Paste #1 · 8 lines]` marker whose
body is swapped in at submit while the transcript echoes the marker, and
`Screen::open` now asks for **bracketed paste**, which no earlier titi did, so
no terminal ever marked a paste and the collapse could not have fired at all:
that half was a live bug, not a regression (`3f86696`). The section model is
back as `/details <section> <mode>` over thinking, tools, subagents and
activity, with the deleted module's DoD defaults and one departure — activity
stays expanded, or `/usage`, `/context`, `/jobs` and `/recap` would print
nothing — plus a `▸ folded 14 turns · 22k tokens` divider that is collapsed by
default (`21bc0fb`). `/sessions` finally has the caller the FTS5 index never
had (`6d7a89e`): a bare one opens exactly what Ctrl+X opens, and a query offers
a row per matching entry and narrows as it is typed. Honest limits there: it
reaches only what the index holds, both lists cap at forty rows, and the row's
time is the session's last write rather than the hit's own — which is why
`SearchHit.ts` is queued.

Sessions now know the workspace they were started in (`ca61791`, `1fe4bc0`,
`a77d12f`): `SessionMeta::cwd` is persisted as `sessions.cwd` and `None` means
*unknown, never "not yours"*, so nothing may treat it as a filter result; the
v1→v2 migration probes `PRAGMA table_info(sessions)` before adding the column
and stamps `user_version = 2`; and resume is `newest_session_in(agent_dir,
workspace)` with three branches — the scoped pick when the index has one, the
newest session anywhere when this workspace has nothing recorded, and the
newest anywhere when the index cannot be read — so a session written before the
column existed is never hidden by a filter it predates. The store also records
the process working directory when its caller names no workspace, which is what
makes the field live in production rather than a value only callers could fill.

The lints sweep finished: `titi-cli` and `titi-tui` are scoped now (`b63140a`,
`3315395`, `e0be096`), which took the `unwrap`/`expect` headers from 441 to
**15** of 111 `warning:` lines (`cargo clippy --workspace --all-targets`,
measured on this tree). What is left is production code — `titi-genome/src/
refs.rs`'s three kept regex `.expect`s, `src/shared.rs`, `titi-memory/src/
redact.rs`, `titi-tui/src/theme/mod.rs` — plus four in
`titi-engine/tests/protocol.rs`, a test file that missed the sweep. Two files
did: that one, and `titi-cli/tests/continue.rs`, which still carries the old
blanket `#![allow(clippy::unwrap_used)]` because the sweep deliberately left it
to the worker who owned it. One commit's scope is half wrong rather than its
content: `e0be096` says `chore(cli)` and carries three `titi-tui/tests/*` files
beside the thirteen cli ones — one concern, two crates. The docs surface landed
with it: a `CHANGELOG.md` exists, newest-first and user-facing, and the README's
placeholder link is gone (`240db13`).

Taimyr phase 0 landed in `crates/titi-genome` only, one concern per commit:
`91f17d8` (a file's content hash in `FileRecord`, so a same-size rewrite inside
one mtime tick is not re-parsed), `d8f3deb` (the graph is recomputed only when
a file's `(exports, imports, used_symbols)` tuple moved — the condition phase 1
formalises), `d0eb4ed` (`Genome::apply_changes` for a path set the caller
already knows, with no walk) and `580de37` (`SharedGenome`: the index behind
`Arc<RwLock<Arc<Genome>>>`, published as one consistent snapshot), then
`dca1460` wrapping the new code to rustfmt and `92b97ff` moving the example's
assertions onto the file it touched. What phase 0 provably skips is those three
things exactly: the parse when size and hash match, the rank pass when the
tuple did not move, and the walk when the paths are named. Its own check is
`cargo run -p titi-genome --example refresh -- .`, which asserted whole-tree
counters until `92b97ff` made them the touched file's, because several agents
edit this tree at once. The snapshot type has no production reader yet: the
engine still keeps `Arc<Mutex<Option<Genome>>>` (queued). The design is
`docs/research/genome-incremental.md` — dated, with its pipeline, its
update-strategy table, its consistency contract, phases 0–5 each with the test
that fails without it, and the `notify` decision confined to phase 3.

The suite went 1812 → 1855 passed (77 targets, 0 failed): `titi-cli` unit +21,
`titi-core` unit +7, `titi-genome` unit +3, genome `tests/index.rs` +7 and
`titi-cli/tests/continue.rs` +5. The restored code in `chat.rs` was hand-wrapped
and needed its own `style:` commit (`a3b63c5`) before the fmt job would pass.

The lints-residue, money and Taimyr-1/2 wave (2026-10-09). The sweep's last two
files are scoped (`3118e96`): `titi-cli/tests/continue.rs` gave up its blanket
`#![allow(clippy::unwrap_used)]` for the crate-level form, and
`titi-engine/tests/protocol.rs` — which had no header at all — took the
scoping's four headers out of the count.

Money landed as a mechanism with an honest empty table. `ModelPrice`
(`titi-engine/src/registry.rs:46`) carries `input`, `output` and an optional
`cached_input` (a provider that does not bill cache at its own rate says so with
`None`), all in micro-dollars, and `cost_micro_usd` rounds **up** — a turn that
spent anything must not print `$0.00`, and a zero price is still zero — with
`cached_tokens` clamped to `prompt_tokens` because a provider reporting more
cached than prompted is the provider's bug. The built-in table ships
**empty-but-typed**: `NO_PRICE_MODELS` (`titi-cli/src/engine.rs:246`) names all
26 built-in ids with the reason each is there — the metered backends this tree
cannot price and the subscription/gateway backends whose flat plans have no
per-token rate — and
`every_builtin_model_is_priced_or_on_the_no_price_list`
(`titi-cli/tests/engine.rs:226`) walks `default_registry_config()` and refuses an
id that is neither priced nor listed, so a new model forces the decision instead
of shipping a silent gap. Nothing was invented for the table and nothing was
fetched: the tree holds one price note (`docs/research/prompt-cache.md` —
Sonnet 4.5 at $3/MTok in, $3.75 cache write, $0.30 cache read) and **no output
price**, and a figure built from input and cache reads alone would understate
every coding turn. The surfaces are what the review asked for: the turn footer
carries `cost_micro_usd` and prints `· $0.004` when the model has a price and
nothing when it does not (`$0.000` would read as free, and unpriced is not
free), `/usage` adds `· session total $0.38`, or
`· session total $0.38+ (unpriced turns excluded)` when any turn was unpriced.
`/budget` still refuses a cap in money, but the refusal is now specific: with a
price it names the model's own rate and says the missing piece is the engine's
cost ledger; without one it says the model is unpriced here; it never guesses a
rate into a token cap. The mechanism that would make `/budget $2` real is
queued — a `micro_usd` on `SetBudget` and a cost accumulated from `TurnUsage` in
`Runtime::over_budget`, which today reads token counts (`runtime.rs:1309`).

Taimyr phases 1 and 2 landed, both in `crates/titi-genome` plus the engine's
two files. Phase 1 (`eac8a9c`, `3a7aafa`, `b4b1401`): raw mentions are kept and
inverted into a name index, imports resolve from candidates rather than from a
filtered answer (every language row's `resolve_*` now returns candidates), and a
targeted update parses the caller's files first. What it provably skips is
measured, not asserted in prose: the test
`an_export_change_reresolves_only_the_files_that_mention_the_name` changes an
export in a four-file tree and asserts `stats.reresolved == 2` — "the edited file
and the file that mentions the new name" — with `reresolved < files.len()`;
`a_specifier_that_now_resolves_is_found_without_reparsing_the_importer` calls
`apply_changes(&["src/missing.ts"])` and gets `parsed == 1`, `!walked` ("a
targeted update trusts the path it was handed") and a fixed importer that was
never re-parsed. Phase 2 (`a4716a6`, `aede977`): the engine holds
`genome: SharedGenome` at `runtime.rs:780` (`runtime.rs:117`'s
`Arc<tokio::sync::Mutex<Option<Genome>>>` is gone), and the tool loop folds a
write in **before the tool returns** — `genome.apply_changes(&[path])` at
`tool_loop.rs:227`, skipped when the result is an error or no path was written —
so a turn's prompt is current by construction, because the writer is synchronous
inside the tool call; an out-of-process writer is still caught by the turn-start
walk. `RefreshStats.walked` and `SharedGenome::last_stats` are new public
surface, added on purpose to make that observable. The honest edges: the
no-walk claim rests on a unit test, because the engine keeps its handle private;
`tool_agent.rs:199-202` passes `None` for subagents (their writes arrive at the
next turn's walk) and that is queued; a deleted import target does not re-parse
its importers, also queued; and candidates are frozen at parse time, so a
resolution is only as fresh as the last parse of the file that asked.

The suite went 1855 → 1879 passed (77 targets, 0 failed): `titi-cli/tests/
engine.rs` +3, genome `tests/index.rs` +2, engine `tests/loop.rs` +1, `titi-cli`
unit +5, `titi-engine` unit +6, `titi-genome` unit +3, `titi-tui` unit +4. The
`unwrap`/`expect` headers are **13** of 102 `warning:` lines now (`shared.rs` 5 —
the deliberate poisoned-lock panics — `redact.rs` 4, `refs.rs` 3,
`theme/mod.rs` 1).

One operational lesson worth keeping: `cargo test --workspace` failed twice in
`crates/titi-tui/tests/dependency_rule.rs` with `NotFound` for
`titi-tui/Cargo.toml`, a file that exists. The cause was a **foreign stale
artifact** in `target/debug/deps/` — that one `dependency_rule` binary carried
no `/Users/workie/proj/titi` path at all, while every rebuilt one does — so the
harness ran a binary built from a checkout that no longer exists, whose baked
`CARGO_MANIFEST_DIR` points nowhere. `cargo clean -p titi-tui` fixed it (and
freed 6.2 GiB of artifacts from the `/tmp/titi-baseline` and `/tmp/titi-head`
checkouts that share this target directory). A `NotFound` on a file that is
present means the *binary* is not yours.

The background-indexer and split wave (2026-10-09) finished Taimyr's phases 4
and 5 and took the review's structural finding first. The index is no longer
built inside the turn: `GenomeHandle` is one long-lived thread fed by an mpsc
channel that coalesces a burst of saves into a single parse, and a `Quiesce`
whose reply is the generation that was published (`5d8d9a7`); the turn sends the
files it touched as the parse priority plus a resync for the paths no tool
named, waits at most `GENOME_QUIESCE` = 40 ms and renders from a snapshot
(`ef304a9`, `titi-engine/src/runtime.rs:138`), so the map is current by
construction when the worker has caught up and says so when it has not. `pending`
is `sent > applied` — two monotonic counters, the handle bumping `sent` before
it sends — rather than the count of whatever window the worker happens to hold,
which is what made a request already in the channel invisible and let a turn
render a map missing the changes it had just handed over (`dcd962e`). A window
that fails keeps its queue (`paths`, `urgent` and the walk flag survive; only
the window flag clears) and is applied with whatever arrived since, so a failure
is retried instead of forgotten by the next empty window (`4f455ec`). A subagent
carries the session's handle now (`6cdb2af`), so its writes are folded in as the
call returns like the main loop's, and `None` stays the honest value for a
caller with no root. Phase 5 makes the LSP follow the editor: the server
advertises full sync, folds a buffer's text in as that file's content
(`Genome::overlay`, a map separate from `files` so the record and the disk read
agree), and holds the invariant that one query is answered from one text — the
record is parsed from the overlay and the identifier a position points at is read
through it too, so a cursor on an edited line cannot locate a name in text the
user can no longer see; `didClose` drops the overlay and re-stats the path
(`0a5a8ea`). **Phase 3 — the watcher and its debounce — is the only phase left**,
still blocked on the owner's word about `notify`, and it now has a worker to feed
rather than a design to invent.

The honest limits, as the workers wrote them: on a big tree the per-turn resync
can outlast the 40 ms wait — the ordinary case rather than a rare one — which is
exactly what `pending` names, and the number shown is the window's item count
with a floor of one because outstanding work is worth naming even when the window
has no paths yet; a **failed window is invisible to logs and events** (the
backlog survives and is retried, and only `pending` says so); and a **path-only
window failure is unexercised** by a test, the case the old comment assumed away
by claiming the next walk would cover the same tree.

`chat.rs` — 17,154 lines at the review's last look, one writer, the reason every
UI item in this file serialized — was split: the transcript renderer into
`transcript.rs` (`1d4f354`), the pickers into `pickers.rs` (`a5779d1`), the key
path into `keys.rs` (`9b49145`) and the composer into `composer.rs` (`24e51ef`),
each with `lib.rs` declaring it. About 4,900 lines left the file; what remains is
14,991 lines of session, loop and binding core, still the largest file in the
tree, so the next split is its own item rather than a finished story. The slash
completion got its two fixes (`132760f` runs the highlighted command on Enter
instead of inserting its name; `d1bc5b8` lets Esc close the command list and keep
the draft). `/hotkeys` landed (`7a7e7bc`, recorded above). The bash background
threshold became a setting (`16753dd`): `bash.autoBackground.thresholdMs`, with
precedence **environment over setting over the 60 000 ms default**, passed
through `titi_tools::background_after_with` so a value already exported is never
overridden by a config layer. The turn footer's three parts can be switched off
(`ada4032`: `display.turnFooter.time`, `.tokens`, `.cacheMiss`, unset means on,
read with the same `settings::switch_off` as the notify and progress keys). And
the two allowance cleanups: `1cf904a` narrowed `lang/mod.rs`'s module-wide
`#![allow(clippy::expect_used)]` to one `support::literal_regex` with an
item-level allow and the why on it, and `3f06121` gave `redact.rs`'s four
`Regex::new` sites the same treatment — each with the reason a source-literal
pattern has no input that can make it fail. The skill that came out of the last
round's artifact hunt is recorded where an agent will read it: `46a539a` adds to
`ci-and-tests` that a target directory must never be shared between checkouts.

The suite went 1879 → 1907 passed (77 targets, 0 failed): genome
`tests/check.rs` +3 (the LSP tests), genome `tests/index.rs` +1, `titi-cli` unit
+6, `titi-config` unit +1 (the threshold key), `titi-engine` unit +3,
`titi-genome` unit +11 (the live worker and the overlay), `titi-tools` unit +1
and `titi-tui` unit +2 (the footer switches). The `unwrap`/`expect` headers went
13 → **9** of 98 `warning:` lines — `titi-genome/src/shared.rs` 5 (the deliberate
poisoned-lock panics), `titi-genome/src/refs.rs` 3 (the kept regex literals) and
`titi-tui/src/theme/mod.rs` 1 — because `3f06121` moved the four redaction
expects and `1cf904a` the language table's behind item-level allowances that say
why. All three gates ran in a **clean detached worktree** of this commit with its
own target directory (`git worktree add --detach`), so nothing in the dirty tree
was staged, formatted, or built against the shared `target/`.

The money-budget, tree and paste wave (2026-10-09) closed the last of the
review's ranked gaps. Money now has a cap end to end. The wire gained
`SetMoneyBudget { micro_usd }` as a **sibling** of `SetBudget` rather than a
field beside its tokens (`36d3aef`) — a wire-compatibility choice, since a
struct variant cannot gain a field without breaking every construction site, and
two commands also state the truth that the two bounds are independent: setting
one does not restate the other, and clearing one leaves the other standing.
Three events answer it: `MoneyBudgetUpdated` (what the engine has measured
against what cap), `MoneyBudgetExceeded` (the same stop as the token cap's
event, its own variant because the token event's shape is pinned by a surface
this change does not own) and `MoneyBudgetUnpriced { model }` — an unpriced model
is not a free one, so a cap the engine cannot measure is named rather than
accepted. The ledger (`89d8ec0`) is the same per-turn figure a footer shows:
`ModelPrice::cost_micro_usd` over the counts the turn itself reports, computed
where it reports them, so there is one arithmetic and one place it happens; the
price comes from the resolver (`TransportResolver::price`, implemented by
`ProviderRegistry` from the descriptors a surface already reads, defaulted to
`None`, so a resolver that only maps ids is honestly unpriced). The bound is
enforced in `over_budget` **beside** the token cap — either one reached stops
the next turn starting and hands back what was queued behind it, each reporting
its own event once. `/budget $2` (`be0211f`) reads digits into micro-dollars
(`$0.50` is 500 000, never a float) and refuses a figure finer than a
millionth, a sign, or a typo in either unit by naming which; `/budget off`
lifts both bounds in one intent, as two commands sent in order. The limit that
remains is the one recorded before: **no built-in model carries a price**, so
the ledger is exact where a user has written a rate and `MoneyBudgetUnpriced`
says so where it has not.

`/tree` (`141700f`) draws the append-only store as the tree it is — one row per
entry indented by depth, the path to the leaf marked `•`, the leaf `✓ current`,
the title counting what is off the path — and Enter moves the leaf to the row
under the cursor and replays that path (`session_fs::branch_at` forks the
store's leaf, walks the new path, and the `RestoreHistory` a rewind already
sends replaces the engine's history). What is left behind stays in the store,
which is what makes this a branch and not a rewind. Deliberately not taken from
omp: `/tree`'s entry filtering and the summary of the branch you leave.

The large-paste menu (`b8995eb`) is omp's `paste.largeMenuThreshold` at 100
lines: a longer paste stages its marker **first** — exactly as a short paste's
does — and only then opens the panel (`pasted 150 lines · esc keeps the marker`),
so the offer can only sharpen what already happened and a paste is never lost to
the menu. Two of its options are load-bearing: "attach as a block" keeps the
marker and turns what it stands for into the fenced body at send, and "attach as
a file" writes the body to `.titi/pastes/paste-<n>.txt` and puts that path in
the draft. That path is inside the user's repository, so `18ebf77` makes the
directory write its own `.gitignore` holding `*` before the paste and only when
there is none — the repository's own `.gitignore` is never touched, and a
directory that will not take the ignore file **fails the attach** instead (the
marker stays, nothing is written, the note says why). The test inits a real
repository in a temp dir and asserts `git status --porcelain` is empty after an
attach.

`ec6df82` moved the reference patterns' compile site: `refs.rs` held three more
literal-regex `expect`s than the language modules, and the helper the previous
commit introduced lived in `lang/support`, the wrong side of a crate whose
collector is language-agnostic on purpose. One private module at the crate root
— `patterns` — is the honest home, `crate::patterns::literal_regex` is
`pub(crate)` inside it, and the single item-level allow with its why stays on
that one item. The three reference patterns (`CALL`, `PATH`, `TYPE` in
`collect_refs`) now compile through it, and the test asserts each separately,
because a pattern that still compiled but stopped matching would otherwise go
unnoticed. `c8c8dbf` restores a doc comment on `share` that a refactor had
dropped.

Two commits landed **while this round was running**, after the eight above, so
the range grew after the inventory: `e5b72ff` drops a stray doc block and
`#[test]` that `be0211f` left behind when it removed a test — the next test
carried two attributes and every test build warned
`duplicate_macro_attributes` — and **`e5b72ff` is where the previous push
stopped**.

**`ask` is in, end to end.** `d52f1cf` added the tool and the engine's
`SessionAsk`, `ebf8fc6` the headless answer path, `c505bd7` the screen's panel
and `015f8ed` the mode bit on the tool's test file. The whole line landed once
the interactive surface existed, which is exactly why it was held back a round:
a question the chat could not answer would have parked the turn. The shape is as
designed — the tool owns no surface (it hands an `AskRequest` to an `AskSink` the
session installs, `ToolHandler::set_ask`, the shape `bash`'s background door
already had), the engine's `SessionAsk` turns one ask into one
`EngineEvent::AskRequested` plus a wait on a oneshot that
`EngineCommand::AnswerAsk` resolves, and the panel follows the approval prompt's
interaction because it is the same kind of moment: the model is stopped, the
session reads `needs you`, and the keys belong to the prompt until it is
answered — arrows move, Enter takes a row, Space ticks rows on a multi-choice
question and Enter sends the set (an empty set is refused with a hint, because
it is not an answer), Esc answers `Cancelled`, and Ctrl+C interrupts the turn
exactly as it does over an approval. One question per call, with a cap on the
choices (a longer dialog "is not a question, it is a document", the module
says), and a question with no list is answered in the user's own words in the
composer. Deliberate and unchanged: it is **read-tier**, so a question is never
itself a prompt to approve (plan mode keeps it with `read`, duck mode withholds
it); a **subagent cannot ask** — its registry is built without the door, so its
`ask` answers "no user to ask" and the subagent carries on, where omp aborts the
whole turn, because forwarding to the parent's surface would attribute a
question to a worker the user never started; the answer path is **interruption,
not a deadline** (there is no timeout, where omp's auto-selects the recommended
option, because a deadline would have to answer a question only the user can);
and a session with no surface says so rather than waiting forever.

The network guard closed the two holes that were left beside it. `fetch` refused
a metadata host **by name** and then let its client follow five redirects itself,
so a page answering `302 Location: http://169.254.169.254/latest/meta-data/`
walked straight past it; `b8a51ad` makes the client follow nothing
(`Policy::none()`) and has `fetch` follow by hand — every hop parsed,
scheme-checked and refused-or-allowed exactly as the first URL is, a relative
`Location` resolved against the URL that answered, the chain bounded by
`FETCH_REDIRECTS` (5, the bound the client used to apply) with
`TooManyRedirects` naming the count and the last URL, and a redirect with no
`Location` reported as the status it is. Refusing by name *and* by URL still
cannot catch a name that **resolves** to a metadata address — `metadata.example.
com` with an A record of 169.254.169.254 passed every check — and resolving in
the tool would not fix it either, because the address checked and the address
connected would be two lookups with a rebind or a second answer between them;
`924190c` moves the refusal to where a name becomes addresses: `GuardedResolver`
wraps the system's resolution and drops every answer `forbidden_address` names
(link-local `169.254.0.0/16` and `fe80::/10`, and the metadata addresses),
failing the lookup with the sentence the URL check uses when every answer is
forbidden, and `refusal_in` walks the client's error chain so the tool answers
`Refused` rather than `request failed`. `metadata_refusal` now builds its words
from the same `forbidden_address`, so the URL check and the address check cannot
drift apart.

`a973055` moved the welcome out of `chat.rs` into `welcome.rs` — 592 lines out,
611 in — continuing the split the wave before it started.

Nothing is in flight: the main checkout is clean and both workers are idle.

The suite went 1925 → **1952 passed** (78 targets, 0 failed) at this head, with
the ask line and the network guard in it: the new engine `tests/ask.rs` +3, the
`titi-cli` unit tests +8 (the screen's panel) and `titi-tools` unit +16 (the
per-hop redirect check and the guarded resolver). The `unwrap`/`expect` headers
are unchanged at **6** of **95** `warning:` lines —
`titi-genome/src/shared.rs` 5 (the deliberate poisoned-lock panics) and
`titi-tui/src/theme/mod.rs` 1 — because nothing here added a new one, and
`refs.rs`'s three stay out of the count through `patterns::literal_regex`, the
single item-level allow whose comment says a literal pattern cannot fail for any
input. All three gates ran in a clean detached worktree of this head with its
own target directory, so the shared `target/` in the main checkout was never
built against.
