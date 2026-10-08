#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A token cap the engine enforces: reaching it stops turns starting.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, ModelPrice, RegistryError,
    ResolvedModel, TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, TokenUsage, Transport,
    TransportError,
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

/// A resolver that states what each model costs, and nothing for one it cannot
/// price: the two states a money cap has to tell apart.
struct PriceTable {
    transports: HashMap<String, Arc<dyn Transport>>,
    prices: HashMap<String, ModelPrice>,
}

impl TransportResolver for PriceTable {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self.transports
            .get(model)
            .cloned()
            .map(|transport| ResolvedModel::without_credential(model, transport))
            .ok_or_else(|| RegistryError::UnknownModel(model.into()))
    }

    fn price(&self, model: &str) -> Option<ModelPrice> {
        self.prices.get(model).copied()
    }
}

/// A dollar per million tokens each way, so one token is one micro-dollar and
/// a test can state the arithmetic it expects in its own head.
fn a_dollar_a_megatoken() -> ModelPrice {
    ModelPrice {
        input: 1_000_000,
        output: 1_000_000,
        cached_input: None,
    }
}

/// A turn that reports its own usage, so the money is a number a test chose
/// rather than the engine's estimate of one.
fn says_paid(text: &str, prompt: u64, completion: u64) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("0"),
            text: text.into(),
        },
        StreamEvent::Usage(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: 0,
        }),
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
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

/// A money cap stops the next turn exactly as a token cap does, and the turn
/// that reached it states what it cost.
///
/// The figure is asserted twice over: once as the event's own number, and once
/// against the arithmetic the footer would do from the same three token counts
/// — which is the point of the engine keeping the ledger rather than a surface
/// recomputing it.
#[tokio::test]
async fn reaching_the_money_cap_stops_the_next_turn_and_returns_it() {
    let transport = Arc::new(MockTransport::new(vec![
        says_paid("a long enough answer to cost something", 11, 22),
        says_paid("this one must never be asked for", 0, 0),
    ]));
    let captured = Arc::clone(&transport);
    let resolver: Arc<dyn TransportResolver> = Arc::new(PriceTable {
        transports: HashMap::from([("primary".to_owned(), transport as Arc<dyn Transport>)]),
        prices: HashMap::from([("primary".to_owned(), a_dollar_a_megatoken())]),
    });
    let mut engine = EngineRuntime::start(EngineConfig::new("primary"), resolver);

    // One micro-dollar: the first turn is under the cap when it starts and
    // over it when it ends, which is the only moment a budget is checked.
    engine
        .send(EngineCommand::SetMoneyBudget { micro_usd: Some(1) })
        .await
        .unwrap();
    let limit = wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetUpdated {
            limit_micro_usd, ..
        } => Some(*limit_micro_usd),
        _ => None,
    })
    .await;
    assert_eq!(limit, Some(1));

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "first question".into(),
        })
        .await
        .unwrap();
    let usage = wait_for(&mut engine, |event| match event {
        EngineEvent::TurnUsage { cost_micro_usd, .. } => Some(*cost_micro_usd),
        _ => None,
    })
    .await;
    assert_eq!(
        usage,
        Some(a_dollar_a_megatoken().cost_micro_usd(11, 0, 22)),
        "the turn's cost is the price times its own tokens"
    );

    let (spent_micro_usd, limit_micro_usd) = wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetExceeded {
            spent_micro_usd,
            limit_micro_usd,
        } => Some((*spent_micro_usd, *limit_micro_usd)),
        _ => None,
    })
    .await;
    assert_eq!(limit_micro_usd, 1);
    assert_eq!(
        spent_micro_usd, 33,
        "11 + 22 tokens at one micro-dollar each"
    );

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
        "a turn ran after the money cap was reached"
    );

    // Raising the cap lets work start again.
    engine
        .send(EngineCommand::SetMoneyBudget {
            micro_usd: Some(1_000_000),
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

/// A money cap over a model with no price is refused, and the refusal names
/// the model.
///
/// Accepting it would be a promise the engine cannot keep: an unpriced model
/// is not a free one, so a bound whose spend is invisible is not a bound.
#[tokio::test]
async fn an_unpriced_model_refuses_a_money_cap() {
    let transport = Arc::new(MockTransport::new(vec![says_paid("fine", 11, 22)]));
    let resolver: Arc<dyn TransportResolver> = Arc::new(PriceTable {
        transports: HashMap::from([("primary".to_owned(), transport as Arc<dyn Transport>)]),
        prices: HashMap::new(),
    });
    let mut engine = EngineRuntime::start(EngineConfig::new("primary"), resolver);

    engine
        .send(EngineCommand::SetMoneyBudget {
            micro_usd: Some(1_000_000),
        })
        .await
        .unwrap();
    let model = wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetUnpriced { model } => Some(model.to_string()),
        _ => None,
    })
    .await;
    assert_eq!(model, "primary");

    // Nothing was capped: the turn runs, and the state says so.
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "a question".into(),
        })
        .await
        .unwrap();
    // The turn's own report first: the engine's session update follows it.
    let usage = wait_for(&mut engine, |event| match event {
        EngineEvent::TurnUsage { cost_micro_usd, .. } => Some(*cost_micro_usd),
        _ => None,
    })
    .await;
    assert_eq!(usage, None, "an unpriced turn states no figure");

    let limit = wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetUpdated {
            limit_micro_usd, ..
        } => Some(*limit_micro_usd),
        _ => None,
    })
    .await;
    assert_eq!(limit, None, "a refused cap must not be in force");
}

/// A cap set while the model was priced stays in force when a fallback takes
/// the turn somewhere the engine cannot measure, and the engine says so.
///
/// The cap is the user's word and is not withdrawn; what changes is that the
/// engine can no longer see the spend it is bounding, which is exactly what
/// the surface has to be told.
#[tokio::test]
async fn a_cap_survives_a_switch_to_an_unpriced_model_and_says_so() {
    let primary = Arc::new(MockTransport::new(vec![
        MockBody::Err(TransportError::Retryable {
            status: Some(429),
            message: "limited".into(),
        }),
        MockBody::Err(TransportError::Retryable {
            status: Some(429),
            message: "limited".into(),
        }),
    ]));
    let backup = Arc::new(MockTransport::new(vec![says_paid("backup", 11, 22)]));
    let resolver: Arc<dyn TransportResolver> = Arc::new(PriceTable {
        transports: HashMap::from([
            ("primary".to_owned(), primary as Arc<dyn Transport>),
            ("backup".to_owned(), backup as Arc<dyn Transport>),
        ]),
        prices: HashMap::from([("primary".to_owned(), a_dollar_a_megatoken())]),
    });
    let mut config = EngineConfig::new("primary");
    config.fallback_models = vec!["backup".into()];
    config.max_transient_retries = 1;
    let mut engine = EngineRuntime::start(config, resolver);

    engine
        .send(EngineCommand::SetMoneyBudget {
            micro_usd: Some(1_000_000),
        })
        .await
        .unwrap();
    wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetUpdated {
            limit_micro_usd, ..
        } => Some(*limit_micro_usd),
        _ => None,
    })
    .await;

    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let model = wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetUnpriced { model } => Some(model.to_string()),
        _ => None,
    })
    .await;
    assert_eq!(model, "backup");

    let usage = wait_for(&mut engine, |event| match event {
        EngineEvent::TurnUsage { cost_micro_usd, .. } => Some(*cost_micro_usd),
        _ => None,
    })
    .await;
    assert_eq!(usage, None, "the turn ran unpriced and states no figure");

    let limit = wait_for(&mut engine, |event| match event {
        EngineEvent::MoneyBudgetUpdated {
            limit_micro_usd, ..
        } => Some(*limit_micro_usd),
        _ => None,
    })
    .await;
    assert_eq!(
        limit,
        Some(1_000_000),
        "the cap is the user's word and stays"
    );
}
