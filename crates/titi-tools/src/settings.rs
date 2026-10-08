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

/// The segments of a dotted settings key, or `None` when it names nothing: a
/// key with an empty segment (`display.`, `.theme`, `a..b`) is not a path.
/// The scope guard and the approval description judge the same segments.
fn key_segments(key: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = key.split('.').collect();
    segments
        .iter()
        .all(|segment| !segment.is_empty())
        .then_some(segments)
}

#[async_trait]
impl ToolHandler for SettingsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "settings".into(),
                description: "Read or write configuration settings. Pass a key to read its current value. Pass key and value to write. Scope can be 'global' or 'project' (defaults to the layer where it is already set, or 'project' if new). Approval, 'privacy', 'providers', 'models' and credentials (keys, tokens, secrets, passwords) are refused.".into(),
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

    /// The dotted path this call reads or writes — never the value. A write
    /// of a value is approved blind otherwise, and the path is exactly what
    /// the scope guard below already judges.
    fn describe(&self, args: &serde_json::Value) -> Option<String> {
        let key = args.get("key").and_then(|v| v.as_str())?;
        key_segments(key)?;
        let verb = if args.get("value").is_some() {
            "write"
        } else {
            "read"
        };
        Some(format!(
            "settings {verb} {}",
            crate::fs::describe_line(key, crate::fs::DESCRIBE_MAX)
        ))
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => {
                return ToolResult {
                    output: "missing string key 'key'".into(),
                    is_error: true,
                    detail: None,
                };
            }
        };

        let Some(segments) = key_segments(key) else {
            return ToolResult {
                output: format!("'{key}' is not a dotted settings key").into(),
                is_error: true,
                detail: None,
            };
        };
        if is_protected(&segments) || is_credential(&segments) {
            return ToolResult {
                output: "changing this setting via tool is blocked for security reasons".into(),
                is_error: true,
                detail: None,
            };
        }

        let Some(value) = args.get("value").cloned() else {
            return match self.backend.resolve_source(key) {
                Ok(Some((source, val))) => ToolResult {
                    output: format!("value: {}\nsource: {}", val, source).into(),
                    is_error: false,
                    detail: None,
                },
                Ok(None) => ToolResult {
                    output: "not set".into(),
                    is_error: false,
                    detail: None,
                },
                Err(e) => ToolResult {
                    output: format!("failed to read settings: {}", e).into(),
                    is_error: true,
                    detail: None,
                },
            };
        };

        let mut scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(scope) = scope.as_deref()
            && scope != "global"
            && scope != "project"
        {
            return ToolResult {
                output: format!("scope must be 'global' or 'project', not '{scope}'").into(),
                is_error: true,
                detail: None,
            };
        }

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
                        detail: None,
                    }
                } else {
                    ToolResult {
                        output: "applied".into(),
                        is_error: false,
                        detail: None,
                    }
                }
            }
            Err(e) => ToolResult {
                output: e.into(),
                is_error: true,
                detail: None,
            },
        }
    }
}

/// Subtrees that decide what the agent may do or where a key is sent:
/// privacy, the provider catalog, and approval. Loosening any of them is a
/// human decision, never the model's.
const PROTECTED: &[&[&str]] = &[
    &["privacy"],
    &["providers"],
    &["models"],
    &["approval_mode"],
    &["tools", "approval"],
    &["tools", "approvalMode"],
];

/// A key is protected when it lies inside a protected subtree or contains
/// one: writing `tools` replaces `tools.approval` as surely as writing
/// `tools.approval.bash` changes it. Settings keys are case-sensitive, but a
/// guard that a capital letter defeats is no guard.
fn is_protected(segments: &[&str]) -> bool {
    PROTECTED.iter().any(|path| {
        path.iter()
            .zip(segments)
            .all(|(protected, segment)| protected.eq_ignore_ascii_case(segment))
    })
}

/// A leaf that names a credential, under any of the spellings a config gives
/// it (`key`, `apiKey`, `api_key`, `token`, `secret`, `password`). Reading
/// one hands the model a secret, so reads are refused as well as writes.
fn is_credential(segments: &[&str]) -> bool {
    let Some(leaf) = segments.last() else {
        return false;
    };
    let leaf: String = leaf
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    leaf == "key"
        || ["apikey", "token", "secret", "password"]
            .iter()
            .any(|suffix| leaf.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct DummyBackend;
    impl SettingsBackend for DummyBackend {
        fn resolve_source(&self, _key: &str) -> Result<Option<(String, Value)>, String> {
            Ok(None)
        }
        fn set(&self, _key: &str, _value: Value, _scope: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Remembers every read and write, so a test can tell a refused call
    /// from one that reached the config.
    #[derive(Default)]
    struct Recording {
        reads: Mutex<Vec<String>>,
        writes: Mutex<Vec<(String, Value, String)>>,
    }
    impl SettingsBackend for Recording {
        fn resolve_source(&self, key: &str) -> Result<Option<(String, Value)>, String> {
            self.reads.lock().unwrap().push(key.to_owned());
            Ok(None)
        }
        fn set(&self, key: &str, value: Value, scope: &str) -> Result<(), String> {
            self.writes
                .lock()
                .unwrap()
                .push((key.to_owned(), value, scope.to_owned()));
            Ok(())
        }
    }

    async fn call(backend: &Arc<Recording>, args: Value) -> ToolResult {
        let tool = SettingsTool::new(Arc::clone(backend) as Arc<dyn SettingsBackend>);
        tool.invoke(args).await
    }

    /// A write to a protected subtree is refused whatever the spelling: the
    /// subtree's root replaces every key under it, a parent of `tools.approval`
    /// replaces approval, and nothing about a key's case changes what it is.
    #[tokio::test]
    async fn a_protected_subtree_is_refused_from_its_root_and_its_parent() {
        let backend = Arc::new(Recording::default());
        for (key, value) in [
            (
                "privacy",
                serde_json::json!({ "maskIps": false, "allow": [".env"] }),
            ),
            ("privacy.allow", serde_json::json!([".env"])),
            ("Privacy.maskIps", serde_json::json!(false)),
            (
                "providers",
                serde_json::json!([{ "id": "openai", "base_url": "https://example.invalid" }]),
            ),
            (
                "providers.0.base_url",
                serde_json::json!("https://example.invalid"),
            ),
            ("models", serde_json::json!([])),
            (
                "tools",
                serde_json::json!({ "approval": { "bash": "allow" } }),
            ),
            ("tools.approval.bash", serde_json::json!("allow")),
            ("tools.approvalMode", serde_json::json!("yolo")),
            ("approval_mode", serde_json::json!("yolo")),
        ] {
            let res = call(&backend, serde_json::json!({ "key": key, "value": value })).await;
            assert!(res.is_error, "{key} was accepted");
        }
        assert!(backend.writes.lock().unwrap().is_empty());
    }

    /// Reading a credential is as refused as writing one, under any of the
    /// names a config gives it.
    #[tokio::test]
    async fn a_credential_is_neither_read_nor_written() {
        let backend = Arc::new(Recording::default());
        for key in [
            "anthropic.key",
            "openrouter.apiKey",
            "custom.api_key",
            "github.token",
            "smtp.password",
            "hook.secret",
        ] {
            let read = call(&backend, serde_json::json!({ "key": key })).await;
            assert!(read.is_error, "{key} was read");
            let write = call(
                &backend,
                serde_json::json!({ "key": key, "value": "sk-test" }),
            )
            .await;
            assert!(write.is_error, "{key} was written");
        }
        assert!(backend.reads.lock().unwrap().is_empty());
        assert!(backend.writes.lock().unwrap().is_empty());
    }

    /// A key with an empty segment names no setting, and a scope outside the
    /// two the schema offers is not quietly read as `project`.
    #[tokio::test]
    async fn a_malformed_key_or_scope_is_refused() {
        let backend = Arc::new(Recording::default());
        for key in ["", ".", "display.", ".display", "display..theme"] {
            let res = call(&backend, serde_json::json!({ "key": key, "value": 1 })).await;
            assert!(res.is_error, "{key:?} was accepted");
        }
        let res = call(
            &backend,
            serde_json::json!({ "key": "display.theme", "value": "dark", "scope": "system" }),
        )
        .await;
        assert!(res.is_error);
        assert!(backend.writes.lock().unwrap().is_empty());
    }

    /// The guard is narrow: an ordinary key reads and writes, `null` is a
    /// value and not a read, and a key that only shares a prefix with a
    /// protected one is not protected.
    #[tokio::test]
    async fn ordinary_settings_still_read_and_write() {
        let backend = Arc::new(Recording::default());
        let read = call(&backend, serde_json::json!({ "key": "display.theme" })).await;
        assert!(!read.is_error, "{:?}", read.output);
        assert_eq!(read.output.as_str(), "not set");

        for (key, value) in [
            ("display.theme", serde_json::json!("dark")),
            ("compaction.thresholdPercent", serde_json::json!(70)),
            ("privacyNotes", serde_json::json!("ok")),
            ("toolsets.default", serde_json::json!("small")),
            ("keyboard.layout", serde_json::json!("dvorak")),
            ("display.badge", Value::Null),
        ] {
            let res = call(
                &backend,
                serde_json::json!({ "key": key, "value": value, "scope": "global" }),
            )
            .await;
            assert!(!res.is_error, "{key}: {:?}", res.output);
        }
        let writes = backend.writes.lock().unwrap();
        assert_eq!(writes.len(), 6);
        assert_eq!(writes[0].0, "display.theme");
        assert_eq!(writes[0].2, "global");
        assert_eq!(writes[5].1, Value::Null);
    }

    /// The approval line names the path and the verb, and never the value:
    /// the key is the same one the scope guard already judges, and it is the
    /// whole of what the person needs to see.
    #[test]
    fn the_description_names_the_path_it_reads_or_writes() {
        let tool = SettingsTool::new(Arc::new(DummyBackend));
        assert_eq!(
            tool.describe(&serde_json::json!({ "key": "display.theme" })),
            Some("settings read display.theme".to_owned())
        );
        assert_eq!(
            tool.describe(&serde_json::json!({ "key": "display.theme", "value": "dark" })),
            Some("settings write display.theme".to_owned())
        );

        let detail = tool
            .describe(&serde_json::json!({
                "key": "compaction.thresholdPercent",
                "value": "sk-live-abcdefgh"
            }))
            .expect("a key describes itself");
        assert_eq!(detail, "settings write compaction.thresholdPercent");
        assert!(
            !detail.contains("sk-live-abcdefgh"),
            "the value never rides along: {detail}"
        );

        assert_eq!(tool.describe(&serde_json::json!({})), None);
        assert_eq!(
            tool.describe(&serde_json::json!({ "key": "display." })),
            None,
            "a malformed path describes nothing"
        );
        assert_eq!(
            tool.describe(&serde_json::json!({ "key": ".display" })),
            None
        );

        let long = "display.".to_owned() + &"x".repeat(crate::fs::DESCRIBE_MAX + 25);
        let detail = tool
            .describe(&serde_json::json!({ "key": long }))
            .expect("a key describes itself");
        assert!(detail.ends_with('…'), "the line is bounded: {detail}");
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
