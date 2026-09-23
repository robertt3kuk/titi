use std::sync::Arc;

use async_trait::async_trait;
use smol_str::SmolStr;
use titi_engine::{
    AgentContext, AgentKind, AgentRequest, AgentRunner, AgentStatus, EngineCommand, EngineConfig,
    EngineEvent, EngineRuntime, RegistryError, TransportResolver,
};
fn no_models() -> Arc<dyn TransportResolver> {
    Arc::new(|model: &str| Err(RegistryError::UnknownModel(model.into())))
}

struct ReportingRunner;

#[async_trait]
impl AgentRunner for ReportingRunner {
    async fn run(&self, request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        context
            .progress(format!("working on {}", request.task))
            .await;
        Ok("agent complete".into())
    }
}

/// Selecting an agent moves the view; selecting one that does not exist does
/// not pretend it did.
#[tokio::test]
async fn focusing_an_agent_emits_the_move() {
    let mut engine = EngineRuntime::start_with_agents(
        EngineConfig::new("unused"),
        no_models(),
        Arc::new(ReportingRunner),
    );
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Worker".into(),
            task: "look around".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();

    let mut agent_id = None;
    while let Some(event) = engine.recv().await {
        if let EngineEvent::AgentStarted { agent_id: id, .. } = &event {
            agent_id = Some(id.clone());
        }
        if matches!(event, EngineEvent::AgentFinished { .. }) {
            break;
        }
    }
    let agent_id = agent_id.expect("the agent started");

    engine
        .send(EngineCommand::FocusAgent {
            agent_id: agent_id.clone(),
        })
        .await
        .unwrap();
    let focused = engine.recv().await;
    assert!(
        matches!(focused, Some(EngineEvent::AgentFocused { agent_id: Some(ref id) }) if *id == agent_id),
        "focusing reports the move: {focused:?}"
    );

    engine
        .send(EngineCommand::FocusAgent {
            agent_id: "nobody".into(),
        })
        .await
        .unwrap();
    let rejected = engine.recv().await;
    assert!(
        matches!(rejected, Some(EngineEvent::Failed { .. })),
        "an unknown agent is reported, not silently ignored: {rejected:?}"
    );
}

#[tokio::test]
async fn spawn_agent_streams_lifecycle_events() {
    let mut engine = EngineRuntime::start_with_agents(
        EngineConfig::new("unused"),
        no_models(),
        Arc::new(ReportingRunner),
    );
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Trace runtime".into(),
            task: "inspect provider flow".into(),
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

    assert!(
        matches!(events[0], EngineEvent::AgentStarted { ref name, .. } if name == "Trace runtime")
    );
    assert!(events.iter().any(|event| matches!(event, EngineEvent::AgentProgress { text, .. } if text.contains("provider flow"))));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::AgentStatusChanged {
            status: AgentStatus::Completed,
            ..
        }
    )));
    assert!(
        matches!(events.last(), Some(EngineEvent::AgentFinished { success: true, summary, .. }) if summary == "agent complete")
    );
}

/// Reports a finding, claims a file, and then blocks until stopped.
struct ClaimingRunner;

#[async_trait]
impl AgentRunner for ClaimingRunner {
    async fn run(&self, _request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        context.finding("found the wiring");
        context
            .claim("src/shared.rs")
            .map_err(|error| SmolStr::from(error.to_string()))?;
        while !context.is_aborted() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Err("stopped".into())
    }
}

#[tokio::test]
async fn a_subagent_finding_reaches_the_parent_bus() {
    let mut engine = EngineRuntime::start_with_agents(
        EngineConfig::new("unused"),
        no_models(),
        Arc::new(ReportingRunner),
    );
    let before = engine.findings().cursor();
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Trace".into(),
            task: "inspect".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();

    while let Some(event) = engine.recv().await {
        if matches!(event, EngineEvent::AgentFinished { .. }) {
            break;
        }
    }

    let (_, findings) = engine.findings().drain_since(before);
    assert_eq!(findings.len(), 1, "the summary is the agent's finding");
    assert_eq!(findings[0].text, "agent complete");
    assert_eq!(findings[0].agent_id, "agent-1");
}

#[tokio::test]
async fn stopping_an_agent_releases_its_write_claims() {
    let mut engine = EngineRuntime::start_with_agents(
        EngineConfig::new("unused"),
        no_models(),
        Arc::new(ClaimingRunner),
    );
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Holder".into(),
            task: "hold a file".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();

    // Wait until the agent has claimed the file and reported its finding.
    let mut agent_id = String::new();
    while let Some(event) = engine.recv().await {
        match event {
            EngineEvent::AgentStarted { agent_id: id, .. } => agent_id = id.to_string(),
            EngineEvent::AgentFinished { .. } => panic!("the runner blocks until stopped"),
            _ => {}
        }
        if engine.claims().holder("src/shared.rs").is_some() {
            break;
        }
    }
    assert_eq!(
        engine.claims().holder("src/shared.rs").as_deref(),
        Some(agent_id.as_str()),
        "the agent holds the file while it runs"
    );

    engine
        .send(EngineCommand::StopAgent {
            agent_id: agent_id.clone().into(),
        })
        .await
        .unwrap();

    let mut stopped = false;
    while let Some(event) = engine.recv().await {
        if matches!(
            event,
            EngineEvent::AgentStatusChanged {
                status: AgentStatus::Aborted,
                ..
            }
        ) {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "the stop was acknowledged");
    assert!(
        engine.claims().is_empty(),
        "a stopped agent must not leave files locked: {:?}",
        engine.claims().holder("src/shared.rs")
    );
}

struct WaitingRunner;

#[async_trait]
impl AgentRunner for WaitingRunner {
    async fn run(&self, _request: AgentRequest, context: AgentContext) -> Result<SmolStr, SmolStr> {
        while !context.is_aborted() {
            tokio::task::yield_now().await;
        }
        Err("aborted".into())
    }
}

#[tokio::test]
async fn stop_agent_aborts_the_running_agent() {
    let mut engine = EngineRuntime::start_with_agents(
        EngineConfig::new("unused"),
        no_models(),
        Arc::new(WaitingRunner),
    );
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Worker".into(),
            task: "wait".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();

    let agent_id = match engine.recv().await {
        Some(EngineEvent::AgentStarted { agent_id, .. }) => agent_id,
        other => panic!("expected AgentStarted, got {other:?}"),
    };
    engine
        .send(EngineCommand::StopAgent {
            agent_id: agent_id.clone(),
        })
        .await
        .unwrap();

    assert!(matches!(
        engine.recv().await,
        Some(EngineEvent::AgentStatusChanged { agent_id: id, status: AgentStatus::Aborted }) if id == agent_id
    ));
}

#[tokio::test]
async fn streaming_runner_reports_provider_progress() {
    use titi_engine::{ResolvedModel, StreamingAgentRunner};
    use titi_providers::{BlockId, MockBody, MockTransport, StopReason, StreamEvent, Transport};

    let transport: Arc<dyn Transport> = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: "found it".into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])]));
    let resolver: Arc<dyn TransportResolver> = Arc::new(move |model: &str| {
        Ok(ResolvedModel::without_credential(
            model,
            Arc::clone(&transport),
        ))
    });
    let mut engine = EngineRuntime::start_with_agents(
        EngineConfig::new("unused"),
        Arc::clone(&resolver),
        Arc::new(StreamingAgentRunner::new(resolver, "primary")),
    );
    engine
        .send(EngineCommand::SpawnAgent {
            name: "Scout".into(),
            task: "search".into(),
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
    assert!(events.iter().any(
        |event| matches!(event, EngineEvent::AgentProgress { text, .. } if text == "found it")
    ));
    assert!(matches!(
        events.last(),
        Some(EngineEvent::AgentFinished { success: true, summary, .. }) if summary == "found it"
    ));
}

/// A subagent has no surface to show an approval on: its tool events go to a
/// dropped channel and nobody can answer the prompt. So with `agent_writes`
/// the registry keeps write and exec tools *and* the runner must be able to
/// run them — otherwise the first write parks forever and the agent never
/// reports back.
#[tokio::test]
async fn a_subagent_with_write_access_does_not_park_on_approval() {
    use std::time::Duration;
    use titi_engine::ResolvedModel;
    use titi_providers::{
        BlockId, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, Transport,
    };
    use titi_tools::{ApprovalMode, ToolRegistry};

    let workspace = tempfile::tempdir().expect("a temp workspace");
    let transport: Arc<dyn Transport> = Arc::new(MockTransport::new(vec![
        MockBody::Events(vec![
            StreamEvent::ToolcallStart {
                id: BlockId::new("tool"),
                call: ToolCallRef {
                    call_id: "call-1".into(),
                    name: "write".into(),
                },
            },
            StreamEvent::ToolcallDelta {
                id: BlockId::new("tool"),
                json: r#"{"path":"notes.md","content":"from the subagent"}"#.into(),
            },
            StreamEvent::ToolcallEnd {
                id: BlockId::new("tool"),
            },
            StreamEvent::Done {
                reason: StopReason::ToolUse,
            },
        ]),
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "wrote the notes".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
    ]));
    let resolver: Arc<dyn TransportResolver> = Arc::new(move |model: &str| {
        Ok(ResolvedModel::without_credential(
            model,
            Arc::clone(&transport),
        ))
    });

    let mut config = EngineConfig::new("primary");
    config.agent_model = Some("agent-model".into());
    config.workspace_root = Some(workspace.path().to_path_buf());
    config.agent_writes = true;
    // The session itself still asks for anything above read tier; the
    // subagent's registry is the exception it was built with.
    config.approval_mode = ApprovalMode::Write;
    let mut engine = EngineRuntime::start_with_tools(config, resolver, ToolRegistry::new());

    engine
        .send(EngineCommand::SpawnAgent {
            name: "Worker".into(),
            task: "write the notes".into(),
            kind: AgentKind::Subagent,
        })
        .await
        .unwrap();

    let summary = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = engine.recv().await {
            if let EngineEvent::AgentFinished {
                success, summary, ..
            } = event
            {
                assert!(success, "the subagent failed: {summary}");
                return summary;
            }
        }
        panic!("the engine stopped before the agent finished");
    })
    .await
    .expect("a subagent parked on an approval nobody can answer");

    assert_eq!(summary, "wrote the notes");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.md")).expect("the file was written"),
        "from the subagent"
    );
}
