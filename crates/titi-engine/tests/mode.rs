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
