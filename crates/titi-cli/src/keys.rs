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

/// The line above the composer while a second press is owed, one per key: a
/// two-press exit names the key that confirms *it*.
const CTRL_C_HINT: &str = "ctrl-c again to quit";
const EXIT_HINT: &str = "press Enter again to quit";

impl Chat {
    // ---- Mouse selection -------------------------------------------------
    //
    // The transcript is the only surface with a selection: the composer has a
    // caret, the pickers have a cursor. A press anchors, a drag moves the
    // anchor's other corner, a release copies. Nothing here scrolls — a drag
    // that scrolled would move the text out from under the selection.

    /// Mouse press: anchor a drag-select at a screen cell.
    pub fn mouse_press(&mut self, x: u16, y: u16) {
        self.selection = Some(Selection::anchor(x, y));
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
                let token = slash_token(&self.input).map(|(start, name)| (start, name.to_owned()));
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
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::Backspace => {
                self.disarm();
                self.input.pop();
                // The query may still stand after the pop (`:sm` from `:smi`),
                // so the picker follows the text here too.
                self.sync_emoji_picker();
                self.picker = 0;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Char(ch) => {
                self.disarm();
                self.type_char(ch);
                self.picker = 0;
                self.scroll_offset = 0;
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
        self.disarm();
        // A pasted body is composer input, not a picker keystroke.
        self.login_picker = None;
        self.model_picker = None;
        self.emoji_picker.hide();
        let body = paste_body(text);
        let lines = body.lines().count();
        if lines <= PASTE_INLINE_MAX_LINES {
            self.input.push_str(&body);
            return;
        }
        self.next_paste += 1;
        let marker = paste_marker(self.next_paste, lines);
        self.pastes.insert(marker.clone(), body);
        self.input.push_str(&marker);
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
    match code {
        KeyCode::Char('c') if control => Some(Key::CtrlC),
        KeyCode::Char('d') if control => Some(Key::CtrlD),
        // The crate's table binds `app.session.switch` to ctrl+x and
        // `app.model.select` to alt+m; a live screen that drops the modifier
        // leaves both chords with nothing to reach.
        KeyCode::Char('x') if control => Some(Key::CtrlX),
        KeyCode::Char('m') if modifiers.contains(KeyModifiers::ALT) => Some(Key::AltM),
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
        KeyCode::Char('u') if control => Some(Key::PageUpHalf),
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
