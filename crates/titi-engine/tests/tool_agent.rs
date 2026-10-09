#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! E4: a subagent that works — its own tool loop, sharing the runtime's
//! claims, touched set and read cache.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E4).

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::{
    AgentKind, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError,
    ResolvedModel, TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, Transport,
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

fn resolver(transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        [("worker".to_owned(), transport)].into_iter().collect(),
    ))
}

/// A streamed tool call with one complete argument payload.
fn tool_call(name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
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
    ]
}

fn text(body: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: body.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ]
}

/// Drives one spawn to completion and returns its summary.
async fn spawn_and_collect(
    engine: &mut titi_engine::Engine,
    task: &str,
) -> (String, Vec<EngineEvent>) {
    let mut events = Vec::new();
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Worker".into(),
            task: task.into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();
    let mut summary = String::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(event, EngineEvent::AgentFinished { .. });
        if let EngineEvent::AgentFinished { summary: text, .. } = &event {
            summary = text.to_string();
        }
        events.push(event);
        if done {
            break;
        }
    }
    (summary, events)
}

fn agent_config(workspace: &std::path::Path) -> EngineConfig {
    let mut config = EngineConfig::new("unused");
    config.workspace_root = Some(workspace.to_path_buf());
    config.agent_model = Some("worker".into());
    config
}

#[tokio::test]
async fn a_subagent_reads_a_file_through_its_own_tool_loop() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "the payload").unwrap();

    // First request asks for the read, the second reports it.
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("read", r#"{"path":"note.txt"}"#)),
        MockBody::Events(text("read it")),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        agent_config(workspace.path()),
        resolver(Arc::clone(&transport) as _),
        ToolRegistry::new(),
    );

    let (summary, events) = spawn_and_collect(&mut engine, "read the note").await;

    assert_eq!(summary, "read it");
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::AgentActivity { text, .. } if text == "tools: read"
        )),
        "what the subagent is doing is an activity, not a line of its answer: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::AgentProgress { text, .. } if text == "read it"
        )),
        "and what it says is progress: {events:?}"
    );
    // Two provider requests: the tool round and the answer.
    assert_eq!(transport.call_count(), 2);
    let requests = transport.requests();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "the payload"),
        "the tool result was replayed into the subagent's next request"
    );
}

#[tokio::test]
async fn a_subagent_warms_the_shared_read_cache() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "cached body").unwrap();

    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call("read", r#"{"path":"note.txt"}"#)),
        MockBody::Events(text("done")),
    ]));
    let mut config = agent_config(workspace.path());
    let cache = titi_tools::ReadCache::default();
    config.read_cache = cache.clone();

    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        ToolRegistry::new(),
    );
    let _ = spawn_and_collect(&mut engine, "read the note").await;

    let (_, misses) = cache.stats();
    assert_eq!(
        misses, 1,
        "the subagent's read went through the shared cache"
    );
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn a_subagent_cannot_write_by_default() {
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "write",
            r#"{"path":"sneaky.txt","content":"nope"}"#,
        )),
        MockBody::Events(text("tried")),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        agent_config(workspace.path()),
        resolver(Arc::clone(&transport) as _),
        ToolRegistry::new(),
    );

    let (_, events) = spawn_and_collect(&mut engine, "write a file").await;

    assert!(
        !workspace.path().join("sneaky.txt").exists(),
        "a read-only subagent must not write"
    );
    let requests = transport.requests();
    assert!(
        requests[1].messages.iter().any(|message| {
            message
                .content
                .contains("write is withheld here; only read tools are offered")
        }),
        "the refusal is reported back to the model: {:?}",
        requests[1]
            .messages
            .iter()
            .map(|m| m.content.to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::AgentFinished { success: true, .. }))
    );
}

#[tokio::test]
async fn a_subagent_with_writes_enabled_still_respects_a_foreign_claim() {
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call(
            "write",
            r#"{"path":"taken.rs","content":"mine"}"#,
        )),
        MockBody::Events(text("done")),
    ]));
    let mut config = agent_config(workspace.path());
    config.agent_writes = true;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        ToolRegistry::new(),
    );

    // The parent holds the file the subagent is about to write.
    engine.claims().try_claim("taken.rs", "Main").unwrap();

    let (_, events) = spawn_and_collect(&mut engine, "write taken.rs").await;

    assert!(
        !workspace.path().join("taken.rs").exists(),
        "a claimed file must not be written"
    );
    let requests = transport.requests();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("claimed by Main")),
        "the conflict names the holder: {:?}",
        requests[1]
            .messages
            .iter()
            .map(|m| m.content.to_string())
            .collect::<Vec<_>>()
    );
    // The parent's claim is untouched by the refusal.
    assert_eq!(engine.claims().holder("taken.rs").as_deref(), Some("Main"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::AgentFinished { .. }))
    );
}

#[tokio::test]
async fn a_subagent_stops_at_the_tool_round_cap() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "body").unwrap();
    // Every request asks for another read, so the cap is the only exit. One
    // more body than the cap: the request that trips it must still arrive.
    let mut bodies = Vec::new();
    for _ in 0..4 {
        bodies.push(MockBody::Events(tool_call(
            "read",
            r#"{"path":"note.txt"}"#,
        )));
    }
    let transport = Arc::new(MockTransport::new(bodies));

    let mut config = agent_config(workspace.path());
    config.agent_rounds = 2;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        ToolRegistry::new(),
    );

    let (summary, events) = spawn_and_collect(&mut engine, "loop forever").await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::AgentFinished { success: false, .. })),
        "a runaway subagent fails rather than hanging: {events:?}"
    );
    assert!(
        summary.contains("round cap"),
        "the failure names the cap, not a spent mock: {summary}"
    );
    assert_eq!(
        transport.call_count(),
        3,
        "two rounds run, the third request trips the cap"
    );
}
