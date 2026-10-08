#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The model's `ask` tool: the question reaches the surface, the answer
//! reaches the model, and a cancelled turn answers nothing at all.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    AgentKind, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError,
    ResolvedModel, TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{
    AskAnswer, ReadCache, SensitivePolicy, ToolRegistry, workspace_tools_with_interrupt,
};

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

/// The workspace tools, sharing the engine's interrupt so a cancel reaches a
/// parked question the way it reaches a parked `bash`.
fn tools(root: &Path, interrupt: titi_tools::Interrupt) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    for tool in workspace_tools_with_interrupt(
        root,
        ReadCache::default(),
        SensitivePolicy::default(),
        interrupt,
    ) {
        tools.register(Arc::from(tool));
    }
    tools
}

fn tool_call(name: &str, args: &str) -> MockBody {
    MockBody::Events(vec![
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
    ])
}

fn says(text: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("0"),
            text: text.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

async fn wait_for<T>(
    engine: &mut titi_engine::Engine,
    mut pick: impl FnMut(&EngineEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = engine.recv().await {
            if let Some(found) = pick(&event) {
                return found;
            }
        }
        panic!("the engine stopped before the event arrived");
    })
    .await
    .expect("the event never arrived")
}

/// The last thing the model was sent, which is where a tool result shows up.
fn last_request(transport: &MockTransport) -> Vec<titi_providers::ChatMessage> {
    transport
        .requests()
        .last()
        .expect("the model was asked at least once")
        .messages
        .clone()
}

fn tool_message(transport: &MockTransport) -> String {
    last_request(transport)
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("the tool result goes back to the model")
        .content
        .to_string()
}

/// The model asks, the surface answers, and the answer is what the model is
/// told on its next request.
#[tokio::test]
async fn the_model_asks_and_the_answer_reaches_it() {
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        tool_call(
            "ask",
            r#"{"question":"Which colour?","options":["blue","red"]}"#,
        ),
        says("thanks"),
    ]));
    let captured = Arc::clone(&transport);
    let config = EngineConfig::new("primary");
    let interrupt = config.interrupt.clone();
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(vec![("primary", transport as Arc<dyn Transport>)]),
        tools(workspace.path(), interrupt),
    );

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "paint the widget".into(),
        })
        .await
        .unwrap();
    let (request_id, question, options, multi, free_text) =
        wait_for(&mut engine, |event| match event {
            EngineEvent::AskRequested {
                request_id,
                question,
                options,
                multi,
                free_text,
            } => Some((
                request_id.clone(),
                question.to_string(),
                options.iter().map(ToString::to_string).collect::<Vec<_>>(),
                *multi,
                *free_text,
            )),
            _ => None,
        })
        .await;
    assert_eq!(request_id, "ask-1");
    assert_eq!(question, "Which colour?");
    assert_eq!(options, vec!["blue", "red"]);
    assert!(!multi, "the model did not ask for several choices");
    assert!(free_text, "a list the model wrote is not the only answer");

    engine
        .send(EngineCommand::AnswerAsk {
            request_id,
            answer: AskAnswer::Chosen(vec!["blue".to_owned()]),
        })
        .await
        .unwrap();
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;

    assert_eq!(tool_message(&captured), "The user chose: blue");
}

/// A question in a turn the user cancelled is answered `Cancelled`, and the
/// tool says so rather than the turn waiting for an answer that cannot come.
#[tokio::test]
async fn a_cancelled_turn_answers_the_question_with_cancelled() {
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![tool_call(
        "ask",
        r#"{"question":"Deploy now?","options":["yes","no"]}"#,
    )]));
    let config = EngineConfig::new("primary");
    let interrupt = config.interrupt.clone();
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(vec![("primary", transport as Arc<dyn Transport>)]),
        tools(workspace.path(), interrupt),
    );

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "ship it".into(),
        })
        .await
        .unwrap();
    wait_for(&mut engine, |event| match event {
        EngineEvent::AskRequested { .. } => Some(()),
        _ => None,
    })
    .await;

    engine.send(EngineCommand::Cancel).await.unwrap();
    // The cancel reaches the parked question and the turn's teardown follows
    // it, so both facts are read from one pass: a `wait_for` per fact would
    // discard the other on its way to the one it wants.
    let (cancelled, output) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut cancelled = false;
        let mut output: Option<String> = None;
        while let Some(event) = engine.recv().await {
            match event {
                EngineEvent::Cancelled { .. } => cancelled = true,
                EngineEvent::ToolFinished { output: text, .. } => output = Some(text.to_string()),
                _ => {}
            }
            if cancelled && output.is_some() {
                break;
            }
        }
        (cancelled, output)
    })
    .await
    .expect("the cancelled turn never settled");
    assert!(cancelled, "the turn reports its cancel");
    let output = output.expect("the tool result comes back");
    assert!(output.contains("did not answer"), "{output}");
    assert!(output.contains("turn was cancelled"), "{output}");
}

/// A subagent has no surface, so its question is refused instead of parked:
/// nothing reaches the session's event stream, and the subagent keeps working.
#[tokio::test]
async fn a_subagent_cannot_ask_the_user() {
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        tool_call("ask", r#"{"question":"Which colour?"}"#),
        says("carried on"),
    ]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.agent_model = Some("worker".into());
    let interrupt = config.interrupt.clone();
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(vec![
            ("primary", Arc::clone(&transport) as Arc<dyn Transport>),
            ("worker", transport as Arc<dyn Transport>),
        ]),
        tools(workspace.path(), interrupt),
    );

    engine
        .send(EngineCommand::SpawnAgent {
            name: "Worker".into(),
            task: "paint the widget".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();

    // Bounded: the failure this guards is a subagent *parked* on a question
    // nobody can answer, and a test that waits for a park to end would hang
    // rather than fail.
    let asked = tokio::time::timeout(Duration::from_secs(10), async {
        let mut asked = false;
        while let Some(event) = engine.recv().await {
            if matches!(event, EngineEvent::AskRequested { .. }) {
                asked = true;
            }
            if matches!(event, EngineEvent::AgentFinished { .. }) {
                break;
            }
        }
        asked
    })
    .await
    .expect("the subagent never finished: its question parked it");
    assert!(
        !asked,
        "a subagent's question must not reach the session's surface"
    );
    assert!(
        tool_message(&captured).contains("no user to ask"),
        "the subagent is told why, not left waiting: {}",
        tool_message(&captured)
    );
}
