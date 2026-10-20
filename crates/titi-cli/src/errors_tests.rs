//! Tests for the error log, its `/errors` panel and the status line's count.
//!
//! They drive the screen through keys and engine events, so each reads as a
//! thing a person does.

use std::sync::Arc;
use std::time::Instant;

use titi_engine::{EngineEvent, TurnId};
use titi_tui::theme::Theme;

use crate::chat::{Chat, Key, masthead_snapshot};
use crate::transcript::LineKind;

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

/// A chat on a fresh agent directory, so no command reaches the real one.
fn chat() -> (tempfile::TempDir, Chat) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme());
    chat.set_agent_dir(dir.path());
    (dir, chat)
}

fn type_command(chat: &mut Chat, text: &str) {
    for ch in text.chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
    chat.on_key(Key::Enter, Instant::now());
}

use crate::errors_panel::{ERROR_LOG_CAP, ErrorLog, ErrorSource};
use crate::pickers::panel_view_for;

// ---- The error log -----------------------------------------------------

#[test]
fn the_log_keeps_the_newest_two_hundred_newest_first() {
    let mut log = ErrorLog::default();
    for n in 0..ERROR_LOG_CAP + 5 {
        log.record(ErrorSource::Screen, format!("error {n}"), None);
    }
    assert_eq!(log.len(), ERROR_LOG_CAP);
    let listed: Vec<&str> = log
        .newest_first()
        .map(|entry| entry.text.as_str())
        .collect();
    assert_eq!(listed.first(), Some(&"error 204"), "newest first");
    assert_eq!(listed.last(), Some(&"error 5"), "the five oldest went");
    assert_eq!(log.unseen(), ERROR_LOG_CAP + 5, "every one was news");
}

#[test]
fn every_kind_of_error_reaches_the_log_with_its_source() {
    let (_dir, mut chat) = chat();
    chat.on_event(EngineEvent::Failed {
        turn_id: Some(TurnId(1)),
        reason: titi_providers::ErrorReason::Rejected,
        message: "provider said 500: ".repeat(20).into(),
    });
    chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(1),
        call_id: "ghost".into(),
        output: "no such file".into(),
        is_error: true,
        detail: None,
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "read".into(),
        detail: Some("read nope.txt".into()),
    });
    chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        output: "ENOENT: nope.txt".into(),
        is_error: true,
        detail: None,
    });
    type_command(&mut chat, "/no-such-command");

    let sources: Vec<ErrorSource> = chat
        .errors
        .log
        .newest_first()
        .map(|entry| entry.source)
        .collect();
    assert_eq!(
        sources,
        [
            ErrorSource::Screen,
            ErrorSource::Tool,
            ErrorSource::Tool,
            ErrorSource::Provider
        ]
    );
    let provider = chat.errors.log.newest_first().last().expect("provider");
    assert!(
        provider.text.len() > 200,
        "the log keeps what the row cut: {}",
        provider.text.len()
    );
    let tool_row = chat.errors.log.newest_first().nth(1).expect("tool row");
    assert!(tool_row.text.contains("read  nope.txt"), "{tool_row:?}");
    assert!(tool_row.text.contains("ENOENT: nope.txt"), "{tool_row:?}");
}

#[test]
fn a_note_is_not_an_error() {
    let (_dir, mut chat) = chat();
    chat.push(LineKind::Note, "just so you know".to_owned());
    assert!(chat.errors.log.is_empty());
    assert_eq!(masthead_snapshot(&chat).errors, 0);
}

#[test]
fn the_status_count_resets_when_the_panel_opens_and_counts_again() {
    let (_dir, mut chat) = chat();
    assert_eq!(masthead_snapshot(&chat).errors, 0, "hidden while clean");
    chat.push(LineKind::Error, "one".to_owned());
    chat.push(LineKind::Error, "two".to_owned());
    chat.push(LineKind::Error, "three".to_owned());
    assert_eq!(masthead_snapshot(&chat).errors, 3);

    type_command(&mut chat, "/errors");
    assert!(chat.errors.picker.is_some());
    assert_eq!(masthead_snapshot(&chat).errors, 0, "seen");
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(
        masthead_snapshot(&chat).errors,
        0,
        "closing changes nothing"
    );

    chat.push(LineKind::Error, "four".to_owned());
    assert_eq!(masthead_snapshot(&chat).errors, 1, "only what is new");
    assert_eq!(chat.errors.log.len(), 4, "the log keeps all of them");
}

#[test]
fn the_panel_lists_newest_first_and_enter_shows_the_whole_text() {
    let (_dir, mut chat) = chat();
    chat.push(LineKind::Error, "first failure".to_owned());
    let long = format!("provider said no: {}", "very long reason ".repeat(30));
    chat.push_error(
        ErrorSource::Provider,
        "provider said no: very…".to_owned(),
        &long,
    );

    type_command(&mut chat, "/errors");
    let view = panel_view_for(&chat, 30, 80).expect("the panel is up");
    assert_eq!(view.title.as_deref(), Some("errors · 2"));
    let rows: Vec<String> = view
        .lines
        .iter()
        .map(|line| match line {
            crate::pickers::PanelLine::Row { text, .. } => text.clone(),
            crate::pickers::PanelLine::Heading(text) => text.clone(),
        })
        .collect();
    assert!(rows[0].contains("provider"), "newest first: {rows:?}");
    assert!(rows[0].contains("provider said no"), "{rows:?}");
    assert!(
        rows[1].contains("titi") && rows[1].contains("first failure"),
        "{rows:?}"
    );

    chat.on_key(Key::Enter, Instant::now());
    let view = panel_view_for(&chat, 30, 80).expect("the text is up");
    for line in &view.lines {
        if let crate::pickers::PanelLine::Row { text, .. } = line {
            assert!(
                titi_tui::width::visible_width(text) <= 80 - 8,
                "a wrapped row must fit the box the panel draws it in: {text:?}"
            );
        }
    }
    let title = view.title.expect("a title");
    assert!(title.starts_with("error 1 of 2"), "{title}");
    assert!(title.ends_with("provider"), "{title}");
    let body: String = view
        .lines
        .iter()
        .filter_map(|line| match line {
            crate::pickers::PanelLine::Row { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        body.matches("very long reason").count() >= 25,
        "the whole message, wrapped: {body}"
    );

    // Enter again goes back to the list; Esc closes the panel altogether.
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        panel_view_for(&chat, 30, 80)
            .and_then(|view| view.title)
            .as_deref(),
        Some("errors · 2")
    );
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Enter, Instant::now());
    let title = panel_view_for(&chat, 30, 80)
        .and_then(|view| view.title)
        .expect("second error");
    assert!(title.starts_with("error 2 of 2"), "{title}");
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.errors.picker.is_none());
    assert!(panel_view_for(&chat, 30, 80).is_none());
}

#[test]
fn the_panel_scrolls_through_a_message_taller_than_itself() {
    let (_dir, mut chat) = chat();
    let tall = (0..40)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    chat.push_error(ErrorSource::Tool, "tool error".to_owned(), &tall);
    type_command(&mut chat, "/errors");
    chat.on_key(Key::Enter, Instant::now());
    let first = panel_view_for(&chat, 30, 80).expect("panel");
    assert_eq!(first.window.start, 0);
    for _ in 0..30 {
        chat.on_key(Key::Down, Instant::now());
    }
    let scrolled = panel_view_for(&chat, 30, 80).expect("panel");
    assert!(scrolled.window.start > 0, "{:?}", scrolled.window);
    for _ in 0..100 {
        chat.on_key(Key::Down, Instant::now());
    }
    let end = panel_view_for(&chat, 30, 80).expect("panel");
    assert_eq!(end.selected, Some(39), "stops on the last line");
}

#[test]
fn errors_with_nothing_logged_says_so_instead_of_opening_an_empty_panel() {
    let (_dir, mut chat) = chat();
    type_command(&mut chat, "/errors");
    assert!(chat.errors.picker.is_none());
    let said = chat.lines.last().expect("a note");
    assert_eq!(said.text, "errors: none this session");
    assert_eq!(said.kind, LineKind::Note);
}

#[test]
fn another_key_closes_the_panel_and_does_its_own_job() {
    let (_dir, mut chat) = chat();
    chat.push(LineKind::Error, "boom".to_owned());
    type_command(&mut chat, "/errors");
    chat.on_key(Key::Char('x'), Instant::now());
    assert!(chat.errors.picker.is_none());
    assert_eq!(chat.input, "x", "the key was not swallowed");
}
