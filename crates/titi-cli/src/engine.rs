use std::collections::BTreeMap;
use std::sync::Arc;

use titi_engine::protocol::SessionMode;
use titi_engine::{
    Engine, EngineConfig, EngineRuntime, HttpTransportFactory, LayeredCredentialSource,
    ModelDescriptor, ModelPrice, ProviderDescriptor, ProviderRegistry, ProviderRegistryConfig,
    TrajectorySink,
};
use titi_providers::ApiKind;
use titi_tools::{ApprovalMode, SensitivePolicy, ToolRegistry, workspace_tools_with_interrupt};

pub fn default_registry_config() -> ProviderRegistryConfig {
    let mut config = ProviderRegistryConfig {
        providers: vec![
            ProviderDescriptor {
                id: "openai".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://api.openai.com/v1".into(),
                credential_env: Some("OPENAI_API_KEY".into()),
                credential_required: true,
                discover_with_credential: false,
            },
            ProviderDescriptor {
                id: "openrouter".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://openrouter.ai/api/v1".into(),
                credential_env: Some("OPENROUTER_API_KEY".into()),
                credential_required: true,
                discover_with_credential: false,
            },
            ProviderDescriptor {
                id: "opencode-go".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://opencode.ai/zen/go/v1".into(),
                credential_env: Some("OPENCODE_API_KEY".into()),
                credential_required: true,
                discover_with_credential: false,
            },
            ProviderDescriptor {
                id: "anthropic".into(),
                api: ApiKind::AnthropicMessages,
                base_url: "https://api.anthropic.com".into(),
                credential_env: Some("ANTHROPIC_API_KEY".into()),
                credential_required: true,
                discover_with_credential: false,
            },
            // A ChatGPT subscription instead of an API key: the credential
            // comes from `titi --login openai-codex`, so there is no
            // `credential_env` to fall back on. `credential_required` keeps
            // the keyless listing pass away from this endpoint;
            // `discover_with_credential` lets the registry ask it for the
            // live catalog once a token is stored, and the ids below are what
            // the descriptor ships before that.
            ProviderDescriptor {
                id: "openai-codex".into(),
                api: ApiKind::OpenAiResponses,
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                credential_env: None,
                credential_required: true,
                discover_with_credential: true,
            },
            // Cheap OpenAI-compatible gateways: plain Chat Completions, so
            // they reuse the compat transport and add no provider branch.
            ProviderDescriptor {
                id: "clinepass".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://api.cline.bot/api/v1".into(),
                credential_env: Some("CLINE_API_KEY".into()),
                credential_required: true,
                discover_with_credential: false,
            },
            ProviderDescriptor {
                id: "bai".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "https://api.b.ai/v1".into(),
                credential_env: Some("BAI_API_KEY".into()),
                credential_required: true,
                discover_with_credential: false,
            },
            // Local servers. No key, and no built-in models: what is loaded
            // is whatever the user pulled, so the ids come from their config
            // (or, later, from the endpoint itself). Nothing here contacts
            // the server, so an absent one costs a failed request, not a
            // failed start.
            ProviderDescriptor {
                id: "ollama".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "http://127.0.0.1:11434/v1".into(),
                credential_env: None,
                credential_required: false,
                discover_with_credential: false,
            },
            ProviderDescriptor {
                id: "lmstudio".into(),
                api: ApiKind::OpenAiCompletions,
                base_url: "http://127.0.0.1:1234/v1".into(),
                credential_env: None,
                credential_required: false,
                discover_with_credential: false,
            },
        ],
        models: vec![
            ModelDescriptor {
                id: "openai/gpt-4.1".into(),
                provider: "openai".into(),
                wire_model: "gpt-4.1".into(),
                context_window: Some(1_000_000),
                price: None,
            },
            ModelDescriptor {
                id: "openrouter/gpt-4.1".into(),
                provider: "openrouter".into(),
                wire_model: "openai/gpt-4.1".into(),
                context_window: Some(1_000_000),
                price: None,
            },
            ModelDescriptor {
                id: "opencode-go/glm-5.3-flash".into(),
                provider: "opencode-go".into(),
                wire_model: "glm-5.3-flash".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "opencode-go/deepseek-v4-flash".into(),
                provider: "opencode-go".into(),
                wire_model: "deepseek-v4-flash".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "anthropic/claude-sonnet-4-5".into(),
                provider: "anthropic".into(),
                wire_model: "claude-sonnet-4-5".into(),
                context_window: Some(200_000),
                price: None,
            },
            ModelDescriptor {
                id: "clinepass/glm-5.3".into(),
                provider: "clinepass".into(),
                wire_model: "glm-5.3".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "clinepass/deepseek-v4-flash".into(),
                provider: "clinepass".into(),
                wire_model: "deepseek-v4-flash".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "clinepass/deepseek-v4-pro".into(),
                provider: "clinepass".into(),
                wire_model: "deepseek-v4-pro".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "bai/glm-5.3-flash".into(),
                provider: "bai".into(),
                wire_model: "glm-5.3-flash".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "bai/qwen3.8-flash".into(),
                provider: "bai".into(),
                wire_model: "qwen3.8-flash".into(),
                context_window: None,
                price: None,
            },
            ModelDescriptor {
                id: "bai/qwen3.8-max".into(),
                provider: "bai".into(),
                wire_model: "qwen3.8-max".into(),
                context_window: None,
                price: None,
            },
        ],
    };
    config.models.extend(codex_models());
    config
}

/// Codex wire ids, from the omp catalog rule
/// (`pi-catalog/src/compat/rules/providers/openai-codex.kdl`, MIT, and the
/// wire census in `docs/research/providers-streaming/oauth-login.md`). They
/// are what the descriptor offers before a login: once a token is stored the
/// registry folds the endpoint's own listing in beside them, and a declared
/// id always wins. The `-wm` ids are the worker siblings.
const CODEX_WIRE_MODELS: &[(&str, Option<u64>)] = &[
    ("gpt-5.5", None),
    ("gpt-5.6", None),
    ("gpt-5.6-luna", None),
    ("gpt-5.6-sol", None),
    ("gpt-5.6-terra", None),
    ("gpt-6-astra", Some(272_000)),
    ("gpt-6-astra-wm", Some(272_000)),
    ("gpt-6-sol", None),
    ("gpt-6-sol-wm", None),
    ("gpt-6-luna", None),
    ("gpt-6-luna-wm", None),
    ("gpt-daybreak-blue-latest", None),
    ("gpt-daybreak-blue-latest-wm", None),
    ("gpt-daybreak-red-latest", None),
    ("gpt-daybreak-red-latest-wm", None),
];

fn codex_models() -> Vec<ModelDescriptor> {
    CODEX_WIRE_MODELS
        .iter()
        .map(|(wire, window)| ModelDescriptor {
            id: format!("openai-codex/{wire}").into(),
            provider: "openai-codex".into(),
            wire_model: (*wire).into(),
            context_window: *window,
            price: None,
        })
        .collect()
}

/// Every built-in model whose price this repo cannot state, and why.
///
/// The catalogue above is a list of ids, never a price list: titi fetches no
/// price at build or run time (no network on either path), and no note in the
/// tree covers these models. The one price note there is —
/// `docs/research/prompt-cache.md` — states Anthropic's Sonnet 4.5 *input*
/// ($3/MTok) and cached-*read* ($0.30/MTok) rates but not its output rate,
/// and output is most of a coding turn's bill; half a price would understate
/// every figure the footer printed, so `anthropic/claude-sonnet-4-5` is named
/// here too. Every id below ships unpriced, and unpriced is not free: the
/// surfaces omit the money rather than print `$0.000`, and `/budget $2` is
/// refused instead of converted at a rate nobody stated.
///
/// This list is the table's anti-rot pin. A price belongs on the descriptor
/// ([`ModelDescriptor::price`]) and an id with no source belongs here; the
/// test `every_builtin_model_is_priced_or_on_the_no_price_list` walks
/// `default_registry_config()` and refuses an id that is neither, so adding a
/// model forces the decision instead of shipping a silent gap.
///
/// A user who knows a price can supply it: a `models` entry in the settings
/// merges over the descriptor built here (same id, whole descriptor) and the
/// money appears in the footer and in `/usage` from the next turn on.
pub const NO_PRICE_MODELS: &[&str] = &[
    // Metered, but priced by nobody whose numbers are in this tree.
    "openai/gpt-4.1",
    "openrouter/gpt-4.1",
    "anthropic/claude-sonnet-4-5",
    // Subscription and gateway backends: flat-rate plans, so no per-token
    // price exists for this repo to write down.
    "opencode-go/glm-5.3-flash",
    "opencode-go/deepseek-v4-flash",
    "clinepass/glm-5.3",
    "clinepass/deepseek-v4-flash",
    "clinepass/deepseek-v4-pro",
    "bai/glm-5.3-flash",
    "bai/qwen3.8-flash",
    "bai/qwen3.8-max",
    "openai-codex/gpt-5.5",
    "openai-codex/gpt-5.6",
    "openai-codex/gpt-5.6-luna",
    "openai-codex/gpt-5.6-sol",
    "openai-codex/gpt-5.6-terra",
    "openai-codex/gpt-6-astra",
    "openai-codex/gpt-6-astra-wm",
    "openai-codex/gpt-6-sol",
    "openai-codex/gpt-6-sol-wm",
    "openai-codex/gpt-6-luna",
    "openai-codex/gpt-6-luna-wm",
    "openai-codex/gpt-daybreak-blue-latest",
    "openai-codex/gpt-daybreak-blue-latest-wm",
    "openai-codex/gpt-daybreak-red-latest",
    "openai-codex/gpt-daybreak-red-latest-wm",
];

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

/// The model a session starts on, and the line owed when a pin could not be
/// honoured.
///
/// A pin that is not available — an id the registry cannot resolve, whether it
/// is unknown or keyless — is never a silent fallback: the session starts on
/// the model it would have used anyway, and the caller is handed one line
/// naming both, to show in the screen or print in a headless run. A pin that
/// *is* available is the session's model, and the line is `None`.
pub fn start_model(
    models: &[String],
    pinned: Option<&str>,
    mut available: impl FnMut(&str) -> bool,
) -> (String, Option<String>) {
    let fallback = models.first().cloned().unwrap_or_default();
    let Some(pinned) = pinned else {
        return (fallback, None);
    };
    if available(pinned) {
        return (pinned.to_owned(), None);
    }
    let note = format!(
        "{}: {pinned} is not available · starting on {fallback}",
        titi_config::settings::MODEL_DEFAULT_ROLE_KEY
    );
    (fallback, Some(note))
}

/// The model list a surface offers.
///
/// The startup order comes first and never moves: it is the availability
/// order, models with a key ahead of models without. Whatever the registry
/// has learned since — a local server answers well after the first frame —
/// follows, in the registry's own sorted order. Rows therefore never jump
/// under the cursor while a listing arrives.
#[derive(Clone)]
pub struct ModelCatalog {
    startup: Vec<String>,
    registry: Option<Arc<ProviderRegistry>>,
    /// Why a provider offered nothing, for surfaces with no registry behind
    /// them (tests, embedded). A real catalog reads the registry's own list
    /// live, because discovery answers long after this is built.
    failures: Vec<titi_providers::DiscoveryError>,
    /// Prices a catalog with no registry behind it carries. A live catalog
    /// asks the registry instead — it holds the descriptors the settings
    /// merged over the built-ins, so a price the user wrote is found there.
    prices: BTreeMap<String, ModelPrice>,
}

impl ModelCatalog {
    pub fn new(startup: Vec<String>, registry: Arc<ProviderRegistry>) -> Self {
        Self {
            startup,
            registry: Some(registry),
            failures: Vec::new(),
            prices: BTreeMap::new(),
        }
    }

    /// A catalog that cannot grow, for surfaces and tests with no registry.
    pub fn fixed(models: Vec<String>) -> Self {
        Self {
            startup: models,
            registry: None,
            failures: Vec::new(),
            prices: BTreeMap::new(),
        }
    }

    /// A fixed catalog that also states what its models cost: for a surface
    /// with no registry behind it.
    ///
    /// A model absent from `prices` is *unpriced*, which is not free: the
    /// money is omitted wherever it would have been printed. The tests that
    /// drive the footer and `/usage` money paths build one of these, because
    /// no built-in model ships with a price ([`NO_PRICE_MODELS`]).
    pub fn fixed_priced(models: Vec<String>, prices: Vec<(String, ModelPrice)>) -> Self {
        Self {
            startup: models,
            registry: None,
            failures: Vec::new(),
            prices: prices.into_iter().collect(),
        }
    }

    /// A fixed catalog that also carries the reasons it is short.
    pub fn fixed_with_failures(
        models: Vec<String>,
        failures: Vec<titi_providers::DiscoveryError>,
    ) -> Self {
        Self {
            startup: models,
            registry: None,
            failures,
            prices: BTreeMap::new(),
        }
    }

    /// What the named model costs, when anything here knows.
    ///
    /// `None` is *unpriced*: a local server's tag, a subscription backend, or
    /// a model whose price nobody wrote down. It must never be read as a
    /// price of zero, so a surface that prints money omits it instead.
    pub fn price(&self, id: &str) -> Option<ModelPrice> {
        if let Some(price) = self.prices.get(id) {
            return Some(*price);
        }
        self.registry
            .as_ref()
            .and_then(|registry| registry.price(id))
    }

    /// Read when a picker opens or a command runs, never per frame: it takes
    /// the registry's read lock.
    pub fn ids(&self) -> Vec<String> {
        let mut ids = self.startup.clone();
        let Some(registry) = &self.registry else {
            return ids;
        };
        for id in registry.model_ids() {
            let id = id.to_string();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// Why the list is short.
    ///
    /// A provider that refused the key looks exactly like a provider that has
    /// no models, and the difference is the only one the user can do anything
    /// about. Read at the same moments as [`ModelCatalog::ids`], and only
    /// ever failures worth acting on — a local server that is not running
    /// says nothing here.
    pub fn discovery_failures(&self) -> Vec<titi_providers::DiscoveryError> {
        let mut failures = self.failures.clone();
        if let Some(registry) = &self.registry {
            failures.extend(registry.discovery_errors());
        }
        failures
    }

    /// Re-reads the registry after a credential appears — a `/login`.
    ///
    /// The startup order was decided from the keys that existed then, so a
    /// model that has just become usable sits behind the ones that already
    /// were. `resolve` is the acceptance `start_engine_with` applies, and
    /// discovery is re-asked: a provider that lists its catalog against a
    /// stored credential has one now. A catalog with no registry behind it
    /// does not move.
    pub fn refresh_after_login(&mut self) {
        let Some(registry) = &self.registry else {
            return;
        };
        registry.spawn_local_discovery();
        let mut candidates = self.startup.clone();
        for id in registry.model_ids() {
            let id = id.to_string();
            if !candidates.contains(&id) {
                candidates.push(id);
            }
        }
        self.startup = prefer_available_models(candidates, |id| registry.resolve(id).is_ok());
    }
}

/// Models offered by the picker — the fallback chains from
/// `docs/research/STATE.md`.
pub fn model_choices() -> Vec<String> {
    [
        "opencode-go/glm-5.3-flash",
        "clinepass/glm-5.3",
        "opencode-go/deepseek-v4-flash",
        "clinepass/deepseek-v4-flash",
        "bai/glm-5.3-flash",
        "bai/qwen3.8-flash",
        "clinepass/deepseek-v4-pro",
        "qwen3.8-max",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// A refusal short enough for a one-line header, where the full sentence
/// would be truncated away. The provider and the status are the two things
/// the user needs; the rest is in the transcript line beside it.
pub fn short_discovery_reason(error: &titi_providers::DiscoveryError) -> String {
    match error.status() {
        Some(status) => format!(
            "{}: HTTP {status} — check the key with `titi --set-key`",
            error.provider()
        ),
        None => format!("{}: no model list", error.provider()),
    }
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
    // The user's own models lead the catalog. A config that declares a model
    // means that model, and a builtin sitting in front of it would be the one
    // the session runs on — and, through `agent_model`, the one a subagent
    // asks for, which is how a one-provider config ends up with a subagent
    // asking for an id the user never declared.
    let mut models = overlay.models;
    for model in base.models {
        if !models.iter().any(|item| item.id == model.id) {
            models.push(model);
        }
    }
    base.models = models;
    base
}

/// The effective provider registry: the builtins with the user's
/// `providers`/`models` merged over them.
///
/// Surfaces must validate against this, not against the builtin table: a
/// provider the user declared is one the engine will happily run, so a
/// screen that only knows the builtins refuses keys for models it is about
/// to call.
///
/// The catalog is read from the user's own layers only, never from `cwd`'s
/// `.titi/config.yml`: a provider entry names the `base_url` a key is sent to
/// and the `credential_env` it is read from, so a cloned repo that could
/// redefine `openai` would receive the user's key with its first request.
pub fn registry_config_for(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> ProviderRegistryConfig {
    registry_config_for_with_problems(agent_dir, cwd).0
}

/// The same registry, with what the user's own block got wrong.
///
/// One problem per price that could not be read, each naming the key — a
/// negative rate, a word where a figure belongs, a seventh decimal place — and
/// empty for a config that is entirely readable. The model itself is still
/// there, unpriced: a price nobody can read is not a price of zero, and the
/// engine says so rather than charging the turn as free.
pub fn registry_config_for_with_problems(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> (ProviderRegistryConfig, Vec<String>) {
    let defaults = default_registry_config();
    let Ok(settings) = titi_config::settings::Settings::load(agent_dir, cwd, &[]) else {
        return (defaults, Vec::new());
    };
    let user = serde_json::json!({
        "providers": settings.get_user("providers"),
        "models": settings.get_user("models"),
    });
    match ProviderRegistryConfig::from_user_settings(&user) {
        (Some(parsed), problems) => (merge_registry_config(defaults, parsed), problems),
        (None, problems) => (defaults, problems),
    }
}

pub fn load_registry_config() -> ProviderRegistryConfig {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (config, problems) = registry_config_for_with_problems(&titi_config::agent_dir(), &cwd);
    for problem in &problems {
        // Said once, where the session starts: the screen owns the terminal
        // from here, so a price nobody could read has to be said now or not
        // at all. The model is unpriced, and `/budget $` says as much again
        // when a cap meets it.
        eprintln!("titi: config: {problem}");
    }
    config
}

/// Starts the engine, returning it with the model catalog and the session id
/// it resumed or created.
/// Messages replayed from a resumed session. Older turns are dropped: past a
/// point they crowd the request without informing the next one.
pub const MAX_RESTORED_MESSAGES: usize = 40;

/// The session this launch runs on, and the history it replays.
///
/// `resume` is `--continue`, or `session.autoResume` in the settings. When
/// either asks for it, the newest session the agent directory holds is
/// reopened with its conversation — the same listing Ctrl+X and `/sessions`
/// show, so the session a launch continues is the first row of the picker.
/// When neither asks, the launch starts blank even though the directory is
/// full of sessions: a continued conversation is always one the user asked
/// for, and one they did not stays untouched on disk.
///
/// Split from [`start_engine_with`] so the rule is testable without a
/// provider registry; the wiring is the same either way.
pub fn launch_session(
    agent_dir: &std::path::Path,
    resume: bool,
) -> (String, Vec<titi_providers::ChatMessage>) {
    let Ok(store) = titi_core::session::store::SessionStore::new(agent_dir) else {
        return ("session".into(), Vec::new());
    };
    if resume && let Some(id) = crate::session_fs::newest_session(agent_dir) {
        let history = crate::session_fs::session_history(agent_dir, &id).unwrap_or_default();
        return (id, history);
    }
    let id = store
        .create(titi_core::session::SessionMeta {
            title: Some("titi".into()),
            source: Some("cli".into()),
            ..Default::default()
        })
        .unwrap_or_else(|_| "session".into());
    (id, Vec::new())
}

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

/// Genome prompt-map cap from settings; `genome.limit` as a plain integer.
///
/// Read through the effective view, so a project's `.titi/config.yml` may set
/// it: unlike privacy and approval, the cap is not a latch a cloned repo must
/// not loosen. Anything but an integer in 1..=64 — missing key, failed load,
/// wrong type, out of range — keeps the engine default: a bad cap must not
/// stop startup.
pub fn genome_limit_from(settings: &titi_config::settings::Settings) -> usize {
    settings
        .get(titi_config::settings::GENOME_LIMIT_KEY)
        .and_then(|value| value.as_i64())
        .filter(|limit| (1..=64).contains(limit))
        .map_or(24, |limit| limit as usize)
}

/// Whether the genome prompt map is enabled from settings; `genome.enabled` as a boolean.
///
/// Missing key → true (default on). JSON bool true/false → that. String `on`/`true`/`yes` →
/// true, `off`/`false`/`no` → false (ascii case-insensitive). Anything else (number, array,
/// `maybe`) → true (do not refuse startup). The `TITI_NO_GENOME` env var is handled separately,
/// at engine startup, and takes precedence over this setting.
pub fn genome_enabled_from(settings: &titi_config::settings::Settings) -> bool {
    match settings.get(titi_config::settings::GENOME_ENABLED_KEY) {
        None => true, // unset means on
        Some(value) => {
            if let Some(b) = value.as_bool() {
                b
            } else if let Some(s) = value.as_str() {
                let lower = s.to_ascii_lowercase();
                match lower.as_str() {
                    "on" | "true" | "yes" => true,
                    "off" | "false" | "no" => false,
                    _ => true, // anything else defaults to on
                }
            } else {
                true // numbers, arrays, etc. default to on
            }
        }
    }
}

/// Whether a launch resumes the newest session without being asked to;
/// `session.autoResume` as a boolean.
///
/// Missing key → false (default off). JSON bool → that. String
/// `on`/`true`/`yes` → true, `off`/`false`/`no` → false (ascii
/// case-insensitive). Anything else — a number, an array, a typo — → false,
/// because a key that cannot be read must not turn a blank launch into a
/// resumed one. This key only says *whether* to continue; which session that
/// is belongs to [`launch_session`].
pub fn session_auto_resume_from(settings: &titi_config::settings::Settings) -> bool {
    match settings.get(titi_config::settings::SESSION_AUTO_RESUME_KEY) {
        None => false,
        Some(value) => {
            if let Some(b) = value.as_bool() {
                b
            } else if let Some(s) = value.as_str() {
                matches!(s.to_ascii_lowercase().as_str(), "on" | "true" | "yes")
            } else {
                false
            }
        }
    }
}

/// The status note `/genome` prints: the four facts as plain lines.
///
/// One formatter is shared by the chat and `titi genome`, so a note cannot
/// disagree with what the terminal command prints for the same directory.
/// `reason` is about the *key*: a fresh directory that loads fine reads
/// `default`, and the env switch says so ahead of everything else.
pub fn genome_note(
    settings: &Option<titi_config::settings::Settings>,
    agent_dir: &std::path::Path,
) -> String {
    let enabled = settings.as_ref().is_none_or(genome_enabled_from);
    let key_set = settings
        .as_ref()
        .and_then(|s| s.resolve_source(titi_config::settings::GENOME_ENABLED_KEY))
        .is_some();
    let (state, reason) = if std::env::var_os("TITI_NO_GENOME").is_some() {
        ("off", "TITI_NO_GENOME")
    } else if key_set {
        if enabled {
            ("on", "setting")
        } else {
            ("off", "setting")
        }
    } else {
        ("on", "default")
    };
    let limit = settings.as_ref().map(genome_limit_from).unwrap_or(24);
    format!(
        "genome: {state}\nreason: {reason}\nlimit: {limit}\nconfig: {}",
        agent_dir.join("config.yml").display()
    )
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

/// Starts the engine with the default approval policy, in agent mode.
pub fn start_engine() -> Result<(Engine, ModelCatalog, String, Option<String>), String> {
    start_engine_with(ApprovalMode::Write, SessionMode::Agent, false)
}

/// Starts the engine with an explicit approval policy and session mode.
///
/// A surface that cannot show an approval prompt must not leave a write-tier
/// call waiting for one: under `always-ask` and `write` the engine emits
/// `ToolApprovalNeeded` and blocks until an `ApproveTool` arrives. Headless
/// scripts that never answer hang there forever, which is why the policy is a
/// flag rather than a constant.
///
/// `mode` is what `--mode plan|duck` starts the session in; the surface can
/// change it later with `SetMode`.
///
/// `resume` is `--continue` on the command line. `session.autoResume` is read
/// here, from the same settings load the rest of the config uses, so the flag
/// and the key share one decision; see [`launch_session`] for which session
/// each one continues.
pub fn start_engine_with(
    approval_mode: ApprovalMode,
    mode: SessionMode,
    resume: bool,
) -> Result<(Engine, ModelCatalog, String, Option<String>), String> {
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
    // Local servers introduce themselves on their own time. The catalog is
    // already complete without them, so this is fire-and-forget: whatever
    // answers joins the registry, and whatever does not is simply absent.
    registry.spawn_local_discovery();
    // A missing key fails the turn immediately, so a keyless model must not
    // sit in front of one that can actually run. If none have a key, keep the
    // catalog order and let the first request say which credential is missing.
    let models = prefer_available_models(models, |id| registry.resolve(id).is_ok());
    // The config may pin the model the session starts on (`modelRoles.default`).
    // Unset is what it always was — the first available model — and a pin that
    // cannot be honoured starts on that same model and owes one line saying so.
    let pinned = titi_config::settings::Settings::load(
        &titi_config::agent_dir(),
        &crate::session_fs::current_workspace(),
        &[],
    )
    .ok()
    .and_then(|settings| titi_config::settings::pinned_model(&settings));
    let (primary, pin_note) = start_model(&models, pinned.as_deref(), |id| {
        registry.resolve(id).is_ok()
    });
    // The session's model leads the list a surface offers, so `/model`'s first
    // row and the masthead cannot disagree about which model is in use.
    let models = std::iter::once(primary.clone())
        .chain(models.into_iter().filter(|id| *id != primary))
        .collect::<Vec<String>>();
    let context_window = models
        .first()
        .and_then(|id| windows.iter().find(|(model, _)| model == id))
        .and_then(|(_, window)| *window);
    if primary.is_empty() {
        return Err("no models configured".to_owned());
    }
    let mut engine_config = EngineConfig::new(primary.clone());
    engine_config.approval_mode = approval_mode;
    engine_config.mode = mode;
    // The model declares its window; compaction folds at a share of it.
    if let Some(window) = context_window {
        engine_config.context_window = window;
    }
    engine_config.fallback_models = models.iter().skip(1).map(|id| id.clone().into()).collect();
    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    // Load settings early so genome.enabled can be checked.
    let agent_dir = titi_config::agent_dir();
    let settings = titi_config::settings::Settings::load(&agent_dir, &workspace, &[]).ok();
    // Clear genome_root if TITI_NO_GENOME env is set or genome.enabled is false.
    let genome_enabled = settings.as_ref().map_or(true, genome_enabled_from);
    if std::env::var_os("TITI_NO_GENOME").is_none() && genome_enabled {
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
    // A config that fails to load keeps the strict defaults.
    let (sensitive, mask_ips) = settings
        .as_ref()
        .map(privacy_policy)
        .unwrap_or_else(|| (SensitivePolicy::default(), true));
    engine_config.sensitive = sensitive.clone();
    engine_config.mask_ips = mask_ips;
    // A round's reasoning text is the most sensitive thing a trace holds, so it
    // is opt-in (`trace.thinking: true`); the size and the time are recorded
    // either way. Read through the same helper the editor switch uses: unset is
    // off, and a typo leaves it off rather than quietly turning it on.
    engine_config.trace_thinking = settings.as_ref().is_some_and(|settings| {
        titi_config::settings::switch_on(settings, titi_config::settings::TRACE_THINKING_KEY)
    });
    if let Some(settings) = &settings {
        engine_config.genome_limit = genome_limit_from(settings);
        // The setting names how long a `bash` call may hold the turn before
        // it is handed to the background. A value that is zero or absurd is
        // refused here, by name, instead of starting with a bound nobody
        // meant; a readable `TITI_BASH_BACKGROUND_MS` still wins inside
        // `background_after_with`.
        let threshold = settings
            .auto_background_threshold()
            .map_err(|error| error.to_string())?;
        engine_config.background_after = Some(titi_tools::background_after_with(threshold));
    }
    let mut tools = ToolRegistry::new();
    // One cache for the main turn and every subagent it spawns.
    let read_cache = titi_tools::ReadCache::default();
    engine_config.read_cache = read_cache.clone();
    // Built with the engine's interrupt, so a cancel stops the command a
    // `bash` call is waiting on instead of waiting for it to finish.
    for tool in workspace_tools_with_interrupt(
        &workspace,
        read_cache,
        sensitive,
        engine_config.interrupt.clone(),
    ) {
        tools.register(Arc::from(tool));
    }
    tools.register(std::sync::Arc::new(
        titi_memory::tool::MemoryTool::with_providers(agent_dir.clone(), provider_ids),
    ));
    tools.register(Arc::new(titi_tools::TodoTool::new()));
    struct CliSettingsBackend {
        agent_dir: std::path::PathBuf,
        workspace: std::path::PathBuf,
    }
    impl titi_tools::settings::SettingsBackend for CliSettingsBackend {
        fn resolve_source(&self, key: &str) -> Result<Option<(String, serde_json::Value)>, String> {
            let settings =
                titi_config::settings::Settings::load(&self.agent_dir, &self.workspace, &[])
                    .map_err(|e| e.to_string())?;
            Ok(settings
                .resolve_source(key)
                .map(|(s, v)| (s.to_string(), v)))
        }
        fn set(&self, key: &str, value: serde_json::Value, scope: &str) -> Result<(), String> {
            let mut settings =
                titi_config::settings::Settings::load(&self.agent_dir, &self.workspace, &[])
                    .map_err(|e| e.to_string())?;
            if scope == "global" {
                settings.set(key, value).map_err(|e| e.to_string())
            } else {
                settings.set_project(key, value).map_err(|e| e.to_string())
            }
        }
    }

    tools.register(std::sync::Arc::new(titi_tools::SettingsTool::new(
        std::sync::Arc::new(CliSettingsBackend {
            agent_dir: agent_dir.clone(),
            workspace: workspace.clone(),
        }),
    )));
    // `memory.embeddingModel` picks the vector space. Empty or "local" keeps
    // the trigram embedder; a model id is resolved through the registry.
    if let Some(settings) = &settings {
        engine_config.embedding_model = settings
            .get("memory.embeddingModel")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty() && s != "local");
    }
    // The flag is the user's word for this run; the key is standing consent.
    // Either one asks for the same thing, and `launch_session` decides which
    // session that is; with neither, a launch starts blank.
    let resume = resume || settings.as_ref().is_some_and(session_auto_resume_from);
    let (session_id, restored) = launch_session(&agent_dir, resume);
    engine_config.restored_messages = restored;
    // The engine names this session once the first turn finishes; without the
    // id it has nothing to name.
    engine_config.session_id = Some(session_id.clone());
    let recorder = titi_core::trajectory::TrajectoryRecorder::open(&agent_dir, &session_id).ok();
    let trajectory: TrajectorySink = std::sync::Arc::new(tokio::sync::Mutex::new(recorder));
    // The session's traces: the same shape as the trajectory above, but one
    // file per turn of spans. The recorder is bound to the session here — it is
    // the only place that knows both the agent directory and the id — and the
    // engine opens each turn's file as the turn starts.
    let spans = titi_engine::SpanSink::default();
    *spans
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(titi_engine::SpanRecorder::new(
        agent_dir.clone(),
        session_id.clone(),
    ));
    let catalog = ModelCatalog::new(models, Arc::clone(&registry));
    Ok((
        EngineRuntime::start_with_session(engine_config, registry, None, tools, trajectory, spans),
        catalog,
        session_id,
        pin_note,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session's starting model: unset is the first available one, a pin
    /// that resolves is the session's, and a pin that does not is the default
    /// again with one line naming both.
    #[test]
    fn a_pinned_model_is_the_start_or_a_named_notice() {
        let models = vec!["openai/gpt-4.1".to_owned(), "local/llama".to_owned()];
        let resolves = |id: &str| id == "local/llama";

        // Unset: what it always was.
        assert_eq!(
            start_model(&models, None, resolves),
            ("openai/gpt-4.1".to_owned(), None)
        );
        // Set and available: the session starts on it.
        assert_eq!(
            start_model(&models, Some("local/llama"), resolves),
            ("local/llama".to_owned(), None)
        );
        // Set and not available: the default, and a line that names both the
        // key, the id and what was used instead — never a silent fallback.
        let (primary, note) = start_model(&models, Some("nope/nope"), resolves);
        assert_eq!(primary, "openai/gpt-4.1");
        let note = note.expect("a notice");
        assert!(note.starts_with("modelRoles.default:"), "{note}");
        assert!(note.contains("nope/nope"), "{note}");
        assert!(note.contains("openai/gpt-4.1"), "{note}");
        assert!(!note.contains('\n'), "one line: {note}");

        // An empty list cannot be pinned either: the default is empty and the
        // note still says what happened.
        let (primary, note) = start_model(&[], Some("local/llama"), |_| false);
        assert_eq!(primary, "");
        assert!(note.expect("a notice").contains("local/llama"));
    }
}
