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
use crate::tool_loop::{
    ApprovalWaiters, ToolCallCollector, TouchedSink, TrajectorySink, execute_tools,
};

/// Rounds a subagent may spend calling tools before it is stopped.
pub const DEFAULT_AGENT_ROUNDS: u32 = 6;

/// Runs a subagent as a tool-calling turn.
pub struct ToolAgentRunner {
    resolver: Arc<dyn TransportResolver>,
    model: SmolStr,
    tools: ToolRegistry,
    claims: Claims,
    touched: TouchedSink,
    approval_mode: ApprovalMode,
    max_rounds: u32,
    mask_ips: bool,
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
            // Read-tier calls proceed; the registry decides what else exists.
            approval_mode: ApprovalMode::Write,
            max_rounds: DEFAULT_AGENT_ROUNDS,
            mask_ips: true,
        }
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
    async fn run(&self, request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        let resolved = self
            .resolver
            .resolve(&self.model)
            .map_err(|error| SmolStr::from(error.to_string()))?;
        let api_key = resolved.credential.map(|credential| credential.access);

        let mut messages = vec![ChatMessage {
            role: Role::User,
            content: request.task.clone(),
            tool_calls: Vec::new(),
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

            let mut wire = WireRequest::new(resolved.wire_model.clone());
            wire.messages = messages.clone();
            wire.tools = self.tools.specs();
            let mut stream = resolved
                .transport
                .stream(
                    wire,
                    RequestCtx {
                        api_key: api_key.clone(),
                        aborted: Arc::clone(&aborted),
                    },
                )
                .await
                .map_err(|error| SmolStr::from(error.to_string()))?;

            let mut collector = ToolCallCollector::default();
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                if context.is_aborted() {
                    aborted.store(true, Ordering::SeqCst);
                    return Err("aborted".into());
                }
                collector.observe(&event);
                match event {
                    StreamEvent::TextDelta { text: delta, .. } => {
                        text.push_str(&delta);
                        context.progress(delta).await;
                    }
                    StreamEvent::Error { message, .. } => return Err(message),
                    StreamEvent::Done { .. } => break,
                    _ => {}
                }
            }

            if !text.is_empty() {
                summary.push_str(&text);
            }
            let calls = collector.take();
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
            context
                .progress(format!("tools: {}", names.join(", ")))
                .await;

            let results = execute_tools(
                TurnId(0),
                calls,
                text.into(),
                &self.tools,
                self.approval_mode,
                &waiters,
                &sink,
                &aborted,
                &trajectory,
                &self.touched,
                &self.claims,
                &request.id,
                self.mask_ips,
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
