#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! What a turn writes into its trace.
//!
//! The shape the plan promised: one `Turn` span framing each turn, a `Llm`
//! span per model call carrying its tokens and cost, a `Tool` span under the
//! call that asked for it, a retry as an `Event` inside the round it belongs
//! to, a subagent as a branch of the turn, and a round's thinking text only
//! when the session asked for it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use titi_core::trace::{self, Span, SpanKind, SpanStatus};
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    SpanRecorder, SpanSink, TransportResolver,
};
use titi_providers::{
    BlockId, ErrorReason, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{EchoTool, ToolRegistry};

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

fn resolver(entries: Vec<(&str, Arc<dyn Transport>)>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        entries
            .into_iter()
            .map(|(model, transport)| (model.to_owned(), transport))
            .collect(),
    ))
}

/// The sink an engine writes through, bound to `agent_dir` the way the CLI
/// binds it.
fn sink(agent_dir: &Path, session: &str) -> SpanSink {
    Arc::new(std::sync::Mutex::new(Some(SpanRecorder::new(
        agent_dir.to_path_buf(),
        session.to_owned(),
    ))))
}

fn echo_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools
}

/// A streamed tool call with one complete argument payload.
fn tool_call(name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: name.into(),
                ..Default::default()
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

fn spans_of(agent_dir: &Path, session: &str, turn: u64) -> Vec<Span> {
    trace::read_turn(agent_dir, session, turn).expect("the turn's trace")
}

/// Two rounds and a tool call: the frame, two model calls, the tool under the
/// call that asked for it, each model call carrying what it cost.
#[tokio::test]
async fn a_turn_writes_its_tree_with_each_span_under_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("echo", r#"{"text":"hi"}"#)),
        MockBody::Events(text("done")),
    ]));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(dir.path().to_path_buf());
    let mut engine = EngineRuntime::start_with_session(
        config,
        resolver(vec![("primary", transport)]),
        None,
        echo_registry(),
        Default::default(),
        sink(dir.path(), "s1"),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let spans = spans_of(dir.path(), "s1", 1);
    let turn = spans
        .iter()
        .find(|span| span.kind == SpanKind::Turn)
        .expect("the turn's own span");
    assert_eq!(turn.name, "turn 1");
    assert_eq!(turn.status, SpanStatus::Ok);
    assert!(
        turn.parent_span_id.is_none(),
        "the frame hangs from nothing"
    );

    let calls: Vec<&Span> = spans.iter().filter(|s| s.kind == SpanKind::Llm).collect();
    assert_eq!(calls.len(), 2, "two rounds are two model calls");
    assert!(
        calls
            .iter()
            .all(|span| span.parent_span_id.as_deref() == Some(turn.span_id.as_str())),
        "a model call hangs from the turn"
    );
    assert_eq!(calls[0].name, "chat primary");
    assert_eq!(
        calls[0].attributes["gen_ai.request.model"],
        json!("primary")
    );
    assert_eq!(
        calls[0].attributes["gen_ai.response.finish_reasons"],
        json!(["tool_calls"]),
        "the round that asked for a tool says so"
    );
    assert_eq!(
        calls[1].attributes["gen_ai.response.finish_reasons"],
        json!(["stop"])
    );

    let tools: Vec<&Span> = spans.iter().filter(|s| s.kind == SpanKind::Tool).collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].attributes["gen_ai.tool.name"], json!("echo"));
    assert_eq!(tools[0].attributes["gen_ai.tool.call.id"], json!("call-1"));
    assert_eq!(tools[0].status, SpanStatus::Ok);
    assert_eq!(
        tools[0].parent_span_id.as_deref(),
        Some(calls[0].span_id.as_str()),
        "the tool hangs from the round that asked for it"
    );

    // Everything is inside the frame, and nothing has a duration that runs
    // backwards. Equal stamps are honest: a mocked turn takes under a
    // millisecond, and the trace is stamped in milliseconds.
    for span in &spans {
        assert!(span.end_ms >= span.start_ms, "{span:?}");
        if span.kind != SpanKind::Turn {
            assert!(span.start_ms >= turn.start_ms, "{span:?}");
            assert!(span.end_ms <= turn.end_ms, "{span:?}");
        }
    }
}

/// A retry is an event inside the round it repeated, not a span beside it: the
/// call's own span covers every attempt, which is what the conventions ask.
#[tokio::test]
async fn a_retry_is_an_event_inside_the_round_it_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(vec![StreamEvent::Error {
            reason: ErrorReason::Connection,
            message: "boom".into(),
        }]),
        MockBody::Events(text("second time")),
    ]));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(dir.path().to_path_buf());
    config.retry_backoff = std::time::Duration::from_millis(1);
    let mut engine = EngineRuntime::start_with_session(
        config,
        resolver(vec![("primary", transport)]),
        None,
        echo_registry(),
        Default::default(),
        sink(dir.path(), "s1"),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let spans = spans_of(dir.path(), "s1", 1);
    let round = spans
        .iter()
        .find(|span| span.kind == SpanKind::Llm)
        .expect("the round's span");
    assert_eq!(
        round.attributes["titi.attempts"],
        json!(2),
        "one call, two attempts"
    );
    assert_eq!(
        round.status,
        SpanStatus::Ok,
        "the call succeeded on its second attempt"
    );

    let retries: Vec<&Span> = spans
        .iter()
        .filter(|span| span.kind == SpanKind::Event && span.name.starts_with("retry"))
        .collect();
    assert_eq!(retries.len(), 1, "one pause, one event");
    assert_eq!(retries[0].attributes["error.type"], json!("retryable"));
    assert_eq!(
        retries[0].parent_span_id.as_deref(),
        Some(round.span_id.as_str()),
        "the event belongs to the call it repeated"
    );
}

/// A round's thinking is a size and a time always; the text itself only when
/// the session asked for it (`trace.thinking`), because it is the most
/// sensitive thing a trace could hold.
#[tokio::test]
async fn thinking_text_is_written_only_when_the_session_asked() {
    for asked in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
            StreamEvent::ThinkingDelta {
                id: BlockId::new("think"),
                text: "let me think about it".into(),
            },
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "an answer".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])]));
        let mut config = EngineConfig::new("primary");
        config.agent_dir = Some(dir.path().to_path_buf());
        config.trace_thinking = asked;
        let mut engine = EngineRuntime::start_with_session(
            config,
            resolver(vec![("primary", transport)]),
            None,
            echo_registry(),
            Default::default(),
            sink(dir.path(), "s1"),
        );
        engine
            .send(EngineCommand::SubmitPrompt { text: "go".into() })
            .await
            .unwrap();
        let _ = collect_until_terminal(&mut engine).await;

        let round = spans_of(dir.path(), "s1", 1)
            .into_iter()
            .find(|span| span.kind == SpanKind::Llm)
            .expect("the round's span");
        assert_eq!(
            round.thinking_chars(),
            Some(21),
            "the size is recorded either way (asked = {asked})"
        );
        assert_eq!(
            round.thinking.is_some(),
            asked,
            "the text follows the setting (asked = {asked})"
        );
        if let Some(text) = &round.thinking {
            assert_eq!(text, "let me think about it");
        }
    }
}

/// A subagent is a branch of the turn that spawned it: its own model calls and
/// tool calls hang from its span, in the parent turn's file.
#[tokio::test]
async fn a_subagent_is_a_branch_of_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "the note").unwrap();

    let worker = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("read", r#"{"path":"note.txt"}"#)),
        MockBody::Events(text("the note says the note")),
    ]));
    let primary = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("agent", r#"{"task":"read note.txt"}"#)),
        MockBody::Events(text("done")),
    ]));

    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(dir.path().to_path_buf());
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.agent_model = Some("worker".into());
    let mut engine = EngineRuntime::start_with_session(
        config,
        resolver(vec![("primary", primary), ("worker", worker)]),
        None,
        echo_registry(),
        Default::default(),
        sink(dir.path(), "s1"),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "use a subagent".into(),
        })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let spans = spans_of(dir.path(), "s1", 1);
    let turn = spans
        .iter()
        .find(|span| span.kind == SpanKind::Turn)
        .expect("the turn's span");
    let agent = spans
        .iter()
        .find(|span| span.kind == SpanKind::Agent)
        .expect("the subagent's span");
    assert!(!agent.name.is_empty(), "the agent span is named: {agent:?}");
    assert_eq!(
        agent.attributes["gen_ai.operation.name"],
        json!("invoke_agent")
    );
    assert_eq!(
        agent.parent_span_id.as_deref(),
        Some(turn.span_id.as_str()),
        "the branch hangs from the turn"
    );

    // Its own model call, and the tool call that call asked for — nested two
    // levels deep, which is what makes this a trace rather than a list.
    let call = spans
        .iter()
        .find(|span| {
            span.kind == SpanKind::Llm
                && span.parent_span_id.as_deref() == Some(agent.span_id.as_str())
        })
        .expect("the subagent's model call");
    let tool = spans
        .iter()
        .find(|span| span.kind == SpanKind::Tool && span.name == "read")
        .expect("the subagent's read");
    assert_eq!(
        tool.parent_span_id.as_deref(),
        Some(call.span_id.as_str()),
        "the subagent's tool hangs from the subagent's own call"
    );
    assert_eq!(tool.attributes["gen_ai.tool.name"], json!("read"));
}

/// Without a sink there is no trace at all: nothing is written, and the turn
/// runs exactly as it did before the feature existed. This is the test that
/// fails if the sink were ever wired in by default.
#[tokio::test]
async fn without_a_sink_no_trace_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(text("done"))]));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(dir.path().to_path_buf());
    let mut engine = EngineRuntime::start_with_session(
        config,
        resolver(vec![("primary", transport)]),
        None,
        echo_registry(),
        Default::default(),
        SpanSink::default(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnFinished { .. })),
        "the turn still finished"
    );
    assert!(
        !dir.path().join("traces").exists(),
        "no sink, no trace file"
    );
}
