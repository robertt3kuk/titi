//! A token cap the engine enforces: reaching it stops turns starting.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{BlockId, MockBody, MockTransport, StopReason, StreamEvent, Transport};

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

/// A turn spends against the cap, and the cap stops the next one.
#[tokio::test]
async fn reaching_the_cap_stops_the_next_turn_and_returns_it() {
    let transport = Arc::new(MockTransport::new(vec![
        says("a long enough answer to cost something"),
        says("this one must never be asked for"),
    ]));
    let captured = Arc::clone(&transport);
    let mut engine =
        EngineRuntime::start(EngineConfig::new("primary"), resolver("primary", transport));

    // One token: the first turn is under the cap when it starts and over it
    // when it ends, which is the only moment a budget can be checked.
    engine
        .send(EngineCommand::SetBudget { tokens: Some(1) })
        .await
        .unwrap();
    wait_for(&mut engine, |event| match event {
        EngineEvent::BudgetUpdated { limit, .. } => Some(*limit),
        _ => None,
    })
    .await;

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "first question".into(),
        })
        .await
        .unwrap();
    let (spent, limit) = wait_for(&mut engine, |event| match event {
        EngineEvent::BudgetExceeded { spent, limit } => Some((*spent, *limit)),
        _ => None,
    })
    .await;
    assert_eq!(limit, 1);
    assert!(spent > 0, "the turn spent nothing");

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "second question".into(),
        })
        .await
        .unwrap();
    let returned = wait_for(&mut engine, |event| match event {
        EngineEvent::PromptReturned { text } => Some(text.to_string()),
        _ => None,
    })
    .await;
    assert_eq!(returned, "second question");
    assert_eq!(
        captured.requests().len(),
        1,
        "a turn ran after the cap was reached"
    );

    // Raising the cap lets work start again.
    engine
        .send(EngineCommand::SetBudget {
            tokens: Some(1_000_000),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "third question".into(),
        })
        .await
        .unwrap();
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(captured.requests().len(), 2);
}

/// Without a cap nothing is refused, and the spend is still reported.
#[tokio::test]
async fn a_session_without_a_cap_is_never_stopped() {
    let transport = Arc::new(MockTransport::new(vec![says("fine")]));
    let mut engine =
        EngineRuntime::start(EngineConfig::new("primary"), resolver("primary", transport));

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "a question".into(),
        })
        .await
        .unwrap();
    let (spent, limit) = wait_for(&mut engine, |event| match event {
        EngineEvent::BudgetUpdated { spent, limit } => Some((*spent, *limit)),
        _ => None,
    })
    .await;
    assert!(spent > 0, "the turn spent nothing");
    assert_eq!(limit, None);
}
