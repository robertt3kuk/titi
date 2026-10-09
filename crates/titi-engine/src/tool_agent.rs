//! A subagent that works: its own bounded tool loop.
//!
//! [`StreamingAgentRunner`](crate::StreamingAgentRunner) is one provider turn
//! with no tools, so a subagent could report findings but never touch a file.
//! This runner runs the same tool loop the main turn runs, under its own agent
//! identity, sharing the runtime's write claims, touched-file set and read
//! cache.
//!
//! Safety: the registry and the approval mode are one decision, made once by
//! the caller at construction. An approval the engine cannot surface is a
//! hang — the subagent's event sink goes nowhere and nobody can answer the
//! prompt — so the two must agree: either the registry is filtered to tiers
//! the mode auto-approves, or the mode covers everything the registry keeps.
//! Escalation is the caller's decision, never this runner's.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E4).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use smol_str::SmolStr;
use titi_providers::{ChatMessage, RequestCtx, Role, StreamEvent, WireRequest};
use titi_tools::{ApprovalMode, ToolRegistry};
use tokio::sync::mpsc;

use crate::agents::{AgentContext, AgentRequest, AgentRunner};
use crate::claims::Claims;
use crate::protocol::TurnId;
use crate::runtime::TransportResolver;
use crate::spans::{self, OpenSpan, SpanSink};
use crate::tool_loop::{
    ApprovalWaiters, ToolCallCollector, TouchedSink, TrajectorySink, execute_tools, mask,
};
use titi_core::trace::SpanKind;

/// Rounds a subagent may spend calling tools before it is stopped.
pub const DEFAULT_AGENT_ROUNDS: u32 = 6;

/// Runs a subagent as a tool-calling turn.
pub struct ToolAgentRunner {
    resolver: Arc<dyn TransportResolver>,
    model: SmolStr,
    tools: ToolRegistry,
    claims: Claims,
    touched: TouchedSink,
    /// The session's live index, when there is one. A subagent's `write` or
    /// `edit` folds its path in through this exactly as the main turn's does,
    /// instead of waiting for the session's next walk to notice the file.
    genome: Option<titi_genome::GenomeHandle>,
    approval_mode: ApprovalMode,
    max_rounds: u32,
    mask_ips: bool,
    /// Where this subagent's spans are written, shared with the turn that
    /// spawned it: a subagent's calls are part of its parent's trace, and its
    /// tool calls are recorded through the same sink its own model calls use.
    spans: SpanSink,
    /// Whether this subagent's reasoning text is recorded (`trace.thinking`),
    /// as the session asked for it.
    trace_thinking: bool,
}

impl ToolAgentRunner {
    /// `approval_mode` must auto-approve every tier present in `tools`;
    /// otherwise the loop blocks on an approval nobody can give.
    pub fn new(
        resolver: Arc<dyn TransportResolver>,
        model: impl Into<SmolStr>,
        tools: ToolRegistry,
        claims: Claims,
        touched: TouchedSink,
    ) -> Self {
        Self {
            resolver,
            model: model.into(),
            tools,
            claims,
            touched,
            genome: None,
            // Read-tier calls proceed; the registry decides what else exists.
            approval_mode: ApprovalMode::Write,
            max_rounds: DEFAULT_AGENT_ROUNDS,
            mask_ips: true,
            spans: SpanSink::default(),
            trace_thinking: false,
        }
    }

    /// The session's span sink, and whether a round's reasoning text is
    /// recorded. A runner built without one produces no spans at all, which is
    /// the honest state for a caller that has no trace of its own.
    pub fn with_spans(mut self, spans: SpanSink, trace_thinking: bool) -> Self {
        self.spans = spans;
        self.trace_thinking = trace_thinking;
        self
    }

    /// The session's index, so this runner's writes are folded in as they
    /// happen rather than at the session's next walk.
    ///
    /// `None` is the honest value for a runner built before the index exists
    /// — a caller with no root, or a test with no graph: the write still
    /// happens, and the session's next turn picks it up the way it picks up
    /// any other change no tool named.
    pub fn with_genome(mut self, genome: Option<titi_genome::GenomeHandle>) -> Self {
        self.genome = genome;
        self
    }

    /// Whether IPv4 addresses in tool output are masked; keys always are.
    pub fn with_mask_ips(mut self, mask: bool) -> Self {
        self.mask_ips = mask;
        self
    }

    /// Sets the round cap.
    pub fn with_max_rounds(mut self, rounds: u32) -> Self {
        self.max_rounds = rounds.max(1);
        self
    }

    /// Sets what the runner assumes it may do without an approval. Only pass
    /// something looser than [`ApprovalMode::Write`] for a registry whose
    /// write/exec tools are intentional.
    pub fn with_approval_mode(mut self, mode: ApprovalMode) -> Self {
        self.approval_mode = mode;
        self
    }

    /// Tools this runner may call.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }
}

#[async_trait]
impl AgentRunner for ToolAgentRunner {
    /// The subagent's run, wrapped in the span that frames it: everything this
    /// agent does — its model calls, its tool calls — hangs from it in the
    /// turn's trace, so a subagent is a branch of the turn that spawned it
    /// rather than a set of unrelated rows.
    async fn run(&self, request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        let mut agent_span = OpenSpan::open(
            &self.spans,
            spans::turn_span(&self.spans),
            SpanKind::Agent,
            request.name.to_string(),
        )
        .attr("gen_ai.operation.name", serde_json::json!("invoke_agent"))
        .attr("gen_ai.agent.id", serde_json::json!(request.id.as_str()))
        .attr(
            "gen_ai.agent.name",
            serde_json::json!(request.name.as_str()),
        )
        .attr(
            "gen_ai.agent.kind",
            serde_json::json!(format!("{:?}", request.kind).to_lowercase()),
        );
        let agent_span_id = agent_span.id().map(str::to_owned);
        let outcome = self.run_agent(&request, &context, agent_span_id).await;
        agent_span = match &outcome {
            Ok(_) => agent_span.status(titi_core::trace::SpanStatus::Ok),
            Err(reason) => agent_span.failed(reason.to_string()),
        };
        agent_span.finish(spans::now_ms());
        outcome
    }
}

impl ToolAgentRunner {
    /// The body of [`run`](AgentRunner::run), with the frame's span id in
    /// hand: it is what this agent's model calls and tool calls hang from.
    async fn run_agent(
        &self,
        request: &AgentRequest,
        context: &AgentContext,
        agent_span_id: Option<String>,
    ) -> Result<SmolStr, SmolStr> {
        let resolved = self
            .resolver
            .resolve(&self.model)
            .map_err(|error| SmolStr::from(error.to_string()))?;
        let credential = resolved.credential;

        let mut messages = vec![ChatMessage {
            role: Role::User,
            content: request.task.clone(),
            tool_calls: Vec::new(),
            ..Default::default()
        }];
        let mut summary = String::new();

        // The subagent's own cancellation flag, chained to the caller's.
        let aborted = Arc::new(AtomicBool::new(false));
        // Tool activity has no agent-tagged event, so the subagent reports it
        // through progress instead of polluting the main turn's tool rail.
        // The receiver is dropped rather than merely unread: a bounded channel
        // with a live reader that never polls fills up and blocks the second
        // send forever. Dropped, every send fails immediately and is ignored.
        let (sink, receiver) = mpsc::channel(1);
        drop(receiver);
        let waiters = ApprovalWaiters::default();
        let trajectory = TrajectorySink::default();

        let mut rounds = 0;
        loop {
            if context.is_aborted() {
                aborted.store(true, Ordering::SeqCst);
                return Err("aborted".into());
            }

            // This round's model call, under the agent's own span: a subagent
            // is a branch of the turn that spawned it, not a row beside it.
            let mut llm_span = OpenSpan::open(
                &self.spans,
                agent_span_id.clone(),
                SpanKind::Llm,
                format!("chat {}", resolved.wire_model),
            )
            .attr("gen_ai.operation.name", serde_json::json!("chat"))
            .attr(
                "gen_ai.request.model",
                serde_json::json!(resolved.wire_model.as_str()),
            );
            let llm_span_id = llm_span.id().map(str::to_owned);
            let mut reported: Option<titi_providers::TokenUsage> = None;
            // The reasoning this round streams, counted from the deltas: the
            // collector's blocks are what has to be replayed, and not every
            // family sends one.
            let mut thinking_text = String::new();

            let mut wire = WireRequest::new(resolved.wire_model.clone());
            wire.messages = messages.clone();
            wire.tools = self.tools.specs();
            let mut stream = match resolved
                .transport
                .stream(
                    wire,
                    RequestCtx {
                        credential: credential.clone(),
                        aborted: Arc::clone(&aborted),
                    },
                )
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    let reason = SmolStr::from(error.to_string());
                    llm_span.mark_failed(reason.to_string());
                    return Err(reason);
                }
            };

            let mut collector = ToolCallCollector::default();
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                if context.is_aborted() {
                    aborted.store(true, Ordering::SeqCst);
                    llm_span.set_status(titi_core::trace::SpanStatus::Cancelled);
                    return Err("aborted".into());
                }
                collector.observe(&event);
                match event {
                    StreamEvent::TextDelta { text: delta, .. } => {
                        text.push_str(&delta);
                        context.progress(delta).await;
                    }
                    StreamEvent::Usage(usage) => reported = Some(usage),
                    StreamEvent::ThinkingDelta { text: thought, .. } => {
                        thinking_text.push_str(&thought);
                    }
                    StreamEvent::Error { message, .. } => {
                        llm_span.mark_failed(message.to_string());
                        return Err(message);
                    }
                    StreamEvent::Done { .. } => break,
                    _ => {}
                }
            }

            if !text.is_empty() {
                summary.push_str(&text);
            }
            let calls = collector.take();
            // What this round cost and reasoned, then its span closes: the
            // tools it asked for run after the call that asked for them, and
            // hang from this span by id.
            let (prompt, completion, cached) = match reported {
                Some(usage) => (
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.cached_tokens,
                ),
                None => (0, 0, 0),
            };
            llm_span = llm_span.tokens(prompt, completion, cached, 0).attr(
                "gen_ai.response.finish_reasons",
                serde_json::json!([if calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }]),
            );
            let thought_chars = thinking_text.chars().count() as u64;
            if thought_chars > 0 {
                llm_span = llm_span.attr(
                    titi_core::trace::THINKING_CHARS_ATTR,
                    serde_json::json!(thought_chars),
                );
                if self.trace_thinking {
                    llm_span = llm_span.thinking(Some(mask(&thinking_text, self.mask_ips)));
                }
            }
            llm_span.finish(spans::now_ms());
            if calls.is_empty() {
                break;
            }
            if rounds >= self.max_rounds {
                return Err(
                    format!("subagent tool round cap reached ({})", self.max_rounds).into(),
                );
            }
            rounds += 1;

            let names: Vec<String> = calls.iter().map(|call| call.name.to_string()).collect();
            // Activity, not progress: this is the engine saying what the agent
            // is doing, not the model saying something. A surface that renders
            // them the same way is the surface's choice; the wire keeps them
            // apart so it can choose.
            context
                .activity(format!("tools: {}", names.join(", ")))
                .await;

            let results = execute_tools(
                TurnId(0),
                calls,
                text.into(),
                collector.thinking().to_vec(),
                &self.tools,
                self.approval_mode,
                &waiters,
                &sink,
                &aborted,
                &trajectory,
                &self.spans,
                &self.touched,
                // The same publish point the main turn's tool loop gets: a
                // subagent's write is folded in as the call returns, so the
                // session's next map is current by construction rather than
                // by the walk that would otherwise be the only thing to see
                // it.
                self.genome.as_ref().map(titi_genome::GenomeHandle::shared),
                &self.claims,
                &request.id,
                self.mask_ips,
                llm_span_id.as_deref(),
            )
            .await;
            messages.extend(results);
        }

        // Whatever files it claimed go back to the pool when it finishes; the
        // supervisor releases them too, belt and braces.
        self.claims.release_all(&request.id);

        if summary.is_empty() {
            Ok(format!("{} complete", request.name).into())
        } else {
            Ok(summary.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;

    use titi_genome::GenomeHandle;
    use titi_providers::mock::{MockBody, MockTransport};
    use titi_providers::{BlockId, StopReason, StreamEvent, ToolCallRef, Transport};
    use titi_tools::{ApprovalMode, ToolRegistry};

    use super::ToolAgentRunner;
    use crate::agents::{AgentContext, AgentRequest, AgentRunner};
    use crate::claims::Claims;
    use crate::protocol::AgentKind;
    use crate::registry::{RegistryError, ResolvedModel};
    use crate::runtime::TransportResolver;
    use crate::tool_loop::TouchedSink;

    struct MapResolver(HashMap<String, Arc<dyn Transport>>);

    impl TransportResolver for MapResolver {
        fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
            self.0
                .get(model)
                .cloned()
                .map(|transport| ResolvedModel::without_credential(model, transport))
                .ok_or_else(|| RegistryError::UnknownModel(model.into()))
        }
    }

    /// One complete tool call, then a report.
    fn write_then_report(path: &str, content: &str) -> Arc<MockTransport> {
        let arguments = serde_json::json!({ "path": path, "content": content }).to_string();
        Arc::new(MockTransport::new(vec![
            MockBody::Events(vec![
                StreamEvent::ToolcallStart {
                    id: BlockId::new("tool"),
                    call: ToolCallRef {
                        call_id: "call-1".into(),
                        name: "write".into(),
                        ..Default::default()
                    },
                },
                StreamEvent::ToolcallDelta {
                    id: BlockId::new("tool"),
                    json: arguments.clone().into(),
                },
                StreamEvent::ToolcallEnd {
                    id: BlockId::new("tool"),
                },
                StreamEvent::Done {
                    reason: StopReason::ToolUse,
                },
            ]),
            MockBody::Events(vec![
                StreamEvent::TextDelta {
                    id: BlockId::new("text"),
                    text: "wrote it".into(),
                },
                StreamEvent::Done {
                    reason: StopReason::Stop,
                },
            ]),
        ]))
    }

    /// A subagent's write reaches the index as it happens, exactly as the main
    /// turn's does — not at the session's next walk.
    ///
    /// The evidence is the handle's own record of the last update: a targeted
    /// fold reports `walked: false` and one re-parse, where a runner that
    /// passed `None` leaves the cold start's walk standing and the file out of
    /// the graph.
    #[tokio::test]
    async fn a_subagent_write_reaches_the_index_without_a_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/hub.rs"), "pub fn hub() {}\n").unwrap();
        let genome = GenomeHandle::spawn(root, titi_genome::live::Options::default()).unwrap();
        assert!(
            genome.last_stats().is_some_and(|stats| stats.walked),
            "the cold start is a walk"
        );

        let transport = write_then_report("src/fresh.rs", "pub fn fresh() {}\n");
        let resolver: Arc<dyn TransportResolver> = Arc::new(MapResolver(
            [("worker".to_owned(), transport as Arc<dyn Transport>)]
                .into_iter()
                .collect(),
        ));
        let mut tools = ToolRegistry::new();
        for tool in titi_tools::workspace_tools(root) {
            tools.register(Arc::from(tool));
        }
        let runner = ToolAgentRunner::new(
            resolver,
            "worker",
            tools,
            Claims::new(),
            TouchedSink::default(),
        )
        .with_approval_mode(ApprovalMode::Yolo)
        .with_genome(Some(genome.clone()));

        let summary = runner
            .run(
                AgentRequest {
                    id: "worker-1".into(),
                    name: "Worker".into(),
                    task: "write the file".into(),
                    kind: AgentKind::Subagent,
                    parent_id: None,
                },
                AgentContext::detached(),
            )
            .await
            .expect("the subagent finished");
        assert_eq!(summary, "wrote it");

        let stats = genome.last_stats().expect("the fold was recorded");
        assert!(
            !stats.walked,
            "the write folded in as it happened, not by a walk: {stats:?}"
        );
        assert_eq!(stats.parsed, 1, "one path, one parse: {stats:?}");
        assert!(
            genome.snapshot().0.files.contains_key("src/fresh.rs"),
            "the index knows the file the subagent wrote: {:?}",
            genome.snapshot().0.files.keys().collect::<Vec<_>>()
        );
    }
}
