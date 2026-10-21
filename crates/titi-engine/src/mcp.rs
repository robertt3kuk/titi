//! The session's MCP servers, as the engine holds them.
//!
//! The surface resolves which servers may run (user file always, a
//! repository's only with the user's consent) and hands the engine the list
//! (`SetMcp`); the engine reads no MCP file and no trust store. The servers
//! start in the background, so the first frame and the first turn never wait
//! for one. When a server comes up, changes its tool list or dies, the engine
//! swaps that server's `mcp__<server>__<tool>` tools in or out of its
//! registry. A running turn keeps the registry it began with, so the tool
//! array a model sees does not move under it; the next turn gets the new one.

use std::sync::Arc;

use titi_config::mcp::McpServerDef;
use titi_mcp::{ManagerConfig, McpEvent, McpManager, ServerStatus};
use titi_tools::ToolRegistry;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Prefix of every tool an MCP server contributes.
const PREFIX: &str = "mcp__";

/// How long a prompt waits for servers that are still starting before the turn
/// goes ahead without them. A person types for longer than a server takes to
/// come up, but a one-shot (`--prompt`) submits at once and would otherwise
/// have its first turn call tools that do not exist yet.
pub(crate) const STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) struct McpHost {
    manager: Option<McpManager>,
    /// Servers were handed over and no prompt has yet had its moment to wait
    /// for them.
    wait_armed: bool,
    tx: UnboundedSender<McpEvent>,
    /// What the manager reports. A field, not a method, so the command loop
    /// can wait on it beside the command channel.
    pub(crate) rx: UnboundedReceiver<McpEvent>,
}

/// What a batch of manager events asks of the engine.
#[derive(Debug, Default)]
pub(crate) struct Digest {
    pub(crate) notices: Vec<String>,
    pub(crate) changed: bool,
}

impl McpHost {
    pub(crate) fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            manager: None,
            wait_armed: false,
            tx,
            rx,
        }
    }

    /// Replaces the running servers with `servers`. Must run inside the
    /// engine's runtime: the servers are started as tasks.
    pub(crate) fn set(&mut self, servers: Vec<McpServerDef>, config: ManagerConfig) {
        let manager = self
            .manager
            .get_or_insert_with(|| McpManager::new(config, self.tx.clone()));
        manager.set_servers(servers);
        self.wait_armed = true;
    }

    /// Whether the next prompt should wait for servers that are starting.
    /// Only the first prompt after `set` does, and only when one is: later
    /// prompts never stall on a server that is slow to come up.
    pub(crate) fn take_wait(&mut self) -> bool {
        std::mem::take(&mut self.wait_armed) && !self.starting().is_empty()
    }

    /// `false` when no server has that name.
    pub(crate) fn restart(&self, name: &str) -> bool {
        self.manager
            .as_ref()
            .is_some_and(|manager| manager.restart(name))
    }

    pub(crate) fn status(&self) -> Vec<ServerStatus> {
        self.manager
            .as_ref()
            .map(McpManager::status)
            .unwrap_or_default()
    }

    /// Names of the servers that have not yet come up or failed.
    pub(crate) fn starting(&self) -> Vec<String> {
        self.status()
            .into_iter()
            .filter(|server| server.state == titi_mcp::ServerState::Starting)
            .map(|server| server.name.to_string())
            .collect()
    }

    /// Takes `first` and everything queued behind it as one change, so a
    /// burst of state transitions is one registry swap and one status.
    pub(crate) fn digest(&mut self, first: McpEvent) -> Digest {
        let mut digest = Digest::default();
        let mut next = Some(first);
        while let Some(event) = next {
            match event {
                McpEvent::Changed => digest.changed = true,
                McpEvent::Notice(text) => digest.notices.push(text),
            }
            next = self.rx.try_recv().ok();
        }
        digest
    }

    /// Replaces the MCP tools in `tools` with those of the ready servers, and
    /// returns a line for each one the registry refused.
    pub(crate) fn install(&self, tools: &mut ToolRegistry) -> Vec<String> {
        tools.unregister_prefix(PREFIX);
        let mut refused = Vec::new();
        if let Some(manager) = &self.manager {
            for handler in manager.handlers() {
                if !tools.register(Arc::clone(&handler)) {
                    refused.push(format!(
                        "mcp: the tool {} is left out — the name is already taken",
                        handler.definition().spec.name
                    ));
                }
            }
        }
        refused
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(manager) = self.manager.take() {
            manager.shutdown();
        }
    }
}
