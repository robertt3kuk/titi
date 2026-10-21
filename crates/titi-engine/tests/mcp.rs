//! MCP servers through the real engine: `SetMcp` starts them in the
//! background, their tools join the next turn as `mcp__<server>__<tool>`,
//! and a call goes through approval, hooks and the output bound like any
//! other tool. The server is the python fixture of `titi-mcp`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use titi_config::hooks::{HookDef, HookEvent};
use titi_config::mcp::{McpApproval, McpServerDef};
use titi_engine::protocol::SessionMode;
use titi_engine::{
    Engine, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_mcp::{ServerState, ServerStatus};
use titi_providers::{
    BlockId, MockBody, MockTransport, Role, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{ApprovalMode, ToolRegistry};

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

fn call(name: &str, args: &str) -> MockBody {
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

fn say(text: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: text.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

fn server(name: &str, env: &[(&str, &str)]) -> McpServerDef {
    McpServerDef {
        name: name.to_owned(),
        command: "python3".to_owned(),
        args: vec![format!(
            "{}/../titi-mcp/tests/fixtures/mcp_server.py",
            env!("CARGO_MANIFEST_DIR")
        )],
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        cwd: None,
        disabled: false,
        timeout_secs: Some(10),
        approval: None,
        source: "test".to_owned(),
    }
}

fn start(transport: &Arc<MockTransport>, approval: ApprovalMode) -> Engine {
    let mut config = EngineConfig::new("primary");
    config.approval_mode = approval;
    let resolver = Arc::new(MapResolver(
        [(
            "primary".to_owned(),
            Arc::clone(transport) as Arc<dyn Transport>,
        )]
        .into_iter()
        .collect(),
    ));
    EngineRuntime::start_with_tools(config, resolver, ToolRegistry::new())
}

/// Events up to and including the first one `stop` accepts.
async fn until(
    engine: &mut Engine,
    mut stop: impl FnMut(&EngineEvent) -> bool,
) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Ok(Some(event)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        engine.recv(),
    )
    .await
    {
        let done = stop(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
    panic!("the engine never produced the event: {seen:?}");
}

fn turn_end(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::TurnFinished { .. }
            | EngineEvent::Failed { .. }
            | EngineEvent::Cancelled { .. }
    )
}

fn servers_of(event: &EngineEvent) -> Option<&[ServerStatus]> {
    match event {
        EngineEvent::McpStatus { servers } => Some(servers),
        _ => None,
    }
}

/// Waits until the status says every named server is `state`.
async fn wait_for(engine: &mut Engine, names: &[&str], state: ServerState) -> Vec<ServerStatus> {
    let mut found = None;
    until(engine, |event| {
        servers_of(event).is_some_and(|servers| {
            let all = names.iter().all(|name| {
                servers
                    .iter()
                    .any(|server| server.name == *name && server.state == state)
            });
            if all {
                found = Some(servers.to_vec());
            }
            all
        })
    })
    .await;
    found.unwrap()
}

async fn set_mcp(engine: &Engine, servers: Vec<McpServerDef>) {
    engine
        .send(EngineCommand::SetMcp { servers })
        .await
        .unwrap();
}

async fn submit(engine: &Engine, text: &str) {
    engine
        .send(EngineCommand::SubmitPrompt { text: text.into() })
        .await
        .unwrap();
}

fn tool_answer(events: &[EngineEvent]) -> (String, bool) {
    events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolFinished {
                output, is_error, ..
            } => Some((output.to_string(), *is_error)),
            _ => None,
        })
        .expect("the call was answered")
}

#[tokio::test(flavor = "multi_thread")]
async fn set_mcp_returns_at_once_and_the_status_goes_from_starting_to_ready() {
    let transport = Arc::new(MockTransport::new(vec![]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    // The first thing back is the answer to the command, before any server
    // has had time to start.
    let first = until(&mut engine, |event| servers_of(event).is_some()).await;
    let first = servers_of(first.last().unwrap()).unwrap();
    assert_eq!(first[0].name, "fixture");
    assert_eq!(first[0].state, ServerState::Starting, "{first:?}");
    let ready = wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    assert!(
        ready[0]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__fixture__echo")
    );
    assert!(ready[0].command.starts_with("python3 "));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_sent_right_after_set_mcp_waits_for_the_servers_that_are_starting() {
    // A headless one-shot submits its prompt the moment the engine is up; the
    // first turn must still see the tools, not "unknown tool".
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__echo", r#"{"text":"early"}"#),
        say("done"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    submit(&engine, "go at once").await;
    let events = until(&mut engine, turn_end).await;
    assert_eq!(tool_answer(&events), ("early".to_owned(), false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_is_still_starting_after_the_wait_does_not_hold_the_turn_forever() {
    let transport = Arc::new(MockTransport::new(vec![say("hello")]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    let mut hung = server("hung", &[("FIXTURE_NEVER_ANSWER", "1")]);
    hung.timeout_secs = Some(120);
    set_mcp(&engine, vec![hung]).await;
    let started = Instant::now();
    submit(&engine, "hello").await;
    let events = until(&mut engine, turn_end).await;
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::Notice { message } if message.contains("hung") && message.contains("still starting")
        )),
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_is_offered_the_tool_and_a_call_round_trips_under_yolo() {
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__echo", r#"{"text":"hello from the model"}"#),
        say("done"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "echo something").await;
    let events = until(&mut engine, turn_end).await;
    assert_eq!(
        tool_answer(&events),
        ("hello from the model".to_owned(), false)
    );
    let first = &transport.requests()[0];
    let echo = first
        .tools
        .iter()
        .find(|spec| spec.name == "mcp__fixture__echo")
        .expect("the server's tool is declared to the model");
    assert_eq!(echo.description, "Echo the text back");
    assert_eq!(echo.parameters["required"], serde_json::json!(["text"]));
    let told = transport.requests()[1]
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .map(|message| message.content.to_string())
        .unwrap();
    assert_eq!(told, "hello from the model");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_call_waits_for_approval_in_write_mode_as_an_exec_tool() {
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__echo", r#"{"text":"x"}"#),
        say("done"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Write);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "go").await;
    let asked = until(&mut engine, |event| {
        matches!(event, EngineEvent::ToolApprovalNeeded { .. })
    })
    .await;
    let Some(EngineEvent::ToolApprovalNeeded { call_id, name, .. }) = asked.last() else {
        unreachable!()
    };
    assert_eq!(name, "mcp__fixture__echo");
    engine
        .send(EngineCommand::ApproveTool {
            call_id: call_id.clone(),
            approved: false,
        })
        .await
        .unwrap();
    let events = until(&mut engine, turn_end).await;
    let (output, is_error) = tool_answer(&events);
    assert_eq!(
        (output.as_str(), is_error),
        ("tool invocation denied", true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_set_read_approval_runs_without_a_prompt_in_write_mode() {
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__echo", r#"{"text":"quiet"}"#),
        say("done"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Write);
    let mut lowered = server("fixture", &[]);
    lowered.approval = Some(McpApproval::Read);
    set_mcp(&engine, vec![lowered]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "go").await;
    let events = until(&mut engine, turn_end).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolApprovalNeeded { .. })),
        "{events:?}"
    );
    assert_eq!(tool_answer(&events), ("quiet".to_owned(), false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_tool_hook_written_for_claude_names_matches_an_mcp_tool() {
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__echo", r#"{"text":"x"}"#),
        say("understood"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    engine
        .send(EngineCommand::SetHooks {
            hooks: vec![HookDef {
                event: HookEvent::PreToolUse,
                matcher: Some("mcp__.*__echo".to_owned()),
                command: "cat >/dev/null; echo 'no mcp echo' >&2; exit 2".to_owned(),
                timeout_secs: None,
                source: "test".to_owned(),
            }],
        })
        .await
        .unwrap();
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "go").await;
    let events = until(&mut engine, turn_end).await;
    let (output, is_error) = tool_answer(&events);
    assert!(is_error && output.contains("no mcp echo"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_is_masked_and_bounded_like_any_tool_output() {
    let transport = Arc::new(MockTransport::new(vec![
        call(
            "mcp__fixture__echo",
            r#"{"text":"key sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD"}"#,
        ),
        say("ok"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "go").await;
    let (output, _) = tool_answer(&until(&mut engine, turn_end).await);
    assert!(
        !output.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
        "{output}"
    );

    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__big", r#"{"size":100000}"#),
        say("ok"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "go").await;
    let (output, _) = tool_answer(&until(&mut engine, turn_end).await);
    assert!(output.len() < 45_000, "{}", output.len());
    assert!(output.contains("characters left out"), "{}", &output[..200]);
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_withholds_exec_tier_mcp_tools_but_not_a_lowered_one() {
    let transport = Arc::new(MockTransport::new(vec![say("planned")]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    let mut lowered = server("calm", &[]);
    lowered.approval = Some(McpApproval::Read);
    set_mcp(&engine, vec![server("fixture", &[]), lowered]).await;
    wait_for(&mut engine, &["fixture", "calm"], ServerState::Ready).await;
    engine
        .send(EngineCommand::SetMode {
            mode: SessionMode::Plan,
        })
        .await
        .unwrap();
    submit(&engine, "plan").await;
    until(&mut engine, turn_end).await;
    let names: Vec<String> = transport.requests()[0]
        .tools
        .iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert!(names.contains(&"mcp__calm__echo".to_owned()), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("mcp__fixture__")),
        "{names:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_fails_is_reported_with_a_notice_and_the_session_goes_on() {
    let transport = Arc::new(MockTransport::new(vec![say("fine")]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    let mut broken = server("broken", &[]);
    broken.command = "titi-no-such-program".to_owned();
    set_mcp(&engine, vec![broken, server("fixture", &[])]).await;
    let events = until(&mut engine, |event| {
        servers_of(event).is_some_and(|servers| {
            servers.iter().any(|s| s.state == ServerState::Failed)
                && servers.iter().any(|s| s.state == ServerState::Ready)
        })
    })
    .await;
    let failed = servers_of(events.last().unwrap())
        .unwrap()
        .iter()
        .find(|server| server.name == "broken")
        .unwrap();
    assert!(failed.reason.contains("could not start"), "{failed:?}");
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::Notice { message } if message.contains("broken failed")
        )),
        "{events:?}"
    );
    submit(&engine, "hello").await;
    until(&mut engine, turn_end).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_and_restart_answer_and_a_new_set_replaces_the_old() {
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__pid", "{}"),
        say("a"),
        call("mcp__fixture__pid", "{}"),
        say("b"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    engine.send(EngineCommand::ListMcp).await.unwrap();
    until(&mut engine, |event| servers_of(event).is_some()).await;

    submit(&engine, "pid").await;
    let (first, _) = tool_answer(&until(&mut engine, turn_end).await);

    engine
        .send(EngineCommand::RestartMcp {
            name: "fixture".into(),
        })
        .await
        .unwrap();
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "pid").await;
    let (second, _) = tool_answer(&until(&mut engine, turn_end).await);
    assert_ne!(first, second, "a restart is a new process");

    engine
        .send(EngineCommand::RestartMcp {
            name: "nobody".into(),
        })
        .await
        .unwrap();
    until(&mut engine, |event| {
        matches!(event, EngineEvent::Notice { message } if message.contains("no server named nobody"))
    })
    .await;

    // The empty set stops everything.
    set_mcp(&engine, vec![]).await;
    let events = until(&mut engine, |event| {
        servers_of(event).is_some_and(<[ServerStatus]>::is_empty)
    })
    .await;
    assert!(!events.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_stops_the_servers_before_the_engine_ends() {
    let transport = Arc::new(MockTransport::new(vec![
        call("mcp__fixture__pid", "{}"),
        say("a"),
    ]));
    let mut engine = start(&transport, ApprovalMode::Yolo);
    set_mcp(&engine, vec![server("fixture", &[])]).await;
    wait_for(&mut engine, &["fixture"], ServerState::Ready).await;
    submit(&engine, "pid").await;
    let (pid, _) = tool_answer(&until(&mut engine, turn_end).await);
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    assert!(alive(&pid));
    engine.send(EngineCommand::Shutdown).await.unwrap();
    while engine.recv().await.is_some() {}
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(&pid) {
        assert!(
            Instant::now() < deadline,
            "server {pid} outlived the engine"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
