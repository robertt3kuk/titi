use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use smol_str::SmolStr;
use tokio::sync::{Mutex, mpsc, watch};

use futures::StreamExt;
use titi_providers::{ChatMessage, RequestCtx, Role, StreamEvent, WireRequest};

use crate::claims::Claims;
use crate::findings::Findings;
use crate::protocol::{AgentKind, AgentStatus, EngineEvent};
use crate::runtime::TransportResolver;

#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub id: SmolStr,
    pub name: SmolStr,
    pub task: SmolStr,
    pub kind: AgentKind,
    pub parent_id: Option<SmolStr>,
}

/// How a run ended, for a caller that waited on it.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub status: AgentStatus,
    pub summary: SmolStr,
    /// True only for [`AgentStatus::Completed`]. Kept beside the status so a
    /// caller that only wants "did it work" does not match on an enum whose
    /// other arms are about *how* it did not.
    pub success: bool,
}

#[derive(Clone)]
pub struct AgentContext {
    agent_id: SmolStr,
    events: mpsc::Sender<EngineEvent>,
    aborted: Arc<AtomicBool>,
    findings: Findings,
    /// Files this agent may write; released when it stops.
    claims: Claims,
}

impl AgentContext {
    /// A context for a one-shot run the supervisor does not own: progress goes
    /// nowhere, nothing can abort it, and it holds no shared state.
    pub fn detached() -> Self {
        let (events, _receiver) = mpsc::channel(1);
        Self {
            agent_id: SmolStr::new_inline("detached"),
            events,
            aborted: Arc::new(AtomicBool::new(false)),
            findings: Findings::default(),
            claims: Claims::new(),
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The model's own text, as it arrives.
    pub async fn progress(&self, text: impl Into<SmolStr>) {
        let _ = self
            .events
            .send(EngineEvent::AgentProgress {
                agent_id: self.agent_id.clone(),
                text: text.into(),
            })
            .await;
    }

    /// What this agent is *doing* rather than saying: `tools: read, grep`.
    ///
    /// The engine's line, not the model's, and a different event from
    /// [`Self::progress`] for exactly that reason — a surface showing "what is
    /// this agent up to" cannot tell a status from a sentence otherwise.
    pub async fn activity(&self, text: impl Into<SmolStr>) {
        let _ = self
            .events
            .send(EngineEvent::AgentActivity {
                agent_id: self.agent_id.clone(),
                text: text.into(),
            })
            .await;
    }

    /// Records something worth handing back to the parent.
    pub fn finding(&self, text: impl Into<SmolStr>) -> u64 {
        self.findings.push(self.agent_id.clone(), None, text)
    }

    /// Claims a file for this agent's writes.
    pub fn claim(&self, path: &str) -> Result<SmolStr, crate::claims::ClaimError> {
        self.claims.try_claim(path, &self.agent_id)
    }

    /// Releases every file this agent holds.
    pub fn release_claims(&self) -> usize {
        self.claims.release_all(&self.agent_id)
    }
}

#[async_trait]
pub trait AgentRunner: Send + Sync + 'static {
    async fn run(&self, request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr>;
}

#[derive(Clone)]
struct AgentRecord {
    request: AgentRequest,
    status: AgentStatus,
    aborted: Arc<AtomicBool>,
    /// How the run ended, once it has. The sender is kept here so
    /// [`AgentSupervisor::stop`] can resolve a waiter at once instead of
    /// leaving it on a runner that may never come back; the receiver is the
    /// handle [`AgentSupervisor::wait`] hands out.
    outcome_tx: watch::Sender<Option<AgentOutcome>>,
    outcome_rx: watch::Receiver<Option<AgentOutcome>>,
}

#[derive(Clone)]
pub struct AgentSupervisor {
    runner: Arc<dyn AgentRunner>,
    events: mpsc::Sender<EngineEvent>,
    records: Arc<Mutex<HashMap<SmolStr, AgentRecord>>>,
    next_id: Arc<AtomicU64>,
    claims: Claims,
    findings: Findings,
}

impl AgentSupervisor {
    pub fn new(runner: Arc<dyn AgentRunner>, events: mpsc::Sender<EngineEvent>) -> Self {
        Self::with_state(runner, events, Claims::new(), Findings::default())
    }

    /// Shares the runtime's claim table and findings bus, so a subagent's
    /// writes and discoveries land where the parent looks for them.
    pub fn with_state(
        runner: Arc<dyn AgentRunner>,
        events: mpsc::Sender<EngineEvent>,
        claims: Claims,
        findings: Findings,
    ) -> Self {
        Self {
            runner,
            events,
            records: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            claims,
            findings,
        }
    }

    /// The shared findings bus.
    pub fn findings(&self) -> &Findings {
        &self.findings
    }

    /// The shared write-claim table.
    pub fn claims(&self) -> &Claims {
        &self.claims
    }

    pub async fn spawn(&self, name: SmolStr, task: SmolStr, kind: AgentKind) -> SmolStr {
        let id: SmolStr = format!("agent-{}", self.next_id.fetch_add(1, Ordering::SeqCst)).into();
        let request = AgentRequest {
            id: id.clone(),
            name,
            task,
            kind,
            parent_id: Some("Main".into()),
        };
        self.launch(request).await;
        id
    }

    /// Moves the view to `agent_id`.
    ///
    /// Returns false when no such agent exists, so the caller can say so
    /// instead of pretending the view moved.
    pub async fn focus(&self, agent_id: &str) -> bool {
        if !self.records.lock().await.contains_key(agent_id) {
            return false;
        }
        let _ = self
            .events
            .send(EngineEvent::AgentFocused {
                agent_id: Some(agent_id.into()),
            })
            .await;
        true
    }

    pub async fn stop(&self, agent_id: &str) -> bool {
        let mut records = self.records.lock().await;
        let Some(record) = records.get_mut(agent_id) else {
            return false;
        };
        record.aborted.store(true, Ordering::SeqCst);
        record.status = AgentStatus::Aborted;
        // A waiter must not be left on a runner that may be mid-request: the
        // agent is over the moment it is stopped, whatever the model is still
        // doing. The launch task's own send is skipped when it sees the abort
        // flag, so this is the only outcome that waiter gets.
        let _ = record.outcome_tx.send(Some(AgentOutcome {
            status: AgentStatus::Aborted,
            // Empty: the status is what there is to say, and a caller that
            // prints both would say "was stopped" twice.
            summary: SmolStr::default(),
            success: false,
        }));
        drop(records);
        // A stopped agent must not leave its files locked.
        self.claims.release_all(agent_id);
        let _ = self
            .events
            .send(EngineEvent::AgentStatusChanged {
                agent_id: agent_id.into(),
                status: AgentStatus::Aborted,
            })
            .await;
        true
    }

    /// Stops every agent that is still running, for a cancel that reaches the
    /// parent: a turn's children are the turn's, and one that outlives it is
    /// an orphan holding claims.
    pub async fn stop_all(&self) -> usize {
        let running: Vec<SmolStr> = {
            let records = self.records.lock().await;
            records
                .iter()
                .filter(|(_, record)| {
                    matches!(record.status, AgentStatus::Running | AgentStatus::Idle)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in &running {
            self.stop(id).await;
        }
        running.len()
    }

    /// Waits for `agent_id` to end and hands back how it ended.
    ///
    /// `None` when no such agent exists. The wait resolves for every way a run
    /// can end, including [`Self::stop`], so a caller is never left waiting on
    /// an agent that will not finish.
    pub async fn wait(&self, agent_id: &str) -> Option<AgentOutcome> {
        let mut outcome = self.records.lock().await.get(agent_id)?.outcome_rx.clone();
        loop {
            if let Some(done) = outcome.borrow().clone() {
                return Some(done);
            }
            if outcome.changed().await.is_err() {
                return None;
            }
        }
    }

    pub async fn revive(&self, agent_id: &str) -> bool {
        let request = {
            let records = self.records.lock().await;
            let Some(record) = records.get(agent_id) else {
                return false;
            };
            if !matches!(
                record.status,
                AgentStatus::Parked | AgentStatus::Aborted | AgentStatus::Failed
            ) {
                return false;
            }
            record.request.clone()
        };
        self.launch(request).await;
        true
    }

    async fn launch(&self, request: AgentRequest) {
        let aborted = Arc::new(AtomicBool::new(false));
        let (outcome_tx, outcome_rx) = watch::channel(None);
        self.records.lock().await.insert(
            request.id.clone(),
            AgentRecord {
                request: request.clone(),
                status: AgentStatus::Running,
                aborted: Arc::clone(&aborted),
                outcome_tx,
                outcome_rx,
            },
        );
        let _ = self
            .events
            .send(EngineEvent::AgentStarted {
                agent_id: request.id.clone(),
                name: request.name.clone(),
                parent_id: request.parent_id.clone(),
                kind: request.kind,
            })
            .await;

        let supervisor = self.clone();
        let outcome_tx = self
            .records
            .lock()
            .await
            .get(&request.id)
            .map(|record| record.outcome_tx.clone());
        tokio::spawn(async move {
            let context = AgentContext {
                agent_id: request.id.clone(),
                events: supervisor.events.clone(),
                aborted: Arc::clone(&aborted),
                findings: supervisor.findings.clone(),
                claims: supervisor.claims.clone(),
            };
            let result = supervisor.runner.run(request.clone(), context).await;
            if aborted.load(Ordering::SeqCst) {
                // `stop` already resolved the outcome and said what happened;
                // the runner may be reporting from a request it was mid-way
                // through, and that is not the end of this agent.
                return;
            }
            let (status, summary, success) = match result {
                Ok(summary) => (AgentStatus::Completed, summary, true),
                Err(message) => (AgentStatus::Failed, message, false),
            };
            if let Some(record) = supervisor.records.lock().await.get_mut(&request.id) {
                record.status = status;
            }
            // The summary is the last finding the parent needs from this agent.
            supervisor
                .findings
                .push(request.id.clone(), None, summary.clone());
            supervisor.claims.release_all(&request.id);
            let _ = supervisor
                .events
                .send(EngineEvent::AgentStatusChanged {
                    agent_id: request.id.clone(),
                    status,
                })
                .await;
            let _ = supervisor
                .events
                .send(EngineEvent::AgentFinished {
                    agent_id: request.id.clone(),
                    summary: summary.clone(),
                    success,
                })
                .await;
            // Last, so a waiter that wakes has the events already in the
            // stream: `AgentFinished` before the tool call that spawned it
            // returns.
            if let Some(outcome_tx) = outcome_tx {
                let _ = outcome_tx.send(Some(AgentOutcome {
                    status,
                    summary,
                    success,
                }));
            }
        });
    }
}

/// Runs a spawned agent as a one-shot provider turn on the given model.
pub struct StreamingAgentRunner {
    resolver: Arc<dyn TransportResolver>,
    model: SmolStr,
}

impl StreamingAgentRunner {
    pub fn new(resolver: Arc<dyn TransportResolver>, model: impl Into<SmolStr>) -> Self {
        Self {
            resolver,
            model: model.into(),
        }
    }
}

#[async_trait]
impl AgentRunner for StreamingAgentRunner {
    async fn run(&self, request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        let resolved = self
            .resolver
            .resolve(&self.model)
            .map_err(|error| SmolStr::from(error.to_string()))?;
        let mut wire = WireRequest::new(resolved.wire_model.clone());
        wire.messages.push(ChatMessage {
            role: Role::User,
            content: request.task.clone(),
            tool_calls: Vec::new(),
            ..Default::default()
        });
        let aborted = Arc::new(AtomicBool::new(false));
        let ctx = RequestCtx {
            credential: resolved.credential,
            aborted: Arc::clone(&aborted),
        };
        let mut stream = resolved
            .transport
            .stream(wire, ctx)
            .await
            .map_err(|error| SmolStr::from(error.to_string()))?;
        let mut summary = String::new();
        while let Some(event) = stream.next().await {
            if context.is_aborted() {
                aborted.store(true, Ordering::SeqCst);
                return Err("aborted".into());
            }
            match event {
                StreamEvent::TextDelta { text, .. } | StreamEvent::ThinkingDelta { text, .. } => {
                    summary.push_str(&text);
                    context.progress(text).await;
                }
                StreamEvent::Error { message, .. } => return Err(message),
                StreamEvent::Done { .. } => break,
                _ => {}
            }
        }
        if summary.is_empty() {
            Ok(format!("{} complete", request.name).into())
        } else {
            Ok(summary.into())
        }
    }
}
