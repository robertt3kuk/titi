//! The session's servers: start them in the background, keep them up, say
//! what state each is in, and hand the engine the tools of the ones that are
//! ready.
//!
//! The manager starts nothing by itself and reads no file: it is given the
//! definitions the surface resolved and the user approved
//! (`EngineCommand::SetMcp`). Each server has one supervising task. It starts
//! the process, runs the handshake, lists the tools and then waits for the
//! server to change its list or die. A server that dies is started once more;
//! one that dies again, or never comes up, is `failed` with the reason and
//! stays so until `/mcp restart`.
//!
//! Whoever owns the manager learns of a change through the [`McpEvent`]
//! channel and asks for [`McpManager::status`] and [`McpManager::handlers`];
//! nothing here touches the tool registry. The engine rebuilds its registry
//! between turns, so a tool list that changes mid-turn takes effect at the
//! next one and the model's tool array does not move under a running turn.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_config::mcp::{DEFAULT_TIMEOUT_SECS, McpApproval, McpServerDef};
use titi_providers::ToolSpec;
use titi_tools::{ApprovalTier, ToolHandler};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::client::{Client, ClientEvent, Launch, McpError, ToolInfo};
use crate::tool::{McpTool, tool_name};

/// Servers that die are started again this many times.
pub const MAX_RESTARTS: u32 = 1;

/// What the manager tells its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEvent {
    /// A server's state or tool list changed.
    Changed,
    /// Something a person should read: a server failed, a tool was left out.
    Notice(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    Starting,
    Ready,
    Failed,
    Disabled,
}

impl ServerState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerState::Starting => "starting",
            ServerState::Ready => "ready",
            ServerState::Failed => "failed",
            ServerState::Disabled => "disabled",
        }
    }
}

/// A tool a ready server offers, as the model is told its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: SmolStr,
    #[serde(default)]
    pub description: String,
}

/// One server, as a surface shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub name: SmolStr,
    pub state: ServerState,
    /// Why it is `failed`; empty otherwise.
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub tools: Vec<ToolSummary>,
    /// The command line, for a person.
    #[serde(default)]
    pub command: String,
    /// The file the definition came from.
    #[serde(default)]
    pub source: String,
}

/// Where the manager runs servers.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    pub agent_dir: PathBuf,
    pub workspace: PathBuf,
    /// The variables a server inherits, already filtered: nothing else of
    /// titi's own environment reaches it.
    pub inherit: Vec<(OsString, OsString)>,
}

impl ManagerConfig {
    /// The process's own environment, cut to the allow-list a hook gets.
    pub fn from_process(agent_dir: &Path, workspace: &Path) -> Self {
        Self {
            agent_dir: agent_dir.to_owned(),
            workspace: workspace.to_owned(),
            inherit: titi_tools::hook_environment(std::env::vars_os(), workspace),
        }
    }
}

/// The per-server log of standard error.
pub fn log_path(agent_dir: &Path, server: &str) -> PathBuf {
    agent_dir.join("mcp").join(format!("{server}.log"))
}

pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Listed {
    name: String,
    remote: String,
    description: String,
    schema: serde_json::Value,
}

struct SlotState {
    state: ServerState,
    reason: String,
    listing: Vec<Listed>,
    client: Option<Arc<Client>>,
}

/// One server's live state, shared by its supervisor, its tools and the
/// manager.
pub(crate) struct Slot {
    pub(crate) name: String,
    def: McpServerDef,
    inner: Mutex<SlotState>,
}

impl Slot {
    fn new(def: McpServerDef) -> Arc<Self> {
        let state = if def.disabled {
            ServerState::Disabled
        } else {
            ServerState::Starting
        };
        Arc::new(Self {
            name: def.name.clone(),
            def,
            inner: Mutex::new(SlotState {
                state,
                reason: String::new(),
                listing: Vec::new(),
                client: None,
            }),
        })
    }

    /// The connection a tool call goes through, or why there is none.
    pub(crate) fn client(&self) -> Result<Arc<Client>, String> {
        let slot = locked(&self.inner);
        match (&slot.client, slot.state) {
            (Some(client), ServerState::Ready) if client.is_alive() => Ok(Arc::clone(client)),
            (_, state) if slot.reason.is_empty() => {
                Err(format!("mcp server {} is {}", self.name, state.as_str()))
            }
            (_, state) => Err(format!(
                "mcp server {} is {}: {}",
                self.name,
                state.as_str(),
                slot.reason
            )),
        }
    }

    fn set(&self, state: ServerState, reason: &str) {
        let mut slot = locked(&self.inner);
        slot.state = state;
        slot.reason = reason.to_owned();
        if state != ServerState::Ready {
            slot.listing.clear();
            slot.client = None;
        }
    }

    fn status(&self) -> ServerStatus {
        let slot = locked(&self.inner);
        ServerStatus {
            name: SmolStr::new(&self.name),
            state: slot.state,
            reason: slot.reason.clone(),
            tools: slot
                .listing
                .iter()
                .map(|tool| ToolSummary {
                    name: SmolStr::new(&tool.name),
                    description: tool.description.clone(),
                })
                .collect(),
            command: self.def.command_line(),
            source: self.def.source.clone(),
        }
    }
}

struct Entry {
    slot: Arc<Slot>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Entry {
    /// Stops the supervisor and the process now, not when the aborted task
    /// is next polled: a restart must not start a second copy beside one that
    /// is still shutting down.
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        let client = locked(&self.slot.inner).client.take();
        if let Some(client) = client {
            client.shutdown();
        }
    }
}

/// The session's MCP servers.
pub struct McpManager {
    config: ManagerConfig,
    events: UnboundedSender<McpEvent>,
    servers: Mutex<BTreeMap<String, Entry>>,
}

impl McpManager {
    pub fn new(config: ManagerConfig, events: UnboundedSender<McpEvent>) -> Self {
        Self {
            config,
            events,
            servers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Makes the running set the given one: a server no longer listed is
    /// stopped, a new one or one whose definition changed is started, one that
    /// is unchanged is left running. Must be called inside a tokio runtime;
    /// returns at once, the servers come up in the background.
    pub fn set_servers(&self, defs: Vec<McpServerDef>) {
        let mut servers = locked(&self.servers);
        servers.retain(|name, _| defs.iter().any(|def| &def.name == name));
        for def in defs {
            if servers
                .get(&def.name)
                .is_some_and(|entry| entry.slot.def == def)
            {
                continue;
            }
            let entry = self.start(def.clone());
            servers.insert(def.name, entry);
        }
        drop(servers);
        let _ = self.events.send(McpEvent::Changed);
    }

    /// Stops a server and starts it again from its definition, with its
    /// restart allowance renewed. `false` for a name nobody knows.
    pub fn restart(&self, name: &str) -> bool {
        let mut servers = locked(&self.servers);
        let Some(def) = servers.get(name).map(|entry| entry.slot.def.clone()) else {
            return false;
        };
        servers.remove(name);
        let entry = self.start(def);
        servers.insert(name.to_owned(), entry);
        drop(servers);
        let _ = self.events.send(McpEvent::Changed);
        true
    }

    fn start(&self, def: McpServerDef) -> Entry {
        let slot = Slot::new(def);
        let task = (!slot.def.disabled).then(|| {
            tokio::spawn(supervise(
                Arc::clone(&slot),
                self.config.clone(),
                self.events.clone(),
            ))
        });
        Entry { slot, task }
    }

    /// Every server, by name.
    pub fn status(&self) -> Vec<ServerStatus> {
        locked(&self.servers)
            .values()
            .map(|entry| entry.slot.status())
            .collect()
    }

    /// The tools of every ready server, as registry handlers. A server's
    /// approval tier is the one the user set for it, else `exec`.
    pub fn handlers(&self) -> Vec<Arc<dyn ToolHandler>> {
        let mut handlers: Vec<Arc<dyn ToolHandler>> = Vec::new();
        for entry in locked(&self.servers).values() {
            let slot = &entry.slot;
            let state = locked(&slot.inner);
            if state.state != ServerState::Ready {
                continue;
            }
            let approval = match slot.def.approval {
                Some(McpApproval::Read) => ApprovalTier::Read,
                Some(McpApproval::Write) => ApprovalTier::Write,
                Some(McpApproval::Exec) | None => ApprovalTier::Exec,
            };
            for tool in &state.listing {
                handlers.push(Arc::new(McpTool {
                    slot: Arc::clone(slot),
                    remote_name: tool.remote.clone(),
                    spec: ToolSpec {
                        name: SmolStr::new(&tool.name),
                        description: SmolStr::new(&tool.description),
                        parameters: tool.schema.clone(),
                    },
                    approval,
                }));
            }
        }
        handlers
    }

    /// Stops every server, the process groups included, and forgets them.
    /// Blocks for a fraction of a second; the servers are stopped together.
    pub fn shutdown(&self) {
        let entries: Vec<Entry> = std::mem::take(&mut *locked(&self.servers))
            .into_values()
            .collect();
        let clients: Vec<Arc<Client>> = entries
            .iter()
            .filter_map(|entry| locked(&entry.slot.inner).client.take())
            .collect();
        drop(entries);
        std::thread::scope(|scope| {
            for client in &clients {
                scope.spawn(|| client.shutdown());
            }
        });
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn launch_for(def: &McpServerDef, config: &ManagerConfig) -> Launch {
    let mut env = config.inherit.clone();
    for (key, value) in &def.env {
        env.retain(|(name, _)| name != key.as_str());
        env.push((key.into(), value.into()));
    }
    let cwd = match def.cwd.as_deref() {
        Some(cwd) if !cwd.is_empty() => config.workspace.join(cwd),
        _ => config.workspace.clone(),
    };
    Launch {
        command: def.command.clone(),
        args: def.args.clone(),
        env,
        cwd,
        stderr_log: Some(log_path(&config.agent_dir, &def.name)),
        timeout: Duration::from_secs(def.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
    }
}

/// Bytes of the end of a stderr log that are read for the reason a server failed.
const TAIL_BYTES: u64 = 4096;

/// Least time between two re-lists of one server's tools, however often it
/// says they changed.
const RELIST_EVERY: Duration = Duration::from_secs(1);

/// What a server said on standard error last, to put beside "it exited".
fn stderr_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    // Only the end of the file: a log can be a megabyte, and what a failure
    // left is in its last lines.
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(line.trim().chars().take(160).collect())
}

fn failure_reason(error: &McpError, log: &Path) -> String {
    let mut reason = error.to_string();
    if matches!(error, McpError::Closed(_))
        && let Some(tail) = stderr_tail(log)
    {
        reason.push_str(&format!(" · stderr: {tail}"));
    }
    reason
}

async fn supervise(slot: Arc<Slot>, config: ManagerConfig, events: UnboundedSender<McpEvent>) {
    let log = log_path(&config.agent_dir, &slot.name);
    let mut restarts = 0;
    loop {
        let (client_events, mut from_client) = unbounded_channel();
        let launch = launch_for(&slot.def, &config);
        let started = async {
            let client = Arc::new(Client::connect(&launch, client_events).await?);
            let tools = if client.has_tools {
                client.list_tools().await?
            } else {
                Vec::new()
            };
            Ok::<_, McpError>((client, tools))
        }
        .await;
        let crash = match started {
            Ok((client, tools)) => {
                let notes = publish(&slot, &client, tools);
                for note in notes {
                    let _ = events.send(McpEvent::Notice(note));
                }
                let _ = events.send(McpEvent::Changed);
                // Ready: wait for the list to change or the server to die.
                loop {
                    match from_client.recv().await {
                        Some(ClientEvent::ToolsChanged) => {
                            match client.list_tools().await {
                                Ok(tools) => {
                                    for note in publish(&slot, &client, tools) {
                                        let _ = events.send(McpEvent::Notice(note));
                                    }
                                }
                                Err(error) => {
                                    let _ = events.send(McpEvent::Notice(format!(
                                        "mcp: {}: could not re-list its tools: {error}",
                                        slot.name
                                    )));
                                }
                            }
                            let _ = events.send(McpEvent::Changed);
                            // A server that announces a change over and over
                            // is read at this pace, not at its own.
                            tokio::time::sleep(RELIST_EVERY).await;
                        }
                        Some(ClientEvent::Exited(reason)) => break Some(reason),
                        None => break Some("the connection was lost".to_owned()),
                    }
                }
            }
            Err(error) => {
                let reason = failure_reason(&error, &log);
                if matches!(error, McpError::Closed(_)) {
                    Some(reason)
                } else {
                    slot.set(ServerState::Failed, &reason);
                    let _ = events.send(McpEvent::Notice(format!(
                        "mcp: {} failed: {reason}",
                        slot.name
                    )));
                    let _ = events.send(McpEvent::Changed);
                    return;
                }
            }
        };
        let Some(reason) = crash else { return };
        let reason = match stderr_tail(&log) {
            Some(tail) if !reason.contains("stderr:") => format!("{reason} · stderr: {tail}"),
            _ => reason,
        };
        if restarts >= MAX_RESTARTS {
            slot.set(
                ServerState::Failed,
                &format!(
                    "{reason} (restarted {restarts}x, giving up; /mcp restart {} to try again)",
                    slot.name
                ),
            );
            let _ = events.send(McpEvent::Notice(format!(
                "mcp: {} failed: {reason}",
                slot.name
            )));
            let _ = events.send(McpEvent::Changed);
            return;
        }
        restarts += 1;
        slot.set(
            ServerState::Starting,
            &format!("restarting after: {reason}"),
        );
        let _ = events.send(McpEvent::Notice(format!(
            "mcp: {} stopped ({reason}); restarting",
            slot.name
        )));
        let _ = events.send(McpEvent::Changed);
    }
}

/// Records a connection and its tool list as the slot's ready state, and
/// returns what was left out of the list and why.
fn publish(slot: &Slot, client: &Arc<Client>, tools: Vec<ToolInfo>) -> Vec<String> {
    let (tools, vetted) = crate::vet::vet(&slot.name, tools);
    let mut notes: Vec<String> = vetted.into_iter().collect();
    let mut seen = HashSet::new();
    let mut listing = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool_name(&slot.name, &tool.name);
        if !seen.insert(name.clone()) {
            notes.push(format!(
                "mcp: {}: the tool {:?} is left out — its name is {name} once made safe, which another tool has",
                slot.name, tool.name
            ));
            continue;
        }
        listing.push(Listed {
            name,
            remote: tool.name,
            description: tool.description,
            schema: tool.input_schema,
        });
    }
    listing.sort_by(|a, b| a.name.cmp(&b.name));
    let mut state = locked(&slot.inner);
    state.state = ServerState::Ready;
    state.reason.clear();
    state.listing = listing;
    state.client = Some(Arc::clone(client));
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stderr_tail_reads_only_the_end_of_a_large_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let mut text = "first line\n".to_owned();
        text.push_str(&"filler line\n".repeat(300_000));
        text.push_str("the failure\n\n");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(stderr_tail(&path).as_deref(), Some("the failure"));
        assert_eq!(stderr_tail(&dir.path().join("none.log")), None);
        // A log that is nothing but one long line gives its end, cut to a row.
        std::fs::write(&path, "y".repeat(100_000)).unwrap();
        assert_eq!(stderr_tail(&path).unwrap().len(), 160);
    }
}
