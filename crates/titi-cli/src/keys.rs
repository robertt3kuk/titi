//! The keyboard, the mouse and the clipboard.
//!
//! Two halves. The first is the mapping the crate's keybindings read: a
//! crossterm `KeyEvent` into a canonical key id (`ctrl+q`, `alt+up`,
//! `shift+tab`), which feeds
//! [`titi_tui::keybindings::KeybindingsManager::matches_canonical`]. The second
//! is the live path: what a key the screen understands does — [`Chat::on_key`],
//! the modal-ish states it drives, the two-press quit and its Esc-Esc arming,
//! the mouse routing with the selection a drag makes, and the paste path that
//! keeps a long paste out of the draft.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use titi_tui::keys::canonical_key_id;
use titi_tui::selection::Selection;

use titi_engine::EngineCommand;

use crate::chat::*;
use crate::pickers::*;
use crate::transcript::*;

/// Convert a crossterm key event into a canonical key id.
///
/// Returns `None` for key-release events (OMP filters those unless a
/// component sets `wantsKeyRelease`).
pub fn canonical_from_key_event(key: &KeyEvent) -> Option<String> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("super");
    }

    let base = match key.code {
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(c) => {
            let lower = c.to_ascii_lowercase();
            if c.is_ascii_uppercase() && !parts.contains(&"shift") {
                parts.push("shift");
            }
            lower.to_string()
        }
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "escape".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => {
            if !parts.contains(&"shift") {
                parts.push("shift");
            }
            "tab".to_owned()
        }
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        _ => return None,
    };

    let raw = if parts.is_empty() {
        base
    } else {
        format!("{}+{base}", parts.join("+"))
    };
    Some(canonical_key_id(&raw))
}

/// Map a key event to an overlay input sequence.
///
/// Panels consume raw decoded input (Esc, arrows, Enter, Ctrl+D/N/R, and
/// printable type-to-filter characters). Keys without a mapping are ignored
/// while an overlay is open (modal).
pub fn overlay_key_data(key: &KeyEvent) -> Option<String> {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => Some("\x1b".into()),
        (KeyCode::Enter, _) => Some("\r".into()),
        (KeyCode::Tab, _) => Some("\t".into()),
        (KeyCode::Up, _) => Some("\x1b[A".into()),
        (KeyCode::Down, _) => Some("\x1b[B".into()),
        (KeyCode::Left, _) => Some("\x1b[D".into()),
        (KeyCode::Backspace, _) => Some("\x7f".into()),
        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => Some("\x04".into()),
        (KeyCode::Char('n'), m) if m.contains(KeyModifiers::CONTROL) => Some("\x0e".into()),
        (KeyCode::Char('r'), m) if m.contains(KeyModifiers::CONTROL) => Some("\x12".into()),
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Some("\x03".into()),
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            Some(c.to_string())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The live key path
// ---------------------------------------------------------------------------

pub(crate) const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Pastes longer than this many lines collapse to a marker instead of filling
/// the draft (the deleted `composer.rs`'s `PASTE_INLINE_MAX_LINES`, restored).
///
/// Six is where a paste stops reading as something the user typed: a prompt, a
/// path, a couple of log lines stay inline, while a stack trace or a file no
/// longer becomes the prompt verbatim.
const PASTE_INLINE_MAX_LINES: usize = 6;

/// Where a collapsed paste's marker starts. Only a registered marker is ever
/// expanded, and only from this prefix, so a literal in prose is never one.
const PASTE_MARKER_HEAD: &str = "[Paste #";

/// How many lines a paste must reach before the screen offers the large-paste
/// menu (`paste.menuThreshold`): what a person pastes on purpose — a log, a
/// stack trace, a file — rather than a line or two of prose. omp's
/// `paste.largeMenuThreshold` is 100 too, and its 0 turns the menu off, which
/// is what this number's 0 does.
pub(crate) const PASTE_MENU_AFTER: u32 = 100;

/// The line above the composer while a second press is owed, one per key: a
/// two-press exit names the key that confirms *it*.
const CTRL_C_HINT: &str = "ctrl-c again to quit";
const EXIT_HINT: &str = "press Enter again to quit";

// ---------------------------------------------------------------------------
// What `/hotkeys` lists
// ---------------------------------------------------------------------------
//
// One table, beside the `map_key` above that turns a crossterm event into a
// key and the `on_key` below that answers it. `/hotkeys` renders this table and
// nothing else, so the listing and the screen cannot be written twice: the test
// `every_key_the_screen_answers_is_named_in_the_hotkeys_listing` walks every
// key `map_key` can produce through every state `on_key` branches on, and a key
// the screen answers that this table does not name fails the suite.

/// A group of bindings, in the order `/hotkeys` prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HotkeyGroup {
    Composer,
    Lists,
    Transcript,
    Turn,
    Session,
}

impl HotkeyGroup {
    /// Every group, in listing order.
    pub(crate) const ALL: [HotkeyGroup; 5] = [
        HotkeyGroup::Composer,
        HotkeyGroup::Lists,
        HotkeyGroup::Transcript,
        HotkeyGroup::Turn,
        HotkeyGroup::Session,
    ];

    /// The heading this group prints under.
    pub(crate) fn title(self) -> &'static str {
        match self {
            HotkeyGroup::Composer => "composer",
            HotkeyGroup::Lists => "lists & pickers",
            HotkeyGroup::Transcript => "transcript & mouse",
            HotkeyGroup::Turn => "turn control",
            HotkeyGroup::Session => "session",
        }
    }
}

/// One row of `/hotkeys`.
pub(crate) struct Hotkey {
    pub(crate) group: HotkeyGroup,
    /// The keys as a person presses them, `·` between alternatives — the
    /// spelling the drift guard in `chat.rs` reads the listing back by.
    pub(crate) keys: &'static str,
    /// What the screen does with them.
    pub(crate) what: &'static str,
}

/// Every binding the live screen answers, grouped the way `/hotkeys` prints
/// them.
///
/// A row is here when `on_key` — or the state it hands the key to, an open list
/// or the approval prompt — answers it. Only keys a terminal can actually
/// produce are listed: `ctrl+d` is quit, never the half-page-down the mapper's
/// later arm would give it.
pub(crate) const HOTKEYS: &[Hotkey] = &[
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "any character",
        what: "type into the draft",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "backspace",
        what: "delete the character before the caret",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "alt+backspace · ctrl+w",
        what: "delete the word before the caret",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "enter",
        what: "send the draft; a highlighted command runs first",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "← · →",
        what: "move the caret a character",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "alt+← · alt+→",
        what: "move the caret a word",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "home · end · ctrl+a · ctrl+e",
        what: "the start and the end of the draft",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "delete",
        what: "delete the character after the caret",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "ctrl+u",
        what: "delete back to the start of the draft",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "esc",
        what: "clear the draft",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "esc esc",
        what: "rewind the last turn, on an empty draft",
    },
    Hotkey {
        group: HotkeyGroup::Composer,
        keys: "paste",
        what: "insert what was pasted; a long paste becomes a marker",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "↑ · ↓",
        what: "move the highlight of the open list",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "tab",
        what: "apply the highlighted row to the draft",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "enter",
        what: "run the highlighted command",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "esc",
        what: "hide the list and keep the draft; close a picker",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "any character",
        what: "narrow the list's query",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "backspace",
        what: "take the last character back out of the query",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "y · n",
        what: "allow or refuse the tool call waiting on you",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "space",
        what: "tick a row of a question that takes several",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "ctrl+r",
        what: "browse this session's own prompts",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "↑",
        what: "browse this session's own prompts, from an empty draft",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "alt+m",
        what: "pick a model",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "alt+f",
        what: "cycle what `/tree` shows: everything, no tool traffic, only yours",
    },
    Hotkey {
        group: HotkeyGroup::Lists,
        keys: "alt+a",
        what: "move the view through the live agents and back to the turn",
    },
    Hotkey {
        group: HotkeyGroup::Transcript,
        keys: "↑ · ↓",
        what: "scroll the transcript a row, with text in the draft",
    },
    Hotkey {
        group: HotkeyGroup::Transcript,
        keys: "page up · page down",
        what: "scroll the transcript a page",
    },
    Hotkey {
        group: HotkeyGroup::Transcript,
        keys: "drag",
        what: "select transcript text",
    },
    Hotkey {
        group: HotkeyGroup::Transcript,
        keys: "release",
        what: "copy the selection",
    },
    Hotkey {
        group: HotkeyGroup::Transcript,
        keys: "wheel",
        what: "scroll the transcript, or move an open list's highlight",
    },
    Hotkey {
        group: HotkeyGroup::Turn,
        keys: "ctrl+c",
        what: "stop the running turn",
    },
    Hotkey {
        group: HotkeyGroup::Session,
        keys: "ctrl+x",
        what: "switch sessions",
    },
    Hotkey {
        group: HotkeyGroup::Session,
        keys: "ctrl+c ctrl+c",
        what: "quit — the same key twice inside 2 s",
    },
    Hotkey {
        group: HotkeyGroup::Session,
        keys: "ctrl+d",
        what: "quit when the draft is empty",
    },
];

/// The `/hotkeys` listing: one heading per group, one row per binding, the keys
/// padded into a column. The screen pushes each line as a note, the way
/// `/help` lists the commands.
pub(crate) fn hotkey_lines(extra: &[(&str, &str)]) -> Vec<String> {
    let room = HOTKEYS
        .iter()
        .map(|row| row.keys.chars().count())
        .chain(extra.iter().map(|(keys, _)| keys.chars().count()))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for group in HotkeyGroup::ALL {
        lines.push(format!("hotkeys · {}", group.title()));
        for row in HOTKEYS.iter().filter(|row| row.group == group) {
            lines.push(format!(
                "  {keys:<room$}  {what}",
                keys = row.keys,
                what = row.what
            ));
        }
    }
    // A mode that is off unless the config asks for it brings its own block:
    // the vim vocabulary, under its own heading, in the same column.
    if !extra.is_empty() {
        lines.push("hotkeys · vim (editor.vim)".to_owned());
        for (keys, what) in extra {
            lines.push(format!("  {keys:<room$}  {what}"));
        }
    }
    lines
}

impl Chat {
    // ---- Mouse selection -------------------------------------------------
    //
    // The transcript is the only surface with a selection: the composer has a
    // caret, the pickers have a cursor. A press anchors, a drag moves the
    // anchor's other corner, a release copies. Nothing here scrolls — a drag
    // that scrolled would move the text out from under the selection.

    /// Mouse press: anchor a drag-select at a screen cell.
    /// Mouse press. On a pinned agent's row it is a click on the jump list and
    /// nothing else — no selection starts there, because the strip is chrome,
    /// not transcript — which is why this returns what the screen should do.
    pub fn mouse_press(&mut self, x: u16, y: u16) -> Option<Applied> {
        if let Some(agent_id) = self.agent_at_row(y) {
            let agent_id = agent_id.to_owned();
            self.agent_focus = Some(agent_id.clone());
            return Some(Applied::effect(ChatEffect::Send(
                EngineCommand::FocusAgent {
                    agent_id: agent_id.into(),
                },
            )));
        }
        self.selection = Some(Selection::anchor(x, y));
        None
    }

    /// Mouse drag: move the selection's far corner.
    pub fn mouse_drag(&mut self, x: u16, y: u16) {
        if let Some(selection) = &mut self.selection {
            selection.drag(x, y);
        }
    }

    /// Mouse release: the selection stands, and its text is what was copied.
    ///
    /// Returns `None` for a click (an empty selection) so nothing reaches the
    /// clipboard on a press that selected nothing.
    pub fn mouse_release(&mut self) -> Option<String> {
        let selection = self.selection.as_mut()?;
        selection.release();
        let text = self.selection_text();
        (!text.is_empty()).then_some(text)
    }

    /// The wheel: a panel's cursor when one is open — the wheel moves it the
    /// way the arrows do — and the transcript's scroll otherwise. The wheel
    /// never *opens* anything: ↑ at an empty composer is the history's, and a
    /// wheel is not a key.
    pub fn mouse_wheel(&mut self, delta: isize, now: Instant) {
        if self.panel_open() {
            let key = if delta > 0 { Key::Up } else { Key::Down };
            self.on_key(key, now);
            return;
        }
        if delta > 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(delta as usize);
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(delta.unsigned_abs());
        }
    }

    /// Forget the selection — a key or a new press takes it away.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The committed selection, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// The selection as the transcript sees it: the screen rows translated to
    /// the transcript's own rows, and `None` when the selection never reached
    /// the transcript at all (`None` after a release is the same as a selection
    /// that holds no text).
    pub(crate) fn transcript_selection(&self) -> Option<Selection> {
        let selection = self.selection?;
        if !selection.is_non_empty() {
            return None;
        }
        let (_, top, _, bottom) = selection.rect()?;
        let origin = self.transcript_top;
        let last = origin.saturating_add(self.last_rows.len() as u16);
        if bottom < origin || top >= last {
            return None;
        }
        let shift = |y: u16| y.saturating_sub(origin);
        Some(Selection {
            anchor: (selection.anchor.0, shift(selection.anchor.1)),
            current: (selection.current.0, shift(selection.current.1)),
            active: selection.active,
        })
    }

    /// What a copy of the selection carries: the plain text of the selected
    /// columns, with no styling and no padding.
    pub fn selection_text(&self) -> String {
        match self.transcript_selection() {
            Some(selection) => selection.text(&self.last_rows),
            None => String::new(),
        }
    }

    pub fn on_key(&mut self, key: Key, now: Instant) -> Applied {
        // The next key takes a standing selection away, the way every terminal
        // does: the highlight is about the copy that just happened, not a mode.
        self.clear_selection();
        if self.approval.is_some() {
            return self.approval_key(key);
        }
        if self.pending_ask.is_some() {
            return self.ask_key(key);
        }
        if self.login_for.is_some() {
            return self.login_key(key);
        }
        if self.theme_picker.is_some() {
            return self.theme_picker_key(key, now);
        }
        if self.session_picker.is_some() {
            return self.session_picker_key(key, now);
        }
        if self.session_search.is_some() {
            return self.session_search_key(key, now);
        }
        if self.paste_menu.is_some() {
            return self.paste_menu_key(key, now);
        }
        if self.tree_picker.is_some() {
            return self.tree_picker_key(key, now);
        }
        if self.login_picker.is_some() {
            return self.login_picker_key(key, now);
        }
        if self.model_picker.is_some() {
            return self.model_picker_key(key, now);
        }
        if self.emoji_picker.is_visible() {
            return self.emoji_picker_key(key, now);
        }
        if self.history_picker.is_some() {
            return self.history_picker_key(key, now);
        }
        // The vim keys, when the mode is on: Normal's own vocabulary first —
        // it swallows printable keys — then Insert, which is the composer the
        // screen always had but for Esc. `None` is a key the mode does not
        // own, and it falls through to the match below.
        if let Some(applied) = self.vim_key(key) {
            return applied;
        }
        match key {
            Key::CtrlC if self.turn_active => {
                self.disarm();
                Applied::effect(ChatEffect::Send(EngineCommand::Cancel))
            }
            Key::CtrlC => self.arm_quit(now, CTRL_C_HINT),
            Key::CtrlR => self.open_history(),
            Key::CtrlX => self.open_session_picker(),
            Key::AltM => {
                self.open_model_picker();
                Applied::none()
            }
            // alt+f is the tree's own: with no tree open it does nothing
            // rather than opening one, so the key cannot surprise a composer.
            Key::AltF if self.tree_picker.is_some() => self.tree_picker_key(key, now),
            Key::AltF => Applied::none(),
            // alt+a walks the pinned agents and back to the main turn. Landing
            // on an agent asks the engine for its pane; the walk back to the
            // main turn is the screen's own, because the engine's `FocusAgent`
            // names an agent and has no form for "none".
            Key::AltA => {
                self.disarm();
                self.cycle_agent_focus();
                match self.agent_focus.clone() {
                    Some(id) => Applied::effect(ChatEffect::Send(EngineCommand::FocusAgent {
                        agent_id: id.into(),
                    })),
                    None => Applied::none(),
                }
            }
            Key::CtrlD if self.input.is_empty() => Applied::effect(ChatEffect::Quit),
            Key::Up if self.picking() => {
                self.move_picker(-1);
                Applied::none()
            }
            Key::Down if self.picking() => {
                self.move_picker(1);
                Applied::none()
            }
            Key::Tab if self.picking() => {
                self.accept_picker();
                Applied::none()
            }
            Key::Enter => {
                // A space terminates an emoticon as it is typed; Enter is the
                // other terminator, so the line reaches the transcript as the
                // glyph rather than the keystrokes.
                self.expand_trailing_emoticon();
                let token = slash_token(&self.input, self.caret())
                    .map(|token| (token.start, token.name.to_owned()));
                if let Some((start, name)) = token {
                    let at_line_start = self.input[..start].trim().is_empty();
                    if at_line_start && name.is_empty() {
                        return Applied::none();
                    }
                    // The row the arrows are on is the choice, not the word
                    // under the caret: a word that names a row as well as
                    // starting others (`/checkpoint` beside `/checkpoints`)
                    // must not shadow the highlighted one. A word that is
                    // already the highlighted row is still sent as typed, so
                    // one Enter keeps running a complete command — and keeps
                    // sending a sentence that names a skill exactly.
                    let rows = picker_rows(self);
                    let chosen_is_typed = rows
                        .get(self.picker % rows.len().max(1))
                        .is_some_and(|row| self.row_name(row) == name.as_str());
                    if !rows.is_empty() && !chosen_is_typed {
                        self.accept_picker();
                        // Mid-sentence the message is not finished: complete
                        // the token and let the next Enter send it.
                        if !at_line_start {
                            return Applied::none();
                        }
                    }
                }
                self.submit(now)
            }
            Key::DeleteWord => {
                self.disarm();
                self.delete_word();
                self.picker_hidden = false;
                self.sync_emoji_picker();
                Applied::none()
            }
            // The caret's keys, in the free composer: a list or a picker
            // takes them first (the arrows move its highlight), so what is
            // left here is a person editing the draft.
            Key::Left => {
                self.disarm();
                self.move_caret(-1);
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::Right => {
                self.disarm();
                self.move_caret(1);
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::WordLeft => {
                self.disarm();
                self.move_caret_word(-1);
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::WordRight => {
                self.disarm();
                self.move_caret_word(1);
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::Home => {
                self.disarm();
                self.caret_to_start();
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::End => {
                self.disarm();
                self.caret_to_end();
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::Delete => {
                self.disarm();
                self.delete_forward();
                self.sync_emoji_picker();
                self.picker = 0;
                self.picker_hidden = false;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::DeleteToStart => {
                self.disarm();
                self.delete_to_start();
                self.sync_emoji_picker();
                self.picker = 0;
                self.picker_hidden = false;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Backspace => {
                self.disarm();
                self.backspace();
                // The query may still stand after the pop (`:sm` from `:smi`),
                // so the picker follows the text here too.
                self.sync_emoji_picker();
                self.picker = 0;
                self.picker_hidden = false;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Char(ch) => {
                self.disarm();
                self.type_char(ch);
                self.picker = 0;
                self.picker_hidden = false;
                self.scroll_offset = 0;
                Applied::none()
            }
            // Esc on an open list closes it the way the emoji picker's does:
            // the draft stays exactly as typed, and the next keystroke offers
            // the list again. On a draft with no list up it still clears, and
            // on an empty composer the second press is the rewind chord.
            Key::Esc if self.picking() => {
                self.picker_hidden = true;
                self.disarm();
                Applied::none()
            }
            // An agent's pane is Esc's next stop: back to the main turn.
            Key::Esc if self.agent_focus.is_some() => {
                self.agent_focus = None;
                self.disarm();
                Applied::none()
            }
            Key::Esc => self.escape(now),
            // ↑ at an empty composer is the prompt history's (omp
            // `app.history.search`); with text in the composer it scrolls the
            // transcript, as it always has.
            Key::Up if self.input.is_empty() => self.open_history(),
            Key::Up => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                Applied::none()
            }
            Key::Down => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                Applied::none()
            }
            Key::PageUp => {
                let h = self.last_transcript_height;
                self.scroll_offset = self.scroll_offset.saturating_add(h.saturating_sub(1));
                Applied::none()
            }
            Key::PageDown => {
                let h = self.last_transcript_height;
                self.scroll_offset = self.scroll_offset.saturating_sub(h.saturating_sub(1));
                Applied::none()
            }
            Key::PageUpHalf => {
                let h = self.last_transcript_height / 2;
                self.scroll_offset = self.scroll_offset.saturating_add(h.max(1));
                Applied::none()
            }
            Key::PageDownHalf => {
                let h = self.last_transcript_height / 2;
                self.scroll_offset = self.scroll_offset.saturating_sub(h.max(1));
                Applied::none()
            }
            Key::CtrlD | Key::Tab => {
                self.disarm();
                Applied::none()
            }
        }
    }

    /// The keys a question the model asked answers to.
    ///
    /// Two shapes in one prompt. Until the user types, the panel is the picker:
    /// arrows move, Enter takes the row (for a question that takes several,
    /// Space ticks rows and Enter sends the set), Esc cancels. A printable
    /// character starts answering in the composer instead — whatever
    /// `free_text` said, a question with no list has nothing else to say with —
    /// and from there Enter sends the words and Esc cancels. Ctrl+C interrupts
    /// the turn, exactly as it does over an approval: the engine answers
    /// `Cancelled` to a question whose turn is gone.
    fn ask_key(&mut self, key: Key) -> Applied {
        let Some(ask) = self.pending_ask.clone() else {
            return Applied::none();
        };
        // One keystroke's hint, and no more: whatever this key says, the next
        // one starts with an empty line above the composer.
        self.disarm();
        if ask.answering() {
            return match key {
                Key::Enter => {
                    let text = self.input.trim().to_owned();
                    if text.is_empty() {
                        Applied::none()
                    } else {
                        self.answer_ask(titi_tools::AskAnswer::Text(text))
                    }
                }
                Key::Backspace => {
                    self.backspace();
                    Applied::none()
                }
                Key::Char(ch) if !ch.is_control() => {
                    self.insert_at_caret(&ch.to_string());
                    Applied::none()
                }
                Key::Esc => self.answer_ask(titi_tools::AskAnswer::Cancelled),
                Key::CtrlC => self.cancel_ask(),
                _ => Applied::none(),
            };
        }
        match key {
            Key::Up => {
                self.move_ask(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_ask(1);
                Applied::none()
            }
            Key::Char(' ') if ask.multi => {
                self.toggle_ask();
                Applied::none()
            }
            Key::Enter if ask.multi => {
                let chosen = ask.ticked();
                if chosen.is_empty() {
                    // An empty set is not an answer: the engine's `ask` tool
                    // was told what the user picked, and it picked nothing.
                    self.set_hint("pick at least one, or type your own".to_owned());
                    Applied::none()
                } else {
                    self.answer_ask(titi_tools::AskAnswer::Chosen(chosen))
                }
            }
            Key::Enter => {
                let Some(option) = ask.options.get(ask.selected).cloned() else {
                    return Applied::none();
                };
                self.answer_ask(titi_tools::AskAnswer::Chosen(vec![option]))
            }
            Key::Esc => self.answer_ask(titi_tools::AskAnswer::Cancelled),
            // A character starts the answer in the composer: the list the model
            // wrote cannot know it holds what the user means.
            Key::Char(ch) if !ch.is_control() && ask.free_text => {
                if let Some(pending) = self.pending_ask.as_mut() {
                    pending.typing = true;
                }
                self.insert_at_caret(&ch.to_string());
                Applied::none()
            }
            Key::CtrlC => self.cancel_ask(),
            _ => Applied::none(),
        }
    }

    /// Moves the cursor over the question's rows, wrapping.
    fn move_ask(&mut self, delta: isize) {
        let Some(ask) = self.pending_ask.as_mut() else {
            return;
        };
        let len = ask.options.len();
        if len == 0 {
            return;
        }
        let current = ask.selected % len;
        ask.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Ticks or unticks the row the cursor is on, for a question that takes
    /// several.
    fn toggle_ask(&mut self) {
        let Some(ask) = self.pending_ask.as_mut() else {
            return;
        };
        let at = ask.selected;
        if let Some(ticked) = ask.chosen.get_mut(at) {
            *ticked = !*ticked;
        }
    }

    /// Interrupts the turn a question belongs to. The engine answers
    /// `Cancelled` to the question itself, so nothing is answered here.
    fn cancel_ask(&mut self) -> Applied {
        self.pending_ask = None;
        self.disarm();
        Applied::effect(ChatEffect::Send(EngineCommand::Cancel))
    }

    /// Answers the question and says so on screen: the answer goes to the
    /// engine, which is waiting on this request id, and the transcript keeps
    /// the line so scrollback shows what was decided.
    fn answer_ask(&mut self, answer: titi_tools::AskAnswer) -> Applied {
        let Some(ask) = self.pending_ask.take() else {
            return Applied::none();
        };
        self.clear_input();
        let said = match &answer {
            titi_tools::AskAnswer::Chosen(chosen) => format!("ask · chose {}", chosen.join(" · ")),
            titi_tools::AskAnswer::Text(text) => format!("ask · answered {text}"),
            titi_tools::AskAnswer::Cancelled => "ask · cancelled".to_owned(),
        };
        self.push(LineKind::Note, said);
        Applied::send(
            EngineCommand::AnswerAsk {
                request_id: ask.request_id.into(),
                answer,
            },
            None,
        )
    }

    /// Insert pasted text into the composer. A paste is usually code or a
    /// log, so its line breaks and tabs are kept — a `\r\n` or lone `\r`
    /// becomes `\n` — and every other control character is dropped.
    ///
    /// More than [`PASTE_INLINE_MAX_LINES`] lines do not go into the draft at
    /// all: the draft takes a one-line marker and the body is kept aside, so a
    /// pasted stack trace cannot read as the prompt the user is writing (and
    /// the one-row composer can show the whole draft). [`Chat::submit`] swaps
    /// the two, and the transcript echoes the marker rather than the wall.
    pub fn paste(&mut self, text: &str) {
        if self.approval.is_some() {
            return;
        }
        // A paste while a question waits is an answer typed the long way: it
        // goes into the composer's row, with no marker and no menu, because the
        // row is the answer field and not a draft.
        if self.pending_ask.is_some() {
            if let Some(ask) = self.pending_ask.as_mut() {
                ask.typing = true;
            }
            let body = paste_body(text);
            self.insert_at_caret(&body);
            return;
        }
        self.disarm();
        // A pasted body is composer input, not a picker keystroke.
        self.login_picker = None;
        self.model_picker = None;
        self.emoji_picker.hide();
        self.picker_hidden = false;
        let body = paste_body(text);
        let lines = body.lines().count();
        if lines <= PASTE_INLINE_MAX_LINES {
            self.insert_at_caret(&body);
            return;
        }
        self.next_paste += 1;
        let marker = paste_marker(self.next_paste, lines);
        self.pastes.insert(marker.clone(), body);
        self.insert_at_caret(&marker);
        // Long enough to be worth a choice, so offer one. The marker is already
        // staged above: whatever the menu does, or does not do, the paste is
        // where a short one would have left it.
        if self.paste_menu_after > 0
            && lines as u32 >= self.paste_menu_after
            // A panel already on screen has the keys, so an offer here could
            // not be answered: the paste keeps its marker, which is what a
            // paste does while any picker is up.
            && !self.panel_open()
        {
            self.paste_menu = Some(PasteMenu {
                marker,
                seq: self.next_paste,
                lines,
                selected: 0,
            });
        }
    }

    /// Attach the staged paste as a fenced block: the marker stays in the draft
    /// and what it stands for at send becomes the fenced body, so the model
    /// reads a log as a block of text rather than as prose around it.
    pub(crate) fn attach_paste_as_block(&mut self, menu: &PasteMenu) -> Applied {
        let Some(body) = self.pastes.get(&menu.marker) else {
            return Applied::none();
        };
        // One newline before the closing fence, wherever the paste ended: a
        // fence that does not start a line is not one.
        let fenced = format!("```\n{}\n```", body.trim_end_matches('\n'));
        self.pastes.insert(menu.marker.clone(), fenced);
        self.push(
            LineKind::Note,
            format!("paste: {} lines will be sent as a fenced block", menu.lines),
        );
        Applied::none()
    }

    /// Attach the staged paste as a file under the workspace, and leave its
    /// path where the marker was: the model reads it when it needs it — in
    /// ranges, if it is long — instead of carrying the whole body in every
    /// request. Nothing is written unless the row was taken.
    pub(crate) fn attach_paste_as_file(&mut self, menu: &PasteMenu) -> Applied {
        let Some(body) = self.pastes.get(&menu.marker).cloned() else {
            return Applied::none();
        };
        match crate::session_fs::write_paste(&self.workspace, menu.seq, &body) {
            Ok(path) => {
                if let Some(at) = self.input.find(&menu.marker) {
                    let end = at + menu.marker.len();
                    let was = self.caret();
                    self.input.replace_range(at..end, &path);
                    // The menu was just up, so the caret is at or after the
                    // marker: it follows what replaced it.
                    self.set_caret(if was >= end {
                        at + path.len()
                    } else {
                        was.min(at)
                    });
                }
                self.pastes.remove(&menu.marker);
                self.push(
                    LineKind::Note,
                    format!("paste: wrote {path} ({} lines)", menu.lines),
                );
                Applied::none()
            }
            Err(reason) => {
                // The marker is still in the draft and still registered: a
                // workspace that cannot be written leaves the paste exactly
                // where a short one would be.
                self.push(LineKind::Error, format!("paste: not written ({reason})"));
                Applied::none()
            }
        }
    }

    /// The draft as it will be sent: every marker this draft holds replaced by
    /// the body it stands for.
    ///
    /// A marker is expanded only where the registry has it, and a substituted
    /// body is never scanned again, so text that merely looks like a marker —
    /// or a marker left over from a draft the user has moved on from — stays
    /// literal and can never ship a body from somewhere else.
    pub(crate) fn expand_pastes(&self, draft: &str) -> String {
        if self.pastes.is_empty() {
            return draft.to_owned();
        }
        let mut out = String::with_capacity(draft.len());
        let mut rest = draft;
        while let Some(at) = rest.find(PASTE_MARKER_HEAD) {
            out.push_str(&rest[..at]);
            let tail = &rest[at..];
            match self
                .pastes
                .iter()
                .find(|(marker, _)| tail.starts_with(marker.as_str()))
            {
                Some((marker, body)) => {
                    out.push_str(body);
                    rest = &tail[marker.len()..];
                }
                None => {
                    out.push_str(PASTE_MARKER_HEAD);
                    rest = &tail[PASTE_MARKER_HEAD.len()..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    fn approval_key(&mut self, key: Key) -> Applied {
        let Some(pending) = self.approval.clone() else {
            return Applied::none();
        };
        match key {
            Key::Char('y') | Key::Char('Y') | Key::Enter => {
                self.approval = None;
                Applied::effect(ChatEffect::Send(EngineCommand::ApproveTool {
                    call_id: pending.call_id.into(),
                    approved: true,
                }))
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                self.approval = None;
                Applied::effect(ChatEffect::Send(EngineCommand::ApproveTool {
                    call_id: pending.call_id.into(),
                    approved: false,
                }))
            }
            Key::CtrlC => {
                self.approval = None;
                self.disarm();
                Applied::effect(ChatEffect::Send(EngineCommand::Cancel))
            }
            _ => Applied::none(),
        }
    }

    /// Esc. On a draft it clears the composer, as it always has; on an empty
    /// composer a second press inside [`QUIT_WINDOW`] is the rewind chord
    /// (omp `doubleEscapeAction`, default `rewind`), which is exactly what
    /// `/rewind` does, so the chord and the command cannot drift.
    ///
    /// A command list that is open is not this function's: `on_key` intercepts
    /// that Esc and closes the list, leaving the draft as typed.
    fn escape(&mut self, now: Instant) -> Applied {
        let armed = self.esc_armed.take();
        self.disarm();
        if !self.input.is_empty() {
            self.clear_input();
            self.picker = 0;
            return Applied::none();
        }
        if armed.is_some_and(|at| now.saturating_duration_since(at) <= QUIT_WINDOW) {
            return self.rewind("");
        }
        self.esc_armed = Some(now);
        Applied::none()
    }

    fn arm_quit(&mut self, now: Instant, hint: &str) -> Applied {
        if let Some(armed) = self.quit_armed
            && now.saturating_duration_since(armed) <= QUIT_WINDOW
        {
            return Applied::effect(ChatEffect::Quit);
        }
        self.quit_armed = Some(now);
        self.hint = hint.to_owned();
        Applied::none()
    }

    /// Leaving from the composer: the bare word `exit`/`quit`/`q`, or
    /// `/exit`/`/quit` (omp `input.bareExitOnEmptySession`).
    ///
    /// A session with nothing in it has nothing to keep, so the first word
    /// leaves; once a turn is on the screen — finished or in flight — the
    /// same word only arms the exit, and a second press inside
    /// [`QUIT_WINDOW`] leaves. The window is the one Ctrl+C uses: one shape
    /// for "press it twice to be sure", so both keys confirm the same intent.
    pub(crate) fn exit_word(&mut self, now: Instant) -> Applied {
        if !self.has_conversation() {
            return Applied::effect(ChatEffect::Quit);
        }
        self.arm_quit(now, EXIT_HINT)
    }

    /// One reveal frame: how much of the running answer is on screen now.
    ///
    /// Off, this does nothing at all — the whole buffer is what the renderer
    /// draws, exactly as before the key existed. On, the pure core decides,
    /// with the *frames* read out of the time since the last one, so a tick
    /// that arrives late reveals as much as it owes.
    pub(crate) fn reveal_tick(&mut self, now: Instant) {
        if !self.smooth || !self.turn_active {
            return;
        }
        let elapsed = match self.reveal_at {
            Some(at) => now.saturating_duration_since(at),
            None => crate::reveal::FRAME,
        };
        self.reveal_at = Some(now);
        self.revealed = crate::reveal::reveal(&self.reply, self.revealed, elapsed);
    }

    /// The text of the answer as the screen may draw it: the revealed prefix
    /// while the key is on and the answer is still arriving, and the whole text
    /// otherwise. Always cut at a character boundary.
    pub(crate) fn revealed_prefix<'a>(&self, text: &'a str) -> &'a str {
        if !self.smooth || !self.turn_active {
            return text;
        }
        let at = crate::reveal::byte_at(text, self.revealed.min(text.chars().count()));
        &text[..at]
    }

    /// Everything arrives at once: a tool call closes the answer's line, and
    /// the turn's end, a failure and a cancel all mean there is nothing left
    /// to pace. Nothing is ever left unrevealed past one of them.
    pub(crate) fn reveal_all(&mut self) {
        self.revealed = self.reply.chars().count();
        self.reveal_at = None;
    }

    /// An agent that reached the end of its life leaves the strip — and takes
    /// the view with it when its pane had it, so the screen never shows a pane
    /// that is gone.
    pub(crate) fn forget_agent(&mut self, agent_id: &str) {
        self.agents.retain(|agent| agent.id != agent_id);
        if self.agent_focus.as_deref() == Some(agent_id) {
            self.agent_focus = None;
        }
    }

    /// The agent whose pane has the view, if one does and it is still live.
    pub(crate) fn focused_agent(&self) -> Option<&PinnedAgent> {
        let id = self.agent_focus.as_deref()?;
        self.agents.iter().find(|agent| agent.id == id)
    }

    /// The agent a screen row belongs to, for a click on the strip: `row` is a
    /// screen row and the strip knows where it starts.
    pub(crate) fn agent_at_row(&self, row: u16) -> Option<&str> {
        let offset = row.checked_sub(self.pinned_top)?;
        if offset >= self.pinned_rows {
            return None;
        }
        self.agents
            .get(offset as usize)
            .map(|agent| agent.id.as_str())
    }

    /// The next focus in the jump list: the main turn, then each live agent in
    /// start order, back round to the main turn.
    pub(crate) fn cycle_agent_focus(&mut self) {
        if self.agents.is_empty() {
            self.agent_focus = None;
            return;
        }
        self.agent_focus = match self.agent_focus.as_deref() {
            None => Some(self.agents[0].id.clone()),
            Some(current) => {
                let at = self.agents.iter().position(|agent| agent.id == current);
                match at {
                    Some(at) if at + 1 < self.agents.len() => Some(self.agents[at + 1].id.clone()),
                    _ => None,
                }
            }
        };
    }

    /// Whether the screen holds something a leave would give up: a finished
    /// turn's lines, or one still in flight.
    fn has_conversation(&self) -> bool {
        self.turn_active
            || self
                .lines
                .iter()
                .any(|line| matches!(line.kind, LineKind::User | LineKind::Assistant))
    }

    pub(crate) fn disarm(&mut self) {
        self.quit_armed = None;
        self.esc_armed = None;
        self.hint.clear();
    }
}

/// A pasted body as the composer keeps it: `\r\n` and a lone `\r` become `\n`,
/// tabs and newlines are kept — a paste is usually code or a log, so its line
/// breaks and indentation are part of it — and every other control character
/// is dropped.
fn paste_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(ch),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// The one-line stand-in a collapsed paste leaves in the draft:
/// `[Paste #2 · 14 lines]`. One line, so the one-row composer can show the
/// whole draft, and bracketed so it cannot read as prose the user typed.
fn paste_marker(seq: u32, lines: usize) -> String {
    format!("[Paste #{seq} · {lines} lines]")
}

pub(crate) fn map_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char('c') if control => Some(Key::CtrlC),
        KeyCode::Char('d') if control => Some(Key::CtrlD),
        // The crate's table binds `app.session.switch` to ctrl+x and
        // `app.model.select` to alt+m; a live screen that drops the modifier
        // leaves both chords with nothing to reach.
        KeyCode::Char('x') if control => Some(Key::CtrlX),
        KeyCode::Char('m') if modifiers.contains(KeyModifiers::ALT) => Some(Key::AltM),
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::ALT) => Some(Key::AltF),
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::ALT) => Some(Key::AltA),
        KeyCode::Char('r') if control => Some(Key::CtrlR),
        KeyCode::Char('w') if control => Some(Key::DeleteWord),
        // The two ends of the keyboard's own word delete: the macOS chord and
        // the readline one. Both are a word at a time, which the composer
        // otherwise cannot do — backspace takes exactly one character.
        KeyCode::Backspace if modifiers.contains(KeyModifiers::ALT) => Some(Key::DeleteWord),
        KeyCode::Char(ch) if !control && !modifiers.contains(KeyModifiers::ALT) => {
            Some(Key::Char(ch))
        }
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        // The caret's own keys, as the crate's keybinding table names them
        // (`tui.editor.cursor*`, `deleteCharForward`, `deleteToLineStart`):
        // word motions on alt and ctrl (terminals disagree about which they
        // send), the line's ends on home/end and ctrl+a/ctrl+e, and ctrl+u for
        // everything before the caret.
        KeyCode::Left if alt || control => Some(Key::WordLeft),
        KeyCode::Right if alt || control => Some(Key::WordRight),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        KeyCode::Delete => Some(Key::Delete),
        KeyCode::Char('a') if control => Some(Key::Home),
        KeyCode::Char('e') if control => Some(Key::End),
        KeyCode::Char('u') if control => Some(Key::DeleteToStart),
        KeyCode::Char('d') if control => Some(Key::PageDownHalf),
        _ => None,
    }
}

/// The OS clipboard writers this build knows, in the order they are asked for.
///
/// The same three the old `App` read the clipboard with, so a copy lands in
/// the one clipboard a terminal, a browser and an editor all share.
const CLIPBOARD_WRITERS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
];

/// The file `bin` would be run from, if one is on `path`.
///
/// The capability check for the OS clipboard: a build with no `pbcopy` and no
/// selection tool falls back to OSC 52 rather than spawning a program that is
/// not there. `path` is passed in (not read from the environment here) so the
/// check is a pure function of what it is given.
pub(crate) fn executable_path(bin: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(bin))
        .find(|candidate| candidate.is_file())
}

/// The first OS clipboard writer on `path`, resolved to the file that will be
/// run — never the bare name, so the check and the spawn cannot disagree about
/// which `pbcopy` answered.
pub(crate) fn clipboard_writer(
    path: &str,
) -> Option<(&'static str, &'static [&'static str], PathBuf)> {
    CLIPBOARD_WRITERS
        .iter()
        .find_map(|(bin, args)| executable_path(bin, path).map(|program| (*bin, *args, program)))
}

/// Hand `text` to the OS clipboard writer at `program`.
///
/// The child's stdin is taken and dropped before the wait: leaving the pipe
/// open would leave `pbcopy` waiting for an end of input that never comes.
pub(crate) fn write_to_clipboard(program: &Path, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().ok_or_else(|| "no stdin".to_owned())?;
    stdin
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    drop(stdin);
    let status = child.wait().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {status}", program.display()))
    }
}

/// Put a copied selection where the user can paste it, and say where.
///
/// The OS writer when this machine has one; otherwise OSC 52, which the
/// terminal itself puts on the clipboard the user is at — the route that works
/// over SSH. A copy with no route at all still names itself, so a screen that
/// copied nothing does not look like one that did. `path` is the search path
/// the OS writer is looked for on.
pub(crate) fn copy_selection(chat: &mut Chat, text: &str, path: &str) {
    let chars = text.chars().count();
    let route = match clipboard_writer(path) {
        Some((bin, args, program)) => match write_to_clipboard(&program, args, text) {
            Ok(()) => format!("copied {chars} chars · {bin}"),
            Err(reason) => {
                chat.output_flush
                    .push_str(&titi_tui::caps::osc52_copy(text));
                format!("copied {chars} chars · OSC 52 ({reason})")
            }
        },
        None => {
            chat.output_flush
                .push_str(&titi_tui::caps::osc52_copy(text));
            format!("copied {chars} chars · OSC 52")
        }
    };
    chat.set_hint(route);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_q_follow_up() {
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::Char('q'), KeyModifiers::CONTROL)).as_deref(),
            Some("ctrl+q")
        );
    }

    #[test]
    fn ctrl_enter() {
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::Enter, KeyModifiers::CONTROL)).as_deref(),
            Some("ctrl+enter")
        );
    }

    #[test]
    fn alt_up_and_shift_up() {
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::Up, KeyModifiers::ALT)).as_deref(),
            Some("alt+up")
        );
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::Up, KeyModifiers::SHIFT)).as_deref(),
            Some("shift+up")
        );
    }

    #[test]
    fn alt_m_model_select() {
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::Char('m'), KeyModifiers::ALT)).as_deref(),
            Some("alt+m")
        );
    }

    #[test]
    fn shift_tab() {
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::BackTab, KeyModifiers::SHIFT)).as_deref(),
            Some("shift+tab")
        );
        assert_eq!(
            canonical_from_key_event(&key(KeyCode::BackTab, KeyModifiers::NONE)).as_deref(),
            Some("shift+tab")
        );
    }

    #[test]
    fn release_is_ignored() {
        let mut ev = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        ev.kind = KeyEventKind::Release;
        assert_eq!(canonical_from_key_event(&ev), None);
    }

    #[test]
    fn overlay_printable_reaches_filter() {
        assert_eq!(
            overlay_key_data(&key(KeyCode::Char('g'), KeyModifiers::NONE)).as_deref(),
            Some("g")
        );
        let bs = overlay_key_data(&key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(bs.as_deref(), Some(""));
        assert_eq!(
            overlay_key_data(&key(KeyCode::Tab, KeyModifiers::NONE)).as_deref(),
            Some("\t")
        );
        assert_eq!(
            overlay_key_data(&key(KeyCode::Left, KeyModifiers::NONE)).as_deref(),
            Some("\x1b[D")
        );
    }
}
