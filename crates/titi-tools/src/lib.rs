//! Agent tool registry: schemas, approval tiers, and invocation.
//!
//! Spec: `docs/research/tools-core/README.md` and `docs/research/reference-product-port/README.md` (E1).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod ansi;
pub mod cache;
pub mod fs;
pub mod git;
pub mod hashline;
pub mod intercept;
mod loose;
pub mod pipe;
pub mod pty;
pub mod sensitive;
pub mod web;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_providers::ToolSpec;

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod settings;
pub mod todo;

pub use cache::{READ_CACHE_CAPACITY, ReadCache};
pub use fs::{
    BashTool, EditFileTool, GlobTool, GrepTool, ReadFileTool, WriteFileTool, workspace_tools,
    workspace_tools_with_cache, workspace_tools_with_interrupt, workspace_tools_with_policy,
};
pub use git::{DiagnoseTool, GIT_TIMEOUT, GitCommitTool, GitError, GitTool, git_tools};
pub use pipe::{
    BACKGROUND_AFTER, BACKGROUND_ENV, Background, BackgroundCancel, BackgroundSink,
    background_after,
};
pub use pty::{Interrupt, PtyError};
pub use sensitive::SensitivePolicy;
pub use settings::SettingsTool;
pub use todo::TodoTool;
pub use web::{
    FETCH_BYTE_CAP, FETCH_TIMEOUT, FetchTool, SearchProvider, WebError, WebSearchTool, web_tools,
};

/// How dangerous a tool is. Unknown tools are treated as [`ApprovalTier::Exec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTier {
    Read,
    /// Reaching an outside host. Its own tier because a network read is not a
    /// filesystem read: the URL is a channel out of the machine, so `Write`
    /// mode asks for it even though nothing on disk changes.
    Network,
    Write,
    Exec,
}

impl ApprovalTier {
    /// The tier as a word in a sentence: `read`, `network`, `write`, `exec`.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalTier::Read => "read",
            ApprovalTier::Network => "network",
            ApprovalTier::Write => "write",
            ApprovalTier::Exec => "exec",
        }
    }
}

/// When the engine auto-approves versus waiting for `ApproveTool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Prompt for every tool.
    AlwaysAsk,
    /// Auto-approve read; ask for network/write/exec.
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
    /// What the model is told. The tool message the provider sees is this, and
    /// nothing else.
    pub output: SmolStr,
    pub is_error: bool,
    /// What a surface should show *besides* the answer — a diff of what the
    /// tool changed, today.
    ///
    /// Presentation only, and by construction: the engine puts this on the
    /// event a surface reads and keeps it out of the tool message, so it can
    /// neither spend the model's context nor answer for the tool. A tool that
    /// has nothing extra to show leaves it empty.
    pub detail: Option<SmolStr>,
}

#[async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    fn definition(&self) -> ToolDefinition;
    async fn invoke(&self, args: serde_json::Value) -> ToolResult;

    /// What this call is about to do, for a surface to show beside the tool's
    /// name: `read docs/README.md`, `bash cargo test -p titi-core`.
    ///
    /// Presentation only, and by construction: the engine puts this on the
    /// `ToolStarted` event and never sends it to the provider, so it can
    /// neither spend the model's context nor answer for the tool. It is one row
    /// on a screen, not a payload — keep it to a few words. `None`, the
    /// default, means the tool has nothing to say beyond its own name.
    fn describe(&self, args: &serde_json::Value) -> Option<String> {
        let _ = args;
        None
    }

    /// Why this call would be refused without running, decided from its
    /// arguments alone. The engine asks before it puts the call to the user,
    /// so nobody approves a call that was never going to run. `None`, the
    /// default, sends every call on to approval and [`ToolHandler::invoke`],
    /// which must still refuse the same calls when called directly.
    fn refusal(&self, args: &serde_json::Value) -> Option<String> {
        let _ = args;
        None
    }

    /// Installs the session's door for a command that outlives the turn's
    /// threshold — see [`pipe::BackgroundSink`]. Only a tool that can leave a
    /// process running has anywhere to put one; the default, for every other
    /// tool, does nothing.
    fn set_background(&self, sink: Arc<dyn pipe::BackgroundSink>) {
        let _ = sink;
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<SmolStr, Arc<dyn ToolHandler>>,
    /// Names [`ToolRegistry::retain_tiers`] took out. A call to one of them
    /// is refused as out of reach, not as a tool that does not exist.
    withheld: std::collections::BTreeSet<SmolStr>,
    /// The tiers the last [`ToolRegistry::retain_tiers`] kept, to say what is
    /// on offer instead.
    offered: Vec<ApprovalTier>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, handler: Arc<dyn ToolHandler>) {
        let name = handler.definition().spec.name;
        self.withheld.remove(&name);
        self.tools.insert(name, handler);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.tools.get(name).cloned()
    }

    /// Sorted by name. The specs go out in every request in this order, and
    /// a `HashMap` walk gives a different one in every process, which is
    /// enough on its own to miss the provider's prompt cache on every turn.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|handler| handler.definition().spec)
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
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
        self.offered = tiers.to_vec();
        let withheld = &mut self.withheld;
        self.tools.retain(|name, handler| {
            let keep = tiers.contains(&handler.definition().approval);
            if !keep {
                withheld.insert(name.clone());
            }
            keep
        });
    }

    /// Why a call to `name` cannot run, when the tool exists but
    /// [`ToolRegistry::retain_tiers`] withheld it: `write is withheld here;
    /// only read tools are offered`. `None` for a name never registered.
    pub fn withheld_reason(&self, name: &str) -> Option<String> {
        if !self.withheld.contains(name) {
            return None;
        }
        let offered: Vec<&str> = self.offered.iter().map(|tier| tier.as_str()).collect();
        Some(if offered.is_empty() {
            format!("{name} is withheld here; no tools are offered")
        } else {
            format!(
                "{name} is withheld here; only {} tools are offered",
                offered.join(" and ")
            )
        })
    }

    /// Hands every registered tool the session's door for a command that
    /// outlives the turn's threshold, so a `bash` call can be handed over to
    /// the job registry instead of dying at its deadline. The engine installs
    /// one as it starts, on the registry the surface built.
    pub fn install_background(&self, sink: Arc<dyn pipe::BackgroundSink>) {
        for handler in self.tools.values() {
            handler.set_background(Arc::clone(&sink));
        }
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
            detail: None,
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
            detail: None,
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

    struct NamedTool(&'static str);

    #[async_trait]
    impl ToolHandler for NamedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                spec: ToolSpec {
                    name: self.0.into(),
                    description: "test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                approval: ApprovalTier::Read,
            }
        }

        async fn invoke(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult {
                output: "".into(),
                is_error: false,
                detail: None,
            }
        }
    }

    /// The tool array is the first thing a provider hashes for its prompt
    /// cache. A `HashMap` walk reorders it per process, so the order has to
    /// come from the names and not from the registry's internals. Eight
    /// tools make an accidental pass vanishingly unlikely.
    #[test]
    fn specs_come_out_sorted_by_name() {
        let mut registry = ToolRegistry::new();
        for name in [
            "write", "read", "glob", "grep", "edit", "bash", "task", "hub",
        ] {
            registry.register(Arc::new(NamedTool(name)));
        }
        let specs = registry.specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "bash", "edit", "glob", "grep", "hub", "read", "task", "write"
            ]
        );
    }
}
