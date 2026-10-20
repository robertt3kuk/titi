#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A request the provider rejects for being longer than the model's window:
//! the engine folds the history once and asks again, and a second rejection
//! names the window instead of repeating the provider's bare sentence.
//!
//! One family end to end — OpenAI completions, whose 400 carries
//! `context_length_exceeded` — because the per-family wording is covered by
//! `wire::tests::a_context_length_rejection_is_recognised_in_every_family`.

use std::sync::Arc;

use serde_json::Value;
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    ApiKind, ChatMessage, FamilyTransport, HttpFetch, MockFetch, MockFetchResponse, Role,
    Transport, TransportError,
};
use titi_tools::ToolRegistry;

struct FixedResolver(Arc<dyn Transport>);

impl TransportResolver for FixedResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        Ok(ResolvedModel::without_credential(
            model,
            Arc::clone(&self.0),
        ))
    }
}

fn replay(bodies: Vec<MockFetchResponse>) -> (Arc<dyn Transport>, Arc<MockFetch>) {
    let fetch = Arc::new(MockFetch::new(
        bodies.into_iter().map(Ok).collect::<Vec<_>>(),
    ));
    let transport: Arc<dyn Transport> = Arc::new(FamilyTransport::new(
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9/v1",
        Arc::clone(&fetch) as Arc<dyn HttpFetch>,
    ));
    (transport, fetch)
}

fn rejected_too_long() -> MockFetchResponse {
    MockFetchResponse::sse(vec![
        r#"data: {"error":{"message":"This model's maximum context length is 128000 tokens","code":"context_length_exceeded"}}"#
            .to_owned()
            + "\n\n",
    ])
    .with_status(400)
}

fn answered(body: &str) -> MockFetchResponse {
    MockFetchResponse::sse(vec![
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{body}\"}},\"finish_reason\":null}}]}}\n\n"
        ),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: [DONE]\n\n".to_owned(),
    ])
}

/// The policy the engine's own fold tests use: the default keeps 20 000 recent
/// tokens, and this fixture is a few hundred, so nothing would ever be folded.
fn foldable() -> EngineConfig {
    let mut config = EngineConfig::new("primary");
    config.compaction.keep_recent_tokens = 4;
    config
}

/// A history long enough that a forced fold has something to fold.
fn seeded_history() -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    for turn in 0..6 {
        messages.push(ChatMessage {
            role: Role::User,
            content: format!("question {turn}, long enough to be worth folding into a digest")
                .into(),
            ..Default::default()
        });
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: format!("answer {turn}, also long enough that the fold has real text").into(),
            ..Default::default()
        });
    }
    messages
}

fn messages_of(body: &Value) -> Vec<Value> {
    body["messages"].as_array().cloned().unwrap_or_default()
}

async fn collect_until_terminal(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
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

/// The first request is rejected for length, the engine folds once, and the
/// retry succeeds — with a *shorter* history than the request that was
/// rejected.
#[tokio::test]
async fn a_too_long_request_is_folded_once_and_asked_again() {
    let (transport, fetch) = replay(vec![rejected_too_long(), answered("done")]);
    let mut engine = EngineRuntime::start_with_tools(
        foldable(),
        Arc::new(FixedResolver(transport)),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::RestoreHistory {
            messages: seeded_history(),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "and now?".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnFinished { .. })),
        "the turn finished on the retry: {events:?}"
    );

    let requests: Vec<Value> = fetch
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| {
            request
                .body
                .as_ref()
                .and_then(|body| serde_json::from_slice::<Value>(body).ok())
        })
        .collect();
    assert!(
        requests.len() >= 2,
        "the request was asked twice: {requests:?}"
    );
    let first = messages_of(&requests[0]);
    let second = messages_of(&requests[1]);
    assert!(
        second.len() < first.len(),
        "the retry carried a folded history: {} messages then {}",
        first.len(),
        second.len()
    );
    assert!(
        second.iter().any(|message| message["role"] == "user"),
        "and the prompt it is answering is still there: {second:?}"
    );
}

/// Both requests rejected: the fold was tried, and the failure says so and
/// names the window rather than repeating the provider's sentence alone.
#[tokio::test]
async fn a_second_too_long_rejection_names_the_window() {
    let (transport, fetch) = replay(vec![rejected_too_long(), rejected_too_long()]);
    let mut engine = EngineRuntime::start_with_tools(
        foldable(),
        Arc::new(FixedResolver(transport)),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::RestoreHistory {
            messages: seeded_history(),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "and now?".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::Failed { message, .. } => Some(message.to_string()),
            _ => None,
        })
        .expect("the turn failed: {events:?}");
    assert!(
        message.contains("context length"),
        "the provider's own words are kept: {message}"
    );
    assert!(
        message.contains("window"),
        "and the model's window is named: {message}"
    );
    assert!(
        message.contains("folded"),
        "and it says a fold was already tried: {message}"
    );
    assert_eq!(
        fetch.requests.lock().unwrap().len(),
        2,
        "exactly two attempts: a fold once, not a loop"
    );
}

/// A transport error with no window in it is not folded for: the fold is for
/// the one rejection that a fold can fix.
#[tokio::test]
async fn another_rejection_is_not_folded_for() {
    let (transport, fetch) = replay(vec![
        MockFetchResponse::sse(vec![
            "data: {\"error\":{\"message\":\"invalid api key\"}}\n\n".to_owned(),
        ])
        .with_status(401),
        answered("done"),
    ]);
    let mut engine = EngineRuntime::start_with_tools(
        foldable(),
        Arc::new(FixedResolver(transport)),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::RestoreHistory {
            messages: seeded_history(),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "and now?".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Failed { .. })),
        "a 401 fails the turn: {events:?}"
    );
    assert_eq!(
        fetch.requests.lock().unwrap().len(),
        1,
        "and is not retried"
    );
}

/// A rejection for length that the policy cannot answer by folding: the
/// history is already as small as it will make it, so the second attempt would
/// carry the identical request. It is not sent — the failure says so and names
/// the window instead.
#[tokio::test]
async fn a_history_that_cannot_fold_is_not_asked_again() {
    // The default policy keeps 20 000 recent tokens, so this short history is
    // entirely "recent" and a forced fold returns None.
    let (transport, fetch) = replay(vec![rejected_too_long(), answered("done")]);
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        Arc::new(FixedResolver(transport)),
        ToolRegistry::new(),
    );
    engine
        .send(EngineCommand::RestoreHistory {
            messages: seeded_history(),
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "and now?".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::Failed { message, .. } => Some(message.to_string()),
            _ => None,
        })
        .expect("the turn failed rather than retrying the same request");
    assert!(message.contains("window"), "the window is named: {message}");
    assert!(
        message.contains("could be folded"),
        "and it says the fold found nothing to do: {message}"
    );
    assert_eq!(
        fetch.requests.lock().unwrap().len(),
        1,
        "the identical request was not sent a second time"
    );
}

/// The two fields the engine acts on are the providers' own: a `Fatal` that is
/// not marked too-long is never folded for, and `is_context_too_long` says so.
#[test]
fn only_a_marked_rejection_is_too_long() {
    let marked = TransportError::Fatal {
        status: Some(400),
        message: "context_length_exceeded".into(),
        context_too_long: true,
    };
    let plain = TransportError::Fatal {
        status: Some(400),
        message: "malformed".into(),
        context_too_long: false,
    };
    assert!(marked.is_context_too_long());
    assert!(!plain.is_context_too_long());
    assert!(
        marked
            .clone()
            .naming_the_window(128_000, true)
            .to_string()
            .contains("128000")
    );
    assert!(
        marked
            .naming_the_window(128_000, false)
            .to_string()
            .contains("could be folded")
    );
}
