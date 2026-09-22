use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{ApprovalMode, EchoTool, ShellProbeTool, ToolRegistry};

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
    tools.register(Arc::new(ShellProbeTool));
    tools
}

/// The next event that is not a context-usage report.
async fn next_tool_event(engine: &mut titi_engine::Engine) -> Option<EngineEvent> {
    loop {
        match engine.recv().await {
            Some(EngineEvent::ContextUsage { .. }) => {}
            other => return other,
        }
    }
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

/// Tool-call events whose arguments arrive in fragments, the way a real
/// OpenAI-compatible stream delivers them.
fn tool_call_events_split(name: &str, fragments: &[&str]) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::ToolcallStart {
        id: BlockId::new("tool"),
        call: ToolCallRef {
            call_id: "call-1".into(),
            name: name.into(),
        },
    }];
    for fragment in fragments {
        events.push(StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: (*fragment).into(),
        });
    }
    events.push(StreamEvent::ToolcallEnd {
        id: BlockId::new("tool"),
    });
    events.push(StreamEvent::Done {
        reason: StopReason::ToolUse,
    });
    events
}

fn tool_call_events(name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: name.into(),
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

#[tokio::test]
async fn auto_approves_read_tool_and_continues() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events("echo", r#"{"text":"pong"}"#)),
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "done".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(transport),
        echo_registry(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ToolFinished { output, is_error: false, .. } if output == "pong"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::StreamDelta { text, .. } if text == "done"
    )));
}

/// Tool output goes to a remote provider; a key or a server address in it
/// must be masked before the model or the transcript sees it.
#[tokio::test]
async fn tool_output_is_masked_before_the_model_sees_it() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events(
            "echo",
            r#"{"text":"KEY=sk-test-0000000000000000 host 203.0.113.7"}"#,
        )),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(transport),
        echo_registry(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    let output = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolFinished { output, .. } => Some(output.to_string()),
            _ => None,
        })
        .unwrap();
    assert!(!output.contains("sk-test-0000000000000000"), "{output}");
    assert!(!output.contains("203.0.113.7"), "{output}");
    assert!(output.contains("host [ip]"), "{output}");
}

/// Turning address masking off is the user's call; keys stay masked anyway.
#[tokio::test]
async fn with_ip_masking_off_keys_are_still_masked() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events(
            "echo",
            r#"{"text":"KEY=sk-test-0000000000000000 host 203.0.113.7"}"#,
        )),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut config = EngineConfig::new("primary");
    config.mask_ips = false;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), echo_registry());
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    let output = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolFinished { output, .. } => Some(output.to_string()),
            _ => None,
        })
        .unwrap();
    assert!(!output.contains("sk-test-0000000000000000"), "{output}");
    assert!(output.contains("203.0.113.7"), "{output}");
}

#[tokio::test]
async fn fragmented_tool_arguments_reach_the_handler_joined() {
    // The arguments arrive as fragments, the way a real OpenAI-compatible
    // stream delivers them. Appending each fragment must reconstruct the JSON;
    // a decoder that re-sends the accumulated buffer corrupts it.
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events_split(
            "echo",
            &[r#"{"text":"po"#, r#"ng"}"#],
        )),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(transport),
        echo_registry(),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolFinished { output, is_error: false, .. } if output == "pong"
        )),
        "fragments must reconstruct the arguments: {events:?}"
    );
}

#[tokio::test]
async fn exec_tool_waits_for_approval() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events("shell_probe", r#"{"command":"ls"}"#)),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Write;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), echo_registry());
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let started = engine.recv().await;
    assert!(matches!(started, Some(EngineEvent::TurnStarted { .. })));
    // ContextUsage is reported before the request is sent, so it lands
    // between the turn starting and the first tool.
    let tool_started = next_tool_event(&mut engine).await;
    assert!(matches!(
        tool_started,
        Some(EngineEvent::ToolStarted { name, .. }) if name == "shell_probe"
    ));
    let needed = engine.recv().await;
    assert!(matches!(
        needed,
        Some(EngineEvent::ToolApprovalNeeded { name, call_id, .. })
            if name == "shell_probe" && call_id == "call-1"
    ));
    engine
        .send(EngineCommand::ApproveTool {
            call_id: "call-1".into(),
            approved: true,
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ToolFinished { output, is_error: false, .. } if output == "ran ls"
    )));
}

#[tokio::test]
async fn cancelling_a_turn_unblocks_a_pending_approval() {
    // The moment a user most wants Ctrl+C is while an approval prompt sits
    // there. The tool loop waits on a oneshot nobody will fill, so a cancel
    // that does not also break that wait hangs the turn forever.
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(
        tool_call_events("shell_probe", r#"{"command":"ls"}"#),
    )]));
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Write;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), echo_registry());
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    assert!(matches!(
        engine.recv().await,
        Some(EngineEvent::TurnStarted { .. })
    ));
    assert!(matches!(
        next_tool_event(&mut engine).await,
        Some(EngineEvent::ToolStarted { .. })
    ));
    assert!(matches!(
        engine.recv().await,
        Some(EngineEvent::ToolApprovalNeeded { .. })
    ));

    engine.send(EngineCommand::Cancel).await.unwrap();

    // The turn must end, and it must end as cancelled.
    let events = collect_until_terminal(&mut engine).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Cancelled { .. })),
        "a cancelled turn reports cancellation: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolFinished { .. })),
        "the un-approved tool never ran: {events:?}"
    );
}

#[tokio::test]
async fn a_long_turn_folds_its_oldest_messages() {
    // A tiny window makes the threshold fire on the second round, which is
    // exactly what a long session does for real.
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events("echo", r#"{"text":"pong"}"#)),
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: "done".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
    ]));
    let mut config = EngineConfig::new("primary");
    // A one-token window arms compaction immediately; a tiny keep-recent
    // budget leaves a prefix to fold. The defaults (80% of a real window,
    // 20k-token tail) mean a short turn legitimately folds nothing.
    config.context_window = 1;
    config.compaction = titi_core::compaction::CompactionPolicy {
        threshold_percent: 0.0,
        keep_recent_tokens: 4,
        ..Default::default()
    };
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        echo_registry(),
    );
    engine
        .send(EngineCommand::SubmitPrompt {
            text: "say something long enough to matter".into(),
        })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let compacted = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::Compacted {
                folded,
                tokens_before,
                strategy,
                ..
            } => Some((*folded, *tokens_before, strategy.clone())),
            _ => None,
        })
        .expect("the turn reports a compaction");
    assert!(compacted.0 >= 1, "something was folded: {compacted:?}");
    assert_eq!(compacted.2, "snapcompact", "the model-free strategy ran");

    // The request that followed carries the digest, not the folded prefix.
    let requests = transport.requests();
    let second = requests.last().expect("a second request");
    assert_eq!(second.messages[0].role, Role::System);
    assert!(
        second.messages[0].content.contains("folded"),
        "the digest explains what was dropped: {}",
        second.messages[0].content
    );
    // And the turn still completed.
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::StreamDelta { text, .. } if text == "done"
    )));
}

#[tokio::test]
async fn tool_round_cap_stops_the_turn() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events("echo", r#"{"text":"one"}"#)),
        MockBody::Events(tool_call_events("echo", r#"{"text":"two"}"#)),
    ]));
    let mut config = EngineConfig::new("primary");
    config.max_tool_rounds = 1;
    let mut engine = EngineRuntime::start_with_tools(config, resolver(transport), echo_registry());
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    assert!(matches!(
        events.last(),
        Some(EngineEvent::Failed { message, .. }) if message == "tool round cap reached"
    ));
}

#[tokio::test]
async fn session_trajectory_records_user_tools_and_turn_end() {
    use titi_core::trajectory::{EventKind, TrajectoryRecorder};
    use titi_engine::TrajectorySink;
    use tokio::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let recorder = TrajectoryRecorder::open(dir.path(), "sess").unwrap();
    let trajectory: TrajectorySink = Arc::new(Mutex::new(Some(recorder)));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_call_events("echo", r#"{"text":"pong"}"#)),
        MockBody::Events(vec![StreamEvent::Done {
            reason: StopReason::Stop,
        }]),
    ]));
    let mut engine = EngineRuntime::start_with_session(
        EngineConfig::new("primary"),
        resolver(transport),
        None,
        echo_registry(),
        trajectory,
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .unwrap();
    let _ = collect_until_terminal(&mut engine).await;
    let replay = TrajectoryRecorder::open(dir.path(), "sess").unwrap();
    let kinds: Vec<_> = replay
        .tail(16)
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::UserMessage { text } if text == "hi"))
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ToolCall { name, .. } if name == "echo"))
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::ToolResult { ok: true, .. }))
    );
    assert!(kinds.iter().any(|kind| matches!(kind, EventKind::TurnEnd)));
}
