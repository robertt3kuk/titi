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

// ---- the model-written summary ----------------------------------------------

use std::time::Duration;
use titi_engine::protocol::EngineCommand as Command;
use titi_providers::{
    ApiKind, ChatMessage, EventStream, RequestCtx, Role, TransportError, WireRequest,
};

/// What the scripted summary model answers.
fn written(text: &str) -> MockBody {
    answer(text, 700)
}

/// A provider that accepts every request and never answers.
struct Silent;

#[async_trait::async_trait]
impl Transport for Silent {
    fn api(&self) -> ApiKind {
        ApiKind::OpenAiCompletions
    }

    async fn stream(
        &self,
        _req: WireRequest,
        _ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        Ok(Box::pin(futures::stream::pending()))
    }
}

fn with_summary(window: u64) -> EngineConfig {
    let mut config = EngineConfig::new("primary");
    config.context_window = window;
    config.compaction = CompactionPolicy {
        threshold_percent: 80.0,
        keep_recent_tokens: 4,
        ..CompactionPolicy::default()
    };
    config
}

fn message(role: Role, text: &str) -> ChatMessage {
    ChatMessage {
        role,
        content: text.into(),
        ..Default::default()
    }
}

fn digest_of(request: &WireRequest) -> &ChatMessage {
    request
        .messages
        .iter()
        .find(|m| m.content.contains("<conversation-summary"))
        .expect("the request carries the digest")
}

#[tokio::test]
async fn the_session_model_writes_the_digest_mid_turn() {
    let transport = Arc::new(MockTransport::new(vec![
        echo_round(900),
        written("## Goal\nfold the context\n## Next step\nanswer the user"),
        answer("done", 40),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        with_summary(1_000),
        resolver(Arc::clone(&transport) as _),
        echo_registry(),
    );
    engine
        .send(Command::SubmitPrompt {
            text: "a question long enough to be folded away".into(),
        })
        .await
        .unwrap();
    let events = until_terminal(&mut engine).await;

    let (folded, _, strategy, _) = compacted(&events).expect("a fold");
    assert!(folded >= 1);
    assert_eq!(strategy, "remote");
    let summary_model = events.iter().find_map(|event| match event {
        EngineEvent::Compacted { summary_model, .. } => summary_model.clone(),
        _ => None,
    });
    assert_eq!(summary_model.as_deref(), Some("primary"));

    let requests = transport.requests();
    assert_eq!(requests.len(), 3, "round, summary, round");
    assert!(
        requests[1]
            .system
            .as_deref()
            .unwrap()
            .contains("[titi-compact]"),
        "the second request is the summary"
    );
    assert!(requests[1].tools.is_empty());
    let digest = digest_of(&requests[2]);
    assert_eq!(digest.role, Role::User);
    assert!(digest.content.contains("## Goal\nfold the context"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::StreamDelta { text, .. } if text == "done"))
    );
}

#[tokio::test]
async fn a_failed_summary_call_falls_back_and_the_turn_goes_on() {
    let transport = Arc::new(MockTransport::new(vec![
        echo_round(900),
        MockBody::Err(TransportError::Fatal {
            status: Some(500),
            message: "boom".into(),
            context_too_long: false,
        }),
        answer("done", 40),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        with_summary(1_000),
        resolver(Arc::clone(&transport) as _),
        echo_registry(),
    );
    engine
        .send(Command::SubmitPrompt {
            text: "a question long enough to be folded away".into(),
        })
        .await
        .unwrap();
    let events = until_terminal(&mut engine).await;

    let (_, _, strategy, _) = compacted(&events).expect("a fold");
    assert_eq!(strategy, "snapcompact");
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Notice { message } if message.contains("summary model failed") && message.contains("boom")
    )));
    let digest = transport.requests();
    assert!(
        digest_of(&digest[2])
            .content
            .contains("earlier message(s) folded")
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnFinished { .. }))
    );
}

/// Tool results were the weight: masking is enough, the model is never asked
/// and nothing is folded away.
#[tokio::test]
async fn masking_alone_ends_the_compaction_without_a_model_call() {
    let transport = Arc::new(MockTransport::new(vec![answer("ok", 50)]));
    let mut engine =
        EngineRuntime::start(with_summary(1_000), resolver(Arc::clone(&transport) as _));
    let call = ChatMessage {
        role: Role::Assistant,
        tool_calls: vec![ToolCallRef {
            call_id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"src/big.rs"}"#.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let result = ChatMessage {
        role: Role::Tool,
        content: "fn big() {}\n".repeat(1_000).into(),
        tool_call_id: Some("c1".into()),
        ..Default::default()
    };
    engine
        .send(Command::RestoreHistory {
            messages: vec![
                message(Role::User, "read the big file"),
                call,
                result,
                message(Role::Assistant, "read it"),
            ],
        })
        .await
        .unwrap();
    engine
        .send(Command::SubmitPrompt { text: "ok?".into() })
        .await
        .unwrap();
    let events = until_terminal(&mut engine).await;

    let (folded, masked, strategy, _) = compacted(&events).expect("masking fired");
    assert_eq!((folded, masked, strategy.as_str()), (0, 1, "mask"));
    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "no summary call");
    let stub = requests[0]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("the result is still in the request");
    assert_eq!(
        stub.content,
        "[tool result elided: read src/big.rs · 12000 chars · re-run to see it]"
    );
    assert_eq!(stub.tool_call_id.as_deref(), Some("c1"));
}

async fn until_compacted_or_notice(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), engine.recv()).await {
        let last = matches!(
            event,
            EngineEvent::Compacted { .. }
                | EngineEvent::Notice { .. }
                | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if last {
            break;
        }
    }
    events
}

/// Everything up to the fold's own report; a notice on the way is kept.
async fn until_compacted(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), engine.recv()).await {
        let last = matches!(
            event,
            EngineEvent::Compacted { .. } | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if last {
            break;
        }
    }
    events
}

fn old_history() -> Vec<ChatMessage> {
    vec![
        message(Role::User, "the first question, long enough to fold"),
        message(Role::Assistant, "the first answer"),
        message(Role::User, "now"),
    ]
}

/// `/compact [focus]` takes the same path as the automatic fold, and what it
/// leaves is what the next turn is sent.
#[tokio::test]
async fn manual_compaction_asks_the_model_with_the_focus() {
    let transport = Arc::new(MockTransport::new(vec![
        written("## Goal\nauth refactor"),
        answer("fine", 30),
    ]));
    let mut engine =
        EngineRuntime::start(with_summary(100_000), resolver(Arc::clone(&transport) as _));
    engine
        .send(Command::RestoreHistory {
            messages: old_history(),
        })
        .await
        .unwrap();
    engine
        .send(Command::Compact {
            focus: Some("auth".into()),
        })
        .await
        .unwrap();
    let events = until_compacted_or_notice(&mut engine).await;

    match events.last() {
        Some(EngineEvent::Compacted {
            folded,
            summary_model,
            strategy,
            ..
        }) => {
            assert_eq!(*folded, 2);
            assert_eq!(strategy, "remote");
            assert_eq!(summary_model.as_deref(), Some("primary"));
        }
        other => panic!("expected a compaction, got {other:?} in {events:?}"),
    }
    assert!(
        transport.requests()[0]
            .system
            .as_deref()
            .unwrap()
            .contains("auth")
    );

    engine
        .send(Command::SubmitPrompt {
            text: "next".into(),
        })
        .await
        .unwrap();
    until_terminal(&mut engine).await;
    let requests = transport.requests();
    let next = requests.last().unwrap();
    assert!(digest_of(next).content.contains("## Goal\nauth refactor"));
    assert!(
        !next
            .messages
            .iter()
            .any(|m| m.content == "the first answer"),
        "the folded messages are gone"
    );
}

#[tokio::test]
async fn a_cancel_stops_a_manual_compaction_and_leaves_the_history() {
    let silent: Arc<dyn Transport> = Arc::new(Silent);
    let mut engine = EngineRuntime::start(with_summary(100_000), resolver(silent));
    engine
        .send(Command::RestoreHistory {
            messages: old_history(),
        })
        .await
        .unwrap();
    engine.send(Command::Compact { focus: None }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine.send(Command::Cancel).await.unwrap();
    let events = until_compacted_or_notice(&mut engine).await;
    assert!(
        matches!(events.last(), Some(EngineEvent::Cancelled { .. })),
        "{events:?}"
    );
    // Nothing is reported after the cancel, and a later prompt still runs on
    // the unfolded history.
    let late = tokio::time::timeout(Duration::from_millis(400), engine.recv()).await;
    assert!(
        !matches!(late, Ok(Some(EngineEvent::Compacted { .. }))),
        "{late:?}"
    );
}

#[tokio::test]
async fn manual_compaction_waits_for_a_running_turn() {
    let silent: Arc<dyn Transport> = Arc::new(Silent);
    let mut engine = EngineRuntime::start(with_summary(100_000), resolver(silent));
    engine
        .send(Command::SubmitPrompt {
            text: "hang".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine.send(Command::Compact { focus: None }).await.unwrap();
    let mut said = None;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), engine.recv()).await {
        if let EngineEvent::Notice { message } = event {
            said = Some(message);
            break;
        }
    }
    assert!(said.is_some_and(|m| m.contains("a turn is running")));
    engine.send(Command::Cancel).await.unwrap();
}

struct TwoModels(HashMap<String, Arc<dyn Transport>>);

impl TransportResolver for TwoModels {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self.0
            .get(model)
            .cloned()
            .map(|transport| ResolvedModel::without_credential(model, transport))
            .ok_or_else(|| RegistryError::UnknownModel(model.into()))
    }
}

/// `modelRoles.compact` moves the summary to another model; one that does not
/// resolve leaves it on the chat model.
#[tokio::test]
async fn the_compact_role_picks_the_summary_model() {
    for (role, expected) in [("cheap", "cheap"), ("nowhere", "primary")] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yml"),
            format!("modelRoles:\n  compact: {role}\n"),
        )
        .unwrap();
        let chat = Arc::new(MockTransport::new(vec![answer("fine", 30)]));
        let cheap = Arc::new(MockTransport::new(vec![written("## Goal\nrole")]));
        let resolver = Arc::new(TwoModels(
            [
                (
                    "primary".to_owned(),
                    Arc::clone(&chat) as Arc<dyn Transport>,
                ),
                ("cheap".to_owned(), Arc::clone(&cheap) as Arc<dyn Transport>),
            ]
            .into_iter()
            .collect(),
        ));
        let mut config = with_summary(100_000);
        config.agent_dir = Some(dir.path().to_path_buf());
        config.workspace_root = Some(dir.path().to_path_buf());
        let mut engine = EngineRuntime::start(config, resolver);
        engine
            .send(Command::RestoreHistory {
                messages: old_history(),
            })
            .await
            .unwrap();
        engine.send(Command::Compact { focus: None }).await.unwrap();
        let events = until_compacted(&mut engine).await;
        let by = events.iter().find_map(|event| match event {
            EngineEvent::Compacted { summary_model, .. } => summary_model.clone(),
            _ => None,
        });
        match expected {
            "cheap" => {
                assert_eq!(by.as_deref(), Some("cheap"), "{events:?}");
                assert_eq!(cheap.requests().len(), 1);
                assert!(chat.requests().is_empty());
            }
            _ => {
                // The chat transport only had a prompt-sized answer scripted,
                // which it hands to the summary call. The slot that could not
                // be used is said once, ahead of the fold.
                assert_eq!(by.as_deref(), Some("primary"), "{events:?}");
                assert!(cheap.requests().is_empty());
                let said: Vec<&str> = events
                    .iter()
                    .filter_map(|event| match event {
                        EngineEvent::Notice { message } => Some(message.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(said.len(), 1, "{events:?}");
                assert!(
                    said[0].starts_with("modelRoles.compact: `nowhere` cannot be used"),
                    "{said:?}"
                );
            }
        }
    }
}
