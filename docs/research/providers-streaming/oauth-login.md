# OAuth-вход в провайдера (reference: omp)

Как харнессы получают подписочный доступ (Claude Pro/Max, ChatGPT Plus/Pro) без API-ключа: authorize-URL + PKCE, локальный callback-сервер, обмен кода на токены, refresh, хранение идентичности. Референс разобран по исходникам omp (`@oh-my-pi/*`, MIT) — установлены локально как исходники, не минифицированный бандл.

## omp

Декларативная модель: поток описан правилом `pi-catalog/src/compat/rules/auth/<id>.kdl`, правило компилируется в `CompiledAuthProvider` (`pi-catalog/src/compat/types.ts:523-548`), из него `buildProviderDefinition()` собирает runtime-дескриптор с ленивыми функциями `login`/`refreshToken` (`pi-ai/src/registry/build.ts:20-73`). Движки потоков — в `pi-ai/src/registry/engine/`; провайдер-специфичные модули (`oauth/anthropic.ts`, `oauth/openai-codex.ts`) дают только post-exchange хуки идентичности.

1. **Authorize-URL.** Собирается `CompiledOAuthCodeLogin` (`types.ts:456-485`): стандартные параметры `client_id`, `response_type=code`, `redirect_uri`, `scope`, `code_challenge`, `code_challenge_method=S256`, `state`, затем правило `authorize-params` (`engine/oauth-code.ts:121-160`). PKCE — 96 случайных байт → base64url verifier, SHA-256 → base64url challenge (`oauth/pkce.ts:5-17`). `state` по умолчанию — 16 случайных байт шестнадцатеричными парами (`oauth/callback-server.ts:191-206`).
2. **Callback-сервер.** `TcpListener` на `hostname` (по умолчанию `localhost`, то есть IPv4 + IPv6, если доступен) и `port` из правила; свободный порт вместо занятого берётся только если правило не запретило (`port-fallback #false` — тогда `ConfigurationError`); таймаут ожидания 300 с (`callback-server.ts:14-35,392-442,482-571,629-685`). Маршруты: путь правила (по умолчанию `/callback`) читает `code`/`state`/`error`/`error_description`; несовпадение state → `State mismatch - possible CSRF attack`; `/launch` отвечает `302` на pending authorize-URL; прочее — `404` (`callback-server.ts:572-628`). Страница успеха вызывает `window.close()` (`oauth/oauth.html:303-338`). Движок **не** открывает браузер сам — только публикует URL через `OAuthController.onAuth` (`callback-server.ts:220-287`).
3. **Ручная вставка.** Параллельно с callback ждётся `onManualCodeInput`; принимается полный redirect-URL, query-строка с `code=` или сам код (опционально `code#state`) — `parseCallbackInput` (`callback-server.ts:629-685`).
4. **Обмен кода.** POST на `token.url`, тело `form` либо `json` (`Content-Type` соответствующий), параметры `grant_type=authorization_code`, `client_id`, `client_secret`, `code`, `redirect_uri`, `code_verifier` + параметры правила (у Anthropic — `state`); таймаут 30 с по умолчанию (`engine/common.ts:207-274`; `engine/oauth-code.ts:179-216`). Маппинг ответа → креды: `access`, `refresh` (при отсутствии сохраняется прежний), `expires` (секунды → мс, минус per-provider skew), плюс `email`/`accountId`/`orgId`/`orgName`/`projectId` (`engine/common.ts:128-184`).
5. **Anthropic (Claude Pro/Max).** `rules/auth/anthropic.kdl:1-52`: client-id `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (в KDL в base64), `https://claude.ai/oauth/authorize`, scopes `org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload`, `code=true`, callback `http://localhost:54545/callback`, токен `https://api.anthropic.com/v1/oauth/token` (`body=json`, лишний параметр `state={state}`), expiry `expires_in` секунд со skew 300 000 мс. Refresh — тот же URL, `body=json`, с заголовками `anthropic-beta: oauth-2025-04-20` и `User-Agent: anthropic-sdk-typescript/{sdk} userOAuthProvider`; поля org на refresh не перезаписываются (сливаются из сохранённой строки). Хук идентичности — `GET https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli&model=<model>` с `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`, читает `oauth_account.{account_uuid,account_email,organization_uuid,organization_name}` (`oauth/anthropic.ts:15,47-119`).
6. **OpenAI Codex (подписка).** `rules/auth/openai-codex.kdl:1-35`: client-id `app_EMoamEEZ73f0CkXaXp7hrann`, `https://auth.openai.com/oauth/authorize`, scopes `openid profile email offline_access api.connectors.read api.connectors.invoke`, PKCE, доп. параметры `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `originator=<app>`; callback жёстко `http://localhost:1455/auth/callback` (fallback портов запрещён — OpenAI вайтлистит ровно этот URI); токен `https://auth.openai.com/oauth/token` (`body=form`, таймаут 15 с), skew 0. Идентичность — из JWT access/id-token: claim `https://api.openai.com/auth.chatgpt_account_id`, `https://api.openai.com/profile.email`, `chatgpt_plan_type` (accountId = orgId = account id, orgName = plan) (`oauth/openai-codex.ts:13-67,95-121`). Headless-вариант — device-flow: `POST https://auth.openai.com/api/accounts/deviceauth/usercode` (JSON `{client_id}`), опрос `POST .../deviceauth/token` (`{device_auth_id,user_code}`; 403/404 = pending), обмен полученного `authorization_code`+`code_verifier` на токен с `redirect_uri=https://auth.openai.com/deviceauth/callback`; интервал 5 с, страховочная надбавка 3 с, максимум 120 опросов (`oauth/openai-codex.ts:8-25,161-258`).
7. **Refresh при запросе.** Креды обновляются в момент резолва, если `now + 60_000 >= expires` (`auth/refresh.ts:20-35,388-393`). Single-flight в процессе по id строки + межпроцессный lease в SQLite (TTL 15 с, продление 5 с, опрос 50 мс), запись обратно через compare-and-set; при потере lease — ошибка, при гонке с другой ротацией — перечитывание строки (`auth/refresh.ts:65-124,168-267`). Терминальный отказ refresh (`invalid_grant` и т.п.) помечает строку `disabled_cause`, а не удаляет; транзиентный — бэкофф 5 минут и креды остаются (`auth/select.ts:1035-1065`).
8. **Хранение.** SQLite `auth_credentials(id, provider, credential_type, data, disabled_cause, identity_key, created_at, updated_at)`; OAuth-`data` — JSON `{refresh, access, expires, email?, accountId?, orgId?, orgName?, authorizedAt?, …}` в **миллисекундах** (`auth/sqlite-credential-store.ts:64-72,127-132,714-722`). Порядок резолва: runtime `--api-key` → `models.yml` `apiKey` → stored OAuth (с refresh) → login-ключ → env → прочие stored-ключи (`auth/cascade.ts:287-353`).
9. **Терминальный UX.** `omp login [provider]`: список провайдеров нумерацией, при отсутствии аргумента — выбор номером; вывод `Open this URL in your browser:` + URL, при `openBrowser` вызывается системный opener; при `PASTE_CODE_LOGIN_PROVIDERS` — приглашение `Paste the authorization code (or full redirect URL):`; успех — `Logged in to {name} as {email|accountId} ({org})` (`cli/oauth-terminal.ts:98-224`, `cli/login-cli.ts:35-65`).
10. **Инференс на OAuth-токене (Anthropic).** Токен идёт в `Authorization: Bearer`, `x-api-key` подавляется; добавляются `anthropic-beta` из профиля Claude Code (`claude-code-20250219`, `oauth-2025-04-20`, `interleaved-thinking-2025-05-14`, `thinking-token-count-2026-05-13`, `context-management-2025-06-27`, `prompt-caching-scope-2026-01-05`, `mid-conversation-system-2026-04-07`, плюс `effort-2025-11-24` и `fallback-credit-2026-06-01`), `User-Agent: claude-cli/<версия> (external, cli)`, `anthropic-dangerous-direct-browser-access: true`, `x-app: cli`, `Accept: application/json` и набор `X-Stainless-*` (`providers/anthropic.ts:226-253,290-403,590-599`; `providers/claude-code-fingerprint.ts:17,35-37`). Codex-инференс идёт на тот же ChatGPT-origin, путь `/backend-api/codex/responses`, с заголовком `chatgpt-account-id` из кредов (`providers/openai-codex-responses.ts:4858-4860`; `auth/types.ts:975`).

## Hermes

- OAuth-креды — отдельный файл `~/.hermes/auth.json` (не `.env`); Codex device-code импортируется из `~/.codex/auth.json`; Nous Portal — scoped `inference:invoke` JWT с fallback на session-key; Qwen/MiniMax/xAI — браузерный PKCE (https://hermes-agent.nousresearch.com/docs/integrations/providers).
- Ревокнутый refresh-токен карантинится против replay-лупов, терминальная ошибка refresh помечает токен мёртвым и прекращает реплей (там же).
- Anthropic OAuth предпочитает credential store Claude Code, а не копирование токена в `.env` (там же).

## Vellum

- «Учётные данные живут в отдельном процессе и никогда не попадают в модель» (`local://vellum-summary.md`).
- «OAuth без самописного refresh» у потребителя: refresh сосредоточен в одном компоненте, потребитель получает готовый access-токен (там же).

## Решение (одно/комбо)

База — omp: декларативный дескриптор провайдера (authorize-URL, scopes, callback, token-request, маппинг кредов, skew), PKCE S256 + локальный callback + ручная вставка кода как параллельный путь, refresh по `expires` со skew при резолве, отдельная строка хранилища на аккаунт. Из Hermes — карантин мёртвых refresh-токенов и импорт уже существующих кред (позже). Из Vellum — refresh-материал не выходит за пределы хранилища и не попадает в модель (в titi это уже зафиксировано типом `titi_providers::Credential` без refresh-поля). Отличие от omp: KDL-компилятора нет — дескрипторы живут как константы Rust, правило одно на провайдера.

Атрибуция: omp и `@oh-my-pi/pi-catalog` распространяются под MIT (Stencil Labs, Inc.); переносимые константы (client-id, URL, scopes, beta-заголовки) — это факты протокола, но ссылка на источник обязательна в комментарии модуля.

## Rust-маппинг

Крейты: поток и HTTP — в `titi-providers` (там уже есть `HttpFetch`/`MockFetch` для инъекции, `reqwest`, `tokio net`), хранение — в `titi-secrets` (текущий `auth.db`), проводка — `titi-engine` (резолв и refresh-свип), поверхность — `titi-cli`. Новая зависимость одна: `sha2` (уже в workspace-дереве); base64url/percent-encoding — свои (~40 строк), случайность — `/dev/urandom`, как в `titi-core/src/share.rs:272-279`.

```rust
// crates/titi-providers/src/oauth/provider.rs — дескриптор одного провайдера
pub struct OAuthProvider {
    pub id: &'static str,                 // "anthropic"
    pub name: &'static str,               // "Anthropic (Claude Pro/Max)"
    pub store_as: &'static str,           // id строки в auth.db
    pub client_id: &'static str,
    pub authorize_url: &'static str,
    pub scopes: &'static [&'static str],
    pub authorize_params: &'static [(&'static str, &'static str)],
    pub callback: CallbackSpec,           // { host, port, path, redirect_uri, port_fallback }
    pub token: TokenSpec,                 // { url, body: Form|Json, params, headers, timeout }
    pub refresh_skew_secs: i64,           // 300 anthropic, 0 codex
    pub identity: IdentitySource,         // AnthropicBootstrap | JwtClaims
    pub instructions: &'static str,
}
pub fn builtin() -> &'static [OAuthProvider];    // anthropic, openai-codex

// crates/titi-providers/src/oauth/mod.rs — то, что видит поверхность
pub struct OAuthTokens { pub access: String, pub refresh: Option<String>, pub expires_at: Option<i64>,
                         pub account_id: Option<String>, pub email: Option<String>,
                         pub org_id: Option<String>, pub org_name: Option<String> }
pub trait OAuthUi: Send + Sync {
    fn on_auth(&self, url: &str, instructions: &str);
    fn on_progress(&self, message: &str);
    /// Параллельный callback-серверу путь: ждать ручную вставку кода/URL.
    fn manual_code(&self, deadline: std::time::Instant) -> Option<String>;
}
pub async fn login(p: &OAuthProvider, fetch: &dyn HttpFetch, ui: &dyn OAuthUi) -> Result<OAuthTokens, OAuthError>;
pub async fn refresh(p: &OAuthProvider, tokens: &OAuthTokens, fetch: &dyn HttpFetch) -> Result<OAuthTokens, OAuthError>;
pub fn parse_callback_input(input: &str) -> (Option<String>, Option<String>); // (code, state)

// crates/titi-secrets/src/store.rs — схема v3 (миграция как v1→v2)
// ALTER: credentials += refresh_token TEXT, account_id TEXT, email TEXT,
//        org_id TEXT, org_name TEXT, authorized_at INTEGER
pub struct Credential { /* … + refresh_token: Option<String>, identity: Option<Identity> */ }
impl AuthStore { pub fn store_oauth(&self, provider: &str, label: &str, tokens: &OAuthTokens) -> Result<()>; }

// crates/titi-engine/src/registry.rs — refresh на старте хода, вне синхронной лестницы
impl LayeredCredentialSource {
    /// Обновляет строки, у которых `expires_at - skew <= now`; терминальный 4xx —
    /// карантин строки, транзиентная ошибка — строка остаётся.
    pub async fn refresh_due(&self, providers: &[OAuthProvider], fetch: &dyn HttpFetch) -> Vec<RefreshOutcome>;
}

// crates/titi-providers/src/wire.rs — авторизация по типу креды, не только по ApiKind
pub struct RequestCtx { pub api_key: Option<SmolStr>, pub kind: CredKind, /* … */ }
pub fn build_http_request(api: ApiKind, base_url: &str, req: &WireRequest, cred: Option<(&str, CredKind)>) -> HttpRequest;
```

Поверхность CLI: `titi --login [provider]` (без аргумента — список), `/login <provider>` в TUI запускает поток на runtime без блокировки рендера (существующий паттерн `tokio::sync::mpsc` + `TryRecvError` в `chat.rs`), вставка кода идёт в тот же режим ввода, что и ключ; `/login <provider> <key>` по-прежнему кладёт API-ключ; `/keys` показывает `oauth` с остатком жизни.

## Definition of Done

- [ ] PKCE: verifier 96 случайных байт → base64url, challenge = base64url(SHA-256(verifier)); тест на векторе RFC 7636 (verifier `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk` → challenge `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`).
- [ ] Callback-сервер: слушает заданный порт/путь, отдаёт `code` и совпавший `state`; чужая строка state → отказ; `error` → отказ с текстом провайдера; прочий путь → 404; занятый порт без `port_fallback` → ошибка конфигурации (тесты на реальном `TcpListener` на 127.0.0.1:0).
- [ ] Ручная вставка: полный redirect-URL, query-строка, голый код, `code#state` — `parse_callback_input` (unit-тесты).
- [ ] Обмен кода: Anthropic — `Content-Type: application/json`, тело с `grant_type=authorization_code`, `state`, `code_verifier`; Codex — `application/x-www-form-urlencoded`; тест на локальном стенде проверяет метод, URL, заголовки и тело, а также маппинг `expires_at` = now + `expires_in` − skew.
- [ ] Идентичность: Anthropic — bootstrap-запрос даёт email/account/org, отсутствие ответа не ломает вход; Codex — claim `chatgpt_account_id` из JWT даёт accountId/orgId, email из `profile.email` (тесты на подложенном JWT и на стабе bootstrap).
- [ ] Refresh: `expires_at − skew ≤ now` → обмен `grant_type=refresh_token` с сохранением org-идентичности из прежней строки; терминальный 4xx удаляет строку (карантин), транзиентная ошибка оставляет; тест на стабе.
- [ ] Хранилище v3: миграция v2→v3 сохраняет существующие строки; `store_oauth`/`get` возвращают refresh и идентичность; `Debug` по-прежнему маскирует токены (тест).
- [ ] Провод: `CredKind::BearerToken` + `anthropic-messages` → `Authorization: Bearer` и `anthropic-beta` без `x-api-key`; `ApiKind` с `ApiKey` → прежнее поведение (тест на `build_http_request`).
- [ ] Поверхность: `/login anthropic` печатает URL и переходит в режим вставки кода, Esc отменяет без записи; `--login` без аргумента печатает список провайдеров; `/keys` различает `oauth` и `api_key` (интеграционные тесты titi-cli).
- [ ] Живая проверка владельцем: вход в Anthropic по подписке и один ход модели на OAuth-токене (нужен реальный браузер и подписка; запись в `docs/QA_STATUS.md`).

## Deep-dive

Подсистемы темы: [auth-credentials.md](./auth-credentials.md) (лестница резолва и хранение), [transports.md](./transports.md), [streaming-events.md](./streaming-events.md). Место OAuth в лестнице и карантин мёртвых refresh-токенов описаны в `auth-credentials.md`; здесь — сам поток входа.

Не входит в первый срез: auth-broker (`titi creds serve`) и forward-gateway, ротация мульти-аккаунтов и бэкоффы лимитов, device-flow Codex, импорт кред Claude Code/Codex CLI, инференс на подписочном токене Codex (`/backend-api/codex/responses`).
