//! The Shift+Tab mode cycle and the composer's badge, tested through the chat.

use super::*;
use crate::mode_cycle::next_mode_command;
use std::path::Path;

fn theme() -> Arc<Theme> {
    let options = titi_tui::theme::loader::CreateThemeOptions {
        mode: Some(titi_tui::theme::ColorMode::Truecolor),
        ..Default::default()
    };
    match titi_tui::theme::loader::load_theme("titanium", &options) {
        Ok(theme) => Arc::new(theme),
        Err(reason) => panic!("built-in theme: {reason}"),
    }
}

fn chat_in(agent: &Path) -> Chat {
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    chat.set_agent_dir(agent);
    chat
}

fn frame_rows(chat: &mut Chat) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal.draw(|f| draw(f, chat)).expect("draw");
    let buffer = terminal.backend().buffer();
    (0..24)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect()
}

fn set_mode(chat: &mut Chat, mode: SessionMode) {
    chat.on_event(EngineEvent::ModeChanged { mode });
}

#[test]
fn shift_tab_walks_agent_plan_duck_and_back() {
    assert_eq!(next_mode_command(SessionMode::Agent), "/plan");
    assert_eq!(next_mode_command(SessionMode::Plan), "/duck");
    assert_eq!(next_mode_command(SessionMode::Duck), "/done");

    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    for expected in [
        SessionMode::Plan,
        SessionMode::Duck,
        SessionMode::Agent,
        SessionMode::Plan,
    ] {
        let applied = chat.on_key(Key::ShiftTab, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SetMode { mode: expected })),
            "the step is the engine's own SetMode"
        );
        // The screen's mode is the engine's answer, not the keystroke.
        assert_ne!(chat.mode, expected, "the badge waits for the engine");
        set_mode(&mut chat, expected);
        assert_eq!(chat.mode, expected);
    }
}

#[test]
fn the_composer_shows_a_badge_for_plan_and_duck_only() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    let agent = frame_rows(&mut chat).join("\n");
    assert!(
        !agent.contains("─ agent ─"),
        "agent mode is the plain composer"
    );
    assert!(!agent.contains("─ plan ─") && !agent.contains("─ duck ─"));

    set_mode(&mut chat, SessionMode::Plan);
    let rows = frame_rows(&mut chat);
    let top = rows
        .iter()
        .find(|row| row.starts_with("╭─ plan ─"))
        .unwrap_or_else(|| panic!("no plan badge on a composer's top rule: {rows:#?}"));
    assert!(top.ends_with('╮'), "{top}");

    set_mode(&mut chat, SessionMode::Duck);
    let rows = frame_rows(&mut chat);
    assert!(
        rows.iter().any(|row| row.starts_with("╭─ duck ─")),
        "{rows:#?}"
    );

    set_mode(&mut chat, SessionMode::Agent);
    let rows = frame_rows(&mut chat);
    assert!(
        !rows.iter().any(|row| row.starts_with("╭─ duck ─")),
        "{rows:#?}"
    );
}
