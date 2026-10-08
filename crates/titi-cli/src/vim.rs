//! The vim-style editor: the mode, its motions and its edits.
//!
//! Off unless `editor.vim` asks for it: [`Chat::vim`] is `None` then and every
//! path here is unreachable, so the screen answers keys exactly as it did
//! before the mode existed. On, the composer has two modes. Insert is the
//! composer the screen always had, with one exception — Esc leaves it and the
//! caret steps back one, the way vim's does. Normal has vim's vocabulary over
//! the draft: the motions, the operators built from them, counts, and the
//! named keys a terminal's arrows, home/end, backspace and delete stand for.
//!
//! The draft is one line — the composer is one row, and a paste keeps its line
//! breaks as `↵` inside it — so `j`/`k`, `gg`/`G`, visual modes, text objects,
//! registers and `p`/`P` have no subject here and are not ported. `dd`, `D`,
//! `cc`, `S` and `C` mean the whole draft or its tail, and `u` has nothing to
//! undo, because the composer keeps no undo stack. What is missing is listed
//! in `docs/research/omp-parity/GAP.md`.
//!
//! omp's `tui.vimMode` is the shape this follows (`pi-tui/src/vim.ts`): the
//! mode starts in Insert, Esc leaves Insert and steps the caret back one, a
//! quiet Normal Esc is handed back to the screen (its rewind chord), Normal
//! swallows printable keys rather than typing them, and the mode is shown in
//! the composer's own chrome.
//!
//! Every cut here goes through the composer's own marker rule
//! ([`Chat::whole_markers`]), so a paste marker crossed by a motion or an
//! operator is taken whole, exactly as `ctrl+w` and backspace take it.

use crate::chat::{Applied, Chat, Key};

/// The mode the draft is in. Off is not a mode: [`Chat::vim`] is `None` and
/// none of this runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum VimMode {
    /// The composer the screen always had.
    #[default]
    Insert,
    /// vim's vocabulary over the draft.
    Normal,
}

impl VimMode {
    /// The word the composer's border shows for it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            VimMode::Insert => "INSERT",
            VimMode::Normal => "NORMAL",
        }
    }
}

/// Where a Normal-mode motion takes the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    /// `h`/`l`: a character at a time, either way.
    Char(isize),
    /// `w`/`b`: the next word, the word before, by the draft's own word rule
    /// (whitespace-delimited, the one `ctrl+w` and `alt+←/→` use).
    Word(isize),
    /// `e`: the end of the word, which is where the caret rests (inclusive).
    WordEnd,
    /// `0`: the start of the draft.
    Start,
    /// `^`: the first character that is not a space.
    FirstNonBlank,
    /// `$`: the end of the draft.
    End,
}

impl Motion {
    /// Whether the caret lands *on* a character rather than before it: `e` is
    /// inclusive, so an operator with it takes the character it lands on.
    fn inclusive(self) -> bool {
        matches!(self, Motion::WordEnd)
    }
}

/// An operator waiting for a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operator {
    /// `d`: take the text out.
    Delete,
    /// `c`: take it out and open Insert where it was.
    Change,
}

/// Where Insert mode opens the caret when Normal hands it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertAt {
    /// `i`: where the caret is.
    Caret,
    /// `a`: after the character under the caret.
    After,
    /// `I`: at the first character that is not a space.
    FirstNonBlank,
    /// `A`: at the end of the draft.
    End,
}

/// What a Normal-mode key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VimCommand {
    /// A motion, alone: the caret moves.
    Motion(Motion),
    /// `d`/`c`: wait for a motion — or for a second `d`/`c`, which is the
    /// whole draft.
    Operator(Operator),
    /// The doubled operator: the whole draft (`dd`, `cc`).
    Draft(Operator),
    /// To the end of the draft (`D`, `C`).
    ToEnd(Operator),
    /// The character under the caret (`x`, `s`), and the ones after it when a
    /// count says so.
    Under(Operator),
    /// The character before the caret (`X`), and the ones before it with a
    /// count.
    Before,
    /// Leave for Insert mode.
    Insert(InsertAt),
}

/// The Normal-mode vocabulary: one table, so the dispatch and the listing
/// (`VIM_HOTKEYS`) cannot be written twice. A test holds the two to each
/// other: every key here is named in the listing, and the listing names
/// nothing but these keys and the count's digits.
pub(crate) const VIM_KEYS: &[(char, VimCommand)] = &[
    ('h', VimCommand::Motion(Motion::Char(-1))),
    ('l', VimCommand::Motion(Motion::Char(1))),
    ('w', VimCommand::Motion(Motion::Word(1))),
    ('b', VimCommand::Motion(Motion::Word(-1))),
    ('e', VimCommand::Motion(Motion::WordEnd)),
    ('0', VimCommand::Motion(Motion::Start)),
    ('^', VimCommand::Motion(Motion::FirstNonBlank)),
    ('$', VimCommand::Motion(Motion::End)),
    ('x', VimCommand::Under(Operator::Delete)),
    ('X', VimCommand::Before),
    ('d', VimCommand::Operator(Operator::Delete)),
    ('c', VimCommand::Operator(Operator::Change)),
    ('D', VimCommand::ToEnd(Operator::Delete)),
    ('C', VimCommand::ToEnd(Operator::Change)),
    ('s', VimCommand::Under(Operator::Change)),
    ('S', VimCommand::Draft(Operator::Change)),
    ('i', VimCommand::Insert(InsertAt::Caret)),
    ('a', VimCommand::Insert(InsertAt::After)),
    ('I', VimCommand::Insert(InsertAt::FirstNonBlank)),
    ('A', VimCommand::Insert(InsertAt::End)),
];

/// The vim rows of `/hotkeys`, printed only while the mode is on: a mode the
/// config did not ask for must not read as a binding the screen answers.
///
/// The keys are spelled the way a person presses them, `·` between
/// alternatives, the same column the rest of the listing uses.
pub(crate) const VIM_HOTKEYS: &[(&str, &str)] = &[
    ("h · l", "move the caret a character"),
    (
        "w · b · e",
        "the next word, the word before, the end of a word",
    ),
    ("0 · ^ · $", "the start, the first non-blank, the end"),
    (
        "3w · 2dw",
        "a count: the motion, or the operator's motion, that many times",
    ),
    (
        "x · X",
        "delete the character under the caret, or the one before it",
    ),
    (
        "d w · d b · d d",
        "delete through a motion; the whole draft for dd",
    ),
    (
        "D · C",
        "delete to the end of the draft; C opens Insert after it",
    ),
    (
        "c w · c c",
        "change through a motion; the whole draft for cc",
    ),
    (
        "s · S",
        "change the character under the caret, or the whole draft",
    ),
    (
        "i · a · I · A",
        "Insert at the caret, after it, at the first non-blank, at the end",
    ),
    (
        "esc",
        "leave Insert for Normal; a quiet Normal esc is the screen's own",
    ),
];

/// The state the mode keeps between keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct VimState {
    pub(crate) mode: VimMode,
    /// The count being typed (`3` in `3w`), as the number so far. `None` when
    /// no digit has been pressed since the last command.
    count: Option<u32>,
    /// The operator waiting for a motion (`d` in `dw`).
    operator: Option<Operator>,
}

/// The most a count may reach. A person typing digits forever is not asking
/// for four billion of anything, and the arithmetic below must not wrap.
const COUNT_MAX: u32 = 999;

impl VimState {
    /// The same mode with nothing half-typed: what every key that is not a
    /// count or an operator leaves behind.
    pub(crate) fn cleared(self) -> Self {
        Self {
            mode: self.mode,
            ..Self::default()
        }
    }

    /// Esc out of Insert: Normal, with nothing half-typed.
    fn leave_insert(self) -> Self {
        Self {
            mode: VimMode::Normal,
            ..Self::default()
        }
    }

    /// A digit of the count being typed.
    fn push_digit(self, digit: u32) -> Self {
        let count = self
            .count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit)
            .min(COUNT_MAX);
        Self {
            count: Some(count),
            ..self
        }
    }

    /// The operator waiting for a motion. The count typed before it stands:
    /// `2dw` and `d2w` are the same command in vim.
    fn with_operator(self, operator: Operator) -> Self {
        Self {
            operator: Some(operator),
            ..self
        }
    }
}

impl Chat {
    /// Which mode the draft is in, when the vim keys are on. `None` when the
    /// setting is off — the composer's own border reads this.
    pub(crate) fn vim_mode(&self) -> Option<VimMode> {
        self.vim.map(|state| state.mode)
    }

    /// One key, when the vim keys are on.
    ///
    /// `None` hands the key back to the screen, which answers it exactly as it
    /// did before the mode existed: that is every key in Insert (but Esc), and
    /// in Normal the app's own chords, Enter, and the keys the transcript
    /// scrolls with. omp's `VimState::handleKey` returns `null` for the same
    /// reason.
    pub(crate) fn vim_key(&mut self, key: Key) -> Option<Applied> {
        let state = self.vim?;
        // A list that is open owns Esc first: it hides the list and leaves
        // both the draft and the mode as they were. omp's editor does the same
        // for its autocomplete (`editor.ts`: `isShowingAutocomplete`).
        if key == Key::Esc && self.picking() {
            return None;
        }
        if state.mode == VimMode::Insert {
            if key != Key::Esc {
                return None;
            }
            // Esc leaves Insert for Normal, and the caret steps back one the
            // way vim's does — across a paste marker whole. The two-press
            // timers go: a mode change is not the first press of anything.
            self.disarm();
            self.move_caret(-1);
            self.sync_emoji_picker();
            self.vim = Some(state.leave_insert());
            return Some(Applied::none());
        }
        self.vim_normal(key, state)
    }

    /// One key in Normal mode. Normal answers every printable key — an unknown
    /// one is swallowed rather than typed, which is what vim does — and the
    /// named keys a terminal produces for vim's motions.
    fn vim_normal(&mut self, key: Key, state: VimState) -> Option<Applied> {
        let command = match key {
            // A digit is the count, except a leading `0`, which is the motion.
            Key::Char(ch) if ch.is_ascii_digit() && !(ch == '0' && state.count.is_none()) => {
                let digit = ch.to_digit(10).unwrap_or(0);
                self.vim = Some(state.push_digit(digit));
                return Some(Applied::none());
            }
            Key::Char(ch) => match VIM_KEYS.iter().find(|(key, _)| *key == ch) {
                Some((_, command)) => *command,
                None => {
                    self.vim = Some(state.cleared());
                    return Some(Applied::none());
                }
            },
            // The named keys vim reads as motions: the arrows are `h`/`l`,
            // home/end are `0`/`$`, backspace is `h` and delete is `x`
            // (omp's `VIM_NAV_KEYS`). ↑/↓ are not among them: they have no
            // line to move over in a one-row composer, and they keep scrolling
            // the transcript as they always have.
            Key::Left | Key::Backspace => VimCommand::Motion(Motion::Char(-1)),
            Key::Right => VimCommand::Motion(Motion::Char(1)),
            Key::Home => VimCommand::Motion(Motion::Start),
            Key::End => VimCommand::Motion(Motion::End),
            Key::Delete => VimCommand::Under(Operator::Delete),
            // Esc with something half-typed is the mode's: it cancels and
            // stays in Normal (omp's `#handleEscape`: `if (this.pending)`).
            // A quiet Normal Esc is not — it falls through to the screen,
            // whose clear and two-press rewind are its own.
            Key::Esc if state.count.is_some() || state.operator.is_some() => {
                self.vim = Some(state.cleared());
                return Some(Applied::none());
            }
            // Everything else — Enter, the app's chords, the transcript's own
            // scroll — is not the mode's.
            _ => {
                self.vim = Some(state.cleared());
                return None;
            }
        };
        Some(self.vim_command(command, state))
    }

    /// Run one command of the vocabulary.
    fn vim_command(&mut self, command: VimCommand, state: VimState) -> Applied {
        let count = state.count.unwrap_or(1).max(1) as usize;
        match command {
            VimCommand::Motion(motion) => {
                if let Some(operator) = state.operator {
                    // An operator with a motion: the range runs from the
                    // caret to where the motion lands. vim's `cw` quirk is
                    // here too — on a non-blank, `cw` changes to the end of
                    // the word (`ce`) rather than swallowing the space after
                    // it.
                    let motion = if operator == Operator::Change
                        && motion == Motion::Word(1)
                        && self.caret_is_on_word()
                    {
                        Motion::WordEnd
                    } else {
                        motion
                    };
                    let end = self.vim_target(motion, count);
                    return self.vim_operate(operator, self.caret(), end, motion.inclusive());
                }
                let at = self.vim_target(motion, count);
                self.set_caret(at);
                self.sync_emoji_picker();
                self.vim = Some(state.cleared());
                Applied::none()
            }
            VimCommand::Operator(operator) => {
                if state.operator == Some(operator) {
                    // The doubled operator: the whole draft.
                    return self.vim_operate(operator, 0, self.input.len(), true);
                }
                self.vim = Some(state.with_operator(operator));
                Applied::none()
            }
            // These ranges are already the characters to take, so `inclusive`
            // is not theirs to add: only a motion that rests *on* a character
            // (`e`) needs it.
            VimCommand::Draft(operator) => self.vim_operate(operator, 0, self.input.len(), false),
            VimCommand::ToEnd(operator) => {
                self.vim_operate(operator, self.caret(), self.input.len(), false)
            }
            VimCommand::Under(operator) => {
                let from = self.caret();
                let to = self.vim_target(Motion::Char(1), count);
                self.vim_operate(operator, from, to, false)
            }
            VimCommand::Before => {
                let to = self.caret();
                let from = self.vim_target(Motion::Char(-1), count);
                self.vim_operate(Operator::Delete, from, to, false)
            }
            VimCommand::Insert(at) => {
                let caret = match at {
                    InsertAt::Caret => self.caret(),
                    InsertAt::After => self.vim_target(Motion::Char(1), 1),
                    InsertAt::FirstNonBlank => self.vim_target(Motion::FirstNonBlank, 1),
                    InsertAt::End => self.input.len(),
                };
                self.set_caret(caret);
                self.vim = Some(VimState {
                    mode: VimMode::Insert,
                    ..VimState::default()
                });
                self.sync_emoji_picker();
                Applied::none()
            }
        }
    }

    /// One operator over `start..end`: the cut, marker-whole, then Insert when
    /// the operator changes rather than deletes.
    ///
    /// The caret lands at the start of what went, which is where vim leaves it
    /// — and where a change opens Insert.
    fn vim_operate(
        &mut self,
        operator: Operator,
        start: usize,
        end: usize,
        inclusive: bool,
    ) -> Applied {
        let (mut start, mut end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        // An inclusive motion (`e`) covers the character it lands on.
        if inclusive && end < self.input.len() {
            end = next_start(&self.input, end);
        }
        // A paste marker is one unit: a range that touches one takes all of
        // it, the same rule every other cut in the composer follows.
        (start, end) = self.whole_markers(start, end);
        if start < end {
            self.input.replace_range(start..end, "");
            self.forget_cut_markers();
        }
        self.set_caret(start);
        self.sync_emoji_picker();
        // A change opens Insert where the text was; a delete stays in Normal.
        // Either way the command is done, so nothing stays half-typed.
        self.vim = Some(match operator {
            Operator::Change => VimState {
                mode: VimMode::Insert,
                ..VimState::default()
            },
            Operator::Delete => VimState {
                mode: VimMode::Normal,
                ..VimState::default()
            },
        });
        Applied::none()
    }

    /// Whether the character under the caret is part of a word: what vim's
    /// `cw` quirk turns on.
    fn caret_is_on_word(&self) -> bool {
        self.input[self.caret()..]
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace())
    }

    /// Where a motion takes the caret: `count` of them, each from where the
    /// last one landed, with a paste marker crossed whole.
    fn vim_target(&self, motion: Motion, count: usize) -> usize {
        let mut at = self.caret();
        for _ in 0..count {
            at = self.vim_step(motion, at);
        }
        at
    }

    /// One step of a motion, from `at`.
    ///
    /// The walk is over the draft's characters — a paste marker's own spaces
    /// included, which is what makes a walk land inside one — and the offset
    /// is then moved off any marker it landed in: to the far edge in the
    /// direction of travel, or, for `e`, onto the marker's last character,
    /// because `e` rests *on* a character.
    fn vim_step(&self, motion: Motion, at: usize) -> usize {
        let input = self.input.as_str();
        let forward = match motion {
            Motion::Char(delta) => delta >= 0,
            Motion::Word(delta) => delta >= 0,
            Motion::WordEnd | Motion::End => true,
            Motion::Start | Motion::FirstNonBlank => false,
        };
        let landed = match motion {
            Motion::Char(delta) if delta < 0 => prev_start(input, at),
            Motion::Char(_) => next_start(input, at),
            Motion::Word(delta) if delta < 0 => word_back(input, at),
            Motion::Word(_) => word_forward(input, at),
            Motion::WordEnd => word_end(input, at),
            Motion::Start => 0,
            Motion::FirstNonBlank => first_non_blank(input),
            Motion::End => input.len(),
        };
        if motion.inclusive() {
            return self.marker_last_char(landed);
        }
        self.skip_markers(landed, forward)
    }

    /// The character a motion that rests *on* one would land on, when the
    /// offset it computed is inside a paste marker: the marker's last
    /// character, not the far edge (which is one past it).
    fn marker_last_char(&self, at: usize) -> usize {
        for (start, end) in self.marker_spans() {
            if at > start && at < end {
                return prev_start(&self.input, end);
            }
        }
        at
    }
}

/// The offset of the character before `at`.
fn prev_start(text: &str, at: usize) -> usize {
    text[..at]
        .char_indices()
        .next_back()
        .map(|(at, _)| at)
        .unwrap_or(0)
}

/// The offset of the character after `at`.
fn next_start(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .next()
        .map(|ch| at + ch.len_utf8())
        .unwrap_or(at)
}

/// The first character that is not a space, or the start when the draft is
/// nothing but spaces.
fn first_non_blank(text: &str) -> usize {
    text.char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(at, _)| at)
        .unwrap_or(0)
}

/// vim's `w`: the start of the next word. From a word, the rest of it is
/// crossed first, so `w` lands on the word after the one the caret is in.
fn word_forward(text: &str, at: usize) -> usize {
    let mut offset = at;
    while let Some(ch) = text[offset..].chars().next() {
        if ch.is_whitespace() {
            break;
        }
        offset += ch.len_utf8();
    }
    while let Some(ch) = text[offset..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        offset += ch.len_utf8();
    }
    offset
}

/// vim's `b`: the start of the word before.
fn word_back(text: &str, at: usize) -> usize {
    let mut offset = at;
    while let Some((i, ch)) = text[..offset].char_indices().next_back() {
        if !ch.is_whitespace() {
            break;
        }
        offset = i;
    }
    while let Some((i, ch)) = text[..offset].char_indices().next_back() {
        if ch.is_whitespace() {
            break;
        }
        offset = i;
    }
    offset
}

/// vim's `e`: the last character of the word the caret is about to run into,
/// or of the one it is already in when that word has more of it ahead.
fn word_end(text: &str, at: usize) -> usize {
    let mut offset = at;
    if text[offset..]
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_whitespace())
    {
        while let Some(ch) = text[offset..].chars().next() {
            if ch.is_whitespace() {
                break;
            }
            offset += ch.len_utf8();
        }
        // Already on the word's last character: `e` means the next word.
        if prev_start(text, offset) != at {
            return prev_start(text, offset);
        }
    }
    while let Some(ch) = text[offset..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        offset += ch.len_utf8();
    }
    let start = offset;
    while let Some(ch) = text[offset..].chars().next() {
        if ch.is_whitespace() {
            break;
        }
        offset += ch.len_utf8();
    }
    if offset > start {
        prev_start(text, offset)
    } else {
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// `w` from a word lands on the next word's first character, and from a
    /// gap on the word that starts it.
    #[test]
    fn word_forward_lands_on_the_next_word() {
        let text = "one two  three";
        assert_eq!(word_forward(text, 0), 4);
        assert_eq!(word_forward(text, 1), 4);
        assert_eq!(word_forward(text, 4), 9);
        assert_eq!(word_forward(text, 9), text.len());
    }

    /// `b` lands on the first character of the word the caret is in, and on
    /// the one before it from there — vim's own rule for both.
    #[test]
    fn word_back_lands_on_the_word_before() {
        let text = "one two  three";
        assert_eq!(word_back(text, 13), 9, "from the last word: its start");
        assert_eq!(word_back(text, 9), 4, "and then the one before it");
        assert_eq!(word_back(text, 6), 4, "from inside a word: its start");
        assert_eq!(word_back(text, 4), 0);
        assert_eq!(word_back(text, 0), 0);
    }

    /// `e` lands on the last character of the word: of the one the caret is
    /// in when it has more ahead, and of the next one when it is already at
    /// the end (or in a gap).
    #[test]
    fn word_end_lands_on_the_last_character() {
        let text = "one two";
        assert_eq!(word_end(text, 0), 2, "inside a word");
        assert_eq!(word_end(text, 2), 6, "at its end: the next word");
        assert_eq!(word_end(text, 3), 6, "in the gap");
        assert_eq!(word_end(text, 6), 6, "at the end of the draft");
    }

    /// The listing and the vocabulary are held to each other: every key the
    /// dispatch answers is named in the rows, and the rows name nothing but
    /// those keys and the digits a count is typed with.
    #[test]
    fn the_listing_names_exactly_the_vocabulary() {
        let named: BTreeSet<char> = VIM_HOTKEYS
            .iter()
            .flat_map(|(keys, _)| keys.chars())
            .filter(|ch| !ch.is_whitespace() && *ch != '·')
            .collect();
        let table: BTreeSet<char> = VIM_KEYS.iter().map(|(key, _)| *key).collect();
        for key in &table {
            assert!(named.contains(key), "{key} is not in the vim listing");
        }
        for key in &named {
            assert!(
                table.contains(key) || key.is_ascii_digit(),
                "{key} is listed but not a command"
            );
        }
    }

    /// The words a mode is shown by.
    #[test]
    fn a_mode_has_a_word() {
        assert_eq!(VimMode::Insert.label(), "INSERT");
        assert_eq!(VimMode::Normal.label(), "NORMAL");
    }
}
