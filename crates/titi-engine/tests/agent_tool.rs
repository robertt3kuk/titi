#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The `agent` tool: the model handing a task to a subagent through the
//! supervisor, and getting the answer back in its next request.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use smol_str::SmolStr;
use titi_engine::{
    AgentContext, AgentRequest, AgentRunner, AgentStatus, EngineCommand, EngineConfig, EngineEvent,
    EngineRuntime, RegistryError, ResolvedModel, TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::ToolRegistry;

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

fn resolver(transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        [("primary".to_owned(), transport)].into_iter().collect(),
    ))
}

fn two_model_resolver(
    parent: Arc<dyn Transport>,
    child: Arc<dyn Transport>,
) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        [("primary".to_owned(), parent), ("worker".to_owned(), child)]
            .into_iter()
            .collect(),
    ))
}

/// A streamed tool call with one complete argument payload.
fn tool_call(name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: name.into(),
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: args.into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
        StreamEvent::Done {
            reason: StopReason::ToolUse,
        },
    ]
}

fn text(body: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: body.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ]
}

async fn collect_until_terminal(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let terminal = matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

/// Answers at once, so the parent's wait is a real wait but a short one.
struct AnsweringRunner;

#[async_trait]
impl AgentRunner for AnsweringRunner {
    async fn run(&self, request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        context.activity("reading the tree").await;
        context.progress("found something").await;
        Ok(format!("{}: the payload is in note.txt", request.name).into())
    }
}

/// Never finishes on its own, so only a stop ends it.
struct HangingRunner;

#[async_trait]
impl AgentRunner for HangingRunner {
    async fn run(
        &self,
        _request: AgentRequest,
        _context: AgentContext,
    ) -> Result<SmolStr, SmolStr> {
        futures::future::pending::<()>().await;
        Ok("unreachable".into())
    }
}

/// The model is offered the tool, the subagent's life is on the stream, and the
/// parent's *next* request carries the answer — which is what makes the call a
/// delegation rather than a fire-and-forget.
#[tokio::test]
async fn a_parent_turn_gets_the_subagents_answer_in_its_next_request() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "agent",
            r#"{"name":"Scout","task":"find the payload"}"#,
        )),
        MockBody::Events(text("it is in note.txt")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(AnsweringRunner),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "find the payload".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let requests = transport.requests();
    assert!(
        requests[0].tools.iter().any(|tool| tool.name == "agent"),
        "the model is offered the tool: {:?}",
        requests[0].tools
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::AgentStarted { name, .. } if name == "Scout"
        )),
        "the spawn is on the stream: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::AgentActivity { text, .. } if text == "reading the tree"
        )),
        "what the agent is doing is its own event: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::AgentFinished { success: true, summary, .. } if summary.contains("note.txt")
        )),
        "the run finished with its summary: {events:?}"
    );
    assert!(
        requests[1].messages.iter().any(|message| message
            .content
            .contains("Scout: the payload is in note.txt")),
        "the parent's next request carries the answer: {:?}",
        requests[1].messages
    );
}

/// A subagent that fails says so to the parent, and the tool call is an error
/// rather than a silence.
#[tokio::test]
async fn a_failed_subagent_reports_why_to_the_parent() {
    struct FailingRunner;
    #[async_trait]
    impl AgentRunner for FailingRunner {
        async fn run(
            &self,
            _request: AgentRequest,
            _context: AgentContext,
        ) -> Result<SmolStr, SmolStr> {
            Err("the parser blew up".into())
        }
    }

    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("agent", r#"{"task":"try it"}"#)),
        MockBody::Events(text("ok")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(FailingRunner),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolFinished { output, is_error: true, .. }
                if output.contains("failed") && output.contains("the parser blew up")
        )),
        "the parent is told the child failed: {events:?}"
    );
}

/// Cancel reaches the child. The abort flag alone would not: the tool call that
/// spawned it is mid-await, and the loop only reads the flag between calls.
#[tokio::test]
async fn a_cancelled_turn_stops_the_child_it_spawned() {
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(tool_call(
        "agent",
        r#"{"task":"hang about"}"#,
    ))]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(HangingRunner),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();

    let mut events = Vec::new();
    loop {
        let event = engine.recv().await.expect("an event");
        let started = matches!(event, EngineEvent::AgentStarted { .. });
        events.push(event);
        if started {
            break;
        }
    }
    engine.send(EngineCommand::Cancel).await.unwrap();
    events.extend(collect_until_terminal(&mut engine).await);

    // The stop is part of the cancel and lands just after `Cancelled`, so keep
    // reading for it rather than assuming it arrived before the terminal event.
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = engine.recv().await.expect("an event");
            let stopped = matches!(
                event,
                EngineEvent::AgentStatusChanged {
                    status: AgentStatus::Aborted,
                    ..
                }
            );
            events.push(event);
            if stopped {
                break;
            }
        }
    })
    .await
    .is_ok();

    assert!(stopped, "the child was stopped with the turn: {events:?}");
}

/// Depth is bounded by the tool list: the subagent's own registry has no
/// `agent` tool, so a subagent cannot spawn a subagent at any depth.
#[tokio::test]
async fn a_subagent_is_not_offered_the_agent_tool() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "the payload").unwrap();
    let child = Arc::new(MockTransport::new(vec![MockBody::Events(text("done"))]));
    let parent = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("agent", r#"{"task":"read the note"}"#)),
        MockBody::Events(text("the child said done")),
    ]));

    let mut config = EngineConfig::new("primary");
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.agent_model = Some("worker".into());
    let mut engine = EngineRuntime::start_with_tools(
        config,
        two_model_resolver(Arc::clone(&parent) as _, Arc::clone(&child) as _),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    collect_until_terminal(&mut engine).await;

    let parent_tools: Vec<String> = parent.requests()[0]
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(
        parent_tools.iter().any(|name| name == "agent"),
        "the parent is offered it: {parent_tools:?}"
    );
    let child_tools: Vec<String> = child.requests()[0]
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(
        !child_tools.is_empty(),
        "the subagent has its own tools: {child_tools:?}"
    );
    assert!(
        !child_tools.iter().any(|name| name == "agent"),
        "a subagent cannot spawn a subagent: {child_tools:?}"
    );
}

/// A `task` with nothing in it is refused before anyone is asked to approve it.
#[tokio::test]
async fn an_empty_task_is_refused() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("agent", r#"{"task":"   "}"#)),
        MockBody::Events(text("ok")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(AnsweringRunner),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolFinished { output, is_error: true, .. } if output.contains("needs a `task`")
        )),
        "an empty task is refused: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::AgentStarted { .. })),
        "and nothing was spawned: {events:?}"
    );
}
