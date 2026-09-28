# Базовые инструменты

> Сравнительный research 2026-08. Продуктовые решения — этот README и [карта тем](../README.md). OMP остаётся источником конкретной UX/semantics механики (hashline, bash policy). Исполнение инструментов идёт через engine/tool registry и публикует lifecycle events.

Тема: ядро файловых и командных инструментов агента — `read` (anchored snapshots), `edit` (hashline), `write`, `bash` (PTY-рантайм, фоновые задачи), `glob`, `grep`, fs-scan cache. Это самый «горячий» контур harness: именно здесь живёт основная экономия токенов (структурные резюме вместо полных файлов) и основная безопасность (политика запуска команд).

## omp

omp покрывает тему исчерпывающе — восемь отдельных доков. Ключевые факты:

1. **`read` — единый инструмент с грамматикой селекторов и hashline-анкерами** (omp://tools/read). `splitPathAndSel()` в `packages/coding-agent/src/tools/path-utils.ts` распознаёт хвостовые селекторы `:raw`, `:conflicts`, `:N`, `:A-B`, `:A+C`, `:R1,R2,...`; парсинг падает сквозь нераспознанные суффиксы, чтобы работали `archive.zip:inner/file` и `db.sqlite:table:key`. Готовый к парсингу код без селектора структурно суммируется (`summarizeCode`: пороги `read.summarize.minTotalLines = 100`, ≤2 MiB, ≤20 000 строк), элидированные диапазоны закрываются футером `[…NNln elided; re-read needed ranges, e.g. <path>:5-16,40-80]` с конкретными диапазонами. В hashline-режиме вывод префиксуется заголовком `[PATH#TAG]`, где TAG — четырёхзначный hex-хеш нормализованного содержимого файла из session snapshot store (`packages/hashline/src/snapshots.ts`: до 256 путей × 4 версии, файлы >4 MiB не снапшотятся); одиночные ограниченные не-raw диапазоны добавляют 1 ведущую и 3 хвостовые строки контекста, raw и мультидиапазоны — точные. Read стримит файл (`streamLinesFromFile`, чанк 8 KiB) и не грузит его целиком.
2. **`edit` — hashline-патчи с привязкой к снапшот-тегам** (omp://tools/edit). Язык патчей: `[PATH#TAG]`-секции с операциями `PUT N.=M:`, `PUT N*:` (tree-sitter-блок), `PUT <N:`/`PUT >N:`/`PUT >$:`, `CUT N.=M`/`CUT N*` с именованными регистрами `@name`, `REM`, `MV DEST`; тело — только финальные строки `+TEXT`. Все номера относятся к оригинальному снапшоту; правки по строкам вне записанных видимых диапазонов отклоняются; стейл-теги проходят восстановление через цепочку снапшотов (`packages/hashline/src/recovery.ts`); byte-identical правка — ошибка с no-op-эскалацией после трёх повторов. Каноничная грамматика — `packages/hashline/src/grammar.lark`, парсер `input.ts`/`parser.ts`, применение `apply.ts`.
3. **`write` — диспетчер, а не просто запись файла** (omp://tools/write). Один `path` ветвится: архив-члены (`archive.ext:inner/path`, атомарная перезапись через временный файл + rename), SQLite-строки (`db.sqlite:table[:key]`, JSON5-контент), `conflict://<N>`, внутренние URL с write-хуком (`xd://`-девайсы). Запись снимает вставленные hashline-префиксы (`stripWriteContent` — только при включённом `hashLines`), выдаёт свежий `[path#TAG]`-заголовок, чтобы следующий edit не требовал перечитывания, и вызывает `invalidateFsScanAfterWrite()`.
4. **`bash` — несколько режимов исполнения поверх одного executor'а** (omp://tools/bash + omp://bash-tool-runtime). Входы: `command`, `env`, `timeout` (дефолт 300 c, `0` отключает дедлайн, кламп `1..3600` + глобальный `tools.maxTimeout`), `cwd`, `pty`, `async`. Слои до запуска: извлечение ведущего `cd <path> && ...` в `cwd`, политика `bash.patterns` (allow/prompt/deny, glob-матчинг по полной команде и по сегментам составной), опциональный интерсептор `bashInterceptor.patterns` (regex → перенаправление на read/grep/glob/edit/write/hub; блокирует только если целевой инструмент доступен в `ctx.toolNames`). Не-PTY исполнение — `executeBash()` из `src/exec/bash-executor.ts`: process-global кэш нативных `Shell`-сессий, ключ = (shell path, prefix, snapshot, env, session key, minimizer); при параллельных вызовах по одному ключу владелец — первый, остальные идут в one-shot `executeShell()` (`shellSessionsInUse`); таймаут «карантинит» сессию. Дефолтные env-закладки — `buildNonInteractiveEnv()` (`PAGER=cat`, `GIT_EDITOR=true`, `TERM=dumb`, `GIT_TERMINAL_PROMPT=0`, `NO_COLOR=1`, …), direnv-preflight (`bash.direnv: "auto"`, таймаут `bash.direnvLoadTimeoutMs = 30_000`). PTY-путь — `runInteractiveBashPty()` + нативный `PtySession`: требует `pty: true` И UI-контекст И `PI_NO_PTY !== "1"` (`canUseInteractiveBashPty()`); наследует пользовательский env с `TERM=xterm-256color` и НЕ применяет non-interactive hardening; Esc из оверлея убивает PTY. Вывод — `OutputSink`: rolling tail 50 KiB, опциональный head (`tools.artifactHeadBytes`, дефолт 20 KiB) с элидией середины, per-line cap `tools.outputMaxColumns` (768 байт), spill «сырого» потока в `artifact://` при переполнении.
5. **Фоновые задачи** (omp://tools/bash). `async: true` (только при `async.enabled`) немедленно возвращает `Backgrounded as job <id>; result will be delivered automatically` + `details.async: { state: "running", jobId }`; прогресс/завершение приходят через `onUpdate`/async job manager с `details.async.state: "completed" | "failed"` (ненулевой код и таймаут = failed). Авто-фон: `bash.autoBackground.enabled` + свободный слот менеджера → ждёт до `min(threshold, timeout-1s)` (дефолт `DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS = 60_000`) и бэкграундит; на переполнении менеджера — форграунд-фолбэк; steering-сообщение может забэкграундить кандидат раньше.
6. **`glob`** (omp://tools/glob): дефолтный `limit` 200 (и макс 200), `hidden` по умолчанию **true**, `gitignore` по умолчанию true, внутренний таймаут 5 с (partial-результат вместо ошибки), несколько корней через `;` с пропуском отсутствующих (`missingPaths`), сортировка по mtime desc, выдача — префикс-свёрнутое дерево через `formatGroupedPaths()`. `parseFindPattern()`: голый `*.ts` превращается в `**/*.ts` от корня, `src/*.ts` остаётся не-рекурсивным. Нативный бэкенд — `crates/pi-natives/src/glob.rs` поверх `pi-walker`.
7. **`grep`** (omp://tools/grep + omp://natives-text-search-pipeline): натив `grep()` в `crates/pi-natives/src/grep.rs` — Rust regex → PCRE2 (lookaround/backreferences) → escape скобок → literal-фолбэк; брекеты вне квантификаторов (`${platform}`) санитизируются. Лимиты: 20 файлов/страница (`DEFAULT_FILE_LIMIT`), пагинация `skip` по файлам; 20 матчей/файл для мультифайл-скоупов, 200 для одиночного файла; внутренний cap 2000; строки до 512 символов; таймаут 30 с; файлы >4 MiB пропускаются (`MAX_FILE_BYTES`). Контекст по умолчанию `grep.contextBefore = 1`, `grep.contextAfter = 3`. Формат — `*LINE:content` (матч) / ` LINE:content` (контекст) под `[PATH#TAG]`; на рендер каждой файловой страницы инструмент снапшотит файл (`recordFileSnapshot()`) и минтит теги прямо для edit. Grep **никогда** не кэширует обход (`cache: false`).
8. **fs-scan cache** (omp://fs-scan-cache-architecture): общий Rust-кэш обхода в `crates/pi-walker/src/cache.rs` хранит **владельческие списки записей каталога** (не результаты инструментов). Ключ = канонизированный корень + полное `WalkOptions` (hidden/gitignore-политика, pruning `.git`/`node_modules`, symlinks, detail, depth, …) минус бит `cache` — любое отличие опций = отдельная партиция. Политика: `FS_SCAN_CACHE_TTL_MS` дефолт 1000, `FS_SCAN_EMPTY_RECHECK_MS` дефолт 200 (один recheck при пустом результате с ненулевым возрастом хита), `FS_SCAN_CACHE_MAX_ENTRIES` дефолт 16 (eviction oldest-first), пул Rayon `PI_WALK_WORKERS` дефолт 4. Инвалидация — `invalidateFsScanCache(path?)` (префиксный матч по корню) и обёртки `invalidateFsScanAfterWrite/Delete/Rename`, вызываемые всеми мутационными путями write/edit/patch/replace. Консьюмеры: glob и fuzzyFind — opt-in, astGrep/astEdit — always, grep — никогда.
9. **Него покрыто omp'ом**: отдельного «fs»-инструмента нет — glob и grep разделены по назначению (пути vs контент), что явно зафиксировано в промптах инструментов (omp://tools/glob, Notes).

## Hermes

Hermes покрывает тему на уровне toolset'ов и терминала, без hashline-аналогов.

1. **Инструменты организованы в toolsets, включаемые per-platform** (https://hermes-agent.nousresearch.com/docs/user-guide/features/tools). Категория Terminal & Files — `terminal`, `process`, `read_file`, `patch`; файловый инструментарий — это `read_file` + `patch` (unified-diff-патч), т.е. **нет** ни anchored-снапшотов, ни структурных резюме; включение — `hermes tools` / `hermes chat --toolsets "web,terminal"`. Из аннотаций результатов: `read_file` детектирует UTF-16 (BOM или байтовая эвристика, любой endianness) и **транскодирует в UTF-8** с пометкой-раскрытием вместо отказа; файлы >10 MB и настоящая бинарщина — отказ; смерть по сигналу расшифровывается человеческим текстом (`exit -9/137` → «terminated by signal 9: SIGKILL — often the kernel OOM killer…»).
2. **Семь бэкендов терминала за одним инструментом**: `local`, `docker`, `ssh`, `singularity`, `modal`, `daytona`, `vercel_sandbox` (та же страница). Конфиг `~/.hermes/config.yaml`: `terminal: { backend: local, cwd: ".", timeout: 180 }`. Docker-бэкенд — **один персистентный контейнер** на процесс (`docker run -d … sleep infinity`), все вызовы идут через `docker exec`; состояние `/workspace` переживает `/new`, `/reset` и сабагентов; hardening: read-only rootfs, сброс всех Linux capabilities, запрет привилегий, лимит 256 PID, полная namespace-изоляция. Это готовая модель «terminal backend как enum + транспорт».
3. **Фоновые процессы — отдельный инструмент `process` над сессиями** (та же страница): `terminal(command=..., background=true)` → `{"session_id": "proc_abc123", "pid": 12345}`, дальше `process(action="list"|"poll"|"wait"|"log"|"kill"|"write", session_id=...)`; `pty=true` включает интерактивные CLI (Codex, Claude Code). Управление не привязано к моменту вызова: poll/log/write доступны в произвольных следующих ходах.
4. **Неинтерактивная shell-гигиена** (та же страница, раздел Shell startup files): агент запускает shell без TTY, и док прямо требует non-interactive guard вверху `.bashrc` (`case $- in *i*) ;; *) return;; esac`), вынося тяжёлую инициализацию (nvm и пр.) под guard — иначе каждая команда агента получает multi-second латенс или зависание. Дополнение: sudo-запросы кэшируются на сессию, либо `SUDO_PASSWORD` в `~/.hermes/.env`.

Hermes не покрывает: anchored-редактирование с хеш-тегами, структурные резюме read, общий fs-scan cache. Ближайший аналог его `patch` (unified diff против актуального содержимого); по кэшу аналога нет вовсе.

## Vellum

По материалу `local://vellum-summary.md` Vellum **не покрывает** базовые файловые инструменты (read/edit/glob/grep/bash как таковые не описаны). Ближайшие аналоги — уровень политики исполнения, а не механики инструментов:

1. **Sandbox per tool call + deny by default**: «Каждый вызов инструмента — в песочнице. По умолчанию — deny.» — модель одобрения, комплементарная omp'овской `bash.patterns`/approval tier (источник: local://vellum-summary.md, раздел «Безопасность»).
2. **Учётные данные вне модели**: «Учётные данные живут в отдельном процессе и никогда не попадают в модель» — прямой аргумент против прокидывания секретов в `env`-параметр bash-вызова; секреты должны резолвиться на стороне рантайма (тот же источник).
3. **Actor identity как гейт инструментов**: unknown-акторы «не могут … триггерить инструменты или эскалировать» — относится к мультиботной теме, но фиксирует принцип: права на инструмент зависят от того, кто вызвал, а не только от настроек сессии.

## Решение (одно/комбо)

База — omp: его связка «read с hashline-анкерами → edit по тегам → write, минтящий свежий тег» — самая токен-эффективная модель редактирования из трёх (правим только показанные строки, без перечитывания и без diff-шума Hermes-`patch`), а снапшот-стор (256 путей × 4 версии) уже решает проблему устаревших анкеров через recovery-цепочку. Комбо-добавка №1 (Hermes) — проектировать `bash` сразу поверх трейта `TerminalBackend` (local/docker/ssh/…): механика omp'овского executor'а не меняется, меняется только транспорт, и «каждый вызов в песочнице» из Vellum достаётся дёшево через backend `docker` без переписывания инструмента. Комбо-добавка №2 (Vellum) — deny-by-default как дефолт политики одобрения для exec-эффектов, с allow-листом поверх: у omp есть готовый механизм `bash.patterns`, инвертируется только дефолт. Для эффективности: grep — как в omp, без fs-scan cache (кэш выигрывает только на повторных обходах одного корня, а grep-паттерны уникальны), glob — с общим кэшем TTL 1 c и инвалидацией на запись; natives — обычные крейты workspace, а не FFI-слой (в Rust-клоне N-API-граница omp — чистый оверхед). От Hermes дополнительно берём транскодирование UTF-16 → UTF-8 в read: одна проверка BOM снимает целый класс «бинарный файл»-ложных отказов на Windows-артефактах.

## Rust-маппинг

**Крейты workspace**: `titi-tools` — реализации трейта `Tool` (read/edit/write/bash/glob/grep) и их схем; `titi-core` — `Session`, реестр инструментов, snapshot store, политика одобрения, artifact store; `titi-providers` — без изменений по теме; **новый `titi-fs`** — walker + fs-scan cache + glob/grep-нативы (в omp это pi-walker/pi-natives через N-API; у нас — прямые крейты); **новый `titi-hashline`** — грамматика, парсер, apply, recovery, snapshot-теги.

Ключевые типы (эскизы):

```rust
// titi-core
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn schema(&self) -> &ToolSchema;
    async fn execute(&self, args: ToolArgs, ctx: &SessionCtx,
                     signal: CancellationToken) -> Result<ToolResult, ToolError>;
}

pub struct FileSnapshotStore { /* LRU: 256 путей, 4 версии/путь, cap 4 MiB */ }
impl FileSnapshotStore {
    pub fn record(&self, path: &Path, lines: &[String]) -> Tag; // 4-hex от sha256(normalized)
    pub fn recover(&self, path: &Path, tag: Tag) -> Option<&Snapshot>; // для stale-tag recovery
}

// titi-hashline
pub enum Op { PutRange{span: LineSpan, body: Vec<String>}, PutBlock{anchor: usize, body: Vec<String>},
              Insert{after: Anchor, body: Vec<String>}, Cut{span: LineSpan, reg: Option<Reg>},
              Paste{gap: Gap, reg: Reg}, Rem, Mv(PathBuf) }
pub fn apply(patch: &str, store: &FileSnapshotStore, cwd: &Path)
    -> Result<ApplyReport, ApplyError>; // валидация тегов/диапазонов до первой записи

// titi-fs
pub struct WalkOptions { hidden: bool, gitignore: bool, follow_links: bool,
                         detail: Detail, max_depth: Option<u32>, /* …все поля участвуют в ключе */ }
pub struct ScanCache { map: DashMap<CacheKey, CachedScan>, ttl: Duration, max_entries: usize }
pub fn glob(pattern: &Glob, root: &Path, opts: GlobOpts) -> Result<Vec<Match>, ToolError>;
pub fn grep(pattern: &Pattern, scope: &Scope, ctx: GrepCtx) -> Result<GrepResult, ToolError>;
// Pattern: enum { Rust(regex::Regex), Pcre(pcre2::Regex), Literal(String) } — каскад omp

// titi-tools
pub struct BashTool { backend: Box<dyn TerminalBackend>, sessions: ShellSessionPool }
#[async_trait::async_trait]
pub trait TerminalBackend: Send + Sync {          // компромисс из Hermes
    async fn run(&self, req: ShellRequest<'_>) -> Result<RunHandle, ToolError>;
}
pub struct OutputSink { tail: RingBuf<50KiB>, head: Option<HeadWindow>, artifact: Option<ArtifactSink> }
pub struct AsyncJobManager { jobs: Mutex<HashMap<JobId, Job>>, running_cap: usize }
```

**Внешние крейты**: `tokio` (процессы, таймауты, `CancellationToken`), `portable-pty` + `crossterm` (PTY-режим bash и TUI-оверлей titi-tui), `regex` + `pcre2` (каскад матчеров grep), `ignore` + `globset` + `walkdir` (walker с .gitignore-политикой в titi-fs), `dashmap` (fs-scan cache), `lru` (snapshot store), `tree-sitter` (блочные анкеры `PUT N*:` в edit), `rusqlite` (read/write SQLite-веток), `tar` + `zip` + `flate2` (архивы read/write), `serde_json` + `json5` (SQLite-строки), `sha2` (snapshot-теги), `pest` или `lalrpop` (каноничная грамматика hashline — аналог `grammar.lark`).

## Definition of Done

- [ ] `titi-cli` в сессии экспонирует read/edit/write/bash/glob/grep; вызов каждого инструмента возвращает результат, формат которого покрыт golden-тестом в `titi-tools/tests/`.
- [ ] `read src/foo.rs:50-200` возвращает заголовок `[src/foo.rs#TAG]` с пронумерованными строками; TAG зарегистрирован в `FileSnapshotStore` и принимается следующим `edit`; для файла >100 строк read возвращает структурное резюме с футером конкретных elided-диапазонов, и повторный `read` по каждому диапазону из футера успешен.
- [ ] `edit` отклоняет (юнит-тесты `titi-hashline::apply`): неверный/неизвестный TAG, правку по строке вне записанного видимого диапазона, перекрывающиеся диапазоны, byte-identical патч; stale-tag восстанавливается через recovery-цепочку или возвращает mismatch.
- [ ] `bash`: команда с `timeout: 2` и `sleep 5` завершается по таймауту с `details.timedOut = true`; `async: true` возвращает job id и доставляет результат завершения через job manager; два параллельных не-PTY вызова не разделяют одну shell-сессию (второй идёт в one-shot); PTY-режим отклоняется без UI-контекста с notice.
- [ ] `glob` по умолчанию уважает `.gitignore` (и видит игнорируемое при `gitignore: false`); `grep` с lookaround-паттерном матчит через PCRE2-фолбэк, возвращает ≤20 файлов на страницу и продолжает с `skip=<N>`.
- [ ] `write` инвалидирует fs-scan cache: интеграционный тест «write нового файла → немедленный glob его видит» без ручной инвалидации.
- [ ] Политика `bash.patterns` с правилом `deny` блокирует и полную команду, и сегменты составной команды (`cd /tmp && rm -rf build`); дефолт для exec-эффектов — deny-until-allowlisted (решение из раздела «Решение»).
- [ ] Файл в UTF-16 (BOM) читается `read` как UTF-8 с пометкой о транскодировании (перенос из Hermes).

## Deep-dive

План подсистем-доков второго уровня (`docs/research/tools-core/`):

- `read-anchored-snapshots.md` — грамматика селекторов, структурные резюме, snapshot store, отображение контекста.
- `edit-hashline.md` — каноничная грамматика патчей, tree-sitter блочные анкеры, регистры, no-op/loop-гарды.
- `write-dispatch.md` — ветвление path (файл/архив/sqlite/internal URL), генерация свежих тегов, гард от mis-dispatch.
- `bash-runtime-pty.md` — executor, пул shell-сессий, non-interactive env, direnv, OutputSink и спилл артефактов.
- `bash-background-jobs.md` — explicit async, авто-бэкграунд, job manager, доставка результатов.
- `glob-grep-native-search.md` — walker-политики, каскад regex-движков, лимиты страниц и пагинация.
- `fs-scan-cache.md` — ключ партиций, TTL/empty-recheck/eviction, контракты инвалидации.
