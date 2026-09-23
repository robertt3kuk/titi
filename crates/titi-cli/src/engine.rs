use std::sync::Arc;

use titi_engine::{
    Engine, EngineConfig, EngineRuntime, HttpTransportFactory, LayeredCredentialSource,
    ModelDescriptor, ProviderDescriptor, ProviderRegistry, ProviderRegistryConfig, TrajectorySink,
};
use titi_providers::ApiKind;
use titi_tools::{ApprovalMode, SensitivePolicy, ToolRegistry, workspace_tools_with_policy};

pub fn default_registry_config() -> ProviderRegistryConfig {
    ProviderRegistryConfig {
        providers: vec![
            ProviderDescriptor {
                id: "openai".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://api.openai.com/v1".into(),
                credential_env: Some("OPENAI_API_KEY".into()),
                credential_required: true,
            },
            ProviderDescriptor {
                id: "openrouter".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://openrouter.ai/api/v1".into(),
                credential_env: Some("OPENROUTER_API_KEY".into()),
                credential_required: true,
            },
            ProviderDescriptor {
                id: "opencode-go".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://opencode.ai/zen/go/v1".into(),
                credential_env: Some("OPENCODE_API_KEY".into()),
                credential_required: true,
            },
            ProviderDescriptor {
                id: "anthropic".into(),
                api: ApiKind::AnthropicMessages,
                base_url: "https://api.anthropic.com".into(),
                credential_env: Some("ANTHROPIC_API_KEY".into()),
                credential_required: true,
            },
        ],
        models: vec![
            ModelDescriptor {
                id: "openai/gpt-4.1".into(),
                provider: "openai".into(),
                wire_model: "gpt-4.1".into(),
                context_window: Some(1_000_000),
            },
            ModelDescriptor {
                id: "openrouter/gpt-4.1".into(),
                provider: "openrouter".into(),
                wire_model: "openai/gpt-4.1".into(),
                context_window: Some(1_000_000),
            },
            ModelDescriptor {
                id: "opencode-go/glm-5.3-flash".into(),
                provider: "opencode-go".into(),
                wire_model: "glm-5.3-flash".into(),
                context_window: None,
            },
            ModelDescriptor {
                id: "opencode-go/deepseek-v4-flash".into(),
                provider: "opencode-go".into(),
                wire_model: "deepseek-v4-flash".into(),
                context_window: None,
            },
            ModelDescriptor {
                id: "anthropic/claude-sonnet-4-5".into(),
                provider: "anthropic".into(),
                wire_model: "claude-sonnet-4-5".into(),
                context_window: Some(200_000),
            },
        ],
    }
}

/// Keeps the models that `available` accepts, in their original order.
///
/// An empty acceptance returns `models` unchanged, so a machine with no keys
/// still starts and the first request can name the missing credential.
pub fn prefer_available_models(
    models: Vec<String>,
    mut available: impl FnMut(&str) -> bool,
) -> Vec<String> {
    let ready: Vec<String> = models.iter().filter(|id| available(id)).cloned().collect();
    if ready.is_empty() { models } else { ready }
}

/// Overlay `overlay` onto `base` by id.
///
/// A user file that only names one provider used to replace the catalog, so
/// adding `opencode-go` dropped OpenAI. The same id from the overlay wins;
/// every other builtin stays. New ids are appended.
pub fn merge_registry_config(
    mut base: ProviderRegistryConfig,
    overlay: ProviderRegistryConfig,
) -> ProviderRegistryConfig {
    for provider in overlay.providers {
        if let Some(existing) = base
            .providers
            .iter_mut()
            .find(|item| item.id == provider.id)
        {
            *existing = provider;
        } else {
            base.providers.push(provider);
        }
    }
    for model in overlay.models {
        if let Some(existing) = base.models.iter_mut().find(|item| item.id == model.id) {
            *existing = model;
        } else {
            base.models.push(model);
        }
    }
    base
}

pub fn load_registry_config() -> ProviderRegistryConfig {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let defaults = default_registry_config();
    if let Ok(settings) =
        titi_config::settings::Settings::load(&titi_config::agent_dir(), &cwd, &[])
        && let Some(parsed) = ProviderRegistryConfig::from_settings_value(&settings.effective())
    {
        return merge_registry_config(defaults, parsed);
    }
    defaults
}

/// Starts the engine, returning it with the model catalog and the session id
/// it resumed or created.
/// Messages replayed from a resumed session. Older turns are dropped: past a
/// point they crowd the request without informing the next one.
pub const MAX_RESTORED_MESSAGES: usize = 40;

/// Trims a replayed conversation to at most `limit` messages, cutting only
/// where no tool round is split.
///
/// A tool result whose call was cut away — or a call whose result was never
/// recorded — is a request Anthropic and OpenAI both reject. A call goes to
/// the session file the moment it starts (`chat.rs`, `ToolStarted`), so a
/// cancel or a crash mid-round leaves one behind, and it can sit anywhere in
/// the file rather than only at its end. Such a round is repaired in place:
/// dropping everything after it would silently lose the rest of the session,
/// which is the larger loss. The window is therefore sometimes shorter than
/// `limit`, never inconsistent.
pub fn restore_window(
    mut messages: Vec<titi_providers::ChatMessage>,
    limit: usize,
) -> Vec<titi_providers::ChatMessage> {
    repair_rounds(&mut messages);
    let mut start = messages.len().saturating_sub(limit);
    start += messages[start..]
        .iter()
        .take_while(|message| message.role == titi_providers::Role::Tool)
        .count();
    messages.drain(..start);
    messages
}

/// Drops every tool call that has no result, and the partial results that
/// referenced it. The assistant's prose is kept: what it said still belongs
/// to the conversation even when the call it opened never came back.
fn repair_rounds(messages: &mut Vec<titi_providers::ChatMessage>) {
    let mut index = 0;
    while index < messages.len() {
        let calls = messages[index].tool_calls.len();
        if calls == 0 {
            index += 1;
            continue;
        }
        let results = messages[index + 1..]
            .iter()
            .take_while(|message| message.role == titi_providers::Role::Tool)
            .count();
        if results < calls {
            messages[index].tool_calls.clear();
            messages.drain(index + 1..index + 1 + results);
            index += 1;
            continue;
        }
        index += 1 + results;
    }
    messages.retain(|message| {
        message.role != titi_providers::Role::Assistant
            || !message.content.is_empty()
            || !message.tool_calls.is_empty()
    });
}

/// Parses `--approval <always-ask|write|yolo>`.
/// Privacy from settings: which files are credentials, and whether IPv4
/// addresses are masked in tool output.
///
/// A cloned repo controls its `.titi/config.yml`, so it may add sensitive
/// files but can never turn masking off or open a file: `privacy.maskIps`
/// and `privacy.allow` are read from the user's own layers only.
pub fn privacy_policy(settings: &titi_config::settings::Settings) -> (SensitivePolicy, bool) {
    let strings = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let extra = settings
        .layer_values("privacy.sensitive")
        .iter()
        .flat_map(strings)
        .collect();
    let allow = settings
        .get_user("privacy.allow")
        .map(|value| strings(&value))
        .unwrap_or_default();
    let mask_ips = settings
        .get_user("privacy.maskIps")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    (SensitivePolicy::new(extra, allow), mask_ips)
}

pub fn parse_approval(raw: &str) -> Result<ApprovalMode, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always-ask" | "ask" => Ok(ApprovalMode::AlwaysAsk),
        "write" => Ok(ApprovalMode::Write),
        "yolo" | "auto" => Ok(ApprovalMode::Yolo),
        other => Err(format!(
            "unknown approval mode {other:?}; expected always-ask, write or yolo"
        )),
    }
}

/// Starts the engine with the default approval policy.
pub fn start_engine() -> Result<(Engine, Vec<String>, String), String> {
    start_engine_with(ApprovalMode::Write)
}

/// Starts the engine with an explicit approval policy.
///
/// A surface that cannot show an approval prompt must not leave a write-tier
/// call waiting for one: under `always-ask` and `write` the engine emits
/// `ToolApprovalNeeded` and blocks until an `ApproveTool` arrives. Headless
/// scripts that never answer hang there forever, which is why the policy is a
/// flag rather than a constant.
pub fn start_engine_with(
    approval_mode: ApprovalMode,
) -> Result<(Engine, Vec<String>, String), String> {
    let config = load_registry_config();
    let models: Vec<String> = config
        .models
        .iter()
        .map(|model| model.id.to_string())
        .collect();
    // Windows stay with their model ids: the primary may not be the first
    // entry once models without a key are set aside.
    let windows: Vec<(String, Option<u64>)> = config
        .models
        .iter()
        .map(|model| (model.id.to_string(), model.context_window))
        .collect();
    let provider_ids: Vec<String> = config
        .providers
        .iter()
        .map(|provider| provider.id.to_string())
        .collect();
    let registry = Arc::new(
        ProviderRegistry::new(
            config,
            Arc::new(LayeredCredentialSource::from_defaults()),
            Arc::new(HttpTransportFactory),
        )
        .map_err(|error| error.to_string())?,
    );
    // A missing key fails the turn immediately, so a keyless model must not
    // sit in front of one that can actually run. If none have a key, keep the
    // catalog order and let the first request say which credential is missing.
    let models = prefer_available_models(models, |id| registry.resolve(id).is_ok());
    let context_window = models
        .first()
        .and_then(|id| windows.iter().find(|(model, _)| model == id))
        .and_then(|(_, window)| *window);
    let primary = models
        .first()
        .cloned()
        .ok_or_else(|| "no models configured".to_owned())?;
    let mut engine_config = EngineConfig::new(primary.clone());
    engine_config.approval_mode = approval_mode;
    // The model declares its window; compaction folds at a share of it.
    if let Some(window) = context_window {
        engine_config.context_window = window;
    }
    engine_config.fallback_models = models.iter().skip(1).map(|id| id.clone().into()).collect();
    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if std::env::var_os("TITI_NO_GENOME").is_none() {
        engine_config.genome_root = Some(workspace.clone());
    }
    // A subagent runs the same tool loop as the main turn, in the workspace,
    // on the chosen model. Read-only by default; writes stay with the main
    // turn, which has an approval surface.
    engine_config.workspace_root = Some(workspace.clone());
    engine_config.agent_model = Some(primary.clone().into());
    // Identity, personality and memory live in the agent directory.
    engine_config.agent_dir = Some(titi_config::agent_dir());
    // No runner is passed: with agent_model and workspace_root set, the runtime
    // builds a ToolAgentRunner and hands it its own claims, touched set and
    // read cache. Passing a StreamingAgentRunner here would take its place and
    // leave the subagent unable to call a single tool.
    let agent_dir = titi_config::agent_dir();
    let settings = titi_config::settings::Settings::load(&agent_dir, &workspace, &[]).ok();
    // A config that fails to load keeps the strict defaults.
    let (sensitive, mask_ips) = settings
        .as_ref()
        .map(privacy_policy)
        .unwrap_or_else(|| (SensitivePolicy::default(), true));
    engine_config.sensitive = sensitive.clone();
    engine_config.mask_ips = mask_ips;
    let mut tools = ToolRegistry::new();
    // One cache for the main turn and every subagent it spawns.
    let read_cache = titi_tools::ReadCache::default();
    engine_config.read_cache = read_cache.clone();
    for tool in workspace_tools_with_policy(&workspace, read_cache, sensitive) {
        tools.register(Arc::from(tool));
    }
    tools.register(std::sync::Arc::new(
        titi_memory::tool::MemoryTool::with_providers(agent_dir.clone(), provider_ids),
    ));
    // `memory.embeddingModel` picks the vector space. Empty or "local" keeps
    // the trigram embedder; a model id is resolved through the registry.
    if let Some(settings) = &settings {
        engine_config.embedding_model = settings
            .get("memory.embeddingModel")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty() && s != "local");
    }
    // Resume the newest session and replay its history into the engine;
    // otherwise start a fresh one.
    let mut restored = Vec::new();
    let session_id = match titi_core::session::store::SessionStore::new(&agent_dir) {
        Ok(store) => match store.restore_latest() {
            Ok(Some((id, _))) => {
                // One place builds the replayed history, so `/rewind` and
                // startup cannot disagree about what the model sees.
                restored = crate::app::session_history(&agent_dir, &id).unwrap_or_default();
                id
            }
            _ => store
                .create(titi_core::session::SessionMeta {
                    title: Some("titi".into()),
                    source: Some("cli".into()),
                    ..Default::default()
                })
                .unwrap_or_else(|_| "session".into()),
        },
        Err(_) => "session".into(),
    };
    engine_config.restored_messages = restored;
    let recorder = titi_core::trajectory::TrajectoryRecorder::open(&agent_dir, &session_id).ok();
    let trajectory: TrajectorySink = std::sync::Arc::new(tokio::sync::Mutex::new(recorder));
    Ok((
        EngineRuntime::start_with_session(engine_config, registry, None, tools, trajectory),
        models,
        session_id,
    ))
}
