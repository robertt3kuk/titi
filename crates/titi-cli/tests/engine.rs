use titi_cli::engine::{
    default_registry_config, merge_registry_config, parse_approval, prefer_available_models,
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
/// that does must name the variable it reads: the message for a missing key
/// is the only instruction the user gets.
#[test]
fn a_provider_asks_for_a_key_exactly_when_it_has_one_to_ask_for() {
    for provider in default_registry_config().providers {
        assert_eq!(
            provider.credential_required,
            provider.credential_env.is_some(),
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
