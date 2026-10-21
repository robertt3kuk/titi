#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The submitted prompt is the query the turn's memory recall is asked with.

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{MockBody, MockTransport, StopReason, StreamEvent, Transport};

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

/// What the model is sent as the newest user message for `prompt`, with two
/// stored memories of which only one is about sqlite.
async fn newest_message_for(prompt: &str) -> String {
    let agent = tempfile::tempdir().unwrap();
    let index = titi_memory::index::MemoryIndex::open(agent.path()).unwrap();
    // Used more, so an empty query ranks it first and keeps it.
    for _ in 0..3 {
        index
            .remember("context", "helm chart values live in deploy/", "", &[])
            .unwrap();
    }
    index
        .remember("gotcha", "sqlite wal checkpoint blocks readers", "", &[])
        .unwrap();
    drop(index);

    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let captured = Arc::clone(&transport);
    let resolver: Arc<dyn TransportResolver> = Arc::new(MapResolver(
        [("primary".to_owned(), transport as Arc<dyn Transport>)]
            .into_iter()
            .collect(),
    ));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(agent.path().to_path_buf());
    let mut engine = EngineRuntime::start(config, resolver);
    engine
        .send(EngineCommand::SubmitPrompt {
            text: prompt.into(),
        })
        .await
        .unwrap();
    while let Some(event) = engine.recv().await {
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    let requests = captured.requests();
    requests[0]
        .messages
        .last()
        .expect("a user message")
        .content
        .to_string()
}

#[tokio::test]
async fn the_prompt_picks_which_memories_ride_the_newest_message() {
    let about_sqlite = newest_message_for("why does the sqlite wal checkpoint stall?").await;
    assert!(
        about_sqlite.contains("sqlite wal checkpoint blocks readers"),
        "{about_sqlite}"
    );
    assert!(
        !about_sqlite.contains("helm chart"),
        "a note the prompt does not match stays out: {about_sqlite}"
    );

    // Nothing in the prompt matches a stored note: the fallback is the same
    // use-ranked recall an empty query gave.
    let unrelated = newest_message_for("hi").await;
    assert!(unrelated.contains("helm chart"), "{unrelated}");
}
