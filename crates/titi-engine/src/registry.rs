use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_providers::{
    ApiKind, CredKind, Credential, DiscoveryError, FamilyTransport, HttpFetch, LadderLevel,
    Transport, TransportError,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: SmolStr,
    pub api: ApiKind,
    pub base_url: SmolStr,
    pub credential_env: Option<SmolStr>,
    #[serde(default = "default_true")]
    pub credential_required: bool,
    /// Whether this provider's catalog comes from a live listing that costs a
    /// stored credential. Off by default: a subscription backend is the only
    /// provider that has a listing worth asking for and a credential that is
    /// not an env var.
    #[serde(default)]
    pub discover_with_credential: bool,
}

const fn default_true() -> bool {
    true
}

/// What a model costs to run, in micro-dollars (millionths of a US dollar)
/// per million tokens.
///
/// Integers, so a hand-written price is exact: `$3/MTok` is `3_000_000`, and
/// the arithmetic in [`ModelPrice::cost_micro_usd`] cannot drift the way a
/// float table would. Dollars per million tokens is how every provider
/// publishes a price, so the unit is the published one.
///
/// The cached-input rate is optional and separate, because the providers
/// that publish one distinguish it: a cached read bills at a share of the
/// input price (Anthropic: a tenth). A provider that does not bill cached
/// reads separately leaves it `None`, and cached tokens then bill as input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Input tokens the provider did not serve from its cache.
    pub input: u64,
    /// Output (completion) tokens.
    pub output: u64,
    /// Input tokens the provider served from its cache. `None` means this
    /// provider does not bill them at a rate of their own.
    #[serde(default)]
    pub cached_input: Option<u64>,
}

impl ModelPrice {
    /// What one turn's usage costs, in micro-dollars.
    ///
    /// Rounded up, never down: a turn that spent anything at all must not
    /// display as `$0.00`, and the ceiling is below one millionth of a
    /// dollar. A zero price still costs zero — a free model is free.
    ///
    /// `cached_tokens` is clamped to `prompt_tokens`: a report whose cached
    /// share exceeds the prompt is a provider bug, and billing more prompt
    /// tokens than were sent would be this code's.
    pub fn cost_micro_usd(
        &self,
        prompt_tokens: u32,
        cached_tokens: u32,
        completion_tokens: u32,
    ) -> u64 {
        let cached = u64::from(cached_tokens).min(u64::from(prompt_tokens));
        let uncached = u64::from(prompt_tokens) - cached;
        let cached_rate = self.cached_input.unwrap_or(self.input);
        let micro = u128::from(uncached) * u128::from(self.input)
            + u128::from(cached) * u128::from(cached_rate)
            + u128::from(completion_tokens) * u128::from(self.output);
        u64::try_from(micro.div_ceil(1_000_000)).unwrap_or(u64::MAX)
    }

    /// A price as a user writes one in `config.yml`: dollars per million
    /// tokens.
    ///
    /// `3`, `0.15`, `12.5` — the digits are read as integers and scaled, never
    /// parsed as a float, so a price the user states exactly is the price the
    /// ledger charges. A sign is not part of an amount and a seventh decimal
    /// place is finer than the unit the ledger keeps, so both are refused
    /// rather than rounded or guessed; the refusal names the key. Zero is a
    /// price, not a missing one: a free model is free.
    ///
    /// `input` and `output` are what a price always states. A provider that
    /// bills a cached read at a rate of its own adds `cachedInput`; one that
    /// does not leaves it out, and cached tokens then bill as input.
    pub fn from_dollars_per_mtok(
        input: &str,
        output: &str,
        cached_input: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            input: as_micro_dollars(input).map_err(|problem| format!("`input`: {problem}"))?,
            output: as_micro_dollars(output).map_err(|problem| format!("`output`: {problem}"))?,
            cached_input: match cached_input {
                Some(text) => Some(
                    as_micro_dollars(text).map_err(|problem| format!("`cachedInput`: {problem}"))?,
                ),
                None => None,
            },
        })
    }
}

/// A decimal dollar figure, as micro-dollars.
///
/// The unit a price is stated in, and the one the ledger keeps. Digits only:
/// `3`, `0.15`, `.5`, `12.5`. At most six decimal places, because a seventh
/// is finer than a micro-dollar and rounding it would charge a rate nobody
/// wrote down.
fn as_micro_dollars(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let unreadable = || {
        format!("{text:?} is not a figure — dollars per million tokens, digits only")
    };
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return Err(unreadable());
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(unreadable());
    }
    if fraction.len() > 6 {
        return Err(format!(
            "{text:?} is finer than a micro-dollar — at most six decimal places"
        ));
    }
    let units: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| format!("{text:?} is too large a figure"))?
    };
    let part: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| format!("{text:?} is too large a figure"))?
    };
    units
        .checked_mul(1_000_000)
        .and_then(|micro| micro.checked_add(part * 10u64.pow(6 - fraction.len() as u32)))
        .ok_or_else(|| format!("{text:?} is too large a figure"))
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
    /// What the model costs, when that is known.
    ///
    /// `None` is *unpriced*, which is not the same as free: a model served
    /// locally, a subscription backend, or a price nobody has written down.
    /// Every surface omits the money for an unpriced model rather than
    /// printing `$0.000`, and `/budget $2` is refused rather than guessed
    /// from a rate nobody stated.
    #[serde(default)]
    pub price: Option<ModelPrice>,
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

    /// The user's own `providers`/`models`, with any price read the way a user
    /// writes one.
    ///
    /// The shape is [`Self::from_settings_value`]'s, with one thing done
    /// first: a model may declare `price: { input, output, cachedInput }` in
    /// dollars per million tokens, which becomes the micro-dollars the
    /// descriptor holds. The keys are the user's own, the conversion
    /// [`ModelPrice::from_dollars_per_mtok`]'s.
    ///
    /// A price that cannot be read is **refused, not rounded and not
    /// dropped**: the model is loaded *unpriced* — which the engine already
    /// treats as "no price is known", never as free — and the problem is
    /// returned naming the key at fault, so the caller can say which line of
    /// the file is wrong. Every other model, and the rest of that entry,
    /// stands.
    pub fn from_user_settings(value: &serde_json::Value) -> (Option<Self>, Vec<String>) {
        let mut value = value.clone();
        let mut problems = Vec::new();
        if let Some(models) = value
            .get_mut("models")
            .and_then(|models| models.as_array_mut())
        {
            for (index, model) in models.iter_mut().enumerate() {
                let Some(entry) = model.as_object_mut() else {
                    continue;
                };
                let Some(price) = entry.remove("price") else {
                    continue;
                };
                match user_price(&price) {
                    Ok(price) => {
                        entry.insert(
                            "price".to_owned(),
                            serde_json::json!({
                                "input": price.input,
                                "output": price.output,
                                "cached_input": price.cached_input,
                            }),
                        );
                    }
                    Err(problem) => problems.push(format!("models[{index}].price: {problem}")),
                }
            }
        }
        (Self::from_settings_value(&value), problems)
    }
}

/// A `price:` block as a user writes it, in the descriptor's micro-dollars.
///
/// `input` and `output` are required and `cachedInput` is optional; each is a
/// number (`3`, `0.15`) or a string of digits (`"0.15"`), so a price can be
/// stated exactly even where a YAML float could not be.
fn user_price(price: &serde_json::Value) -> Result<ModelPrice, String> {
    let Some(price) = price.as_object() else {
        return Err(
            "expected a mapping with `input` and `output` in dollars per million tokens".to_owned(),
        );
    };
    let term = |key: &str| -> Result<String, String> {
        match price.get(key) {
            Some(serde_json::Value::String(text)) => Ok(text.clone()),
            Some(serde_json::Value::Number(number)) => Ok(number.to_string()),
            Some(_) => Err(format!("`{key}` is not a number")),
            None => Err(format!("`{key}` is missing")),
        }
    };
    let input = term("input")?;
    let output = term("output")?;
    let cached_input = match price.get("cachedInput") {
        Some(_) => Some(term("cachedInput")?),
        None => None,
    };
    ModelPrice::from_dollars_per_mtok(&input, &output, cached_input.as_deref())
}

/// Longest one local server may take to list its models.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Most models one local server may contribute.
///
/// The answer is untrusted input: a server with a thousand pulled tags would
/// otherwise bury the paid models in `/model` and in the prompt.
pub const MAX_DISCOVERED_MODELS: usize = 64;

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

/// Model ids an OpenAI-compatible server lists, as this catalog names them.
///
/// The listing itself belongs to [`titi_providers::list_models`], which keeps
/// *why* it failed; this function adds the catalog's own policy on top — which
/// ids are printable, and how many a single server may contribute.
///
/// `credential` is the provider's resolved credential, sent as a bearer token
/// whichever kind it is: an OpenAI-compatible listing authenticates that way,
/// and the `x-api-key` distinction belongs to inference, not to a catalog
/// request. A provider that needs no credential passes `None`, and one that
/// answers 401 or 403 anyway is the user's to fix — the error says so rather
/// than reading as an empty catalog.
pub async fn discover_models(
    provider: &ProviderDescriptor,
    credential: Option<&Credential>,
    fetch: &dyn HttpFetch,
) -> Result<Vec<ModelDescriptor>, DiscoveryError> {
    let key = credential.map(|credential| credential.access.as_str());
    let wire_models =
        titi_providers::list_models(&provider.id, &provider.base_url, key, fetch).await?;
    Ok(wire_models
        .iter()
        .map(|wire| wire.as_str())
        .filter(|wire| is_usable_model_id(wire))
        .take(MAX_DISCOVERED_MODELS)
        .map(|wire| ModelDescriptor {
            id: format!("{}/{wire}", provider.id).into(),
            provider: provider.id.clone(),
            wire_model: wire.into(),
            context_window: None,
            // A server's listing is a list of ids, not a price list: what a
            // pulled tag costs this machine is not something the endpoint
            // states.
            price: None,
        })
        .collect())
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

    /// Renew stored OAuth credentials that are about to expire. The default
    /// source stores nothing to renew.
    ///
    /// Boxed rather than `async fn` because a registry reaches its source
    /// through `dyn CredentialSource`, which an async method cannot serve.
    fn refresh_due<'a>(
        &'a self,
        _fetch: &'a dyn HttpFetch,
    ) -> Pin<Box<dyn Future<Output = Vec<RefreshOutcome>> + Send + 'a>> {
        Box::pin(async { Vec::new() })
    }
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
        Some(Credential::api_key(access, LadderLevel::Env))
    }
}

/// What the refresh sweep did with one stored OAuth row.
///
/// Never carries token material: the row is named by `provider/label`, which
/// is an identifier, and the outcome is one of three states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshState {
    /// A fresh access token was exchanged and written back.
    Refreshed,
    /// The provider definitively rejected the refresh token: the row is gone
    /// and the account has to log in again.
    Quarantined,
    /// A transport failure. The row is untouched — it may still be good, and
    /// deleting it on a flaky network would log the user out.
    Transient,
}

/// One row the sweep looked at, identified by `provider/label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub account: SmolStr,
    pub state: RefreshState,
}

/// Unix seconds, the clock the store writes and the provider skews speak.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
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

    /// Stored OAuth rows whose access token is within its provider's skew of
    /// expiry. Synchronous and network-free: exactly what the sweep below
    /// would renew.
    ///
    /// A row without a refresh token, or without an expiry, is not due: there
    /// is nothing to exchange, and "never expires" is not a deadline.
    pub fn due_oauth_rows(&self) -> Vec<titi_secrets::store::Credential> {
        let now = unix_now();
        let Ok(store) = titi_secrets::store::AuthStore::open(&self.store_path) else {
            return Vec::new();
        };
        let Ok(rows) = store.list() else {
            return Vec::new();
        };
        rows.into_iter()
            .filter(|row| row.kind == "oauth" && row.refresh_token.is_some())
            .filter(|row| {
                let Some(expires_at) = row.expires_at else {
                    return false;
                };
                let skew = titi_providers::oauth::find(&row.provider)
                    .map_or(0, |provider| provider.refresh_skew_secs);
                expires_at - skew <= now
            })
            .collect()
    }

    /// Renew every due OAuth row: exchange the refresh token, write the
    /// result back through the store, and drop a row whose refresh token the
    /// provider has definitively rejected.
    ///
    /// Called in front of a turn's first resolve, outside the synchronous
    /// ladder, because the exchange is network work.
    pub async fn refresh_due(&self, fetch: &dyn HttpFetch) -> Vec<RefreshOutcome> {
        let due = self.due_oauth_rows();
        if due.is_empty() {
            return Vec::new();
        }
        let Ok(store) = titi_secrets::store::AuthStore::open(&self.store_path) else {
            return Vec::new();
        };
        let mut outcomes = Vec::with_capacity(due.len());
        for row in due {
            let account: SmolStr = format!("{}/{}", row.provider, row.label).into();
            let Some(provider) = titi_providers::oauth::find(&row.provider) else {
                // No descriptor: the row is not one titi can renew, and
                // guessing an endpoint for it would be worse than skipping.
                continue;
            };
            let current = titi_providers::oauth::from_stored(&row);
            let state = match titi_providers::oauth::refresh(provider, &current, fetch).await {
                Ok(tokens) => {
                    let mut record =
                        titi_providers::oauth::to_record(&tokens, &row.provider, &row.label);
                    // A refresh renews the token, not the authorization: the
                    // moment the account first logged in stays put.
                    record.authorized_at = row.authorized_at.or(record.authorized_at);
                    // A failed write is transient: the row still holds the
                    // old token and the next turn tries again.
                    match store.store_oauth(record) {
                        Ok(()) => RefreshState::Refreshed,
                        Err(_) => RefreshState::Transient,
                    }
                }
                Err(error) if error.is_terminal() => {
                    let _ = store.remove_account(&row.provider, &row.label);
                    RefreshState::Quarantined
                }
                Err(_) => RefreshState::Transient,
            };
            outcomes.push(RefreshOutcome { account, state });
        }
        outcomes
    }
}

impl CredentialSource for LayeredCredentialSource {
    fn resolve(&self, provider: &ProviderDescriptor) -> Option<Credential> {
        if let Some(key) = provider.credential_env.as_deref()
            && let Some(access) = self.env.resolve(key)
            && !access.trim().is_empty()
        {
            return Some(Credential::api_key(access, LadderLevel::Env));
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
            // OAuth rows carry the account the token belongs to; the wire
            // schemes that need it (Codex) read it from here.
            account_id: stored.account_id.map(SmolStr::new),
            level: LadderLevel::Stored,
        })
    }

    fn refresh_due<'a>(
        &'a self,
        fetch: &'a dyn HttpFetch,
    ) -> Pin<Box<dyn Future<Output = Vec<RefreshOutcome>> + Send + 'a>> {
        // Inherent wins over trait dispatch here: this is the async sweep.
        Box::pin(self.refresh_due(fetch))
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
    /// Why a provider contributed nothing, when the reason is the user's to
    /// act on. Keyed by provider, so a refresh replaces its own last word
    /// instead of stacking duplicates, and ordered so a surface lists them
    /// the same way twice.
    discovery_errors: std::sync::RwLock<BTreeMap<SmolStr, DiscoveryError>>,
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
            discovery_errors: std::sync::RwLock::new(BTreeMap::new()),
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

    /// What the named model costs, when the catalog states a price.
    ///
    /// Read by the surfaces that print money — the turn footer, `/usage`,
    /// `/budget`'s refusal. `None` is *unpriced*: a model the registry
    /// discovered, a local server's tag, or a descriptor nobody priced. It
    /// is never to be read as a price of zero.
    pub fn price(&self, model_id: &str) -> Option<ModelPrice> {
        let models = self.models.read().ok()?;
        models.get(model_id).and_then(|model| model.price)
    }

    /// Discovery failures worth telling the user about, by provider.
    ///
    /// Only the ones a key change would fix. A provider that is simply not
    /// running, answers 404, or returns something that is not a listing has
    /// nothing to offer and nothing to say — that is its normal state, and
    /// reporting it would make every session start with a warning.
    pub fn discovery_errors(&self) -> Vec<DiscoveryError> {
        let Ok(errors) = self.discovery_errors.read() else {
            return Vec::new();
        };
        errors.values().cloned().collect()
    }

    /// Asks every listable provider what it can serve, in the background, one
    /// task each.
    ///
    /// Nothing on the start path waits for this, and no provider waits for
    /// another: each listing joins the catalog the moment it arrives, so a
    /// server that accepts the connection and then goes quiet delays only
    /// itself. Never answering is that provider's normal state, not an error
    /// anyone has to handle. Without a runtime — `--set-key`, tests — there
    /// is nothing to spawn on, and discovery is skipped.
    ///
    /// A provider that needs a key is left alone: asking a paid gateway for
    /// its catalog is a request the user did not make. A provider that opted
    /// in ([`ProviderDescriptor::discover_with_credential`]) is asked with the
    /// credential it already has.
    pub fn spawn_local_discovery(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let providers = self.discoverable_providers();
        if providers.is_empty() {
            return;
        }
        let Ok(fetch) = titi_providers::ReqwestFetch::new() else {
            return;
        };
        let fetch: Arc<dyn HttpFetch> = Arc::new(fetch);
        for (provider, credential) in providers {
            let registry = Arc::clone(self);
            let fetch = Arc::clone(&fetch);
            handle.spawn(async move {
                registry
                    .refresh_models(&provider, credential.as_ref(), fetch.as_ref())
                    .await;
            });
        }
    }

    /// The same pass on an injected fetch, one provider after another: what
    /// [`ProviderRegistry::spawn_local_discovery`] spawns, without a runtime.
    pub async fn discover_providers(&self, fetch: &dyn HttpFetch) {
        for (provider, credential) in self.discoverable_providers() {
            self.refresh_models(&provider, credential.as_ref(), fetch)
                .await;
        }
    }

    /// Providers whose catalog may be asked for, each with the credential the
    /// listing needs.
    ///
    /// A credential-free provider is listed with no credential. A provider
    /// that opted in is listed only once a credential actually resolves: the
    /// credential is the entry ticket, and a request without it would only be
    /// answered 401.
    fn discoverable_providers(&self) -> Vec<(ProviderDescriptor, Option<Credential>)> {
        self.providers
            .values()
            .filter_map(|entry| {
                let provider = &entry.descriptor;
                let keyless = !provider.credential_required && provider.credential_env.is_none();
                if keyless {
                    return Some((provider.clone(), None));
                }
                if !provider.discover_with_credential {
                    return None;
                }
                self.credentials
                    .resolve(provider)
                    .map(|credential| (provider.clone(), Some(credential)))
            })
            .collect()
    }

    /// Folds one provider's current model list into the catalog, and keeps
    /// the reason when there is no list.
    ///
    /// One provider's refusal is its own: the listing runs per provider, so a
    /// gateway that rejects the key costs that gateway's models and nothing
    /// else. The others keep filling the catalog around it.
    async fn refresh_models(
        &self,
        provider: &ProviderDescriptor,
        credential: Option<&Credential>,
        fetch: &dyn HttpFetch,
    ) {
        // Bounds a task rather than a person: a socket that accepts and never
        // answers must not pin a listing future for the life of the process.
        let Ok(listing) = tokio::time::timeout(
            DISCOVERY_TIMEOUT,
            discover_models(provider, credential, fetch),
        )
        .await
        else {
            return;
        };
        match listing {
            Ok(models) => {
                self.clear_discovery_error(&provider.id);
                self.add_models(models);
            }
            // The credential is what has to change, and only the user can
            // change it, so this one is kept for the surface to show.
            Err(error) if error.is_auth() => self.record_discovery_error(error),
            // Not running, no such endpoint, not a listing: this provider
            // has nothing to offer right now, which is not news.
            Err(_) => {}
        }
    }

    fn record_discovery_error(&self, error: DiscoveryError) {
        if let Ok(mut errors) = self.discovery_errors.write() {
            errors.insert(error.provider().into(), error);
        }
    }

    /// A listing that arrives clears the provider's last complaint: the key
    /// the user fixed must not keep warning them.
    fn clear_discovery_error(&self, provider: &str) {
        if let Ok(mut errors) = self.discovery_errors.write() {
            errors.remove(provider);
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

    /// The descriptor's price, which is the same one a surface reads through
    /// [`ProviderRegistry::price`]: the engine and the footer cannot disagree
    /// about what a model costs when there is one place to ask.
    fn price(&self, model: &str) -> Option<ModelPrice> {
        ProviderRegistry::price(self, model)
    }

    fn refresh_due(&self) -> Pin<Box<dyn Future<Output = Vec<RefreshOutcome>> + Send + '_>> {
        Box::pin(async move {
            // The client is built once per process, on the first turn that
            // runs, and only if the TLS stack builds at all.
            let Some(fetch) = shared_fetch() else {
                return Vec::new();
            };
            self.credentials.refresh_due(fetch.as_ref()).await
        })
    }
}

/// One HTTP client per process, for the credentials that renew outside a
/// transport: the sweep reads and writes the store itself, so it has no
/// transport to borrow a fetch from.
fn shared_fetch() -> Option<&'static Arc<dyn HttpFetch>> {
    static FETCH: std::sync::LazyLock<Option<Arc<dyn HttpFetch>>> =
        std::sync::LazyLock::new(|| {
            titi_providers::ReqwestFetch::new()
                .ok()
                .map(|fetch| Arc::new(fetch) as Arc<dyn HttpFetch>)
        });
    FETCH.as_ref()
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
                context_too_long: false,
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
            discover_with_credential: false,
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

    fn priced(model: ModelDescriptor, price: ModelPrice) -> ModelDescriptor {
        ModelDescriptor {
            price: Some(price),
            ..model
        }
    }

    /// A price multiplies the tokens it names, at its own rate: input and
    /// output are priced separately, and the figure is a count of
    /// micro-dollars rather than a float that could drift.
    #[test]
    fn a_price_multiplies_tokens_at_its_own_rate() {
        // $1/MTok in, $2/MTok out, no separate cached rate.
        let price = ModelPrice {
            input: 1_000_000,
            output: 2_000_000,
            cached_input: None,
        };
        // 600 uncached + 400 cached (both at the input rate) + 300 out:
        // 600 + 400 micro-dollars in, 600 out.
        assert_eq!(price.cost_micro_usd(1_000, 400, 300), 1_600);
        assert_eq!(price.cost_micro_usd(0, 0, 1), 2);
    }

    /// A cached rate of its own is what the cached share bills at; without
    /// one, cached tokens are input tokens and bill like them.
    #[test]
    fn a_cached_rate_of_its_own_bills_the_cached_share() {
        let base = ModelPrice {
            input: 1_000_000,
            output: 2_000_000,
            cached_input: None,
        };
        let cheaper_read = ModelPrice {
            cached_input: Some(250_000),
            ..base
        };
        // The same turn: 600 uncached + 400 cached + 300 out.
        assert_eq!(base.cost_micro_usd(1_000, 400, 300), 1_600);
        assert_eq!(cheaper_read.cost_micro_usd(1_000, 400, 300), 1_300);
    }

    /// A cost is never rounded down to nothing: the ceiling keeps a turn
    /// that spent a fraction of a micro-dollar out of `$0.00`.
    #[test]
    fn a_fraction_of_a_micro_dollar_still_costs_one() {
        let price = ModelPrice {
            input: 1,
            output: 1,
            cached_input: None,
        };
        assert_eq!(price.cost_micro_usd(1, 0, 0), 1, "rounded up, not to zero");
        let free = ModelPrice {
            input: 0,
            output: 0,
            cached_input: None,
        };
        assert_eq!(free.cost_micro_usd(1_000_000, 0, 1_000_000), 0);
    }

    /// A report whose cached share exceeds the prompt cannot bill more
    /// prompt tokens than were sent.
    #[test]
    fn a_cached_share_larger_than_the_prompt_is_clamped() {
        let price = ModelPrice {
            input: 1_000_000,
            output: 0,
            cached_input: Some(0),
        };
        assert_eq!(price.cost_micro_usd(100, 5_000, 0), 0);
    }

    /// Unpriced is not free: a descriptor with no price answers `None`
    /// while a priced one answers its own, and the two are different
    /// questions.
    #[test]
    fn an_unpriced_model_has_no_cost_and_a_priced_one_has_its_own() {
        let factory = Arc::new(RecordingFactory::default());
        let config = ProviderRegistryConfig {
            providers: vec![gateway("local", "http://127.0.0.1:11434/v1", None)],
            models: vec![
                model("local/llama", "local", "llama3"),
                priced(
                    model("local/sonnet", "local", "sonnet"),
                    ModelPrice {
                        input: 3_000_000,
                        output: 15_000_000,
                        cached_input: Some(300_000),
                    },
                ),
            ],
        };
        let registry = ProviderRegistry::new(config, Arc::new(NoCredentials), factory)
            .expect("registry builds");

        assert_eq!(registry.price("local/llama"), None, "no price is not zero");
        assert_eq!(
            registry.price("local/sonnet").map(|price| price.input),
            Some(3_000_000)
        );
        assert_eq!(registry.price("nobody/knows"), None);
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
                    account_id: None,
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

        let found = discover_models(&provider, None, &fetch)
            .await
            .expect("a listing the server answered");

        let ids: Vec<&str> = found.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["ollama/qwen3:8b", "ollama/llama3.2"]);
        assert_eq!(found[0].wire_model.as_str(), "qwen3:8b");
        let requests = fetch.requests.lock().expect("recorded requests");
        assert_eq!(requests[0].method.as_str(), "GET");
        assert_eq!(requests[0].url.as_str(), "http://127.0.0.1:11434/v1/models");
    }

    /// A server that is simply not running has nothing to offer, and that is
    /// its normal state: an empty list, not a reason anyone has to read.
    #[tokio::test]
    async fn discovery_is_silent_when_nothing_answers() {
        let fetch = titi_providers::MockFetch::new(vec![Err(TransportError::Retryable {
            status: None,
            message: "request failed: connection refused".into(),
            retry_after: None,
        })]);
        let provider = gateway("lmstudio", "http://127.0.0.1:1234/v1", None);

        let error = discover_models(&provider, None, &fetch)
            .await
            .expect_err("an unreachable server is reported, not invented");
        assert!(!error.is_auth(), "{error}");
        assert_eq!(error.provider(), "lmstudio");
    }

    /// A key the provider refuses is the one failure a user can act on, so
    /// it must survive the call instead of reading as an empty catalog.
    #[tokio::test]
    async fn discovery_keeps_the_reason_a_key_was_refused() {
        for (status, label) in [(401u16, "unauthorized"), (403, "forbidden")] {
            let fetch =
                titi_providers::MockFetch::new(vec![Ok(titi_providers::MockFetchResponse {
                    status,
                    chunks: vec![r#"{"error":{"message":"invalid api key"}}"#.to_owned()],
                })]);
            let provider = gateway("openai", "https://api.openai.com/v1", None);

            let error = discover_models(&provider, None, &fetch)
                .await
                .expect_err("a refused key is not an empty listing");

            assert!(error.is_auth(), "{label}: {error}");
            assert_eq!(error.provider(), "openai");
            assert_eq!(error.status(), Some(status));
            let shown = error.to_string();
            assert!(shown.contains("openai"), "{shown}");
            assert!(shown.contains(&status.to_string()), "{shown}");
        }
    }

    /// A provider with no listing endpoint at all is not a problem to report.
    #[tokio::test]
    async fn a_missing_listing_endpoint_is_not_an_auth_failure() {
        let fetch = titi_providers::MockFetch::new(vec![Ok(titi_providers::MockFetchResponse {
            status: 404,
            chunks: vec!["not found".to_owned()],
        })]);
        let provider = gateway("ollama", "http://127.0.0.1:11434/v1", None);

        let error = discover_models(&provider, None, &fetch)
            .await
            .expect_err("404 is still an answer, not a listing");
        assert!(!error.is_auth(), "{error}");
        assert_eq!(error.status(), Some(404));
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

        let found = discover_models(&provider, None, &fetch)
            .await
            .expect("the flood still parses");

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
            .discoverable_providers()
            .into_iter()
            .map(|(provider, _)| provider.id)
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

        registry.refresh_models(&ollama, None, &fetch).await;

        assert_eq!(registry.model_ids(), vec![SmolStr::from("ollama/qwen3:8b")]);
        assert!(registry.resolve("ollama/qwen3:8b").is_ok());
    }

    /// One refused key costs that provider's models and nothing else: the
    /// rest of the catalog still fills, and the reason is kept where a
    /// surface can read it.
    #[tokio::test]
    async fn a_refused_key_is_reported_without_stopping_the_other_providers() {
        let config = ProviderRegistryConfig {
            providers: vec![
                gateway("gatewayd", "http://127.0.0.1:8080/v1", None),
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

        let refusing = gateway("gatewayd", "http://127.0.0.1:8080/v1", None);
        let refused = titi_providers::MockFetch::new(vec![Ok(titi_providers::MockFetchResponse {
            status: 401,
            chunks: vec![r#"{"error":"invalid api key"}"#.to_owned()],
        })]);
        registry.refresh_models(&refusing, None, &refused).await;

        let answering = gateway("ollama", "http://127.0.0.1:11434/v1", None);
        let listing =
            titi_providers::MockFetch::sse(vec![r#"{"data":[{"id":"qwen3:8b"}]}"#.to_owned()]);
        registry.refresh_models(&answering, None, &listing).await;

        assert_eq!(registry.model_ids(), vec![SmolStr::from("ollama/qwen3:8b")]);
        let reported = registry.discovery_errors();
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(reported[0].provider(), "gatewayd");
        assert!(reported[0].is_auth());
        assert!(
            reported[0].to_string().contains("401"),
            "{}",
            reported[0].to_string()
        );
    }

    /// A provider that is only quiet is not worth a warning, and a key the
    /// user fixed must stop warning them.
    #[tokio::test]
    async fn only_auth_failures_are_kept_and_a_good_listing_clears_them() {
        let config = ProviderRegistryConfig {
            providers: vec![gateway("ollama", "http://127.0.0.1:11434/v1", None)],
            models: Vec::new(),
        };
        let registry = ProviderRegistry::new(
            config,
            Arc::new(NoCredentials),
            Arc::new(RecordingFactory::default()),
        )
        .expect("registry builds");
        let provider = gateway("ollama", "http://127.0.0.1:11434/v1", None);

        let unreachable = titi_providers::MockFetch::new(vec![Err(TransportError::Retryable {
            status: None,
            message: "connection refused".into(),
            retry_after: None,
        })]);
        registry.refresh_models(&provider, None, &unreachable).await;
        assert!(
            registry.discovery_errors().is_empty(),
            "a server that is not running is not news"
        );

        let refused = titi_providers::MockFetch::new(vec![Ok(titi_providers::MockFetchResponse {
            status: 403,
            chunks: vec!["forbidden".to_owned()],
        })]);
        registry.refresh_models(&provider, None, &refused).await;
        assert_eq!(registry.discovery_errors().len(), 1);

        let listing =
            titi_providers::MockFetch::sse(vec![r#"{"data":[{"id":"qwen3:8b"}]}"#.to_owned()]);
        registry.refresh_models(&provider, None, &listing).await;
        assert!(
            registry.discovery_errors().is_empty(),
            "the fixed key kept warning"
        );
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

#[cfg(test)]
mod user_price_tests {
    use super::*;

    /// One user model, with the price block as written.
    fn config(price: serde_json::Value) -> (Option<ProviderRegistryConfig>, Vec<String>) {
        ProviderRegistryConfig::from_user_settings(&serde_json::json!({
            "providers": [{
                "id": "fake",
                "api": "openai-completions",
                "base_url": "http://127.0.0.1:18999/v1",
                "credential_required": false,
            }],
            "models": [{
                "id": "fake/scripted",
                "provider": "fake",
                "wire_model": "fake",
                "price": price,
            }],
        }))
    }

    fn price_of(parsed: &Option<ProviderRegistryConfig>) -> Option<ModelPrice> {
        parsed.as_ref()?.models.first()?.price
    }

    /// A price written the way every provider publishes one is the price the
    /// ledger charges, to the micro-dollar.
    #[test]
    fn a_price_is_read_in_dollars_per_million_tokens() {
        let (parsed, problems) = config(serde_json::json!({
            "input": 3,
            "output": 15,
            "cachedInput": 0.3,
        }));
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(
            price_of(&parsed),
            Some(ModelPrice {
                input: 3_000_000,
                output: 15_000_000,
                cached_input: Some(300_000),
            })
        );
    }

    /// A string is read exactly, which is how a rate finer than a YAML float
    /// can still be stated without a rounding story.
    #[test]
    fn a_price_may_be_written_as_a_string() {
        let (parsed, problems) = config(serde_json::json!({"input": "0.15", "output": "12.5"}));
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(
            price_of(&parsed),
            Some(ModelPrice {
                input: 150_000,
                output: 12_500_000,
                cached_input: None,
            })
        );
    }

    /// Zero is a price, not a missing one: a free model is free, and it has
    /// to be *stated* to be a price of zero rather than an unknown one.
    #[test]
    fn a_free_model_is_a_price_of_zero() {
        let (parsed, problems) = config(serde_json::json!({"input": 0, "output": 0}));
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(
            price_of(&parsed),
            Some(ModelPrice {
                input: 0,
                output: 0,
                cached_input: None,
            })
        );
    }

    /// A model with no price is unpriced, exactly as before this key existed.
    #[test]
    fn a_model_without_a_price_is_left_unpriced() {
        let (parsed, problems) = ProviderRegistryConfig::from_user_settings(
            &serde_json::json!({
                "providers": [{
                    "id": "fake",
                    "api": "openai-completions",
                    "base_url": "http://127.0.0.1:18999/v1",
                    "credential_required": false,
                }],
                "models": [{"id": "fake/scripted", "provider": "fake", "wire_model": "fake"}],
            }),
        );
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(price_of(&parsed), None);
    }

    /// A rate that cannot be read is refused *by key* — and the model is not
    /// thrown away with it: it is loaded unpriced, which the engine already
    /// treats as "no price is known" rather than as free.
    #[test]
    fn a_price_that_cannot_be_read_is_refused_by_key() {
        for (price, expected) in [
            (
                serde_json::json!({"input": -2, "output": 15}),
                "`input`",
            ),
            (
                serde_json::json!({"input": "free", "output": 15}),
                "`input`",
            ),
            (
                serde_json::json!({"input": 0.0000001, "output": 15}),
                "`input`",
            ),
            (
                serde_json::json!({"input": 3, "output": 15, "cachedInput": "-1"}),
                "`cachedInput`",
            ),
            (serde_json::json!({"input": 3}), "`output`"),
            (serde_json::json!({"output": 15}), "`input`"),
            (serde_json::json!(3), "expected a mapping"),
            (serde_json::json!({"input": [3], "output": 15}), "not a number"),
        ] {
            let (parsed, problems) = config(price.clone());
            let named = problems.join(" ");
            assert!(
                named.contains("models[0].price"),
                "{price} is not named by its key: {named:?}"
            );
            assert!(
                named.contains(expected),
                "{price} does not name {expected}: {named:?}"
            );
            let model = parsed
                .as_ref()
                .and_then(|parsed| parsed.models.first())
                .expect("the model stands even when its price does not");
            assert_eq!(model.id.as_str(), "fake/scripted");
            assert_eq!(model.price, None, "{price} must not become a number");
        }
    }
}
