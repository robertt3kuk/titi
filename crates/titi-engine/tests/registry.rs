#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use titi_engine::{
    CredentialSource, EngineCommand, EngineConfig, EngineEvent, EngineRuntime,
    LayeredCredentialSource, ModelDescriptor, ProviderDescriptor, ProviderRegistry,
    ProviderRegistryConfig, RefreshOutcome, RefreshState, RegistryError, ResolvedModel,
    TransportFactory, TransportResolver,
};
use titi_providers::{
    ApiKind, CredKind, Credential, EventStream, LadderLevel, MockBody, MockFetch,
    MockFetchResponse, MockTransport, RequestCtx, StopReason, StreamEvent, Transport,
    TransportError, WireRequest,
};
use titi_secrets::store::{AuthStore, OAuthRecord};

struct StaticCredentials(HashMap<String, String>);

impl CredentialSource for StaticCredentials {
    fn resolve(&self, provider: &ProviderDescriptor) -> Option<Credential> {
        self.0.get(provider.id.as_str()).map(|access| Credential {
            access: access.clone().into(),
            kind: CredKind::ApiKey,
            account_id: None,
            level: LadderLevel::Config,
        })
    }
}

struct MapFactory(HashMap<String, Arc<dyn Transport>>);

impl TransportFactory for MapFactory {
    fn build(&self, provider: &ProviderDescriptor) -> Result<Arc<dyn Transport>, TransportError> {
        self.0
            .get(provider.id.as_str())
            .cloned()
            .ok_or_else(|| TransportError::Fatal {
                context_too_long: false,
                status: None,
                message: "missing test transport".into(),
            })
    }
}

fn descriptor(id: &str, required: bool) -> ProviderDescriptor {
    ProviderDescriptor {
        id: id.into(),
        api: ApiKind::OpenAiCompletions,
        base_url: "https://example.invalid/v1".into(),
        credential_env: Some(format!("{}_KEY", id.to_uppercase()).into()),
        credential_required: required,
        discover_with_credential: false,
    }
}

/// The ChatGPT subscription backend as the CLI registers it: a stored OAuth
/// credential instead of an env var, and a listing that has to be asked for.
fn codex_descriptor(discover_with_credential: bool) -> ProviderDescriptor {
    ProviderDescriptor {
        id: "openai-codex".into(),
        api: ApiKind::OpenAiResponses,
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        credential_env: None,
        credential_required: true,
        discover_with_credential,
    }
}

fn model(id: &str, provider: &str, wire_model: &str) -> ModelDescriptor {
    ModelDescriptor {
        id: id.into(),
        provider: provider.into(),
        wire_model: wire_model.into(),
        context_window: None,
        price: None,
    }
}

fn registry(
    providers: Vec<ProviderDescriptor>,
    models: Vec<ModelDescriptor>,
    keys: Vec<(&str, &str)>,
    transports: Vec<(&str, Arc<dyn Transport>)>,
) -> ProviderRegistry {
    ProviderRegistry::new(
        ProviderRegistryConfig { providers, models },
        Arc::new(StaticCredentials(
            keys.into_iter()
                .map(|(provider, key)| (provider.to_owned(), key.to_owned()))
                .collect(),
        )),
        Arc::new(MapFactory(
            transports
                .into_iter()
                .map(|(provider, transport)| (provider.to_owned(), transport))
                .collect(),
        )),
    )
    .unwrap()
}

#[test]
fn registry_resolves_transport_wire_model_and_credential() {
    let transport: Arc<dyn Transport> = Arc::new(MockTransport::default());
    let registry = registry(
        vec![descriptor("primary", true)],
        vec![model("primary/chat", "primary", "wire-chat")],
        vec![("primary", "secret")],
        vec![("primary", transport)],
    );

    let resolved = registry.resolve("primary/chat").unwrap();
    assert_eq!(resolved.id, "primary/chat");
    assert_eq!(resolved.wire_model, "wire-chat");
    assert_eq!(resolved.credential.unwrap().access, "secret");
}

#[test]
fn settings_value_parses_provider_catalog() {
    let value = serde_json::json!({
        "providers": [{
            "id": "primary",
            "api": "openai-completions",
            "base_url": "https://example.invalid/v1",
            "credential_required": false
        }],
        "models": [{
            "id": "primary/chat",
            "provider": "primary",
            "wire_model": "wire-chat"
        }]
    });
    let parsed = titi_engine::ProviderRegistryConfig::from_settings_value(&value).unwrap();
    assert_eq!(parsed.models[0].wire_model, "wire-chat");
}

#[test]
fn registry_rejects_missing_credential_and_unknown_model() {
    let transport: Arc<dyn Transport> = Arc::new(MockTransport::default());
    let registry = registry(
        vec![descriptor("primary", true)],
        vec![model("primary/chat", "primary", "wire-chat")],
        vec![],
        vec![("primary", transport)],
    );

    assert!(matches!(
        registry.resolve("primary/chat"),
        Err(RegistryError::MissingCredential { .. })
    ));
    assert!(matches!(
        registry.resolve("missing"),
        Err(RegistryError::UnknownModel(_))
    ));
}

#[derive(Default)]
struct RecordingTransport {
    seen: Mutex<Option<(String, Option<String>)>>,
}

#[async_trait]
impl Transport for RecordingTransport {
    fn api(&self) -> ApiKind {
        ApiKind::OpenAiCompletions
    }

    async fn stream(
        &self,
        request: WireRequest,
        context: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        *self.seen.lock().unwrap() = Some((
            request.model.to_string(),
            context.credential.map(|cred| cred.access.to_string()),
        ));
        Ok(Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }])))
    }
}

#[tokio::test]
async fn engine_uses_registry_wire_model_and_credential() {
    let transport = Arc::new(RecordingTransport::default());
    let registry = registry(
        vec![descriptor("primary", true)],
        vec![model("primary/chat", "primary", "wire-chat")],
        vec![("primary", "secret")],
        vec![("primary", transport.clone())],
    );
    let mut engine = EngineRuntime::start(EngineConfig::new("primary/chat"), Arc::new(registry));
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "hello".into(),
        })
        .await
        .unwrap();
    while let Some(event) = engine.recv().await {
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }

    assert_eq!(
        *transport.seen.lock().unwrap(),
        Some(("wire-chat".to_owned(), Some("secret".to_owned())))
    );
}

#[tokio::test]
async fn engine_falls_back_between_registry_providers() {
    let primary = Arc::new(MockTransport::new(vec![MockBody::Err(
        TransportError::Retryable {
            retry_after: None,
            status: Some(429),
            message: "limited".into(),
        },
    )]));
    let backup = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let registry = registry(
        vec![descriptor("primary", false), descriptor("backup", false)],
        vec![
            model("primary/chat", "primary", "wire-primary"),
            model("backup/chat", "backup", "wire-backup"),
        ],
        vec![],
        vec![("primary", primary.clone()), ("backup", backup.clone())],
    );
    let mut config = EngineConfig::new("primary/chat");
    config.fallback_models = vec!["backup/chat".into()];
    config.max_transient_retries = 0;
    let mut engine = EngineRuntime::start(config, Arc::new(registry));
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "hello".into(),
        })
        .await
        .unwrap();

    let mut switched = false;
    while let Some(event) = engine.recv().await {
        switched |= matches!(event, EngineEvent::ModelSwitched { .. });
        if matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        ) {
            break;
        }
    }
    assert!(switched);
    assert_eq!(primary.call_count(), 1);
    assert_eq!(backup.call_count(), 1);
}

/// Unix seconds, matching the unit the store and the provider skew use.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs() as i64
}

/// One stored OAuth row (the Anthropic provider's `refresh_skew_secs` is 300
/// when the provider is left at its default).
fn store_oauth_row(
    provider: &str,
    dir: &std::path::Path,
    label: &str,
    access: &str,
    expires_at: i64,
) {
    AuthStore::open(&dir.join("auth.db"))
        .expect("store opens")
        .store_oauth(OAuthRecord {
            provider,
            label,
            access,
            refresh: Some("refresh-token"),
            expires_at: Some(expires_at),
            account_id: Some("acct-1"),
            email: None,
            org_id: None,
            org_name: None,
            authorized_at: None,
        })
        .expect("row is stored");
}

fn token_response() -> MockFetchResponse {
    MockFetchResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#
                .to_owned(),
        ],
    }
}

/// A stale row is exchanged and written back; a fresh one is not touched, and
/// no request is spent on it.
#[tokio::test]
async fn the_sweep_refreshes_a_stale_oauth_row_and_leaves_a_fresh_one_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = LayeredCredentialSource::for_agent_dir(dir.path());
    // Past its expiry, and inside the 300 s skew in either case.
    store_oauth_row("anthropic", dir.path(), "work", "old-access", now() - 10);
    store_oauth_row(
        "anthropic",
        dir.path(),
        "personal",
        "fresh-access",
        now() + 10_000,
    );

    let fetch = MockFetch::new(vec![Ok(token_response())]);
    let outcomes = source.refresh_due(&fetch).await;

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].account, "anthropic/work");
    assert_eq!(outcomes[0].state, RefreshState::Refreshed);
    // Only the stale row is exchanged. A provider may spend a second request
    // on the account's identity after the token one, so the count is a floor.
    assert!(fetch.request_count() >= 1);

    let store = AuthStore::open(&dir.path().join("auth.db")).expect("store opens");
    let refreshed = store
        .get_account("anthropic", "work")
        .expect("read")
        .expect("still stored");
    assert_eq!(refreshed.token, "new-access");
    assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
    let untouched = store
        .get_account("anthropic", "personal")
        .expect("read")
        .expect("still stored");
    assert_eq!(untouched.token, "fresh-access");
    assert_eq!(untouched.refresh_token.as_deref(), Some("refresh-token"));
}

/// A row whose token expires within the provider's skew is already due: by
/// the time a turn reaches the endpoint it would be expired.
#[tokio::test]
async fn the_sweep_honours_the_provider_skew() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = LayeredCredentialSource::for_agent_dir(dir.path());
    // 200 s of life left, 300 s of skew: due.
    store_oauth_row("anthropic", dir.path(), "work", "old-access", now() + 200);

    let fetch = MockFetch::new(vec![Ok(token_response())]);
    let outcomes = source.refresh_due(&fetch).await;

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].state, RefreshState::Refreshed);
}

/// A definitive rejection quarantines the row: the refresh token is dead and
/// keeping it would fail every turn from here on.
#[tokio::test]
async fn a_rejected_refresh_token_removes_the_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = LayeredCredentialSource::for_agent_dir(dir.path());
    store_oauth_row("anthropic", dir.path(), "work", "old-access", now() - 10);

    let fetch = MockFetch::new(vec![Ok(MockFetchResponse {
        status: 400,
        headers: Vec::new(),
        chunks: vec![r#"{"error":"invalid_grant","error_description":"expired"}"#.to_owned()],
    })]);
    let outcomes = source.refresh_due(&fetch).await;

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].state, RefreshState::Quarantined);
    let store = AuthStore::open(&dir.path().join("auth.db")).expect("store opens");
    assert!(
        store
            .get_account("anthropic", "work")
            .expect("read")
            .is_none(),
        "a dead refresh token must not stay stored"
    );
}

/// A transport failure keeps the row: the token may still be good, and a
/// flaky network must not log the user out.
#[tokio::test]
async fn a_transport_failure_leaves_the_row_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = LayeredCredentialSource::for_agent_dir(dir.path());
    store_oauth_row("anthropic", dir.path(), "work", "old-access", now() - 10);

    let fetch = MockFetch::new(vec![Err(TransportError::Retryable {
        retry_after: None,
        status: None,
        message: "connection reset".into(),
    })]);
    let outcomes = source.refresh_due(&fetch).await;

    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].state, RefreshState::Transient);
    let store = AuthStore::open(&dir.path().join("auth.db")).expect("store opens");
    let kept = store
        .get_account("anthropic", "work")
        .expect("read")
        .expect("the row survives a transport failure");
    assert_eq!(kept.token, "old-access");
    assert_eq!(kept.refresh_token.as_deref(), Some("refresh-token"));
}

/// A row that never expires, and one with no refresh token to exchange, are
/// not due: neither has anything the sweep could renew.
#[tokio::test]
async fn the_sweep_skips_rows_it_cannot_renew() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = LayeredCredentialSource::for_agent_dir(dir.path());
    let store = AuthStore::open(&dir.path().join("auth.db")).expect("store opens");
    store
        .store_oauth(OAuthRecord {
            provider: "anthropic",
            label: "never-expires",
            access: "access",
            refresh: Some("refresh-token"),
            expires_at: None,
            account_id: None,
            email: None,
            org_id: None,
            org_name: None,
            authorized_at: None,
        })
        .expect("row is stored");
    store
        .store_oauth(OAuthRecord {
            provider: "anthropic",
            label: "no-refresh",
            access: "access",
            refresh: None,
            expires_at: Some(now() - 10),
            account_id: None,
            email: None,
            org_id: None,
            org_name: None,
            authorized_at: None,
        })
        .expect("row is stored");

    let fetch = MockFetch::new(Vec::new());
    let outcomes = source.refresh_due(&fetch).await;

    assert!(outcomes.is_empty(), "{outcomes:?}");
    assert_eq!(fetch.request_count(), 0);
}

/// The turn asks for the sweep before it resolves anything: refreshing after
/// the resolve would send the turn on the very token that had expired.
struct SweepingResolver {
    inner: Arc<ProviderRegistry>,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl TransportResolver for SweepingResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self.log.lock().expect("log").push("resolve");
        self.inner.resolve(model)
    }

    fn refresh_due(&self) -> Pin<Box<dyn Future<Output = Vec<RefreshOutcome>> + Send + '_>> {
        self.log.lock().expect("log").push("refresh");
        Box::pin(async { Vec::new() })
    }
}

#[tokio::test]
async fn a_turn_refreshes_before_it_resolves() {
    let transport: Arc<dyn Transport> = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(registry(
        vec![descriptor("primary", true)],
        vec![model("primary/chat", "primary", "wire-chat")],
        vec![("primary", "secret")],
        vec![("primary", transport)],
    ));
    let resolver = Arc::new(SweepingResolver {
        inner: registry,
        log: Arc::clone(&log),
    });
    let mut engine = EngineRuntime::start(EngineConfig::new("primary/chat"), resolver);
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "hello".into(),
        })
        .await
        .expect("prompt accepted");
    while let Some(event) = engine.recv().await {
        if matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        ) {
            break;
        }
    }

    let calls = log.lock().expect("log").clone();
    assert_eq!(calls.first(), Some(&"refresh"), "{calls:?}");
    assert!(calls.contains(&"resolve"), "{calls:?}");
}

/// The subscription backend as the registry sees it: the codex descriptor,
/// the models the CLI's catalog already declares, and one transport.
fn codex_registry(agent_dir: &std::path::Path, discover_with_credential: bool) -> ProviderRegistry {
    let transport: Arc<dyn Transport> = Arc::new(MockTransport::default());
    ProviderRegistry::new(
        ProviderRegistryConfig {
            providers: vec![codex_descriptor(discover_with_credential)],
            models: vec![model("openai-codex/gpt-5.5", "openai-codex", "gpt-5.5")],
        },
        Arc::new(LayeredCredentialSource::for_agent_dir(agent_dir)),
        Arc::new(MapFactory(HashMap::from([(
            "openai-codex".to_owned(),
            transport,
        )]))),
    )
    .expect("registry builds")
}

/// A credential-bearing provider is listed with the token it stores, on the
/// endpoint's own versioned URL — and what the endpoint reports joins the
/// catalog.
#[tokio::test]
async fn a_flagged_provider_is_listed_with_its_stored_credential() {
    let dir = tempfile::tempdir().expect("temp dir");
    store_oauth_row(
        "openai-codex",
        dir.path(),
        "default",
        "sk-test-access-1234",
        now() + 10_000,
    );
    let registry = codex_registry(dir.path(), true);

    let fetch = MockFetch::new(vec![Ok(MockFetchResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![r#"{"data":[{"id":"gpt-6-sol"},{"id":"gpt-5.5"}]}"#.to_owned()],
    })]);
    registry.discover_providers(&fetch).await;

    {
        let requests = fetch.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1, "one listing");
        // The pinned Codex client version: the backend hides newer models
        // from a client that does not declare it.
        assert_eq!(
            requests[0].url.as_str(),
            "https://chatgpt.com/backend-api/codex/models?client_version=0.155.1"
        );
        let auth = requests[0]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .expect("the stored credential authenticates the listing");
        assert_eq!(auth.1, "Bearer sk-test-access-1234");
    }
    assert!(
        registry
            .model_ids()
            .iter()
            .any(|id| id == "openai-codex/gpt-6-sol"),
        "the live listing joins the catalog: {:?}",
        registry.model_ids()
    );
}

/// The credential is the entry ticket: flagged but with nothing stored, the
/// request is never made.
#[tokio::test]
async fn a_flagged_provider_without_a_credential_is_not_asked() {
    let dir = tempfile::tempdir().expect("temp dir");
    let registry = codex_registry(dir.path(), true);

    let fetch = MockFetch::new(Vec::new());
    registry.discover_providers(&fetch).await;

    assert_eq!(fetch.request_count(), 0);
}

/// The flag is what buys the request: a provider that never opted in is left
/// alone even when a credential is stored for it.
#[tokio::test]
async fn an_unflagged_provider_is_never_listed_with_a_credential() {
    let dir = tempfile::tempdir().expect("temp dir");
    store_oauth_row(
        "openai-codex",
        dir.path(),
        "default",
        "sk-test-access-1234",
        now() + 10_000,
    );
    let registry = codex_registry(dir.path(), false);

    let fetch = MockFetch::new(Vec::new());
    registry.discover_providers(&fetch).await;

    assert_eq!(fetch.request_count(), 0);
}

/// The credential-free case is untouched: a local server is listed with no
/// authorization header, exactly as it was before the flag existed.
#[tokio::test]
async fn a_keyless_provider_is_still_listed_without_a_credential() {
    let transport: Arc<dyn Transport> = Arc::new(MockTransport::default());
    let keyless = ProviderDescriptor {
        id: "ollama".into(),
        api: ApiKind::OpenAiCompletions,
        base_url: "http://127.0.0.1:11434/v1".into(),
        credential_env: None,
        credential_required: false,
        discover_with_credential: false,
    };
    let registry = ProviderRegistry::new(
        ProviderRegistryConfig {
            providers: vec![keyless],
            models: vec![model("ollama/qwen3", "ollama", "qwen3")],
        },
        Arc::new(StaticCredentials(HashMap::new())),
        Arc::new(MapFactory(HashMap::from([(
            "ollama".to_owned(),
            transport,
        )]))),
    )
    .expect("registry builds");

    let fetch = MockFetch::new(vec![Ok(MockFetchResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![r#"{"data":[{"id":"qwen3:8b"}]}"#.to_owned()],
    })]);
    registry.discover_providers(&fetch).await;

    {
        let requests = fetch.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0]
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            "a keyless server is not handed a credential"
        );
    }
    assert!(
        registry
            .model_ids()
            .iter()
            .any(|id| id == "ollama/qwen3:8b"),
        "{:?}",
        registry.model_ids()
    );
}

/// The flag is off unless a descriptor asks for it, so a catalog written
/// before it existed never spends a credential on a listing.
#[test]
fn the_authenticated_discovery_flag_is_off_by_default() {
    let value = serde_json::json!({
        "providers": [{
            "id": "openai-codex",
            "api": "openai-responses",
            "base_url": "https://chatgpt.com/backend-api/codex",
            "credential_required": true
        }],
        "models": [{
            "id": "openai-codex/gpt-5.5",
            "provider": "openai-codex",
            "wire_model": "gpt-5.5"
        }]
    });
    let parsed = ProviderRegistryConfig::from_settings_value(&value).expect("parses");
    assert!(!parsed.providers[0].discover_with_credential);
}
