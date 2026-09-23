//! `/advisor` end to end: a consult is answered or reported, never dropped.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, ChatMessage, MockBody, MockTransport, Role, StopReason, StreamEvent, Transport,
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

fn resolver(model: &str, transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(HashMap::from([(model.to_owned(), transport)])))
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

fn asked(text: &str) -> ChatMessage {
    ChatMessage {
        role: Role::User,
        content: text.into(),
        tool_calls: Vec::new(),
    }
}

async fn next_advice(engine: &mut titi_engine::Engine) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = engine.recv().await {
            if matches!(
                event,
                EngineEvent::AdvisorAnswer { .. } | EngineEvent::AdvisorFailed { .. }
            ) {
                return event;
            }
        }
        panic!("the engine stopped before the consult answered");
    })
    .await
    .expect("the consult never answered")
}

/// The advisor sees the conversation and is handed no tools, so the second
/// opinion cannot turn into an action.
#[tokio::test]
async fn a_consult_answers_from_the_conversation_without_tools() {
    let transport = Arc::new(MockTransport::new(vec![says("the migration is missing")]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.restored_messages = vec![asked("rename the column, no migration needed")];
    let mut engine = EngineRuntime::start(config, resolver("primary", transport));

    engine
        .send(EngineCommand::Consult {
            question: Some("is that safe?".into()),
        })
        .await
        .unwrap();

    match next_advice(&mut engine).await {
        EngineEvent::AdvisorAnswer { text } => {
            assert_eq!(text.as_str(), "the migration is missing");
        }
        other => panic!("expected an answer, got {other:?}"),
    }
    let requests = captured.requests();
    let request = requests.first().expect("one request");
    assert!(request.tools.is_empty(), "{:?}", request.tools);
    let prompt = &request.messages.last().expect("a prompt").content;
    assert!(prompt.contains("rename the column"), "{prompt}");
    assert!(prompt.contains("is that safe?"), "{prompt}");
}

/// A consult with nothing to judge fails loudly: silence would read as an
/// advisor that had no objection.
#[tokio::test]
async fn a_consult_with_no_conversation_reports_a_failure() {
    let transport = Arc::new(MockTransport::new(vec![says("never asked")]));
    let mut engine =
        EngineRuntime::start(EngineConfig::new("primary"), resolver("primary", transport));

    engine
        .send(EngineCommand::Consult { question: None })
        .await
        .unwrap();

    match next_advice(&mut engine).await {
        EngineEvent::AdvisorFailed { reason } => {
            assert!(reason.contains("no conversation"), "{reason}");
        }
        other => panic!("expected a failed consult, got {other:?}"),
    }
}

/// An advisor that answers with nothing is a failed consult, not a quiet one.
#[tokio::test]
async fn an_empty_answer_comes_back_as_a_failed_consult() {
    let transport = Arc::new(MockTransport::new(vec![says("   ")]));
    let mut config = EngineConfig::new("primary");
    config.restored_messages = vec![asked("ship it")];
    let mut engine = EngineRuntime::start(config, resolver("primary", transport));

    engine
        .send(EngineCommand::Consult { question: None })
        .await
        .unwrap();

    match next_advice(&mut engine).await {
        EngineEvent::AdvisorFailed { reason } => {
            assert!(reason.contains("nothing"), "{reason}");
        }
        other => panic!("expected a failed consult, got {other:?}"),
    }
}
