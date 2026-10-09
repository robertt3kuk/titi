//! A bar chart under a GFM table that reads as one.
//!
//! omp charts tables (`tui.autoGraph`, default `always`, drawn as SVG on a
//! graphics-capable terminal) with a plan over nine chart kinds, unit
//! normalization, and a judge model that picks the columns of a multi-series
//! table. This is the first slice of that, in text: **one** measure across
//! categories, as horizontal bars under the table it came from.
//!
//! What qualifies — every rule below is a refusal, and a table that fails one
//! renders exactly as it does today:
//!
//! - **4 to 12 body rows.** Fewer is a list, more is a scroll; omp's own
//!   `worthCharting` wants at least four categories.
//! - **The first column is a label, not an index.** A table whose first header
//!   is `#`, `no.`, `rank` or `index` is a listing, and bars numbered `1,2,3`
//!   under a table that already names its rows would be a worse picture than
//!   the table — omp's own rule is that an index is never the label.
//! - **One measure: the first column that qualifies, the label's own column
//!   never.** A second numeric column is left alone; a bar chart of one series
//!   is what this draws, and choosing between series is omp's judge model.
//! - **Every cell of that column is one quantity in one unit.** The quantity is
//!   the cell's *leading* figure, so `1,400 tok/s (avg)` and `**4** ✅` read,
//!   while anything that carries a second number — a range `24–72 h`, a
//!   transition `12 → 15`, a note `17 (21.5%)`, a date `2024-01-02` — does not,
//!   and one such cell skips the table.
//! - **`%`, a currency sign (`$ € £ ¥`), a unit word, or a multiplier
//!   (`k`, `M`, `B`) are read; a column that mixes units is not charted**
//!   (omp: units never share an axis).
//! - **No negatives**, and no leading sign at all: a diverging bar needs a zero
//!   line, which this slice does not draw, so such a table is left alone.
//! - **The picture says more than the table.** omp's `worthCharting`, kept as
//!   its own numbers: nine or more values, or a three-fold spread between the
//!   largest and the smallest positive one.
//! - **It fits.** A pane narrower than [`MIN_WIDTH`], or a bar field left with
//!   fewer than [`MIN_BAR_FIELD`] cells once the labels and the widest value
//!   are paid for, draws nothing rather than bars that all look alike.
//!
//! Drawn as: two cells of indent, the row's label, one space, the bar scaled so
//! the largest value fills the field, and the value as written at that bar's
//! end. Labels are cut to a third of the pane at most. The caller styles the
//! rows — nothing here allocates for a colour.

use crate::theme::{Theme, ThemeColor};
use crate::width::visible_width;

/// Fewest and most body rows a table is charted at.
const MIN_ROWS: usize = 4;
const MAX_ROWS: usize = 12;
/// omp's `worthCharting`: enough values to read as a distribution, or a spread
/// wide enough that the bar lengths differ at a glance.
const WORTH_POINTS: usize = 9;
const WORTH_SPREAD: f64 = 3.0;
/// Narrowest pane, and narrowest bar field, a chart is drawn in.
const MIN_WIDTH: usize = 24;
const MIN_BAR_FIELD: usize = 8;
/// A label never takes more than this share of the pane.
const MAX_LABEL_SHARE: usize = 3;
/// Longest unit word read from a cell.
const MAX_UNIT: usize = 8;
/// Eighths of a cell a bar can be filled to, the whole block last.
const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// One bar: what the row is called, and what it measures.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bar {
    /// The row's label, as written, decoration stripped.
    label: String,
    /// The figure as written (`1,400 tok/s`), which is what the bar's end says.
    figure: String,
    /// The value behind the figure, in the column's own unit.
    value: f64,
}

/// A chart read out of a table: one bar per body row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Chart {
    bars: Vec<Bar>,
}

/// Read a chart out of a table, or `None` if the table does not qualify.
///
/// `header` is the table's header row, `rows` its body, cells as written
/// (inline markdown and all). See the module docs for every rule.
pub(crate) fn plan(header: &[String], rows: &[Vec<String>]) -> Option<Chart> {
    if rows.len() < MIN_ROWS || rows.len() > MAX_ROWS {
        return None;
    }
    let columns = rows.first()?.len();
    if columns < 2 || rows.iter().any(|row| row.len() != columns) {
        return None;
    }
    if header.len() != columns || is_index_header(&header[0]) {
        return None;
    }
    let labels: Vec<String> = rows
        .iter()
        .map(|row| plain(&row[0]))
        .collect::<Option<Vec<String>>>()?;
    for column in 1..columns {
        if let Some(bars) = bars_in(rows, &labels, column) {
            return worth_charting(&bars).then_some(Chart { bars });
        }
    }
    None
}

/// The bars a column holds, if it is one measure in one unit.
fn bars_in(rows: &[Vec<String>], labels: &[String], column: usize) -> Option<Vec<Bar>> {
    let mut unit: Option<String> = None;
    let mut bars = Vec::with_capacity(rows.len());
    for (row, label) in rows.iter().zip(labels) {
        let cell = read(&row[column])?;
        if cell.value < 0.0 {
            return None;
        }
        match &unit {
            None => unit = Some(cell.unit.clone()),
            Some(held) if *held == cell.unit => {}
            Some(_) => return None,
        }
        bars.push(Bar {
            label: label.clone(),
            figure: cell.figure,
            value: cell.value,
        });
    }
    Some(bars)
}

/// Whether the bar lengths would say more than the numbers already do.
fn worth_charting(bars: &[Bar]) -> bool {
    let positive: Vec<f64> = bars
        .iter()
        .map(|bar| bar.value)
        .filter(|value| *value > 0.0)
        .collect();
    if positive.len() < 2 {
        return false;
    }
    let high = positive.iter().copied().fold(f64::MIN, f64::max);
    let low = positive.iter().copied().fold(f64::MAX, f64::min);
    bars.len() >= WORTH_POINTS || high / low >= WORTH_SPREAD
}

/// Whether a header names a row number rather than a category.
fn is_index_header(header: &str) -> bool {
    let text = header
        .trim()
        .trim_start_matches('#')
        .trim()
        .to_ascii_lowercase();
    matches!(
        text.as_str(),
        "" | "#" | "no" | "no." | "num" | "rank" | "index" | "id"
    )
}

/// Draw a chart: labels left, bars scaled to `width`, values at the bar ends.
pub(crate) fn render(chart: &Chart, theme: &Theme, width: usize) -> Vec<String> {
    if width < MIN_WIDTH {
        return Vec::new();
    }
    let label_field = chart
        .bars
        .iter()
        .map(|bar| visible_width(&bar.label))
        .max()
        .unwrap_or(1)
        .min(width / MAX_LABEL_SHARE)
        .max(1);
    let figure_field = chart
        .bars
        .iter()
        .map(|bar| visible_width(&bar.figure))
        .max()
        .unwrap_or(1);
    let field = width.saturating_sub(label_field + figure_field + 3);
    if field < MIN_BAR_FIELD {
        return Vec::new();
    }
    let high = chart
        .bars
        .iter()
        .map(|bar| bar.value)
        .fold(f64::MIN, f64::max);
    let mut out = Vec::with_capacity(chart.bars.len());
    for bar in &chart.bars {
        let eighths = if high > 0.0 {
            ((bar.value / high) * (field * 8) as f64).round() as usize
        } else {
            0
        }
        .min(field * 8);
        let (whole, part) = (eighths / 8, eighths % 8);
        let label = elide(&bar.label, label_field);
        let mut bar_cells = if full_blocks(theme) {
            let mut cells = EIGHTHS[7].repeat(whole);
            if part > 0 {
                cells.push_str(EIGHTHS[part - 1]);
            }
            cells
        } else {
            format!(
                "{}{}",
                hash(theme).repeat(whole),
                if part > 0 { hash(theme) } else { String::new() }
            )
        };
        bar_cells.push_str(&" ".repeat(field.saturating_sub(whole + usize::from(part > 0))));
        out.push(format!(
            "  {}{}{} {}",
            theme.fg(ThemeColor::MdCodeBlockBorder, &label),
            " ".repeat(label_field.saturating_sub(visible_width(&label)) + 1),
            theme.fg(ThemeColor::MdCodeBlock, &bar_cells),
            theme.fg(ThemeColor::Text, &bar.figure),
        ));
    }
    out
}

/// Whether the theme draws in blocks (`sep.block` is a block glyph) or ASCII.
fn full_blocks(theme: &Theme) -> bool {
    let glyph = hash(theme);
    !glyph.is_ascii() && !glyph.is_empty()
}

/// The whole-block glyph a filled cell is drawn with: the theme's own, the
/// full block where it has none (and the ASCII preset's `#`, which is what
/// makes [`full_blocks`] false there).
fn hash(theme: &Theme) -> String {
    let glyph = theme.symbol("scroll.thumb");
    if glyph.is_empty() {
        "█".to_owned()
    } else {
        glyph.to_owned()
    }
}

/// Truncate to `width` display cells, ending with `…` when it does not fit.
fn elide(text: &str, width: usize) -> String {
    if visible_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let mut buffer = [0u8; 4];
        let cell = visible_width(ch.encode_utf8(&mut buffer));
        if used + cell + 1 > width {
            break;
        }
        used += cell;
        out.push(ch);
    }
    out.push('…');
    out
}

/// A cell as plain text: emphasis and code decoration removed.
///
/// `None` when nothing readable is left, which is what makes a label column of
/// blank cells — or a row carrying no label — a table not worth charting.
fn plain(cell: &str) -> Option<String> {
    let trimmed = cell.trim();
    let text: String = trimmed
        .chars()
        .filter(|ch| !matches!(ch, '*' | '_' | '`'))
        .collect();
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// One cell read as a quantity: its value, the figure as written, its unit.
struct Cell {
    value: f64,
    figure: String,
    unit: String,
}

/// Read a cell's leading figure, or `None` when it is not one quantity.
///
/// The rule, in one line: an optional currency sign, digits with optional
/// `,`/`_` groups and one decimal point, an optional `k`/`M`/`B`, then either
/// `%`, or a unit word, or nothing — and **nothing after that may hold another
/// number**, so a range, a transition and a parenthesized share are all
/// refused rather than half-read. A leading sign is refused with them.
fn read(cell: &str) -> Option<Cell> {
    let text = plain(cell)?;
    let (currency, rest) = match text.chars().next() {
        Some(sign) if matches!(sign, '$' | '€' | '£' | '¥') => {
            (Some(sign), &text[sign.len_utf8()..])
        }
        _ => (None, text.as_str()),
    };
    let end = rest
        .char_indices()
        .find(|(_, ch)| !(ch.is_ascii_digit() || matches!(ch, ',' | '_' | '.')))
        .map(|(at, _)| at)
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    if !digits.bytes().any(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: f64 = digits.replace([',', '_'], "").parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    let tail = &rest[end..];
    let (multiplier, multiplied, tail) = match tail.chars().next() {
        Some(ch @ ('k' | 'K')) => (1_000.0, Some(ch), &tail[1..]),
        Some('M') => (1_000_000.0, Some('M'), &tail[1..]),
        Some('B') => (1_000_000_000.0, Some('B'), &tail[1..]),
        _ => (1.0, None, tail),
    };
    let separated = tail.len() != tail.trim_start().len();
    let tail = tail.trim_start();
    let (word, rest) = match tail.chars().next() {
        None => (String::new(), ""),
        Some('%') => ("%".to_owned(), &tail[1..]),
        // A unit, glued (`12ms`) or separated (`24 ms`).
        Some(lead) if lead.is_alphabetic() || lead == '/' => {
            let at = tail
                .find(|ch: char| !(ch.is_alphabetic() || ch == '/'))
                .unwrap_or(tail.len());
            (tail[..at].to_owned(), &tail[at..])
        }
        // After a space, anything that is not a unit is a note (`4 ✅`, which
        // the digit check below still has to clear).
        Some(_) if separated => (String::new(), tail),
        // Glued to the figure and not a unit: `2024-01-02`, `24–72`, `12→15`.
        // Not one quantity, so not a cell to chart.
        Some(_) => return None,
    };
    if word.len() > MAX_UNIT
        || rest.contains(|ch: char| ch.is_ascii_digit())
        || (currency.is_some() && word == "%")
    {
        return None;
    }
    // The unit two cells have to share is the currency and the word together,
    // the way a published table states it: `$` and `ms` are two units, and so
    // are `$` and no unit at all.
    let unit = if word == "%" {
        word.clone()
    } else {
        format!("{}{word}", currency.map(String::from).unwrap_or_default())
    };
    let mut figure = String::new();
    if let Some(sign) = currency {
        figure.push(sign);
    }
    figure.push_str(digits);
    if let Some(ch) = multiplied {
        figure.push(ch);
    }
    if word == "%" {
        figure.push('%');
    } else if !word.is_empty() {
        figure.push(' ');
        figure.push_str(&word);
    }
    Some(Cell {
        value: value * multiplier,
        figure,
        unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|cell| (*cell).to_owned()).collect()
    }

    fn rows(cells: &[&[&str]]) -> Vec<Vec<String>> {
        cells
            .iter()
            .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
            .collect()
    }

    /// A qualifying table: `Name | Time`.
    fn table() -> (Vec<String>, Vec<Vec<String>>) {
        (
            header(&["Name", "Time"]),
            rows(&[
                &["build", "120 ms"],
                &["test", "45 ms"],
                &["lint", "8 ms"],
                &["fmt", "2 ms"],
            ]),
        )
    }

    #[test]
    fn a_numeric_table_is_charted() {
        let (header, rows) = table();
        let chart = plan(&header, &rows).expect("chart");
        assert_eq!(chart.bars.len(), 4);
        assert_eq!(chart.bars[0].label, "build");
        assert_eq!(chart.bars[0].figure, "120 ms");
        assert_eq!(chart.bars[0].value, 120.0);
    }

    /// Units and separators, every shape the reader claims to take.
    #[test]
    fn the_reader_takes_the_shapes_it_claims() {
        for (cell, value, figure, unit) in [
            ("12", 12.0, "12", ""),
            ("1,400", 1400.0, "1,400", ""),
            ("0.5", 0.5, "0.5", ""),
            ("45%", 45.0, "45%", "%"),
            ("$250", 250.0, "$250", "$"),
            ("€1,200", 1200.0, "€1,200", "€"),
            ("2.5k", 2500.0, "2.5k", ""),
            ("3M", 3_000_000.0, "3M", ""),
            ("24 ms", 24.0, "24 ms", "ms"),
            ("12ms", 12.0, "12 ms", "ms"),
            ("9 tok/s", 9.0, "9 tok/s", "tok/s"),
            ("**4** ✅", 4.0, "4", ""),
            ("1,400 tok/s (avg)", 1400.0, "1,400 tok/s", "tok/s"),
        ] {
            let read = read(cell).unwrap_or_else(|| panic!("{cell} is a quantity"));
            assert_eq!(read.value, value, "{cell}");
            assert_eq!(read.figure, figure, "{cell}");
            assert_eq!(read.unit, unit, "{cell}");
        }
    }

    /// Anything carrying a second number is not one quantity, and a sign is not
    /// a figure: the cell is refused, which skips the table.
    #[test]
    fn a_cell_that_is_not_one_quantity_is_refused() {
        for cell in [
            "",
            "   ",
            "n/a",
            "2024-01-02",
            "24–72 h",
            "12 → 15",
            "17 (21.5%)",
            "1.2.3",
            "-5",
            "+5",
            "~5",
            "５", // fullwidth digits are not a number here
            "12 34",
            "5/8",
            "1e999",
            "\u{0}\u{7f}",
        ] {
            assert!(read(cell).is_none(), "{cell:?} must not read as a quantity");
        }
    }

    /// The refusals, one table each: fewer than four rows, more than twelve,
    /// a listing, an index header, mixed units, a negative, and a picture that
    /// does not say more than the numbers already do.
    #[test]
    fn a_table_that_does_not_qualify_is_not_charted() {
        let (cols, _) = table();
        let three = rows(&[&["a", "1"], &["b", "2"], &["c", "3"]]);
        assert!(plan(&cols, &three).is_none(), "three rows is a list");

        let many: Vec<Vec<String>> = (0..13)
            .map(|at| vec!["row".to_owned(), format!("{}", at + 1)])
            .collect();
        assert!(plan(&cols, &many).is_none(), "thirteen rows is a scroll");

        let listed = rows(&[
            &["1", "a", "10"],
            &["2", "b", "20"],
            &["3", "c", "30"],
            &["4", "d", "40"],
        ]);
        assert!(
            plan(&header(&["#", "Name", "Size"]), &listed).is_none(),
            "an index header means a listing"
        );

        let mixed = rows(&[&["a", "$1"], &["b", "2"], &["c", "$3"], &["d", "4"]]);
        assert!(plan(&cols, &mixed).is_none(), "units never share an axis");

        let negative = rows(&[&["a", "-1"], &["b", "2"], &["c", "-3"], &["d", "4"]]);
        assert!(
            plan(&cols, &negative).is_none(),
            "a bar cannot carry a sign"
        );

        let flat = rows(&[&["a", "10"], &["b", "10"], &["c", "10"], &["d", "10"]]);
        assert!(plan(&cols, &flat).is_none(), "nothing to compare");

        let narrow = rows(&[&["a", "10"], &["b", "11"], &["c", "12"], &["d", "13"]]);
        assert!(
            plan(&cols, &narrow).is_none(),
            "a spread under three says no more than the table"
        );
    }

    /// omp's numbers, both halves: nine values chart without a spread, four
    /// with one at three-fold or wider.
    #[test]
    fn the_picture_has_to_say_more_than_the_table() {
        let flat_nine: Vec<Vec<String>> = (1..=9)
            .map(|at| vec![format!("row{at}"), "10".to_owned()])
            .collect();
        assert!(
            plan(&header(&["Name", "N"]), &flat_nine).is_some(),
            "nine values"
        );

        let spread = rows(&[&["a", "3"], &["b", "4"], &["c", "5"], &["d", "9"]]);
        assert!(
            plan(&header(&["Name", "N"]), &spread).is_some(),
            "three-fold"
        );
    }

    /// The first column that qualifies is the measure; a label is never one.
    #[test]
    fn the_first_qualifying_column_is_the_measure() {
        let (_, _) = table();
        // Column 1 is words, column 2 is the measure.
        let two = rows(&[
            &["a", "fast", "10"],
            &["b", "slow", "40"],
            &["c", "fast", "20"],
            &["d", "slow", "80"],
        ]);
        let chart = plan(&header(&["Name", "Kind", "N"]), &two).expect("chart");
        assert_eq!(chart.bars[1].figure, "40");
        assert_eq!(chart.bars[1].label, "b");
    }

    /// A row with no label, or a column of blank labels, is not charted.
    #[test]
    fn a_row_without_a_label_is_not_charted() {
        let (header, _) = table();
        let blank = rows(&[&["a", "1"], &["", "2"], &["c", "3"], &["d", "4"]]);
        assert!(plan(&header, &blank).is_none());
        let decorated = rows(&[&["**a**", "1"], &["_b_", "2"], &["`c`", "3"], &["d", "4"]]);
        let chart = plan(&header, &decorated).expect("chart");
        assert_eq!(chart.bars[0].label, "a");
        assert_eq!(chart.bars[1].label, "b");
        assert_eq!(chart.bars[2].label, "c");
    }

    /// A figure too long to draw is not a crash and not a broken chart: the
    /// bars lose their field, so nothing is drawn.
    #[test]
    fn an_absurd_figure_leaves_no_room_for_bars() {
        let long = "9".repeat(200);
        let rows = rows(&[
            &["a", long.as_str()],
            &["b", long.as_str()],
            &["c", long.as_str()],
            &["d", long.as_str()],
        ]);
        assert!(
            plan(&header(&["Name", "N"]), &rows).is_none(),
            "no room, no chart"
        );
    }

    /// Hostile shapes: nothing panics, nothing charts.
    #[test]
    fn hostile_tables_are_refused_without_a_panic() {
        let (header, _) = table();
        let cases: Vec<Vec<Vec<String>>> = vec![
            Vec::new(),
            vec![vec![]],
            vec![vec!["only one column".to_owned()]; 4],
            vec![
                vec!["a".to_owned(), "1".to_owned()],
                vec!["b".to_owned(), "2".to_owned()],
            ],
            vec![vec![String::new(); 0]; 4],
            vec![vec!["\u{0}".to_owned(); 64]; 4],
            vec![vec!["x".repeat(10_000), "1".repeat(10_000)]; 4],
            vec![
                vec!["a".to_owned(), "1e309".to_owned()],
                vec!["b".to_owned(), "-0".to_owned()],
                vec!["c".to_owned(), "0".to_owned()],
                vec!["d".to_owned(), "0".to_owned()],
            ],
        ];
        for rows in cases {
            let _ = plan(&header, &rows);
            let _ = plan(&[], &rows);
        }
    }
}
