//! The command palette and its value lists, tested through the chat: what a
//! key does, and what the panel then shows.

use super::*;
use crate::palette::{Action, SETTINGS, Target, VALUE_COMMANDS, command_values, entries, rank};
use crate::pickers::{PanelLine, panel_view_for};
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

/// A chat whose agent directory is the test's own, so nothing a test saves
/// reaches the real config.
fn chat_in(agent: &Path) -> Chat {
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    chat.set_agent_dir(agent);
    chat
}

fn type_text(chat: &mut Chat, text: &str) {
    for ch in text.chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
}

fn press(chat: &mut Chat, key: Key) -> Applied {
    chat.on_key(key, Instant::now())
}

/// The lines of the panel on screen, as text.
fn panel_rows(chat: &Chat) -> Vec<String> {
    let Some(view) = panel_view_for(chat, 30, 80) else {
        return Vec::new();
    };
    view.lines
        .into_iter()
        .map(|line| match line {
            PanelLine::Heading(text) => text,
            PanelLine::Row { text, .. } => text,
        })
        .collect()
}

fn panel_title(chat: &Chat) -> String {
    panel_view_for(chat, 30, 80)
        .and_then(|view| view.title)
        .unwrap_or_default()
}

fn notes(chat: &Chat) -> Vec<String> {
    chat.lines.iter().map(|line| line.text.clone()).collect()
}

fn labels(found: &[usize]) -> Vec<String> {
    let all = entries();
    found.iter().map(|at| all[*at].label.clone()).collect()
}

// ---- Ranking ---------------------------------------------------------------

#[test]
fn a_fuzzy_query_finds_the_command_it_abbreviates() {
    let all = entries();
    let found = labels(&rank(&all, "ctx"));
    assert_eq!(
        found.first().map(String::as_str),
        Some("/context"),
        "{found:?}"
    );
}

#[test]
fn the_exact_name_ranks_above_the_names_it_starts() {
    let all = entries();
    let found = labels(&rank(&all, "model"));
    assert_eq!(found[0], "/model", "{found:?}");
    assert_eq!(
        found[1], "models",
        "the panel follows the command: {found:?}"
    );
    let found = labels(&rank(&all, "/plan"));
    assert_eq!(found[0], "/plan", "a leading slash is not part of the name");
}

#[test]
fn commands_panels_and_settings_share_one_list() {
    let all = entries();
    for expected in [
        "/context",
        "models",
        "themes",
        "sessions",
        "history",
        "tree",
        "statusline preset",
        "login",
        "genome.enabled",
    ] {
        assert!(
            all.iter().any(|entry| entry.label == expected),
            "{expected} missing from the palette"
        );
    }
    for command in crate::pickers::COMMANDS {
        assert!(
            all.iter()
                .any(|entry| entry.action == Action::Command(command.name)),
            "/{} missing from the palette",
            command.name
        );
    }
    assert_eq!(rank(&all, "").len(), all.len(), "an empty query keeps all");
}

#[test]
fn a_substring_beats_a_scatter_and_a_description_is_the_last_resort() {
    let all = entries();
    let found = labels(&rank(&all, "preset"));
    assert_eq!(found[0], "statusline preset", "{found:?}");
    assert!(found.contains(&"statusLine.preset".to_owned()), "{found:?}");
    let found = labels(&rank(&all, "toolless"));
    assert!(found.contains(&"/duck".to_owned()), "{found:?}");
    assert!(rank(&all, "zzqx").is_empty());
    // One letter is not enough for the fuzzy tiers: only names holding it.
    let found = labels(&rank(&all, "x"));
    assert!(
        found.iter().all(|label| label.to_lowercase().contains('x')),
        "{found:?}"
    );
}

// ---- Ctrl+K ----------------------------------------------------------------

#[test]
fn ctrl_k_opens_the_palette_on_an_empty_draft() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    assert!(chat.panel_open());
    assert!(
        panel_title(&chat).starts_with("palette · "),
        "{}",
        panel_title(&chat)
    );
    type_text(&mut chat, "ctx");
    assert_eq!(
        panel_title(&chat),
        format!(
            "palette · {} of {} · ctx",
            panel_rows(&chat).len(),
            entries().len()
        )
    );
    assert!(
        panel_rows(&chat)[0].starts_with("/context"),
        "{:?}",
        panel_rows(&chat)
    );
    press(&mut chat, Key::Esc);
    assert!(!chat.panel_open(), "Esc closes it");
    press(&mut chat, Key::CtrlK);
    press(&mut chat, Key::CtrlK);
    assert!(!chat.panel_open(), "Ctrl+K again closes it");
}

#[test]
fn ctrl_k_on_a_draft_deletes_to_the_end_and_opens_nothing() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    type_text(&mut chat, "abc def");
    chat.set_caret(3);
    press(&mut chat, Key::CtrlK);
    assert_eq!(chat.input, "abc");
    assert_eq!(chat.caret(), 3);
    assert!(!chat.panel_open());
    // At the end there is nothing to delete, and still no palette.
    press(&mut chat, Key::CtrlK);
    assert_eq!(chat.input, "abc");
    assert!(!chat.panel_open());
}

#[test]
fn enter_runs_a_command_that_needs_nothing() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "plan");
    let applied = press(&mut chat, Key::Enter);
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Plan
        }))
    );
    assert!(!chat.panel_open());
}

#[test]
fn a_command_that_needs_text_is_left_in_the_composer() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "btw");
    let applied = press(&mut chat, Key::Enter);
    assert_eq!(applied.effect, None);
    assert_eq!(chat.input, "/btw ");
    assert!(!chat.panel_open());
}

#[test]
fn a_panel_entry_opens_its_panel() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "themes");
    press(&mut chat, Key::Enter);
    assert!(chat.theme_picker.is_some(), "the theme picker is up");
    assert!(chat.palette.list.is_none());

    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "statusline preset");
    press(&mut chat, Key::Enter);
    let list = chat.palette.values.as_ref().expect("a value list");
    assert_eq!(list.target, Target::Command("statusline"));
}

// ---- Value lists -----------------------------------------------------------

#[test]
fn a_value_list_marks_the_value_in_force() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "details");
    press(&mut chat, Key::Enter);
    let rows = panel_rows(&chat);
    assert!(
        panel_title(&chat).starts_with("/details"),
        "{}",
        panel_title(&chat)
    );
    assert!(
        rows.contains(&"thinking expanded  ✓ current".to_owned()),
        "{rows:?}"
    );
    assert!(
        rows.contains(&"subagents collapsed  ✓ current".to_owned()),
        "{rows:?}"
    );
    assert!(rows.contains(&"thinking hidden".to_owned()), "{rows:?}");
    assert_eq!(
        rows.iter().filter(|row| row.ends_with("✓ current")).count(),
        DETAIL_SECTIONS_COUNT,
        "one mark per section: {rows:?}"
    );

    // Picking a word runs the command with it.
    type_text(&mut chat, "tools hid");
    press(&mut chat, Key::Enter);
    assert_eq!(
        chat.details.mode("tools"),
        Some(titi_tui::markdown::SectionMode::Hidden)
    );
    assert!(!chat.panel_open(), "the pick closes the lists");
}

const DETAIL_SECTIONS_COUNT: usize = 5;

#[test]
fn esc_in_a_value_list_goes_back_to_the_palette() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "mouse");
    press(&mut chat, Key::Enter);
    let rows = panel_rows(&chat);
    assert_eq!(
        rows,
        ["off", "wheel", "buttons  ✓ current", "all"],
        "the default preset is the one marked"
    );
    press(&mut chat, Key::Esc);
    assert!(chat.palette.values.is_none());
    assert!(chat.palette.list.is_some(), "back at the palette");
    assert!(panel_title(&chat).starts_with("palette"));
}

#[test]
fn typing_a_value_command_and_a_space_opens_its_list() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    type_text(&mut chat, "/theme");
    assert!(
        !panel_rows(&chat).iter().any(|row| row.starts_with("auto")),
        "the command list, not the values, before the space"
    );
    type_text(&mut chat, " ");
    assert_eq!(chat.arg_rows().map(|found| found.command), Some("theme"));
    assert_eq!(panel_title(&chat), "/theme");
    let rows = panel_rows(&chat);
    assert_eq!(rows[0], "auto  ✓ current", "{rows:?}");
    assert!(rows.iter().any(|row| row == "titanium"), "{rows:?}");

    // The list follows the draft: typing narrows it, the draft keeps the text.
    type_text(&mut chat, "titan");
    assert_eq!(chat.input, "/theme titan");
    let rows = panel_rows(&chat);
    assert!(
        rows.iter().all(|row| row.contains('t')) && rows.len() < 5,
        "{rows:?}"
    );

    // Esc hides the list and leaves the draft.
    press(&mut chat, Key::Esc);
    assert!(!chat.panel_open());
    assert_eq!(chat.input, "/theme titan");

    // Backspace takes the space back, and the command list is the one up.
    let mut chat = chat_in(dir.path());
    type_text(&mut chat, "/theme ");
    press(&mut chat, Key::Backspace);
    assert!(chat.arg_rows().is_none());
    assert_eq!(chat.input, "/theme");
}

#[test]
fn the_arrows_choose_a_word_and_enter_runs_the_command_with_it() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    type_text(&mut chat, "/theme titanium");
    let rows = panel_rows(&chat);
    assert_eq!(rows[0], "titanium", "{rows:?}");
    // Moving the highlight is the choice: down and back up lands on the
    // first row again, and Enter now means that row.
    press(&mut chat, Key::Down);
    press(&mut chat, Key::Up);
    press(&mut chat, Key::Enter);
    assert!(
        notes(&chat)
            .iter()
            .any(|line| line.starts_with("theme titanium")),
        "{:?}",
        notes(&chat)
    );
    assert_eq!(chat.input, "", "the draft is spent with the command");
    assert!(!chat.panel_open());
}

#[test]
fn enter_sends_what_was_typed_unless_the_highlight_moved() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    // `/details thinking` reports a section; the list's first row under it
    // is `thinking hidden`, which Enter must not substitute.
    type_text(&mut chat, "/details thinking");
    assert!(chat.arg_rows().is_some(), "the list is up under it");
    press(&mut chat, Key::Enter);
    assert!(
        notes(&chat).contains(&"details: thinking expanded".to_owned()),
        "{:?}",
        notes(&chat)
    );
    assert_eq!(
        chat.details.mode("thinking"),
        Some(titi_tui::markdown::SectionMode::Expanded),
        "reporting a section moved nothing"
    );

    // A command typed out in full goes as typed, list or no list.
    type_text(&mut chat, "/details tools hidden");
    press(&mut chat, Key::Enter);
    assert_eq!(
        chat.details.mode("tools"),
        Some(titi_tui::markdown::SectionMode::Hidden)
    );

    // A highlight moved on one draft is not a choice on the next.
    type_text(&mut chat, "/mouse ");
    press(&mut chat, Key::Down);
    type_text(&mut chat, "x");
    press(&mut chat, Key::Backspace);
    press(&mut chat, Key::Enter);
    assert_eq!(
        chat.mouse_preset,
        MousePreset::Buttons,
        "nothing was picked"
    );
}

#[test]
fn tab_writes_the_highlighted_word_into_the_draft() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    type_text(&mut chat, "/stat");
    press(&mut chat, Key::Tab);
    assert_eq!(chat.input, "/statusline ");
    assert!(panel_rows(&chat).contains(&"default  ✓ current".to_owned()));
    press(&mut chat, Key::Down);
    press(&mut chat, Key::Tab);
    assert_eq!(chat.input, "/statusline minimal");
}

#[test]
fn every_value_command_has_a_list_and_no_other_command_does() {
    let dir = tempfile::tempdir().expect("temp");
    let chat = chat_in(dir.path());
    for name in VALUE_COMMANDS {
        let values = command_values(&chat, name).unwrap_or_default();
        assert!(!values.is_empty(), "/{name} has no values");
        // `/details` takes a section and a mode, so it marks one mode per
        // section; every other list marks at most one word.
        let allowed = if name == "details" {
            DETAIL_SECTIONS_COUNT
        } else {
            1
        };
        assert!(
            values.iter().filter(|value| value.current).count() <= allowed,
            "/{name} marks too many values"
        );
    }
    for command in crate::pickers::COMMANDS {
        if !VALUE_COMMANDS.contains(&command.name) {
            assert!(
                command_values(&chat, command.name).is_none(),
                "/{}",
                command.name
            );
        }
    }
}

// ---- Settings --------------------------------------------------------------

#[test]
fn enter_on_a_setting_says_its_value_and_source_then_offers_its_words() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(dir.path().join("config.yml"), "genome:\n  enabled: false\n").expect("config");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "genome.enabled");
    press(&mut chat, Key::Enter);
    assert!(
        notes(&chat).contains(&"genome.enabled = false (agent)".to_owned()),
        "{:?}",
        notes(&chat)
    );
    let rows = panel_rows(&chat);
    assert_eq!(rows, ["true", "false  ✓ current"], "{rows:?}");
    assert!(
        panel_title(&chat).contains("agent"),
        "{}",
        panel_title(&chat)
    );

    type_text(&mut chat, "tr");
    press(&mut chat, Key::Enter);
    let settings = titi_config::settings::Settings::load(
        dir.path(),
        &crate::session_fs::current_workspace(),
        &[],
    )
    .expect("settings");
    assert_eq!(
        settings.get("genome.enabled"),
        Some(serde_json::json!(true)),
        "the pick was saved"
    );
    assert!(
        notes(&chat)
            .iter()
            .any(|line| line.starts_with("genome.enabled = true · saved")),
        "{:?}",
        notes(&chat)
    );
}

#[test]
fn a_free_setting_is_only_reported() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_in(dir.path());
    press(&mut chat, Key::CtrlK);
    type_text(&mut chat, "genome.limit");
    press(&mut chat, Key::Enter);
    assert!(
        notes(&chat)
            .iter()
            .any(|line| line.starts_with("genome.limit: not set")),
        "{:?}",
        notes(&chat)
    );
    assert!(!chat.panel_open(), "nothing to choose from");
}

/// The palette's setting table and `titi-config`'s `*_KEY` constants are two
/// lists of the same names; this is the one place they are compared.
#[test]
fn the_setting_table_covers_every_key_constant_of_the_config_crate() {
    let source = include_str!("../../titi-config/src/settings.rs");
    let mut declared: Vec<String> = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if !(line.starts_with("pub const ") && line.contains("_KEY: &str = \"")) {
            continue;
        }
        if let Some(value) = line.split('"').nth(1) {
            declared.push(value.to_owned());
        }
    }
    assert!(declared.len() > 20, "the scan found {}", declared.len());
    for key in &declared {
        assert!(
            SETTINGS.iter().any(|setting| setting.key == key),
            "{key} is a titi-config key the palette does not list"
        );
    }
    for setting in SETTINGS {
        assert!(
            declared.iter().any(|key| key == setting.key),
            "{} is listed but titi-config has no such key",
            setting.key
        );
    }
}

// ---- /help -------------------------------------------------------------------

/// Bare `/help` opens the palette on the commands alone, with a title that
/// counts them, instead of dumping a note per command into the transcript.
#[test]
fn bare_help_opens_the_palette_on_the_commands() {
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(agent.path());
    type_text(&mut chat, "/help");
    press(&mut chat, Key::Enter);
    assert!(chat.palette.list.is_some(), "the palette is up");
    assert!(
        chat.lines.is_empty(),
        "nothing is written to the transcript: {:?}",
        notes(&chat)
    );
    let title = panel_title(&chat);
    let count = crate::pickers::COMMANDS.len();
    assert_eq!(title, format!("help · {count} commands"));
    let rows = panel_rows(&chat);
    assert_eq!(rows.len(), count, "{rows:?}");
    assert!(
        rows.iter().all(|row| row.starts_with('/')),
        "commands only, no panels or settings: {rows:?}"
    );
}

/// `/help <text>` opens the same palette with the text already typed.
#[test]
fn help_with_text_opens_the_palette_filtered() {
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(agent.path());
    type_text(&mut chat, "/help ctx");
    press(&mut chat, Key::Enter);
    let title = panel_title(&chat);
    assert!(title.starts_with("help · "), "{title}");
    assert!(title.ends_with("ctx"), "{title}");
    let rows = panel_rows(&chat);
    assert!(rows[0].starts_with("/context"), "{rows:?}");
    assert!(rows.iter().all(|row| row.starts_with('/')), "{rows:?}");
}

/// Enter on a row of the help palette runs that command.
#[test]
fn a_row_of_the_help_palette_runs_its_command() {
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(agent.path());
    type_text(&mut chat, "/help hotkeys");
    press(&mut chat, Key::Enter);
    press(&mut chat, Key::Enter);
    assert!(chat.palette.list.is_none());
    assert!(
        notes(&chat).iter().any(|row| row.contains("alt+s")),
        "/hotkeys ran: {:?}",
        notes(&chat)
    );
    assert!(
        !notes(&chat)
            .iter()
            .any(|row| row.starts_with("keys  @file")),
        "the text listing did not run: {:?}",
        notes(&chat)
    );
}

/// `/help all` keeps the old text listing, keys line included.
#[test]
fn help_all_lists_every_command_as_text() {
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(agent.path());
    type_text(&mut chat, "/help all");
    press(&mut chat, Key::Enter);
    assert!(chat.palette.list.is_none());
    let all = notes(&chat);
    assert_eq!(all.len(), crate::pickers::COMMANDS.len() + 1, "{all:#?}");
    assert!(all[0].starts_with('/'), "{all:#?}");
    assert!(all.last().is_some_and(|row| row.starts_with("keys  @file")));
}
