//! The `agent` tool: the model handing a task to a subagent.
//!
//! This is the model's door to the same [`AgentSupervisor`] the surface's
//! `SpawnAgent` command opens, so there is one spawn path: the tool does not
//! start a runner of its own, it asks the supervisor and then waits on the
//! outcome the supervisor already knows about.
//!
//! The call **blocks**. The parent's next request carries the subagent's
//! answer, which is what makes the hand-off a delegation rather than a
//! fire-and-forget — and it is why the answer is bounded: it becomes a tool
//! message in the parent's context, so it is capped at
//! [`MAX_ANSWER_CHARS`] with the cut stated in the text.
//!
//! Depth is bounded by the tool list, not by a counter: the runtime builds a
//! subagent's registry without this tool (see `EngineRuntime::start_inner`),
//! so a subagent cannot spawn a subagent, at any depth. omp's `task` tool
//! allows two levels by default; titi does not, because the subagent runner
//! has a round cap but no nesting budget and a tree of agents with no ceiling
//! is not something this engine can account for.
//!
//! A subagent cannot `ask`: it has no surface to put a question on, and the
//! engine refuses the call rather than parking a turn nobody can answer. The
//! same reasoning sets this tool's tier — see [`AgentTool::new`].

use async_trait::async_trait;
use serde_json::{Value, json};
use smol_str::SmolStr;
use titi_providers::ToolSpec;
use titi_tools::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

use crate::agents::AgentSupervisor;
use crate::protocol::{AgentKind, AgentStatus};

/// Characters of a subagent's answer the parent model is given.
///
/// The answer becomes a tool message in the parent's context, so it is bounded
/// like any other tool output; the cut is stated in the text rather than left
/// for the parent to notice. A subagent with more to say than this is meant to
/// write it to a file and say where, which is the same advice the truncation
/// line gives.
pub const MAX_ANSWER_CHARS: usize = 30_000;

/// The model's door to the agent supervisor.
pub struct AgentTool {
    agents: AgentSupervisor,
    /// Whether a spawned subagent can reach a tool that writes or executes.
    ///
    /// The subagent's registry and approval mode are decided once, by the
    /// runtime, when it builds the runner: either it is filtered to read-tier
    /// tools, or it runs with everything the workspace has and `Yolo`. A
    /// subagent has no surface to show an approval on, so nothing in between
    /// is possible — which means the *spawn* is the only place a user can be
    /// asked about what a subagent will do. This tool's tier says so: `Exec`
    /// when the child may execute, `Read` when it cannot do more than read.
    child_may_exec: bool,
}

impl AgentTool {
    pub fn new(agents: AgentSupervisor, child_may_exec: bool) -> Self {
        Self {
            agents,
            child_may_exec,
        }
    }
}

/// Stops a subagent whose call was dropped before it finished.
///
/// A cancelled turn is handled by the runtime, which stops every running agent
/// as part of the cancel (see `EngineRuntime`'s `Cancel` arm). This covers the
/// other way a call can end: its future being dropped — the engine going away
/// mid-call, or a panic unwinding through the loop — where nothing else would
/// tell the subagent to stop. `Drop` cannot await, so the stop is spawned.
struct StopOnDrop {
    agents: AgentSupervisor,
    agent_id: SmolStr,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let agents = self.agents.clone();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            agents.stop(&agent_id).await;
        });
    }
}

#[async_trait]
impl ToolHandler for AgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "agent".into(),
                description: "Hand one task to a subagent and wait for its answer. The \
                              subagent has its own context and its own tool loop, and \
                              reports back with what it found; the call returns when it \
                              finishes, so the next thing you say can rely on its answer. \
                              Use it for work that would flood this context — searching a \
                              tree, reading many files, a self-contained edit — not for \
                              something one tool call already answers. A subagent cannot \
                              ask you anything and cannot spawn subagents of its own."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "What the subagent is to do, in full. It cannot \
                                            see this conversation, so name the files, the \
                                            goal and what to report back."
                        },
                        "name": {
                            "type": "string",
                            "description": "Short label for the transcript, e.g. `Scout`. \
                                            Defaults to `Subagent`."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["subagent", "advisor"],
                            "description": "`subagent` (default) does the work; `advisor` \
                                            is for a second opinion on a plan."
                        }
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
            },
            approval: if self.child_may_exec {
                ApprovalTier::Exec
            } else {
                ApprovalTier::Read
            },
        }
    }

    fn describe(&self, args: &Value) -> Option<String> {
        let task = args.get("task")?.as_str()?.trim();
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Subagent");
        Some(format!("{name}: {}", head(task, 60)))
    }

    fn refusal(&self, args: &Value) -> Option<String> {
        if task_of(args).is_none() {
            return Some("agent needs a `task`: what the subagent is to do".into());
        }
        None
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(task) = task_of(&args) else {
            return ToolResult {
                output: "agent needs a `task`: what the subagent is to do".into(),
                is_error: true,
                detail: None,
            };
        };
        let name: SmolStr = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("Subagent")
            .into();
        let kind = match args.get("kind").and_then(Value::as_str) {
            Some("advisor") => AgentKind::Advisor,
            _ => AgentKind::Subagent,
        };

        // The one spawn path: the supervisor the surface's command uses too.
        let agent_id = self.agents.spawn(name.clone(), task.into(), kind).await;
        let guard = StopOnDrop {
            agents: self.agents.clone(),
            agent_id: agent_id.clone(),
            armed: true,
        };
        let outcome = self.agents.wait(&agent_id).await;
        // Reaching here means the wait resolved, not that the future survived.
        let mut guard = guard;
        guard.armed = false;
        drop(guard);

        match outcome {
            Some(outcome) if outcome.success => ToolResult {
                output: bounded(&outcome.summary),
                is_error: false,
                detail: None,
            },
            Some(outcome) => ToolResult {
                output: format!(
                    "agent {name} {}: {}",
                    word(outcome.status),
                    bounded(&outcome.summary)
                )
                .into(),
                is_error: true,
                detail: None,
            },
            None => ToolResult {
                output: format!("agent {agent_id} is no longer known to the supervisor").into(),
                is_error: true,
                detail: None,
            },
        }
    }
}

/// The `task` argument, trimmed, when it says something.
fn task_of(args: &Value) -> Option<&str> {
    args.get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
}

/// The status as a word for a sentence the parent model reads.
fn word(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::Aborted => "was stopped",
        AgentStatus::Running | AgentStatus::Idle | AgentStatus::Parked => "did not finish",
    }
}

/// The answer, capped, with the cut stated.
fn bounded(summary: &str) -> SmolStr {
    let total = summary.chars().count();
    if total <= MAX_ANSWER_CHARS {
        return summary.into();
    }
    let head: String = summary.chars().take(MAX_ANSWER_CHARS).collect();
    format!(
        "{head}\n[answer truncated: {MAX_ANSWER_CHARS} of {total} characters; the subagent \
         should write the rest to a file and name it]"
    )
    .into()
}

/// The first `chars` characters of `text`, on one line.
fn head(text: &str, chars: usize) -> String {
    let mut head: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        head.push('…');
    }
    head
}
