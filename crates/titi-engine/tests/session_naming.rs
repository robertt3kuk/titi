//! A finished turn names its session, cheaply and without being able to hurt
//! the turn (`P1-SES-9`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use titi_core::session::{SessionIndex, SessionMeta, SessionStore};
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, EventStream, MockBody, MockTransport, RequestCtx, StopReason, StreamEvent, Transport,
    TransportError, WireRequest,
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

fn resolver(entries: Vec<(&str, Arc<dyn Transport>)>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        entries
            .into_iter()
            .map(|(model, transport)| (model.to_owned(), transport))
            .collect(),
    ))
}

/// A transport that takes its time before the stream even opens.
struct SlowTransport(Duration);

#[async_trait::async_trait]
impl Transport for SlowTransport {
    fn api(&self) -> titi_providers::ApiKind {
        titi_providers::ApiKind::OpenAiCompletions
    }

    async fn stream(
        &self,
        _req: WireRequest,
        _ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        tokio::time::sleep(self.0).await;
        Err(TransportError::Fatal {
            status: None,
            message: "too late".into(),
        })
    }
}

fn answered(text: &str) -> Arc<MockTransport> {
    Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: text.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]))
}

/// An agent directory with a session the surface just created, named with the
/// placeholder the CLI writes, plus a role map pointing `smol` at `namer`.
fn agent_dir(roles: bool) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    if roles {
        std::fs::write(
            dir.path().join("config.yml"),
            "modelRoles:\n  smol: namer\n",
        )
        .unwrap_or_else(|e| panic!("config: {e}"));
    }
    let store = SessionStore::new(dir.path()).unwrap_or_else(|e| panic!("store: {e}"));
    let session_id = store
        .create(SessionMeta {
            title: Some("titi".into()),
            ..SessionMeta::default()
        })
        .unwrap_or_else(|e| panic!("session: {e}"));
    (dir, session_id)
}

fn config(agent: &std::path::Path, session_id: &str) -> EngineConfig {
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(agent.to_path_buf());
    config.session_id = Some(session_id.to_owned());
    // Keeps settings resolution inside the test's own tree.
    config.workspace_root = Some(agent.to_path_buf());
    config
}

fn title(agent: &std::path::Path, session_id: &str) -> Option<String> {
    SessionIndex::open(&agent.join("state.db"))
        .unwrap_or_else(|e| panic!("index: {e}"))
        .title(session_id)
        .unwrap_or_else(|e| panic!("title: {e}"))
}

/// Collects events until the turn ends, then keeps listening — up to
/// `deadline`, or until the name lands — so an event from the naming task
/// still has its chance to arrive.
async fn events_until_quiet(
    engine: &mut titi_engine::Engine,
    deadline: Duration,
) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let finished = matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        );
        events.push(event);
        if finished {
            break;
        }
    }
    let until = Instant::now() + deadline;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, engine.recv()).await {
            Ok(Some(event)) => {
                let named = matches!(event, EngineEvent::SessionNamed { .. });
                events.push(event);
                if named {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    events
}

async fn run_turn(engine: &mut titi_engine::Engine, deadline: Duration) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "make the parser stop eating commas".into(),
        })
        .await
        .unwrap_or_else(|e| panic!("submit: {e}"));
    events_until_quiet(engine, deadline).await
}

/// The point of the feature: a session nobody named comes out of its first
/// turn with a short title, taken from the cheap role and not from the model
/// the turn itself ran on.
#[tokio::test]
async fn a_session_is_named_after_its_first_turn() {
    let (dir, session_id) = agent_dir(true);
    let namer = answered("\"Parser Comma Fix\"\n");
    let mut engine = EngineRuntime::start(
        config(dir.path(), &session_id),
        resolver(vec![
            ("primary", answered("done") as Arc<dyn Transport>),
            ("namer", Arc::clone(&namer) as Arc<dyn Transport>),
        ]),
    );

    let events = run_turn(&mut engine, Duration::from_secs(5)).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::SessionNamed { title, .. } if title == "Parser Comma Fix"
        )),
        "no name reached the surface: {events:?}"
    );
    assert_eq!(
        title(dir.path(), &session_id),
        Some("Parser Comma Fix".to_owned())
    );
    let asked = namer.requests();
    assert_eq!(asked.len(), 1, "the cheap model was asked {}x", asked.len());
    assert!(
        asked[0].messages[0]
            .content
            .contains("make the parser stop eating commas"),
        "the naming prompt lost the user's message: {}",
        asked[0].messages[0].content
    );
}

/// A name the user chose is not a suggestion: the namer does not call a model
/// for it and does not overwrite it.
#[tokio::test]
async fn a_user_chosen_name_is_never_replaced() {
    let (dir, session_id) = agent_dir(true);
    SessionIndex::open(&dir.path().join("state.db"))
        .unwrap_or_else(|e| panic!("index: {e}"))
        .set_title(&session_id, "release cut")
        .unwrap_or_else(|e| panic!("set_title: {e}"));
    let namer = answered("Parser Comma Fix");
    let mut engine = EngineRuntime::start(
        config(dir.path(), &session_id),
        resolver(vec![
            ("primary", answered("done") as Arc<dyn Transport>),
            ("namer", Arc::clone(&namer) as Arc<dyn Transport>),
        ]),
    );

    let events = run_turn(&mut engine, Duration::from_millis(500)).await;

    assert_eq!(
        title(dir.path(), &session_id),
        Some("release cut".to_owned())
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::SessionNamed { .. })),
        "the namer renamed a session the user had named: {events:?}"
    );
    assert_eq!(
        namer.call_count(),
        0,
        "a named session still paid for a model call"
    );
}

/// No `smol` model configured, or one that fails: the session keeps its name
/// and the turn hears nothing about it.
#[tokio::test]
async fn a_missing_or_failing_namer_is_silent() {
    for roles in [true, false] {
        let (dir, session_id) = agent_dir(roles);
        // With a role map the `namer` model is unresolvable; without one the
        // role falls back to `primary`, whose mock is already exhausted.
        let mut engine = EngineRuntime::start(
            config(dir.path(), &session_id),
            resolver(vec![("primary", answered("done") as Arc<dyn Transport>)]),
        );

        let events = run_turn(&mut engine, Duration::from_millis(500)).await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::TurnFinished { .. })),
            "the turn did not finish (roles: {roles}): {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Failed { .. })),
            "naming failed the turn (roles: {roles}): {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::SessionNamed { .. })),
            "a session was named without a model (roles: {roles}): {events:?}"
        );
        assert_eq!(title(dir.path(), &session_id), Some("titi".to_owned()));
    }
}

/// A naming model that takes minutes must not hold the turn — or the next
/// prompt — for a single one of them.
#[tokio::test]
async fn a_slow_namer_does_not_hold_up_the_turn() {
    let (dir, session_id) = agent_dir(true);
    let mut engine = EngineRuntime::start(
        config(dir.path(), &session_id),
        resolver(vec![
            ("primary", answered("done") as Arc<dyn Transport>),
            (
                "namer",
                Arc::new(SlowTransport(Duration::from_secs(300))) as Arc<dyn Transport>,
            ),
        ]),
    );

    let started = Instant::now();
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "make the parser stop eating commas".into(),
        })
        .await
        .unwrap_or_else(|e| panic!("submit: {e}"));
    let mut finished = None;
    while let Some(event) = engine.recv().await {
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            finished = Some(started.elapsed());
            break;
        }
    }
    let waited = finished.unwrap_or_else(|| panic!("the turn never finished"));
    assert!(
        waited < Duration::from_secs(5),
        "the turn waited {waited:?} on the namer"
    );

    // And the engine keeps taking work while the namer is still stuck.
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "and the second one".into(),
        })
        .await
        .unwrap_or_else(|e| panic!("second submit: {e}"));
    let second = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = engine.recv().await {
            if matches!(
                event,
                EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
            ) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(second, Ok(true), "the next turn never ran");
    assert_eq!(title(dir.path(), &session_id), Some("titi".to_owned()));
}
