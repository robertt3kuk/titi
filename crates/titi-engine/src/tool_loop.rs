use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smol_str::SmolStr;
use titi_genome::SharedGenome;
use titi_providers::{BlockId, ChatMessage, Role, StreamEvent, ToolCallRef};
use titi_tools::{ApprovalMode, ApprovalTier, ToolHandler, ToolRegistry, ToolResult};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::claims::{ClaimError, Claims};
use crate::protocol::{EngineEvent, TurnId};

#[derive(Debug, Clone)]
pub(crate) struct PendingToolCall {
    pub call_id: SmolStr,
    pub name: SmolStr,
    pub arguments: String,
    /// Gemini's `thoughtSignature` for the part this call came on, echoed back
    /// with the call when the turn is replayed.
    pub thought_signature: SmolStr,
}

/// Gathers a response's tool calls as the provider streams them.
///
/// One slot per call, in the order the provider opened them, each keyed by the
/// block id its deltas carry: a response may hold several calls and their
/// deltas interleave. A single "current" slot — which is what this was — gave
/// the first call's arguments to the second and then dropped one of the two
/// entirely, so a model that asked for two files had one of them run.
#[derive(Default)]
pub(crate) struct ToolCallCollector {
    open: Vec<(BlockId, PendingToolCall)>,
    /// The response's thinking blocks, in order. A signed turn has to replay
    /// them with the call it made; the text is plain so a trace can read it.
    thinking: Vec<titi_providers::ThinkingBlock>,
}

impl ToolCallCollector {
    pub fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::ToolcallStart { id, call } => {
                self.open.push((
                    id.clone(),
                    PendingToolCall {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: String::new(),
                        thought_signature: call.thought_signature.clone(),
                    },
                ));
            }
            StreamEvent::ToolcallDelta { id, json } => {
                if let Some((_, pending)) = self.open.iter_mut().find(|(block, _)| block == id) {
                    pending.arguments.push_str(json);
                }
            }
            StreamEvent::ThinkingBlock { block } => self.thinking.push(block.clone()),
            // A call is done when the turn is: `take` hands back every slot,
            // opened and closed alike, in the order they were opened.
            StreamEvent::ToolcallEnd { .. } => {}
            _ => {}
        }
    }

    /// The response's calls, in the order the provider opened them.
    pub fn take(&mut self) -> Vec<PendingToolCall> {
        self.open.drain(..).map(|(_, pending)| pending).collect()
    }

    /// The response's thinking blocks, in order.
    pub fn thinking(&self) -> &[titi_providers::ThinkingBlock] {
        &self.thinking
    }
}

pub(crate) type ApprovalWaiters = Arc<Mutex<HashMap<SmolStr, oneshot::Sender<bool>>>>;
pub type TrajectorySink = Arc<Mutex<Option<titi_core::trajectory::TrajectoryRecorder>>>;

/// How many touched files the Genome boost tracks. Past this every file in a
/// long session is "touched" and the boost stops discriminating.
pub const TOUCHED_CAPACITY: usize = 64;

/// The files this session read or edited, most recent last, deduplicated.
///
/// Bounded on purpose: an unbounded set turns the ×3 rank boost into a
/// constant once a session has touched everything.
#[derive(Debug, Default)]
pub struct TouchedSet {
    order: Vec<String>,
}

impl TouchedSet {
    /// Records a path, refreshing its recency. Already-known paths move to the
    /// front of the queue rather than duplicating.
    pub fn insert(&mut self, path: String) {
        if let Some(at) = self.order.iter().position(|known| *known == path) {
            self.order.remove(at);
        }
        self.order.push(path);
        if self.order.len() > TOUCHED_CAPACITY {
            self.order.remove(0);
        }
    }

    /// The tracked paths, least recently touched first.
    pub fn snapshot(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Workspace paths the session read or edited; the Genome boosts them.
pub type TouchedSink = Arc<Mutex<TouchedSet>>;

/// Tools whose `path` argument counts as "touched by this session".
pub const TOUCHING_TOOLS: &[&str] = &["read", "write", "edit"];

/// Tools that mutate a file, so they take an exclusive claim for the call.
const WRITING_TOOLS: &[&str] = &["write", "edit"];

/// How many read calls of one response run at the same time.
///
/// A response that asks for five files is five independent reads; running them
/// one after another spends the whole round trip on I/O that could overlap.
/// Four at a time is the same bound the `agent` tool's batches use, and small
/// enough that a long response cannot swamp the machine the turn shares.
pub const READ_GROUP_LIMIT: usize = 4;

/// Tools that may not share a group whatever their tier says.
///
/// Both block on something outside the engine — `ask` on the user, `agent` on
/// its children — and two of them in flight would put two questions, or two
/// waits, in front of one surface at once. Named here for the same reason
/// [`TOUCHING_TOOLS`] is: the declaration a tool could carry for this lives in
/// `titi-tools`, which this turn does not own. Note that `ask`'s tier is
/// `Read`, so the tier rule alone would have grouped it.
const BLOCKING_TOOLS: &[&str] = &["ask", "agent"];

/// Whether this call can run beside the ones next to it.
///
/// Read tier and no approval needed, so nothing waits on a person, and not one
/// of [`BLOCKING_TOOLS`]. An unknown name answers `Exec` from
/// [`ToolRegistry::approval_tier`] and is therefore never grouped — it errors
/// out on its own, in order.
fn may_group(call: &PendingToolCall, tools: &ToolRegistry, approval_mode: ApprovalMode) -> bool {
    // A tool that writes is never grouped, whatever its tier says: the genome
    // fold and the file claim below key on the name, and a write that ran
    // beside another call would race both.
    if BLOCKING_TOOLS.contains(&call.name.as_str()) || WRITING_TOOLS.contains(&call.name.as_str()) {
        return false;
    }
    let tier = tools.approval_tier(&call.name);
    tier == ApprovalTier::Read && approval_mode.auto_approves(tier)
}

/// One call's work that has to happen in the calls' own order, before any of
/// them runs.
struct Prepared {
    call: PendingToolCall,
    args: serde_json::Value,
    /// The path this call takes an exclusive claim on, when it writes.
    write_path: Option<String>,
    claimed: Option<Result<SmolStr, ClaimError>>,
    started: std::time::Instant,
}

/// Announces a call, records it, and takes its claim — in order.
///
/// This is everything a surface sees *before* a call runs: `ToolStarted`, the
/// touched set, the trajectory's `ToolCall`. For a group these all happen
/// before any of the group's calls is invoked, so a surface reads them in the
/// order the model asked for them.
#[allow(clippy::too_many_arguments)]
async fn prepare(
    turn_id: TurnId,
    call: PendingToolCall,
    tools: &ToolRegistry,
    touched: &TouchedSink,
    trajectory: &TrajectorySink,
    claims: &Claims,
    agent_id: &SmolStr,
    events: &mpsc::Sender<EngineEvent>,
    mask_ips: bool,
) -> Prepared {
    let args = serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    let detail = tools
        .get(&call.name)
        .and_then(|handler| handler.describe(&args))
        .map(|detail| mask(&detail, mask_ips).into());
    let _ = events
        .send(EngineEvent::ToolStarted {
            turn_id,
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            detail,
        })
        .await;
    if TOUCHING_TOOLS.contains(&call.name.as_str())
        && let Some(path) = args.get("path").and_then(|value| value.as_str())
    {
        touched.lock().await.insert(path.to_owned());
    }
    if let Some(recorder) = trajectory.lock().await.as_mut() {
        // The trajectory writes the arguments to disk and keeps them for
        // review, so they are masked here: this is the one place a
        // `ToolCall` is recorded, and `titi-core`, which owns the writer,
        // sits below `titi-memory` and cannot reach the redactor itself.
        let _ = recorder.record(titi_core::trajectory::EventKind::ToolCall {
            id: call.call_id.to_string(),
            name: call.name.to_string(),
            args: mask_args(&args, mask_ips),
        });
    }
    // A write-tier call takes an exclusive claim on its file, so a parallel
    // agent cannot edit the same path underneath it.
    let write_path: Option<String> = args
        .get("path")
        .and_then(|value| value.as_str())
        .filter(|_| WRITING_TOOLS.contains(&call.name.as_str()))
        .map(str::to_owned);
    let claimed = write_path
        .as_deref()
        .map(|path| claims.try_claim(path, agent_id));
    Prepared {
        call,
        args,
        write_path,
        claimed,
        started: std::time::Instant::now(),
    }
}

/// Runs a group's calls at once and answers them in the calls' own order.
///
/// A group is read-tier and approval-free by construction ([`may_group`]), so
/// each call can be spawned: a `read` does its I/O on the calling thread inside
/// its own `invoke`, which means polling the group's futures in one task would
/// run them one after another and buy nothing. The handles are joined in order,
/// so the results — and therefore the tool messages the model reads — come back
/// exactly as they were asked for.
async fn run_group(
    prepared: &[Prepared],
    tools: &ToolRegistry,
    aborted: &AtomicBool,
) -> Vec<Executed> {
    let ids: Vec<SmolStr> = prepared
        .iter()
        .map(|item| item.call.call_id.clone())
        .collect();
    let handles: Vec<tokio::task::JoinHandle<Executed>> = prepared
        .iter()
        .map(|item| {
            let call_id = item.call.call_id.clone();
            let handler = tools.get(&item.call.name);
            let args = item.args.clone();
            tokio::spawn(async move { invoke_group(handler, args, call_id).await })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for (index, handle) in handles.into_iter().enumerate() {
        // A cancel stops the *wait*, not the read: a spawned call finishes on
        // its own and its answer is dropped. Every call the group did not get
        // to, this call included, still gets an answer, because a model that
        // asked for five reads must not be left with four results and one
        // dangling tool call.
        let joined = tokio::select! {
            result = handle => result,
            _ = wait_aborted(aborted) => {
                out.extend(ids[index..].iter().map(aborted_answer));
                return out;
            }
        };
        out.push(match joined {
            Ok(executed) => executed,
            // A tool that panicked inside its own task: the turn reports it as
            // the call's failure rather than unwinding the whole turn.
            Err(why) => Executed {
                call_id: ids[index].clone(),
                output: format!("the tool's task did not finish: {why}").into(),
                is_error: true,
                detail: None,
            },
        });
    }
    out
}

/// The answer a grouped call gets when the turn was cancelled before it ran.
fn aborted_answer(call_id: &SmolStr) -> Executed {
    Executed {
        call_id: call_id.clone(),
        output: "the turn was cancelled before this call ran".into(),
        is_error: true,
        detail: None,
    }
}

/// One grouped call's answer: the tool's refusal, or its result.
///
/// No tier check and no approval — [`may_group`] decided both before the call
/// was grouped — and no abort handle, which is why this can be spawned.
async fn invoke_group(
    handler: Option<Arc<dyn ToolHandler>>,
    args: serde_json::Value,
    call_id: SmolStr,
) -> Executed {
    let Some(handler) = handler else {
        return Executed {
            call_id,
            output: "unknown tool".into(),
            is_error: true,
            detail: None,
        };
    };
    if let Some(refusal) = handler.refusal(&args) {
        return Executed {
            call_id,
            output: refusal.into(),
            is_error: true,
            detail: None,
        };
    }
    let ToolResult {
        output,
        is_error,
        detail,
    } = handler.invoke(args).await;
    Executed {
        call_id,
        output,
        is_error,
        detail,
    }
}

/// Masks a call's answer, records it, and hands it to the model — in order.
async fn finish(
    turn_id: TurnId,
    prepared: Prepared,
    executed: Executed,
    messages: &mut Vec<ChatMessage>,
    trajectory: &TrajectorySink,
    events: &mpsc::Sender<EngineEvent>,
    mask_ips: bool,
) {
    // The answer goes to the provider, the transcript and the session file;
    // a key or a server address the tool printed stops here. The detail is
    // masked with it — a diff quotes what the tool wrote — and goes to the
    // event alone: the model never reads a presentation detail, so it can
    // never spend the context or answer for the tool.
    let result = Executed {
        output: cap_output(&mask(&executed.output, mask_ips)).into(),
        detail: executed
            .detail
            .as_deref()
            .map(|detail| mask(detail, mask_ips).into()),
        ..executed
    };
    if let Some(recorder) = trajectory.lock().await.as_mut() {
        let _ = recorder.record(titi_core::trajectory::EventKind::ToolResult {
            id: result.call_id.to_string(),
            duration_ms: prepared.started.elapsed().as_millis() as u64,
            ok: !result.is_error,
        });
    }
    let _ = events
        .send(EngineEvent::ToolFinished {
            turn_id,
            call_id: result.call_id.clone(),
            output: result.output.clone(),
            is_error: result.is_error,
            detail: result.detail.clone(),
        })
        .await;
    messages.push(ChatMessage {
        role: Role::Tool,
        content: result.output,
        // The call this result answers, so a wire builder never has to guess
        // by order.
        tool_call_id: Some(result.call_id.clone()),
        ..Default::default()
    });
}

pub(crate) async fn execute_tools(
    turn_id: TurnId,
    calls: Vec<PendingToolCall>,
    // What the assistant said in the same message as these calls.
    assistant_text: SmolStr,
    // The thinking blocks that message produced, in order, so the turn can be
    // replayed to the family that signed them.
    thinking: Vec<titi_providers::ThinkingBlock>,
    tools: &ToolRegistry,
    approval_mode: ApprovalMode,
    waiters: &ApprovalWaiters,
    events: &mpsc::Sender<EngineEvent>,
    aborted: &AtomicBool,
    trajectory: &TrajectorySink,
    touched: &TouchedSink,
    // The session's live index, when it has one: a mutating tool call folds
    // the path it wrote into it before returning. `None` for a caller with no
    // index of its own — a subagent — which is the fallback walk's business.
    genome: Option<&SharedGenome>,
    claims: &Claims,
    agent_id: &SmolStr,
    mask_ips: bool,
) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut assistant_calls = Vec::new();
    for call in &calls {
        assistant_calls.push(ToolCallRef {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            // The arguments the model asked with, as they were streamed: a
            // replayed history has to show the model what it actually asked
            // for, not an empty string.
            arguments: call.arguments.as_str().into(),
            thought_signature: call.thought_signature.clone(),
            ..Default::default()
        });
    }
    messages.push(ChatMessage {
        role: Role::Assistant,
        content: assistant_text,
        tool_calls: assistant_calls,
        thinking,
        ..Default::default()
    });

    // Calls are grouped: a run of read-tier, approval-free ones goes at once,
    // and anything else — a write, an exec, a call that needs an approval, one
    // of the blocking tools — is a barrier that ends the run and runs alone.
    // The results are always handed back in the order the model asked.
    let mut queue = calls.into_iter().peekable();
    while let Some(call) = queue.next() {
        if aborted.load(Ordering::SeqCst) {
            break;
        }
        if may_group(&call, tools, approval_mode) {
            let mut group = vec![call];
            while group.len() < READ_GROUP_LIMIT
                && queue
                    .peek()
                    .is_some_and(|next| may_group(next, tools, approval_mode))
            {
                if let Some(next) = queue.next() {
                    group.push(next);
                }
            }
            let mut prepared = Vec::with_capacity(group.len());
            for call in group {
                prepared.push(
                    prepare(
                        turn_id, call, tools, touched, trajectory, claims, agent_id, events,
                        mask_ips,
                    )
                    .await,
                );
            }
            let executed = run_group(&prepared, tools, aborted).await;
            for (prepared, executed) in prepared.into_iter().zip(executed) {
                finish(
                    turn_id,
                    prepared,
                    executed,
                    &mut messages,
                    trajectory,
                    events,
                    mask_ips,
                )
                .await;
            }
            continue;
        }

        let prepared = prepare(
            turn_id, call, tools, touched, trajectory, claims, agent_id, events, mask_ips,
        )
        .await;
        let result = match &prepared.claimed {
            Some(Err(error)) => Executed {
                call_id: prepared.call.call_id.clone(),
                output: error.to_string().into(),
                is_error: true,
                detail: None,
            },
            Some(Ok(path)) => {
                let result = invoke_one(
                    turn_id,
                    &prepared,
                    tools,
                    approval_mode,
                    waiters,
                    aborted,
                    events,
                )
                .await;
                claims.release(path, agent_id);
                result
            }
            None => {
                invoke_one(
                    turn_id,
                    &prepared,
                    tools,
                    approval_mode,
                    waiters,
                    aborted,
                    events,
                )
                .await
            }
        };
        // The index hears about a write from the call that made it, not from
        // the next turn's walk. This runs before the tool returns, so the graph
        // is current by the time anything reads it; the cost is a `stat`, a
        // read and a parse of the one file, and a read never pays it. A call
        // that failed or was refused changed nothing on disk, so it is skipped.
        if !result.is_error
            && let (Some(genome), Some(path)) = (genome.cloned(), prepared.write_path.clone())
        {
            let _ = tokio::task::spawn_blocking(move || genome.apply_changes(&[path])).await;
        }
        finish(
            turn_id,
            prepared,
            result,
            &mut messages,
            trajectory,
            events,
            mask_ips,
        )
        .await;
    }
    messages
}

/// Most characters of one tool result that go anywhere: to the model, the
/// transcript, the session file. A `cat` of a large file or a chatty build
/// would otherwise fill the context window in one call, past what compaction
/// can fold, since the newest round is the one that cannot be folded.
pub const MAX_TOOL_OUTPUT: usize = 40_000;

/// An output past [`MAX_TOOL_OUTPUT`], cut to its head and its tail — where a
/// listing starts and where a log ends with its error — around a note that
/// says how much was left out and how to see it. The one bound of a tool's
/// answer, so the report a finished background job delivers is bounded the
/// same way, and not by a second cap that could disagree with this one.
pub(crate) fn cap_output(output: &str) -> String {
    let total = output.chars().count();
    if total <= MAX_TOOL_OUTPUT {
        return output.to_owned();
    }
    let tail = MAX_TOOL_OUTPUT / 4;
    let head = MAX_TOOL_OUTPUT - tail;
    let omitted = total - head - tail;
    let start: String = output.chars().take(head).collect();
    let end: String = output.chars().skip(total - tail).collect();
    format!(
        "{start}\n… [{omitted} characters left out: the output was longer than \
         {MAX_TOOL_OUTPUT}; narrow it, e.g. with grep, head or a line range] …\n{end}"
    )
}

/// Masks a report before it reaches the model, exactly as a tool result is:
/// a backgrounded command prints whatever it prints, and a key in that output
/// must not enter the session just because the command outlived its turn.
pub(crate) fn mask(output: &str, mask_ips: bool) -> String {
    if mask_ips {
        titi_memory::redact::redact_for_model(output).text
    } else {
        titi_memory::redact::redact(output).text
    }
}

/// Masks every string inside a tool call's arguments, keys and non-strings
/// left alone, so a credential a call carries is not written to the
/// trajectory — the same redactor the answer and the diff go through.
///
/// This is the one place a `ToolCall` is recorded: the engine is the only
/// producer, and `TrajectorySink` is handed down from the surface, so every
/// turn — main, subagent, goal, council — funnels through here.
fn mask_args(args: &serde_json::Value, mask_ips: bool) -> serde_json::Value {
    use serde_json::Value;

    match args {
        Value::String(text) => Value::String(mask(text, mask_ips)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| mask_args(item, mask_ips)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), mask_args(value, mask_ips)))
                .collect(),
        ),
        other => other.clone(),
    }
}

struct Executed {
    call_id: SmolStr,
    output: SmolStr,
    /// The tool's presentation detail, if it reported one. Deliberately absent
    /// from the tool message pushed to the provider.
    detail: Option<SmolStr>,
    is_error: bool,
}

async fn invoke_one(
    turn_id: TurnId,
    prepared: &Prepared,
    tools: &ToolRegistry,
    approval_mode: ApprovalMode,
    waiters: &ApprovalWaiters,
    aborted: &AtomicBool,
    events: &mpsc::Sender<EngineEvent>,
) -> Executed {
    let call = &prepared.call;
    let args = &prepared.args;
    let Some(handler) = tools.get(&call.name) else {
        let output = tools
            .withheld_reason(&call.name)
            .unwrap_or_else(|| format!("unknown tool {}", call.name));
        return Executed {
            call_id: call.call_id.clone(),
            output: output.into(),
            is_error: true,
            detail: None,
        };
    };
    // A call the tool would refuse anyway is answered before anyone is asked
    // to approve it.
    if let Some(refusal) = handler.refusal(args) {
        return Executed {
            call_id: call.call_id.clone(),
            output: refusal.into(),
            is_error: true,
            detail: None,
        };
    }
    let tier = tools.approval_tier(&call.name);
    if !approval_mode.auto_approves(tier) {
        let _ = events
            .send(EngineEvent::ToolApprovalNeeded {
                turn_id,
                call_id: call.call_id.clone(),
                name: call.name.clone(),
            })
            .await;
        let (tx, rx) = oneshot::channel();
        waiters.lock().await.insert(call.call_id.clone(), tx);
        let approved = tokio::select! {
            result = rx => result.unwrap_or(false),
            _ = wait_aborted(aborted) => false,
        };
        if !approved {
            return Executed {
                call_id: call.call_id.clone(),
                output: "tool invocation denied".into(),
                is_error: true,
                detail: None,
            };
        }
    }
    let ToolResult {
        output,
        is_error,
        detail,
    } = handler.invoke(args.clone()).await;
    Executed {
        call_id: call.call_id.clone(),
        output,
        is_error,
        detail,
    }
}

async fn wait_aborted(aborted: &AtomicBool) {
    while !aborted.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touched_set_keeps_recency_without_duplicates() {
        let mut set = TouchedSet::default();
        set.insert("a.rs".into());
        set.insert("b.rs".into());
        // Re-touching moves a path to the recent end instead of duplicating it.
        set.insert("a.rs".into());
        assert_eq!(set.snapshot(), vec!["b.rs".to_owned(), "a.rs".to_owned()]);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn touched_set_is_bounded_and_drops_the_oldest() {
        let mut set = TouchedSet::default();
        for index in 0..TOUCHED_CAPACITY + 5 {
            set.insert(format!("file{index}.rs"));
        }
        assert_eq!(set.len(), TOUCHED_CAPACITY);
        let snapshot = set.snapshot();
        assert!(!snapshot.contains(&"file0.rs".to_owned()), "oldest dropped");
        assert!(snapshot.contains(&format!("file{}.rs", TOUCHED_CAPACITY + 4)));
    }

    #[test]
    fn an_empty_touched_set_is_empty() {
        let set = TouchedSet::default();
        assert!(set.is_empty());
        assert!(set.snapshot().is_empty());
    }

    /// A `write` call through the loop puts its file in the index before the
    /// call returns, and does it without a walk.
    ///
    /// This is the phase-2 claim stated as an experiment: the snapshot is read
    /// straight after `execute_tools`, with no refresh anywhere, and the stats
    /// of the update say no tree was listed. Without the fold-in the file is
    /// simply absent, and with a walk in its place `walked` is true.
    /// Two calls in one response: their deltas interleave by block id, and a
    /// single "current" slot gave the first call's arguments to the second and
    /// then dropped one of the two entirely.
    #[test]
    fn two_tool_calls_of_one_response_are_both_kept() {
        let mut collector = ToolCallCollector::default();
        let first = BlockId::new("tool_0");
        let second = BlockId::new("tool_1");
        collector.observe(&StreamEvent::ToolcallStart {
            id: first.clone(),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: "read".into(),
                ..Default::default()
            },
        });
        collector.observe(&StreamEvent::ToolcallStart {
            id: second.clone(),
            call: ToolCallRef {
                call_id: "call-2".into(),
                name: "grep".into(),
                ..Default::default()
            },
        });
        // Interleaved: the second call's arguments arrive before the first's.
        collector.observe(&StreamEvent::ToolcallDelta {
            id: second.clone(),
            json: r#"{"pattern":"b"}"#.into(),
        });
        collector.observe(&StreamEvent::ToolcallDelta {
            id: first.clone(),
            json: r#"{"path":"a.rs"}"#.into(),
        });
        collector.observe(&StreamEvent::ToolcallEnd { id: second });
        collector.observe(&StreamEvent::ToolcallEnd { id: first });

        let calls = collector.take();
        assert_eq!(calls.len(), 2, "both calls survive: {calls:?}");
        assert_eq!(calls[0].call_id, "call-1");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(calls[1].call_id, "call-2");
        assert_eq!(calls[1].name, "grep");
        assert_eq!(
            calls[1].arguments, r#"{"pattern":"b"}"#,
            "each call keeps its own arguments"
        );
    }

    #[tokio::test]
    async fn a_tool_write_reaches_the_index_without_a_walk() {
        use titi_tools::{ApprovalMode, ToolRegistry};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/hub.rs"), "pub fn hub() {}\n").unwrap();
        let live = SharedGenome::index(root).unwrap();
        assert!(
            live.last_stats().is_some_and(|stats| stats.walked),
            "the first index is a walk"
        );

        let mut tools = ToolRegistry::new();
        for tool in titi_tools::workspace_tools(root) {
            tools.register(Arc::from(tool));
        }
        let (events, _inbox) = mpsc::channel(8);
        let touched: TouchedSink = TouchedSink::default();
        let trajectory: TrajectorySink = TrajectorySink::default();
        let aborted = AtomicBool::new(false);
        let claims = Claims::new();
        let call = PendingToolCall {
            call_id: "call-1".into(),
            name: "write".into(),
            arguments: serde_json::json!({
                "path": "src/fresh.rs",
                "content": "pub fn fresh() {}\n",
            })
            .to_string(),
            // A Gemini thinking model signs the part a call came on; this
            // fixture is a plain write with nothing to echo.
            thought_signature: SmolStr::default(),
        };

        let messages = execute_tools(
            TurnId(1),
            vec![call],
            SmolStr::new_inline("writing"),
            // The thinking blocks the assistant message produced, which this
            // fixture's stream did not carry.
            Vec::new(),
            &tools,
            ApprovalMode::Yolo,
            &ApprovalWaiters::default(),
            &events,
            &aborted,
            &trajectory,
            &touched,
            Some(&live),
            &claims,
            &SmolStr::new_inline("Main"),
            false,
        )
        .await;
        assert_eq!(messages.len(), 2, "the call message and its result");
        assert!(
            !messages[1].content.contains("error"),
            "the write must have succeeded: {}",
            messages[1].content
        );

        let snapshot = live.snapshot();
        assert!(
            snapshot.files.contains_key("src/fresh.rs"),
            "the index knows the file the tool wrote: {:?}",
            snapshot.files.keys().collect::<Vec<_>>()
        );
        let stats = live.last_stats().expect("the write was folded in");
        assert_eq!(stats.parsed, 1, "one file, read and parsed");
        assert!(!stats.walked, "and no tree was listed to find it");
    }
}
