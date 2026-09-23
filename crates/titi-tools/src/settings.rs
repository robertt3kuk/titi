use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use titi_providers::ToolSpec;

pub trait SettingsBackend: Send + Sync {
    fn resolve_source(&self, key: &str) -> Result<Option<(String, Value)>, String>;
    fn set(&self, key: &str, value: Value, scope: &str) -> Result<(), String>;
}

pub struct SettingsTool {
    backend: Arc<dyn SettingsBackend>,
}

impl SettingsTool {
    pub fn new(backend: Arc<dyn SettingsBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl ToolHandler for SettingsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "settings".into(),
                description: "Read or write configuration settings. Pass a key to read its current value. Pass key and value to write. Scope can be 'global' or 'project' (defaults to the layer where it is already set, or 'project' if new). NEVER change 'approval_mode', 'privacy.*' (including 'maskIps'), or provider keys.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "Dotted config key." },
                        "value": { "description": "Value to set (boolean, string, number). Omit to read." },
                        "scope": { "type": "string", "enum": ["global", "project"], "description": "Layer to write to." }
                    },
                    "required": ["key"]
                }),
            },
            approval: ApprovalTier::Write,
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => {
                return ToolResult {
                    output: "missing string key 'key'".into(),
                    is_error: true,
                };
            }
        };

        if key == "approval_mode" || key.starts_with("privacy.") || key.ends_with(".key") {
            return ToolResult {
                output: "changing this setting via tool is blocked for security reasons".into(),
                is_error: true,
            };
        }

        let has_value = args.get("value").is_some();
        if !has_value {
            match self.backend.resolve_source(key) {
                Ok(Some((source, val))) => {
                    return ToolResult {
                        output: format!("value: {}\nsource: {}", val, source).into(),
                        is_error: false,
                    };
                }
                Ok(None) => {
                    return ToolResult {
                        output: "not set".into(),
                        is_error: false,
                    };
                }
                Err(e) => {
                    return ToolResult {
                        output: format!("failed to read settings: {}", e).into(),
                        is_error: true,
                    };
                }
            }
        }

        let value = args.get("value").unwrap().clone();
        let mut scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if scope.is_none() {
            if let Ok(Some((source, _))) = self.backend.resolve_source(key) {
                if source == "agent" || source == "global" {
                    scope = Some("global".to_string());
                } else if source == "project" {
                    scope = Some("project".to_string());
                }
            }
        }

        let scope = scope.unwrap_or_else(|| "project".to_string());

        match self.backend.set(key, value, &scope) {
            Ok(_) => {
                if key.starts_with("display.") || key == "agent_model" {
                    ToolResult {
                        output: "applied after restart".into(),
                        is_error: false,
                    }
                } else {
                    ToolResult {
                        output: "applied".into(),
                        is_error: false,
                    }
                }
            }
            Err(e) => ToolResult {
                output: e.into(),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyBackend;
    impl SettingsBackend for DummyBackend {
        fn resolve_source(&self, _key: &str) -> Result<Option<(String, Value)>, String> {
            Ok(None)
        }
        fn set(&self, _key: &str, _value: Value, _scope: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn rejects_security_settings() {
        let tool = SettingsTool::new(Arc::new(DummyBackend));

        let res = tool
            .invoke(serde_json::json!({
                "key": "approval_mode",
                "value": "auto"
            }))
            .await;
        assert!(res.is_error);

        let res = tool
            .invoke(serde_json::json!({
                "key": "privacy.maskIps",
                "value": false
            }))
            .await;
        assert!(res.is_error);

        let res = tool
            .invoke(serde_json::json!({
                "key": "anthropic.key",
                "value": "sk-123"
            }))
            .await;
        assert!(res.is_error);
    }
}
