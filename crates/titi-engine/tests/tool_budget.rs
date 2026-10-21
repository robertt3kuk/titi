#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A turn that spends its tool budget is asked to wrap up, not failed, and a
//! turn that repeats itself is told so once.

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::{
    DEFAULT_AGENT_ROUNDS, DEFAULT_TOOL_ROUNDS, EngineCommand, EngineConfig, EngineEvent,
    EngineRuntime, RegistryError, ResolvedModel, TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
    WireRequest,
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

fn echo_call(text: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: "echo".into(),
                ..Default::default()
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: serde_json::json!({ "text": text }).to_string().into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
        StreamEvent::Done {
            reason: StopReason::ToolUse,
        },
    ])
}

fn answer(text: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: text.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

struct Run {
    events: Vec<Vec<EngineEvent>>,
    requests: Vec<WireRequest>,
}

/// One turn per prompt against the scripted bodies, with the turn's tool-round
/// limit set to `rounds`.
async fn run(rounds: u32, bodies: Vec<MockBody>, prompts: &[&str]) -> Run {
    let transport = Arc::new(MockTransport::new(bodies));
    let captured = Arc::clone(&transport);
    let resolver: Arc<dyn TransportResolver> = Arc::new(MapResolver(
        [("primary".to_owned(), transport as Arc<dyn Transport>)]
            .into_iter()
            .collect(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let mut config = EngineConfig::new("primary");
    config.max_tool_rounds = rounds;
    let mut engine = EngineRuntime::start_with_tools(config, resolver, tools);
    let mut events = Vec::new();
    for prompt in prompts {
        engine
            .send(EngineCommand::SubmitPrompt {
                text: (*prompt).into(),
            })
            .await
            .unwrap();
        let mut turn = Vec::new();
        while let Some(event) = engine.recv().await {
            let terminal = matches!(
                event,
                EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
            );
            turn.push(event);
            if terminal {
                break;
            }
        }
        events.push(turn);
    }
    Run {
        events,
        requests: captured.requests(),
    }
}

fn streamed(turn: &[EngineEvent]) -> String {
    turn.iter()
        .filter_map(|event| match event {
            EngineEvent::StreamDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_results(request: &WireRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.content.to_string())
        .collect()
}

#[test]
fn the_defaults_are_a_hundred_rounds_and_forty_for_a_subagent() {
    assert_eq!(DEFAULT_TOOL_ROUNDS, 100);
    assert_eq!(DEFAULT_AGENT_ROUNDS, 40);
    assert_eq!(EngineConfig::new("primary").max_tool_rounds, 100);
    assert_eq!(EngineConfig::new("primary").agent_rounds, 40);
}

/// At the cap the turn does not fail: one more request goes out with tool
/// calls forbidden and a note asking for a summary, its answer streams, and
/// the turn ends normally with a visible notice.
#[tokio::test]
async fn at_the_cap_the_model_is_asked_to_wrap_up_and_the_turn_ends_normally() {
    let run = run(
        1,
        vec![
            echo_call("one"),
            answer("done: one; left: nothing"),
            answer("second turn"),
        ],
        &["hi", "again"],
    )
    .await;
    let turn = &run.events[0];

    assert!(
        !turn
            .iter()
            .any(|event| matches!(event, EngineEvent::Failed { .. })),
        "the cap must not fail the turn: {turn:?}"
    );
    assert!(matches!(
        turn.last(),
        Some(EngineEvent::TurnFinished { .. })
    ));
    assert!(
        turn.iter().any(|event| matches!(
            event,
            EngineEvent::Notice { message } if message.contains("tool budget reached")
        )),
        "the cap is said out loud: {turn:?}"
    );
    assert_eq!(streamed(turn), "done: one; left: nothing");

    // Request 1 is an ordinary round; request 2 is the wrap-up.
    assert!(!run.requests[0].forbid_tool_calls);
    let last = &run.requests[1];
    assert!(last.forbid_tool_calls, "the wrap-up forbids tool calls");
    assert!(
        !last.tools.is_empty(),
        "and still declares them (Anthropic refuses tool_use history without tools)"
    );
    let note = last.messages.last().expect("the wrap-up note");
    assert_eq!(note.role, Role::User);
    assert!(
        note.content
            .contains("tool budget reached: summarise what is done, what is left, and stop"),
        "{}",
        note.content
    );
    assert_eq!(tool_results(last), ["one"], "the work so far is kept");

    // The note was a request for one answer, not history: the next turn is
    // not told its budget is spent, and the summary it gave is there.
    let next = &run.requests[2];
    assert!(!next.forbid_tool_calls);
    assert!(
        next.messages
            .iter()
            .all(|message| !message.content.contains("tool budget reached")),
        "{:?}",
        next.messages
    );
    assert!(
        next.messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.content == "done: one; left: nothing")
    );
}

/// A model that calls a tool anyway on the wrap-up request gets nothing run:
/// its calls are dropped, the turn ends, and no tool is executed past the cap.
#[tokio::test]
async fn a_tool_call_on_the_wrap_up_request_is_not_executed() {
    let run = run(1, vec![echo_call("one"), echo_call("two")], &["hi"]).await;
    let turn = &run.events[0];
    assert!(
        matches!(turn.last(), Some(EngineEvent::TurnFinished { .. })),
        "{turn:?}"
    );
    let started = turn
        .iter()
        .filter(|event| matches!(event, EngineEvent::ToolStarted { .. }))
        .count();
    assert_eq!(started, 1, "only the round inside the budget ran: {turn:?}");
    assert_eq!(run.requests.len(), 2);
}

/// Inside the budget nothing changes: no note, no forbidden tools.
#[tokio::test]
async fn a_turn_inside_its_budget_is_not_asked_to_wrap_up() {
    let run = run(2, vec![echo_call("one"), answer("fine")], &["hi"]).await;
    assert!(
        run.requests
            .iter()
            .all(|request| !request.forbid_tool_calls)
    );
    assert!(
        !run.events[0]
            .iter()
            .any(|event| matches!(event, EngineEvent::Notice { .. }))
    );
    assert_eq!(streamed(&run.events[0]), "fine");
}

/// The same call three times with the same result: the third result carries
/// one nudge, the turn keeps going, and a fourth does not nag.
#[tokio::test]
async fn the_third_identical_call_gets_one_nudge_and_the_turn_goes_on() {
    let run = run(
        10,
        vec![
            echo_call("same"),
            echo_call("same"),
            echo_call("same"),
            echo_call("same"),
            answer("changed approach"),
        ],
        &["hi"],
    )
    .await;
    assert!(matches!(
        run.events[0].last(),
        Some(EngineEvent::TurnFinished { .. })
    ));
    // Request n+1 carries the results of rounds 1..=n.
    let second = tool_results(&run.requests[1]);
    let third = tool_results(&run.requests[2]);
    let fourth = tool_results(&run.requests[3]);
    let fifth = tool_results(&run.requests[4]);
    assert!(second.iter().chain(&third).all(|r| !r.contains("[titi:")));
    assert_eq!(fourth.len(), 3);
    assert!(
        fourth[2].starts_with("same\n\n[titi: you are repeating `echo"),
        "{}",
        fourth[2]
    );
    assert!(fourth[2].contains("Change approach or conclude"));
    assert_eq!(
        fifth.iter().filter(|r| r.contains("[titi:")).count(),
        1,
        "once per turn: {fifth:?}"
    );
}

#[tokio::test]
async fn varied_calls_are_never_nudged() {
    let run = run(
        10,
        vec![
            echo_call("a"),
            echo_call("b"),
            echo_call("c"),
            echo_call("d"),
            answer("done"),
        ],
        &["hi"],
    )
    .await;
    for request in &run.requests {
        assert!(
            tool_results(request).iter().all(|r| !r.contains("[titi:")),
            "{:?}",
            tool_results(request)
        );
    }
}
