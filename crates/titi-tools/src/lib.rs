//! Agent tool registry: schemas, approval tiers, and invocation.
//!
//! Spec: `docs/research/tools-core/README.md` and `docs/research/reference-product-port/README.md` (E1).

pub mod cache;
pub mod fs;
pub mod sensitive;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_providers::ToolSpec;

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use cache::{READ_CACHE_CAPACITY, ReadCache};
pub use fs::{
    BashTool, EditFileTool, GlobTool, GrepTool, ReadFileTool, WriteFileTool, workspace_tools,
    workspace_tools_with_cache, workspace_tools_with_policy,
};
pub use sensitive::SensitivePolicy;

/// How dangerous a tool is. Unknown tools are treated as [`ApprovalTier::Exec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTier {
    Read,
    Write,
    Exec,
}

/// When the engine auto-approves versus waiting for `ApproveTool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Prompt for every tool.
    AlwaysAsk,
    /// Auto-approve read; ask for write/exec.
    #[default]
    Write,
    /// Auto-approve everything.
    Yolo,
}

impl ApprovalMode {
    pub fn auto_approves(self, tier: ApprovalTier) -> bool {
        match self {
            ApprovalMode::Yolo => true,
            ApprovalMode::Write => matches!(tier, ApprovalTier::Read),
            ApprovalMode::AlwaysAsk => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub spec: ToolSpec,
    pub approval: ApprovalTier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub output: SmolStr,
    pub is_error: bool,
}

#[async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    fn definition(&self) -> ToolDefinition;
    async fn invoke(&self, args: serde_json::Value) -> ToolResult;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<SmolStr, Arc<dyn ToolHandler>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, handler: Arc<dyn ToolHandler>) {
        let name = handler.definition().spec.name;
        self.tools.insert(name, handler);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|handler| handler.definition().spec)
            .collect()
    }

    pub fn approval_tier(&self, name: &str) -> ApprovalTier {
        self.tools
            .get(name)
            .map(|handler| handler.definition().approval)
            .unwrap_or(ApprovalTier::Exec)
    }

    /// Keeps only tools whose tier is in `tiers`. Used to hand a subagent a
    /// registry it cannot escalate out of: with nothing exec-tier registered,
    /// no call can ever wait for an approval no one will give.
    pub fn retain_tiers(&mut self, tiers: &[ApprovalTier]) {
        self.tools
            .retain(|_, handler| tiers.contains(&handler.definition().approval));
    }

    /// Names of the registered tools, sorted.
    pub fn names(&self) -> Vec<SmolStr> {
        let mut names: Vec<SmolStr> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

/// Echoes `{"text": ...}` back. Read-tier, for tests and smoke.
pub struct EchoTool;

#[async_trait]
impl ToolHandler for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "echo".into(),
                description: "Echo the provided text".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let text = args
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        ToolResult {
            output: text.into(),
            is_error: false,
        }
    }
}

/// Exec-tier tool used to exercise approval gating.
pub struct ShellProbeTool;

#[async_trait]
impl ToolHandler for ShellProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "shell_probe".into(),
                description: "Exec-tier probe that echoes its command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }),
            },
            approval: ApprovalTier::Exec,
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let command = args
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        ToolResult {
            output: format!("ran {command}").into(),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_is_read_tier() {
        let tool = EchoTool;
        assert_eq!(tool.definition().approval, ApprovalTier::Read);
        let result = tool.invoke(serde_json::json!({"text": "hi"})).await;
        assert_eq!(result.output, "hi");
        assert!(!result.is_error);
    }

    #[test]
    fn write_mode_auto_approves_read_only() {
        assert!(ApprovalMode::Write.auto_approves(ApprovalTier::Read));
        assert!(!ApprovalMode::Write.auto_approves(ApprovalTier::Exec));
        assert!(ApprovalMode::Yolo.auto_approves(ApprovalTier::Exec));
        assert!(!ApprovalMode::AlwaysAsk.auto_approves(ApprovalTier::Read));
    }
}
