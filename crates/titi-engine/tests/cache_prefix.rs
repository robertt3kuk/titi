#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Nothing titi does between two turns may move the prefix the provider has
//! cached. With the things that are rebuilt every turn all present — a skill
//! list, agent definitions, an AGENTS.md, an identity file, MCP tools and a
//! tool-using turn behind the history — the second turn's first request must
//! start with exactly what the first turn's last request sent: same tools in
//! the same order, same system prompt, same messages, byte for byte.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use titi_config::mcp::McpServerDef;
use titi_engine::{
    Engine, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_mcp::ServerState;
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, Transport, WireRequest,
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

fn tool_call(name: &str, args: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::ThinkingBlock {
            block: titi_providers::ThinkingBlock {
                text: "let me look".into(),
                signature: "sig-1".into(),
                api: titi_providers::ApiKind::AnthropicMessages,
                ..Default::default()
            },
        },
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: "looking".into(),
        },
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

async fn until(engine: &mut Engine, mut stop: impl FnMut(&EngineEvent) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Ok(Some(event)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        engine.recv(),
    )
    .await
    {
        if stop(&event) {
            return;
        }
    }
    panic!("the engine never produced the event");
}

async fn turn(engine: &mut Engine, text: &str) {
    engine
        .send(EngineCommand::SubmitPrompt { text: text.into() })
        .await
        .unwrap();
    until(engine, |event| {
        matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        )
    })
    .await;
}

fn write(path: &std::path::Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// The messages of `later` that `earlier` already sent, compared as the bytes
/// a provider would hash.
fn assert_prefix(earlier: &WireRequest, later: &WireRequest) {
    assert!(
        later.messages.len() >= earlier.messages.len(),
        "the later request is shorter than the earlier one"
    );
    for (at, (before, after)) in earlier.messages.iter().zip(&later.messages).enumerate() {
        assert_eq!(
            format!("{before:?}"),
            format!("{after:?}"),
            "message {at} was rewritten between the two requests"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_rebuilt_per_turn_moves_the_prefix() {
    let agent = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    write(&agent.path().join("SOUL.md"), "IDENTITY-MARKER");
    write(
        &agent.path().join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Check a diff\n---\nBody\n",
    );
    write(
        &agent.path().join("skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Ship it\n---\nBody\n",
    );
    write(&workspace.path().join("AGENTS.md"), "RULES-MARKER");
    write(&workspace.path().join("note.txt"), "the payload");
    for name in ["zeta", "alpha", "mid"] {
        write(
            &workspace.path().join(format!(".claude/agents/{name}.md")),
            &format!("---\nname: {name}\ndescription: the {name} one\n---\nBody\n"),
        );
    }

    let transport = Arc::new(MockTransport::new(vec![
        tool_call("read", r#"{"path":"note.txt"}"#),
        say("the first answer"),
        tool_call("read", r#"{"path":"note.txt"}"#),
        say("the second answer"),
        say("the third answer"),
    ]));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(agent.path().to_path_buf());
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.atlas_root = Some(workspace.path().to_path_buf());
    config.agent_model = Some("primary".into());
    config.approval_mode = ApprovalMode::Yolo;
    let resolver = Arc::new(MapResolver(
        [(
            "primary".to_owned(),
            Arc::clone(&transport) as Arc<dyn Transport>,
        )]
        .into_iter()
        .collect(),
    ));
    let mut tools = ToolRegistry::new();
    for tool in titi_tools::workspace_tools(workspace.path()) {
        tools.register(Arc::from(tool));
    }
    let mut engine = EngineRuntime::start_with_tools(config, resolver, tools);

    // An MCP server whose tools are there before the first turn.
    engine
        .send(EngineCommand::SetMcp {
            servers: vec![McpServerDef {
                name: "fixture".to_owned(),
                command: "python3".to_owned(),
                args: vec![format!(
                    "{}/../titi-mcp/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                timeout_secs: Some(10),
                source: "test".to_owned(),
                ..Default::default()
            }],
        })
        .await
        .unwrap();
    until(&mut engine, |event| {
        matches!(event, EngineEvent::McpStatus { servers }
            if servers.iter().any(|s| s.name == "fixture" && s.state == ServerState::Ready))
    })
    .await;

    turn(&mut engine, "first question").await;
    turn(&mut engine, "second question").await;
    turn(&mut engine, "third question").await;

    let requests = transport.requests();
    assert_eq!(requests.len(), 5, "{} requests", requests.len());
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(names.iter().any(|name| name.starts_with("mcp__fixture__")));
    assert!(names.contains(&"agent"));
    // Rounds and turns alike: the tools never move.
    for (at, request) in requests.iter().enumerate() {
        assert_eq!(
            request.tools, requests[0].tools,
            "request {at} sent a different tool list"
        );
    }
    let system = |request: &WireRequest| request.messages[0].content.clone();
    assert!(system(&requests[0]).contains("IDENTITY-MARKER"));
    assert!(system(&requests[0]).contains("RULES-MARKER"));
    assert!(system(&requests[0]).contains("- review: Check a diff"));
    for (at, request) in requests.iter().enumerate() {
        assert_eq!(
            system(request),
            system(&requests[0]),
            "request {at} sent a different system prompt"
        );
    }

    // Round to round inside a turn, then turn to turn: each request starts
    // with everything the one before it sent.
    for pair in requests.windows(2) {
        assert_prefix(&pair[0], &pair[1]);
    }
}
