#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use titi_cli::engine::{
    NO_PRICE_MODELS, default_registry_config, merge_registry_config, parse_approval,
    prefer_available_models,
};
use titi_engine::{
    CredentialSource, HttpTransportFactory, ProviderDescriptor, ProviderRegistry,
    ProviderRegistryConfig,
};
use titi_tools::ApprovalMode;

#[test]
fn a_keyless_model_does_not_block_one_that_can_run() {
    let models = vec![
        "openai/gpt-4.1".to_owned(),
        "opencode-go/glm-5.3-flash".to_owned(),
        "openrouter/gpt-4.1".to_owned(),
    ];
    let ordered = prefer_available_models(models.clone(), |id| id.starts_with("opencode"));
    assert_eq!(ordered, vec!["opencode-go/glm-5.3-flash".to_owned()]);
    let unchanged = prefer_available_models(models, |_| false);
    assert_eq!(unchanged[0], "openai/gpt-4.1");
    assert_eq!(unchanged.len(), 3);
}

#[test]
fn approval_modes_parse_and_reject_typos() {
    assert_eq!(parse_approval("write"), Ok(ApprovalMode::Write));
    assert_eq!(parse_approval("always-ask"), Ok(ApprovalMode::AlwaysAsk));
    assert_eq!(parse_approval("ask"), Ok(ApprovalMode::AlwaysAsk));
    assert_eq!(parse_approval("yolo"), Ok(ApprovalMode::Yolo));
    // Case and surrounding space are tolerated; a typo is not.
    assert_eq!(parse_approval("  YOLO "), Ok(ApprovalMode::Yolo));
    let reason = parse_approval("yes").unwrap_err();
    assert!(
        reason.contains("always-ask") && reason.contains("yolo"),
        "the error lists the valid modes: {reason}"
    );
}

/// Any key at all, so the check is about the catalog rather than about what
/// this machine happens to have configured.
struct AlwaysKeyed;

impl CredentialSource for AlwaysKeyed {
    fn resolve(&self, _provider: &ProviderDescriptor) -> Option<titi_providers::Credential> {
        Some(titi_providers::Credential {
            access: "sk-test".into(),
            kind: titi_providers::CredKind::ApiKey,
            account_id: None,
            level: titi_providers::LadderLevel::Env,
        })
    }
}

/// The catalog grows with every provider, so pinning its contents would only
/// buy a test to update. What has to hold is that it is internally sound: a
/// typo in a provider id, a duplicated model or an endpoint the transport
/// layer refuses would each strand a model that the model picker offers.
#[test]
fn every_builtin_model_resolves_through_its_own_provider() {
    let config = default_registry_config();
    let registry = ProviderRegistry::new(
        config.clone(),
        std::sync::Arc::new(AlwaysKeyed),
        std::sync::Arc::new(HttpTransportFactory),
    )
    .expect("the built-in catalog builds a registry");

    for model in &config.models {
        let resolved = registry
            .resolve(model.id.as_str())
            .unwrap_or_else(|error| panic!("{} does not resolve: {error}", model.id));
        assert_eq!(resolved.wire_model, model.wire_model);
    }
}

/// A provider that needs no key must not claim to need one, and a provider
/// that does must have a way to get one: an environment variable, or — for
/// the Codex subscription — a credential stored by `titi --login`.
#[test]
fn a_provider_asks_for_a_key_exactly_when_it_has_one_to_ask_for() {
    for provider in default_registry_config().providers {
        let from_login = provider.id == "openai-codex";
        assert_eq!(
            provider.credential_required,
            provider.credential_env.is_some() || from_login,
            "{} disagrees with itself about needing a key",
            provider.id
        );
    }
}

/// Endpoints are the one thing a registry entry cannot get approximately
/// right: a wrong base URL is a silent failure that only shows up as a
/// request into nowhere.
#[test]
fn the_new_providers_point_at_their_documented_endpoints() {
    let config = default_registry_config();
    let provider = |id: &str| -> ProviderDescriptor {
        config
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap_or_else(|| panic!("{id} is missing from the built-in catalog"))
            .clone()
    };

    let clinepass = provider("clinepass");
    assert_eq!(clinepass.base_url.as_str(), "https://api.cline.bot/api/v1");
    assert_eq!(clinepass.credential_env.as_deref(), Some("CLINE_API_KEY"));

    let bai = provider("bai");
    assert_eq!(bai.base_url.as_str(), "https://api.b.ai/v1");
    assert_eq!(bai.credential_env.as_deref(), Some("BAI_API_KEY"));

    // Local servers: the default ports of Ollama and LM Studio, and no key.
    let ollama = provider("ollama");
    assert_eq!(ollama.base_url.as_str(), "http://127.0.0.1:11434/v1");
    assert!(!ollama.credential_required);

    let lmstudio = provider("lmstudio");
    assert_eq!(lmstudio.base_url.as_str(), "http://127.0.0.1:1234/v1");
    assert!(!lmstudio.credential_required);
}

#[test]
fn overlay_keeps_openai_and_replaces_the_opencode_url() {
    let value = serde_json::json!({
        "providers": [{
            "id": "opencode-go",
            "api": "openai-completions",
            "base_url": "https://example.test/v1",
            "credential_required": true
        }],
        "models": [{
            "id": "opencode-go/custom",
            "provider": "opencode-go",
            "wire_model": "custom"
        }]
    });
    let overlay = ProviderRegistryConfig::from_settings_value(&value).unwrap();
    let merged = merge_registry_config(default_registry_config(), overlay);
    let models: Vec<_> = merged
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert!(models.contains(&"openai/gpt-4.1"));
    assert!(models.contains(&"opencode-go/custom"));
    let opencode = merged
        .providers
        .iter()
        .find(|provider| provider.id.as_str() == "opencode-go")
        .unwrap();
    assert_eq!(opencode.base_url.as_str(), "https://example.test/v1");
    assert_eq!(
        merged
            .providers
            .iter()
            .filter(|provider| provider.id.as_str() == "opencode-go")
            .count(),
        1
    );
}

#[test]
fn overlay_replaces_the_openai_url_once() {
    let value = serde_json::json!({
        "providers": [{
            "id": "openai",
            "api": "openai-completions",
            "base_url": "https://example.test/v1",
            "credential_env": "OPENAI_API_KEY",
            "credential_required": true
        }],
        "models": [{
            "id": "openai/gpt-4.1",
            "provider": "openai",
            "wire_model": "gpt-4.1"
        }]
    });
    let overlay = ProviderRegistryConfig::from_settings_value(&value).unwrap();
    let merged = merge_registry_config(default_registry_config(), overlay);
    let openai: Vec<_> = merged
        .providers
        .iter()
        .filter(|provider| provider.id.as_str() == "openai")
        .collect();
    assert_eq!(openai.len(), 1);
    assert_eq!(openai[0].base_url.as_str(), "https://example.test/v1");
    assert!(
        merged
            .models
            .iter()
            .any(|model| model.id.as_str() == "anthropic/claude-sonnet-4-5")
    );
}

#[test]
fn settings_value_overrides_default_catalog() {
    let value = serde_json::json!({
        "providers": [{
            "id": "local",
            "api": "openai-completions",
            "base_url": "http://127.0.0.1:11434/v1",
            "credential_required": false
        }],
        "models": [{
            "id": "local/llama",
            "provider": "local",
            "wire_model": "llama3"
        }]
    });
    let parsed = ProviderRegistryConfig::from_settings_value(&value).unwrap();
    assert_eq!(parsed.models[0].id, "local/llama");
    assert_eq!(parsed.providers[0].id, "local");
}

/// Adding a built-in model forces a decision: a price on its descriptor, or
/// a line on the explicit no-price list. Without this, a new model could
/// ship silently unpriced and the money would simply never appear for it.
#[test]
fn every_builtin_model_is_priced_or_on_the_no_price_list() {
    let config = default_registry_config();
    for model in &config.models {
        let id = model.id.as_str();
        let listed = NO_PRICE_MODELS.contains(&id);
        match (model.price, listed) {
            (Some(_), false) | (None, true) => {}
            (Some(price), true) => panic!(
                "{id} is priced at {price:?} and also on the no-price list; \
                 drop one"
            ),
            (None, false) => panic!(
                "{id} has no price and is not on NO_PRICE_MODELS; give it a \
                 price and a source, or list it as unpriced on purpose"
            ),
        }
    }
}

/// The no-price list names built-in models and nothing else: an id left
/// behind after a model is dropped would hide the next gap.
#[test]
fn the_no_price_list_names_only_builtin_models() {
    let config = default_registry_config();
    for id in NO_PRICE_MODELS {
        assert!(
            config.models.iter().any(|model| model.id == *id),
            "{id} is on the no-price list but is not a built-in model"
        );
    }
}

/// The built-in table ships empty-but-typed: no descriptor carries a price and
/// no id is guessed at, but the machinery is reachable — a user who knows a
/// price writes one into settings and the merged descriptor carries it.
#[test]
fn a_price_in_the_settings_reaches_the_descriptor() {
    let builtin = default_registry_config();
    let sonnet = builtin
        .models
        .iter()
        .find(|model| model.id.as_str() == "anthropic/claude-sonnet-4-5")
        .expect("the built-in catalog names sonnet");
    assert_eq!(
        sonnet.price, None,
        "nothing in this tree states sonnet's output price, so it ships unpriced"
    );

    let value = serde_json::json!({
        "providers": [{
            "id": "anthropic",
            "api": "anthropic-messages",
            "base_url": "https://api.anthropic.com",
            "credential_env": "ANTHROPIC_API_KEY",
            "credential_required": true
        }],
        "models": [{
            "id": "anthropic/claude-sonnet-4-5",
            "provider": "anthropic",
            "wire_model": "claude-sonnet-4-5",
            "price": {
                "input": 3_000_000,
                "output": 15_000_000,
                "cached_input": 300_000
            }
        }]
    });
    let overlay = ProviderRegistryConfig::from_settings_value(&value).expect("settings parse");
    let merged = merge_registry_config(default_registry_config(), overlay);
    let priced = merged
        .models
        .iter()
        .find(|model| model.id.as_str() == "anthropic/claude-sonnet-4-5")
        .expect("sonnet survives the merge");
    let price = priced.price.expect("the user's price is on the descriptor");
    assert_eq!(price.input, 3_000_000);
    assert_eq!(price.output, 15_000_000);
    assert_eq!(price.cached_input, Some(300_000));
    // $3/MTok in, $15/MTok out, $0.30/MTok cached read: one turn's bill.
    assert_eq!(price.cost_micro_usd(1_000, 800, 250), 4_590);
}
