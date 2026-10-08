#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! E4 integration: per-file write claims and mid-flight steering.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E4).

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, ChatMessage, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef,
    Transport,
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

fn resolver(transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        [("primary".to_owned(), transport)].into_iter().collect(),
    ))
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

fn write_call_events(path: &str, body: &str) -> Vec<StreamEvent> {
    let args = serde_json::json!({ "path": path, "content": body }).to_string();
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: "write".into(),
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

fn workspace_tool_registry(root: &std::path::Path) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    for tool in workspace_tools(root) {
        tools.register(Arc::from(tool));
    }
    tools
}

#[tokio::test]
async fn a_claimed_file_is_refused_without_touching_disk() {
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(write_call_events("src/taken.rs", "hello")),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Yolo;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        workspace_tool_registry(workspace.path()),
    );

    // Another agent already holds the file.
    engine
        .claims()
        .try_claim("src/taken.rs", "agent-2")
        .unwrap();

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "write".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let finished = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolFinished {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the write tool reported a result");
    assert!(finished.1, "a claimed file must be refused");
    assert!(
        finished.0.contains("agent-2"),
        "the refusal names the holder: {}",
        finished.0
    );
    assert!(
        !workspace.path().join("src/taken.rs").exists(),
        "the handler must not run"
    );
    // The foreign claim is untouched by the refused call.
    assert_eq!(
        engine.claims().holder("src/taken.rs").as_deref(),
        Some("agent-2")
    );
}

#[tokio::test]
async fn a_released_file_can_be_written() {
    let workspace = tempfile::tempdir().unwrap();
    // The jail canonicalizes the parent, so it must exist before the write.
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(write_call_events("src/free.rs", "hello")),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Yolo;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        workspace_tool_registry(workspace.path()),
    );

    engine.claims().try_claim("src/free.rs", "agent-2").unwrap();
    engine.claims().release("src/free.rs", "agent-2");
    assert!(engine.claims().is_empty());

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "write".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ToolFinished {
            is_error: false,
            ..
        }
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("src/free.rs")).unwrap(),
        "hello"
    );
    // The claim was released after the call, so nothing stays locked.
    assert!(
        engine.claims().is_empty(),
        "claims left behind: {:?}",
        engine.claims().holder("src/free.rs")
    );
}

#[tokio::test]
async fn queued_steering_is_injected_before_the_prompt_answer() {
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let mut engine = EngineRuntime::start(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
    );

    // Steering queued before the turn exists is still delivered.
    engine
        .send(EngineCommand::Steer {
            text: "also check the tests".into(),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "fix the bug".into(),
        })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let messages: Vec<&ChatMessage> = requests[0].messages.iter().collect();
    assert_eq!(messages.len(), 2, "prompt plus the steering message");
    assert_eq!(messages[0].content, "fix the bug");
    assert_eq!(messages[1].role, Role::User);
    assert_eq!(messages[1].content, "also check the tests");
}

#[tokio::test]
async fn steering_sent_mid_turn_reaches_the_provider() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(write_call_events("src/a.rs", "one")),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Yolo;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        workspace_tool_registry(tempfile::tempdir().unwrap().path()),
    );

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "write a file".into(),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::Steer {
            text: "stop after this".into(),
        })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;

    let requests = transport.requests();
    assert!(requests.len() >= 2, "the tool round drove a second attempt");
    assert!(
        requests.iter().any(|request| request
            .messages
            .iter()
            .any(|message| message.content == "stop after this")),
        "steering never reached the provider"
    );
}
