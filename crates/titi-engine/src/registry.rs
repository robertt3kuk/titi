use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_providers::{
    ApiKind, CredKind, Credential, FamilyTransport, HttpFetch, HttpRequest, LadderLevel, Transport,
    TransportError,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: SmolStr,
    pub api: ApiKind,
    pub base_url: SmolStr,
    pub credential_env: Option<SmolStr>,
    #[serde(default = "default_true")]
    pub credential_required: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub id: SmolStr,
    pub provider: SmolStr,
    pub wire_model: SmolStr,
    /// Context window the model reports, used to decide when to compact.
    /// `None` leaves the engine's default in place.
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRegistryConfig {
    pub providers: Vec<ProviderDescriptor>,
    pub models: Vec<ModelDescriptor>,
}

impl ProviderRegistryConfig {
    /// Context window of the first configured model, if it declares one.
    pub fn primary_context_window(&self) -> Option<u64> {
        self.models.first().and_then(|model| model.context_window)
    }

    pub fn from_settings_value(value: &serde_json::Value) -> Option<Self> {
        let parsed: Self = serde_json::from_value(value.clone()).ok()?;
        if parsed.providers.is_empty() || parsed.models.is_empty() {
            return None;
        }
        Some(parsed)
    }
}

/// Longest one local server may take to list its models.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Most models one local server may contribute.
///
/// The answer is untrusted input: a server with a thousand pulled tags would
/// otherwise bury the paid models in `/model` and in the prompt.
pub const MAX_DISCOVERED_MODELS: usize = 64;

/// Most bytes read from a model listing before giving up on it.
const MAX_DISCOVERY_BODY: usize = 256 * 1024;

/// Longest model id accepted from a server.
const MAX_MODEL_ID: usize = 96;

/// A model id we are willing to print, send on the wire and match on.
///
/// Anything else is a server misbehaving or a response that is not a model
/// list at all; either way it has no business reaching the prompt.
fn is_usable_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID
        && id.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '/' | '+' | '@')
        })
}

/// Model ids an OpenAI-compatible server lists at `GET <base_url>/models`.
///
/// Every failure — refused connection, error status, body that is not the
/// OpenAI model list — means "this server has nothing to offer right now",
/// never a hard error: an absent local server must cost a missing entry in
/// `/model`, not a failed start.
pub async fn discover_models(
    provider: &ProviderDescriptor,
    fetch: &dyn HttpFetch,
) -> Vec<ModelDescriptor> {
    let request = HttpRequest {
        method: "GET".into(),
        url: format!("{}/models", provider.base_url.trim_end_matches('/')).into(),
        headers: vec![("accept".into(), "application/json".into())],
        body: None,
    };
    let Ok(response) = fetch.fetch(request).await else {
        return Vec::new();
    };
    if response.status >= 400 {
        return Vec::new();
    }
    let mut body = Vec::new();
    let mut chunks = response.body;
    while let Some(chunk) = futures::StreamExt::next(&mut chunks).await {
        let Ok(bytes) = chunk else {
            return Vec::new();
        };
        if body.len() + bytes.len() > MAX_DISCOVERY_BODY {
            return Vec::new();
        }
        body.extend_from_slice(&bytes);
    }
    let Ok(listing) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Vec::new();
    };
    let Some(entries) = listing.get("data").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|wire| is_usable_model_id(wire))
        .take(MAX_DISCOVERED_MODELS)
        .map(|wire| ModelDescriptor {
            id: format!("{}/{wire}", provider.id).into(),
            provider: provider.id.clone(),
            wire_model: wire.into(),
            context_window: None,
        })
        .collect()
}

pub struct ResolvedModel {
    pub id: SmolStr,
    pub wire_model: SmolStr,
    pub transport: Arc<dyn Transport>,
    pub credential: Option<Credential>,
}

impl ResolvedModel {
    pub fn without_credential(id: impl Into<SmolStr>, transport: Arc<dyn Transport>) -> Self {
        let id = id.into();
        Self {
            id: id.clone(),
            wire_model: id,
            transport,
            credential: None,
        }
    }
}

pub trait CredentialSource: Send + Sync + 'static {
    fn resolve(&self, provider: &ProviderDescriptor) -> Option<Credential>;
}

#[derive(Debug, Default)]
pub struct EnvCredentialSource;

impl CredentialSource for EnvCredentialSource {
    fn resolve(&self, provider: &ProviderDescriptor) -> Option<Credential> {
        let key = provider.credential_env.as_deref()?;
        let access = std::env::var(key).ok()?;
        if access.trim().is_empty() {
            return None;
        }
        Some(Credential {
            access: access.into(),
            kind: CredKind::ApiKey,
            level: LadderLevel::Env,
        })
    }
}

/// Env, layered `.env`, then `auth.db` for the provider id.
pub struct LayeredCredentialSource {
    env: titi_secrets::env::LayeredEnv,
    store_path: std::path::PathBuf,
}

impl LayeredCredentialSource {
    pub fn from_defaults() -> Self {
        let agent_dir = titi_config::agent_dir();
        Self {
            env: titi_secrets::env::LayeredEnv::from_defaults(),
            store_path: agent_dir.join("auth.db"),
        }
    }

    /// Bound to an explicit agent directory, for tests and alternate profiles.
    pub fn for_agent_dir(agent_dir: impl Into<std::path::PathBuf>) -> Self {
        let agent_dir = agent_dir.into();
        Self {
            env: titi_secrets::env::LayeredEnv::new(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                agent_dir.clone(),
            ),
            store_path: agent_dir.join("auth.db"),
        }
    }
}

impl CredentialSource for LayeredCredentialSource {
    fn resolve(&self, provider: &ProviderDescriptor) -> Option<Credential> {
        if let Some(key) = provider.credential_env.as_deref()
            && let Some(access) = self.env.resolve(key)
            && !access.trim().is_empty()
        {
            return Some(Credential {
                access: access.into(),
                kind: CredKind::ApiKey,
                level: LadderLevel::Env,
            });
        }
        let store = titi_secrets::store::AuthStore::open(&self.store_path).ok()?;
        let stored = store.get(provider.id.as_str()).ok()??;
        if stored.token.trim().is_empty() {
            return None;
        }
        Some(Credential {
            access: stored.token.into(),
            kind: if stored.kind == "oauth" {
                CredKind::BearerToken
            } else {
                CredKind::ApiKey
            },
            level: LadderLevel::Stored,
        })
    }
}

pub trait TransportFactory: Send + Sync + 'static {
    fn build(&self, provider: &ProviderDescriptor) -> Result<Arc<dyn Transport>, TransportError>;
}

#[derive(Debug, Default)]
pub struct HttpTransportFactory;

impl TransportFactory for HttpTransportFactory {
    fn build(&self, provider: &ProviderDescriptor) -> Result<Arc<dyn Transport>, TransportError> {
        FamilyTransport::with_default_fetch(provider.api, provider.base_url.clone())
            .map(|transport| Arc::new(transport) as Arc<dyn Transport>)
    }
}

struct ProviderEntry {
    descriptor: ProviderDescriptor,
    transport: Arc<dyn Transport>,
}

pub struct ProviderRegistry {
    providers: HashMap<SmolStr, ProviderEntry>,
    /// Late-filling: a local server's models are only known once it answers,
    /// and the start path must not wait for that.
    models: std::sync::RwLock<HashMap<SmolStr, ModelDescriptor>>,
    credentials: Arc<dyn CredentialSource>,
}

impl ProviderRegistry {
    pub fn new(
        config: ProviderRegistryConfig,
        credentials: Arc<dyn CredentialSource>,
        transports: Arc<dyn TransportFactory>,
    ) -> Result<Self, RegistryError> {
        let mut providers = HashMap::new();
        for descriptor in config.providers {
            if providers.contains_key(&descriptor.id) {
                return Err(RegistryError::DuplicateProvider(descriptor.id));
            }
            let transport =
                transports
                    .build(&descriptor)
                    .map_err(|source| RegistryError::Transport {
                        provider: descriptor.id.clone(),
                        source,
                    })?;
            providers.insert(
                descriptor.id.clone(),
                ProviderEntry {
                    descriptor,
                    transport,
                },
            );
        }

        let mut models = HashMap::new();
        for model in config.models {
            if !providers.contains_key(&model.provider) {
                return Err(RegistryError::UnknownProvider {
                    model: model.id,
                    provider: model.provider,
                });
            }
            if models.insert(model.id.clone(), model.clone()).is_some() {
                return Err(RegistryError::DuplicateModel(model.id));
            }
        }

        Ok(Self {
            providers,
            models: std::sync::RwLock::new(models),
            credentials,
        })
    }

    pub fn model_ids(&self) -> Vec<SmolStr> {
        let Ok(models) = self.models.read() else {
            return Vec::new();
        };
        let mut ids: Vec<_> = models.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Asks every credential-free provider what it can serve, in the
    /// background, one task each.
    ///
    /// Nothing on the start path waits for this, and no provider waits for
    /// another: each listing joins the catalog the moment it arrives, so a
    /// server that accepts the connection and then goes quiet delays only
    /// itself. Never answering is that provider's normal state, not an error
    /// anyone has to handle. Without a runtime — `--set-key`, tests — there
    /// is nothing to spawn on, and discovery is skipped.
    ///
    /// A provider that needs a key is left alone: asking a paid gateway for
    /// its catalog is a request the user did not make.
    pub fn spawn_local_discovery(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let providers = self.keyless_providers();
        if providers.is_empty() {
            return;
        }
        let Ok(fetch) = titi_providers::ReqwestFetch::new() else {
            return;
        };
        let fetch: Arc<dyn HttpFetch> = Arc::new(fetch);
        for provider in providers {
            let registry = Arc::clone(self);
            let fetch = Arc::clone(&fetch);
            handle.spawn(async move {
                registry.refresh_models(&provider, fetch.as_ref()).await;
            });
        }
    }

    fn keyless_providers(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|entry| &entry.descriptor)
            .filter(|provider| !provider.credential_required && provider.credential_env.is_none())
            .cloned()
            .collect()
    }

    /// Folds one provider's current model list into the catalog.
    async fn refresh_models(&self, provider: &ProviderDescriptor, fetch: &dyn HttpFetch) {
        // Bounds a task rather than a person: a socket that accepts and never
        // answers must not pin a listing future for the life of the process.
        if let Ok(models) =
            tokio::time::timeout(DISCOVERY_TIMEOUT, discover_models(provider, fetch)).await
        {
            self.add_models(models);
        }
    }

    /// Adds models the catalog does not already declare. A configured entry
    /// always wins: the user's wire model and context window are a decision,
    /// the server's listing is only a report.
    fn add_models(&self, models: Vec<ModelDescriptor>) {
        if models.is_empty() {
            return;
        }
        let Ok(mut table) = self.models.write() else {
            return;
        };
        for model in models {
            if !self.providers.contains_key(&model.provider) {
                continue;
            }
            table.entry(model.id.clone()).or_insert(model);
        }
    }

    pub fn resolve(&self, model_id: &str) -> Result<ResolvedModel, RegistryError> {
        let models = self
            .models
            .read()
            .map_err(|_| RegistryError::UnknownModel(model_id.into()))?;
        let model = models
            .get(model_id)
            .ok_or_else(|| RegistryError::UnknownModel(model_id.into()))?;
        let provider =
            self.providers
                .get(&model.provider)
                .ok_or_else(|| RegistryError::UnknownProvider {
                    model: model.id.clone(),
                    provider: model.provider.clone(),
                })?;
        let credential = self.credentials.resolve(&provider.descriptor);
        if provider.descriptor.credential_required && credential.is_none() {
            return Err(RegistryError::MissingCredential {
                provider: provider.descriptor.id.clone(),
                env: provider.descriptor.credential_env.clone(),
            });
        }
        Ok(ResolvedModel {
            id: model.id.clone(),
            wire_model: model.wire_model.clone(),
            transport: Arc::clone(&provider.transport),
            credential,
        })
    }
}

impl crate::runtime::TransportResolver for ProviderRegistry {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        ProviderRegistry::resolve(self, model)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown model {0}")]
    UnknownModel(SmolStr),
    #[error("model {model} references unknown provider {provider}")]
    UnknownProvider { model: SmolStr, provider: SmolStr },
    #[error("duplicate provider {0}")]
    DuplicateProvider(SmolStr),
    #[error("duplicate model {0}")]
    DuplicateModel(SmolStr),
    #[error("provider {provider} requires a credential from {}", .env.as_deref().unwrap_or("no key env configured"))]
    MissingCredential {
        provider: SmolStr,
        env: Option<SmolStr>,
    },
    #[error("failed to build transport for {provider}: {source}")]
    Transport {
        provider: SmolStr,
        #[source]
        source: TransportError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_credential_error_formats_env_var_plainly() {
        let err = RegistryError::MissingCredential {
            provider: "openai".into(),
            env: Some("OPENAI_API_KEY".into()),
        };
        let msg = err.to_string();
        // Should render the env var name plainly, not with Debug formatting
        assert!(
            msg.contains("OPENAI_API_KEY"),
            "error message should contain plain env var name, got: {msg}"
        );
        assert!(
            !msg.contains("Some("),
            "error message should not contain Debug-formatted Option, got: {msg}"
        );
    }

    #[test]
    fn missing_credential_error_handles_missing_env_var() {
        let err = RegistryError::MissingCredential {
            provider: "anthropic".into(),
            env: None,
        };
        let msg = err.to_string();
        // Should say "no key env configured" when env is None
        assert!(
            msg.contains("no key env configured"),
            "error message should indicate no env var configured, got: {msg}"
        );
        assert!(
            !msg.contains("None"),
            "error message should not contain Debug-formatted None, got: {msg}"
        );
    }

    struct FakeTransport;

    #[async_trait::async_trait]
    impl Transport for FakeTransport {
        fn api(&self) -> ApiKind {
            ApiKind::OpenAiCompletions
        }

        fn watchdog(&self) -> titi_providers::WatchdogConfig {
            titi_providers::WatchdogConfig::default()
        }

        async fn stream(
            &self,
            _req: titi_providers::WireRequest,
            _ctx: titi_providers::RequestCtx,
        ) -> Result<titi_providers::EventStream, TransportError> {
            Err(TransportError::Fatal {
                status: None,
                message: "not wired".into(),
            })
        }
    }

    #[derive(Default)]
    struct RecordingFactory {
        built: std::sync::Mutex<Vec<(ProviderDescriptor, Arc<dyn Transport>)>>,
    }

    impl RecordingFactory {
        fn transport_for(&self, provider: &str) -> (ProviderDescriptor, Arc<dyn Transport>) {
            let built = self.built.lock().expect("recorded builds");
            built
                .iter()
                .find(|(descriptor, _)| descriptor.id == provider)
                .map(|(descriptor, transport)| (descriptor.clone(), Arc::clone(transport)))
                .expect("provider was built")
        }
    }

    impl TransportFactory for RecordingFactory {
        fn build(
            &self,
            provider: &ProviderDescriptor,
        ) -> Result<Arc<dyn Transport>, TransportError> {
            let transport: Arc<dyn Transport> = Arc::new(FakeTransport);
            if let Ok(mut built) = self.built.lock() {
                built.push((provider.clone(), Arc::clone(&transport)));
            }
            Ok(transport)
        }
    }

    struct NoCredentials;

    impl CredentialSource for NoCredentials {
        fn resolve(&self, _provider: &ProviderDescriptor) -> Option<Credential> {
            None
        }
    }

    fn gateway(id: &str, base_url: &str, env: Option<&str>) -> ProviderDescriptor {
        ProviderDescriptor {
            id: id.into(),
            api: ApiKind::OpenAiCompletions,
            base_url: base_url.into(),
            credential_env: env.map(SmolStr::from),
            credential_required: env.is_some(),
        }
    }

    fn model(id: &str, provider: &str, wire_model: &str) -> ModelDescriptor {
        ModelDescriptor {
            id: id.into(),
            provider: provider.into(),
            wire_model: wire_model.into(),
            context_window: None,
        }
    }

    /// Two gateways of the same family differ only by URL, so a model must
    /// reach the transport built for *its* provider, not the first one that
    /// happens to speak the same protocol.
    #[test]
    fn a_model_reaches_the_transport_built_for_its_own_endpoint() {
        let factory = Arc::new(RecordingFactory::default());
        let config = ProviderRegistryConfig {
            providers: vec![
                gateway(
                    "clinepass",
                    "https://api.cline.bot/api/v1",
                    Some("CLINE_KEY"),
                ),
                gateway("bai", "https://api.b.ai/v1", Some("BAI_KEY")),
            ],
            models: vec![
                model("clinepass/glm-5.3", "clinepass", "glm-5.3"),
                model("bai/qwen3.8-max", "bai", "qwen3.8-max"),
            ],
        };
        struct AlwaysKeyed;
        impl CredentialSource for AlwaysKeyed {
            fn resolve(&self, _provider: &ProviderDescriptor) -> Option<Credential> {
                Some(Credential {
                    access: "sk-test".into(),
                    kind: CredKind::ApiKey,
                    level: LadderLevel::Env,
                })
            }
        }
        let registry = ProviderRegistry::new(
            config,
            Arc::new(AlwaysKeyed),
            Arc::clone(&factory) as Arc<dyn TransportFactory>,
        )
        .expect("registry builds");

        let resolved = registry.resolve("bai/qwen3.8-max").expect("bai resolves");
        assert_eq!(resolved.wire_model.as_str(), "qwen3.8-max");
        let (descriptor, transport) = factory.transport_for("bai");
        assert_eq!(descriptor.base_url.as_str(), "https://api.b.ai/v1");
        assert_eq!(descriptor.api, ApiKind::OpenAiCompletions);
        assert!(Arc::ptr_eq(&resolved.transport, &transport));

        let resolved = registry
            .resolve("clinepass/glm-5.3")
            .expect("clinepass resolves");
        let (descriptor, transport) = factory.transport_for("clinepass");
        assert_eq!(descriptor.base_url.as_str(), "https://api.cline.bot/api/v1");
        assert!(Arc::ptr_eq(&resolved.transport, &transport));
    }

    #[test]
    fn a_key_requiring_provider_names_the_env_var_it_wants() {
        let config = ProviderRegistryConfig {
            providers: vec![gateway("bai", "https://api.b.ai/v1", Some("BAI_API_KEY"))],
            models: vec![model("bai/glm-5.3-flash", "bai", "glm-5.3-flash")],
        };
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::new(RecordingFactory::default()),
        )
        .expect("registry builds");

        let error = registry
            .resolve("bai/glm-5.3-flash")
            .err()
            .expect("a keyless gateway cannot run");
        assert!(
            matches!(error, RegistryError::MissingCredential { .. }),
            "expected a missing credential, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("BAI_API_KEY") && !message.contains("Some("),
            "expected the env var named plainly, got: {message}"
        );
    }

    /// A local server needs no key, so the catalog must hand out its
    /// transport rather than refuse the model for a credential that does not
    /// exist.
    #[test]
    fn a_local_provider_resolves_without_any_credential() {
        let config = ProviderRegistryConfig {
            providers: vec![gateway("ollama", "http://127.0.0.1:11434/v1", None)],
            models: vec![model("ollama/qwen3", "ollama", "qwen3")],
        };
        let factory = Arc::new(RecordingFactory::default());
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::clone(&factory) as Arc<dyn TransportFactory>,
        )
        .expect("registry builds");

        let resolved = registry.resolve("ollama/qwen3").expect("local resolves");
        assert!(resolved.credential.is_none());
        let (descriptor, _) = factory.transport_for("ollama");
        assert_eq!(descriptor.base_url.as_str(), "http://127.0.0.1:11434/v1");
    }

    /// Nothing listening on the port must cost a failed request, not a panic
    /// and not a failed start.
    #[tokio::test]
    async fn a_local_server_that_is_not_running_is_a_typed_error() {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
            probe.local_addr().expect("bound address").port()
        };
        let config = ProviderRegistryConfig {
            providers: vec![gateway(
                "lmstudio",
                &format!("http://127.0.0.1:{port}/v1"),
                None,
            )],
            models: vec![model("lmstudio/local", "lmstudio", "local")],
        };
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::new(HttpTransportFactory),
        )
        .expect("a dead endpoint still builds a catalog");

        let resolved = registry.resolve("lmstudio/local").expect("still resolves");
        let error = resolved
            .transport
            .stream(
                titi_providers::WireRequest::new("local"),
                titi_providers::RequestCtx::default(),
            )
            .await
            .err()
            .expect("a closed port cannot stream");
        assert!(
            matches!(
                error,
                TransportError::Retryable { .. } | TransportError::Fatal { .. }
            ),
            "expected a typed transport error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn discovery_reports_what_the_local_server_lists() {
        let fetch = titi_providers::MockFetch::sse(vec![
            r#"{"object":"list","data":[{"id":"qwen3:8b"},{"id":"llama3.2"}]}"#.to_owned(),
        ]);
        let provider = gateway("ollama", "http://127.0.0.1:11434/v1", None);

        let found = discover_models(&provider, &fetch).await;

        let ids: Vec<&str> = found.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["ollama/qwen3:8b", "ollama/llama3.2"]);
        assert_eq!(found[0].wire_model.as_str(), "qwen3:8b");
        let requests = fetch.requests.lock().expect("recorded requests");
        assert_eq!(requests[0].method.as_str(), "GET");
        assert_eq!(requests[0].url.as_str(), "http://127.0.0.1:11434/v1/models");
    }

    #[tokio::test]
    async fn discovery_is_silent_when_nothing_answers() {
        let fetch = titi_providers::MockFetch::new(vec![Err(TransportError::Retryable {
            status: None,
            message: "request failed: connection refused".into(),
        })]);
        let provider = gateway("lmstudio", "http://127.0.0.1:1234/v1", None);

        assert!(discover_models(&provider, &fetch).await.is_empty());
    }

    /// The listing is untrusted input: a thousand pulled tags must not bury
    /// the rest of the catalog, and an id that cannot be a model id must not
    /// reach the prompt.
    #[tokio::test]
    async fn discovery_caps_the_flood_and_drops_unusable_ids() {
        let mut entries: Vec<String> = vec![
            r#"{"id":"  "}"#.to_owned(),
            r#"{"id":"has space"}"#.to_owned(),
            format!(r#"{{"id":"{}"}}"#, "x".repeat(200)),
        ];
        entries.extend((0..MAX_DISCOVERED_MODELS + 20).map(|n| format!(r#"{{"id":"m{n}"}}"#)));
        let body = format!(r#"{{"data":[{}]}}"#, entries.join(","));
        let fetch = titi_providers::MockFetch::sse(vec![body]);
        let provider = gateway("ollama", "http://127.0.0.1:11434/v1", None);

        let found = discover_models(&provider, &fetch).await;

        assert_eq!(found.len(), MAX_DISCOVERED_MODELS);
        assert!(
            found
                .iter()
                .all(|model| model.wire_model.as_str().starts_with('m')),
            "junk ids should never reach the catalog: {found:?}"
        );
    }

    /// Asking a paid gateway for its catalog is a request the user did not
    /// make, and it would need their key to answer.
    #[tokio::test]
    async fn discovery_never_touches_a_provider_that_needs_a_key() {
        let config = ProviderRegistryConfig {
            providers: vec![
                gateway(
                    "clinepass",
                    "https://api.cline.bot/api/v1",
                    Some("CLINE_KEY"),
                ),
                gateway("bai", "https://api.b.ai/v1", Some("BAI_API_KEY")),
                gateway("ollama", "http://127.0.0.1:11434/v1", None),
            ],
            models: Vec::new(),
        };
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::new(RecordingFactory::default()),
        )
        .expect("registry builds");

        let asked: Vec<SmolStr> = registry
            .keyless_providers()
            .into_iter()
            .map(|provider| provider.id)
            .collect();
        assert_eq!(asked, vec![SmolStr::from("ollama")]);
    }

    /// One provider's answer must not wait on another's: each listing lands
    /// on its own, which is what lets a silent local server cost nothing.
    #[tokio::test]
    async fn a_providers_listing_lands_on_its_own() {
        let config = ProviderRegistryConfig {
            providers: vec![
                gateway("ollama", "http://127.0.0.1:11434/v1", None),
                gateway("lmstudio", "http://127.0.0.1:1234/v1", None),
            ],
            models: Vec::new(),
        };
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::new(RecordingFactory::default()),
        )
        .expect("registry builds");
        let ollama = gateway("ollama", "http://127.0.0.1:11434/v1", None);
        let fetch =
            titi_providers::MockFetch::sse(vec![r#"{"data":[{"id":"qwen3:8b"}]}"#.to_owned()]);

        registry.refresh_models(&ollama, &fetch).await;

        assert_eq!(registry.model_ids(), vec![SmolStr::from("ollama/qwen3:8b")]);
        assert!(registry.resolve("ollama/qwen3:8b").is_ok());
    }

    /// A late answer joins the catalog; it never overwrites what the user
    /// declared, and it never invents a provider.
    #[test]
    fn a_late_listing_adds_models_without_replacing_the_declared_ones() {
        let config = ProviderRegistryConfig {
            providers: vec![gateway("ollama", "http://127.0.0.1:11434/v1", None)],
            models: vec![model("ollama/qwen3", "ollama", "qwen3-pinned")],
        };
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::new(RecordingFactory::default()),
        )
        .expect("registry builds");

        registry.add_models(vec![
            model("ollama/qwen3", "ollama", "qwen3"),
            model("ollama/llama3.2", "ollama", "llama3.2"),
            model("ghost/model", "ghost", "model"),
        ]);

        assert_eq!(
            registry.model_ids(),
            vec![
                SmolStr::from("ollama/llama3.2"),
                SmolStr::from("ollama/qwen3")
            ]
        );
        let resolved = registry.resolve("ollama/qwen3").expect("declared model");
        assert_eq!(resolved.wire_model.as_str(), "qwen3-pinned");
    }
}
