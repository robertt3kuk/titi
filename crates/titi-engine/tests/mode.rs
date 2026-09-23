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
