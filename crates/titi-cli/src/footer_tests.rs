//! Tests for the turn footer's actions, the error log and `/usage` by model.
//!
//! They drive the screen through its public surface — keys, engine events and
//! the transcript's own renderer — so each one reads as a thing a person does.

use std::sync::Arc;
use std::time::Instant;

use titi_core::session::{Role, SessionStore};
use titi_tui::status::{FooterAction, FooterActions};
use titi_tui::theme::Theme;

use crate::chat::{Chat, Key};
use crate::footer_actions::{PromptAnchor, TurnEnd, anchor_at, answers, locate_turn_end};
use crate::transcript::{LineKind, transcript};

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

/// One finished turn on screen: the question, the answer, the footer.
fn turn(chat: &mut Chat, question: &str, answer: &str, footer: &str) {
    chat.push(LineKind::User, question.to_owned());
    chat.push(LineKind::Assistant, answer.to_owned());
    chat.push(LineKind::Usage, footer.to_owned());
}

/// The rows the transcript draws at `width`, and the row index the line that
/// contains `needle` is on.
fn drawn(chat: &mut Chat, width: u16, height: u16) -> Vec<String> {
    let theme = theme();
    let _ = transcript(chat, width, height, &theme);
    chat.last_rows.clone()
}

fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is drawn: {rows:?}"))
}

const SHORT: &str = "1.4s · 3.4k prompt · 250 out";
const LONG: &str = "1.4s · 3.4k prompt (2.9k cached) · 250 out · $0.0041";

// ---- The footer's layout -----------------------------------------------

#[test]
fn the_footer_draws_its_actions_flush_right_when_the_pane_is_wide() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "a", LONG);
    let rows = drawn(&mut chat, 120, 12);
    let row = &rows[row_of(&rows, "1.4s")];
    assert!(
        row.starts_with("     1.4s"),
        "indented under the text: {row:?}"
    );
    assert!(row.ends_with("copy · fork"), "{row:?}");
    // 120 cells less the two of border, the five of gutter-in and the one of
    // margin: the actions end where the body does.
    assert_eq!(row.chars().count(), 120 - 2 - 1, "{row:?}");
}

#[test]
fn a_footer_that_fits_at_sixty_columns_keeps_its_actions() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "a", SHORT);
    let rows = drawn(&mut chat, 60, 12);
    let row = &rows[row_of(&rows, "1.4s")];
    assert!(row.ends_with("copy · fork"), "{row:?}");
    assert!(row.contains("250 out"), "the text is whole: {row:?}");
}

#[test]
fn a_narrow_pane_gives_the_actions_up_before_the_text() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "a", LONG);
    let rows = drawn(&mut chat, 60, 12);
    let row = &rows[row_of(&rows, "1.4s")];
    assert!(!row.contains("copy"), "no room beside the text: {row:?}");
    assert!(row.contains("$0.0041"), "the text is untouched: {row:?}");
    assert!(
        chat.footer_hits.is_empty(),
        "an action that is not drawn cannot be clicked"
    );
}

#[test]
fn a_footer_that_wraps_has_no_actions() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "a", LONG);
    let rows = drawn(&mut chat, 30, 12);
    assert!(rows.iter().all(|row| !row.contains("copy")), "{rows:?}");
    assert!(chat.footer_hits.is_empty());
}

// ---- Clicking an action ------------------------------------------------

#[test]
fn a_click_lands_on_the_footer_of_its_own_turn() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "first", "answer one", SHORT);
    turn(&mut chat, "second", "answer two", SHORT);
    let rows = drawn(&mut chat, 80, 20);
    chat.transcript_top = 2;
    let usage: Vec<usize> = chat
        .lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| (line.kind == LineKind::Usage).then_some(at))
        .collect();
    let footer_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(at, row)| row.contains("1.4s").then_some(at))
        .collect();
    assert_eq!(footer_rows.len(), 2, "{rows:?}");
    let start = chat.footer_hits[0].actions.start() as u16;

    let screen = |row: usize| row as u16 + chat.transcript_top;
    assert_eq!(
        chat.footer_action_at(start, screen(footer_rows[0])),
        Some((usage[0], FooterAction::Copy))
    );
    assert_eq!(
        chat.footer_action_at(start + 8, screen(footer_rows[1])),
        Some((usage[1], FooterAction::Fork)),
        "the second turn's own fork"
    );
    assert_eq!(
        chat.footer_action_at(start + 5, screen(footer_rows[1])),
        None,
        "the separator is neither"
    );
    assert_eq!(
        chat.footer_action_at(start - 3, screen(footer_rows[1])),
        None,
        "left of the actions is the text"
    );
    assert_eq!(
        chat.footer_action_at(start, screen(footer_rows[1]) + 1),
        None,
        "one row below"
    );
}

#[test]
fn a_scrolled_transcript_moves_the_hits_with_it() {
    let (_dir, mut chat) = chat();
    for n in 0..6 {
        turn(&mut chat, &format!("q{n}"), &format!("a{n}"), SHORT);
    }
    // Room for about three turns: the oldest footers are off screen.
    let rows = drawn(&mut chat, 80, 8);
    assert!(
        chat.footer_hits.len() < 6,
        "off-screen footers have no hit: {rows:?}"
    );
    let newest = chat
        .footer_hits
        .last()
        .copied()
        .expect("a footer is on screen");
    assert_eq!(newest.line, chat.lines.len() - 1);

    chat.scroll_offset = 4;
    let rows = drawn(&mut chat, 80, 8);
    for hit in &chat.footer_hits {
        assert!(
            rows[usize::from(hit.row)].contains("1.4s"),
            "the hit sits on its footer: {rows:?}"
        );
    }
}

#[test]
fn clicking_copy_puts_that_turns_answer_on_the_clipboard() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "first", "answer one", SHORT);
    chat.push(LineKind::User, "second".to_owned());
    chat.push(LineKind::Assistant, "before the tools".to_owned());
    chat.push(LineKind::Assistant, "after the tools".to_owned());
    chat.push(LineKind::Usage, SHORT.to_owned());
    let first = chat
        .lines
        .iter()
        .position(|line| line.kind == LineKind::Usage)
        .expect("first footer");

    chat.run_footer_action(first, FooterAction::Copy, "/nonexistent");
    assert_eq!(chat.output_flush, titi_tui::caps::osc52_copy("answer one"));
    chat.output_flush.clear();

    let last = chat.lines.len() - 1;
    chat.run_footer_action(last, FooterAction::Copy, "/nonexistent");
    assert_eq!(
        chat.output_flush,
        titi_tui::caps::osc52_copy("before the tools\n\nafter the tools"),
        "every round of the turn, not just the last"
    );
    assert!(chat.hint.contains("OSC 52"), "{:?}", chat.hint);
}

#[test]
fn a_press_on_the_footer_does_not_start_a_selection() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "an answer", SHORT);
    let rows = drawn(&mut chat, 80, 12);
    let row = row_of(&rows, "1.4s") as u16;
    let x = chat.footer_hits[0].actions.start() as u16;
    chat.transcript_top = 0;
    let applied = chat.mouse_press(x, row);
    assert!(applied.is_none());
    assert!(chat.selection().is_none(), "a click is not a drag");
    assert!(
        !chat.output_flush.is_empty() || !chat.hint.is_empty(),
        "and it copied something"
    );
}

// ---- Forking at a turn -------------------------------------------------

fn session_with(agent_dir: &std::path::Path, entries: &[(Role, &str)]) -> (SessionStore, String) {
    let store = SessionStore::new(agent_dir).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    for (role, text) in entries {
        store.append(&id, *role, text).expect("append");
    }
    (store, id)
}

fn contents(store: &SessionStore, id: &str) -> Vec<String> {
    store
        .walk(id, None)
        .expect("walk")
        .into_iter()
        .map(|entry| entry.content)
        .collect()
}

#[test]
fn forking_at_a_turn_keeps_the_conversation_up_to_its_end() {
    let (dir, mut chat) = chat();
    let (store, id) = session_with(
        dir.path(),
        &[
            (Role::User, "one"),
            (Role::Assistant, "a1"),
            (Role::Tool, "tool output"),
            (Role::Assistant, "a1 again"),
            (Role::User, "two"),
            (Role::Assistant, "a2"),
        ],
    );
    chat.session_id = id.clone();
    turn(&mut chat, "one", "a1 again", SHORT);
    turn(&mut chat, "two", "a2", SHORT);
    let first = chat
        .lines
        .iter()
        .position(|line| line.kind == LineKind::Usage)
        .expect("first footer");

    chat.run_footer_action(first, FooterAction::Fork, "/nonexistent");
    let said = chat.lines.last().expect("a note").clone();
    assert_eq!(said.kind, LineKind::Note, "{said:?}");
    assert!(said.text.contains("at the end of this turn"), "{said:?}");
    let new_id = said
        .text
        .split_whitespace()
        .nth(2)
        .expect("the new session's id")
        .to_owned();
    assert_eq!(
        contents(&store, &new_id),
        ["one", "a1", "tool output", "a1 again"],
        "everything the first turn did, nothing the second did"
    );
    assert_eq!(contents(&store, &id).len(), 6, "the source is untouched");
}

#[test]
fn forking_at_the_newest_turn_forks_the_whole_session_and_says_so() {
    let (dir, mut chat) = chat();
    let (store, id) = session_with(dir.path(), &[(Role::User, "one"), (Role::Assistant, "a1")]);
    chat.session_id = id;
    turn(&mut chat, "one", "a1", SHORT);
    chat.run_footer_action(chat.lines.len() - 1, FooterAction::Fork, "/nonexistent");
    let said = chat.lines.last().expect("a note").text.clone();
    assert!(said.contains("the latest turn"), "{said}");
    let new_id = said.split_whitespace().nth(2).expect("id").to_owned();
    assert_eq!(contents(&store, &new_id), ["one", "a1"]);
}

#[test]
fn a_turn_that_cannot_be_placed_forks_the_session_and_says_so() {
    let (dir, mut chat) = chat();
    let (store, id) = session_with(
        dir.path(),
        &[
            (Role::User, "something else entirely"),
            (Role::Assistant, "a1"),
            (Role::User, "two"),
            (Role::Assistant, "a2"),
        ],
    );
    chat.session_id = id;
    turn(&mut chat, "what the screen thinks was asked", "a1", SHORT);
    turn(&mut chat, "two", "a2", SHORT);
    let first = chat
        .lines
        .iter()
        .position(|line| line.kind == LineKind::Usage)
        .expect("footer");
    chat.run_footer_action(first, FooterAction::Fork, "/nonexistent");
    let said = chat.lines.last().expect("a note").text.clone();
    assert!(said.contains("could not be placed"), "{said}");
    let new_id = said.split_whitespace().nth(5).expect("id").to_owned();
    assert_eq!(contents(&store, &new_id).len(), 4, "the whole session");
}

#[test]
fn a_fork_with_no_session_file_reports_the_failure() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "one", "a1", SHORT);
    chat.run_footer_action(2, FooterAction::Fork, "/nonexistent");
    let said = chat.lines.last().expect("a line");
    assert_eq!(said.kind, LineKind::Error, "{said:?}");
    assert!(said.text.starts_with("fork: "), "{said:?}");
}

#[test]
fn a_message_sent_mid_turn_counts_as_a_prompt_when_placing_a_turn() {
    let mut lines = Vec::new();
    for (kind, text) in [
        (LineKind::User, "one"),
        (LineKind::Steer, "nudge"),
        (LineKind::Assistant, "a1"),
        (LineKind::Usage, SHORT),
        (LineKind::User, "two"),
        (LineKind::Assistant, "a2"),
        (LineKind::Usage, SHORT),
    ] {
        lines.push(crate::transcript::TranscriptLine::new(
            kind,
            text.to_owned(),
        ));
    }
    let anchor = anchor_at(&lines, 3).expect("anchor");
    assert_eq!(
        anchor,
        PromptAnchor {
            ordinal: 2,
            total: 3,
            text: "nudge".to_owned()
        }
    );
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, id) = session_with(
        dir.path(),
        &[
            (Role::User, "one"),
            (Role::User, "nudge"),
            (Role::Assistant, "a1"),
            (Role::User, "two"),
            (Role::Assistant, "a2"),
        ],
    );
    let entries = store.walk(&id, None).expect("walk");
    assert_eq!(
        locate_turn_end(&entries, &anchor),
        Some(TurnEnd::After(2)),
        "the turn ends with a1, right before `two`"
    );
    let last = anchor_at(&lines, 6).expect("anchor");
    assert_eq!(locate_turn_end(&entries, &last), Some(TurnEnd::Latest));
}

#[test]
fn a_resumed_transcript_that_holds_only_the_tail_still_finds_its_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, id) = session_with(
        dir.path(),
        &[
            (Role::User, "old"),
            (Role::Assistant, "old a"),
            (Role::User, "one"),
            (Role::Assistant, "a1"),
            (Role::User, "two"),
            (Role::Assistant, "a2"),
        ],
    );
    let entries = store.walk(&id, None).expect("walk");
    // The screen restored only the last two turns.
    let anchor = PromptAnchor {
        ordinal: 1,
        total: 2,
        text: "one".to_owned(),
    };
    assert_eq!(locate_turn_end(&entries, &anchor), Some(TurnEnd::After(3)));
    // A transcript with more prompts than the file has is not this file's.
    let too_many = PromptAnchor {
        ordinal: 1,
        total: 9,
        text: "one".to_owned(),
    };
    assert_eq!(locate_turn_end(&entries, &too_many), None);
}

// ---- /copy -------------------------------------------------------------

#[test]
fn copy_takes_the_nth_last_answer_and_defaults_to_the_newest() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q1", "first answer", SHORT);
    turn(&mut chat, "q2", "second answer", SHORT);
    turn(&mut chat, "q3", "third answer", SHORT);

    chat.copy_answer("", "/nonexistent");
    assert_eq!(
        chat.output_flush,
        titi_tui::caps::osc52_copy("third answer")
    );
    let said = chat.lines.last().expect("a note");
    assert_eq!(said.kind, LineKind::Note);
    assert!(
        said.text.starts_with("copied 12 chars") && said.text.contains("OSC 52"),
        "the route is said in the transcript, not in a hint the submit clears: {said:?}"
    );
    chat.output_flush.clear();
    chat.copy_answer("2", "/nonexistent");
    assert_eq!(
        chat.output_flush,
        titi_tui::caps::osc52_copy("second answer")
    );
    chat.output_flush.clear();
    chat.copy_answer("3", "/nonexistent");
    assert_eq!(
        chat.output_flush,
        titi_tui::caps::osc52_copy("first answer")
    );
    chat.output_flush.clear();

    chat.copy_answer("4", "/nonexistent");
    assert!(chat.output_flush.is_empty(), "nothing to copy");
    let said = chat.lines.last().expect("a note");
    assert_eq!(said.text, "copy: only 3 answers so far");
    assert_eq!(said.kind, LineKind::Note);
}

#[test]
fn copy_refuses_a_count_that_is_not_one_or_more() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "an answer", SHORT);
    for bad in ["0", "-1", "two", "1 2"] {
        chat.copy_answer(bad, "/nonexistent");
        let said = chat.lines.last().expect("an error");
        assert_eq!(said.kind, LineKind::Error, "{bad}: {said:?}");
        assert!(said.text.contains("usage: /copy [n]"), "{bad}: {said:?}");
    }
    assert!(
        chat.output_flush.is_empty(),
        "nothing reached the clipboard"
    );
}

#[test]
fn copy_in_an_empty_session_says_there_is_nothing_to_copy() {
    let (_dir, mut chat) = chat();
    type_command(&mut chat, "/copy");
    let said = chat.lines.last().expect("a note");
    assert_eq!(said.text, "copy: no answer yet");
    assert_eq!(said.kind, LineKind::Note);
    assert!(chat.output_flush.is_empty());
    // A question nobody has answered yet is still nothing to copy.
    chat.push(LineKind::User, "hello?".to_owned());
    type_command(&mut chat, "/copy 2");
    assert_eq!(
        chat.lines.last().map(|line| line.text.as_str()),
        Some("copy: no answer yet")
    );
}

#[test]
fn answers_skip_turns_that_said_nothing() {
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q1", "worded", SHORT);
    chat.push(LineKind::User, "q2".to_owned());
    chat.push(LineKind::Assistant, "   ".to_owned());
    chat.push(LineKind::Tool, "tool read".to_owned());
    assert_eq!(answers(&chat.lines), ["worded"]);
}

#[test]
fn the_actions_geometry_agrees_with_what_is_drawn() {
    // The hit the renderer recorded starts exactly where the word is drawn.
    let (_dir, mut chat) = chat();
    turn(&mut chat, "q", "a", SHORT);
    let rows = drawn(&mut chat, 100, 12);
    let row = &rows[row_of(&rows, "1.4s")];
    let hit = chat.footer_hits[0];
    let drawn_at = row.find("copy").expect("copy is drawn");
    assert_eq!(hit.actions.start(), row[..drawn_at].chars().count());
    assert_eq!(
        hit.actions.action_at(hit.actions.start()),
        Some(FooterAction::Copy)
    );
    let placed = FooterActions::place(SHORT.chars().count(), 100 - 2 - 6).expect("fits");
    assert_eq!(placed.shifted(5), hit.actions);
}
