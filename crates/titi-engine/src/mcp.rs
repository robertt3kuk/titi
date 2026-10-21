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

pub(crate) struct McpHost {
    manager: Option<McpManager>,
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

    /// Replaces the MCP tools in `tools` with those of the ready servers.
    pub(crate) fn install(&self, tools: &mut ToolRegistry) {
        tools.unregister_prefix(PREFIX);
        if let Some(manager) = &self.manager {
            for handler in manager.handlers() {
                tools.register(Arc::clone(&handler));
            }
        }
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(manager) = self.manager.take() {
            manager.shutdown();
        }
    }
}
