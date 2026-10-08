//! Mouse selection model — drag-select anchor, current, and background
//! rendering.
//!
//! Contract: `docs/research/agent-ux/README.md` (DoD mouse item):
//! drag-select draws selection background (selectedBg token) instead of
//! SGR inverse; the selection region is a rectangle between anchor and
//! current cursor position.

use crate::theme::{Theme, ThemeBg};
use crate::width::{Span, char_width, spans};

/// A rectangular selection with an anchor and a moving cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    /// Anchor: cursor position when the button was pressed.
    pub anchor: (u16, u16),
    /// Current cursor position (updates on drag).
    pub current: (u16, u16),
    /// Whether the selection is active (button held).
    pub active: bool,
}

impl Selection {
    /// Create a new selection anchored at (x, y).
    pub fn anchor(x: u16, y: u16) -> Self {
        Selection {
            anchor: (x, y),
            current: (x, y),
            active: true,
        }
    }

    /// Update the current position (drag).
    pub fn drag(&mut self, x: u16, y: u16) {
        self.current = (x, y);
    }

    /// Deactivate selection (button released).
    pub fn release(&mut self) {
        self.active = false;
    }

    /// Whether there is a non-empty selection.
    pub fn is_non_empty(&self) -> bool {
        self.anchor != self.current
    }

    /// The bounding rectangle of the selection.  `(left, top, right, bottom)`
    /// — all inclusive.  Returns `None` if the selection is empty.
    pub fn rect(&self) -> Option<(u16, u16, u16, u16)> {
        if self.anchor == self.current {
            return None;
        }
        let left = self.anchor.0.min(self.current.0);
        let right = self.anchor.0.max(self.current.0);
        let top = self.anchor.1.min(self.current.1);
        let bottom = self.anchor.1.max(self.current.1);
        Some((left, top, right, bottom))
    }

    /// The text a copy of this selection carries: the selected columns of
    /// every selected row, joined by a newline.
    ///
    /// Styling is not text, so escape sequences are dropped on the way — the
    /// clipboard gets the characters the screen shows, not the SGR that drew
    /// them — and a row's trailing blanks are padding, not content, so each
    /// row is trimmed. Rows outside the rows passed in (a selection that ran
    /// past the end of the pane) contribute nothing, exactly as in
    /// [`Selection::apply_background`].
    pub fn text(&self, rows: &[String]) -> String {
        let Some((left, top, right, bottom)) = self.rect() else {
            return String::new();
        };
        let mut lines: Vec<String> = rows
            .iter()
            .enumerate()
            .filter_map(|(y, row)| {
                let y = y as u16;
                (y >= top && y <= bottom).then(|| clip_columns(row, left, right))
            })
            .collect();
        // Blank rows the selection merely crossed — above the first line of
        // text and below the last — are not part of what was selected.
        while lines.first().is_some_and(String::is_empty) {
            lines.remove(0);
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Apply the selection background to viewport rows.
    ///
    /// For each row within the selection's vertical span, the columns
    /// `left..=right` get the `selectedBg` background token.  The overlay is
    /// span-aware: ANSI sequences pass through, the background re-applies
    /// after nested full resets, and double-width characters are counted by
    /// their column contribution.
    pub fn apply_background(&self, rows: &[String], theme: &Theme) -> Vec<String> {
        let Some((left, top, right, bottom)) = self.rect() else {
            return rows.to_vec();
        };
        let bg = theme.get_bg_ansi(ThemeBg::SelectedBg);
        rows.iter()
            .enumerate()
            .map(|(y, row)| {
                let y = y as u16;
                if y < top || y > bottom || row.is_empty() {
                    row.clone()
                } else {
                    apply_bg_to_columns(row, left, right, &bg)
                }
            })
            .collect()
    }
}

/// The visible columns `[left, right]` (inclusive) of `row`, as plain text:
/// escape sequences are dropped, wide characters are counted by their cells,
/// and the result is trimmed of the padding the columns past the text carry.
fn clip_columns(row: &str, left: u16, right: u16) -> String {
    let mut out = String::new();
    let mut col: u32 = 0;
    for span in spans(row) {
        match span {
            Span::Escape(_) => {}
            Span::Text(t) => {
                for c in t.chars() {
                    let cw = char_width(c) as u32;
                    // A zero-width character rides on the cell before it, so
                    // it is in the selection whenever that cell is.
                    let within = col <= right as u32 && col + cw.max(1) > left as u32;
                    if within {
                        out.push(c);
                    }
                    col += cw;
                    if col > right as u32 {
                        return out.trim_end().to_owned();
                    }
                }
            }
        }
    }
    out.trim_end().to_owned()
}

/// Wrap the visible columns `[left, right]` (inclusive) of `row` in `bg`.
///
/// ANSI sequences pass through untouched; the background is re-applied after
/// any nested full reset inside the selection.
fn apply_bg_to_columns(row: &str, left: u16, right: u16, bg: &str) -> String {
    let mut out = String::with_capacity(row.len() + 32);
    let mut col: u32 = 0;
    let mut in_selection = false;
    for span in spans(row) {
        match span {
            Span::Escape(seq) => {
                if in_selection && is_full_reset(seq) {
                    // A full reset inside the selection clears the bg —
                    // re-apply it.
                    out.push_str(seq);
                    out.push_str(bg);
                } else {
                    out.push_str(seq);
                }
            }
            Span::Text(t) => {
                for c in t.chars() {
                    let cw = char_width(c) as u32;
                    let within = col <= right as u32 && col + cw > left as u32;
                    if within && !in_selection {
                        out.push_str(bg);
                        in_selection = true;
                    } else if !within && in_selection {
                        // Reached the right edge — close the bg.  We can't
                        // emit a reset that would clear the fg, so emit a
                        // bg-only reset (SGR 49 = default background).
                        out.push_str("\x1b[49m");
                        in_selection = false;
                    }
                    out.push(c);
                    col += cw;
                }
            }
        }
    }
    if in_selection {
        out.push_str("\x1b[49m");
    }
    out
}

/// Whether `seq` is a full SGR reset (`ESC[0m` or `ESC[m`).
fn is_full_reset(seq: &str) -> bool {
    seq == "\x1b[0m" || seq == "\x1b[m" || seq == "\x1b[0;0m"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, SymbolPreset, Theme};
    use serde_json::json;
    use std::collections::HashMap;

    fn test_theme() -> Theme {
        let mut fg = HashMap::new();
        fg.insert("error".into(), json!("#ff0000"));
        let mut bg = HashMap::new();
        bg.insert("selectedBg".into(), json!("#335599"));
        Theme::new(
            "test".into(),
            fg,
            bg,
            ColorMode::Truecolor,
            SymbolPreset::Unicode,
            HashMap::new(),
            None,
            None,
        )
        .expect("test theme builds")
    }

    #[test]
    fn selection_anchor_current() {
        let s = Selection::anchor(5, 10);
        assert!(s.active);
        assert_eq!(s.anchor, (5, 10));
        assert_eq!(s.current, (5, 10));
        assert!(!s.is_non_empty());
    }

    #[test]
    fn selection_drag_updates_current() {
        let mut s = Selection::anchor(5, 10);
        s.drag(15, 20);
        assert_eq!(s.current, (15, 20));
        assert!(s.is_non_empty());
        assert_eq!(s.rect(), Some((5, 10, 15, 20)));
    }

    #[test]
    fn selection_release_deactivates() {
        let mut s = Selection::anchor(5, 10);
        s.drag(15, 20);
        s.release();
        assert!(!s.active);
        // rect still returns the committed region.
        assert_eq!(s.rect(), Some((5, 10, 15, 20)));
    }

    #[test]
    fn selection_rect_folds_negative_direction() {
        let mut s = Selection::anchor(15, 20);
        s.drag(5, 10);
        // rect should normalize: left=5, top=10, right=15, bottom=20
        assert_eq!(s.rect(), Some((5, 10, 15, 20)));
    }

    #[test]
    fn selection_empty_rect() {
        let s = Selection::anchor(5, 10);
        assert_eq!(s.rect(), None);
    }

    #[test]
    fn text_takes_only_the_selected_columns() {
        let mut s = Selection::anchor(2, 0);
        s.drag(4, 0);
        let rows = vec!["abcdef".to_owned()];
        assert_eq!(s.text(&rows), "cde");
    }

    #[test]
    fn text_drops_styling_and_trailing_padding() {
        let theme = test_theme();
        // The row the renderer would draw: an fg-coloured run, then padding to
        // the pane's edge.
        let row = format!("{}!", theme.fg(crate::theme::ThemeColor::Error, "hi"));
        let row = format!("{row}    ");
        let mut s = Selection::anchor(0, 0);
        s.drag(20, 0);
        assert_eq!(s.text(&[row]), "hi!");
    }

    #[test]
    fn text_joins_rows_and_keeps_inner_blanks() {
        let mut s = Selection::anchor(0, 0);
        s.drag(9, 2);
        let rows = vec!["one".to_owned(), String::new(), "two".to_owned()];
        assert_eq!(s.text(&rows), "one\n\ntwo");
    }

    #[test]
    fn text_ignores_rows_outside_the_selection_and_a_click() {
        let rows = vec!["one".to_owned(), "two".to_owned()];
        // Rows 0..=0 only: the second row is not part of the selection.
        let mut s = Selection::anchor(0, 0);
        s.drag(9, 0);
        assert_eq!(s.text(&rows), "one");
        // A click (anchor == current) selects nothing.
        let click = Selection::anchor(3, 1);
        assert_eq!(click.text(&rows), "");
        // A selection that ran past the rows it was given copies what exists.
        let mut past = Selection::anchor(0, 0);
        past.drag(9, 7);
        assert_eq!(past.text(&rows), "one\ntwo");
    }

    #[test]
    fn text_counts_wide_characters_by_their_cells() {
        // "日本" is four cells wide; columns 2..=3 are the second character.
        let mut s = Selection::anchor(2, 0);
        s.drag(3, 0);
        assert_eq!(s.text(&["日本語".to_owned()]), "本");
    }

    #[test]
    fn apply_background_marks_selected_rows() {
        let theme = test_theme();
        let mut s = Selection::anchor(1, 0);
        s.drag(3, 0);
        let rows = vec!["short".to_owned(), "longer".to_owned()];
        let result = s.apply_background(&rows, &theme);
        assert_eq!(result.len(), 2);
        // First row within selection vertical span (y=0) gets bg from col 1.
        let ansi = theme.get_bg_ansi(ThemeBg::SelectedBg);
        assert!(result[0].contains(&ansi), "first row bg: {:?}", &result[0]);
        assert!(
            result[0].contains("\x1b[49m"),
            "bg closed: {:?}",
            &result[0]
        );
        // Second row outside selection (y=1 > bottom=0) unchanged.
        assert_eq!(result[1], "longer");
    }

    #[test]
    fn apply_background_empty_rows() {
        let theme = test_theme();
        let mut s = Selection::anchor(0, 0);
        s.drag(5, 0);
        let rows: Vec<String> = vec![];
        let result = s.apply_background(&rows, &theme);
        assert!(result.is_empty());
    }

    #[test]
    fn apply_background_column_precision() {
        let theme = test_theme();
        // Selection on row 0, columns 2..=4 of a 6-char row.
        // anchor at col 2, drag to col 4 → rect(2, 0, 4, 0)
        let mut s = Selection::anchor(2, 0);
        s.drag(4, 0);
        let rows = vec!["abcdef".to_owned()];
        let result = s.apply_background(&rows, &theme);
        let ansi = theme.get_bg_ansi(ThemeBg::SelectedBg);
        // "ab" + bg + "cde" + reset + "f"
        // But wait: the rect is left=2, right=4, so columns 2,3,4 are selected.
        // "abcdef" → c=col0, d=col1, e=col2? No: a=0,b=1,c=2,d=3,e=4,f=5
        // Selection left=2, right=4 → columns 2,3,4 → "cde"
        let expected = format!("ab{ansi}cde\x1b[49mf");
        assert_eq!(result[0], expected, "got: {:?}", result[0]);
    }

    #[test]
    fn apply_background_reapplies_after_nested_reset() {
        let theme = test_theme();
        // Row with an fg token + fg-reset inside the selected range.
        let styled = theme.fg(crate::theme::ThemeColor::Error, "xy");
        let row = format!("{styled}zw"); // fg-red "xy" then FG_RESET "zw"
        let mut s = Selection::anchor(0, 0);
        s.drag(3, 0);
        let result = s.apply_background(&vec![row.clone()], &theme);
        let ansi = theme.get_bg_ansi(ThemeBg::SelectedBg);
        // Selection covers all columns; bg applied at start (after fg ansi),
        // closed at end.  The fg-reset inside (\x1b[39m) doesn't clear bg,
        // so no re-application needed.
        let fg_ansi = theme.get_fg_ansi(crate::theme::ThemeColor::Error);
        assert!(
            result[0].starts_with(&format!("{fg_ansi}{ansi}")),
            "fg then bg at start: {:?}",
            result[0]
        );
        assert!(
            result[0].ends_with("\x1b[49m"),
            "bg closed: {:?}",
            result[0]
        );
        // The fg-red "xy" text is still present.
        assert!(result[0].contains("xy"));
    }
}
