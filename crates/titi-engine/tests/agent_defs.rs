#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Subagent definitions from files: the `agent` tool lists them, a call by
//! name runs the child as the definition says, and a definition can only
//! narrow what a subagent has.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::ToolRegistry;

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

fn resolver(models: Vec<(&str, &Arc<MockTransport>)>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        models
            .into_iter()
            .map(|(name, transport)| (name.to_owned(), Arc::clone(transport) as Arc<dyn Transport>))
            .collect(),
    ))
}

fn tool_call(name: &str, args: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: name.into(),
                ..Default::default()
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: args.into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
        StreamEvent::Done {
            reason: StopReason::ToolUse,
        },
    ])
}

fn text(body: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: body.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

const REVIEWER: &str = "---\nname: reviewer\ndescription: Reviews a diff for bugs\n\
tools: Read, Grep, Bash, Edit, Write\ndisallowedTools: Edit\n---\nYou are a strict reviewer.\n";

fn config(workspace: &Path) -> EngineConfig {
    let mut config = EngineConfig::new("primary");
    config.workspace_root = Some(workspace.to_path_buf());
    config.agent_model = Some("worker".into());
    config
}

async fn turn(engine: &mut titi_engine::Engine, prompt: &str) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SubmitPrompt {
            text: prompt.into(),
        })
        .await
        .unwrap();
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

fn tool_names(request: &titi_providers::WireRequest) -> Vec<&str> {
    let mut names: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    names.sort_unstable();
    names
}

/// The whole path: the tool's description lists the definition, the call by
/// name starts a child whose first message is the definition's prompt, and
/// whatever the file asked for beyond what a read-only subagent has is simply
/// not there.
#[tokio::test]
async fn a_call_by_name_runs_the_child_as_the_definition_and_only_narrows() {
    let workspace = tempfile::tempdir().unwrap();
    write(
        &workspace.path().join(".claude/agents/reviewer.md"),
        REVIEWER,
    );
    write(
        &workspace.path().join(".claude/agents/broken.md"),
        "---\ndescription: no name\n---\n",
    );
    let parent = Arc::new(MockTransport::new(vec![
        tool_call(
            "agent",
            r#"{"agent":"reviewer","task":"review src/lib.rs"}"#,
        ),
        text("noted"),
    ]));
    let child = Arc::new(MockTransport::new(vec![text("no bugs")]));
    let mut engine = EngineRuntime::start_with_tools(
        config(workspace.path()),
        resolver(vec![("primary", &parent), ("worker", &child)]),
        ToolRegistry::new(),
    );
    let events = turn(&mut engine, "review it").await;
    assert!(
        matches!(events.last(), Some(EngineEvent::TurnFinished { .. })),
        "{events:?}"
    );

    let first = &parent.requests()[0];
    let spec = first
        .tools
        .iter()
        .find(|tool| tool.name == "agent")
        .unwrap();
    assert!(
        spec.description
            .contains("- reviewer: Reviews a diff for bugs"),
        "{}",
        spec.description
    );
    assert!(
        !spec.description.contains("broken"),
        "an invalid file is not offered: {}",
        spec.description
    );
    assert_eq!(spec.parameters["properties"]["agent"]["type"], "string");

    let request = &child.requests()[0];
    assert_eq!(request.messages[0].role, Role::System);
    assert!(
        request.messages[0]
            .content
            .contains("You are a subagent of titi"),
        "titi's preamble comes first: {}",
        request.messages[0].content
    );
    assert!(
        request.messages[0]
            .content
            .ends_with("You are a strict reviewer."),
        "then the definition's prompt: {}",
        request.messages[0].content
    );
    assert_eq!(request.messages[1].role, Role::User);
    assert_eq!(request.messages[1].content, "review src/lib.rs");
    assert_eq!(
        tool_names(request),
        ["grep", "read"],
        "bash and write were asked for and are not read-tier; edit was denied"
    );

    let answer = parent.requests()[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .content
        .clone();
    assert!(
        answer.contains("agent reviewer completed: no bugs"),
        "{answer}"
    );
}

#[tokio::test]
async fn an_unknown_name_is_refused_with_the_names_that_exist() {
    let workspace = tempfile::tempdir().unwrap();
    write(&workspace.path().join(".titi/agents/reviewer.md"), REVIEWER);
    let parent = Arc::new(MockTransport::new(vec![
        tool_call("agent", r#"{"agent":"reviwer","task":"x"}"#),
        text("ok"),
    ]));
    let child = Arc::new(MockTransport::new(vec![text("never")]));
    let mut engine = EngineRuntime::start_with_tools(
        config(workspace.path()),
        resolver(vec![("primary", &parent), ("worker", &child)]),
        ToolRegistry::new(),
    );
    turn(&mut engine, "go").await;

    assert_eq!(child.call_count(), 0, "nothing was spawned");
    let result = parent.requests()[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .content
        .clone();
    assert!(
        result.contains("no definition named `reviwer`") && result.contains("available: reviewer"),
        "{result}"
    );
}

#[tokio::test]
async fn a_definitions_model_role_picks_the_childs_model() {
    let workspace = tempfile::tempdir().unwrap();
    write(
        &workspace.path().join(".titi/agents/cheap.md"),
        "---\nname: cheap\ndescription: d\nmodel: fast\n---\n",
    );
    let parent = Arc::new(MockTransport::new(vec![
        tool_call("agent", r#"{"agent":"cheap","task":"x"}"#),
        text("ok"),
    ]));
    let worker = Arc::new(MockTransport::new(vec![text("from worker")]));
    let small = Arc::new(MockTransport::new(vec![text("from small")]));
    let mut config = config(workspace.path());
    config
        .model_roles
        .insert("fast".to_owned(), "small".to_owned());
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(vec![
            ("primary", &parent),
            ("worker", &worker),
            ("small", &small),
        ]),
        ToolRegistry::new(),
    );
    turn(&mut engine, "go").await;

    assert_eq!(worker.call_count(), 0);
    assert_eq!(small.call_count(), 1, "the role named the model");
}

/// A Claude alias the user never mapped is not a reason to fail the call: the
/// child runs on the subagents' own model and the surface is told.
#[tokio::test]
async fn an_unknown_model_falls_back_with_a_notice() {
    let workspace = tempfile::tempdir().unwrap();
    write(
        &workspace.path().join(".claude/agents/a.md"),
        "---\nname: a\ndescription: d\nmodel: sonnet\n---\n",
    );
    let parent = Arc::new(MockTransport::new(vec![
        tool_call("agent", r#"{"agent":"a","task":"x"}"#),
        text("ok"),
    ]));
    let worker = Arc::new(MockTransport::new(vec![text("from worker")]));
    let mut engine = EngineRuntime::start_with_tools(
        config(workspace.path()),
        resolver(vec![("primary", &parent), ("worker", &worker)]),
        ToolRegistry::new(),
    );
    let events = turn(&mut engine, "go").await;

    assert_eq!(worker.call_count(), 1);
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::Notice { message } if message.contains("agent a: model sonnet")
        )),
        "{events:?}"
    );
}

/// `maxTurns` is the child's round cap, and the child ends the way any
/// subagent at its cap does: a wrap-up request, then a result marked as cut
/// short. It can lower the session's cap, never raise it.
#[tokio::test]
async fn max_turns_caps_the_child_and_cannot_raise_the_sessions_cap() {
    for (agent_rounds, max_turns) in [(40, 1), (1, 5)] {
        let workspace = tempfile::tempdir().unwrap();
        write(&workspace.path().join("note.txt"), "body");
        write(
            &workspace.path().join(".titi/agents/brief.md"),
            &format!("---\nname: brief\ndescription: d\nmaxTurns: {max_turns}\n---\n"),
        );
        let parent = Arc::new(MockTransport::new(vec![
            tool_call("agent", r#"{"agent":"brief","task":"x"}"#),
            text("ok"),
        ]));
        let child = Arc::new(MockTransport::new(vec![
            tool_call("read", r#"{"path":"note.txt"}"#),
            text("read it once"),
            text("unused"),
        ]));
        let mut config = config(workspace.path());
        config.agent_rounds = agent_rounds;
        let mut engine = EngineRuntime::start_with_tools(
            config,
            resolver(vec![("primary", &parent), ("worker", &child)]),
            ToolRegistry::new(),
        );
        turn(&mut engine, "go").await;

        let requests = child.requests();
        assert_eq!(requests.len(), 2, "{agent_rounds}/{max_turns}");
        assert!(requests[1].forbid_tool_calls);
        let answer = parent.requests()[1]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap()
            .content
            .clone();
        assert!(
            answer.contains("(stopped at the tool budget) read it once"),
            "{answer}"
        );
    }
}

/// The list is read when a turn starts: a file added between two turns is
/// offered in the second.
#[tokio::test]
async fn a_definition_added_between_turns_is_offered_in_the_next() {
    let workspace = tempfile::tempdir().unwrap();
    let parent = Arc::new(MockTransport::new(vec![text("one"), text("two")]));
    let worker = Arc::new(MockTransport::new(vec![]));
    let mut engine = EngineRuntime::start_with_tools(
        config(workspace.path()),
        resolver(vec![("primary", &parent), ("worker", &worker)]),
        ToolRegistry::new(),
    );
    turn(&mut engine, "first").await;
    write(&workspace.path().join(".titi/agents/late.md"), REVIEWER);
    turn(&mut engine, "second").await;

    let requests = parent.requests();
    let description = |at: usize| {
        requests[at]
            .tools
            .iter()
            .find(|tool| tool.name == "agent")
            .unwrap()
            .description
            .clone()
    };
    assert!(!description(0).contains("reviewer"));
    assert!(description(1).contains("- reviewer:"), "{}", description(1));
}

#[tokio::test]
async fn a_batch_entry_can_name_a_definition_and_a_bare_name_beside_tasks_is_refused() {
    let workspace = tempfile::tempdir().unwrap();
    write(&workspace.path().join(".titi/agents/reviewer.md"), REVIEWER);
    let parent = Arc::new(MockTransport::new(vec![
        tool_call(
            "agent",
            r#"{"tasks":[{"task":"a","agent":"reviewer"},{"task":"b"}]}"#,
        ),
        tool_call("agent", r#"{"agent":"reviewer","tasks":[{"task":"a"}]}"#),
        text("ok"),
    ]));
    let child = Arc::new(MockTransport::new(vec![text("one"), text("two")]));
    let mut engine = EngineRuntime::start_with_tools(
        config(workspace.path()),
        resolver(vec![("primary", &parent), ("worker", &child)]),
        ToolRegistry::new(),
    );
    turn(&mut engine, "go").await;

    let requests = child.requests();
    assert_eq!(requests.len(), 2);
    let with_prompt = requests
        .iter()
        .filter(|request| request.messages[0].role == Role::System)
        .count();
    assert_eq!(with_prompt, 1, "only the entry that named the definition");
    let refusal = parent.requests()[2]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .content
        .clone();
    assert!(
        refusal.contains("inside each entry of `tasks`"),
        "{refusal}"
    );
}
