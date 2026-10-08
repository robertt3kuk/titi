use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::App;
use titi_cli::themes::default_theme;
use titi_engine::{AgentKind, AgentStatus, EngineEvent, TurnId};
use titi_providers::{ErrorReason, StopReason};

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(true)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

/// Herdr classifies a pane from what we report, so the classification has to
/// match what the user is actually waiting on.
#[test]
fn herdr_state_follows_the_turn() {
    use titi_cli::herdr::AgentState;

    let mut app = app();
    assert_eq!(app.herdr_state().0, AgentState::Idle);

    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    assert_eq!(app.herdr_state().0, AgentState::Working);

    // An approval is the moment another agent should stop and look.
    app.ingest_engine_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "shell".into(),
    });
    let (state, message) = app.herdr_state();
    assert_eq!(state, AgentState::Blocked);
    assert_eq!(message.as_deref(), Some("waiting for approval"));
}

#[test]
fn engine_events_render_stream_thinking_tools_and_agents() {
    let mut app = app();
    let turn_id = TurnId(1);
    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id,
        model: "test/model".into(),
    });
    app.ingest_engine_event(EngineEvent::StreamDelta {
        turn_id,
        text: "answer".into(),
    });
    app.ingest_engine_event(EngineEvent::ThinkingDelta {
        turn_id,
        text: "reasoning".into(),
    });
    app.ingest_engine_event(EngineEvent::ToolStarted {
        turn_id,
        call_id: "call-1".into(),
        name: "read".into(),
        detail: None,
    });
    app.ingest_engine_event(EngineEvent::AgentStarted {
        agent_id: "agent-1".into(),
        name: "Trace runtime".into(),
        parent_id: None,
        kind: AgentKind::Subagent,
    });
    app.ingest_engine_event(EngineEvent::AgentProgress {
        agent_id: "agent-1".into(),
        text: "reading providers".into(),
    });
    app.ingest_engine_event(EngineEvent::AgentStatusChanged {
        agent_id: "agent-1".into(),
        status: AgentStatus::Parked,
    });
    app.ingest_engine_event(EngineEvent::TurnFinished {
        turn_id,
        reason: StopReason::Stop,
    });

    let rendered = app.render().join("\n");
    assert!(rendered.contains("answer"), "{rendered}");
    assert!(rendered.contains("reasoning"), "{rendered}");
    assert!(rendered.contains("read · call-1 · running"), "{rendered}");
    assert_eq!(app.hub_peers().len(), 1);
    assert_eq!(
        app.hub_peers()[0].status,
        titi_tui::hub::AgentStatus::Parked
    );
}

/// A switch the user asked for names no turn and is not a fallback; only a
/// switch inside a turn is the engine giving up on the chosen model.
#[test]
fn only_a_switch_inside_a_turn_reads_as_a_fallback() {
    let mut app = app();
    assert!(app.details("activity expanded"));

    app.ingest_engine_event(EngineEvent::ModelSwitched {
        turn_id: None,
        from: "openai/gpt-4.1".into(),
        to: "anthropic/claude-opus-5".into(),
    });
    let rendered = app.render().join("\n");
    assert!(
        rendered.contains("model: openai/gpt-4.1 → anthropic/claude-opus-5"),
        "{rendered}"
    );
    assert!(!rendered.contains("fallback"), "{rendered}");

    app.ingest_engine_event(EngineEvent::ModelSwitched {
        turn_id: Some(TurnId(3)),
        from: "anthropic/claude-opus-5".into(),
        to: "openai/gpt-4.1".into(),
    });
    let rendered = app.render().join("\n");
    assert!(
        rendered.contains("model fallback: anthropic/claude-opus-5 → openai/gpt-4.1"),
        "{rendered}"
    );
}

#[test]
fn agents_slash_opens_live_hub() {
    let mut app = app();
    app.ingest_engine_event(EngineEvent::AgentStarted {
        agent_id: "agent-1".into(),
        name: "Worker".into(),
        parent_id: None,
        kind: AgentKind::Subagent,
    });
    let mut input = "/agents".to_owned();
    app.handle_canonical("enter", &mut input);
    let rendered = app.render().join("\n");
    assert!(rendered.contains("Agent Hub"), "{rendered}");
    assert!(rendered.contains("Worker"), "{rendered}");
}

#[test]
fn hub_r_and_x_emit_engine_commands() {
    let mut app = app();
    app.ingest_engine_event(EngineEvent::AgentStarted {
        agent_id: "agent-1".into(),
        name: "Worker".into(),
        parent_id: None,
        kind: AgentKind::Subagent,
    });
    app.ingest_engine_event(EngineEvent::AgentStatusChanged {
        agent_id: "agent-1".into(),
        status: AgentStatus::Parked,
    });
    let mut input = "/agents".to_owned();
    app.handle_canonical("enter", &mut input);
    assert_eq!(
        app.overlay_input("r"),
        Some(titi_cli::app::OverlayOutcome::HubRevive("agent-1".into()))
    );
    assert!(app.overlay_open());
    assert_eq!(
        app.overlay_input("x"),
        Some(titi_cli::app::OverlayOutcome::HubStop("agent-1".into()))
    );
}

#[test]
fn ctrl_c_stops_a_running_turn_and_asks_twice_when_idle() {
    use std::time::{Duration, Instant};
    use titi_cli::app::{Dispatch, EXIT_CONFIRM_WINDOW, EXIT_HINT};

    let mut input = String::new();
    let start = Instant::now();

    // Idle: one press only asks, so a stray Ctrl+C does not throw the
    // session away.
    let mut first = app();
    assert_eq!(
        first.handle_canonical_at("ctrl+c", &mut input, start),
        Dispatch::Handled(None)
    );
    assert!(first.exit_armed());
    assert!(first.render().join("\n").contains(EXIT_HINT));

    // A second press inside the window leaves.
    assert_eq!(
        first.handle_canonical_at("ctrl+c", &mut input, start + Duration::from_millis(300)),
        Dispatch::Exit
    );

    // Too slow: the arm expired and the next press asks again.
    let mut slow = app();
    slow.handle_canonical_at("ctrl+c", &mut input, start);
    assert_eq!(
        slow.handle_canonical_at(
            "ctrl+c",
            &mut input,
            start + EXIT_CONFIRM_WINDOW + Duration::from_millis(1)
        ),
        Dispatch::Handled(None)
    );

    // Any other key disarms it.
    let mut other = app();
    other.handle_canonical_at("ctrl+c", &mut input, start);
    other.handle_canonical_at("a", &mut input, start);
    assert!(!other.exit_armed());

    // While a turn runs, Ctrl+C cancels instead — it never exits.
    let mut busy = app();
    busy.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    assert_eq!(
        busy.handle_canonical_at("ctrl+c", &mut input, start),
        Dispatch::Cancel
    );
    assert!(!busy.exit_armed());
}

#[test]
fn pause_stops_the_agent_and_not_only_the_keyboard() {
    use titi_cli::app::SubmitEffect;

    let mut app = app();
    let mut input = "/pause".to_owned();
    assert_eq!(
        app.handle_canonical("enter", &mut input),
        titi_cli::app::Dispatch::Handled(Some(SubmitEffect::Pause)),
        "the modal alone would leave the agent streaming behind it"
    );
    assert!(app.is_paused());

    // Resuming closes the modal. Esc reaches the panel through the overlay
    // path; the binary's loop also short-circuits Esc/Enter/Space/Ctrl+C.
    assert_eq!(
        app.overlay_input("\x1b"),
        Some(titi_cli::app::OverlayOutcome::Dismissed)
    );
    assert!(!app.is_paused());
    assert!(!app.overlay_open());
}

#[test]
fn a_failed_turn_says_so_instead_of_nothing() {
    // The alert is the only channel a failure has. It used to paint only when
    // every transcript section was hidden, so with the defaults on a rejected
    // turn looked like nothing happening.
    let mut app = app();
    app.ingest_engine_event(EngineEvent::Failed {
        turn_id: Some(TurnId(1)),
        reason: ErrorReason::Rejected,
        message: "provider said no".into(),
    });
    let rendered = app.render().join("\n");
    assert!(
        rendered.contains("provider said no"),
        "a failure must reach the screen: {rendered}"
    );
}

#[test]
fn switching_sessions_drops_the_rendered_conversation() {
    let mut app = app();
    app.set_session_id("old");
    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    app.ingest_engine_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "old-session-marker".into(),
    });
    app.ingest_engine_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    assert!(app.render().join("\n").contains("old-session-marker"));

    app.switch_to_session("new");
    assert_eq!(app.session_id(), Some("new"));
    assert!(!app.turn_active());
    let rendered = app.render().join("\n");
    assert!(
        !rendered.contains("old-session-marker"),
        "the old conversation must not linger: {rendered}"
    );
    assert!(rendered.contains("session: new"), "{rendered}");
}

#[test]
fn typing_during_a_turn_steers_instead_of_queueing_a_new_turn() {
    use titi_cli::app::SubmitEffect;

    let mut app = app();
    assert!(!app.turn_active());

    // Idle: a submit is an ordinary prompt.
    let mut input = "hello".to_owned();
    let idle = app.handle_canonical("enter", &mut input);
    assert!(
        matches!(
            idle,
            titi_cli::app::Dispatch::Handled(Some(
                SubmitEffect::Queued(_) | SubmitEffect::Delivered(_)
            ))
        ),
        "{idle:?}"
    );

    // Running: the same submit becomes a steering message.
    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    assert!(app.turn_active());
    let mut input = "also check the tests".to_owned();
    let running = app.handle_canonical("enter", &mut input);
    assert_eq!(
        running,
        titi_cli::app::Dispatch::Handled(Some(SubmitEffect::Steer("also check the tests".into())))
    );

    // The turn ending returns to ordinary submission.
    app.ingest_engine_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    assert!(!app.turn_active());
}

#[test]
fn tool_approval_needed_opens_overlay_and_emits_command() {
    let mut app = app();
    app.ingest_engine_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
    });
    assert!(app.overlay_open());
    let rendered = app.render().join("\n");
    assert!(rendered.contains("Run tool bash?"), "{rendered}");
    assert_eq!(
        app.overlay_input("\r"),
        Some(titi_cli::app::OverlayOutcome::ToolApproval {
            call_id: "call-1".into(),
            approved: true,
        })
    );
    assert!(!app.overlay_open());
}

#[test]
fn tool_approval_esc_denies_without_session_close() {
    let mut app = app();
    app.ingest_engine_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-9".into(),
        name: "bash".into(),
    });
    assert_eq!(
        app.overlay_input("\x1b"),
        Some(titi_cli::app::OverlayOutcome::ToolApproval {
            call_id: "call-9".into(),
            approved: false,
        })
    );
    assert!(app.take_pending_close().is_none());
}
