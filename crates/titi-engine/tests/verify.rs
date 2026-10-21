//! The turn-end verifier through the real turn: it runs only after an edit,
//! feeds one failure back, and ends the turn with what it found.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use titi_engine::protocol::VerifyOutcome;
use titi_engine::{
    Engine, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{ApprovalMode, ToolRegistry, workspace_tools};

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

fn write_call(id: &str, path: &str, content: &str) -> MockBody {
    let args = serde_json::json!({ "path": path, "content": content }).to_string();
    MockBody::Events(vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: id.into(),
                name: "write".into(),
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
    ])
}

fn say(text: &str) -> MockBody {
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

async fn start(
    root: &Path,
    transport: &Arc<MockTransport>,
    command: Option<&str>,
    on_edit: bool,
) -> Engine {
    let mut tools = ToolRegistry::new();
    for tool in workspace_tools(root) {
        tools.register(Arc::from(tool));
    }
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Yolo;
    config.workspace_root = Some(root.to_path_buf());
    let resolver = Arc::new(MapResolver(
        [(
            "primary".to_owned(),
            Arc::clone(transport) as Arc<dyn Transport>,
        )]
        .into_iter()
        .collect(),
    ));
    let engine = EngineRuntime::start_with_tools(config, resolver, tools);
    engine
        .send(EngineCommand::SetVerify {
            command: command.map(str::to_owned),
            on_edit,
        })
        .await
        .unwrap();
    engine
}

async fn turn(engine: &mut Engine, text: &str) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SubmitPrompt { text: text.into() })
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        );
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

fn finished(events: &[EngineEvent]) -> Vec<(VerifyOutcome, Option<i32>, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::VerifyFinished {
                outcome,
                exit_code,
                retrying,
                ..
            } => Some((*outcome, *exit_code, *retrying)),
            _ => None,
        })
        .collect()
}

fn started(events: &[EngineEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, EngineEvent::VerifyStarted { .. }))
        .count()
}

fn turn_finishes(events: &[EngineEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, EngineEvent::TurnFinished { .. }))
        .count()
}

fn user_messages(transport: &MockTransport) -> Vec<String> {
    transport
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.content.to_string())
        .collect()
}

#[tokio::test]
async fn a_turn_that_edited_and_passes_the_check_says_so_and_ends() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "a.txt", "hello"),
        say("done"),
    ]));
    let mut engine = start(dir.path(), &transport, Some("test -f a.txt"), true).await;
    let events = turn(&mut engine, "make a.txt").await;

    assert_eq!(started(&events), 1);
    assert_eq!(
        finished(&events),
        vec![(VerifyOutcome::Passed, Some(0), false)]
    );
    assert_eq!(turn_finishes(&events), 1);
    assert_eq!(transport.requests().len(), 2, "no retry on a pass");
    // The note comes before the turn ends.
    let verify_at = events
        .iter()
        .position(|event| matches!(event, EngineEvent::VerifyFinished { .. }))
        .unwrap();
    let finish_at = events
        .iter()
        .position(|event| matches!(event, EngineEvent::TurnFinished { .. }))
        .unwrap();
    assert!(verify_at < finish_at);
}

#[tokio::test]
async fn a_failure_goes_back_once_the_model_fixes_it_and_the_second_run_passes() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "a.txt", "hello"),
        say("done"),
        write_call("c2", "ok.txt", "fixed"),
        say("fixed it"),
    ]));
    let command = "if test -f ok.txt; then exit 0; else echo 'no ok.txt yet' >&2; exit 3; fi";
    let mut engine = start(dir.path(), &transport, Some(command), true).await;
    let events = turn(&mut engine, "make a.txt").await;

    assert_eq!(
        finished(&events),
        vec![
            (VerifyOutcome::Failed, Some(3), true),
            (VerifyOutcome::Passed, Some(0), false),
        ]
    );
    assert_eq!(turn_finishes(&events), 1, "one turn, one end");
    assert_eq!(transport.requests().len(), 4);
    let sent = user_messages(&transport);
    let feedback = sent
        .iter()
        .find(|text| text.starts_with("verification failed: "))
        .unwrap_or_else(|| panic!("no feedback in {sent:?}"));
    assert!(feedback.contains("exit 3"), "{feedback}");
    assert!(feedback.contains("no ok.txt yet"), "{feedback}");
    assert!(feedback.contains("if test -f ok.txt"), "{feedback}");
}

#[tokio::test]
async fn a_second_failure_ends_the_turn_with_the_note_and_no_second_retry() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "a.txt", "one"),
        say("done"),
        write_call("c2", "a.txt", "two"),
        say("tried"),
    ]));
    let mut engine = start(dir.path(), &transport, Some("echo broken; exit 1"), true).await;
    let events = turn(&mut engine, "go").await;

    assert_eq!(
        finished(&events),
        vec![
            (VerifyOutcome::Failed, Some(1), true),
            (VerifyOutcome::Failed, Some(1), false),
        ]
    );
    assert_eq!(turn_finishes(&events), 1);
    assert_eq!(transport.requests().len(), 4, "the model was told once");
    let feedbacks = user_messages(&transport)
        .into_iter()
        .filter(|text| text.starts_with("verification failed: "))
        .count();
    assert_eq!(feedbacks, 1);
}

#[tokio::test]
async fn a_failed_check_is_rerun_even_if_the_model_edits_nothing_in_reply() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "a.txt", "one"),
        say("done"),
        say("I cannot fix that"),
    ]));
    let mut engine = start(dir.path(), &transport, Some("exit 1"), true).await;
    let events = turn(&mut engine, "go").await;

    assert_eq!(
        finished(&events),
        vec![
            (VerifyOutcome::Failed, Some(1), true),
            (VerifyOutcome::Failed, Some(1), false),
        ]
    );
    assert_eq!(turn_finishes(&events), 1);
}

#[tokio::test]
async fn a_turn_that_edited_nothing_runs_no_check() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![say("just talking")]));
    let mut engine = start(dir.path(), &transport, Some("exit 1"), true).await;
    let events = turn(&mut engine, "hi").await;

    assert_eq!(started(&events), 0);
    assert!(finished(&events).is_empty());
    assert_eq!(turn_finishes(&events), 1);
}

#[tokio::test]
async fn an_edit_the_tool_refused_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "../escape.txt", "x"),
        say("ok"),
    ]));
    let mut engine = start(dir.path(), &transport, Some("exit 1"), true).await;
    let events = turn(&mut engine, "go").await;

    assert_eq!(started(&events), 0, "nothing was written");
}

#[tokio::test]
async fn with_on_edit_off_every_turn_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![say("just talking")]));
    let mut engine = start(dir.path(), &transport, Some("true"), false).await;
    let events = turn(&mut engine, "hi").await;

    assert_eq!(
        finished(&events),
        vec![(VerifyOutcome::Passed, Some(0), false)]
    );
}

#[tokio::test]
async fn with_no_command_nothing_is_guessed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "a.txt", "one"),
        say("done"),
    ]));
    let mut engine = start(dir.path(), &transport, None, true).await;
    let events = turn(&mut engine, "go").await;

    assert_eq!(started(&events), 0);
    assert_eq!(turn_finishes(&events), 1);
}

#[tokio::test]
async fn a_cancel_during_the_check_stops_it_instead_of_waiting_it_out() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        write_call("c1", "a.txt", "one"),
        say("done"),
    ]));
    let mut engine = start(dir.path(), &transport, Some("sleep 30"), true).await;
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let mut saw_start = false;
    while let Some(event) = engine.recv().await {
        if matches!(event, EngineEvent::VerifyStarted { .. }) {
            saw_start = true;
            engine.send(EngineCommand::Cancel).await.unwrap();
        }
        if matches!(
            event,
            EngineEvent::Cancelled { .. } | EngineEvent::TurnFinished { .. }
        ) {
            break;
        }
    }
    assert!(saw_start);
}
