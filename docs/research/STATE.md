# STATE — точка возобновления конвейера titi

## Активное направление: reference product functional port

Статус: **0.1.0 работает** (чат на ratatui, headless, инструменты, сессии, Genome, память, SOUL). Встроенный каталог: OpenAI, OpenRouter, OpenCode, Anthropic; пользовательский конфиг накладывается по id, а не заменяет список. Канон направления — этот файл плюс [карта тем](README.md); каталог `reference-product-port/`, на который ссылались старые доки, в репозитории отсутствует, ссылки в research-доках исправлены. Goal loop поверх reviewer-а в master (`/goal`, 2026-09-23). Следующий шаг — ручная PTY-проверка `/goal` с живой моделью. Не начинать GPUI. Tree-sitter — замена эвристик Genome, не новый граф: symbol-level уже есть.

Правило продолжения: сначала прочитать этот STATE и док темы, прогнать baseline, выполнить только NEXT, затем обновить STATE.

Обновляется после каждого шага. Новая сессия начинает отсюда.

## Порт из omp, границы bash и заставка (2026-10-03, вторая волна, отлендилось в master)

Четыре воркера в отдельных worktree (супервизор смотрел диффы и тесты, а не отчёты, и черри-пикал по одному) плюс правки супервизора. Идеи взяты из omp 18.5.1 (MIT) и переписаны на Rust, кода оттуда нет.

- заставка (`3aae036`, `8d51599`, `6681a9e`): вместо рамки — чёрно-белый локап TITI как у omp: знак `██████ ██████` с полублочным вордмарком `titi` и версией справа, по центру, без рамки; все клетки знака — чистые серые, вычисленные от яркости страницы (на тёмной 0.96, на светлой 0.08), градиент по диагонали как у omp; под ним теглайн, блок фактов, аккорды и `Tip: …` (20 правдивых подсказок, выбор — хэш id сессии); на старте по знаку проходит блик (1.5 с, ease-out), после чего кадр равен покою; деградация по высоте — подсказка → сессии → каталог → теглайн → пустые строки → модель, ниже — одна строка `titi v0.1.0`;
- `todo` (`ef1dffc`, `b384265`, рендер `d3bfe78`): чеклист агента — `write` (весь список), `update` (статус по номеру строки), `view`; в работе не больше одного пункта (старт другого возвращает прежний в pending), 50 пунктов по 200 символов, read-tier (живёт в plan), в duck и у субагентов нет, на перезапуск не сохраняется; чип `todo 1/3 · Fix the parser`, под ним строки чеклиста;
- `read`/`glob`/`grep` (`c0e0108`, `118efd5`, `fd771b2`, `f98315c`): `offset`/`limit` с заголовком `[lines A-B of N]`; настоящие glob-шаблоны (`**/`, классы, `{a,b}`) через `regex` (уже был в lock-дереве, RustSec чист); grep по регулярке с `ignore_case` и фильтром `glob`, порядок по путям, кэпы 1000 путей / 500 строк со счётом остатка; `path` на файл теперь ищет в этом файле, а не возвращает пусто;
- реальные токены (`c1b9e55`, `c151d84`, `c77e5c3`, `c44631f`): `StreamEvent::Usage(TokenUsage)` из всех четырёх семей (Chat Completions, Responses/Codex, Anthropic с кэшем, Gemini с thoughts), Chat Completions просит `stream_options.include_usage`, помпа держит `Done` до 2.5 с ради хвостового чанка с usage, движок берёт счёт провайдера и оценку — только когда счёта нет; `/budget` больше не пишет «estimated» (`3db0dc7`);
- границы `bash` (`ec2abdd`, `7d05ab5`, `762f782`, `829e2a6`): путь по пайпу шёл через `Command::output` без дедлайна, без прерывания, без кэпа и ждал закрытия пайпов — `npm run dev`, `--watch`, `sleep 999`, даже `server &` вешали ход навсегда; теперь `pipe.rs` — дедлайн `timeout_secs` (300 по умолчанию) на обоих путях, своя группа процессов и SIGTERM→SIGKILL всей группе (сигнал шлёт `sh`, потому что `unsafe` запрещён), первые и последние 64 KiB каждого потока, ответ по выходу shell, а не по закрытию пайпов, `exit N` в ошибке. Ctrl+C не доходил до команды вообще: `Interrupt` был, но его никто не поднимал — теперь он в `EngineConfig`, Cancel/Shutdown его поднимают, новый ход опускает, а счётчик поднятий не даёт команде пропустить отмену, если флаг опущен до её опроса. Из интерсептора omp (у них выключен по умолчанию) взяты только правила для команд, которые не завершаются сами: dev-серверы, вотчеры, `tail -f`, `docker compose up` без `-d` на переднем плане отклоняются сразу с подсказкой `cmd > log 2>&1 &` и `timeout 30 cmd`; фон, кавычки, комментарии и heredoc не считаются.

Проверено вживую (PTY + `scripts/fake-provider.py`, новые ключевые слова `forever`, `dev server`, `make todo`), записано в `docs/QA_STATUS.md`: заставка в 80×24/60×20/100×30, 84 оттенка — все серые; `npm run dev` отклонён мгновенно; Ctrl+C на `sleep 600` — `command interrupted` сразу и ни одного `sleep` в `ps`; чеклист строками; `/usage` — `200 prompt + 20 completion` от провайдера. База: workspace 1742 passed, 0 failed (было 1664); clippy — новых предупреждений вне тестов нет.

Открыто: завершённые раунды отменённого хода не попадают в итог `/usage` (у отменённого хода нет `TurnUsage`, как и раньше); `cached_tokens` декодируется, но в `TurnUsage` поля нет; интерсептор срабатывает после подтверждения (вызов сначала спрашивает, потом отказывает); путь `grep`, ведущий за пределы workspace, молча ищет по всему workspace (было и раньше); правила omp «cat/grep/find → read/grep/glob» не портированы (у omp выключены по умолчанию).

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
