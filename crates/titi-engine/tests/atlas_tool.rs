#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The model's `atlas` tool, through the engine: offered on every request
//! when an index exists, answered from it, and absent when switched off.

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{ApprovalMode, ToolRegistry};

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

async fn until_terminal(engine: &mut titi_engine::Engine) {
    while let Some(event) = engine.recv().await {
        if matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        ) {
            break;
        }
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/store.rs"),
        "pub struct SessionStore;\npub fn flush_all() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/service.rs"),
        "use crate::store::SessionStore;\npub fn run() { let _ = SessionStore; }\n",
    )
    .unwrap();
    dir
}

fn script(args: &str) -> Vec<MockBody> {
    vec![
        MockBody::Events(vec![
            StreamEvent::ToolcallStart {
                id: BlockId::new("tool"),
                call: ToolCallRef {
                    call_id: "call-1".into(),
                    name: "atlas".into(),
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
        ]),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]
}

async fn run(config: EngineConfig, args: &str) -> Arc<MockTransport> {
    let transport = Arc::new(MockTransport::new(script(args)));
    let mut config = config;
    config.approval_mode = ApprovalMode::Yolo;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "who uses it".into(),
        })
        .await
        .unwrap();
    until_terminal(&mut engine).await;
    transport
}

#[tokio::test]
async fn the_tool_is_offered_and_answers_from_the_index() {
    let dir = workspace();
    let mut config = EngineConfig::new("primary");
    config.atlas_root = Some(dir.path().to_path_buf());
    let transport = run(config, r#"{"op":"uses","target":"SessionStore"}"#).await;

    let requests = transport.requests();
    assert!(
        requests[0].tools.iter().any(|spec| spec.name == "atlas"),
        "the first request offers the tool: {:?}",
        requests[0]
            .tools
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    );
    let answer = requests[1]
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("the tool's answer reached the model");
    assert!(
        answer.content.starts_with("atlas gen="),
        "{}",
        answer.content
    );
    assert!(
        answer.content.contains("src/service.rs"),
        "{}",
        answer.content
    );
    assert!(
        answer
            .content
            .contains("1: use crate::store::SessionStore;"),
        "{}",
        answer.content
    );
}

#[tokio::test]
async fn switched_off_or_without_an_index_there_is_no_tool() {
    let dir = workspace();
    let mut off = EngineConfig::new("primary");
    off.atlas_root = Some(dir.path().to_path_buf());
    off.atlas_tool = false;
    let transport = run(off, r#"{"op":"find","query":"x"}"#).await;
    assert!(
        !transport.requests()[0]
            .tools
            .iter()
            .any(|s| s.name == "atlas")
    );

    // No root means no index and so no tool: a duck stays repo-blind.
    let transport = run(EngineConfig::new("primary"), r#"{"op":"find","query":"x"}"#).await;
    assert!(
        !transport.requests()[0]
            .tools
            .iter()
            .any(|s| s.name == "atlas")
    );
}

/// `atlas.lens: false` keeps the map and drops the per-prompt lens: the
/// condition an A/B needs to tell the two apart.
#[tokio::test]
async fn the_lens_can_be_switched_off_while_the_map_stays() {
    async fn first_prompt(lens: bool) -> String {
        let dir = workspace();
        let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])]));
        let mut config = EngineConfig::new("primary");
        config.atlas_root = Some(dir.path().to_path_buf());
        config.atlas_lens = lens;
        let mut engine = EngineRuntime::start_with_tools(
            config,
            resolver(Arc::clone(&transport) as _),
            ToolRegistry::new(),
        );
        engine
            .send(EngineCommand::SubmitPrompt {
                text: "why does SessionStore lose entries?".into(),
            })
            .await
            .unwrap();
        until_terminal(&mut engine).await;
        transport.requests()[0]
            .messages
            .last()
            .unwrap()
            .content
            .to_string()
    }
    let with = first_prompt(true).await;
    assert!(
        with.contains("<atlas>") && with.contains("<atlas-lens>"),
        "{with}"
    );
    let without = first_prompt(false).await;
    assert!(without.contains("<atlas>"), "{without}");
    assert!(!without.contains("<atlas-lens>"), "{without}");
}

/// An exec-tier tool that deletes a source file, standing in for `rm`.
struct Remover(std::path::PathBuf);

#[async_trait::async_trait]
impl titi_tools::ToolHandler for Remover {
    fn definition(&self) -> titi_tools::ToolDefinition {
        titi_tools::ToolDefinition {
            spec: titi_providers::ToolSpec {
                name: "remover".into(),
                description: "removes src/service.rs".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            approval: titi_tools::ApprovalTier::Exec,
        }
    }

    async fn invoke(&self, _args: serde_json::Value) -> titi_tools::ToolResult {
        std::fs::remove_file(self.0.join("src/service.rs")).unwrap();
        titi_tools::ToolResult {
            output: "removed".into(),
            is_error: false,
            detail: None,
        }
    }
}

fn call(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new(id),
            call: ToolCallRef {
                call_id: id.into(),
                name: name.into(),
                ..Default::default()
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new(id),
            json: args.into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new(id),
        },
        StreamEvent::Done {
            reason: StopReason::ToolUse,
        },
    ]
}

/// A command that deletes a file is seen by the very next `atlas` call in the
/// same turn, not at the next turn's walk.
#[tokio::test]
async fn a_shell_commands_effects_are_in_the_index_before_the_next_call() {
    let dir = workspace();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(call("rm", "remover", "{}")),
        MockBody::Events(call(
            "ask",
            "atlas",
            r#"{"op":"uses","target":"SessionStore"}"#,
        )),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Remover(dir.path().to_path_buf())));
    let mut config = EngineConfig::new("primary");
    config.atlas_root = Some(dir.path().to_path_buf());
    config.approval_mode = ApprovalMode::Yolo;
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "clean up".into(),
        })
        .await
        .unwrap();
    until_terminal(&mut engine).await;

    let requests = transport.requests();
    let answer = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .rev()
        .find(|message| message.role == Role::Tool && message.content.starts_with("atlas gen="))
        .expect("the atlas answer reached the model");
    assert!(
        !answer.content.contains("src/service.rs"),
        "the deleted file is still listed as a user:\n{}",
        answer.content
    );
}
