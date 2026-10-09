#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The `agent` tool: the model handing a task to a subagent through the
//! supervisor, and getting the answer back in its next request.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use smol_str::SmolStr;
use titi_engine::{
    AgentContext, AgentRequest, AgentRunner, AgentStatus, BATCH_CONCURRENCY, EngineCommand,
    EngineConfig, EngineEvent, EngineRuntime, MAX_ANSWER_CHARS, RegistryError, ResolvedModel,
    TransportResolver,
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

// ---- the batch shape -------------------------------------------------------

/// Both children must be running at once or this never returns: each waits at
/// a barrier of two for the other to arrive. A batch that ran its children one
/// at a time would hang here, which is why the test is bounded by a timeout.
struct RendezvousRunner {
    gate: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl AgentRunner for RendezvousRunner {
    async fn run(&self, request: AgentRequest, _context: AgentContext) -> Result<SmolStr, SmolStr> {
        let briefed = request.task.starts_with("shared briefing");
        self.gate.wait().await;
        Ok(format!("{} met the other (briefed: {briefed})", request.name).into())
    }
}

/// A batch runs its children at once, and the shared `context` reaches each of
/// them as the head of its task.
#[tokio::test]
async fn a_batch_runs_its_children_at_once() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "agent",
            r#"{"context":"shared briefing","tasks":[{"task":"left","name":"Left"},{"task":"right","name":"Right"}]}"#,
        )),
        MockBody::Events(text("both done")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(RendezvousRunner {
            gate: Arc::new(tokio::sync::Barrier::new(2)),
        }),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        collect_until_terminal(&mut engine),
    )
    .await
    .expect("the batch finished; a sequential batch would still be at the barrier");

    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::AgentFinished { summary, .. } if summary.contains("briefed: true")
        )),
        "the shared context was prepended to the task: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolFinished { output, is_error: false, .. }
                if output.contains("Left — completed") && output.contains("Right — completed")
        )),
        "one section per child, in the order asked for: {events:?}"
    );
}

/// The bound is real: six children never have more than `BATCH_CONCURRENCY`
/// alive at once, and they do run together.
#[tokio::test]
async fn a_batch_never_runs_more_than_the_bound() {
    struct CountingRunner {
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentRunner for CountingRunner {
        async fn run(
            &self,
            _request: AgentRequest,
            _context: AgentContext,
        ) -> Result<SmolStr, SmolStr> {
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok("done".into())
        }
    }

    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<String> = (1..=6)
        .map(|index| format!(r#"{{"task":"piece {index}","name":"P{index}"}}"#))
        .collect();
    let args = format!(r#"{{"tasks":[{}]}}"#, tasks.join(","));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("agent", &args)),
        MockBody::Events(text("all done")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(CountingRunner {
            live: Arc::clone(&live),
            peak: Arc::clone(&peak),
        }),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let peak = peak.load(Ordering::SeqCst);
    assert!(
        peak <= BATCH_CONCURRENCY,
        "at most {BATCH_CONCURRENCY} children at once, saw {peak}: {events:?}"
    );
    assert!(peak > 1, "and they did run together, saw {peak}");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::AgentFinished { .. }))
            .count(),
        6,
        "every child finished"
    );
}

/// Each child gets a section, in the order it was asked for, and however long
/// the answers are the whole message fits `MAX_ANSWER_CHARS` with the cut
/// stated.
#[tokio::test]
async fn a_batch_answers_with_one_capped_section_per_child() {
    struct LongRunner;

    #[async_trait]
    impl AgentRunner for LongRunner {
        async fn run(
            &self,
            request: AgentRequest,
            _context: AgentContext,
        ) -> Result<SmolStr, SmolStr> {
            Ok(format!("{} says {}", request.name, "x".repeat(20_000)).into())
        }
    }

    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "agent",
            r#"{"tasks":[{"task":"one","name":"First"},{"task":"two","name":"Second"},{"task":"three","name":"Third"}]}"#,
        )),
        MockBody::Events(text("all done")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(LongRunner),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let output = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("the tool finished");
    for name in [
        "First — completed",
        "Second — completed",
        "Third — completed",
    ] {
        assert!(output.contains(name), "missing {name:?} in {output}");
    }
    assert!(
        output.chars().count() <= MAX_ANSWER_CHARS,
        "the message fits the cap: {} chars",
        output.chars().count()
    );
    assert!(
        output.contains("answer truncated"),
        "and the cut is stated: {output}"
    );
}

/// A child that fails is a section, not an abort: its siblings in the same
/// wave and the waves after it still run. omp's batch is
/// `mapWithConcurrencyLimitAllSettled`, whose contract is that launched
/// siblings always settle, and this follows it.
#[tokio::test]
async fn a_failing_child_does_not_stop_its_siblings() {
    struct OneFails;

    #[async_trait]
    impl AgentRunner for OneFails {
        async fn run(
            &self,
            request: AgentRequest,
            _context: AgentContext,
        ) -> Result<SmolStr, SmolStr> {
            if request.name == "Broken" {
                return Err("this one broke".into());
            }
            Ok(format!("{} is fine", request.name).into())
        }
    }

    // Five children, one of them failing, with a bound of four: the failure is
    // in the first wave, so the second wave only runs if a failure does not
    // stop the batch.
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "agent",
            r#"{"tasks":[{"task":"a","name":"Broken"},{"task":"b","name":"B"},{"task":"c","name":"C"},{"task":"d","name":"D"},{"task":"e","name":"E"}]}"#,
        )),
        MockBody::Events(text("done")),
    ]));
    let mut engine = EngineRuntime::start_with_agents_and_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        Arc::new(OneFails),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::AgentFinished { .. }))
            .count(),
        5,
        "every child ran: {events:?}"
    );
    let output = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the tool finished");
    assert!(output.0.contains("Broken — failed"), "{:?}", output.0);
    assert!(output.0.contains("this one broke"), "{:?}", output.0);
    for name in [
        "B — completed",
        "C — completed",
        "D — completed",
        "E — completed",
    ] {
        assert!(output.0.contains(name), "missing {name:?}");
    }
    assert!(output.1, "the call is not wholly successful");
}

/// A cancelled batch stops every child it spawned, not only the first.
#[tokio::test]
async fn a_cancelled_batch_stops_every_child() {
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(tool_call(
        "agent",
        r#"{"tasks":[{"task":"left","name":"Left"},{"task":"right","name":"Right"}]}"#,
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
    let mut started = 0;
    while started < 2 {
        let event = engine.recv().await.expect("an event");
        if matches!(event, EngineEvent::AgentStarted { .. }) {
            started += 1;
        }
        events.push(event);
    }
    engine.send(EngineCommand::Cancel).await.unwrap();
    events.extend(collect_until_terminal(&mut engine).await);

    let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut stopped = 0;
        while stopped < 2 {
            let event = engine.recv().await.expect("an event");
            if matches!(
                event,
                EngineEvent::AgentStatusChanged {
                    status: AgentStatus::Aborted,
                    ..
                }
            ) {
                stopped += 1;
            }
            events.push(event);
        }
    })
    .await
    .is_ok();
    assert!(
        stopped,
        "both children were stopped with the turn: {events:?}"
    );
}

/// Exactly one of `task` and `tasks`: both is a refusal, and nothing spawns.
#[tokio::test]
async fn both_shapes_at_once_are_refused() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "agent",
            r#"{"task":"one","tasks":[{"task":"two"}]}"#,
        )),
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
            EngineEvent::ToolFinished { output, is_error: true, .. } if output.contains("not both")
        )),
        "both shapes are refused: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::AgentStarted { .. })),
        "and nothing spawned: {events:?}"
    );
}

/// Neither shape is refused too, as is an empty batch.
#[tokio::test]
async fn neither_shape_is_refused() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("agent", r#"{"context":"only a context"}"#)),
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
        "neither shape is refused: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::AgentStarted { .. })),
        "and nothing spawned: {events:?}"
    );
}
