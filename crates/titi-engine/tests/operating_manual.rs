#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The built-in operating manual leads the system prompt of every mode that
//! can see the repository, and never reaches a repo-blind duck.

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::protocol::SessionMode;
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{MockBody, MockTransport, Role, StopReason, StreamEvent, Transport};

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

fn done() -> MockBody {
    MockBody::Events(vec![StreamEvent::Done {
        reason: StopReason::Stop,
    }])
}

async fn finished(engine: &mut titi_engine::Engine) {
    while let Some(event) = engine.recv().await {
        if matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        ) {
            return;
        }
    }
    panic!("the engine stopped before the turn finished");
}

/// The system prompt of each request the transport saw, one turn per prompt.
async fn system_prompts(mode: SessionMode, turns: usize) -> Vec<String> {
    let agent = tempfile::tempdir().unwrap();
    std::fs::write(agent.path().join("SOUL.md"), "IDENTITY-MARKER").unwrap();
    let transport = Arc::new(MockTransport::new((0..turns).map(|_| done()).collect()));
    let captured = Arc::clone(&transport);
    let resolver: Arc<dyn TransportResolver> = Arc::new(MapResolver(
        [("primary".to_owned(), transport as Arc<dyn Transport>)]
            .into_iter()
            .collect(),
    ));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(agent.path().to_path_buf());
    config.mode = mode;
    let mut engine = EngineRuntime::start(config, resolver);
    for turn in 0..turns {
        engine
            .send(EngineCommand::SubmitPrompt {
                text: format!("turn {turn}").into(),
            })
            .await
            .unwrap();
        finished(&mut engine).await;
    }
    captured
        .requests()
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .find(|message| message.role == Role::System)
                .map(|message| message.content.to_string())
                .unwrap_or_default()
        })
        .collect()
}

#[tokio::test]
async fn agent_and_plan_turns_lead_with_the_manual_ahead_of_the_soul() {
    for mode in [SessionMode::Agent, SessionMode::Plan] {
        let system = &system_prompts(mode, 1).await[0];
        assert!(
            system.starts_with(titi_soul::OPERATING_MANUAL),
            "{mode:?}: {system}"
        );
        let manual_at = system.find("# Operating manual").unwrap();
        let soul_at = system.find("IDENTITY-MARKER").unwrap();
        assert!(
            manual_at < soul_at,
            "{mode:?}: manual must precede the soul"
        );
    }
}

#[tokio::test]
async fn a_duck_turn_never_sees_the_manual() {
    let system = &system_prompts(SessionMode::Duck, 1).await[0];
    assert!(system.contains("IDENTITY-MARKER"), "{system}");
    assert!(!system.contains("Operating manual"), "{system}");
    assert!(!system.contains("`project` tool"), "{system}");
}

/// A byte that moves in the prompt's prefix re-bills everything behind it, so
/// the manual — and the whole system prompt — is identical on every turn.
#[tokio::test]
async fn the_system_prompt_is_byte_stable_across_turns() {
    let prompts = system_prompts(SessionMode::Agent, 3).await;
    assert_eq!(prompts.len(), 3);
    assert!(prompts[0].starts_with(titi_soul::OPERATING_MANUAL));
    assert_eq!(prompts[0], prompts[1]);
    assert_eq!(prompts[1], prompts[2]);
}
