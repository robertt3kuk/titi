//! Integration tests for slash commands in titi-cli: builtin route
//! dispatch (reserved names), `/unknown-xyz` passthrough to the LLM, and
//! the floating autocomplete panel (Tab inserts, Esc hides, arrows move).
//!
//! Contract: `docs/research/agent-ux/README.md` (Slash DoD).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::{App, default_theme};
use titi_tui::slash::Route;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Route dispatch (reserved names → Builtin, unknown → Passthrough)
// ---------------------------------------------------------------------------

#[test]
fn reserved_builtins_route_as_builtin() {
    let app = app();
    assert_eq!(app.route_slash("/model"), Route::Builtin("model".into()));
    assert_eq!(
        app.route_slash("/details"),
        Route::Builtin("details".into())
    );
    assert_eq!(
        app.route_slash("/sessions"),
        Route::Builtin("sessions".into())
    );
    assert_eq!(app.route_slash("/help"), Route::Builtin("help".into()));
    assert_eq!(app.route_slash("/mouse"), Route::Builtin("mouse".into()));
}

#[test]
fn goal_is_reserved() {
    let app = app();
    assert_eq!(
        app.route_slash("/goal fix the parser"),
        Route::Builtin("goal".into())
    );
    assert_eq!(app.route_slash("/goal"), Route::Builtin("goal".into()));
}

#[test]
fn builtin_with_arguments_still_routes_as_builtin() {
    let app = app();
    // The command name (up to the first space) decides the route; the
    // argument list is the builtin's own concern.
    assert_eq!(
        app.route_slash("/details thinking"),
        Route::Builtin("details".into())
    );
    assert_eq!(
        app.route_slash("/mouse buttons"),
        Route::Builtin("mouse".into())
    );
}

#[test]
fn unknown_slash_passes_through_to_llm() {
    let app = app();
    assert_eq!(app.route_slash("/unknown-xyz"), Route::Passthrough);
    assert_eq!(app.route_slash("/not-a-command arg"), Route::Passthrough);
}

#[test]
fn plain_prompt_passes_through() {
    let app = app();
    assert_eq!(app.route_slash("hello there"), Route::Passthrough);
    assert_eq!(app.route_slash(""), Route::Passthrough);
}

// ---------------------------------------------------------------------------
// Floating autocomplete panel
// ---------------------------------------------------------------------------

#[test]
fn typing_slash_shows_completions() {
    let mut app = app();
    assert!(!app.completion_visible(), "no input yet");

    app.slash_completions("/m");
    assert!(app.completion_visible(), "suggestions for /m");
    assert_eq!(app.completion_accept(), Some("/model".into()));

    app.slash_completions("/s");
    assert!(app.completion_visible(), "suggestions for /s");
    assert_eq!(app.completion_accept(), Some("/sessions".into()));
}

#[test]
fn typing_arguments_hides_completions() {
    let mut app = app();
    app.slash_completions("/m");
    assert!(app.completion_visible());

    app.slash_completions("/mouse buttons");
    assert!(
        !app.completion_visible(),
        "arguments typed → prefix no longer bare"
    );
}

#[test]
fn plain_text_never_shows_completions() {
    let mut app = app();
    app.slash_completions("hello");
    assert!(!app.completion_visible());
}

#[test]
fn tab_accepts_highlighted_command() {
    let mut app = app();
    app.slash_completions("/m");
    assert_eq!(app.completion_accept(), Some("/model".into()));
    // Accepting hides the panel for the next keystroke.
    assert!(!app.completion_visible());
}

#[test]
fn arrows_move_highlight_and_accept_follows() {
    let mut app = app();
    app.slash_completions("/m");
    // First completion is `/model`; move down then wrap-check.
    app.completion_move(false); // Down → `/mouse`
    assert_eq!(app.completion_accept(), Some("/mouse".into()));
}

#[test]
fn esc_hides_without_touching_input() {
    let mut app = app();
    app.slash_completions("/m");
    assert!(app.completion_visible());
    app.completion_hide();
    assert!(!app.completion_visible());
    assert_eq!(
        app.completion_accept(),
        None,
        "accept after hide is a no-op"
    );
}

#[test]
fn completion_panel_renders_in_frame() {
    let mut app = app();
    app.slash_completions("/m");

    let frame = app.render();
    let joined = frame.join("\n");
    assert!(
        joined.contains("/model"),
        "completion panel renders the suggested command"
    );
    assert!(
        joined.contains("Switch the active model"),
        "completion panel shows the command description"
    );

    // Hiding removes it from the frame.
    app.completion_hide();
    let frame = app.render().join("\n");
    assert!(!frame.contains("Switch the active model"));
}
