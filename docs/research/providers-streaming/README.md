# Провайдеры и стриминг

> Сравнительный research 2026-08. Продуктовые решения — этот README и [карта тем](../README.md). Действующий registry: `titi-engine::ProviderRegistry` резолвит descriptor + transport + credential. Retry/fallback принадлежит engine и запрещён после visible delta.

Исследование каркаса модельных провайдеров: как харнессы организуют маршрутизацию запросов к LLM-бэкендам (Anthropic, OpenAI, Gemini, OpenRouter, локальные движки), чем различаются их транспорта (SSE / WebSocket / REST), как нормализуется поток событий, где живут учётные данные и как устроены retry/fallback-цепочки.

Подсистемные доки: [transports.md](./transports.md), [streaming-events.md](./streaming-events.md), [auth-credentials.md](./auth-credentials.md).

## omp

1. **Четырёхисточниковый реестр моделей.** На старте каталог собирается из: (1) встроенного каталога, (2) пользовательского `~/.omp/agent/models.yml`, (3) runtime-discovery для локальных движков и discovery-enabled гейтвеев, (4) провайдеров, зарегистрированных расширениями. Модель «доступна», если её провайдер не в `disabledProviders` И провайдер keyless или имеет резолвимые креды (omp://providers.md).
2. **Провайдер описывается двумя половинами** (omp://adding-a-provider.md): каталог-половина — запись в `CATALOG_PROVIDERS` (`packages/catalog/src/provider-models/descriptors.ts`: `id`, `defaultModel`, `envVars`, `createModelManagerOptions`), auth-половина — декларативный `ProviderDefinition` в registry (`packages/ai/src/registry/<id>.ts`). Для гейтвея, повторяющего существующий wire-протокол, всё изменение — «одна запись каталога + один def-файл + одна строка в `ALL`», т.к. диспетчеризация стрима идёт по `model.api`, а не по `model.provider`.
3. **Endpoint-семейства как первый слой ограничений** (omp://provider-endpoint-constraints.md): `openai-completions`, `openai-responses`, `openai-codex-responses`, `anthropic-messages`, `google-generative-ai` — не взаимозаменяемы; поверх — gateway/auth overlay (Azure, OpenRouter, Copilot), модельные `compat`-флаги и контекст запроса. Правило: «ограничение кодируется один раз, на самом узком слое, который им владеет».
4. **Квирки моделей кодируются compat-метаданными, а не provider-name ветками** (omp://provider-compat-reference.md): двухфазное резолвление — build-time `buildOpenAICompat(spec)` / `buildOpenAIResponsesCompat(spec)`, request-time `resolveOpenAICompatPolicy(model, options)`; pre-built `compat.whenThinking` свапается указателем без аллокаций.
5. **Retry/fallback на границах**: retry провайдера — только до первого user-visible контента; strict-tool 400 → отключить strict для scope `${provider}:${baseUrl}:${modelId}` и запомнить; stale `previous_response_id` → сброс цепочки и полный replay; Codex websocket → SSE fallback (omp://provider-endpoint-constraints.md, omp://provider-quirks.md).

## Hermes

1. **Провайдеры — плоский реестр `provider:` + `model.default` в `config.yaml`** (~/.hermes/config.yaml, ~/.hermes/.env для ключей). Список из 40+ провайдеров с первым классом для Nous Portal (OAuth, 300+ моделей), Codex, Copilot, Anthropic, OpenRouter, Bedrock (boto3 chain), Vertex (OAuth2 из service-account JSON / ADC, автоперевыпуск токена при 401) (https://hermes-agent.nousresearch.com/docs/integrations/providers).
2. **`fallback_providers:` — цепочка резервных провайдеров**, опробуемых по порядку при rate limit / server error / auth failure; канонический формат — top-level список, легаси `fallback_model:` (одна пара) поддерживается; при активации модель и провайдер меняются mid-session без потери диалога, активация one-shot на сессию (https://hermes-agent.nousresearch.com/docs/integrations/providers#fallback-providers).
3. **Per-provider таймауты и транспорт**: `providers.<id>.request_timeout_seconds`, `providers.<id>.models.<model>.timeout_seconds`, `providers.<id>.stale_timeout_seconds` (нестриминговый детектор зависших вызовов); транспорт выбирается полем `transport` на кастомном провайдере (`chat_completions`, `anthropic_messages`, `openai-wire`), с auto-detection по URL как fallback (https://hermes-agent.nousresearch.com/docs/user-guide/configuration#provider-timeouts, https://hermes-agent.nousresearch.com/docs/integrations/providers).
4. **Локальные модели — через Custom Endpoint** (base URL `http://localhost:11434/v1` для Ollama, без ключа) или `lmstudio`-провайдер; LM Studio с 0.3.6 умеет tool calling с авто-детекцией нативных моделей, для vLLM сохраняются `reasoning`/`reasoning_content` и стриминговые reasoning-дельты (https://hermes-agent.nousresearch.com/docs/integrations/providers).

## Vellum

Харнес тему провайдеров/стриминга напрямую не покрывает (материал о памяти, SOUL, безопасности, каналах). Ближайшие аналоги:

1. **OAuth без самописного token refresh**: Slack, Notion, Google, HubSpot, Linear, Discord, Twitter, Telegram, Twilio — refresh управляется платформенно (local://vellum-summary.md, раздел «OAuth»). Для titi это переносится на модельные креды: refresh-логика живёт в одном месте, а не размазана по ботам.
2. **Учётные данные в отдельном процессе, никогда не попадают в модель**; каждый вызов инструмента в песочнице, default-deny (local://vellum-summary.md, раздел «Безопасность»). Это прямой аналог omp auth-broker/gateway: модель и агентский код не видят сырой ключ.

## Решение (одно/комбо)

Комбо: **каркас omp как основа + отдельный auth-процесс по образцу broker/Vellum**. Берём ядро omp — endpoint-семейства как слои над общим wire-транспортом, декларативный реестр провайдеров (catalog-entry + auth-def), единый контракт стрим-событий и compat-метаданные вместо ветвления по именам провайдеров: это самая доказанная архитектура для десятков провайдеров и сотен квирков, а диспетчеризация по `api`, а не по `provider`, даёт дешёвое подключение гейтвеев. Retry/fallback: статическая цепочка `fallback_providers` из Hermes (просто и достаточно) плюс Scoped-правила omp (retry только до видимого контента, strict-tool/stale-chain сбросы на уровне endpoint-семейства). Креды — ladder omp, но в отдельном подпроцессе/акторе (Vellum + omp auth-gateway): модельный слой получает уже готовый токен и никогда не видит refresh-токен.

## Rust-маппинг

Крейты: `titi-providers` (новый, ядро темы), `titi-core` (контекст/сообщения), `titi-tools` (tool schema), `titi-cli`, `titi-tui`.

```rust
// titi-core: единый контракт событий (аналог AssistantMessageEvent)
pub enum StreamEvent {
    Start,
    TextStart { id: BlockId }, TextDelta { id: BlockId, text: String }, TextEnd { id: BlockId },
    ThinkingStart { id: BlockId }, ThinkingDelta { id: BlockId, text: String }, ThinkingEnd { id: BlockId },
    ToolcallStart { id: BlockId, call: ToolCallRef }, ToolcallDelta { id: BlockId, json: String }, ToolcallEnd { id: BlockId },
    Done { reason: StopReason },
    Error { reason: ErrorReason, message: String },
}
pub enum StopReason { Stop, Length, ToolUse }

// titi-providers: транспорт-абстракция
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    fn api(&self) -> &'static ApiKind; // Completions | Responses | Codex | AnthropicMessages | Gemini
    async fn stream(&self, req: WireRequest, ctx: RequestCtx)
        -> Result<EventStream<StreamEvent>, TransportError>;
}

// titi-providers: endpoint-family policy вместо ветвления по именам
pub struct CompatPolicy { // аналог ResolvedOpenAICompat
    pub thinking_format: ThinkingFormat,        // zai | qwen | openrouter | openai
    pub max_tokens_field: MaxTokensField,       // max_tokens | max_completion_tokens | omit
    pub strict_mode: StrictMode,                // all | mixed | none
    pub when_thinking: Option<Box<CompatPolicy>>,
}
pub fn resolve_compat(model: &Model, opts: &RequestOpts) -> CompatPolicy;

// titi-providers: реестр провайдеров (catalog-entry + auth-def)
pub struct ProviderDescriptor {
    pub id: SmolStr, pub default_model: SmolStr,
    pub env_keys: Vec<SmolStr>, pub api: ApiKind,
    pub discovery: DiscoveryKind, // Static | OpenAiCompat | LocalEngine | Special
}
pub trait CredentialSource { async fn resolve(&self, provider: &str) -> Option<Credential>; }

// fallback-цепочка (Hermes)
pub struct FallbackChain { entries: Vec<ModelRef>, attempted: usize }
impl FallbackChain { pub fn next(&mut self) -> Option<ModelRef>; }
```

Внешние крейты: `reqwest` (streaming body) или `hyper`; `eventsource-stream` + `futures` (SSE-декодирование); `tokio-tungstenite` (Codex-подобный WS-транспорт, опционально); `tokio` (runtime, `AbortHandle` для watchdogs); `serde`/`serde_json` (+ частичный repairing-парсер стриминговых tool-аргументов, аналог `parseStreamingJsonThrottled` — самописный, ~100 строк); `async-trait`; `smol_str`.

## Definition of Done

- [ ] `titi-providers::stream()` возвращает одинаковую последовательность `StreamEvent` для фикстур Anthropic/OpenAI/Gemini SSE (golden-тесты `tests/fixtures/*.sse` → нормализованные события).
- [ ] Диспетчеризация запроса идёт по `ApiKind`, а не по id провайдера: новый OpenAI-совместимый провайдер подключается одной записью `ProviderDescriptor` без изменений в коде транспорта (тест-пример: добавление фейкового гейтвея).
- [ ] Retry любого запроса не выполняется после того, как в `EventStream` был эмитирован `TextDelta`/`ToolcallDelta` (тест: эмуляция ошибки после первой дельты → ошибка пробрасывается, повторного вызова нет).
- [ ] Стриминговые tool-аргументы корректно восстанавливаются из обрезанного JSON (тест на repairing-парсер: валидный суффикс восстанавливается, мусор возвращает `{}` без паники).
- [ ] Thinking-уровень (`Effort`: minimal..max) клампится к лестнице модели и кодируется по `ThinkingFormat` провайдера (тест-таблица модель→wire).
- [ ] Credential ladder (`--api-key` > config > OAuth > env) покрыт unit-тестами на приоритет; refresh-токен недоступен из модельного слоя (компиляторски: `Credential` не содержит refresh-поля).
- [ ] Локальный Ollama-эндпоинт обнаруживается keyless и открывает доступ к моделям без кредов (интеграционный smoke против mock-сервера `/api/tags`).
- [ ] Fallback-цепочка переключает провайдера между ходами с сохранением истории диалога (тест сессии: primary 429 → fallback отвечает, история не потеряна).

## Deep-dive

План подсистем-доков (написаны в рамках этой задачи):

- [transports.md](./transports.md) — транспорт-различия Anthropic/OpenAI/Gemini, endpoint-семейства, gateway overlay, retry/fallback, добавление нового провайдера, локальные движки.
- [streaming-events.md](./streaming-events.md) — единый контракт стрим-событий, нормализация per-provider, квирки моделей в стриме, thinking-уровни.
- [auth-credentials.md](./auth-credentials.md) — ladder кредов, env/.env, OAuth-потоки, auth-broker/gateway, локальные модели keyless.
- [oauth-login.md](./oauth-login.md) — вход в провайдера по подписке: authorize-URL + PKCE, локальный callback, обмен кода, refresh, идентичность и инференс на OAuth-токене (референс omp, точные endpoint'ы и параметры).

Дальнейший 2-й уровень (оставлено как план): `docs/research/providers-streaming/usage-costs.md` — семантика usage/cache-токенов и тарифные мультипликаторы; `docs/research/providers-streaming/thinking-dialects.md` — полная таблица dialect-режимов thinking/reasoning по провайдерам.
