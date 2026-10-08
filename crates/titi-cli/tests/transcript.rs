//! Integration test for transcript accordion integration in titi-cli.
//!
//! Contract: `docs/research/agent-ux/README.md` (DoD transcript item):
//! thinking/tools expanded, subagents collapsed, activity hidden by default;
//! `/details <section> <mode>` switches visibility; floating-alert backstop
//! when all sections are hidden.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::App;
use titi_cli::themes::default_theme;
use titi_tui::markdown::Section;
use titi_tui::theme::Theme;

fn test_theme() -> Arc<Theme> {
    default_theme().unwrap()
}

#[test]
fn transcript_defaults_render_accordion() {
    let mut app = App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        test_theme(),
    );
    app.push_transcript(Section::Thinking, "reasoning step");
    app.push_transcript(Section::Tools, "tool ran");
    app.push_transcript(Section::Subagents, "agent report");
    app.push_transcript(Section::Activity, "ambient");

    let rows = app.render();

    // thinking + tools expanded → header + body; subagents collapsed →
    // header only; activity hidden → nothing.
    let text = rows.join("\n");
    assert!(text.contains("▾ thinking"), "thinking header: {text}");
    assert!(text.contains("reasoning step"), "thinking body: {text}");
    assert!(text.contains("▾ tools"), "tools header: {text}");
    assert!(text.contains("tool ran"), "tools body: {text}");
    assert!(text.contains("▸ subagents"), "subagents collapsed: {text}");
    assert!(
        !text.contains("agent report"),
        "subagents body hidden: {text}"
    );
    assert!(!text.contains("ambient"), "activity hidden: {text}");
    assert!(!text.contains("activity"), "no activity header: {text}");
}

#[test]
fn details_directive_expands_collapsed_section() {
    let mut app = App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        test_theme(),
    );
    app.push_transcript(Section::Subagents, "agent report");

    // Default: collapsed → body not rendered.
    let rows = app.render();
    let text = rows.join("\n");
    assert!(!text.contains("agent report"), "default collapsed: {text}");

    // /details subagents expanded → body renders.
    assert!(app.details("subagents expanded"));
    let rows = app.render();
    let text = rows.join("\n");
    assert!(text.contains("agent report"), "after expand: {text}");
    assert!(text.contains("▾ subagents"), "chevron updated: {text}");
}

#[test]
fn all_hidden_shows_floating_alert_backstop() {
    let mut app = App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        test_theme(),
    );
    app.push_transcript(Section::Thinking, "hidden thought");

    // Hide everything; activity is hidden by default.
    assert!(app.details("thinking hidden"));
    assert!(app.details("tools hidden"));
    assert!(app.details("subagents hidden"));
    assert!(app.all_hidden());

    // Backstop: the transcript renders the alert instead of nothing.
    let rows = app.render();
    let text = rows.join("\n");
    assert!(
        text.contains("all sections hidden"),
        "alert backstop: {text}"
    );
    assert!(
        !text.contains("hidden thought"),
        "content suppressed: {text}"
    );
}

#[test]
fn mouse_drag_select_applies_background() {
    let mut app = App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        test_theme(),
    );
    app.push_transcript(Section::Thinking, "line one");
    app.push_transcript(Section::Thinking, "line two");
    app.push_transcript(Section::Thinking, "line three");

    // Rows: 0 banner, 1 blank, 2 thinking header, 3 line one,
    //       4 line two, 5 line three, 6 tools header, 7 subagents header,
    //       8 status.
    let before = app.render();

    // Press at (0, 3) and drag to (8, 5): selects rows 3..=5.
    app.mouse_press(0, 3);
    app.mouse_drag(8, 5);
    let after = app.render();
    for row in &after[3..=5] {
        assert!(
            row.contains("\x1b[48;2;") || row.contains("\x1b[48;5;"),
            "selected row painted with a background: {row:?}"
        );
    }
    // Row 2 (header) outside the selection region untouched.
    assert_eq!(after[2], before[2], "header unchanged");

    // Release commits the selection.
    app.mouse_release();
    assert_eq!(app.selection().unwrap().rect(), Some((0, 3, 8, 5)));

    // Mouse moved elsewhere clears the selection.
    app.mouse_drag(4, 4);
    assert_ne!(app.selection().unwrap().rect(), Some((0, 3, 8, 5)));
}

#[test]
fn mouse_preset_roundtrips_through_config() {
    use titi_cli::session_fs::{load_mouse_preset_from, save_mouse_preset_to};
    use titi_tui::caps::MousePreset;

    // Use a temp dir as the agent directory; titi-config creates the yml
    // file on first set().
    let tmp = std::env::temp_dir().join(format!("titi-mouse-config-{}", std::process::id()));
    let agent_dir = tmp.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    // Default: nothing persisted.
    assert_eq!(load_mouse_preset_from(&agent_dir), None);
    // Save buttons → load buttons.
    save_mouse_preset_to(&agent_dir, MousePreset::Buttons).unwrap();
    assert_eq!(
        load_mouse_preset_from(&agent_dir),
        Some(MousePreset::Buttons)
    );
    // Save all → load all.
    save_mouse_preset_to(&agent_dir, MousePreset::All).unwrap();
    assert_eq!(load_mouse_preset_from(&agent_dir), Some(MousePreset::All));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn mouse_press_without_drag_paints_nothing() {
    let mut app = App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        test_theme(),
    );
    app.push_transcript(Section::Thinking, "content");
    let before = app.render();
    app.mouse_press(2, 3);
    app.mouse_release();
    // Anchor == current → empty selection → no background.
    let after = app.render();
    assert_eq!(before, after);
}

#[test]
fn first_frame_still_queues_input_with_transcript() {
    let mut app = App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi v0.1.0".to_owned(), String::new()],
        test_theme(),
    );
    let rows = app.render();
    assert!(rows[0].contains("titi v0.1.0"), "banner: {rows:?}");
    assert!(
        rows.iter().any(|r| r.contains('╭')) && rows.iter().any(|r| r.contains('╰')),
        "OMP box composer: {rows:?}"
    );
    assert_eq!(app.state(), titi_tui::status::AgentState::Starting);
    assert!(app.time_to_first_frame().as_millis() < 150, "ttff too slow");

    use titi_cli::first_frame::SubmitOutcome;
    assert_eq!(
        app.submit("queued prompt".to_owned()),
        SubmitOutcome::Queued
    );
    assert_eq!(app.queue_len(), 1);
}
