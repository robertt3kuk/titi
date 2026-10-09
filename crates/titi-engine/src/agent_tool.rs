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
//! One call may also run a **batch**: `tasks: [...]`, with an optional
//! `context` prepended to each child's task. Exactly one of `task` and `tasks`
//! is accepted. A batch runs its children at once, up to
//! [`BATCH_CONCURRENCY`] in flight, and answers with one section per child in
//! the order they were asked for. A child that fails does **not** abort its
//! siblings: omp's batch runs through `mapWithConcurrencyLimitAllSettled`
//! (`task/parallel.ts`, called from `task/index.ts`), whose contract is
//! "rejections are captured at their input position and already launched
//! siblings always settle", and this follows it.
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

use crate::agents::{AgentOutcome, AgentSupervisor};
use crate::protocol::{AgentKind, AgentStatus};

/// Characters of a subagent's answer the parent model is given.
///
/// The answer becomes a tool message in the parent's context, so it is bounded
/// like any other tool output; the cut is stated in the text rather than left
/// for the parent to notice. A subagent with more to say than this is meant to
/// write it to a file and say where, which is the same advice the truncation
/// line gives.
pub const MAX_ANSWER_CHARS: usize = 30_000;

/// How many children of one batch are in flight at a time.
///
/// A constant rather than a settings key: the keys beside it (`agentRounds`,
/// `agentWrites`) are read by the CLI's settings plumbing, which is not mine
/// this turn, and a knob nobody can set is worse than a number with a reason.
/// Four is large enough that the common batch runs in one wave and small
/// enough that a batch cannot flood a provider with concurrent requests.
pub const BATCH_CONCURRENCY: usize = 4;

/// The most children one batch may name.
///
/// A batch is paid for by the caller in one tool message and its children are
/// paid for in provider requests, so it is bounded like everything else here.
pub const MAX_BATCH_TASKS: usize = 32;

/// Room reserved in a batch message for one truncation note per child, so a
/// capped answer cannot push the whole message past [`MAX_ANSWER_CHARS`].
const TRUNCATION_NOTE_CHARS: usize = 120;

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

/// Stops the subagents of a call whose future was dropped before they
/// finished — one child for `task`, all of them for a batch.
///
/// A cancelled turn is handled by the runtime, which stops every running agent
/// as part of the cancel (see `EngineRuntime`'s `Cancel` arm). This covers the
/// other way a call can end: its future being dropped — the engine going away
/// mid-call, or a panic unwinding through the loop — where nothing else would
/// tell the children to stop. `Drop` cannot await, so each stop is spawned.
struct StopChildren {
    agents: AgentSupervisor,
    ids: Vec<SmolStr>,
    armed: bool,
}

impl Drop for StopChildren {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for id in &self.ids {
            let agents = self.agents.clone();
            let id = id.clone();
            tokio::spawn(async move {
                agents.stop(&id).await;
            });
        }
    }
}

#[async_trait]
impl ToolHandler for AgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "agent".into(),
                description: "Hand one task to a subagent and wait for its answer, or \
                              hand several at once with `tasks`. A subagent has its own \
                              context and its own tool loop, and reports back with what it \
                              found; the call returns when it finishes, so the next thing \
                              you say can rely on its answer. Use it for work that would \
                              flood this context — searching a tree, reading many files, a \
                              self-contained edit — not for something one tool call already \
                              answers. To run several agents at once, pass them in one \
                              call as `tasks`: separate `agent` calls in one response run \
                              one after another, and a batch runs its children together \
                              (at most four in flight) and answers with one section per \
                              child, so use it when the pieces are independent of each \
                              other. A subagent cannot \
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
                        },
                        "tasks": {
                            "type": "array",
                            "description": "A batch instead of one `task`: each entry is \
                                            `{task, name?, kind?}`. Exactly one of `task` \
                                            and `tasks` is required. The children run at \
                                            once and the answer carries a section per child.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "task": { "type": "string" },
                                    "name": { "type": "string" },
                                    "kind": { "type": "string", "enum": ["subagent", "advisor"] }
                                },
                                "required": ["task"],
                                "additionalProperties": false
                            }
                        },
                        "context": {
                            "type": "string",
                            "description": "A shared briefing for a batch, prepended to \
                                            every task in `tasks`. It is the place to put \
                                            the goal and the interfaces the children share, \
                                            so each `task` need only say its own piece."
                        }
                    },
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
        if let Some(list) = args.get("tasks").and_then(Value::as_array) {
            return Some(format!(
                "{} tasks: {}",
                list.len(),
                head(shared_context(args).unwrap_or_default(), 40)
            ));
        }
        let task = args.get("task")?.as_str()?.trim();
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Subagent");
        Some(format!("{name}: {}", head(task, 60)))
    }

    fn refusal(&self, args: &Value) -> Option<String> {
        shape_of(args).err()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let shape = match shape_of(&args) {
            Ok(shape) => shape,
            Err(why) => {
                return ToolResult {
                    output: why.into(),
                    is_error: true,
                    detail: None,
                };
            }
        };
        match shape {
            Shape::Single(item) => self.run_single(item).await,
            Shape::Batch { context, items } => self.run_batch(context, items).await,
        }
    }
}

impl AgentTool {
    /// One child, waited on to the end.
    async fn run_single(&self, item: Item) -> ToolResult {
        // The one spawn path: the supervisor the surface's command uses too.
        let agent_id = self
            .agents
            .spawn(item.name.clone(), item.prompt(None), item.kind)
            .await;
        let mut guard = StopChildren {
            agents: self.agents.clone(),
            ids: vec![agent_id.clone()],
            armed: true,
        };
        let outcome = self.agents.wait(&agent_id).await;
        guard.armed = false;

        match outcome {
            Some(outcome) => ToolResult {
                output: format!(
                    "agent {} {}: {}",
                    item.name,
                    word(Some(&outcome)),
                    capped(&outcome.summary, MAX_ANSWER_CHARS)
                )
                .into(),
                is_error: !outcome.success,
                detail: None,
            },
            None => ToolResult {
                output: format!("agent {agent_id} is no longer known to the supervisor").into(),
                is_error: true,
                detail: None,
            },
        }
    }

    /// A batch, in waves of [`BATCH_CONCURRENCY`], all of it waited on.
    ///
    /// A child that fails does not stop its siblings — see the module docs;
    /// the wave's waits all resolve before the next wave is spawned, so a
    /// failure is a section in the answer, not an abort.
    async fn run_batch(&self, context: Option<String>, items: Vec<Item>) -> ToolResult {
        let mut guard = StopChildren {
            agents: self.agents.clone(),
            ids: Vec::new(),
            armed: true,
        };
        let mut outcomes: Vec<Option<AgentOutcome>> = Vec::with_capacity(items.len());
        for wave in items.chunks(BATCH_CONCURRENCY) {
            let mut ids: Vec<SmolStr> = Vec::with_capacity(wave.len());
            for item in wave {
                let id = self
                    .agents
                    .spawn(
                        item.name.clone(),
                        item.prompt(context.as_deref()),
                        item.kind,
                    )
                    .await;
                guard.ids.push(id.clone());
                ids.push(id);
            }
            let done = futures::future::join_all(ids.iter().map(|id| self.agents.wait(id))).await;
            outcomes.extend(done);
        }
        guard.armed = false;

        let failed = outcomes
            .iter()
            .any(|outcome| !outcome.as_ref().is_some_and(|outcome| outcome.success));
        ToolResult {
            output: sections(&items, &outcomes),
            is_error: failed,
            detail: None,
        }
    }
}

/// What a call asked for, once its arguments have been read.
enum Shape {
    Single(Item),
    Batch {
        context: Option<String>,
        items: Vec<Item>,
    },
}

/// One child's brief.
struct Item {
    task: String,
    name: SmolStr,
    kind: AgentKind,
}

impl Item {
    /// The task as the child reads it: a batch's shared context first, so each
    /// `task` need only say its own piece.
    fn prompt(&self, context: Option<&str>) -> SmolStr {
        match context {
            Some(context) if !context.trim().is_empty() => {
                format!("{context}\n\n{}", self.task).into()
            }
            _ => self.task.as_str().into(),
        }
    }
}

/// Reads the arguments into one of the two shapes, or the sentence that
/// refuses them. Exactly one of `task` and `tasks` is accepted.
fn shape_of(args: &Value) -> Result<Shape, String> {
    let single = args.get("task").is_some();
    let batch = args.get("tasks").is_some();
    match (single, batch) {
        (true, true) => {
            Err("agent takes either `task` (one subagent) or `tasks` (a batch), not both".into())
        }
        (false, false) => Err("agent needs a `task`, or a `tasks` list to run as a batch".into()),
        (true, false) => Ok(Shape::Single(item_of(args, None)?)),
        (false, true) => {
            let list = args
                .get("tasks")
                .and_then(Value::as_array)
                .ok_or_else(|| "agent: `tasks` must be a list of tasks".to_string())?;
            if list.is_empty() {
                return Err("agent: `tasks` is empty; give it at least one task".into());
            }
            if list.len() > MAX_BATCH_TASKS {
                return Err(format!(
                    "agent: {} tasks is more than the {MAX_BATCH_TASKS} one batch may run",
                    list.len()
                ));
            }
            let mut items = Vec::with_capacity(list.len());
            for (index, entry) in list.iter().enumerate() {
                items.push(item_of(entry, Some(index))?);
            }
            Ok(Shape::Batch {
                context: shared_context(args).map(str::to_owned),
                items,
            })
        }
    }
}

/// One entry's task, name and kind. `index` names the position in a refusal
/// and numbers a child the caller did not name.
fn item_of(args: &Value, index: Option<usize>) -> Result<Item, String> {
    let which = match index {
        Some(index) => format!("task {}", index + 1),
        None => "the task".to_owned(),
    };
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| format!("agent: {which} needs a `task`: what the subagent is to do"))?;
    let name: SmolStr = match args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => name.into(),
        None => match index {
            Some(index) => format!("Subagent {}", index + 1).into(),
            None => "Subagent".into(),
        },
    };
    let kind = match args.get("kind").and_then(Value::as_str) {
        Some("advisor") => AgentKind::Advisor,
        _ => AgentKind::Subagent,
    };
    Ok(Item {
        task: task.to_owned(),
        name,
        kind,
    })
}

/// The batch's shared briefing, when it says something.
fn shared_context(args: &Value) -> Option<&str> {
    args.get("context")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|context| !context.is_empty())
}

/// One section per child, in the order they were asked for, each answer capped
/// so the whole message fits [`MAX_ANSWER_CHARS`].
///
/// The rule: what is left of the budget after the section headers and one
/// truncation note per child is shared equally, so no child can push the
/// message past the cap and no section is cut by the message's own total.
fn sections(items: &[Item], outcomes: &[Option<AgentOutcome>]) -> SmolStr {
    let headers: Vec<String> = items
        .iter()
        .zip(outcomes)
        .map(|(item, outcome)| format!("{} — {}", item.name, word(outcome.as_ref())))
        .collect();
    let header_chars: usize = headers
        .iter()
        .map(|header| header.chars().count() + 1)
        .sum();
    let share = MAX_ANSWER_CHARS
        .saturating_sub(header_chars + items.len() * TRUNCATION_NOTE_CHARS)
        .checked_div(items.len())
        .unwrap_or(0)
        .max(1);

    let mut out = String::new();
    for (outcome, header) in outcomes.iter().zip(&headers) {
        out.push_str(header);
        out.push('\n');
        let answer = outcome
            .as_ref()
            .map(|outcome| outcome.summary.as_str())
            .unwrap_or_default();
        out.push_str(&capped(answer, share));
        out.push('\n');
    }
    out.trim_end().into()
}

/// How a child ended, as a word for a sentence the parent model reads. `None`
/// is a child the supervisor no longer knows about.
fn word(outcome: Option<&AgentOutcome>) -> &'static str {
    match outcome {
        Some(outcome) if outcome.success => "completed",
        Some(outcome) => match outcome.status {
            AgentStatus::Failed => "failed",
            AgentStatus::Aborted => "was stopped",
            AgentStatus::Completed => "completed",
            AgentStatus::Running | AgentStatus::Idle | AgentStatus::Parked => "did not finish",
        },
        None => "is no longer known to the supervisor",
    }
}

/// The answer, capped at `limit` characters, with the cut stated.
fn capped(summary: &str, limit: usize) -> SmolStr {
    let total = summary.chars().count();
    if total <= limit {
        return summary.into();
    }
    let head: String = summary.chars().take(limit).collect();
    format!(
        "{head}\n[answer truncated: {limit} of {total} characters; the subagent should \
         write the rest to a file and name it]"
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
