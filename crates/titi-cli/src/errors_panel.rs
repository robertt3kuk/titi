//! The error log and its `/errors` panel.
//!
//! An error scrolls off the transcript like any other line, and the one that
//! mattered is usually the one that went by while the turn was still running.
//! So every error the screen shows is also kept here, newest last, up to
//! [`ERROR_LOG_CAP`] of them: what it said, where it came from and when.
//! `/errors` lists them newest first; Enter on a row shows the whole text
//! (the transcript row is one line cut short), Esc closes. The status line's
//! `⚠ N` counts the ones that arrived since the panel was last opened.
//!
//! Alt+R is not bound to this: the crate's keybinding table gives it to
//! `app.retry`, and a chord one table row names must not mean two things.

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::Instant;

use crate::chat::{Applied, Chat, Key};
use crate::composer::{one_line, wrap_plain};
use crate::pickers::{PanelLine, PanelView, panel_body, panel_label_room, panel_view};
use crate::transcript::{LineKind, TranscriptLine};
use crate::wallclock::Stamp;

/// Errors kept. The oldest go first: a long session's first failure is not
/// what anyone opens the panel for.
pub(crate) const ERROR_LOG_CAP: usize = 200;

/// Where an error came from, as the panel names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorSource {
    /// The engine reported a failure: the provider, the stream, a budget.
    Provider,
    /// A tool call ended in an error.
    Tool,
    /// The screen itself: a command that was refused, a file that was not
    /// written.
    Screen,
}

impl ErrorSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ErrorSource::Provider => "provider",
            ErrorSource::Tool => "tool",
            ErrorSource::Screen => "titi",
        }
    }
}

/// One error, as it was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorEntry {
    /// The local time of day, when the system will say it.
    pub(crate) at: Option<Stamp>,
    pub(crate) source: ErrorSource,
    /// The whole text. The transcript row cuts a long message to one line;
    /// this is what it was before the cut.
    pub(crate) text: String,
}

impl ErrorEntry {
    fn time(&self) -> String {
        match self.at {
            Some(stamp) => format!("{:02}:{:02}", stamp.hour, stamp.minute),
            None => "--:--".to_owned(),
        }
    }
}

/// The session's errors, oldest first, and how many arrived unseen.
#[derive(Debug, Default)]
pub(crate) struct ErrorLog {
    entries: VecDeque<ErrorEntry>,
    unseen: usize,
}

impl ErrorLog {
    pub(crate) fn record(&mut self, source: ErrorSource, text: String, at: Option<Stamp>) {
        if self.entries.len() == ERROR_LOG_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(ErrorEntry { at, source, text });
        self.unseen += 1;
    }

    /// Newest first: the order the panel lists them in.
    pub(crate) fn newest_first(&self) -> impl Iterator<Item = &ErrorEntry> {
        self.entries.iter().rev()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Errors that arrived since the panel was last opened: the status line's
    /// `⚠ N`.
    pub(crate) fn unseen(&self) -> usize {
        self.unseen
    }

    pub(crate) fn mark_seen(&mut self) {
        self.unseen = 0;
    }
}

/// The panel's cursor: a row of the list, or the text of one error.
#[derive(Debug, Clone)]
pub(crate) struct ErrorsPicker {
    /// The row the cursor is on, counted from the newest.
    pub(crate) selected: usize,
    /// Whether Enter has opened the selected error's whole text.
    pub(crate) detail: bool,
    /// The line of the open text the cursor is on.
    pub(crate) scroll: usize,
    /// Lines the open text wrapped to when it was last drawn, so the keys can
    /// stop at its end without knowing the pane's width.
    drawn_lines: Cell<usize>,
}

/// What the screen holds for the error log.
#[derive(Debug, Default)]
pub(crate) struct ErrorsState {
    pub(crate) log: ErrorLog,
    pub(crate) picker: Option<ErrorsPicker>,
}

impl Chat {
    /// Show an error in the transcript and keep it in the log.
    ///
    /// `shown` is the transcript row; `full` is what the log keeps, so a
    /// message the row had to cut is still readable in `/errors`.
    pub(crate) fn push_error(&mut self, source: ErrorSource, shown: String, full: &str) {
        let line = TranscriptLine::new(LineKind::Error, shown).stamped(&self.model);
        self.errors.log.record(source, full.to_owned(), line.at);
        self.lines.push(line);
        self.scroll_offset = 0;
    }

    /// `/errors`: open the panel over this session's errors.
    pub(crate) fn errors_command(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Note, format!("usage: /errors (got {args})"));
            return Applied::none();
        }
        if self.errors.log.is_empty() {
            self.push(LineKind::Note, "errors: none this session".to_owned());
            return Applied::none();
        }
        self.errors.picker = Some(ErrorsPicker {
            selected: 0,
            detail: false,
            scroll: 0,
            drawn_lines: Cell::new(0),
        });
        self.errors.log.mark_seen();
        Applied::none()
    }

    /// A key while the panel is up: arrows move (through the list, or through
    /// the lines of an open error), Enter opens or closes the text, Esc closes
    /// the panel. Any other key closes it and goes on to do what it does.
    pub(crate) fn errors_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => self.move_errors_cursor(-1),
            Key::Down => self.move_errors_cursor(1),
            Key::Enter => {
                if let Some(picker) = self.errors.picker.as_mut() {
                    picker.detail = !picker.detail;
                    picker.scroll = 0;
                }
            }
            Key::Esc => {
                self.errors.picker = None;
                self.disarm();
            }
            other => {
                self.errors.picker = None;
                return self.on_key(other, now);
            }
        }
        Applied::none()
    }

    fn move_errors_cursor(&mut self, delta: isize) {
        let len = self.errors.log.len();
        let Some(picker) = self.errors.picker.as_mut() else {
            return;
        };
        if picker.detail {
            let last = picker.drawn_lines.get().saturating_sub(1);
            picker.scroll = picker.scroll.saturating_add_signed(delta).min(last);
        } else if len > 0 {
            picker.selected = (picker.selected as isize + delta).rem_euclid(len as isize) as usize;
        }
    }
}

/// The `/errors` panel: a row per error, newest first, or the whole text of
/// the one Enter opened.
pub(crate) fn errors_panel(chat: &Chat, total: u16, width: u16) -> PanelView {
    let Some(picker) = chat.errors.picker.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let count = chat.errors.log.len();
    let selected = picker.selected.min(count.saturating_sub(1));
    // The box clips a row three cells short of what `panel_label_room` offers
    // once its own border and padding are paid for, and a clipped message
    // would lose its last word: wrap inside what is really there.
    let room = panel_label_room(width).saturating_sub(3);
    if picker.detail
        && let Some(entry) = chat.errors.log.newest_first().nth(selected)
    {
        let lines: Vec<PanelLine> = entry
            .text
            .lines()
            .flat_map(|line| wrap_plain(line, room))
            .map(|text| PanelLine::Row {
                text,
                accent: false,
            })
            .collect();
        picker.drawn_lines.set(lines.len());
        let cursor = picker.scroll.min(lines.len().saturating_sub(1));
        let title = format!(
            "error {} of {count} · {} · {}",
            selected + 1,
            entry.time(),
            entry.source.label()
        );
        return panel_view(Some(title), lines, Some(cursor), panel_body(total));
    }
    let lines: Vec<PanelLine> = chat
        .errors
        .log
        .newest_first()
        .map(|entry| PanelLine::Row {
            text: one_line(
                &format!(
                    "{}  {:<8} {}",
                    entry.time(),
                    entry.source.label(),
                    entry.text
                ),
                room,
            ),
            accent: false,
        })
        .collect();
    panel_view(
        Some(format!("errors · {count}")),
        lines,
        Some(selected),
        panel_body(total),
    )
}
