//! One-column scrollbar for the transcript and for picker/panel lists.
//!
//! Layout only: the caller owns the scrolling state and hands in the geometry
//! (the column's height) plus the numbers it already has (the scroll offset,
//! the rows the viewport shows, the rows the list holds); the widget answers
//! with one styled cell per row. Nothing here is bound to a pane, so the
//! transcript area and a [`crate::panels`] selection list can share it.
//!
//! [`thumb_span`] is the whole of the arithmetic; it follows omp's
//! `components/scroll-viewport.ts:105-127`. The thumb is the track scaled by
//! `visible/total` (`floor`), never shorter than one cell, and its start is
//! the scroll progress across the leftover travel, rounded to the nearest
//! cell. [`render`] paints that span over a resting track using the active
//! symbol preset ([`TRACK_SYMBOL`] / [`THUMB_SYMBOL`]) and two existing theme
//! tokens: [`ThemeColor::Muted`] for the track, [`ThemeColor::Accent`] for the
//! thumb (omp's own pair, `ui-modernization.md` §16b).

use crate::theme::{Theme, ThemeColor};

/// Symbol key for the resting column of the bar (`│`; `|` in the ASCII preset).
pub const TRACK_SYMBOL: &str = "scroll.track";

/// Symbol key for the movable thumb (`█`; `#` in the ASCII preset).
pub const THUMB_SYMBOL: &str = "scroll.thumb";

/// Glyphs for a theme whose symbol map somehow lacks the scroll keys, so a
/// cell is never zero columns wide.
const TRACK_FALLBACK: &str = "│";
const THUMB_FALLBACK: &str = "█";

/// Thumb geometry inside a track of `height` cells: `(start, length)`, both
/// 0-based, with `start + length <= height`.
pub type Thumb = (usize, usize);

/// The thumb's `(start, length)` in a track of `height` cells.
///
/// `offset` is the first visible row (`0` = top), `visible` the rows the
/// viewport shows at once, `total` the rows the list holds.
///
/// Returns `None` — "draw no bar" — when there is nothing to show:
///
/// - `height == 0`: a track with no rows cannot hold a thumb;
/// - `total == 0`: an empty list has no position to point at;
/// - `visible == 0`: a viewport showing no rows has no position either;
/// - `total <= visible`: everything fits, and no column should be reserved.
///
/// Otherwise the thumb is at least one cell and at most the whole track.
/// `offset` is clamped to the last page (`total - visible`), so an offset
/// past the end pins the thumb to the bottom instead of overflowing, and
/// `height == 1` answers `Some((0, 1))` — the single cell is the thumb
/// whenever there is content off-screen.
pub fn thumb_span(height: usize, offset: usize, visible: usize, total: usize) -> Option<Thumb> {
    if height == 0 || total == 0 || visible == 0 || total <= visible {
        return None;
    }
    // `total > visible >= 1`, so the division is safe and `travel < height`.
    let length = (height.saturating_mul(visible) / total).clamp(1, height);
    let travel = height - length;
    let max_offset = total - visible;
    let offset = offset.min(max_offset);
    // Round half up, cell after cell, so the thumb is not biased to the top.
    let start = offset.saturating_mul(travel).saturating_add(max_offset / 2) / max_offset;
    Some((start.min(travel), length))
}

/// The scrollbar's cells for a track `height` rows tall: each one's glyph and
/// whether it is the thumb.
///
/// This is what a host painting its own buffer needs, since it owns the theme's
/// colours already; [`render`] is the same cells encoded as ANSI for a host
/// that emits text. When there is no bar to draw, every cell is a plain space,
/// so the caller can paint the vector as-is and keep the column's geometry.
/// `height == 0` yields an empty vector.
pub fn cells(
    theme: &Theme,
    height: usize,
    offset: usize,
    visible: usize,
    total: usize,
) -> Vec<(String, bool)> {
    let Some((start, length)) = thumb_span(height, offset, visible, total) else {
        return vec![(" ".to_string(), false); height];
    };
    let thumb = pick(theme, THUMB_SYMBOL, THUMB_FALLBACK).to_string();
    let track = pick(theme, TRACK_SYMBOL, TRACK_FALLBACK).to_string();
    (0..height)
        .map(|row| {
            if row >= start && row < start + length {
                (thumb.clone(), true)
            } else {
                (track.clone(), false)
            }
        })
        .collect()
}

/// The scrollbar's cells, one ANSI-styled string per row: the thumb in
/// [`ThemeColor::Accent`], the track in [`ThemeColor::Muted`].
pub fn render(
    theme: &Theme,
    height: usize,
    offset: usize,
    visible: usize,
    total: usize,
) -> Vec<String> {
    if thumb_span(height, offset, visible, total).is_none() {
        // No bar to draw: plain spaces, so a caller can paint the column as-is.
        return vec![" ".to_string(); height];
    }
    cells(theme, height, offset, visible, total)
        .into_iter()
        .map(|(glyph, thumb)| {
            if thumb {
                theme.fg(ThemeColor::Accent, &glyph)
            } else {
                theme.fg(ThemeColor::Muted, &glyph)
            }
        })
        .collect()
}

/// The preset glyph for `key`, or `fallback` when the theme has none.
fn pick<'a>(theme: &'a Theme, key: &str, fallback: &'a str) -> &'a str {
    match theme.symbol(key) {
        "" => fallback,
        glyph => glyph,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, SymbolPreset};
    use crate::width::visible_width;
    use serde_json::json;
    use std::collections::HashMap;

    /// The two built-in `dark` theme tokens this widget uses, so the expected
    /// escapes below are the ones a user actually sees.
    const MUTED: &str = "\x1b[38;2;119;125;136m"; // dark: muted = #777d88
    const ACCENT: &str = "\x1b[38;2;254;188;56m"; // dark: accent = #febc38
    const FG_RESET: &str = "\x1b[39m";

    fn theme(preset: SymbolPreset) -> Theme {
        let mut fg = HashMap::new();
        fg.insert("muted".to_string(), json!("#777d88"));
        fg.insert("accent".to_string(), json!("#febc38"));
        Theme::new(
            "test".to_string(),
            fg,
            HashMap::new(),
            ColorMode::Truecolor,
            preset,
            HashMap::new(),
            None,
            None,
        )
        .expect("theme builds")
    }

    fn track() -> String {
        format!("{MUTED}│{FG_RESET}")
    }

    fn thumb() -> String {
        format!("{ACCENT}█{FG_RESET}")
    }

    // ---- thumb_span -------------------------------------------------------

    #[test]
    fn thumb_hidden_when_everything_fits() {
        assert_eq!(thumb_span(20, 0, 40, 40), None);
        assert_eq!(thumb_span(20, 7, 40, 39), None);
        assert_eq!(thumb_span(1, 0, 10, 1), None);
    }

    #[test]
    fn thumb_degenerate_inputs_answer_none() {
        // No track to draw in.
        assert_eq!(thumb_span(0, 0, 5, 100), None);
        // Empty list.
        assert_eq!(thumb_span(20, 0, 5, 0), None);
        // Empty viewport.
        assert_eq!(thumb_span(20, 0, 0, 100), None);
    }

    #[test]
    fn thumb_pins_to_the_top_at_offset_zero() {
        assert_eq!(thumb_span(20, 0, 5, 100), Some((0, 1)));
        assert_eq!(thumb_span(20, 0, 10, 40), Some((0, 5)));
    }

    #[test]
    fn thumb_pins_to_the_bottom_on_the_last_page() {
        // travel = 20 - 1 = 19, the last offset (95) lands on it exactly.
        assert_eq!(thumb_span(20, 95, 5, 100), Some((19, 1)));
        // travel = 20 - 5 = 15, the last offset (30) lands on it exactly.
        assert_eq!(thumb_span(20, 30, 10, 40), Some((15, 5)));
    }

    #[test]
    fn thumb_offset_past_the_end_clamps_to_the_last_page() {
        assert_eq!(thumb_span(20, 9_999, 5, 100), thumb_span(20, 95, 5, 100));
        assert_eq!(thumb_span(20, usize::MAX, 10, 40), Some((15, 5)));
    }

    #[test]
    fn thumb_length_is_proportional_to_visible_over_total() {
        // 20 cells * 10/40 = 5.
        assert_eq!(thumb_span(20, 0, 10, 40), Some((0, 5)));
        // 10 cells * 5/50 = 1 (the floor is the floor, not the ratio).
        assert_eq!(thumb_span(10, 0, 5, 50), Some((0, 1)));
        // 10 cells * 30/50 = 6, travel = 4, half way = round(0.5 * 4) = 2.
        assert_eq!(thumb_span(10, 10, 30, 50), Some((2, 6)));
    }

    #[test]
    fn thumb_is_at_least_one_cell() {
        assert_eq!(thumb_span(1, 0, 1, 1_000), Some((0, 1)));
        assert_eq!(thumb_span(100, 0, 1, 100_000), Some((0, 1)));
    }

    #[test]
    fn thumb_stays_inside_the_track_across_the_input_space() {
        for height in 0..=25 {
            for total in 0..=40 {
                for visible in 0..=40 {
                    for offset in [0, 1, 7, total, total + 13, usize::MAX] {
                        let Some((start, length)) = thumb_span(height, offset, visible, total)
                        else {
                            continue;
                        };
                        assert!(length >= 1, "height={height} v={visible} t={total}");
                        assert!(
                            start + length <= height,
                            "height={height} offset={offset} v={visible} t={total} -> ({start}, {length})"
                        );
                    }
                }
            }
        }
    }

    // ---- render -----------------------------------------------------------

    #[test]
    fn render_height_one_is_the_thumb() {
        assert_eq!(
            render(&theme(SymbolPreset::Unicode), 1, 0, 5, 12),
            vec![thumb()]
        );
    }

    #[test]
    fn render_height_five_shows_a_one_cell_thumb_at_the_top() {
        // 5 * 2/6 = 1, travel = 4, offset 0.
        assert_eq!(
            render(&theme(SymbolPreset::Unicode), 5, 0, 2, 6),
            vec![thumb(), track(), track(), track(), track()]
        );
    }

    #[test]
    fn render_twenty_rows_in_the_middle_and_at_the_end() {
        let theme = theme(SymbolPreset::Unicode);
        // 20 * 10/40 = 5, travel = 15, offset 15 of 30 -> start 8.
        let middle = render(&theme, 20, 15, 10, 40);
        let expected: Vec<String> = (0..20)
            .map(|row| {
                if (8..13).contains(&row) {
                    thumb()
                } else {
                    track()
                }
            })
            .collect();
        assert_eq!(middle, expected);
        assert_eq!(middle.len(), 20);
        // Last page: offset 30 -> start 15, the thumb touches the bottom.
        let end = render(&theme, 20, 30, 10, 40);
        assert_eq!(
            end.iter().filter(|cell| **cell == thumb()).count(),
            5,
            "thumb cells: {end:?}"
        );
        assert_eq!(end[14], track());
        assert_eq!(end[15], thumb());
        assert_eq!(end[19], thumb());
    }

    #[test]
    fn render_hidden_case_is_blank_cells() {
        assert_eq!(
            render(&theme(SymbolPreset::Unicode), 3, 0, 10, 10),
            vec![" ".to_string(), " ".to_string(), " ".to_string()]
        );
        // Nothing to scroll, and no track to draw in.
        assert!(render(&theme(SymbolPreset::Unicode), 0, 0, 10, 10).is_empty());
        assert!(render(&theme(SymbolPreset::Unicode), 0, 0, 3, 100).is_empty());
    }

    #[test]
    fn render_ascii_preset_uses_ascii_glyphs() {
        let theme = theme(SymbolPreset::Ascii);
        let rows = render(&theme, 5, 0, 2, 6);
        assert_eq!(rows[0], format!("{ACCENT}#{FG_RESET}"));
        assert_eq!(rows[1], format!("{MUTED}|{FG_RESET}"));
        assert_eq!(rows[4], format!("{MUTED}|{FG_RESET}"));
    }

    #[test]
    fn render_cells_never_exceed_one_column() {
        let unicode = theme(SymbolPreset::Unicode);
        let ascii = theme(SymbolPreset::Ascii);
        for theme in [&unicode, &ascii] {
            for height in 0..=22 {
                for visible in 0..=6 {
                    for total in 0..=20 {
                        for offset in [0, 3, total, total + 5] {
                            let rows = render(theme, height, offset, visible, total);
                            assert_eq!(rows.len(), height, "one cell per track row");
                            for (row, cell) in rows.iter().enumerate() {
                                assert!(
                                    visible_width(cell) <= 1,
                                    "height={height} row={row} v={visible} t={total}: {cell:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
