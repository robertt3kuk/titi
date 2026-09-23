//! Session modes decide what a turn is allowed to reach for.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::protocol::SessionMode;
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{MockBody, MockTransport, StopReason, StreamEvent, Transport};
use titi_tools::{EchoTool, ShellProbeTool, ToolRegistry};

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

/// Read-tier `echo` and exec-tier `shell_probe`.
fn mixed_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(ShellProbeTool));
    tools
}

fn done() -> MockBody {
    MockBody::Events(vec![StreamEvent::Done {
        reason: StopReason::Stop,
    }])
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

async fn finished(engine: &mut titi_engine::Engine) {
    wait_for(engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;
}

/// Plan mode is enforced by what the request carries, not by asking the
/// model nicely: nothing above read tier is offered at all.
#[tokio::test]
async fn plan_mode_sends_read_tools_only_and_done_restores_them() {
    let transport = Arc::new(MockTransport::new(vec![done(), done()]));
    let captured = Arc::clone(&transport);
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(transport),
        mixed_registry(),
    );

    engine
        .send(EngineCommand::SetMode {
            mode: SessionMode::Plan,
        })
        .await
        .unwrap();
    let mode = wait_for(&mut engine, |event| match event {
        EngineEvent::ModeChanged { mode } => Some(*mode),
        _ => None,
    })
    .await;
    assert_eq!(mode, SessionMode::Plan);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "how would you fix the parser".into(),
        })
        .await
        .unwrap();
    finished(&mut engine).await;

    let planning = captured.requests();
    let offered: Vec<String> = planning[0]
        .tools
        .iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert_eq!(offered, vec!["echo".to_owned()], "{offered:?}");
    let system = planning[0]
        .messages
        .iter()
        .find(|message| message.role == titi_providers::Role::System)
        .expect("a system prompt");
    assert!(system.content.contains("plan mode"), "{}", system.content);

    // /done leaves the mode, and the next turn is handed everything again.
    engine
        .send(EngineCommand::SetMode {
            mode: SessionMode::Agent,
        })
        .await
        .unwrap();
    let mode = wait_for(&mut engine, |event| match event {
        EngineEvent::ModeChanged { mode } => Some(*mode),
        _ => None,
    })
    .await;
    assert_eq!(mode, SessionMode::Agent);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "now do it".into(),
        })
        .await
        .unwrap();
    finished(&mut engine).await;

    let acting = captured.requests();
    let offered: Vec<String> = acting[1]
        .tools
        .iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert_eq!(
        offered,
        vec!["echo".to_owned(), "shell_probe".to_owned()],
        "{offered:?}"
    );
    assert!(
        !acting[1]
            .messages
            .iter()
            .any(|message| message.content.contains("plan mode")),
        "the plan brief outlived the mode"
    );
}

/// Duck mode is repo-blind: no tool that could reach the machine, and no
/// map of the repository riding the prompt.
#[tokio::test]
async fn duck_mode_sends_no_tools_and_no_repository_map() {
    let workspace = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(workspace.path().join("src")).expect("src");
    std::fs::write(
        workspace.path().join("src/hub.rs"),
        "pub fn hub() -> u8 { 7 }\n",
    )
    .expect("write");

    let transport = Arc::new(MockTransport::new(vec![done(), done()]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.genome_root = Some(workspace.path().to_path_buf());
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), mixed_registry());

    // The map is there in agent mode, which is what makes its absence in
    // duck mode mean something.
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "what does hub do".into(),
        })
        .await
        .unwrap();
    finished(&mut engine).await;
    let seeing = captured.requests();
    assert!(
        seeing[0]
            .messages
            .iter()
            .any(|message| message.content.contains("src/hub.rs")),
        "the agent turn never saw the repository map"
    );

    engine
        .send(EngineCommand::SetMode {
            mode: SessionMode::Duck,
        })
        .await
        .unwrap();
    let mode = wait_for(&mut engine, |event| match event {
        EngineEvent::ModeChanged { mode } => Some(*mode),
        _ => None,
    })
    .await;
    assert_eq!(mode, SessionMode::Duck);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "talk me through it".into(),
        })
        .await
        .unwrap();
    finished(&mut engine).await;

    let ducking = captured.requests();
    let request = &ducking[1];
    assert!(request.tools.is_empty(), "{:?}", request.tools);
    // The replayed history still holds what the agent turn saw — that is
    // the conversation, not the mode. What must be gone is the map this
    // turn would otherwise have had built for it, which rides the newest
    // user message.
    let prompt = request.messages.last().expect("a prompt");
    assert!(
        !prompt.content.contains("<genome>"),
        "the duck turn was handed a repository map: {}",
        prompt.content
    );
    let system = request
        .messages
        .iter()
        .find(|message| message.role == titi_providers::Role::System)
        .expect("a system prompt");
    assert!(system.content.contains("duck mode"), "{}", system.content);
}

/// `--mode duck` starts there, and says so without being asked.
#[tokio::test]
async fn a_session_can_start_in_duck_mode() {
    let transport = Arc::new(MockTransport::new(vec![done()]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.mode = SessionMode::Duck;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), mixed_registry());

    let mode = wait_for(&mut engine, |event| match event {
        EngineEvent::ModeChanged { mode } => Some(*mode),
        _ => None,
    })
    .await;
    assert_eq!(mode, SessionMode::Duck);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "just thinking out loud".into(),
        })
        .await
        .unwrap();
    finished(&mut engine).await;
    assert!(
        captured.requests()[0].tools.is_empty(),
        "a duck session started with tools"
    );
}

/// A network-tier tool under a name the caller picks, so a test can show
/// that what decides a mode's registry is the tier and not the name.
struct FakeNetworkTool(&'static str, Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl titi_tools::ToolHandler for FakeNetworkTool {
    fn definition(&self) -> titi_tools::ToolDefinition {
        titi_tools::ToolDefinition {
            spec: titi_providers::ToolSpec {
                name: self.0.into(),
                description: "Reach an outside host".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }),
            },
            approval: titi_tools::ApprovalTier::Network,
        }
    }

    async fn invoke(&self, _args: serde_json::Value) -> titi_tools::ToolResult {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        titi_tools::ToolResult {
            output: "one result".into(),
            is_error: false,
        }
    }
}

/// Duck mode is a conversation, and reaching outside is the one thing it can
/// do. Under the session's own `Write` mode a network call would park on an
/// approval — which is a prompt nobody asked for on a TUI and a hang in
/// headless — so the duck turn runs what its tier table kept without asking.
#[tokio::test]
async fn duck_mode_searches_without_asking_for_approval() {
    use titi_providers::{BlockId, StreamEvent, ToolCallRef};

    let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(vec![
            StreamEvent::ToolcallStart {
                id: BlockId::new("tool"),
                call: ToolCallRef {
                    call_id: "call-1".into(),
                    name: "web_search".into(),
                },
            },
            StreamEvent::ToolcallDelta {
                id: BlockId::new("tool"),
                json: r#"{"query":"what is a rubber duck"}"#.into(),
            },
            StreamEvent::ToolcallEnd {
                id: BlockId::new("tool"),
            },
            StreamEvent::Done {
                reason: titi_providers::StopReason::ToolUse,
            },
        ]),
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "here is what I found".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
    ]));

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeNetworkTool(
        "web_search",
        Arc::clone(&searches),
    )));
    tools.register(Arc::new(ShellProbeTool));
    let mut config = EngineConfig::new("primary");
    config.mode = SessionMode::Duck;
    // The session asks for everything above read tier. The duck turn is the
    // exception, and only for the network tier its table kept.
    config.approval_mode = titi_tools::ApprovalMode::Write;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), tools);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "look something up for me".into(),
        })
        .await
        .unwrap();

    let mut asked = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = engine.recv().await {
            match event {
                EngineEvent::ToolApprovalNeeded { .. } => asked = true,
                EngineEvent::TurnFinished { .. } => return,
                _ => {}
            }
        }
        panic!("the engine stopped before the turn finished");
    })
    .await
    .expect("a duck turn that parks on approval never finishes");

    assert!(!asked, "duck mode asked to approve its own search");
    assert_eq!(
        searches.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the search never ran"
    );
}

/// Duck mode picks by tier, not by the name `web_search`: a second network
/// tool is in the mode's reach for the same reason the first one is.
#[tokio::test]
async fn duck_mode_keeps_any_network_tool_whatever_it_is_called() {
    use titi_providers::{BlockId, StreamEvent, ToolCallRef};

    let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(vec![
            StreamEvent::ToolcallStart {
                id: BlockId::new("tool"),
                call: ToolCallRef {
                    call_id: "call-1".into(),
                    name: "fetch".into(),
                },
            },
            StreamEvent::ToolcallDelta {
                id: BlockId::new("tool"),
                json: r#"{"query":"https://example.invalid/page"}"#.into(),
            },
            StreamEvent::ToolcallEnd {
                id: BlockId::new("tool"),
            },
            StreamEvent::Done {
                reason: titi_providers::StopReason::ToolUse,
            },
        ]),
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "read the page".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
    ]));
    let captured = Arc::clone(&transport);

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(FakeNetworkTool("fetch", Arc::clone(&fetches))));
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(ShellProbeTool));
    let mut config = EngineConfig::new("primary");
    config.mode = SessionMode::Duck;
    config.approval_mode = titi_tools::ApprovalMode::Write;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), tools);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "go look that up".into(),
        })
        .await
        .unwrap();

    let mut asked = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = engine.recv().await {
            match event {
                EngineEvent::ToolApprovalNeeded { .. } => asked = true,
                EngineEvent::TurnFinished { .. } => return,
                _ => {}
            }
        }
        panic!("the engine stopped before the turn finished");
    })
    .await
    .expect("the duck turn never finished");

    let offered: Vec<String> = captured.requests()[0]
        .tools
        .iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert_eq!(
        offered,
        vec!["fetch".to_owned()],
        "duck mode is the network tier, no more and no less: {offered:?}"
    );
    assert!(!asked, "duck mode asked to approve a network call");
    assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Plan mode does not research. A mode narrows what a turn may do and never
/// widens it: a plan turn keeps the read tools, so adding the network tier
/// would make it the one turn that can read this repository and post it
/// somewhere — and it would have to do so unprompted, since `--mode plan`
/// runs headless where no approval arrives. The network tier stays with the
/// agent turn, where the user is asked.
#[tokio::test]
async fn plan_mode_leaves_the_network_tier_out() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = Arc::new(MockTransport::new(vec![done()]));
    let captured = Arc::clone(&transport);

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(ShellProbeTool));
    tools.register(Arc::new(FakeNetworkTool("web_search", Arc::clone(&calls))));
    let mut config = EngineConfig::new("primary");
    config.mode = SessionMode::Plan;
    config.approval_mode = titi_tools::ApprovalMode::Write;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), tools);

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "how would you fix the parser".into(),
        })
        .await
        .unwrap();

    let mut asked = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = engine.recv().await {
            match event {
                EngineEvent::ToolApprovalNeeded { .. } => asked = true,
                EngineEvent::TurnFinished { .. } => return,
                _ => {}
            }
        }
        panic!("the engine stopped before the turn finished");
    })
    .await
    .expect("the plan turn never finished");

    let offered: Vec<String> = captured.requests()[0]
        .tools
        .iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert_eq!(
        offered,
        vec!["echo".to_owned()],
        "a plan turn was handed something above read tier: {offered:?}"
    );
    // Nothing above read tier is offered, so nothing above read tier can
    // park the turn on an approval this surface may not be able to show.
    assert!(!asked, "a plan turn parked on an approval");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
