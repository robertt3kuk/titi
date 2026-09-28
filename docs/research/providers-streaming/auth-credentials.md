# Аутентификация и учётные данные провайдеров

Откуда берутся API-ключи и OAuth-токены, в каком порядке резолвятся, где хранятся, как refreshed, и как вынести креды из процесса агента (auth-broker/gateway), чтобы модель и код инструментов их никогда не видели.

## omp

1. **Ladder резолва кредов, первый матч побеждает** (omp://providers.md): (1) runtime `--api-key` (никогда не персистится) → (2) `models.yml` `apiKey` (намного бьёт stored OAuth, чтобы прокси не получил upstream OAuth-токен) → (3) stored OAuth (refresh по необходимости; несколько аккаунтов ранжируются и ротируются) → (4) ключ, сохранённый `/login` → (5) env-переменная провайдера (включая `.env`) → (6) прочие stored ключи (broker-migrated) → (7) fallback-резолвер `models.yml`. Хранилище: `~/.omp/agent/agent.db`; `disabledProviders` проверяется ДО кредов.
2. **Env и `.env`-прекеденс**: процесс-env > `<cwd>/.env` > `~/.omp/agent/.env` > `~/.omp/.env` > `~/.env`; уже выставленная переменная не перезаписывается; минимальный парсер (shell-identifier ключи, кавычки стрипаются, NUL отбрасывается). Полная таблица провайдер→env: `anthropic`=`ANTHROPIC_OAUTH_TOKEN`/`ANTHROPIC_API_KEY`, `google`=`GEMINI_API_KEY`, `google-vertex`=ADC (`GOOGLE_APPLICATION_CREDENTIALS`+project+location), `amazon-bedrock`=AWS chain (omp://providers.md).
3. **OAuth-потоки per-provider**: Anthropic PKCE S256 через `claude.ai/oauth/authorize` с grant TTL 30 дней (ежемесячный интерактивный re-login) и квота-трекингом `api/oauth/usage` (five_hour/seven_day окна) с авторотацией кредов при `QUOTA_EXHAUSTED`; Codex OAuth device-code + PKCE, refresh через `auth.openai.com/oauth/token`, ротация аккаунтов с изоляцией лимитов 5h/7d от Spark-метров; лимит-бэкоффы классифицированы: QUOTA_EXHAUSTED 30m, RATE_LIMIT 30s, CONCURRENT 5s, MODEL_CAPACITY 45s±15s (omp://provider-quirks.md).
4. **Auth-broker + auth-gateway**: `omp auth-broker serve` держит канонический SQLite-вёлт и единолично владеет refresh-токенами; клиенты получают redacted snapshot, где каждый `refresh` заменён на `REMOTE_REFRESH_SENTINEL`, а при истечении access-токена клиент зовёт `POST /v1/credential/:id/refresh`; `RemoteAuthCredentialStore` отвергает локальные мутации. `omp auth-gateway serve` — forward-proxy (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/pi/stream`), который сам резолвит креду через broker и диспетчерит через `streamSimple()` — «no raw provider passthrough» (omp://auth-broker-gateway.md).
5. **Кэш и кэширование usage**: снапшот брокера шифруется AES-256-GCM ключом `SHA-256(OMP_AUTH_BROKER_TOKEN)` с broker-URL как AAD, пишется атомарно mode `0600`, TTL 1h, свежий кэш ревалидируется с бюджетом 500ms; usage-отчёты кэшируются серверно на 5 мин с ±25% джиттером (анти-429), клиентски single-flight на 15s; account pool (`OMP_AUTH_BROKER_ACCOUNT_POOL_FILE`) — routing-политика, не авторизация (omp://auth-broker-gateway.md).

## Hermes

1. **Разделение секретов и конфига**: секреты (API-ключи, токены, пароли) — только `~/.hermes/.env`; остальное — `config.yaml`; прекеденс CLI args > config.yaml > .env > defaults; `hermes config set OPENROUTER_API_KEY ...` автоматически маршрутизирует ключи в `.env`; в config.yaml работает `${VAR}`-подстановка и Cursor-совместимый `${env:VAR}` SecretRef (внешние бэкенды `${file:}`/`${vault:}` инлайн не резолвятся) (https://hermes-agent.nousresearch.com/docs/user-guide/configuration).
2. **OAuth-креды в `~/.hermes/auth.json`**: Nous Portal (scoped `inference:invoke` JWT с fallback на opaque session-key, ревокнутые refresh-токены карантинятся против replay-лупов), Codex device-code (импорт из `~/.codex/auth.json`; терминальная ошибка refresh помечает токен мёртвым и прекращает реплей), Qwen/MiniMax/xAI browser PKCE; Anthropic OAuth предпочитает credential store Claude Code вместо копирования токена в `.env`; Copilot — только `gho_*`/`github_pat_*`/`ghu_*` (классические `ghp_*` PAT API не поддерживает) (https://hermes-agent.nousresearch.com/docs/integrations/providers).
3. **Ротация и восстановление**: Copilot на 401 делает one-shot recovery — ре-резолв токена по цепочке env → пересборка клиента → один повтор; Vertex минтит короткоживущий OAuth2-токен (~1h) из service-account JSON/ADC и автоперевыпускает, включая re-mint при 401 посреди сессии (https://hermes-agent.nousresearch.com/docs/integrations/providers).

## Vellum

Харнес тему хранения кредов покрывает принципиально (это его сильная сторона):

1. **«Учётные данные живут в отдельном процессе и никогда не попадают в модель»** (local://vellum-summary.md, «Безопасность») — прямое требование к titi: credential store — отдельный процесс/актор с минимальным интерфейсом «дай bearer для провайдера X».
2. **Actor identity (guardian, trusted, unknown) резолвится один раз и соблюдается всюду; unknown не может триггерить инструменты или эскалировать; default-deny** (local://vellum-summary.md). Для кредов: запрос токена аутентифицирован идентичностью запрашивающего бота; unknown-актор токены не получает.
3. **OAuth «без самописного token refresh»** (local://vellum-summary.md, «OAuth») — refresh сосредоточен в одном компоненте (у titi — credential-актор), потребители только потребляют готовые access-токены.

## Решение (одно/комбо)

Комбо: **ladder omp + изоляция кредов Vellum + карантин refresh-токенов Hermes**. Резолв-лестницу omp берём целиком (она решает реальные конфликты: config-ключ против OAuth-прокси, env против мигрированного ключа), но кладём её в отдельный credential-актор/процесс — как broker у omp и credential-процесс Vellum: модельный слой получает opaque-токен через async-интерфейс и типически не имеет доступа к refresh-материалу. Refresh-логика — единственная в акторе (Vellum: «без самописного refresh» у потребителя), с разделением definitive/transient отказов omp-брокера и карантином мёртвых refresh-токенов Hermes (терминальная 4xx → токен помечен мёртвым, реплей прекращён). Ранжирование/ротация мульти-аккаунтов с бэкофф-классификацией (QUOTA/CONCURRENT/CAPACITY) — у omp и переносится как есть. Для мультиботности titi: pool-файл omp (`ACCOUNT_POOL_FILE`) естественно расширяется до per-bot pools — боту видны только его аккаунты.

## Rust-маппинг

Крейты: `titi-auth` (новый: credential-актор), `titi-providers` (консюмер). Внешние: `tokio` (актор на `mpsc`/`oneshot`), `rusqlite` (хранилище кредов), `ring` или `aes-gcm` (шифрование локального кэша), `oauth2`-крейт или самописный PKCE (S256 — `sha2` + `base64url`), `secrecy` (`SecretString` для токенов), `dirs` (пути `~/.titi`).

```rust
// titi-auth: актор кредов — единственный владелец refresh-материала
pub enum CredentialRequest { Resolve { provider: SmolStr }, Refresh { id: CredentialId } }
pub enum CredentialResponse { Token(SecretString), Unavailable { hint: LoginHint } }

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn resolve(&self, provider: &str, req: LadderCtx) -> Option<Credential>;
}
pub struct LadderCtx { pub runtime_override: Option<SecretString>,   // --api-key
                       pub config_key: Option<SecretString>,         // models.yml
                       pub env: Option<SecretString> }

pub struct Credential { pub access: SecretString, pub kind: CredKind } // НЕТ refresh-поля: типически скрыт

pub struct ActorStore { db: rusqlite::Connection } // sqlite в отдельном треде/процессе
impl ActorStore {
    /// definitive (invalid_grant/revoked) -> quarantine; transient (timeout) -> retry next sweep
    async fn refresh_sweep(&mut self, skew: Duration) -> Vec<CredentialId>;
    fn rank_oauth(&self, provider: &str) -> Vec<CredentialId>; // ротация мульти-аккаунтов
}

// тит-providers: консюмер получает только opaque token
pub struct BearerInjector { store: std::sync::Arc<dyn CredentialStore> }

// per-bot routing (расширение pool-файла omp)
pub struct BotAccountPool { map: FxHashMap<BotId, FxHashMap<ProviderId, Vec<IdentityKey>>> }
```

## Definition of Done

- [ ] Ladder-тест: 7 уровней приоритета проверены pairwise-сценариями (runtime > config > OAuth > login-key > env > stored > fallback); `disabledProviders` отбрасывает провайдера до обращения к кредам (тест).
- [ ] Тип `Credential` не содержит refresh-токена; refresh-материал достижим только внутри `titi-auth::ActorStore` (тест на публичный API крейта + `compile_error`-гейт по линтеру видимости полей).
- [ ] Refresh: `invalid_grant`/`revoked` переводит креду в карантин и убирает из выборки; сетевой timeout оставляет её активной (тест на фейковом токен-эндпоинте).
- [ ] Ротация: при исчерпании лимита у аккаунта A (`QUOTA_EXHAUSTED`) запросы уходят аккаунту B с бэкоффом согласно классификации (30m/30s/5s/45s — тест-таблица).
- [ ] Env/`.env`: уже выставленная в процессе переменная не перезаписывается файлами `<cwd>/.env` > `~/.titi/.env` > `~/.env` (тест на временных директориях).
- [ ] Локальный кэш снапшота шифруется AES-256-GCM с ключом из токена и AAD=URL; подмена URL или токена делает кэш нечитаемым, а не тихо применяемым (тест).
- [ ] Keyless-обнаружение: Ollama (`OLLAMA_BASE_URL`, дефолт `127.0.0.1:11434`) и LM Studio (`127.0.0.1:1234/v1`) без кредов попадают в каталог, как только эндпоинт отвечает (интеграционный smoke).
- [ ] Per-bot pool: бот с ограниченным пулом не получает OAuth-креды вне своего списка; unknown-бот получает отказ (тест мультиботного запроса к актору).

## Deep-dive

Написанные подсистемы: [transports.md](./transports.md), [streaming-events.md](./streaming-events.md), [oauth-login.md](./oauth-login.md) (сам поток входа: authorize+PKCE, локальный callback, обмен кода, refresh, идентичность, инференс на OAuth-токене).

План 2-го уровня: `docs/research/providers-streaming/secrets-hygiene.md` — маскирование токенов в логах/ошибках (omp secrets.md), безопасное логирование HTTP-дампов; `docs/research/providers-streaming/broker-design.md` — полный дизайн credential-актора titi: схема SQLite, протокол снапшота, SSE-дельты, мультиботные пулы.
