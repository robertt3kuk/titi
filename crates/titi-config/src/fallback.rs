//! Per-role fallback chains.
//!
//! ```yaml
//! fallback:
//!   cooldownSecs: 60
//!   chains:
//!     default: [anthropic/claude-opus-4, openai/gpt-5, google/gemini-2.5-pro]
//!     agent: [openai/gpt-5-mini]
//! ```
//!
//! A chain is an ordered list of model ids for one role: the first id is the
//! primary, the rest are tried in order when a turn fails with a retryable
//! error. `cooldownSecs` is how long a model that just failed is skipped for.
//!
//! An absent `fallback` key, an absent role, or an empty list all mean "no
//! chain": the caller keeps its one-shot fallback behaviour.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

/// Cooldown used when `fallback.cooldownSecs` is not set.
pub const DEFAULT_COOLDOWN_SECS: u64 = 60;

/// Why a `fallback` block did not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FallbackConfigError {
    #[error("'fallback' must be a mapping")]
    NotAMapping,
    #[error("'fallback.chains' must be a mapping of role to model list")]
    ChainsNotAMapping,
    #[error("'fallback.cooldownSecs' must be a whole number of seconds above zero")]
    InvalidCooldown,
    #[error("'fallback.chains.{role}' must be a list of model ids")]
    ChainNotAList { role: String },
    #[error("'fallback.chains.{role}[{index}]' must be a non-empty model id")]
    InvalidModelId { role: String, index: usize },
    #[error("'fallback.chains.{role}' lists '{model}' twice")]
    DuplicateModel { role: String, model: String },
}

/// Parsed `fallback` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackConfig {
    /// How long a model that failed with a retryable error is skipped for.
    pub cooldown: Duration,
    /// Role → ordered model ids. A role with no entry has no chain.
    pub chains: BTreeMap<String, Vec<String>>,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            cooldown: Duration::from_secs(DEFAULT_COOLDOWN_SECS),
            chains: BTreeMap::new(),
        }
    }
}

impl FallbackConfig {
    /// Ordered model ids for `role`; empty when the role has no chain.
    pub fn chain(&self, role: &str) -> &[String] {
        self.chains.get(role).map_or(&[], Vec::as_slice)
    }

    /// Whether any role configures a chain worth walking.
    pub fn is_empty(&self) -> bool {
        self.chains.values().all(|models| models.len() < 2)
    }
}

/// Parse an optional `fallback` value. `None` is the default config.
pub fn parse(value: Option<&Value>) -> Result<FallbackConfig, FallbackConfigError> {
    let Some(value) = value else {
        return Ok(FallbackConfig::default());
    };
    if value.is_null() {
        return Ok(FallbackConfig::default());
    }
    let Some(object) = value.as_object() else {
        return Err(FallbackConfigError::NotAMapping);
    };

    let cooldown = match object.get("cooldownSecs") {
        None | Some(Value::Null) => Duration::from_secs(DEFAULT_COOLDOWN_SECS),
        Some(raw) => match raw.as_u64() {
            Some(secs) if secs > 0 => Duration::from_secs(secs),
            _ => return Err(FallbackConfigError::InvalidCooldown),
        },
    };

    let mut chains = BTreeMap::new();
    match object.get("chains") {
        None | Some(Value::Null) => {}
        Some(Value::Object(roles)) => {
            for (role, raw) in roles {
                chains.insert(role.clone(), parse_chain(role, raw)?);
            }
        }
        Some(_) => return Err(FallbackConfigError::ChainsNotAMapping),
    }

    Ok(FallbackConfig { cooldown, chains })
}

fn parse_chain(role: &str, raw: &Value) -> Result<Vec<String>, FallbackConfigError> {
    let Some(list) = raw.as_array() else {
        return Err(FallbackConfigError::ChainNotAList {
            role: role.to_owned(),
        });
    };
    let mut models: Vec<String> = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let model = match item.as_str() {
            Some(text) if !text.trim().is_empty() => text.trim().to_owned(),
            _ => {
                return Err(FallbackConfigError::InvalidModelId {
                    role: role.to_owned(),
                    index,
                });
            }
        };
        if models.contains(&model) {
            return Err(FallbackConfigError::DuplicateModel {
                role: role.to_owned(),
                model,
            });
        }
        models.push(model);
    }
    Ok(models)
}

/// Read `fallback` from layered settings. A missing key is the default.
pub fn resolve_fallback(
    settings: &crate::settings::Settings,
) -> Result<FallbackConfig, FallbackConfigError> {
    parse(settings.get("fallback").as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_block_is_the_default_one_shot_config() {
        let config = parse(None).unwrap();
        assert_eq!(config.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_SECS));
        assert!(config.chain("default").is_empty());
        assert!(config.is_empty());
    }

    #[test]
    fn chains_keep_their_configured_order() {
        let config = parse(Some(&json!({
            "cooldownSecs": 30,
            "chains": {
                "default": ["a/one", "b/two", "c/three"],
                "agent": ["b/two"]
            }
        })))
        .unwrap();
        assert_eq!(config.cooldown, Duration::from_secs(30));
        assert_eq!(config.chain("default"), ["a/one", "b/two", "c/three"]);
        assert_eq!(config.chain("agent"), ["b/two"]);
        assert!(config.chain("reviewer").is_empty());
        assert!(!config.is_empty());
    }

    #[test]
    fn a_zero_cooldown_is_rejected() {
        assert_eq!(
            parse(Some(&json!({ "cooldownSecs": 0 }))).unwrap_err(),
            FallbackConfigError::InvalidCooldown
        );
        assert_eq!(
            parse(Some(&json!({ "cooldownSecs": "60" }))).unwrap_err(),
            FallbackConfigError::InvalidCooldown
        );
    }

    #[test]
    fn a_malformed_chain_is_rejected() {
        assert_eq!(
            parse(Some(&json!({ "chains": { "default": "a/one" } }))).unwrap_err(),
            FallbackConfigError::ChainNotAList {
                role: "default".into()
            }
        );
        assert_eq!(
            parse(Some(&json!({ "chains": { "default": ["a/one", ""] } }))).unwrap_err(),
            FallbackConfigError::InvalidModelId {
                role: "default".into(),
                index: 1
            }
        );
        assert_eq!(
            parse(Some(
                &json!({ "chains": { "default": ["a/one", "a/one"] } })
            ))
            .unwrap_err(),
            FallbackConfigError::DuplicateModel {
                role: "default".into(),
                model: "a/one".into()
            }
        );
        assert_eq!(
            parse(Some(&json!({ "chains": ["a/one"] }))).unwrap_err(),
            FallbackConfigError::ChainsNotAMapping
        );
        assert_eq!(
            parse(Some(&json!(["a/one"]))).unwrap_err(),
            FallbackConfigError::NotAMapping
        );
    }

    #[test]
    fn settings_without_the_key_stay_one_shot() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let settings = crate::settings::Settings::load(&agent, tmp.path(), &[]).unwrap();
        let config = resolve_fallback(&settings).unwrap();
        assert!(config.is_empty());
        assert_eq!(config.cooldown, Duration::from_secs(DEFAULT_COOLDOWN_SECS));
    }

    #[test]
    fn settings_supply_the_chain_for_a_role() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("config.yml"),
            "fallback:\n  cooldownSecs: 120\n  chains:\n    default:\n      - a/one\n      - b/two\n",
        )
        .unwrap();
        let settings = crate::settings::Settings::load(&agent, tmp.path(), &[]).unwrap();
        let config = resolve_fallback(&settings).unwrap();
        assert_eq!(config.cooldown, Duration::from_secs(120));
        assert_eq!(config.chain("default"), ["a/one", "b/two"]);
    }
}
