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
use crate::protocol::{EngineCommand, EngineEvent, TurnId};
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
}

impl EngineConfig {
    pub fn new(primary_model: impl Into<SmolStr>) -> Self {
        Self {
            primary_model: primary_model.into(),
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
        let (done_tx, mut done_rx) = mpsc::channel::<TurnId>(8);
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
                                let system = self.system_prompt().await;
                                  active = Some(self.spawn_turn(text, primary_model.clone(), system, done_tx.clone()));
                              }
                        }
                        EngineCommand::RestoreHistory { messages } => {
                            self.config.restored_messages = messages;
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
                    }
                }
                completed = done_rx.recv() => {
                    if let Some(completed) = completed
                        && active.as_ref().is_some_and(|(id, _)| *id == completed)
                    {
                        active = None;
                        if let Some(text) = queued.pop_front() {
                            let system = self.system_prompt().await;
                          active = Some(self.spawn_turn(text, primary_model.clone(), system, done_tx.clone()));
                        }
                    }
                }
            }
        }
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

    fn spawn_turn(
        &self,
        prompt: SmolStr,
        primary_model: SmolStr,
        system: Option<SmolStr>,
        done: mpsc::Sender<TurnId>,
    ) -> (TurnId, Arc<AtomicBool>) {
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
            run_turn(
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
            let _ = done.send(turn_id).await;
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
        tokio::spawn(async move {
            let outcome = crate::run_goal(coder, reviewer, text).await;
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
) {
    let mut models = Vec::with_capacity(1 + config.fallback_models.len());
    models.push(primary_model);
    models.extend(config.fallback_models);
    let mut previous_model: Option<SmolStr> = None;

    for model in models {
        if aborted.load(Ordering::SeqCst) {
            return;
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
                return;
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
                    Ok(calls) if calls.is_empty() => {
                        completed = true;
                        break;
                    }
                    Ok(calls) => {
                        if tool_rounds >= config.max_tool_rounds {
                            let _ = events
                                .send(EngineEvent::Failed {
                                    turn_id: Some(turn_id),
                                    reason: ErrorReason::Rejected,
                                    message: "tool round cap reached".into(),
                                })
                                .await;
                            return;
                        }
                        tool_rounds += 1;
                        let extra = execute_tools(
                            turn_id,
                            calls,
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
                            return;
                        }
                        if visible_content || !error.is_retryable() {
                            emit_transport_failure(&events, turn_id, error).await;
                            return;
                        }
                        last_error = Some(error);
                    }
                }
            }
            if completed {
                if let Some(recorder) = trajectory.lock().await.as_mut() {
                    let _ = recorder.record(titi_core::trajectory::EventKind::TurnEnd);
                }
                return;
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
) -> Result<Vec<crate::tool_loop::PendingToolCall>, (TransportError, bool)> {
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

    while let Some(event) = stream.next().await {
        if aborted.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        visible_content |= event.is_content();
        collector.observe(&event);
        match event {
            StreamEvent::TextDelta { text, .. } => {
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
                return Ok(calls);
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
