//! OMP `app.*` action dispatch (followUp, dequeue, model.select, session.switch).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::{App, Dispatch, default_theme};
use titi_tui::composer::QueueMode;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

#[test]
fn follow_up_queues_without_submitting() {
    let mut app = app();
    let mut input = "later".to_owned();
    assert_eq!(
        app.handle_canonical("ctrl+q", &mut input),
        Dispatch::Handled(None)
    );
    assert!(input.is_empty());
    assert_eq!(app.stream_queue_len(), 1);
}

#[test]
fn dequeue_pulls_last_follow_up() {
    let mut app = app();
    app.push_queued("one", QueueMode::FollowUp);
    app.push_queued("two", QueueMode::FollowUp);
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("alt+up", &mut input),
        Dispatch::Handled(None)
    );
    assert_eq!(input, "two");
    assert_eq!(app.stream_queue_len(), 1);
    assert!(app.queue_highlighted());
}

#[test]
fn alt_m_opens_model_picker() {
    let mut app = app();
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("alt+m", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.overlay_open());
}

#[test]
fn ctrl_x_opens_session_switcher() {
    let mut app = app();
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("ctrl+x", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.overlay_open());
}

#[test]
fn pause_and_hotkeys_are_builtins() {
    let mut app = app();
    let mut input = "/pause".to_owned();
    let out = app.handle_canonical("enter", &mut input);
    assert!(matches!(out, Dispatch::Handled(_)));
    assert!(app.is_paused());
    app.close_overlay();

    input = "/hotkeys".to_owned();
    let out = app.handle_canonical("enter", &mut input);
    assert!(matches!(out, Dispatch::Handled(_)));
    assert!(app.overlay_open());
}

#[test]
fn default_manager_binds_follow_up() {
    let app = app();
    assert!(
        app.keys()
            .matches_canonical("ctrl+q", "app.message.followUp")
    );
    assert!(app.keys().matches_canonical("alt+m", "app.model.select"));
    assert!(app.keys().matches_canonical("ctrl+x", "app.session.switch"));
}

#[test]
fn frame_has_one_box_composer_not_double_status() {
    let mut app = app();
    let rows = app.render();
    let joined = rows.join("\n");
    let bottoms = joined.matches('╰').count();
    assert!(bottoms >= 1, "boxRound bottom present: {joined}");
    // Hermes agent-state labels are not the product bar.
    let status_hits = rows
        .iter()
        .filter(|r| r.contains("starting") && !r.contains("titi"))
        .count();
    assert_eq!(status_hits, 0, "no Hermes starting bar: {joined}");
}

#[test]
fn plan_toggle_sets_mode_badge() {
    let mut app = app();
    let mut input = String::new();
    assert!(!app.plan_mode());
    assert_eq!(
        app.handle_canonical("shift+alt+p", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.plan_mode());
    let joined = app.render().join("\n");
    assert!(joined.contains("plan"), "plan badge in status: {joined}");
}

#[test]
fn live_toggle_and_hub_overlay() {
    let mut app = app();
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("ctrl+l", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.live_mode());
    assert_eq!(
        app.handle_canonical("alt+a", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.overlay_open());
}

#[test]
fn history_search_picks_prompt() {
    let mut app = app();
    let mut input = "hello world".to_owned();
    let out = app.handle_canonical("enter", &mut input);
    assert!(matches!(out, Dispatch::Handled(_)));
    assert!(input.is_empty());
    assert_eq!(
        app.handle_canonical("ctrl+r", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.overlay_open());
    // Confirm the only history item.
    let outcome = app.overlay_input("\r");
    assert_eq!(
        outcome,
        Some(titi_cli::app::OverlayOutcome::HistoryPicked(
            "hello world".into()
        ))
    );
}

#[test]
fn retry_resubmits_last_prompt() {
    let mut app = app();
    let mut input = "again".to_owned();
    let _ = app.handle_canonical("enter", &mut input);
    let out = app.handle_canonical("alt+r", &mut input);
    assert!(matches!(out, Dispatch::Handled(_)));
}

#[test]
fn display_reset_requests_replay() {
    let mut app = app();
    let mut input = String::new();
    let out = app.handle_canonical("alt+l", &mut input);
    assert_eq!(
        out,
        Dispatch::Handled(Some(titi_cli::app::SubmitEffect::DisplayReset))
    );
}

#[test]
fn copy_line_emits_copy_effect() {
    let mut app = app();
    let mut input = "draft".to_owned();
    let out = app.handle_canonical("shift+alt+l", &mut input);
    assert_eq!(
        out,
        Dispatch::Handled(Some(titi_cli::app::SubmitEffect::Copy("draft".into())))
    );
}

#[test]
fn plan_frame_offers_history_when_transcript_overflows() {
    let mut app = app();
    for i in 0..40 {
        app.push_transcript(
            titi_tui::markdown::Section::Thinking,
            format!("line {i} of overflow padding for history batch"),
        );
    }
    let plan = app.plan_frame("", 12);
    assert!(
        plan.history.is_some(),
        "overflow must offer a history batch"
    );
    let id = plan.history.as_ref().unwrap().id;
    app.acknowledge_history(id);
    let plan2 = app.plan_frame("", 12);
    if let Some(batch) = plan2.history {
        assert_ne!(batch.id, id, "acked id must not be re-offered");
    }
}

#[test]
fn cycle_forward_advances_model() {
    let mut app = app();
    let mut input = String::new();
    let before = app.render().join("\n");
    assert_eq!(
        app.handle_canonical("ctrl+p", &mut input),
        Dispatch::Handled(None)
    );
    let after = app.render().join("\n");
    assert_ne!(
        before, after,
        "cycleForward should change the model segment"
    );
}

#[test]
fn alt_p_opens_temporary_model_picker() {
    let mut app = app();
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("alt+p", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.overlay_open());
}

#[test]
fn stt_toggle_action_id_is_not_a_key() {
    let mut app = app();
    let mut input = String::new();
    // defaultKeys is empty: the action id itself is not a chord.
    assert_eq!(
        app.handle_canonical("app.stt.toggle", &mut input),
        Dispatch::Unhandled
    );
    assert_eq!(app.stt_state(), titi_cli::app::SttState::Idle);
}

#[test]
fn default_stt_binding_has_no_keys() {
    let app = app();
    assert!(!app.keys().matches_canonical("space", "app.stt.toggle"));
}

#[test]
fn stt_toggle_records_when_enabled() {
    let mut app = app();
    app.set_stt_enabled(true);
    assert_eq!(app.stt_state(), titi_cli::app::SttState::Idle);
    // Drive four 40ms spaces: two inserts, one swallow, then start.
    use std::time::{Duration, Instant};
    let mut now = Instant::now();
    let mut input = "hello".to_owned();
    assert_eq!(
        app.handle_canonical_at("space", &mut input, now),
        Dispatch::Handled(None)
    );
    assert_eq!(input, "hello ");
    now += Duration::from_millis(40);
    assert_eq!(
        app.handle_canonical_at("space", &mut input, now),
        Dispatch::Handled(None)
    );
    assert_eq!(input, "hello  ");
    now += Duration::from_millis(40);
    assert_eq!(
        app.handle_canonical_at("space", &mut input, now),
        Dispatch::Handled(None)
    );
    assert_eq!(input, "hello  ");
    now += Duration::from_millis(40);
    assert_eq!(
        app.handle_canonical_at("space", &mut input, now),
        Dispatch::Handled(None)
    );
    assert_eq!(input, "hello");
    assert_eq!(app.stt_state(), titi_cli::app::SttState::Recording);
    now += Duration::from_millis(250);
    assert!(app.poll_space_hold(now));
    assert_eq!(app.stt_state(), titi_cli::app::SttState::Idle);
}

#[test]
fn stt_blocked_during_live_mode() {
    let mut app = app();
    app.set_stt_enabled(true);
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("ctrl+l", &mut input),
        Dispatch::Handled(None)
    );
    use std::time::{Duration, Instant};
    let mut now = Instant::now();
    for _ in 0..4 {
        let _ = app.handle_canonical_at("space", &mut input, now);
        now += Duration::from_millis(40);
    }
    assert_eq!(app.stt_state(), titi_cli::app::SttState::Idle);
}

#[test]
fn space_types_normally_when_stt_disabled() {
    let mut app = app();
    let mut input = "hi".to_owned();
    use std::time::Instant;
    let now = Instant::now();
    assert_eq!(
        app.handle_canonical_at("space", &mut input, now),
        Dispatch::Handled(None)
    );
    assert_eq!(input, "hi ");
    assert_eq!(app.stt_state(), titi_cli::app::SttState::Idle);
}

#[test]
fn hub_empty_state_copy() {
    let mut app = app();
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("alt+a", &mut input),
        Dispatch::Handled(None)
    );
    assert!(app.overlay_open());
    let joined = app.render().join("\n");
    assert!(
        joined.contains("(no connected agents)") == false,
        "old stub chrome leaked: {joined}"
    );
    assert!(
        joined.contains("j/k:select") || joined.contains("No agents in this session"),
        "{joined}"
    );
    assert!(
        joined.contains("omp-dev --continue") || joined.contains("Agent Hub"),
        "{joined}"
    );
    assert_eq!(
        app.overlay_input("\x1b"),
        Some(titi_cli::app::OverlayOutcome::Dismissed)
    );
    assert!(!app.overlay_open());
}

#[test]
fn observe_opens_hub_and_filters_main() {
    use titi_tui::hub::{AgentKind, AgentStatus, HubPeer, MAIN_AGENT_ID};
    let mut app = app();
    app.set_hub_peers(vec![
        HubPeer {
            id: MAIN_AGENT_ID.to_owned(),
            display_name: "Main".into(),
            kind: AgentKind::Main,
            parent_id: None,
            status: AgentStatus::Idle,
        },
        HubPeer::sub("Worker", AgentStatus::Running),
    ]);
    let mut input = String::new();
    assert_eq!(
        app.handle_canonical("ctrl+s", &mut input),
        Dispatch::Handled(None)
    );
    let joined = app.render().join("\n");
    assert!(joined.contains("Worker"), "{joined}");
    assert!(!joined.contains(" Main"), "{joined}");
    assert_eq!(
        app.overlay_input("\r"),
        Some(titi_cli::app::OverlayOutcome::HubSelected("Worker".into()))
    );
}

/// Auto-theme is decided by a process-global registry, and these tests share
/// one process, so a sibling that already moved it would make the first probe
/// a no-op. Anchoring on dark first makes the transition the assertion, not
/// the state the test happened to start in.
#[test]
fn osc11_probe_reply_feeds_auto_theme() {
    use titi_cli::app::AppearanceIngest;
    let mut app = app();
    let dark = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
    let light = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
    let _ = app.ingest_probe_reply(dark);
    assert_eq!(
        app.ingest_probe_reply(light),
        AppearanceIngest::ThemeChanged
    );
    assert_eq!(app.ingest_probe_reply(light), AppearanceIngest::Unchanged);
    assert_eq!(
        app.ingest_probe_reply(b"\x1b[?997;1n"),
        AppearanceIngest::NeedOsc11Query
    );
    assert_eq!(app.ingest_probe_reply(dark), AppearanceIngest::ThemeChanged);
}

#[test]
fn apply_bg_rgb_classifies_luma() {
    use titi_cli::app::AppearanceIngest;
    use titi_tui::caps::Rgb;
    let mut app = app();
    let _ = app.apply_bg_rgb(Rgb { r: 0, g: 0, b: 0 });
    assert_eq!(
        app.apply_bg_rgb(Rgb { r: 0, g: 0, b: 0 }),
        AppearanceIngest::Unchanged
    );
    assert_eq!(
        app.apply_bg_rgb(Rgb {
            r: 255,
            g: 255,
            b: 255
        }),
        AppearanceIngest::ThemeChanged
    );
}
