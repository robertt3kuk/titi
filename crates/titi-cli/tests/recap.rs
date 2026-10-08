//! The session recap: what the sections report, and how the panel behaves in
//! the app.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;
use titi_cli::app::App;
use titi_cli::recap::build;
use titi_cli::themes::default_theme;
use titi_core::session::{Role, SessionMeta, SessionStore};
use titi_core::trajectory::{EventKind, TrajectoryRecorder};
use titi_tui::recap::RecapSection;
use titi_tui::slash::Route;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

/// A session with two turns, one successful read and one failed bash.
fn fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();
    store
        .append(&session_id, Role::User, "fix the parser\nsecond line")
        .unwrap();
    store
        .append(&session_id, Role::Assistant, "the parser is fixed now")
        .unwrap();
    store
        .append(&session_id, Role::User, "run the tests")
        .unwrap();
    store
        .append(&session_id, Role::Assistant, "tests pass")
        .unwrap();

    let mut recorder = TrajectoryRecorder::open(agent_dir, &session_id).unwrap();
    recorder
        .record(EventKind::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: json!({"path": "src/parse.rs"}),
        })
        .unwrap();
    recorder
        .record(EventKind::ToolResult {
            id: "c1".into(),
            duration_ms: 12,
            ok: true,
        })
        .unwrap();
    recorder
        .record(EventKind::ToolCall {
            id: "c2".into(),
            name: "read".into(),
            args: json!({"path": "src/parse.rs"}),
        })
        .unwrap();
    recorder
        .record(EventKind::ToolResult {
            id: "c2".into(),
            duration_ms: 8,
            ok: true,
        })
        .unwrap();
    recorder
        .record(EventKind::ToolCall {
            id: "c3".into(),
            name: "bash".into(),
            args: json!({"command": "cargo test"}),
        })
        .unwrap();
    recorder
        .record(EventKind::ToolResult {
            id: "c3".into(),
            duration_ms: 2400,
            ok: false,
        })
        .unwrap();
    recorder.record(EventKind::TurnEnd).unwrap();
    recorder.flush().unwrap();

    (dir, session_id)
}

fn section<'a>(sections: &'a [RecapSection], title: &str) -> &'a RecapSection {
    sections
        .iter()
        .find(|section| section.title == title)
        .unwrap_or_else(|| panic!("no {title} section in {sections:?}"))
}

#[test]
fn the_recap_reports_the_session_turns_tools_files_and_problems() {
    let (dir, session_id) = fixture();
    let sections = build(dir.path(), &session_id).unwrap();

    let session = section(&sections, "Session");
    assert!(session.summary.contains("4 entries"), "{session:?}");
    assert!(
        session
            .lines
            .iter()
            .any(|line| line.contains("2 user, 2 assistant")),
        "{session:?}"
    );

    let turns = section(&sections, "Turns");
    assert_eq!(turns.summary, "2 prompts");
    assert!(
        turns.lines.iter().any(|line| line == "· fix the parser"),
        "the first line of each prompt, not the whole thing: {turns:?}"
    );

    let tools = section(&sections, "Tools");
    assert_eq!(tools.summary, "3 calls");
    assert!(
        tools
            .lines
            .iter()
            .any(|line| line.starts_with("read: 2 call(s), 0 failed, 20ms total, 10ms avg")),
        "{tools:?}"
    );
    assert!(
        tools
            .lines
            .iter()
            .any(|line| line.starts_with("bash: 1 call(s), 1 failed, 2.4s total")),
        "{tools:?}"
    );

    let files = section(&sections, "Files");
    assert_eq!(files.summary, "1 touched");
    assert_eq!(files.lines, vec!["src/parse.rs ×2".to_owned()]);

    let problems = section(&sections, "Problems");
    assert_eq!(problems.summary, "1 failed tool call(s)");
    assert_eq!(problems.lines, vec!["bash failed after 2.4s".to_owned()]);

    let trajectory = section(&sections, "Trajectory");
    assert!(trajectory.summary.contains("7 events"), "{trajectory:?}");
}

#[test]
fn an_untouched_session_says_so_instead_of_listing_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();

    let sections = build(dir.path(), &session_id).unwrap();
    assert_eq!(section(&sections, "Tools").summary, "none called");
    assert!(section(&sections, "Tools").lines.is_empty());
    assert_eq!(section(&sections, "Files").summary, "none touched");
    assert_eq!(section(&sections, "Problems").summary, "none");
    assert_eq!(section(&sections, "Trajectory").summary, "nothing recorded");
    assert_eq!(section(&sections, "Turns").summary, "0 prompts");
}

#[test]
fn the_recap_builtin_is_reserved_and_opens_the_panel() {
    let app_instance = app();
    assert_eq!(
        app_instance.route_slash("/recap"),
        Route::Builtin("recap".to_owned())
    );
}

#[test]
fn opening_the_recap_without_a_session_reports_it() {
    let mut app = app();
    let reason = app.open_session_recap().unwrap_err();
    assert!(reason.contains("no live session"), "{reason}");
    assert!(!app.overlay_open());
}

#[test]
fn the_panel_takes_keys_until_escape_and_ctrl_o_opens_everything() {
    let mut app = app();
    app.open_recap(vec![
        RecapSection::new("Session", "4 entries", vec!["id: abc".into()]),
        RecapSection::new("Tools", "3 calls", vec!["read ×2".into()]),
    ]);
    assert!(app.overlay_open());

    // Ctrl+O inside the panel opens every section at once.
    assert_eq!(app.overlay_input("\x0f"), None, "the panel stays open");
    // `App::render` does not pad to the viewport, so the composited frame has
    // room for only the top of a tall panel — the panel's own title row is
    // asserted in `titi-tui`'s recap tests. What matters here is that Ctrl+O
    // reached the panel and opened both bodies.
    let rendered = app.render().join("\n");
    assert!(rendered.contains("read ×2"), "{rendered}");
    assert!(rendered.contains("id: abc"), "{rendered}");

    // Escape closes it, and the outcome is a plain dismissal.
    assert_eq!(
        app.overlay_input("\x1b"),
        Some(titi_cli::app::OverlayOutcome::Dismissed)
    );
    assert!(!app.overlay_open());
}

#[test]
fn ctrl_o_expands_then_collapses_every_transcript_section() {
    let mut app = app();
    let mut input = String::new();

    // Defaults leave subagents collapsed and activity hidden, so the first
    // press opens everything.
    assert_eq!(
        app.handle_canonical("ctrl+o", &mut input),
        titi_cli::app::Dispatch::Handled(None)
    );
    for name in ["thinking", "tools", "subagents", "activity"] {
        assert!(
            app.render().join("\n").contains(name),
            "{name} should be visible after Ctrl+O"
        );
    }

    // A second press closes them all. (It also clears the alert, which is why
    // the assertion below is on the frame rather than on a message.)
    app.handle_canonical("ctrl+o", &mut input);
    let rendered = app.render().join("\n");
    assert!(
        !rendered.contains("▾"),
        "every section collapsed: {rendered}"
    );
    assert!(rendered.contains("▸ tools"), "{rendered}");
}
