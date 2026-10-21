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
    first_chat: Mutex<Option<Vec<StreamEvent>>>,
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

    /// The next request to the model `chat` is answered with `events`
    /// instead of the usual text.
    fn script_first_chat(&self, events: Vec<StreamEvent>) {
        *self.first_chat.lock().unwrap() = Some(events);
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
        let scripted = if req.model == "chat" {
            self.first_chat.lock().unwrap().take()
        } else {
            None
        };
        self.requests.lock().unwrap().push(req);
        let events = scripted.unwrap_or_else(|| {
            vec![
                StreamEvent::TextDelta {
                    id: BlockId::new("text"),
                    text: "VERDICT: PASS — fine".into(),
                },
                StreamEvent::Done {
                    reason: StopReason::Stop,
                },
            ]
        });
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Knows `KNOWN`; every one of them takes efforts low to high. Anything else,
/// and `keyless`, does not resolve.
struct Resolver(Arc<Recording>);

const KNOWN: [&str; 10] = [
    "chat",
    "worker",
    "sub-m",
    "explore-m",
    "review-m",
    "advisor-m",
    "compact-m",
    "namer-m",
    "plain",
    "small",
];

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
    session_with(config, writes, |_, _| {})
}

/// [`session`], with a say in the engine's config before it starts.
fn session_with(
    config: &str,
    writes: bool,
    customize: impl FnOnce(&mut EngineConfig, &Path),
) -> Session {
    let agent = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    if !config.is_empty() {
        std::fs::write(agent.path().join("config.yml"), config).unwrap();
    }
    let transport = Arc::new(Recording {
        requests: Mutex::new(Vec::new()),
        first_chat: Mutex::new(None),
    });
    let mut cfg = EngineConfig::new("chat");
    cfg.agent_dir = Some(agent.path().to_path_buf());
    cfg.workspace_root = Some(workspace.path().to_path_buf());
    cfg.agent_model = Some("worker".into());
    cfg.agent_writes = writes;
    customize(&mut cfg, agent.path());
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

fn said(text: &str) -> titi_providers::ChatMessage {
    titi_providers::ChatMessage {
        role: titi_providers::Role::User,
        content: text.into(),
        ..Default::default()
    }
}

async fn consult(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::Consult { question: None })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(
            event,
            EngineEvent::AdvisorAnswer { .. } | EngineEvent::AdvisorFailed { .. }
        );
        events.push(event);
        if done {
            break;
        }
    }
    events
}

#[tokio::test]
async fn the_advisor_role_sets_the_consults_model_and_effort_and_unset_leaves_it() {
    for (config, model, effort) in [
        ("", "chat", None),
        (
            "modelRoles:\n  advisor: {model: advisor-m, effort: medium}\n",
            "advisor-m",
            Some(Effort::Medium),
        ),
    ] {
        let mut s = session_with(config, false, |cfg, _| {
            cfg.restored_messages = vec![said("is this safe?")];
        });
        let events = consult(&mut s.engine).await;
        let requests = s.transport.requests();
        assert_eq!(requests.len(), 1, "{events:?}");
        assert_eq!(requests[0].model, model);
        assert_eq!(effort_of(&requests[0]), effort);
        assert!(notices(&events).is_empty(), "{events:?}");
    }
}

#[tokio::test]
async fn an_advisor_role_naming_a_ghost_consults_the_chat_model_with_a_notice() {
    let mut s = session_with("modelRoles:\n  advisor: ghost\n", false, |cfg, _| {
        cfg.restored_messages = vec![said("is this safe?")];
    });
    let events = consult(&mut s.engine).await;
    assert_eq!(s.transport.models(), ["chat"]);
    let said = notices(&events);
    assert_eq!(said.len(), 1, "{events:?}");
    assert!(
        said[0].starts_with("modelRoles.advisor: `ghost`"),
        "{said:?}"
    );
}

async fn compact(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::RestoreHistory {
            messages: vec![
                said("the first question, long enough to fold"),
                titi_providers::ChatMessage {
                    role: titi_providers::Role::Assistant,
                    content: "the first answer".into(),
                    ..Default::default()
                },
                said("now"),
            ],
        })
        .await
        .unwrap();
    engine
        .send(EngineCommand::Compact { focus: None })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(
            event,
            EngineEvent::Compacted { .. } | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if done {
            break;
        }
    }
    events
}

fn small_window(cfg: &mut EngineConfig, _: &Path) {
    cfg.context_window = 100_000;
    cfg.compaction = titi_core::compaction::CompactionPolicy {
        threshold_percent: 80.0,
        keep_recent_tokens: 4,
        ..titi_core::compaction::CompactionPolicy::default()
    };
}

#[tokio::test]
async fn the_compact_role_sets_the_summary_effort_and_unset_sends_none() {
    for (config, model, effort) in [
        ("", "chat", None),
        (
            "modelRoles:\n  compact: {model: compact-m, effort: high}\n",
            "compact-m",
            Some(Effort::High),
        ),
    ] {
        let mut s = session_with(config, false, small_window);
        let events = compact(&mut s.engine).await;
        let requests = s.transport.requests();
        assert_eq!(requests.len(), 1, "{events:?}");
        assert_eq!(requests[0].model, model);
        assert_eq!(effort_of(&requests[0]), effort);
        assert!(notices(&events).is_empty(), "{events:?}");
    }
}

/// A session the surface has just created, so the namer has a title to
/// replace.
fn named_session(cfg: &mut EngineConfig, agent: &Path) {
    let store = titi_core::session::SessionStore::new(agent).unwrap();
    let id = store
        .create(titi_core::session::SessionMeta {
            title: Some("titi".into()),
            ..Default::default()
        })
        .unwrap();
    cfg.session_id = Some(id);
}

/// Runs one turn and waits for the namer to be heard from, or for it not to be.
async fn turn_and_name(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "make the parser stop eating commas".into(),
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(event, EngineEvent::TurnFinished { .. });
        events.push(event);
        if done {
            break;
        }
    }
    let until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
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

/// The turn is the chat's request; the namer's is the next one.
fn namer_request(s: &Session) -> Option<WireRequest> {
    s.transport.requests().into_iter().nth(1)
}

#[tokio::test]
async fn the_namer_role_and_its_old_name_smol_route_the_title_request() {
    for (config, model, effort) in [
        (
            "modelRoles:\n  namer: {model: namer-m, effort: low}\n",
            "namer-m",
            Some(Effort::Low),
        ),
        ("modelRoles:\n  smol: small\n", "small", None),
        (
            "modelRoles:\n  namer: namer-m\n  smol: small\n",
            "namer-m",
            None,
        ),
    ] {
        let mut s = session_with(config, false, named_session);
        turn_and_name(&mut s.engine).await;
        let request = namer_request(&s).expect("the namer made a request");
        assert_eq!(request.model, model, "{config}");
        assert_eq!(effort_of(&request), effort, "{config}");
    }
}

/// What it always did: no map, the chat model names the session; a map that
/// names neither `namer` nor `smol`, nobody does.
#[tokio::test]
async fn unset_namer_roles_keep_todays_behaviour() {
    let mut s = session_with("", false, named_session);
    turn_and_name(&mut s.engine).await;
    let request = namer_request(&s).expect("the chat model names it");
    assert_eq!(
        (request.model.as_str(), effort_of(&request)),
        ("chat", None)
    );

    let mut s = session_with("modelRoles:\n  default: chat\n", false, named_session);
    turn_and_name(&mut s.engine).await;
    assert_eq!(s.transport.requests().len(), 1, "only the turn itself");
}

#[tokio::test]
async fn a_namer_role_naming_a_ghost_names_with_the_chat_model_and_says_so() {
    let mut s = session_with("modelRoles:\n  namer: ghost\n", false, named_session);
    let events = turn_and_name(&mut s.engine).await;
    assert_eq!(
        namer_request(&s).map(|request| request.model.to_string()),
        Some("chat".to_owned())
    );
    let said = notices(&events);
    assert_eq!(said.len(), 1, "{events:?}");
    assert!(said[0].starts_with("modelRoles.namer: `ghost`"), "{said:?}");
}

/// The chat model's reply that calls the `agent` tool for `definition`.
fn delegate(definition: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: titi_providers::ToolCallRef {
                call_id: "call-1".into(),
                name: "agent".into(),
                ..Default::default()
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: format!(r#"{{"agent":"{definition}","task":"look"}}"#).into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
        StreamEvent::Done {
            reason: StopReason::ToolUse,
        },
    ]
}

/// A definition narrowed to read tools is `explore` work even in a session
/// whose subagents may write; its own `model` and `effort` beat the role's.
#[tokio::test]
async fn a_definition_is_routed_by_its_tools_and_its_own_model_and_effort_win() {
    let roles = "modelRoles:\n  subagent: {model: sub-m, effort: low}\n  \
                 explore: {model: explore-m, effort: high}\n";
    for (file, model, effort) in [
        // Reads only -> explore.
        ("tools: Read, Grep\n", "explore-m", Some(Effort::High)),
        // Can write -> subagent.
        ("tools: Read, Write\n", "sub-m", Some(Effort::Low)),
        // Its own model and effort win over the role's.
        (
            "tools: Read\nmodel: review-m\neffort: medium\n",
            "review-m",
            Some(Effort::Medium),
        ),
        // Its own model alone: the role still decides the effort.
        (
            "tools: Read\nmodel: review-m\n",
            "review-m",
            Some(Effort::High),
        ),
    ] {
        // The `agent` tool is exec tier when subagents may write; the parent
        // turn must not park on an approval nobody answers.
        let mut s = session_with(roles, true, |cfg, _| {
            cfg.approval_mode = titi_tools::ApprovalMode::Yolo;
        });
        std::fs::create_dir_all(s.workspace.path().join(".titi/agents")).unwrap();
        std::fs::write(
            s.workspace.path().join(".titi/agents/r.md"),
            format!("---\nname: r\ndescription: d\n{file}---\nbody\n"),
        )
        .unwrap();
        s.transport.script_first_chat(delegate("r"));
        s.engine
            .send(EngineCommand::SubmitPrompt { text: "go".into() })
            .await
            .unwrap();
        while let Some(event) = s.engine.recv().await {
            if matches!(event, EngineEvent::TurnFinished { .. }) {
                break;
            }
        }
        let requests = s.transport.requests();
        let child = requests
            .iter()
            .find(|request| request.model != "chat")
            .unwrap_or_else(|| panic!("no child ran: {:?}", s.transport.models()));
        assert_eq!(child.model, model, "{file}");
        assert_eq!(effort_of(child), effort, "{file}");
    }
}

/// Without the router's roles set, a definition runs exactly as it did: on the
/// subagents' own model with the effort its file gives, if any.
#[tokio::test]
async fn a_definition_with_no_roles_runs_as_it_did() {
    let mut s = session_with("", true, |cfg, _| {
        cfg.approval_mode = titi_tools::ApprovalMode::Yolo;
    });
    std::fs::create_dir_all(s.workspace.path().join(".titi/agents")).unwrap();
    std::fs::write(
        s.workspace.path().join(".titi/agents/r.md"),
        "---\nname: r\ndescription: d\neffort: low\n---\nbody\n",
    )
    .unwrap();
    s.transport.script_first_chat(delegate("r"));
    s.engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    while let Some(event) = s.engine.recv().await {
        if matches!(event, EngineEvent::TurnFinished { .. }) {
            break;
        }
    }
    let requests = s.transport.requests();
    let child = requests
        .iter()
        .find(|request| request.model != "chat")
        .unwrap();
    assert_eq!(child.model, "worker");
    assert_eq!(effort_of(child), Some(Effort::Low));
}
