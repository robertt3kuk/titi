//! A server's tool as the registry holds it: `mcp__<server>__<tool>`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use smol_str::SmolStr;
use titi_providers::ToolSpec;
use titi_tools::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

use crate::manager::Slot;

/// Longest tool name providers take.
pub const MAX_TOOL_NAME: usize = 64;

/// The name the model sees: Claude's `mcp__<server>__<tool>`, so a hook
/// matcher written for it (`mcp__.*__write.*`) works unchanged. A tool name
/// may hold characters providers reject (`.`, spaces), which become `_`; a
/// name past [`MAX_TOOL_NAME`] is cut and ends in a short hash of the
/// original, so two long names stay two names.
pub fn tool_name(server: &str, tool: &str) -> String {
    let safe: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("mcp__{server}__{safe}");
    if name.chars().count() <= MAX_TOOL_NAME {
        return name;
    }
    let hash = titi_config::trust::sha256_hex(format!("{server}\0{tool}").as_bytes());
    let kept: String = name.chars().take(MAX_TOOL_NAME - 9).collect();
    format!("{kept}_{}", &hash[..8])
}

/// One tool of one server. It holds the server's slot, not a connection, so a
/// restart does not leave it pointing at a process that is gone.
pub(crate) struct McpTool {
    pub(crate) slot: Arc<Slot>,
    pub(crate) remote_name: String,
    pub(crate) spec: ToolSpec,
    pub(crate) approval: ApprovalTier,
}

#[async_trait]
impl ToolHandler for McpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: self.spec.clone(),
            approval: self.approval,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let fail = |text: String| ToolResult {
            output: text.into(),
            is_error: true,
            detail: None,
        };
        if let Some(why) = self.refusal(&args) {
            return fail(why);
        }
        let arguments = if args.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            args
        };
        let client = match self.slot.client() {
            Ok(client) => client,
            Err(why) => return fail(why),
        };
        match client.call_tool(&self.remote_name, arguments).await {
            Ok(output) => ToolResult {
                output: SmolStr::new(&output.text),
                is_error: output.is_error,
                detail: None,
            },
            Err(error) => fail(format!("mcp server {}: {error}", self.slot.name)),
        }
    }

    /// `fixture echo {"text":"hi"}`: the server and tool, then the arguments
    /// cut to a row.
    fn describe(&self, args: &Value) -> Option<String> {
        let mut line = format!("{} {}", self.slot.name, self.remote_name);
        if !args.is_null() && args != &Value::Object(serde_json::Map::new()) {
            line.push(' ');
            line.push_str(&args.to_string());
        }
        Some(line.chars().take(80).collect())
    }

    /// Arguments that cannot be what the tool takes, decided from its own
    /// schema: not an object, or missing a property it lists as required. The
    /// server would refuse them too, after a round trip and an approval.
    fn refusal(&self, args: &Value) -> Option<String> {
        let object = match args {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Some("the arguments must be a JSON object".to_owned()),
        };
        let missing: Vec<&str> = self
            .spec
            .parameters
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|key| object.is_none_or(|object| !object.contains_key(*key)))
            .collect();
        (!missing.is_empty())
            .then(|| format!("missing required argument(s): {}", missing.join(", ")))
    }
}
