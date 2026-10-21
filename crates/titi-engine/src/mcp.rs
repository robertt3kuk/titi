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
use titi_mcp::{ManagerConfig, McpEvent, McpManager, ServerStatus, Snapshot};
use titi_tools::ToolRegistry;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Prefix of every tool an MCP server contributes.
const PREFIX: &str = "mcp__";

/// How long a prompt waits for servers that are still starting before the turn
/// goes ahead without them. A person types for longer than a server takes to
/// come up, but a one-shot (`--prompt`) submits at once and would otherwise
/// have its first turn call tools that do not exist yet.
pub(crate) const STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Where the host reads the servers' state from: the manager, or a double in a
/// test. One call is one instant — see [`McpManager::snapshot`].
pub(crate) trait Source: Send + Sync {
    fn snapshot(&self) -> Snapshot;
    fn set_servers(&self, servers: Vec<McpServerDef>);
    fn restart(&self, name: &str) -> bool;
    fn shutdown(&self);
}

impl Source for McpManager {
    fn snapshot(&self) -> Snapshot {
        McpManager::snapshot(self)
    }

    fn set_servers(&self, servers: Vec<McpServerDef>) {
        McpManager::set_servers(self, servers);
    }

    fn restart(&self, name: &str) -> bool {
        McpManager::restart(self, name)
    }

    fn shutdown(&self) {
        McpManager::shutdown(self);
    }
}

pub(crate) struct McpHost {
    source: Option<Box<dyn Source>>,
    /// The status the registry was last built from. What the host says about
    /// the servers (to a surface, to the hold on a first prompt) is this, never
    /// a newer read: a server that came up since is not `ready` until its
    /// tools are in the registry, and the event that announces it makes the
    /// next [`McpHost::sync`].
    synced: Vec<ServerStatus>,
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
            source: None,
            synced: Vec::new(),
            wait_armed: false,
            tx,
            rx,
        }
    }

    /// Replaces the running servers with `servers`. Must run inside the
    /// engine's runtime: the servers are started as tasks.
    pub(crate) fn set(&mut self, servers: Vec<McpServerDef>, config: ManagerConfig) {
        let source = self
            .source
            .get_or_insert_with(|| Box::new(McpManager::new(config, self.tx.clone())));
        source.set_servers(servers);
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
        self.source
            .as_ref()
            .is_some_and(|source| source.restart(name))
    }

    /// Names of the servers that, as of the last [`McpHost::sync`], have not
    /// yet come up or failed.
    pub(crate) fn starting(&self) -> Vec<String> {
        self.synced
            .iter()
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

    /// Reads the servers once, replaces the MCP tools in `tools` with those of
    /// the ready ones, and returns the status that read holds, with a line for
    /// each tool the registry refused. The status and the registry are the
    /// same instant by construction: report the first, never read the manager
    /// again for the second.
    pub(crate) fn sync(&mut self, tools: &mut ToolRegistry) -> (Vec<ServerStatus>, Vec<String>) {
        tools.unregister_prefix(PREFIX);
        let mut refused = Vec::new();
        let Some(source) = &self.source else {
            return (Vec::new(), refused);
        };
        let Snapshot { status, handlers } = source.snapshot();
        for handler in handlers {
            if !tools.register(Arc::clone(&handler)) {
                refused.push(format!(
                    "mcp: the tool {} is left out — the name is already taken",
                    handler.definition().spec.name
                ));
            }
        }
        self.synced = status.clone();
        (status, refused)
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(source) = self.source.take() {
            source.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use smol_str::SmolStr;
    use titi_mcp::{ServerState, ToolSummary};
    use titi_providers::ToolSpec;
    use titi_tools::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

    use super::*;

    struct Tool;

    #[async_trait]
    impl ToolHandler for Tool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                spec: ToolSpec {
                    name: SmolStr::new("mcp__fixture__echo"),
                    description: SmolStr::new("echo"),
                    parameters: serde_json::json!({ "type": "object" }),
                },
                approval: ApprovalTier::Exec,
            }
        }

        async fn invoke(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult {
                output: SmolStr::new("x"),
                is_error: false,
                detail: None,
            }
        }
    }

    fn status(state: ServerState) -> ServerStatus {
        ServerStatus {
            name: "fixture".into(),
            state,
            reason: String::new(),
            tools: if state == ServerState::Ready {
                vec![ToolSummary {
                    name: "mcp__fixture__echo".into(),
                    description: "echo".to_owned(),
                }]
            } else {
                Vec::new()
            },
            command: "srv".to_owned(),
            host: String::new(),
            source: "mcp.json".to_owned(),
        }
    }

    /// A manager whose server comes up between two reads: the first read of it
    /// says `starting`, every later one `ready` with its tool. Whoever reads
    /// twice gets two instants.
    struct ComesUpBetweenReads {
        reads: AtomicUsize,
        shutdowns: Mutex<usize>,
    }

    impl ComesUpBetweenReads {
        fn host() -> McpHost {
            let mut host = McpHost::new();
            host.source = Some(Box::new(Self {
                reads: AtomicUsize::new(0),
                shutdowns: Mutex::new(0),
            }));
            host
        }
    }

    impl Source for ComesUpBetweenReads {
        fn snapshot(&self) -> Snapshot {
            if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                Snapshot {
                    status: vec![status(ServerState::Starting)],
                    handlers: Vec::new(),
                }
            } else {
                Snapshot {
                    status: vec![status(ServerState::Ready)],
                    handlers: vec![Arc::new(Tool)],
                }
            }
        }

        fn set_servers(&self, _servers: Vec<McpServerDef>) {}

        fn restart(&self, _name: &str) -> bool {
            false
        }

        fn shutdown(&self) {
            *self.shutdowns.lock().unwrap() += 1;
        }
    }

    #[test]
    fn what_is_reported_ready_is_in_the_registry_even_when_the_server_comes_up_mid_sync() {
        let mut host = ComesUpBetweenReads::host();
        let mut tools = ToolRegistry::new();
        let (reported, _) = host.sync(&mut tools);
        for server in &reported {
            if server.state == ServerState::Ready {
                for tool in &server.tools {
                    assert!(
                        tools.get(&tool.name).is_some(),
                        "{} was reported ready and is not in the registry",
                        tool.name
                    );
                }
            }
        }
        // And the other way: a tool in the registry belongs to a server the
        // report calls ready.
        assert_eq!(
            tools.get("mcp__fixture__echo").is_some(),
            reported[0].state == ServerState::Ready
        );
    }

    #[test]
    fn a_first_prompt_still_waits_for_a_server_that_came_up_after_the_last_sync() {
        let mut host = ComesUpBetweenReads::host();
        let mut tools = ToolRegistry::new();
        host.sync(&mut tools);
        host.wait_armed = true;
        // The manager would now say ready; the registry does not hold the tool
        // yet, so the prompt is held until the event that syncs it.
        assert!(host.take_wait(), "the hold decides from the synced status");
        assert_eq!(host.starting(), ["fixture"]);
        let (reported, _) = host.sync(&mut tools);
        assert_eq!(reported[0].state, ServerState::Ready);
        assert!(tools.get("mcp__fixture__echo").is_some());
        assert!(host.starting().is_empty(), "the hold can end");
    }
}
