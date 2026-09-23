use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures::StreamExt;
use smol_str::SmolStr;
use titi_genome::Genome;
use titi_providers::{
    ChatMessage, ErrorReason, RequestCtx, Role, StreamEvent, Transport, TransportError, WireRequest,
};
use titi_tools::{ApprovalMode, ToolRegistry};
use tokio::sync::mpsc;

use crate::claims::Claims;
use crate::findings::Findings;
use crate::protocol::{ContextPart, EngineCommand, EngineEvent, TurnId};
use crate::registry::{RegistryError, ResolvedModel};
use crate::steering::Steering;
use crate::tool_loop::{
    ApprovalWaiters, ToolCallCollector, TouchedSink, TrajectorySink, execute_tools,
};

/// Identity the main turn claims files under.
const MAIN_AGENT: SmolStr = SmolStr::new_inline("Main");

/// Runs blocking map-building work off the async executor.
///
/// The map is an optimisation, never a precondition: a refresh that errors or
/// panics degrades to "no map this turn" instead of failing the turn. A panic
/// inside `spawn_blocking` surfaces as a `JoinError`, which this discards the
/// same way an `Err` is discarded.
async fn run_off_thread(
    work: impl FnOnce() -> Option<SmolStr> + Send + 'static,
) -> Option<SmolStr> {
    tokio::task::spawn_blocking(work).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_off_thread_failure_degrades_to_none() {
        assert_eq!(run_off_thread(|| None).await, None);
    }

    #[tokio::test]
    async fn an_off_thread_panic_degrades_to_none() {
        // The turn must survive a panicking index build.
        let outcome = run_off_thread(|| panic!("index build exploded")).await;
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn a_successful_run_passes_the_map_through() {
        let map = run_off_thread(|| Some(SmolStr::new_inline("<genome>\n</genome>"))).await;
        assert_eq!(map.as_deref(), Some("<genome>\n</genome>"));
    }

    fn message(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: text.into(),
            tool_calls: Vec::new(),
        }
    }

    fn folding_config() -> EngineConfig {
        let mut config = EngineConfig::new("primary");
        // Far below the 80% threshold: the automatic fold would not fire,
        // which is the whole reason `/compact` exists.
        config.context_window = 100_000;
        config.compaction.keep_recent_tokens = 4;
        config
    }

    #[test]
    fn manual_compaction_folds_below_the_threshold() {
        let mut config = folding_config();
        config.restored_messages = vec![
            message(Role::User, "the first question, long enough to fold"),
            message(Role::Assistant, "the first answer"),
            message(Role::User, "now"),
        ];

        let event = compact_history(&mut config, None, false, TurnId(7));

        match event {
            EngineEvent::Compacted {
                turn_id, folded, ..
            } => {
                assert_eq!(turn_id, TurnId(7));
                assert_eq!(folded, 2);
            }
            other => panic!("expected a compaction, got {other:?}"),
        }
        assert_eq!(config.restored_messages.len(), 2);
        assert_eq!(config.restored_messages[0].role, Role::System);
        assert_eq!(config.restored_messages[1].content, "now");
    }

    /// The digest keeps only the first line of each folded prompt, so a
    /// focus has to pull the rest of its thread back in or naming it does
    /// nothing.
    #[test]
    fn a_focused_compaction_keeps_the_lines_it_names() {
        let mut config = folding_config();
        config.restored_messages = vec![
            message(
                Role::User,
                "please review the diff\nthe auth guard moved to the middleware",
            ),
            message(
                Role::User,
                "another question\nabout pagination internals instead",
            ),
            message(Role::User, "now"),
        ];

        let event = compact_history(&mut config, Some("auth"), false, TurnId(1));

        assert!(matches!(event, EngineEvent::Compacted { .. }), "{event:?}");
        let digest = config.restored_messages[0].content.to_string();
        assert!(
            digest.contains("the auth guard moved to the middleware"),
            "{digest}"
        );
        assert!(!digest.contains("pagination internals"), "{digest}");
    }

    /// A running turn hands its own history back when it ends. Folding
    /// underneath it would be overwritten, or cost that turn its answer.
    #[test]
    fn manual_compaction_waits_for_a_running_turn() {
        let mut config = folding_config();
        config.restored_messages = vec![
            message(Role::User, "the first question, long enough to fold"),
            message(Role::Assistant, "the first answer"),
            message(Role::User, "now"),
        ];
        let before = config.restored_messages.clone();

        let event = compact_history(&mut config, None, true, TurnId(1));

        assert!(matches!(event, EngineEvent::Notice { .. }), "{event:?}");
        assert_eq!(config.restored_messages, before);
    }
}

/// Resolves a model id to its provider transport.
pub trait TransportResolver: Send + Sync + 'static {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError>;
}

impl<F> TransportResolver for F
where
    F: Fn(&str) -> Result<ResolvedModel, RegistryError> + Send + Sync + 'static,
{
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self(model)
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub primary_model: SmolStr,
    pub fallback_models: Vec<SmolStr>,
    pub max_transient_retries: u32,
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub max_tool_rounds: u32,
    pub approval_mode: ApprovalMode,
    /// Workspace root kept indexed turn-by-turn. `None` disables the Genome.
    pub genome_root: Option<PathBuf>,
    /// Ranked files injected per prompt.
    pub genome_limit: usize,
    /// Workspace a subagent's tools are jailed to. `None` keeps its registry
    /// empty even when `agent_model` is set.
    pub workspace_root: Option<PathBuf>,
    /// Model a spawned subagent runs on. `None` disables spawning: without a
    /// runner the supervisor is never built.
    pub agent_model: Option<SmolStr>,
    /// Whether a subagent may write and execute. Off by default: a subagent
    /// has no approval surface, so it gets read tools only.
    pub agent_writes: bool,
    /// Tool rounds a subagent may spend before it is stopped.
    pub agent_rounds: u32,
    /// Read cache shared with the subagent's read tool, so a file the main
    /// turn read is not read again from disk.
    pub read_cache: titi_tools::ReadCache,
    /// Conversation replayed from a persisted session, prepended to every
    /// prompt so a resumed session keeps its history.
    pub restored_messages: Vec<ChatMessage>,
    /// Context window the model reports, used to decide when to fold.
    pub context_window: u64,
    /// When and how the oldest messages are folded away.
    pub compaction: titi_core::compaction::CompactionPolicy,
    /// Agent directory holding `SOUL.md`, `PERSONALITY.md` and the memory
    /// index. `None` sends no identity — only the genome map.
    pub agent_dir: Option<PathBuf>,
    /// Embeddings model from `memory.embeddingModel`. `None` uses the local
    /// trigram embedder, which needs no network.
    pub embedding_model: Option<String>,
    /// Which files the subagent's tools refuse as credentials. The surface
    /// builds the main turn's tools with the same policy.
    pub sensitive: titi_tools::SensitivePolicy,
    /// Mask IPv4 addresses in tool output (`privacy.maskIps`). Keys are
    /// masked regardless.
    pub mask_ips: bool,
    /// Checks run against the coder's patch before a reviewer is called
    /// (`goal.gates`). Each entry is a program and its arguments; an empty
    /// list sends every patch straight to the reviewer.
    pub goal_gates: Vec<crate::goal::GateCommand>,
    /// Session the turns are recorded under, so a finished turn can name it.
    /// `None` (headless one-shots, tests) simply never names anything.
    pub session_id: Option<String>,
    pub fallback_cooldown: std::time::Duration,
    pub fallback_chain:
        std::sync::Arc<tokio::sync::Mutex<titi_providers::FallbackChain<smol_str::SmolStr>>>,
    pub judgment_provider: Option<crate::judgment::JudgmentProvider>,
}

impl EngineConfig {
    pub fn new(primary_model: impl Into<SmolStr>) -> Self {
        let primary = primary_model.into();
        Self {
            primary_model: primary.clone(),
            fallback_models: Vec::new(),
            max_transient_retries: 2,
            command_capacity: 64,
            event_capacity: 256,
            max_tool_rounds: 8,
            approval_mode: ApprovalMode::Write,
            genome_root: None,
            genome_limit: 24,
            workspace_root: None,
            agent_model: None,
            agent_writes: false,
            agent_rounds: crate::tool_agent::DEFAULT_AGENT_ROUNDS,
            read_cache: titi_tools::ReadCache::default(),
            restored_messages: Vec::new(),
            context_window: 128_000,
            compaction: titi_core::compaction::CompactionPolicy::default(),
            agent_dir: None,
            embedding_model: None,
            sensitive: titi_tools::SensitivePolicy::default(),
            mask_ips: true,
            goal_gates: Vec::new(),
            session_id: None,
            fallback_cooldown: std::time::Duration::from_secs(60),
            fallback_chain: std::sync::Arc::new(tokio::sync::Mutex::new(
                titi_providers::FallbackChain::new(primary, Vec::new()),
            )),
            judgment_provider: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine command channel closed")]
    CommandChannelClosed,
}

/// Surface-side handle. TUI, GPUI, and headless RPC use the same contract.
pub struct Engine {
    commands: mpsc::Sender<EngineCommand>,
    events: mpsc::Receiver<EngineEvent>,
    claims: Claims,
    findings: Findings,
}

impl Engine {
    pub async fn send(&self, command: EngineCommand) -> Result<(), EngineError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| EngineError::CommandChannelClosed)
    }

    pub async fn recv(&mut self) -> Option<EngineEvent> {
        self.events.recv().await
    }

    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }

    pub fn try_send(&self, command: EngineCommand) -> Result<(), EngineError> {
        self.commands
            .try_send(command)
            .map_err(|_| EngineError::CommandChannelClosed)
    }

    /// The runtime's write-claim table, so a surface can show who holds what
    /// (or hold a file itself before dispatching work).
    pub fn claims(&self) -> &Claims {
        &self.claims
    }

    /// Findings subagents have reported, in order.
    pub fn findings(&self) -> &Findings {
        &self.findings
    }
}

/// UI-independent command loop and turn scheduler.
pub struct EngineRuntime {
    config: EngineConfig,
    resolver: Arc<dyn TransportResolver>,
    commands: mpsc::Receiver<EngineCommand>,
    events: mpsc::Sender<EngineEvent>,
    next_turn: Arc<AtomicU64>,
    agents: Option<crate::agents::AgentSupervisor>,
    tools: ToolRegistry,
    approval_waiters: ApprovalWaiters,
    trajectory: TrajectorySink,
    /// Live index, refreshed from `config.genome_root` before each turn.
    genome: Arc<tokio::sync::Mutex<Option<Genome>>>,
    /// Files this session read or edited; boosts their rank in the projection.
    touched: TouchedSink,
    /// Per-file write claims shared by every agent in this runtime.
    claims: Claims,
    /// Messages typed mid-flight, injected at the next step boundary.
    steering: Steering,
    /// Bumped by every `RestoreHistory`. A turn that started before the
    /// rewind must not write its stale history back over the replacement.
    history_epoch: u64,
}

impl EngineRuntime {
    pub fn start(config: EngineConfig, resolver: Arc<dyn TransportResolver>) -> Engine {
        Self::start_inner(
            config,
            resolver,
            None,
            ToolRegistry::new(),
            TrajectorySink::default(),
        )
    }

    pub fn start_with_agents(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Arc<dyn crate::agents::AgentRunner>,
    ) -> Engine {
        Self::start_inner(
            config,
            resolver,
            Some(runner),
            ToolRegistry::new(),
            TrajectorySink::default(),
        )
    }

    pub fn start_with_tools(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        tools: ToolRegistry,
    ) -> Engine {
        Self::start_inner(config, resolver, None, tools, TrajectorySink::default())
    }

    pub fn start_with_agents_and_tools(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Arc<dyn crate::agents::AgentRunner>,
        tools: ToolRegistry,
    ) -> Engine {
        Self::start_inner(
            config,
            resolver,
            Some(runner),
            tools,
            TrajectorySink::default(),
        )
    }

    pub fn start_with_session(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Option<Arc<dyn crate::agents::AgentRunner>>,
        tools: ToolRegistry,
        trajectory: TrajectorySink,
    ) -> Engine {
        Self::start_inner(config, resolver, runner, tools, trajectory)
    }

    fn start_inner(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Option<Arc<dyn crate::agents::AgentRunner>>,
        tools: ToolRegistry,
        trajectory: TrajectorySink,
    ) -> Engine {
        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let (event_tx, event_rx) = mpsc::channel(config.event_capacity);
        let claims = Claims::new();
        let findings = Findings::default();
        let touched: TouchedSink = TouchedSink::default();
        // A subagent shares the runtime's claim table, touched-file set, read
        // cache and findings bus, so it cannot write a file the parent holds,
        // its reads warm the parent's cache, and the parent can read what it
        // learned.
        let runner = runner.or_else(|| {
            let model = config.agent_model.clone()?;
            let root = config.workspace_root.clone()?;
            let mut tools = ToolRegistry::new();
            for tool in titi_tools::workspace_tools_with_policy(
                &root,
                config.read_cache.clone(),
                config.sensitive.clone(),
            ) {
                tools.register(Arc::from(tool));
            }
            if !config.agent_writes {
                // Nothing exec- or write-tier is registered, so no call can
                // wait for an approval this surface cannot show.
                tools.retain_tiers(&[titi_tools::ApprovalTier::Read]);
            }
            Some(Arc::new(
                crate::tool_agent::ToolAgentRunner::new(
                    Arc::clone(&resolver),
                    model,
                    tools,
                    claims.clone(),
                    Arc::clone(&touched),
                )
                .with_max_rounds(config.agent_rounds)
                .with_mask_ips(config.mask_ips),
            ) as Arc<dyn crate::agents::AgentRunner>)
        });
        let agents = runner.map(|runner| {
            crate::agents::AgentSupervisor::with_state(
                runner,
                event_tx.clone(),
                claims.clone(),
                findings.clone(),
            )
        });
        let runtime = Self {
            config,
            resolver,
            commands: command_rx,
            events: event_tx,
            next_turn: Arc::new(AtomicU64::new(1)),
            agents,
            tools,
            approval_waiters: ApprovalWaiters::default(),
            trajectory,
            genome: Arc::new(tokio::sync::Mutex::new(None)),
            touched,
            claims: claims.clone(),
            steering: Steering::default(),
            history_epoch: 0,
        };
        tokio::spawn(runtime.run());
        Engine {
            commands: command_tx,
            events: event_rx,
            claims,
            findings,
        }
    }

    async fn run(mut self) {
        let (done_tx, mut done_rx) = mpsc::channel::<TurnDone>(8);
        let mut active: Option<(TurnId, Arc<AtomicBool>)> = None;
        let mut queued = VecDeque::<SmolStr>::new();
        let mut primary_model = self.config.primary_model.clone();

        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        EngineCommand::SubmitPrompt { text } | EngineCommand::FollowUp { text } => {
                            if active.is_some() {
                                queued.push_back(text);
                            } else {
                                let text = self.expand_skills(text).await;
                                let system = self.system_prompt().await;
                                active = Some(self.spawn_turn(text, primary_model.clone(), system, done_tx.clone()));
                            }
                        }
                        EngineCommand::RestoreHistory { messages } => {
                            self.config.restored_messages = messages;
                            self.history_epoch += 1;
                        }
                        EngineCommand::Steer { text } => {
                            // Queued, not applied here: the running turn drains it at
                            // its next step boundary, so steering never aborts work.
                            self.steering.push(text);
                        }
                        EngineCommand::Cancel => {
                            if let Some((turn_id, aborted)) = active.take() {
                                aborted.store(true, Ordering::SeqCst);
                                let _ = self.events.send(EngineEvent::Cancelled { turn_id }).await;
                            }
                            // Cancel is a stop, and taking `active` already stops
                            // the drain on the next `TurnDone`. Left in place the
                            // queue would sit there and fire at some later turn's
                            // end, answering a question the user walked away from.
                            // The text goes back to the surface instead.
                            for text in queued.drain(..) {
                                let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                            }
                        }
                        EngineCommand::SwitchModel { model } => {
                            primary_model = model;
                        }
                        EngineCommand::RunGoal { text } => {
                            self.spawn_goal(text, primary_model.clone());
                        }
                        EngineCommand::Shutdown => {
                            if let Some((_, aborted)) = active.take() {
                                aborted.store(true, Ordering::SeqCst);
                            }
                            break;
                        }
                        EngineCommand::ApproveTool { call_id, approved } => {
                            if let Some(waiter) = self.approval_waiters.lock().await.remove(&call_id) {
                                  let _ = waiter.send(approved);
                            }
                        }
                        EngineCommand::SpawnAgent { name, task, kind } => {
                            if let Some(agents) = &self.agents {
                                agents.spawn(name, task, kind).await;
                            } else {
                                self.emit_control_failure("agent supervisor is not configured").await;
                            }
                        }
                        EngineCommand::FocusAgent { agent_id } => {
                            let handled = match &self.agents {
                                Some(agents) => agents.focus(&agent_id).await,
                                None => false,
                            };
                            if !handled {
                                self.emit_control_failure("agent is not available").await;
                            }
                        }
                        EngineCommand::ReviveAgent { agent_id } => {
                            let handled = match &self.agents {
                                Some(agents) => agents.revive(&agent_id).await,
                                None => false,
                            };
                            if !handled {
                                self.emit_control_failure("agent cannot be revived").await;
                            }
                        }
                        EngineCommand::StopAgent { agent_id } => {
                            let handled = match &self.agents {
                                Some(agents) => agents.stop(&agent_id).await,
                                None => false,
                            };
                            if !handled {
                                self.emit_control_failure("agent is not available").await;
                            }
                        }
                        EngineCommand::DescribeContext => {
                            let parts = self.context_parts().await;
                            let window = self.config.context_window;
                            let _ = self.events.send(EngineEvent::ContextBreakdown { parts, window }).await;
                        }
                        EngineCommand::Compact { focus } => {
                            let turn_id = TurnId(self.next_turn.load(Ordering::SeqCst));
                            let running = active.is_some();
                            let event = compact_history(&mut self.config, focus.as_deref(), running, turn_id);
                            let _ = self.events.send(event).await;
                        }
                    }
                }
                completed = done_rx.recv() => {
                    if let Some(completed) = completed
                        && active.as_ref().is_some_and(|(id, _)| *id == completed.turn_id)
                    {
                        active = None;
                        // A rewind that landed while the turn ran replaced the
                        // history the turn was built on, so that turn's copy is
                        // stale and must not come back.
                        if let Some(history) = completed.history
                            && completed.epoch == self.history_epoch
                        {
                            self.config.restored_messages = history;
                        }
                        // The name comes from the finished turn, never from
                        // inside it: this returns before the namer has talked
                        // to anything.
                        self.name_session(primary_model.clone());
                        if let Some(text) = queued.pop_front() {
                            let text = self.expand_skills(text).await;
                            let system = self.system_prompt().await;
                            active = Some(self.spawn_turn(text, primary_model.clone(), system, done_tx.clone()));
                        }
                    }
                }
            }
        }
    }

    /// Hands a finished turn to the session namer.
    ///
    /// Everything here is a cheap local check — is there a session, is there
    /// anything to name it after — and the attempt itself lives in its own
    /// task. No model is called and no database is opened on this thread, so
    /// a slow, keyless or failing namer costs the command loop nothing.
    fn name_session(&self, model: SmolStr) {
        let (Some(agent_dir), Some(session_id)) = (
            self.config.agent_dir.clone(),
            self.config.session_id.clone(),
        ) else {
            return;
        };
        let Some(first_message) = crate::naming::first_user_message(&self.config.restored_messages)
        else {
            return;
        };
        let workspace = self
            .config
            .workspace_root
            .clone()
            .or_else(|| self.config.genome_root.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        crate::naming::SessionNamer::new(
            Arc::clone(&self.resolver),
            self.events.clone(),
            agent_dir,
            session_id,
            workspace,
            model,
        )
        .spawn(first_message);
    }

    async fn emit_control_failure(&self, message: &str) {
        let _ = self
            .events
            .send(EngineEvent::Failed {
                turn_id: None,
                reason: ErrorReason::Rejected,
                message: message.into(),
            })
            .await;
    }

    /// Pull in the body of every skill the prompt names with `/name`.
    ///
    /// Only the bytes sent to the model change: the surface already logged
    /// and drew the text as typed. A refused body is reported instead of
    /// being pasted in, so a skill missing from a prompt is never silent.
    async fn expand_skills(&self, text: SmolStr) -> SmolStr {
        let cwd = self
            .config
            .workspace_root
            .clone()
            .or_else(|| self.config.genome_root.clone());
        let agent_dir = self.config.agent_dir.clone();
        let typed = text.clone();
        // Reads a directory and up to BODY_CAP per skill: off the runtime thread.
        let expanded = tokio::task::spawn_blocking(move || {
            crate::skills::expand(&typed, cwd.as_deref(), agent_dir.as_deref())
        })
        .await;
        let Ok(expanded) = expanded else {
            return text;
        };
        for notice in expanded.notices {
            let _ = self
                .events
                .send(EngineEvent::Notice {
                    message: notice.into(),
                })
                .await;
        }
        expanded.text.map(SmolStr::from).unwrap_or(text)
    }

    /// Identity and personality, then project rules and skill names, then memory and the genome map.
    ///
    /// The model used to see only the map, so it had no identity and no
    /// memory of earlier sessions. A missing agent directory degrades to the
    /// map alone rather than failing the turn. Project rules and skills are
    /// their own sections: they are not folded into `SOUL.md`. Skill bodies
    /// are not injected.
    async fn system_prompt(&self) -> Option<SmolStr> {
        let identity = self.identity_prompt();
        let project = self.project_context();
        let skills = self.skill_list();
        let recalled = self.recalled_memory().await;
        let genome = self.genome_system().await;
        let mut parts = Vec::new();
        if let Some(identity) = identity {
            parts.push(identity.to_string());
        }
        if let Some(project) = project {
            parts.push(project);
        }
        if let Some(skills) = skills {
            parts.push(skills);
        }
        if let Some(recalled) = recalled {
            parts.push(recalled.to_string());
        }
        if let Some(genome) = genome {
            parts.push(genome.to_string());
        }
        let joined = parts.join("\n\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined.into())
        }
    }

    /// `AGENTS.md` from the workspace and the agent directory. Flagged files
    /// are already omitted. A missing workspace still contributes the user file.
    fn project_context(&self) -> Option<String> {
        let cwd = self
            .config
            .workspace_root
            .as_deref()
            .or(self.config.genome_root.as_deref());
        crate::project_context::render(
            cwd,
            self.config.agent_dir.as_deref(),
            process_home().as_deref(),
        )
    }

    /// Name and description of discovered skills. Bodies stay on disk.
    fn skill_list(&self) -> Option<String> {
        let cwd = self
            .config
            .workspace_root
            .as_deref()
            .or(self.config.genome_root.as_deref());
        crate::skills::render(cwd, self.config.agent_dir.as_deref())
    }

    /// The memories relevant to this turn, ranked against the files it has
    /// already touched. Off the runtime thread: the index is synchronous
    /// SQLite, and blocking the runtime thread panics.
    async fn recalled_memory(&self) -> Option<SmolStr> {
        let agent_dir = self.config.agent_dir.clone()?;
        let touched = Arc::clone(&self.touched);
        run_off_thread(move || {
            let index = titi_memory::index::MemoryIndex::open(&agent_dir).ok()?;
            let touched = touched.blocking_lock().snapshot();
            let recalled = index.recall("", &touched).ok()?;
            let block = titi_memory::index::render_recall(&recalled);
            if block.is_empty() {
                None
            } else {
                Some(block.into())
            }
        })
        .await
    }

    /// Soul and personality. Memory is no longer pasted in whole: the index
    /// recalls the rows this turn needs, which is the only copy the model sees.
    fn identity_prompt(&self) -> Option<SmolStr> {
        let agent_dir = self.config.agent_dir.clone()?;
        let built = titi_soul::SystemPromptBuilder::build(&agent_dir, None, None).ok()?;
        Some(built.render().into())
    }

    /// Refresh the live index off the async threads and render this turn's map.
    async fn genome_system(&self) -> Option<SmolStr> {
        let root = self.config.genome_root.clone()?;
        let genome = Arc::clone(&self.genome);
        let touched = Arc::clone(&self.touched);
        let limit = self.config.genome_limit;
        run_off_thread(move || {
            let mut guard = genome.blocking_lock();
            let index = guard.get_or_insert_with(Genome::default);
            index.refresh(&root).ok()?;
            let touched: Vec<String> = touched.blocking_lock().snapshot();
            Some(SmolStr::from(index.project_with(limit, &touched)))
        })
        .await
    }

    /// What fills the context window right now, one part per piece a turn
    /// assembles, measured with the estimator that feeds `ContextUsage`.
    ///
    /// The genome map and the recalled memory are their own parts on
    /// purpose: they are rebuilt every turn, so they are both the largest
    /// moving weight and the reason a prompt cache misses.
    async fn context_parts(&self) -> Vec<ContextPart> {
        let tools = self
            .tools
            .specs()
            .iter()
            .map(|spec| {
                titi_core::compaction::estimate_tokens(&spec.name)
                    + titi_core::compaction::estimate_tokens(&spec.description)
                    + titi_core::compaction::estimate_tokens(&spec.parameters.to_string())
            })
            .sum();
        vec![
            context_part("system prompt", self.identity_prompt().as_deref()),
            context_part("project rules", self.project_context().as_deref()),
            context_part("skills", self.skill_list().as_deref()),
            context_part("recalled memory", self.recalled_memory().await.as_deref()),
            context_part("genome map", self.genome_system().await.as_deref()),
            ContextPart {
                label: "history".into(),
                tokens: crate::compaction::estimate_request(&self.config.restored_messages),
            },
            ContextPart {
                label: "tool specs".into(),
                tokens: tools,
            },
        ]
    }

    fn spawn_turn(
        &self,
        prompt: SmolStr,
        primary_model: SmolStr,
        system: Option<SmolStr>,
        done: mpsc::Sender<TurnDone>,
    ) -> (TurnId, Arc<AtomicBool>) {
        let epoch = self.history_epoch;
        let turn_id = TurnId(self.next_turn.fetch_add(1, Ordering::SeqCst));
        let aborted = Arc::new(AtomicBool::new(false));
        let task_abort = Arc::clone(&aborted);
        let config = self.config.clone();
        let resolver = Arc::clone(&self.resolver);
        let events = self.events.clone();
        let tools = self.tools.clone();
        let waiters = Arc::clone(&self.approval_waiters);
        let trajectory = Arc::clone(&self.trajectory);
        let touched = Arc::clone(&self.touched);
        let claims = self.claims.clone();
        let steering = self.steering.clone();
        tokio::spawn(async move {
            let history = run_turn(
                turn_id,
                prompt,
                primary_model,
                system,
                config,
                resolver,
                events,
                task_abort,
                tools,
                waiters,
                trajectory,
                touched,
                claims,
                steering,
            )
            .await;
            let _ = done
                .send(TurnDone {
                    turn_id,
                    history,
                    epoch,
                })
                .await;
        });
        (turn_id, aborted)
    }

    /// `/goal` runs the existing loop on the session model. It does not
    /// replace the active turn or queue a `SubmitPrompt`.
    fn spawn_goal(&self, text: SmolStr, model: SmolStr) {
        let events = self.events.clone();
        let text = text.trim().to_owned();
        if text.is_empty() {
            tokio::spawn(async move {
                let _ = events
                    .send(EngineEvent::GoalFinished {
                        report: "usage: /goal <text>".into(),
                    })
                    .await;
            });
            return;
        }
        let coder = Arc::new(crate::RunnerCoder::new(Arc::new(
            crate::StreamingAgentRunner::new(Arc::clone(&self.resolver), model.clone()),
        )));
        let reviewer = Arc::new(crate::AgentReviewer::new(
            Arc::new(crate::StreamingAgentRunner::new(
                Arc::clone(&self.resolver),
                model,
            )),
            "reviewer",
        ));
        let gates = crate::CommandGates::new(self.config.goal_gates.clone());
        let gates: Arc<dyn crate::Gates> = Arc::new(match &self.config.workspace_root {
            Some(root) => gates.in_dir(root.clone()),
            None => gates,
        });
        tokio::spawn(async move {
            let outcome = crate::GoalLoop::new(coder, reviewer)
                .with_gates(gates)
                .run(text)
                .await;
            let _ = events
                .send(EngineEvent::GoalFinished {
                    report: crate::goal_report(&outcome).into(),
                })
                .await;
        });
    }
}

fn process_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn context_part(label: &str, text: Option<&str>) -> ContextPart {
    ContextPart {
        label: label.into(),
        tokens: text
            .map(titi_core::compaction::estimate_tokens)
            .unwrap_or(0),
    }
}

/// Manual compaction: fold the replayed history now, whatever the threshold
/// says, and report what happened.
///
/// A running turn built its request on this history and hands its own copy
/// back when it ends, so folding underneath it would either be overwritten
/// or cost that turn its answer. Manual compaction waits for it instead.
///
/// `focus` is what the user asked to keep: the lines of the folded prefix
/// that mention it are appended to the digest, so the thread they named is
/// not the part the fold takes away.
fn compact_history(
    config: &mut EngineConfig,
    focus: Option<&str>,
    turn_running: bool,
    turn_id: TurnId,
) -> EngineEvent {
    if turn_running {
        return EngineEvent::Notice {
            message: "compact: a turn is running, try again when it finishes".into(),
        };
    }
    let forced = titi_core::compaction::CompactionPolicy {
        threshold_percent: 0.0,
        ..config.compaction.clone()
    };
    let before = focus.map(|_| config.restored_messages.clone());
    let Some(done) = crate::compaction::compact(
        &mut config.restored_messages,
        &forced,
        config.context_window.max(1),
    ) else {
        return EngineEvent::Notice {
            message: "compact: nothing to fold yet".into(),
        };
    };
    if let (Some(focus), Some(before)) = (focus, before)
        && let Some(digest) = config.restored_messages.first_mut()
    {
        let folded: Vec<&str> = before
            .iter()
            .take(done.folded)
            .map(|message| message.content.as_str())
            .collect();
        let kept = titi_core::compaction::focus_digest(focus, &folded);
        digest.content = format!("{}\n{kept}", digest.content).into();
    }
    EngineEvent::Compacted {
        turn_id,
        folded: done.folded as u32,
        tokens_before: done.tokens_before,
        strategy: done.strategy,
    }
}

/// What a finished turn hands back to the command loop.
struct TurnDone {
    turn_id: TurnId,
    /// The history the next turn must start from, without the system prompt
    /// (rebuilt per turn). `None` when the turn did not finish cleanly.
    history: Option<Vec<ChatMessage>>,
    /// The value of `EngineRuntime::history_epoch` when the turn started.
    epoch: u64,
}

/// Returns the visible history to keep, or `None` when this turn must leave
/// the history untouched.
async fn run_turn(
    turn_id: TurnId,
    prompt: SmolStr,
    primary_model: SmolStr,
    system: Option<SmolStr>,
    config: EngineConfig,
    resolver: Arc<dyn TransportResolver>,
    events: mpsc::Sender<EngineEvent>,
    aborted: Arc<AtomicBool>,
    tools: ToolRegistry,
    waiters: ApprovalWaiters,
    trajectory: TrajectorySink,
    touched: TouchedSink,
    claims: Claims,
    steering: Steering,
) -> Option<Vec<ChatMessage>> {
    let mut models = Vec::with_capacity(1 + config.fallback_models.len());
    models.push(primary_model);
    models.extend(config.fallback_models);
    let mut previous_model: Option<SmolStr> = None;

    for model in models {
        if aborted.load(Ordering::SeqCst) {
            return None;
        }
        if let Some(previous) = previous_model.take() {
            let _ = events
                .send(EngineEvent::ModelSwitched {
                    turn_id,
                    from: previous,
                    to: model.clone(),
                })
                .await;
        }
        let resolved = match resolver.resolve(&model) {
            Ok(resolved) => resolved,
            Err(error) => {
                let _ = events
                    .send(EngineEvent::Failed {
                        turn_id: Some(turn_id),
                        reason: ErrorReason::Rejected,
                        message: error.to_string().into(),
                    })
                    .await;
                return None;
            }
        };
        let api_key = resolved.credential.map(|credential| credential.access);
        let wire_model = resolved.wire_model;
        let transport = resolved.transport;
        let _ = events
            .send(EngineEvent::TurnStarted {
                turn_id,
                model: model.clone(),
            })
            .await;

        if let Some(recorder) = trajectory.lock().await.as_mut() {
            let _ = recorder.record(titi_core::trajectory::EventKind::UserMessage {
                text: prompt.to_string(),
            });
        }
        let mut messages = Vec::new();
        if let Some(system) = &system {
            messages.push(ChatMessage {
                role: Role::System,
                content: system.clone(),
                tool_calls: Vec::new(),
            });
        }
        // A resumed session replays its history before the new prompt.
        messages.extend(config.restored_messages.iter().cloned());
        messages.push(ChatMessage {
            role: Role::User,
            content: prompt.clone(),
            tool_calls: Vec::new(),
        });
        let mut tool_rounds = 0;
        loop {
            // `execute_tools` returns as soon as the abort lands, and the loop
            // used to walk straight back into another stream. Nothing below
            // this point is worth doing for a turn nobody will read.
            if aborted.load(Ordering::SeqCst) {
                return None;
            }
            // Anything typed mid-flight lands before the next attempt.
            for text in steering.drain() {
                messages.push(ChatMessage {
                    role: Role::User,
                    content: text,
                    tool_calls: Vec::new(),
                });
            }
            // A turn accumulates tool results without bound; fold the oldest
            // away before the provider refuses the request.
            if let Some(folded) =
                crate::compaction::compact(&mut messages, &config.compaction, config.context_window)
            {
                if let Some(recorder) = trajectory.lock().await.as_mut() {
                    let _ = recorder.record(titi_core::trajectory::EventKind::Compaction {
                        folded: folded.folded as u64,
                        strategy: folded.strategy.to_string(),
                    });
                }
                let _ = events
                    .send(EngineEvent::Compacted {
                        turn_id,
                        folded: folded.folded as u32,
                        tokens_before: folded.tokens_before,
                        strategy: folded.strategy.clone(),
                    })
                    .await;
            }
            let _ = events
                .send(EngineEvent::ContextUsage {
                    turn_id,
                    tokens: crate::compaction::estimate_request(&messages),
                    window: config.context_window,
                })
                .await;
            let mut last_error = None;
            let mut completed = false;
            for _attempt in 0..=config.max_transient_retries {
                match stream_attempt(
                    turn_id,
                    &messages,
                    &wire_model,
                    Arc::clone(&transport),
                    api_key.clone(),
                    events.clone(),
                    Arc::clone(&aborted),
                    &tools,
                )
                .await
                {
                    Ok((text, calls)) if calls.is_empty() => {
                        if !text.is_empty() {
                            messages.push(ChatMessage {
                                role: Role::Assistant,
                                content: text,
                                tool_calls: Vec::new(),
                            });
                        }
                        completed = true;
                        break;
                    }
                    Ok((text, calls)) => {
                        if tool_rounds >= config.max_tool_rounds {
                            let _ = events
                                .send(EngineEvent::Failed {
                                    turn_id: Some(turn_id),
                                    reason: ErrorReason::Rejected,
                                    message: "tool round cap reached".into(),
                                })
                                .await;
                            return None;
                        }
                        tool_rounds += 1;
                        let extra = execute_tools(
                            turn_id,
                            calls,
                            text,
                            &tools,
                            config.approval_mode,
                            &waiters,
                            &events,
                            &aborted,
                            &trajectory,
                            &touched,
                            &claims,
                            &MAIN_AGENT,
                            config.mask_ips,
                        )
                        .await;
                        messages.extend(extra);
                        last_error = None;
                        break;
                    }
                    Err((error, visible_content)) => {
                        if aborted.load(Ordering::SeqCst) {
                            return None;
                        }
                        if visible_content || !error.is_retryable() {
                            emit_transport_failure(&events, turn_id, error).await;
                            return None;
                        }
                        last_error = Some(error);
                    }
                }
            }
            if completed {
                if let Some(recorder) = trajectory.lock().await.as_mut() {
                    let _ = recorder.record(titi_core::trajectory::EventKind::TurnEnd);
                }
                // A cancelled turn contributes nothing. Its assistant output is
                // partial and its last tool calls may have no results, and a
                // tool call without its result is a request no provider accepts.
                if aborted.load(Ordering::SeqCst) {
                    return None;
                }
                return Some(visible_history(messages, system.as_ref()));
            }
            if last_error.is_some() {
                previous_model = Some(model.clone());
                break;
            }
        }
    }

    let _ = events
        .send(EngineEvent::Failed {
            turn_id: Some(turn_id),
            reason: ErrorReason::Connection,
            message: "all configured models are unavailable".into(),
        })
        .await;
    None
}

/// The turn's messages without the system prompt it was given: the next turn
/// rebuilds that itself. A digest compaction put in its place is not the
/// system prompt and stays, so folded messages do not come back.
fn visible_history(mut messages: Vec<ChatMessage>, system: Option<&SmolStr>) -> Vec<ChatMessage> {
    if let Some(system) = system
        && messages
            .first()
            .is_some_and(|first| first.role == Role::System && first.content == *system)
    {
        messages.remove(0);
    }
    messages
}

async fn stream_attempt(
    turn_id: TurnId,
    messages: &[ChatMessage],
    model: &SmolStr,
    transport: Arc<dyn Transport>,
    api_key: Option<SmolStr>,
    events: mpsc::Sender<EngineEvent>,
    aborted: Arc<AtomicBool>,
    tools: &ToolRegistry,
) -> Result<(SmolStr, Vec<crate::tool_loop::PendingToolCall>), (TransportError, bool)> {
    // Every request the turn makes goes through here, including each transient
    // retry, so this is the one place that can be the last look at the flag
    // before bytes leave. It cannot close the window completely: a cancel that
    // lands after `stream` was called hits a request already in flight, and
    // only `RequestCtx::aborted` can cut that connection short.
    if aborted.load(Ordering::SeqCst) {
        return Ok((SmolStr::default(), Vec::new()));
    }
    let mut request = WireRequest::new(model.clone());
    request.messages = messages.to_vec();
    request.tools = tools.specs();
    let context = RequestCtx {
        api_key,
        aborted: Arc::clone(&aborted),
    };
    let mut stream = transport
        .stream(request, context)
        .await
        .map_err(|error| (error, false))?;
    let mut visible_content = false;
    let mut collector = ToolCallCollector::default();
    let mut answer = String::new();

    while let Some(event) = stream.next().await {
        if aborted.load(Ordering::SeqCst) {
            return Ok((SmolStr::default(), Vec::new()));
        }
        visible_content |= event.is_content();
        collector.observe(&event);
        match event {
            StreamEvent::TextDelta { text, .. } => {
                answer.push_str(&text);
                let _ = events
                    .send(EngineEvent::StreamDelta { turn_id, text })
                    .await;
            }
            StreamEvent::ThinkingDelta { text, .. } => {
                let _ = events
                    .send(EngineEvent::ThinkingDelta { turn_id, text })
                    .await;
            }
            StreamEvent::Done { reason } => {
                let calls = collector.take();
                if calls.is_empty() {
                    let _ = events
                        .send(EngineEvent::TurnFinished { turn_id, reason })
                        .await;
                }
                return Ok((answer.into(), calls));
            }
            StreamEvent::Error { reason, message } => {
                let error = if reason == ErrorReason::Connection && !visible_content {
                    TransportError::Retryable {
                        status: None,
                        message,
                    }
                } else {
                    TransportError::Fatal {
                        status: None,
                        message,
                    }
                };
                return Err((error, visible_content));
            }
            _ => {}
        }
    }

    Err((
        TransportError::Retryable {
            status: None,
            message: "stream ended without terminal event".into(),
        },
        visible_content,
    ))
}

async fn emit_transport_failure(
    events: &mpsc::Sender<EngineEvent>,
    turn_id: TurnId,
    error: TransportError,
) {
    let reason = match error {
        TransportError::Fatal { .. } => ErrorReason::Rejected,
        TransportError::Retryable { .. } | TransportError::Stalled { .. } => {
            ErrorReason::Connection
        }
    };
    let _ = events
        .send(EngineEvent::Failed {
            turn_id: Some(turn_id),
            reason,
            message: error.to_string().into(),
        })
        .await;
}
