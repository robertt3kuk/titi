#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Compaction as a turn sees it: when it fires, what the request carries
//! afterwards, and what a surface is told. Every provider here is scripted.

use std::collections::HashMap;
use std::sync::Arc;

use titi_core::compaction::{CompactionPolicy, Strategy};
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, TokenUsage, ToolCallRef, Transport,
};
use titi_tools::{EchoTool, ToolRegistry};

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

fn echo_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools
}

fn usage(prompt: u64) -> StreamEvent {
    StreamEvent::Usage(TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: 5,
        cached_tokens: 0,
    })
}

fn text(text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        id: BlockId::new("text"),
        text: text.into(),
    }
}

fn done(reason: StopReason) -> StreamEvent {
    StreamEvent::Done { reason }
}

/// One tool round: a call to `echo`, then the provider's prompt count.
fn echo_round(prompt: u64) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: "echo".into(),
                ..Default::default()
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: r#"{"text":"pong"}"#.into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
        usage(prompt),
        done(StopReason::ToolUse),
    ])
}

fn answer(words: &str, prompt: u64) -> MockBody {
    MockBody::Events(vec![text(words), usage(prompt), done(StopReason::Stop)])
}

async fn until_terminal(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let terminal = matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

fn compacted(events: &[EngineEvent]) -> Option<(u32, u32, String, u64)> {
    events.iter().find_map(|event| match event {
        EngineEvent::Compacted {
            folded,
            masked,
            strategy,
            tokens_before,
            ..
        } => Some((*folded, *masked, strategy.to_string(), *tokens_before)),
        _ => None,
    })
}

/// A window the threshold sits well inside of, and no model-written summary:
/// the structural digest is the strategy under test here.
fn modelless(window: u64) -> EngineConfig {
    let mut config = EngineConfig::new("primary");
    config.context_window = window;
    config.compaction = CompactionPolicy {
        threshold_percent: 80.0,
        keep_recent_tokens: 4,
        method_order: vec![Strategy::SnapCompact],
    };
    config
}

/// The estimate sees a request of a few tokens in a thousand-token window.
/// The provider counted the first round at 900, which is what the second
/// round is judged by.
#[tokio::test]
async fn the_providers_prompt_count_arms_the_fold_mid_turn() {
    let transport = Arc::new(MockTransport::new(vec![
        echo_round(900),
        answer("done", 40),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        modelless(1_000),
        resolver(Arc::clone(&transport) as _),
        echo_registry(),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "a question long enough to be folded away".into(),
        })
        .await
        .unwrap();
    let events = until_terminal(&mut engine).await;

    let (folded, _, strategy, tokens_before) =
        compacted(&events).expect("the provider's count arms the fold");
    assert!(folded >= 1);
    assert_eq!(strategy, "snapcompact");
    assert!(
        tokens_before >= 900,
        "the event carries the provider's figure, not the estimate: {tokens_before}"
    );
    assert_eq!(transport.requests().len(), 2);
}

/// Below the threshold by the provider's own count, nothing happens, however
/// the estimate looks.
#[tokio::test]
async fn a_low_provider_count_leaves_the_request_alone() {
    let transport = Arc::new(MockTransport::new(vec![
        echo_round(100),
        answer("done", 40),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        modelless(1_000),
        resolver(Arc::clone(&transport) as _),
        echo_registry(),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "a question long enough to be folded away".into(),
        })
        .await
        .unwrap();
    let events = until_terminal(&mut engine).await;
    assert_eq!(compacted(&events), None, "{events:?}");
}

/// The first round of a turn is the one a long history is most likely to
/// overflow on, and it has no count of its own yet: it is judged by the one
/// the last turn ended with.
#[tokio::test]
async fn the_last_turns_count_arms_the_next_turns_first_round() {
    let transport = Arc::new(MockTransport::new(vec![
        answer("the first answer", 900),
        answer("the second answer", 950),
    ]));
    let mut engine = EngineRuntime::start(modelless(1_000), resolver(Arc::clone(&transport) as _));
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "the first question, long enough to fold".into(),
        })
        .await
        .unwrap();
    let first = until_terminal(&mut engine).await;
    assert_eq!(compacted(&first), None, "{first:?}");

    engine
        .send(EngineCommand::SubmitPrompt {
            text: "second".into(),
        })
        .await
        .unwrap();
    let second = until_terminal(&mut engine).await;

    let (folded, _, _, tokens_before) =
        compacted(&second).expect("900 of 1000 tokens, as the provider counted it");
    assert!(folded >= 1);
    assert!(tokens_before >= 900, "{tokens_before}");
}
