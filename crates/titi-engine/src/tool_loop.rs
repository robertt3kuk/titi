use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smol_str::SmolStr;
use titi_providers::{ChatMessage, Role, StreamEvent, ToolCallRef};
use titi_tools::{ApprovalMode, ToolRegistry, ToolResult};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::claims::Claims;
use crate::protocol::{EngineEvent, TurnId};

#[derive(Debug, Clone)]
pub(crate) struct PendingToolCall {
    pub call_id: SmolStr,
    pub name: SmolStr,
    pub arguments: String,
}

#[derive(Default)]
pub(crate) struct ToolCallCollector {
    current: Option<PendingToolCall>,
    finished: Vec<PendingToolCall>,
}

impl ToolCallCollector {
    pub fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::ToolcallStart { call, .. } => {
                self.current = Some(PendingToolCall {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    arguments: String::new(),
                });
            }
            StreamEvent::ToolcallDelta { json, .. } => {
                if let Some(current) = &mut self.current {
                    current.arguments.push_str(json);
                }
            }
            StreamEvent::ToolcallEnd { .. } => {
                if let Some(current) = self.current.take() {
                    self.finished.push(current);
                }
            }
            _ => {}
        }
    }

    pub fn take(&mut self) -> Vec<PendingToolCall> {
        if let Some(current) = self.current.take() {
            self.finished.push(current);
        }
        std::mem::take(&mut self.finished)
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

pub(crate) async fn execute_tools(
    turn_id: TurnId,
    calls: Vec<PendingToolCall>,
    // What the assistant said in the same message as these calls.
    assistant_text: SmolStr,
    tools: &ToolRegistry,
    approval_mode: ApprovalMode,
    waiters: &ApprovalWaiters,
    events: &mpsc::Sender<EngineEvent>,
    aborted: &AtomicBool,
    trajectory: &TrajectorySink,
    touched: &TouchedSink,
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
        });
    }
    messages.push(ChatMessage {
        role: Role::Assistant,
        content: assistant_text,
        tool_calls: assistant_calls,
    });

    for call in calls {
        if aborted.load(Ordering::SeqCst) {
            break;
        }
        let args = serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
        // What the tool says it is about to do, for a surface to show beside
        // its name. Masked like the answer, and put on the event alone: the
        // provider is never told it, so it cannot spend the model's context.
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
            let _ = TOUCHING_TOOLS;
        }
        if let Some(recorder) = trajectory.lock().await.as_mut() {
            let _ = recorder.record(titi_core::trajectory::EventKind::ToolCall {
                id: call.call_id.to_string(),
                name: call.name.to_string(),
                args: args.clone(),
            });
        }
        // A write-tier call takes an exclusive claim on its file, so a
        // parallel agent cannot edit the same path underneath it.
        let claimed = if WRITING_TOOLS.contains(&call.name.as_str()) {
            args.get("path")
                .and_then(|value| value.as_str())
                .map(|path| claims.try_claim(path, agent_id))
        } else {
            None
        };
        let started = std::time::Instant::now();
        let result = match claimed {
            Some(Err(error)) => Executed {
                call_id: call.call_id.clone(),
                output: error.to_string().into(),
                is_error: true,
                detail: None,
            },
            Some(Ok(path)) => {
                let result = invoke_one(
                    turn_id,
                    call,
                    tools,
                    approval_mode,
                    waiters,
                    aborted,
                    events,
                )
                .await;
                claims.release(&path, agent_id);
                result
            }
            None => {
                invoke_one(
                    turn_id,
                    call,
                    tools,
                    approval_mode,
                    waiters,
                    aborted,
                    events,
                )
                .await
            }
        };
        // The answer goes to the provider, the transcript and the session file;
        // a key or a server address the tool printed stops here. The detail is
        // masked with it — a diff quotes what the tool wrote — and goes to the
        // event alone: the model never reads a presentation detail, so it can
        // never spend the context or answer for the tool.
        let result = Executed {
            output: cap_output(&mask(&result.output, mask_ips)).into(),
            detail: result
                .detail
                .as_deref()
                .map(|detail| mask(detail, mask_ips).into()),
            ..result
        };
        if let Some(recorder) = trajectory.lock().await.as_mut() {
            let _ = recorder.record(titi_core::trajectory::EventKind::ToolResult {
                id: result.call_id.to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
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
            tool_calls: Vec::new(),
        });
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
/// says how much was left out and how to see it.
fn cap_output(output: &str) -> String {
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

fn mask(output: &str, mask_ips: bool) -> String {
    if mask_ips {
        titi_memory::redact::redact_for_model(output).text
    } else {
        titi_memory::redact::redact(output).text
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
    call: PendingToolCall,
    tools: &ToolRegistry,
    approval_mode: ApprovalMode,
    waiters: &ApprovalWaiters,
    aborted: &AtomicBool,
    events: &mpsc::Sender<EngineEvent>,
) -> Executed {
    let Some(handler) = tools.get(&call.name) else {
        let output = tools
            .withheld_reason(&call.name)
            .unwrap_or_else(|| format!("unknown tool {}", call.name));
        return Executed {
            call_id: call.call_id,
            output: output.into(),
            is_error: true,
            detail: None,
        };
    };
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
                call_id: call.call_id,
                output: "tool invocation denied".into(),
                is_error: true,
                detail: None,
            };
        }
    }
    let args = serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    let ToolResult {
        output,
        is_error,
        detail,
    } = handler.invoke(args).await;
    Executed {
        call_id: call.call_id,
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
}
