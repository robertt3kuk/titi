//! `/effort`: the command, the engine's answer, and the status line.

use std::sync::Arc;
use std::time::Instant;

use titi_engine::{EngineCommand, EngineEvent};
use titi_providers::Effort;
use titi_tui::theme::Theme;

use crate::chat::{Chat, ChatEffect, Key, masthead_snapshot};
use crate::palette::command_values;
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

fn chat() -> (tempfile::TempDir, Chat) {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut chat = Chat::new("anthropic/claude-sonnet-5-5", "session-1", theme());
    chat.set_agent_dir(dir.path());
    (dir, chat)
}

fn submit(chat: &mut Chat, text: &str) -> Option<ChatEffect> {
    for ch in text.chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
    chat.on_key(Key::Enter, Instant::now()).effect
}

fn lines(chat: &Chat, kind: LineKind) -> Vec<String> {
    chat.lines
        .iter()
        .filter(|line| line.kind == kind)
        .map(|line| line.text.clone())
        .collect()
}

fn reported(effort: Option<Effort>, applied: Option<Effort>, cache_miss: bool) -> EngineEvent {
    EngineEvent::EffortChanged {
        effort,
        applied,
        cache_miss,
    }
}

#[test]
fn effort_sends_the_level_and_off_clears_it() {
    let (_dir, mut chat) = chat();
    for (word, effort) in [
        ("low", Some(Effort::Low)),
        ("medium", Some(Effort::Medium)),
        ("high", Some(Effort::High)),
        ("xhigh", Some(Effort::Xhigh)),
        ("off", None),
    ] {
        assert_eq!(
            submit(&mut chat, &format!("/effort {word}")),
            Some(ChatEffect::Send(EngineCommand::SetEffort { effort })),
            "/effort {word}"
        );
    }
}

#[test]
fn a_word_that_is_not_a_level_is_refused_without_asking_the_engine() {
    let (_dir, mut chat) = chat();
    assert_eq!(submit(&mut chat, "/effort turbo"), None);
    let errors = lines(&chat, LineKind::Error);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("usage: /effort"), "{errors:?}");
    assert!(errors[0].contains("turbo"), "{errors:?}");
}

#[test]
fn a_change_after_the_first_request_says_the_cache_is_lost() {
    let (_dir, mut chat) = chat();
    chat.on_event(reported(Some(Effort::High), Some(Effort::High), true));
    assert_eq!(
        lines(&chat, LineKind::Note),
        vec!["effort high · next request misses the prompt cache"]
    );
}

#[test]
fn a_change_before_any_request_costs_nothing_and_says_only_the_level() {
    let (_dir, mut chat) = chat();
    chat.on_event(reported(Some(Effort::Low), Some(Effort::Low), false));
    assert_eq!(lines(&chat, LineKind::Note), vec!["effort low"]);
}

#[test]
fn a_model_that_takes_no_effort_is_named_and_the_status_line_stays_quiet() {
    let (_dir, mut chat) = chat();
    chat.on_event(reported(Some(Effort::High), None, false));
    assert_eq!(
        lines(&chat, LineKind::Note),
        vec![
            "effort high · anthropic/claude-sonnet-5-5 takes no effort setting, so nothing is sent"
        ]
    );
    assert_eq!(masthead_snapshot(&chat).effort, None);
}

#[test]
fn a_level_the_model_moved_is_said_and_the_status_line_shows_what_is_sent() {
    let (_dir, mut chat) = chat();
    chat.on_event(reported(Some(Effort::Max), Some(Effort::High), false));
    let notes = lines(&chat, LineKind::Note);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].starts_with("effort max · "), "{notes:?}");
    assert!(notes[0].contains("is sent high"), "{notes:?}");
    assert_eq!(masthead_snapshot(&chat).effort.as_deref(), Some("high"));
}

#[test]
fn clearing_says_the_provider_default_applies() {
    let (_dir, mut chat) = chat();
    chat.on_event(reported(Some(Effort::High), Some(Effort::High), false));
    chat.on_event(reported(None, None, true));
    assert_eq!(
        lines(&chat, LineKind::Note).last().map(String::as_str),
        Some("effort off · the provider's default · next request misses the prompt cache")
    );
    assert_eq!(masthead_snapshot(&chat).effort, None);
}

#[test]
fn a_repeat_report_stays_silent_unless_a_command_is_waiting_for_it() {
    let (_dir, mut chat) = chat();
    chat.on_event(reported(Some(Effort::High), Some(Effort::High), false));
    // The engine repeats the report after a model switch: nothing changed.
    chat.on_event(reported(Some(Effort::High), Some(Effort::High), false));
    assert_eq!(lines(&chat, LineKind::Note).len(), 1);

    // The person typed `/effort high` again and is owed an answer.
    submit(&mut chat, "/effort high");
    chat.on_event(reported(Some(Effort::High), Some(Effort::High), false));
    assert_eq!(lines(&chat, LineKind::Note).len(), 2);
}

#[test]
fn effort_without_a_word_says_what_is_in_force() {
    let (_dir, mut chat) = chat();
    assert_eq!(submit(&mut chat, "/effort"), None);
    assert_eq!(
        lines(&chat, LineKind::Note),
        vec!["effort off · the provider's default"]
    );
}

#[test]
fn the_value_list_offers_every_word_and_marks_the_one_in_force() {
    let (_dir, mut chat) = chat();
    let words = |chat: &Chat| -> Vec<(String, bool)> {
        command_values(chat, "effort")
            .expect("a value list")
            .into_iter()
            .map(|value| (value.arg, value.current))
            .collect()
    };
    let current = |chat: &Chat| -> Vec<String> {
        words(chat)
            .into_iter()
            .filter(|(_, current)| *current)
            .map(|(word, _)| word)
            .collect()
    };
    let offered: Vec<String> = words(&chat).into_iter().map(|(word, _)| word).collect();
    assert_eq!(
        offered,
        ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(current(&chat), ["off"]);
    chat.on_event(reported(Some(Effort::Xhigh), Some(Effort::Xhigh), false));
    assert_eq!(current(&chat), ["xhigh"]);
}
