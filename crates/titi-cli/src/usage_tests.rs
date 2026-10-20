//! Tests for `/usage` by model: what each turn is booked to, and the rows.

use std::sync::Arc;
use std::time::Instant;

use titi_engine::{EngineEvent, TurnId};
use titi_tui::theme::Theme;

use crate::chat::{Chat, Key};
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

// ---- /usage by model ---------------------------------------------------

fn report(chat: &mut Chat, id: u64, prompt: u32, cached: u32, out: u32, cost: Option<u64>) {
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(id),
        prompt_tokens: prompt,
        completion_tokens: out,
        cached_tokens: cached,
        cost_micro_usd: cost,
    });
}

fn started(chat: &mut Chat, id: u64, model: &str) {
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(id),
        model: model.into(),
    });
}

fn finished(chat: &mut Chat, id: u64) {
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(id),
        reason: titi_providers::StopReason::Stop,
    });
}

fn notes(chat: &Chat) -> Vec<String> {
    chat.lines
        .iter()
        .filter(|line| line.kind == LineKind::Note)
        .map(|line| line.text.clone())
        .collect()
}

#[test]
fn usage_breaks_the_session_down_by_model_with_hit_rates() {
    let (_dir, mut chat) = chat();
    started(&mut chat, 1, "openai/gpt-4.1");
    report(&mut chat, 1, 1_000, 0, 50, Some(2_000));
    finished(&mut chat, 1);
    started(&mut chat, 2, "openai/gpt-4.1");
    report(&mut chat, 2, 3_000, 2_000, 70, Some(1_000));
    finished(&mut chat, 2);
    chat.on_event(EngineEvent::ModelSwitched {
        turn_id: None,
        from: "openai/gpt-4.1".into(),
        to: "local/qwen".into(),
    });
    started(&mut chat, 3, "local/qwen");
    report(&mut chat, 3, 2_000, 1_000, 90, None);
    finished(&mut chat, 3);

    type_command(&mut chat, "/usage");
    let said = notes(&chat);
    let at = said
        .iter()
        .position(|line| line.starts_with("usage by model"))
        .unwrap_or_else(|| panic!("a breakdown: {said:?}"));
    let rows = &said[at..];
    assert!(rows[1].starts_with("openai/gpt-4.1"), "{rows:?}");
    assert!(
        rows[1].contains("4k prompt · 2k cached · 120 out · $0.003 · 50% hit"),
        "{rows:?}"
    );
    assert!(rows[2].starts_with("local/qwen"), "{rows:?}");
    assert!(
        rows[2].contains("2k prompt · 1k cached · 90 out"),
        "{rows:?}"
    );
    assert!(
        rows[2].contains("50% hit") && !rows[2].contains('$'),
        "{rows:?}"
    );
    assert!(rows[3].starts_with("total"), "{rows:?}");
    assert!(
        rows[3].contains("6k prompt · 3k cached · 210 out"),
        "{rows:?}"
    );
    assert!(rows[3].contains("50% hit"), "{rows:?}");
    assert!(
        rows.last().is_some_and(|line| line.starts_with("Turn: ")),
        "the session's own line still comes last: {rows:?}"
    );
}

#[test]
fn a_turn_that_fell_back_is_booked_to_the_model_it_ended_on() {
    let (_dir, mut chat) = chat();
    started(&mut chat, 1, "openai/gpt-4.1");
    chat.on_event(EngineEvent::ModelSwitched {
        turn_id: Some(TurnId(1)),
        from: "openai/gpt-4.1".into(),
        to: "anthropic/sonnet".into(),
    });
    report(&mut chat, 1, 500, 100, 20, None);
    finished(&mut chat, 1);
    let rows = chat.usage_ledger.lines();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[1].starts_with("anthropic/sonnet"), "{rows:?}");
    assert!(chat.usage_ledger.model("openai/gpt-4.1").is_none());
}

#[test]
fn usage_with_no_turns_prints_no_breakdown() {
    let (_dir, mut chat) = chat();
    type_command(&mut chat, "/usage");
    assert!(
        notes(&chat)
            .iter()
            .all(|line| !line.starts_with("usage by model")),
        "{:?}",
        notes(&chat)
    );
}
