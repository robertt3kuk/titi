# Агенты, hub и безопасность

> Сравнительный research 2026-08. Продуктовые решения — этот README и [карта тем](../README.md). Действующий слой: `titi-engine::AgentSupervisor` + injectable `AgentRunner`, lifecycle events, TUI `/agents`. Shared bus, file claims, permissions и steering — следующие слои, не готовы.

Тема: task-субагенты и их discovery, hub-сообщения между агентами, checkpoint/rewind, todo-списки, advisor-watchdog; модель безопасности Vellum (actor identity, sandbox на вызов инструмента, креды вне процесса модели, default deny).

## omp

1. **Discovery и исполнение сабагентов (task).** `discoverAgents(cwd, home)` мержит определения агентов по принципу first-wins по точному имени `agent.name`: проектный `.omp/agents` → пользовательский `~/.omp/agent/agents` → корни OMP-расширений (CLI → project settings → user settings → npm/link плагины) → Claude marketplace-плагины → встроенные (`scout`, `designer`, `reviewer`, `security-reviewer`, `librarian`, `task`, `sonic`); прямые корни `.claude/agents`/`.codex/agents`/`.gemini/agents` намеренно пропускаются. Определение — markdown с frontmatter (`name`, `description`, `system_prompt` обязательны; `tools`, `spawns`, `model`, `advisor`, `blocking`, `output` опциональны); битый файл не валит discovery — он логирует warning и пропускается, а встроенные парсятся с `level: "fatal"`. (Источник: omp://task-agent-discovery.md)
2. **Wire-протокол task и guardrails.** При `task.batch` (по умолчанию on) вызов — `{ context, tasks[] }`, где `context` обязателен и вшивается в system prompt каждого спавна; полю `agent` по умолчанию = спавн-политика. Исполнение: фоновые job'ы при `async.enabled=true`, синхронный фолбэк, per-item `blocking: true`. Дочерняя сессия получает изолированный снапшот настроек, `tools.approvalMode` форсируется в `yolo` (headless-сабагенту некому показывать промпты), `todo` вырезается как parent-owned, `hub` сохраняется. Ограничения: спавн-политика `spawns` (`"*"` / CSV / `""`), `task.maxRecursionDepth` (default 2), `PI_BLOCKED_AGENT` против саморекурсии, `task.maxConcurrency` (семафор, resizable on the fly), `task.agentIdleTtlMs` (default 420 000 мс). (Источники: omp://tools/task.md, omp://task-agent-discovery.md)
3. **Agent Hub и peer-сообщения (hub).** Hub — TUI-ростер (`Alt+A`) статусов `running | idle | parked | aborted` с активностью, моделью, cost/tokens; `Enter` фокусирует транскрипт сабагента (steer обычным промптом), `r` оживляет parked, `x` убивает. Агентный инструмент `hub` даёт `send`/`wait`/`inbox`/`jobs`/`cancel`/`start`/`ps`/`logs`; отправка сообщения parked-агенту его оживляет; `history://<id>` и `agent://<id>` — транскрипт и финальный вывод. Advisor-строки — read-only наблюдатели: они исключены из peer-ростера `hub`, их нельзя ни писать, ни оживлять, ни убивать. (Источник: omp://agent-hub.md)
4. **Checkpoint/rewind — пара «маркер + свёртка контекста».** Инструменты скрыты за флагом `checkpoint.enabled` (default `false`); `checkpoint` (только `goal`) фиксирует границу = число сообщений + id последней персистентной записи (git и файловая система НЕ снапшотятся), и guard `#enforceRewindBeforeYield()` не даёт агенту `yield`-ить без отчёта. `rewind` (только `report`) в конце хода ветвит дерево сессии через `sessionManager.branchWithSummary(checkpointEntryId, report)`: разведочная ветка выпадает из активного контекста (но остаётся в `.jsonl`), отчёт возвращается как скрытое `rewind-report`-сообщение; ровно один активный checkpoint за раз. (Источники: omp://tools/checkpoint.md, omp://tools/rewind.md)
5. **Todo — один op за вызов, ошибки отбрасывают мутацию.** Операции `init | start | done | drop | block | unblock | rm | append | view`; модель состояния `TodoPhase { name, tasks }` / `TodoItem { content, status: pending|in_progress|completed|abandoned|blocked, blocker? }`. Инвариант single-active-task нормализуется после каждого op (ровно одна `in_progress`, иначе авто-промоция первой `pending`, blocked пропускаются); любая ошибка в op → `isError: true` и откат всей мутации. Сабагенты не наследуют `todo` (исключение — prewalk-armed). (Источник: omp://tools/todo.md)
6. **Advisor + WATCHDOG — параллельный ревьюер с карантином.** При `advisor.enabled: true` рантайм advisor'а получает дельты транскрипта primary, имеет собственный `ToolSession` и дефолтный грант `read`/`grep`/`glob` (расширяемый любыми built-ins через `WATCHDOG.yml`, включая `bash`/`edit`). Severity `nit`/`concern`/`blocker`; emission guard нормализует и дедуплицирует заметки (лимит 1 на update, FIFO 4096), `advisor.immuneTurns` (default 3) ограничивает частоту прерываний. Небезопасный вывод advisor'а карантинится до диспетча: недоступные non-bridge инструменты, output-only destructive-shell-директива или ≥3 output-only hazard-класса (destructive shell, instruction override, denial instruction, account-deletion claim) → весь ход advisor'а отбрасывается. `WATCHDOG.md` — приоритеты ревью в system prompt advisor'а; `WATCHDOG.yml` — ростер с `name/enabled/model/tools/instructions`. Advisor никогда не peer: `hub` send, revive, kill для него запрещены. (Источник: omp://advisor-watchdog.md)

## Hermes

1. **Делегирование — `delegate_task` вместо tool-spawn-роутинга.** `delegate_task({goal, context})` / батч `{tasks: [...]}` спавнит дочерние `AIAgent` с полностью чистым контекстом (ничего из истории родителя; в system prompt вшиваются только project context-файлы воркспейса), наследует доступ к инструментам; топ-левел вызовы уходят в фон и возвращают handle, результат приходит как фоновое завершение. Лимит параллелизма — 3 по умолчанию (`delegation.max_concurrent_children` / `DELEGATION_MAX_CONCURRENT_CHILDREN`, floor 1, без потолка); батч больше лимита — ошибка, а не тихая усечка. Глобальный пин дешёвой модели воркерам: `delegation.model`/`delegation.provider` (frontier-планировщик + дешёвые исполнители). (Источник: https://hermes-agent.nousresearch.com/docs/user-guide/features/delegation)
2. **Мультиботность — Bot Mode на профилях.** Бот = Hermes-профиль (`~/.hermes/profiles/<name>/` — изолированные config, memory, skills, credentials, история). Канонический Bot Chat каждого бота получает tool `message_agent(target, message)` — только он, не групповые/обычные сессии: таргет валидируется по живому ростеру, доставка fire-and-forget с автопрефиксом атрибуции `Message from 🤖 <sender>`, ответ приходит фоновым уведомлением; групповые чаты 2–6 ботов — до 3 серийных раундов и 10 сообщений за отправку. Кросс-машинно: Desktop-relay и `hermes peer add <name> --url --key` (`bot_peers` в config, ключ в `HERMES_PEER_<NAME>_KEY`), после чего `message_agent(target="spark/researcher")` работает без десктопа. (Источник: https://hermes-agent.nousresearch.com/docs/user-guide/bot-mode)
3. **Безопасность — 8 слоёв и default deny.** Авторизация гейтвея `_is_user_authorized()`: per-platform allow-all → DM-pairing (8-символьный код, TTL 1 ч, rate limit 1/10 мин, lockout 5 промахов на час) → платформенные allowlists (`TELEGRAM_ALLOWED_USERS=…`) → глобальный `GATEWAY_ALLOWED_USERS` → `GATEWAY_ALLOW_ALL_USERS` → **default deny**. Аппрув опасных команд: `approvals.mode: smart|manual|off` (smart — вспомогательная LLM оценивает риск), timeout (default 300 с) с fail-closed деноем; жёсткий `UNRECOVERABLE_BLOCKLIST` (`rm -rf /`, fork bomb, `dd` на block device, pipe-to-shell) срабатывает **ниже** `--yolo` и не отключаем никем; поверх — пользовательские glob-запреты `approvals.deny`. Песочница: docker-бэкенд с `--cap-drop ALL`, `--security-opt no-new-privileges`, `--pids-limit 256`, ограниченными tmpfs; для контейнерных бэкендов check опасных команд пропускается (контейнер — граница). Креды: `execute_code`/`terminal` вырезают переменные с `KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|PASSWD|AUTH` в имени, MCP-сабпроцессы получают только `PATH, HOME, USER, LANG, LC_ALL, TERM, SHELL, TMPDIR` + `XDG_*`; sensitive-пути (`~/.ssh/`, `auth.json`, `.env`, `mcp-tokens/`) жёстко закрыты для записи, опциональный `HERMES_WRITE_SAFE_ROOT` сужает запись до префикса. (Источник: https://hermes-agent.nousresearch.com/docs/user-guide/security)
4. **Checkpoint/rollback — shadow git, opt-in.** Checkpoint Manager держит один общий bare-repo `~/.hermes/checkpoints/store/` (per-project ref `refs/hermes/<hash>`); снапшот — максимум один на директорию за ход, автоматически перед `write_file`/`patch` и деструктивными командами (`rm`, `sed -i`, `git reset/clean/checkout`…). `/rollback <N>` по умолчанию восстанавливает только файлы, которые писал агент (agent-write ledger по хешам содержимого), ручные правки пользователя сохраняются; caps: `max_snapshots: 20`, `max_total_size_mb: 500`, `max_file_size_mb: 10`, auto-prune по `retention_days`. По умолчанию выключено (`checkpoints.enabled: false`). (Источник: https://hermes-agent.nousresearch.com/docs/user-guide/checkpoints-and-rollback)
5. **Todo / advisor: не покрывает.** Отдельного todo-инструмента в индексе документации Hermes нет (llms.txt такого раздела не содержит); ближайшие аналоги — [Persistent Goals](https://hermes-agent.nousresearch.com/docs/user-guide/features/goals) (стоящая цель, агент работает через ходы) и [Kanban Multi-Agent](https://hermes-agent.nousresearch.com/docs/user-guide/features/kanban) (durable SQLite-борда с per-task model override для координации нескольких профилей). Advisor-watchdog (параллельный ревьюер с карантином) не покрыт: ближайший аналог — команда `/review`, спавнящая независимого фонового ревьюера-сабагента на том же рельсе `delegate_task` (снапшот последних 10 сообщений + полные инструменты).

## Vellum

1. **Actor identity — резолв один раз, принудительно везде.** Актор классифицируется как `guardian`, `trusted` или `unknown` и резолвится один раз, после чего уровень принудительно соблюдается во всех путях: unknown-акторы не могут читать память, триггерить инструменты или эскалировать привилегии. Это ближайший аналог Hermes-овской лестницы allowlists, но встроенный в ядро агента, а не только в гейтвей. (Источник: local://vellum-summary.md, секция «Безопасность»)
2. **Креды — отдельный процесс, модель их не видит.** Учётные данные живут в отдельном процессе и никогда не попадают в модель; каждый вызов инструмента исполняется в песочнице; политика по умолчанию — deny. Это инварианты уровня рантайма (изоляция и fail-closed), а не проcлойки промптов. (Источник: local://vellum-summary.md, секция «Безопасность»)

## Решение (одно/комбо)

Комбо за основу берёт omp как механический каркас (Rust-клон по определению проекта): discovery first-wins + `task`-батч с обязательным `context`, реестр `running|idle|parked|aborted` с idle-TTL-парковкой и peer-хабом `send/wait/inbox`, checkpoint/rewind как ветвление дерева сессии (дёшево, без git), todo с single-active-task инвариантом и advisor-watchdog как единственный канал внешнего контроля качества. Поверх каркаса — модель безопасности Vellum как слой политики: `ActorIdentity { Guardian, Trusted, Unknown }` резолвится при входе сообщения/вызова и фильтрует доступ к памяти, инструментам и эскалации; каждый вызов инструмента уходит в sandbox (процесс/контейнер, у unknown — без сети и записи), креды инжектятся отдельным процессом-брокером через сокет так, что модель и транскрипт их не видят; всё, что не разрешено политикой явно, — deny. Из Hermes заимствуются только дешёвые и проверенные механики без собственного рантайма: hardline-blocklist и `approvals.deny` под слоем «smart»-аппрува для trusted-акторов, фильтрация env-переменных при создании песочниц и глобальный пин дешёвой модели воркерам (`delegation.model`-аналог в overrides агентов). Это оптимально: omp-механики дают 90% функциональности без изобретения, Vellum добавляет маленький, но формально проверяемый слой безопасности, а Hermes даёт готовые списки паттернов вместо проектных решений.

## Rust-маппинг

**Крейты workspace:**
- `titi-core` — типы сессии/сообщений, discovery, реестр агентов, спавн-политики.
- `titi-providers` — вызовы моделей, role-based маршрутизация (`model_roles`), advisor-вызовы.
- `titi-tools` — registry инструментов, включая `checkpoint`/`rewind`/`todo`.
- `titi-tui` — Agent Hub (ростер + инспектор).
- `titi-cli` — входная точка, флаги `--advisor`, config.
- **Новые:** `titi-agents` (task-исполнитель: батчи, семафор, lifecycle-парковка, hub-шина), `titi-security` (identity, policy engine, sandbox-фабрика, credential broker).

**Ключевые типы (эскиз):**

```rust
// titi-core::agents
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub tools: Option<Vec<String>>,          // None = унаследовать
    pub spawns: SpawnPolicy,
    pub model: Vec<ModelSelector>,           // приоритизированный список
    pub blocking: bool,
    pub advisor: Option<AdvisorOpt>,
}
pub enum SpawnPolicy { All, None, Allow(Vec<String>) }   // "*" | "" | CSV

pub trait AgentDiscovery {
    fn discover(&self, cwd: &Path, home: &Path) -> Vec<AgentDefinition>; // first-wins merge
}

// titi-agents::registry
pub enum AgentStatus { Running, Idle, Parked, Aborted }
pub struct AgentRegistry { /* id -> { status, session_path, usage } */ }
pub async fn spawn_batch(parent: &Session, ctx: &str, items: Vec<TaskItem>) -> Vec<JobHandle>;
// семафор tokio::sync::Semaphore, resized из живого конфига перед каждым acquire

// titi-agents::hub
pub enum HubOp { Send { to: String, text: String }, Inbox, Wait, Cancel { ids: Vec<String> } }
impl HubHandle { fn send(&self, to: &AgentId, msg: Msg) -> Result<()>; /* revive при Parked */ }

// titi-tools
pub enum TodoOp { Init(Vec<TodoPhase>), Start(String), Done(Option<String>), Drop(Option<String>),
                  Block(String, Option<String>), Unblock(String), Rm(Option<String>),
                  Append(String, Vec<String>), View }
pub struct TodoItem { pub content: String,
    pub status: Status /* Pending|InProgress|Completed|Abandoned|Blocked */, pub blocker: Option<String> }
// checkpoint/rewind: без git; ветвление дерева сессии
pub struct CheckpointState { pub message_count: usize, pub entry_id: EntryId, pub started_at: DateTime<Utc> }
fn rewind(session: &mut Session, report: &str) -> Result<()>; // branchWithSummary + rewind-report

// titi-security
pub enum ActorIdentity { Guardian, Trusted, Unknown }   // резолвится один раз на вход
pub struct PolicyDecision { pub allow_memory: bool, pub allow_tools: bool, pub allow_escalate: bool }
fn resolve_identity(msg: &InboundMessage) -> ActorIdentity;  // default deny: Unknown для неизвестных
pub trait Sandbox { fn run_tool_call(&self, call: ToolCall, id: ActorIdentity) -> BoxFuture<'static, Result<ToolOutput>>; }
pub struct CredentialBroker;  // отдельный процесс; инжект секретов в песочницу, модели не видны
```

**Внешние крейты:** `tokio` (рантайм, `Semaphore`, `process::Command` для сабагентов/брокера), `crossterm` + `ratatui` (hub-ростер, инспектор), `serde`/`serde_yaml` (frontmatter агентов, WATCHDOG-ростер), `tracing` (структурные логи/observability), `uuid` (v7 id сессий), `git2` — опционально и только если позже решим git-снапшоты как у Hermes; checkpoint v1 обходится ветвлением jsonl. Для sandbox'а per-tool-call: `landlock` (restrict filesystem) + `seccompiler`/`nix` (deny-сети) на Linux, fallback — запуск в контейнере; на macOS — `sandbox-exec`/изолированный процесс с пустым env.

## Definition of Done

- [ ] Тест discovery: merge порядок project `.titi/agents` > user > bundled, first-wins по точному имени; битый frontmatter пропускается с warning, встроенные агенты не теряются.
- [ ] Тест спавн-политики: `SpawnPolicy::Allow(["scout"])` отклоняет spawn иного агента с ошибкой со списком разрешённых; на `task.max_recursion_depth` у потомка инструмент `task` недоступен.
- [ ] Тест lifecycle: idle-агент паркуется по TTL (`agent_idle_ttl_ms`, default 420 с), `hub send` к parked-агенту возвращает его в `Idle`, advisor не появляется в peer-ростере.
- [ ] Тест checkpoint/rewind: после `rewind` активная ветка = маркер + отчёт, разведочные сообщения выпадают из контекста, но остаются в jsonl; второй `rewind` без активного checkpoint'а даёт ошибку.
- [ ] Тест todo: любой op с ошибкой не меняет состояние; после успешного op ровно одна задача `InProgress`.
- [ ] Тест политики: `ActorIdentity::Unknown` получает deny на чтение памяти, любой вызов инструмента и эскалацию; `Trusted` проходит список явно разрешённых; отсутствие правила = deny (fail-closed).
- [ ] Тест брокера кредов: процесс модели/субагента не содержит секретов в env и в транскрипте; снапшот окружения песочницы проходит по чек-листу фильтрации (аналог KEY/TOKEN/SECRET-фильтра Hermes).
- [ ] Smoke TUI: hub открывается, показывает ростер со статусами и usage, `r`/`x` работают на selected-агенте (crossterm-ивенты подаются в тестовый терминал).

## Deep-dive

Подсистемные доки (план 2-го уровня, писать при выделении подсистемы в реализацию):

- `docs/research/agents-hub-security/task-discovery.md` — frontmatter-грамматика агентов, merge-порядок, спавн-политики, depth-гейтинг, yolo-форс для headless.
- `docs/research/agents-hub-security/hub-messaging.md` — реестр статусов, idle-TTL-парковка, revive-протокол, IRC-семантика send/wait/inbox, peer-ростер в system prompt.
- `docs/research/agents-hub-security/checkpoint-rewind.md` — дерево сессий, branch_summary, guard перед yield, реидратация после resume.
- `docs/research/agents-hub-security/todo.md` — state machine статусов, op-семантика, markdown round-trip, интеграция с UI.
- `docs/research/agents-hub-security/advisor-watchdog.md` — дельты транскрипта, severity/steering, emission guard, карантин небезопасного вывода.
- `docs/research/agents-hub-security/security-model.md` — identity-резолв, policy engine, sandbox-фабрика, credential broker, deny-по-умолчанию, hardline-blocklist и env-фильтрация из Hermes.
