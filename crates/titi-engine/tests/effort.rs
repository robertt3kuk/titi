#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The session's reasoning effort, from the command to the request body.
//!
//! The transport here records every `WireRequest` it is handed, so what the
//! model would be sent is read off the request, not inferred from the setting.

use std::sync::{Arc, Mutex};

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    ApiKind, BlockId, Effort, EventStream, ModelCompat, Reasoning, RequestCtx, StopReason,
    StreamEvent, Transport, TransportError, WireRequest,
};

struct Recording {
    api: ApiKind,
    requests: Mutex<Vec<WireRequest>>,
}

#[async_trait::async_trait]
impl Transport for Recording {
    fn api(&self) -> ApiKind {
        self.api
    }

    async fn stream(
        &self,
        req: WireRequest,
        _ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        self.requests.lock().unwrap().push(req);
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "ok".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])))
    }
}

/// Every model it is asked for resolves to the one transport, with the
/// reasoning declaration of that model's name: `reasoner` has a ladder of
/// low..high, `plain` declares none.
struct Resolver(Arc<Recording>);

impl TransportResolver for Resolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        let mut resolved =
            ResolvedModel::without_credential(model, Arc::clone(&self.0) as Arc<dyn Transport>);
        if model == "reasoner" {
            resolved.reasoning = Some(ModelCompat {
                efforts: vec![Effort::Low, Effort::Medium, Effort::High],
                ..ModelCompat::default()
            });
        }
        Ok(resolved)
    }
}

fn recording(api: ApiKind) -> Arc<Recording> {
    Arc::new(Recording {
        api,
        requests: Mutex::new(Vec::new()),
    })
}

async fn run_turn(engine: &mut titi_engine::Engine, prompt: &str) {
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
}

async fn effort_report(engine: &mut titi_engine::Engine) -> EngineEvent {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), engine.recv())
            .await
            .expect("an event")
            .expect("the engine is up");
        if matches!(event, EngineEvent::EffortChanged { .. }) {
            return event;
        }
    }
}

fn sent(transport: &Recording) -> Vec<Option<Reasoning>> {
    transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|req| req.reasoning.clone())
        .collect()
}

#[tokio::test]
async fn nothing_is_sent_until_an_effort_is_set() {
    let transport = recording(ApiKind::OpenAiCompletions);
    let mut engine = EngineRuntime::start(
        EngineConfig::new("reasoner"),
        Arc::new(Resolver(Arc::clone(&transport))),
    );
    run_turn(&mut engine, "hi").await;
    assert_eq!(sent(&transport), vec![None]);
}

#[tokio::test]
async fn set_effort_reaches_the_next_request_clamped_to_the_models_ladder() {
    let transport = recording(ApiKind::AnthropicMessages);
    let mut engine = EngineRuntime::start(
        EngineConfig::new("reasoner"),
        Arc::new(Resolver(Arc::clone(&transport))),
    );
    engine
        .send(EngineCommand::SetEffort {
            effort: Some(Effort::Xhigh),
        })
        .await
        .unwrap();
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: Some(Effort::Xhigh),
            applied: Some(Effort::High),
            cache_miss: false,
        },
        "before any turn there is no cache to lose"
    );
    run_turn(&mut engine, "hi").await;
    let requests = sent(&transport);
    assert_eq!(requests.len(), 1);
    let reasoning = requests[0].as_ref().expect("an effort was asked for");
    assert_eq!(reasoning.effort, Effort::High);
}

#[tokio::test]
async fn a_model_that_declares_no_effort_is_sent_none_and_the_surface_is_told() {
    let transport = recording(ApiKind::OpenAiCompletions);
    let mut engine = EngineRuntime::start(
        EngineConfig::new("plain"),
        Arc::new(Resolver(Arc::clone(&transport))),
    );
    engine
        .send(EngineCommand::SetEffort {
            effort: Some(Effort::High),
        })
        .await
        .unwrap();
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: Some(Effort::High),
            applied: None,
            cache_miss: false,
        }
    );
    run_turn(&mut engine, "hi").await;
    assert_eq!(sent(&transport), vec![None]);
}

#[tokio::test]
async fn changing_effort_after_a_turn_says_the_cache_is_lost_and_clearing_sends_nothing() {
    let transport = recording(ApiKind::OpenAiResponses);
    let mut config = EngineConfig::new("reasoner");
    config.effort = Some(Effort::Low);
    let mut engine = EngineRuntime::start(config, Arc::new(Resolver(Arc::clone(&transport))));
    // The setting the session started with is reported, quietly.
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: Some(Effort::Low),
            applied: Some(Effort::Low),
            cache_miss: false,
        }
    );
    run_turn(&mut engine, "one").await;

    engine
        .send(EngineCommand::SetEffort {
            effort: Some(Effort::High),
        })
        .await
        .unwrap();
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: Some(Effort::High),
            applied: Some(Effort::High),
            cache_miss: true,
        }
    );
    // The same value again changes nothing on the wire.
    engine
        .send(EngineCommand::SetEffort {
            effort: Some(Effort::High),
        })
        .await
        .unwrap();
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: Some(Effort::High),
            applied: Some(Effort::High),
            cache_miss: false,
        }
    );
    run_turn(&mut engine, "two").await;

    engine
        .send(EngineCommand::SetEffort { effort: None })
        .await
        .unwrap();
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: None,
            applied: None,
            cache_miss: true,
        }
    );
    run_turn(&mut engine, "three").await;

    let efforts: Vec<Option<Effort>> = sent(&transport)
        .into_iter()
        .map(|reasoning| reasoning.map(|r| r.effort))
        .collect();
    assert_eq!(
        efforts,
        vec![Some(Effort::Low), Some(Effort::High), None],
        "each request carries the effort in force when it was made"
    );
}

#[tokio::test]
async fn switching_model_reports_what_the_new_model_is_sent() {
    let transport = recording(ApiKind::OpenAiCompletions);
    let mut config = EngineConfig::new("reasoner");
    config.effort = Some(Effort::Medium);
    let mut engine = EngineRuntime::start(config, Arc::new(Resolver(Arc::clone(&transport))));
    effort_report(&mut engine).await;

    engine
        .send(EngineCommand::SwitchModel {
            model: "plain".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        effort_report(&mut engine).await,
        EngineEvent::EffortChanged {
            effort: Some(Effort::Medium),
            applied: None,
            cache_miss: false,
        },
        "the setting stands, and the model it moved to takes none"
    );
}
