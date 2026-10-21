#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Model routing by role, from `modelRoles` in the agent directory's config to
//! the request each kind of work sends.
//!
//! One transport records every `WireRequest` it is handed, whatever model was
//! asked for, so the model and the reasoning a job ran with are read off the
//! request, not inferred from the settings.

use std::path::Path;
use std::sync::{Arc, Mutex};

use titi_engine::{
    AgentKind, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError,
    ResolvedModel, TransportResolver,
};
use titi_providers::{
    ApiKind, BlockId, Effort, EventStream, ModelCompat, RequestCtx, StopReason, StreamEvent,
    Transport, TransportError, WireRequest,
};
use titi_tools::ToolRegistry;

struct Recording {
    requests: Mutex<Vec<WireRequest>>,
}

impl Recording {
    fn requests(&self) -> Vec<WireRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn models(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r.model.to_string())
            .collect()
    }
}

#[async_trait::async_trait]
impl Transport for Recording {
    fn api(&self) -> ApiKind {
        ApiKind::OpenAiCompletions
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
                text: "VERDICT: PASS — fine".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])))
    }
}

/// Knows `KNOWN`; every one of them takes efforts low to high. Anything else,
/// and `keyless`, does not resolve.
struct Resolver(Arc<Recording>);

const KNOWN: [&str; 6] = ["chat", "worker", "sub-m", "explore-m", "review-m", "plain"];

impl TransportResolver for Resolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        if !KNOWN.contains(&model) {
            return Err(RegistryError::UnknownModel(model.into()));
        }
        let mut resolved =
            ResolvedModel::without_credential(model, Arc::clone(&self.0) as Arc<dyn Transport>);
        if model != "plain" {
            resolved.reasoning = Some(ModelCompat {
                efforts: vec![Effort::Low, Effort::Medium, Effort::High],
                ..ModelCompat::default()
            });
        }
        Ok(resolved)
    }
}

struct Session {
    _agent: tempfile::TempDir,
    workspace: tempfile::TempDir,
    transport: Arc<Recording>,
    engine: titi_engine::Engine,
}

/// An engine whose agent directory holds `config`, subagents on `worker`, and
/// writes for subagents as asked.
fn session(config: &str, writes: bool) -> Session {
    let agent = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    if !config.is_empty() {
        std::fs::write(agent.path().join("config.yml"), config).unwrap();
    }
    let transport = Arc::new(Recording {
        requests: Mutex::new(Vec::new()),
    });
    let mut cfg = EngineConfig::new("chat");
    cfg.agent_dir = Some(agent.path().to_path_buf());
    cfg.workspace_root = Some(workspace.path().to_path_buf());
    cfg.agent_model = Some("worker".into());
    cfg.agent_writes = writes;
    let engine = EngineRuntime::start_with_tools(
        cfg,
        Arc::new(Resolver(Arc::clone(&transport))),
        ToolRegistry::new(),
    );
    Session {
        _agent: agent,
        workspace,
        transport,
        engine,
    }
}

async fn spawn(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Worker".into(),
            task: "look around".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(event, EngineEvent::AgentFinished { .. });
        events.push(event);
        if done {
            break;
        }
    }
    events
}

fn notices(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Notice { message } => Some(message.to_string()),
            _ => None,
        })
        .collect()
}

fn effort_of(request: &WireRequest) -> Option<Effort> {
    request.reasoning.as_ref().map(|reasoning| reasoning.effort)
}

/// Nothing in `modelRoles`: the subagent's request is what it always was —
/// the runner's own model, the task as the only message, no reasoning, and
/// nothing said.
#[tokio::test]
async fn unset_roles_leave_the_subagent_request_as_it_was() {
    for config in ["", "modelRoles:\n  default: chat\n"] {
        let mut s = session(config, true);
        let events = spawn(&mut s.engine).await;
        let requests = s.transport.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.model, "worker");
        assert_eq!(effort_of(request), None);
        assert!(request.system.is_none());
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].content, "look around");
        assert!(!request.forbid_tool_calls);
        assert!(notices(&events).is_empty(), "{events:?}");
    }
}

#[tokio::test]
async fn the_subagent_role_sets_model_and_effort_for_a_subagent_that_can_write() {
    let mut s = session(
        "modelRoles:\n  subagent:\n    model: sub-m\n    effort: low\n  explore: explore-m\n",
        true,
    );
    spawn(&mut s.engine).await;
    let requests = s.transport.requests();
    assert_eq!(requests[0].model, "sub-m");
    assert_eq!(effort_of(&requests[0]), Some(Effort::Low));
}

#[tokio::test]
async fn a_subagent_that_can_only_read_runs_on_the_explore_role() {
    let mut s = session(
        "modelRoles:\n  subagent: sub-m\n  explore: {model: explore-m, effort: high}\n",
        false,
    );
    spawn(&mut s.engine).await;
    let requests = s.transport.requests();
    assert_eq!(requests[0].model, "explore-m");
    assert_eq!(effort_of(&requests[0]), Some(Effort::High));

    // Without an `explore` slot it is the `subagent` role's work as well.
    let mut s = session("modelRoles:\n  subagent: sub-m\n", false);
    spawn(&mut s.engine).await;
    assert_eq!(s.transport.models(), ["sub-m"]);
}

/// An effort-only slot changes how hard the subagent thinks, not where it runs.
#[tokio::test]
async fn an_effort_only_slot_keeps_the_model() {
    let mut s = session("modelRoles:\n  subagent:\n    effort: medium\n", true);
    spawn(&mut s.engine).await;
    let requests = s.transport.requests();
    assert_eq!(requests[0].model, "worker");
    assert_eq!(effort_of(&requests[0]), Some(Effort::Medium));
}

/// An effort a model has no ladder for is not sent: the slot asked, the model
/// cannot take it, the request stays as plain as it was.
#[tokio::test]
async fn an_effort_the_model_cannot_take_is_not_sent() {
    let mut s = session(
        "modelRoles:\n  subagent:\n    model: plain\n    effort: high\n",
        true,
    );
    spawn(&mut s.engine).await;
    let requests = s.transport.requests();
    assert_eq!(requests[0].model, "plain");
    assert_eq!(effort_of(&requests[0]), None);
}

/// A slot naming a model the session cannot resolve does not stop the work:
/// it runs where it always did and one notice says why — once, not on every
/// spawn.
#[tokio::test]
async fn an_unusable_role_model_falls_back_with_one_notice() {
    let mut s = session("modelRoles:\n  subagent: ghost\n", true);
    let first = spawn(&mut s.engine).await;
    let said = notices(&first);
    assert_eq!(said.len(), 1, "{first:?}");
    assert!(
        said[0].starts_with("modelRoles.subagent: `ghost` cannot be used"),
        "{said:?}"
    );
    assert!(said[0].contains("using worker"), "{said:?}");
    let second = spawn(&mut s.engine).await;
    assert!(notices(&second).is_empty(), "{second:?}");
    assert_eq!(s.transport.models(), ["worker", "worker"]);
}

#[tokio::test]
async fn a_bad_effort_word_is_noted_and_ignored() {
    let mut s = session(
        "modelRoles:\n  subagent:\n    model: sub-m\n    effort: turbo\n",
        true,
    );
    let events = spawn(&mut s.engine).await;
    assert!(
        notices(&events)
            .iter()
            .any(|note| note.contains("effort `turbo` is not a level")),
        "{events:?}"
    );
    let requests = s.transport.requests();
    assert_eq!(
        (requests[0].model.as_str(), effort_of(&requests[0])),
        ("sub-m", None)
    );
}

/// The config is read when the work starts: an edit between two spawns takes
/// effect on the second.
#[tokio::test]
async fn a_config_edit_applies_to_the_next_spawn() {
    let mut s = session("", true);
    spawn(&mut s.engine).await;
    let path: &Path = s._agent.path();
    std::fs::write(path.join("config.yml"), "modelRoles:\n  subagent: sub-m\n").unwrap();
    spawn(&mut s.engine).await;
    assert_eq!(s.transport.models(), ["worker", "sub-m"]);
}

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository with one committed file and, when `dirty`, an edit to it.
fn repo(root: &Path, dirty: bool) {
    git(root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);
    if dirty {
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    }
}

async fn review(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::RunReview {
            edited: vec!["a.txt".into()],
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(event, EngineEvent::ReviewFinished { .. });
        events.push(event);
        if done {
            break;
        }
    }
    events
}

#[tokio::test]
async fn unset_roles_leave_the_review_request_as_it_was() {
    let mut s = session("", false);
    repo(s.workspace.path(), true);
    let events = review(&mut s.engine).await;
    let requests = s.transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "chat");
    assert_eq!(effort_of(&requests[0]), None);
    assert!(notices(&events).is_empty(), "{events:?}");
}

#[tokio::test]
async fn the_review_role_sets_the_reviewers_model_and_effort() {
    let mut s = session(
        "modelRoles:\n  review: {model: review-m, effort: low}\n  subagent: sub-m\n",
        false,
    );
    repo(s.workspace.path(), true);
    review(&mut s.engine).await;
    let requests = s.transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "review-m");
    assert_eq!(effort_of(&requests[0]), Some(Effort::Low));
}

#[tokio::test]
async fn a_review_role_naming_a_ghost_runs_on_the_chat_model_with_a_notice() {
    let mut s = session("modelRoles:\n  review: ghost\n", false);
    repo(s.workspace.path(), true);
    let events = review(&mut s.engine).await;
    assert_eq!(s.transport.models(), ["chat"]);
    let said = notices(&events);
    assert_eq!(said.len(), 1, "{events:?}");
    assert!(
        said[0].starts_with("modelRoles.review: `ghost`"),
        "{said:?}"
    );
}

/// A review of nothing calls no model, so a broken slot has nothing to say.
#[tokio::test]
async fn a_review_of_nothing_says_nothing_about_the_role() {
    let mut s = session("modelRoles:\n  review: ghost\n", false);
    repo(s.workspace.path(), false);
    let events = review(&mut s.engine).await;
    assert!(s.transport.requests().is_empty());
    assert!(notices(&events).is_empty(), "{events:?}");
}

/// `/goal`: the coder stays on the session model, the reviewer is the
/// `review` role's work.
#[tokio::test]
async fn the_goal_reviewer_runs_on_the_review_role_and_the_coder_does_not() {
    let mut s = session(
        "modelRoles:\n  review: {model: review-m, effort: high}\n",
        false,
    );
    s.engine
        .send(EngineCommand::RunGoal {
            text: "make it so".into(),
        })
        .await
        .unwrap();
    while let Some(event) = s.engine.recv().await {
        if matches!(event, EngineEvent::GoalFinished { .. }) {
            break;
        }
    }
    let requests = s.transport.requests();
    assert_eq!(requests[0].model, "chat", "the coder");
    assert_eq!(effort_of(&requests[0]), None);
    let reviewer = requests
        .iter()
        .find(|request| request.model == "review-m")
        .expect("the reviewer ran on the review role's model");
    assert_eq!(effort_of(reviewer), Some(Effort::High));
    assert!(
        requests
            .iter()
            .filter(|request| request.model == "chat")
            .all(|request| effort_of(request).is_none())
    );
}
