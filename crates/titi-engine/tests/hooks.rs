//! Hooks through the real turn: the gate a tool call passes, the prompt, the
//! end of a turn and the start of a session.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::sync::Arc;

use titi_config::hooks::{HookDef, HookEvent};
use titi_engine::protocol::HookOutcome;
use titi_engine::{
    Engine, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
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

fn registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(ShellProbeTool));
    tools
}

fn hook(event: HookEvent, matcher: Option<&str>, command: &str) -> HookDef {
    HookDef {
        event,
        matcher: matcher.map(str::to_owned),
        command: command.to_owned(),
        timeout_secs: None,
        source: "test".to_owned(),
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

async fn start(
    transport: &Arc<MockTransport>,
    approval: ApprovalMode,
    hooks: Vec<HookDef>,
) -> Engine {
    let mut config = EngineConfig::new("primary");
    config.approval_mode = approval;
    let engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(transport) as Arc<dyn Transport>),
        registry(),
    );
    engine
        .send(EngineCommand::SetHooks { hooks })
        .await
        .unwrap();
    engine
}

async fn submit(engine: &Engine, text: &str) {
    engine
        .send(EngineCommand::SubmitPrompt { text: text.into() })
        .await
        .unwrap();
}

/// Events up to and including the first one `stop` accepts.
async fn until(engine: &mut Engine, stop: impl Fn(&EngineEvent) -> bool) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = stop(&event);
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

fn turn_end(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::TurnFinished { .. }
            | EngineEvent::Failed { .. }
            | EngineEvent::Cancelled { .. }
    )
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

fn hook_rows(events: &[EngineEvent]) -> Vec<(String, HookOutcome, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::HookRan {
                event,
                outcome,
                detail,
                ..
            } => Some((event.to_string(), *outcome, detail.to_string())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_exit_two_pre_tool_hook_refuses_the_call_and_the_model_gets_the_reason() {
    let transport = Arc::new(MockTransport::new(vec![
        call("shell_probe", r#"{"command":"rm -rf /"}"#),
        say("understood"),
    ]));
    let mut engine = start(
        &transport,
        // Yolo: the user's own approval mode would run it. The hook is what
        // stops it.
        ApprovalMode::Yolo,
        vec![hook(
            HookEvent::PreToolUse,
            Some("shell_probe"),
            "cat >/dev/null; echo 'rm -rf is not allowed here' >&2; exit 2",
        )],
    )
    .await;
    submit(&engine, "clean up").await;
    let events = until(&mut engine, turn_end).await;

    let (output, is_error) = tool_answer(&events);
    assert!(is_error, "{output}");
    assert!(output.contains("rm -rf is not allowed here"), "{output}");
    assert!(
        !output.contains("ran rm"),
        "the tool must not have run: {output}"
    );
    assert_eq!(
        hook_rows(&events),
        vec![(
            "PreToolUse".to_owned(),
            HookOutcome::Blocked,
            "rm -rf is not allowed here".to_owned()
        )]
    );

    let second = &transport.requests()[1];
    let told = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("the refusal goes back to the model");
    assert!(told.content.contains("rm -rf is not allowed here"));
}

#[tokio::test]
async fn a_hook_gates_a_read_tool_that_would_otherwise_run_in_a_group() {
    let transport = Arc::new(MockTransport::new(vec![
        call("echo", r#"{"text":"secret plans"}"#),
        say("ok"),
    ]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        vec![hook(
            HookEvent::PreToolUse,
            Some("echo"),
            "cat >/dev/null; echo 'no echoing' >&2; exit 2",
        )],
    )
    .await;
    submit(&engine, "go").await;
    let events = until(&mut engine, turn_end).await;
    let (output, is_error) = tool_answer(&events);
    assert!(is_error && output.contains("no echoing"), "{output}");
}

#[tokio::test]
async fn an_ask_decision_forces_the_prompt_even_under_yolo() {
    let transport = Arc::new(MockTransport::new(vec![
        call("shell_probe", r#"{"command":"ls"}"#),
        say("done"),
    ]));
    let mut engine = start(
        &transport,
        ApprovalMode::Yolo,
        vec![hook(
            HookEvent::PreToolUse,
            Some("shell_probe"),
            r#"cat >/dev/null; printf '%s' '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"look first"}}'"#,
        )],
    )
    .await;
    submit(&engine, "list").await;
    let before = until(&mut engine, |event| {
        matches!(event, EngineEvent::ToolApprovalNeeded { .. })
    })
    .await;
    assert!(
        hook_rows(&before)
            .iter()
            .any(|(_, outcome, detail)| *outcome == HookOutcome::Asked && detail == "look first"),
        "{before:?}"
    );
    engine
        .send(EngineCommand::ApproveTool {
            call_id: "call-1".into(),
            approved: true,
        })
        .await
        .unwrap();
    let events = until(&mut engine, turn_end).await;
    let (output, is_error) = tool_answer(&events);
    assert!(!is_error && output.starts_with("ran ls"), "{output}");
}

#[tokio::test]
async fn an_allow_decision_does_not_skip_the_users_approval() {
    let transport = Arc::new(MockTransport::new(vec![
        call("shell_probe", r#"{"command":"ls"}"#),
        say("done"),
    ]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        vec![hook(
            HookEvent::PreToolUse,
            Some("shell_probe"),
            r#"cat >/dev/null; printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#,
        )],
    )
    .await;
    submit(&engine, "list").await;
    let seen = until(&mut engine, |event| {
        matches!(event, EngineEvent::ToolApprovalNeeded { .. })
    })
    .await;
    assert!(
        matches!(seen.last(), Some(EngineEvent::ToolApprovalNeeded { .. })),
        "an exec call still asks under write mode: {seen:?}"
    );
}

#[tokio::test]
async fn a_post_tool_hook_note_rides_on_the_tool_answer() {
    let transport = Arc::new(MockTransport::new(vec![
        call("echo", r#"{"text":"pong"}"#),
        say("ok"),
    ]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        vec![hook(
            HookEvent::PostToolUse,
            Some("echo"),
            r#"cat >/dev/null; printf '%s' '{"hookSpecificOutput":{"additionalContext":"formatted 1 file"}}'"#,
        )],
    )
    .await;
    submit(&engine, "go").await;
    let events = until(&mut engine, turn_end).await;
    let (output, is_error) = tool_answer(&events);
    assert!(!is_error);
    assert!(output.starts_with("pong"), "{output}");
    assert!(output.contains("[hook"), "{output}");
    assert!(output.contains("formatted 1 file"), "{output}");
}

#[tokio::test]
async fn a_blocked_prompt_is_never_sent_and_comes_back_with_the_reason() {
    let transport = Arc::new(MockTransport::new(vec![say("never")]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        vec![hook(
            HookEvent::UserPromptSubmit,
            None,
            "cat >/dev/null; echo 'no secrets in prompts' >&2; exit 2",
        )],
    )
    .await;
    submit(&engine, "my key is hunter2").await;
    let events = until(&mut engine, |event| {
        matches!(event, EngineEvent::PromptReturned { .. })
    })
    .await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::Notice { message } if message.contains("no secrets in prompts")
        )),
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(EngineEvent::PromptReturned { text }) if text == "my key is hunter2"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnStarted { .. })),
        "no turn started"
    );
    assert_eq!(transport.call_count(), 0, "nothing reached the provider");
}

#[tokio::test]
async fn prompt_hook_output_is_context_after_the_prompt() {
    let transport = Arc::new(MockTransport::new(vec![say("ok")]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        vec![hook(
            HookEvent::UserPromptSubmit,
            None,
            "cat >/dev/null; echo 'current branch: wt/hooks'",
        )],
    )
    .await;
    submit(&engine, "what branch").await;
    until(&mut engine, turn_end).await;
    let first = &transport.requests()[0];
    let user = first
        .messages
        .iter()
        .rfind(|message| message.role == Role::User)
        .unwrap();
    assert!(user.content.starts_with("what branch"), "{}", user.content);
    assert!(user.content.contains("current branch: wt/hooks"));
}

#[tokio::test]
async fn a_stop_hook_that_refuses_continues_the_turn_exactly_once() {
    let transport = Arc::new(MockTransport::new(vec![say("first"), say("second")]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        // Refuses every time: only the cap ends it.
        vec![hook(
            HookEvent::Stop,
            None,
            "cat >/dev/null; echo 'run the tests first' >&2; exit 2",
        )],
    )
    .await;
    submit(&engine, "do it").await;
    let events = until(&mut engine, turn_end).await;
    assert_eq!(transport.call_count(), 2, "one continuation and no more");
    let second = &transport.requests()[1];
    let nudge = second
        .messages
        .iter()
        .rfind(|message| message.role == Role::User)
        .unwrap();
    assert!(
        nudge.content.contains("run the tests first"),
        "{}",
        nudge.content
    );
    assert_eq!(
        hook_rows(&events)
            .iter()
            .filter(|(event, ..)| event == "Stop")
            .count(),
        1,
        "the hook is asked once"
    );
}

#[tokio::test]
async fn session_start_output_joins_the_system_prompt_once() {
    let transport = Arc::new(MockTransport::new(vec![say("one"), say("two")]));
    let mut engine = start(
        &transport,
        ApprovalMode::Write,
        vec![hook(
            HookEvent::SessionStart,
            None,
            "cat >/dev/null; date +%N; echo 'house rules: be brief'",
        )],
    )
    .await;
    submit(&engine, "first").await;
    until(&mut engine, turn_end).await;
    submit(&engine, "second").await;
    until(&mut engine, turn_end).await;

    let requests = transport.requests();
    let system = |index: usize| {
        requests[index]
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .map(|message| message.content.to_string())
            .expect("a system message")
    };
    assert!(system(0).contains("house rules: be brief"), "{}", system(0));
    // The same bytes on the next turn: the hook ran once, so the provider's
    // cache of the prompt is not invalidated.
    assert_eq!(system(0), system(1));
}

#[tokio::test]
async fn a_hook_that_crashes_does_not_stop_the_turn() {
    let transport = Arc::new(MockTransport::new(vec![
        call("shell_probe", r#"{"command":"ls"}"#),
        say("done"),
    ]));
    let mut engine = start(
        &transport,
        ApprovalMode::Yolo,
        vec![hook(
            HookEvent::PreToolUse,
            None,
            "cat >/dev/null; echo 'segfault' >&2; exit 139",
        )],
    )
    .await;
    submit(&engine, "list").await;
    let events = until(&mut engine, turn_end).await;
    let (output, is_error) = tool_answer(&events);
    assert!(!is_error, "{output}");
    assert!(
        hook_rows(&events)
            .iter()
            .any(|(_, outcome, _)| *outcome == HookOutcome::Warning)
    );
}
