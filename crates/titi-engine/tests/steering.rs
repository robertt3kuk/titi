//! Steering that arrives after a turn's last step boundary.
//!
//! A turn drains the steering queue before each provider attempt, so a
//! message typed while the final answer streams has no boundary left to land
//! on. It must still be answered, or handed back on a cancel — never left in
//! the queue to surface after some later prompt.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    ApiKind, BlockId, EventStream, RequestCtx, Role, StopReason, StreamEvent, Transport,
    TransportError, WireRequest,
};
use tokio::sync::Notify;

/// The first request streams one delta and then holds until `release`; every
/// later request answers at once. Holding the first stream open is what puts
/// a steer after the turn's last drain without racing the scheduler.
struct GatedTransport {
    release: Arc<Notify>,
    requests: Mutex<Vec<WireRequest>>,
}

impl GatedTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            release: Arc::new(Notify::new()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<WireRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn text(text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        id: BlockId::new("text"),
        text: text.into(),
    }
}

fn done() -> StreamEvent {
    StreamEvent::Done {
        reason: StopReason::Stop,
    }
}

#[async_trait::async_trait]
impl Transport for GatedTransport {
    fn api(&self) -> ApiKind {
        ApiKind::OpenAiCompletions
    }

    async fn stream(
        &self,
        req: WireRequest,
        _ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(req);
            requests.len() == 1
        };
        if !first {
            return Ok(Box::pin(futures::stream::iter(vec![text("on it"), done()])));
        }
        let release = Arc::clone(&self.release);
        Ok(Box::pin(
            futures::stream::iter(vec![text("partial")]).chain(futures::stream::once(async move {
                release.notified().await;
                done()
            })),
        ))
    }
}

struct One(Arc<GatedTransport>);

impl TransportResolver for One {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        Ok(ResolvedModel::without_credential(
            model,
            Arc::clone(&self.0) as Arc<dyn Transport>,
        ))
    }
}

/// Starts a turn, waits until its first stream is open, steers it, and makes
/// sure the engine has taken the steer before the test goes on.
async fn steer_mid_answer(transport: &Arc<GatedTransport>) -> titi_engine::Engine {
    let mut engine = EngineRuntime::start(
        EngineConfig::new("primary"),
        Arc::new(One(Arc::clone(transport))),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "question".into(),
        })
        .await
        .unwrap();
    while let Some(event) = engine.recv().await {
        if matches!(&event, EngineEvent::StreamDelta { text, .. } if text == "partial") {
            break;
        }
    }
    engine
        .send(EngineCommand::Steer {
            text: "and also this".into(),
        })
        .await
        .unwrap();
    // Commands are handled in order, so once this answers the steer is queued.
    engine.send(EngineCommand::DescribeContext).await.unwrap();
    while let Some(event) = engine.recv().await {
        if matches!(event, EngineEvent::ContextBreakdown { .. }) {
            break;
        }
    }
    engine
}

#[tokio::test]
async fn a_steer_that_missed_the_last_round_is_answered_next() {
    let transport = GatedTransport::new();
    let mut engine = steer_mid_answer(&transport).await;
    transport.release.notify_one();

    let mut finished = 0;
    let mut answered = false;
    while finished < 2 {
        let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(5), engine.recv())
            .await
            .unwrap()
        else {
            break;
        };
        match event {
            EngineEvent::TurnFinished { .. } => finished += 1,
            EngineEvent::StreamDelta { text, .. } if text == "on it" => answered = true,
            EngineEvent::Failed { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    assert!(answered, "the steer was never answered");

    let requests = transport.requests();
    assert_eq!(requests.len(), 2, "the steer runs as the next request");
    let last = requests[1].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content, "and also this");
    // It follows the answer it was typed under, not a prompt sent later.
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant && message.content == "partial")
    );
}

#[tokio::test]
async fn a_cancel_hands_unread_steering_back() {
    let transport = GatedTransport::new();
    let mut engine = steer_mid_answer(&transport).await;
    engine.send(EngineCommand::Cancel).await.unwrap();

    let mut returned = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(300), engine.recv()).await
    {
        if let EngineEvent::PromptReturned { text } = event {
            returned.push(text.to_string());
        }
    }
    assert_eq!(returned, ["and also this"]);

    // Nothing is left to ride along with the next prompt.
    transport.release.notify_one();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "next".into(),
        })
        .await
        .unwrap();
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), engine.recv()).await
    {
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    let requests = transport.requests();
    let next = requests.last().unwrap();
    assert!(
        next.messages
            .iter()
            .all(|message| message.content != "and also this"),
        "{:?}",
        next.messages
    );
}
