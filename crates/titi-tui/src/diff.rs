//! Unified-diff renderer for transcript tool chips.
//!
//! [`render_diff`] turns a unified-diff text blob (`--- a/x`, `+++ b/x`,
//! `@@ -l[,c] +l[,c] @@`, ` `, `-`, `+`, `\ No newline at end of file`) into
//! themed terminal rows, following omp's `chrome/diff.ts:104-123`: context
//! dim, removals in the theme's removal colour, additions in its addition
//! colour, and the changed tokens of a single-line replacement inverted.
//!
//! Rules the renderer holds to:
//!
//! - **Gutter.** Every content row starts with the old and the new line
//!   number, right-aligned in [`GUTTER_DIGITS`] columns each, then a marker
//!   column (`+`, `-`, or a space). A number the line does not have on that
//!   side is blank; a hunk header or a hidden-context row leaves both blank.
//!   omp fixes the gutter at three digits so that a streaming preview renders
//!   byte-identically to the finished block; a diff numbering a line past 999
//!   widens both columns instead of re-padding rows already emitted.
//! - **Width.** A row longer than the pane is *cut*, with a trailing `…`, and
//!   never wrapped — the repo's habit for transcript rows. Every row
//!   satisfies `visible_width(row) <= width`.
//! - **Collapsing.** A context run longer than `2 * CONTEXT_KEEP` rows keeps
//!   its first and last [`CONTEXT_KEEP`] rows and hides the rest behind one
//!   `… N unchanged` row, `N` being the exact number of hidden lines.
//! - **Intra-line emphasis.** Only a 1:1 replacement is compared token by
//!   token, and only the tokens between the common head and tail of the two
//!   lines are inverted. Runs of unequal length render as plain add/remove
//!   rows, and so does a pair whose differing middle exceeds
//!   `INLINE_LCS_LIMIT` tokens: pairing two removed lines with three added
//!   ones would be a guess, not a diff.
//! - **Colours.** Only the `toolDiff*` tokens of [`ThemeColor`] are used;
//!   nothing here hard-codes an RGB value. A theme that leaves a token
//!   unset renders that row in the terminal default foreground.
//!
//! Not a diff: a blob with no parseable hunk header (a tool that returned a
//! plain message) yields [`None`], so a caller can fall back to plain text.
//! Everything before the first hunk header is ignored except for the file
//! names, which are reported as [`RenderedDiff::path`].

use crate::theme::{Theme, ThemeColor};
use crate::width::{replace_tabs, truncate_to_width, visible_width};

/// Digits reserved for each line-number column (omp `chrome/diff.ts:117-123`).
pub const GUTTER_DIGITS: usize = 3;

/// Context rows kept at each edge of a collapsed unchanged run.
pub const CONTEXT_KEEP: usize = 3;

/// Longest token pair compared for intra-line emphasis.
const INLINE_LCS_LIMIT: usize = 64;

/// The ellipsis appended to a row cut at the pane width.
const ELLIPSIS: char = '…';

/// A unified diff rendered one row per diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDiff {
    /// The first file the blob names: the `+++` side when present, else the
    /// `---` side, with a conventional `a/`/`b/` prefix dropped and
    /// `/dev/null` read as unnamed.
    pub path: Option<String>,
    /// Styled rows; `visible_width(row) <= width` for each of them.
    pub rows: Vec<String>,
}

/// Render unified diff `text`, or `None` when it is not a diff at all.
///
/// A blob counts as a diff once it contains one parseable hunk header; the
/// lines of a hunk are consumed by the counts the header declares. Rows are
/// cut to `width` columns.
pub fn render_diff(text: &str, theme: &Theme, width: u16) -> Option<RenderedDiff> {
    let parsed = parse_diff(text)?;
    Some(RenderedDiff {
        path: parsed.path,
        rows: render_rows(&collapse_context(parsed.rows), theme, usize::from(width)),
    })
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// One rendered line of a diff, before styling.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// The `@@ … @@` line that opens a hunk, rendered as a dim separator.
    HunkHeader(String),
    Context {
        old: usize,
        new: usize,
        text: String,
    },
    Removed {
        old: usize,
        text: String,
    },
    Added {
        new: usize,
        text: String,
    },
    /// `\ No newline at end of file`.
    NoNewlineMarker(String),
    /// A run of unchanged context that was folded away; the count is exact.
    Collapsed(usize),
}

/// A blob that parsed as a diff.
struct ParsedDiff {
    path: Option<String>,
    rows: Vec<Row>,
}

/// The `@@ -l[,c] +l[,c] @@` counts of a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkHeader {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

/// Parse a unified diff, or `None` when it has no hunk to render.
fn parse_diff(text: &str) -> Option<ParsedDiff> {
    let mut lines = text.lines().peekable();
    let mut rows = Vec::new();
    let mut hunks = 0usize;
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;

    while let Some(line) = lines.next() {
        if let Some(header) = parse_hunk_header(line) {
            hunks += 1;
            rows.push(Row::HunkHeader(line.trim_end().to_string()));
            parse_hunk_body(&mut lines, header, &mut rows);
            continue;
        }
        if line.starts_with("+++ ") {
            if new_path.is_none() {
                new_path = file_path(line);
            }
        } else if line.starts_with("--- ") && old_path.is_none() {
            old_path = file_path(line);
        }
    }

    if hunks == 0 {
        return None;
    }
    Some(ParsedDiff {
        path: new_path.or(old_path),
        rows,
    })
}

/// Parse `@@ -l[,c] +l[,c] @@ [section]`; `None` when the line is not one.
fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_start, rest) = split_number(rest)?;
    let (old_count, rest) = split_optional_count(rest);
    let (new_start, rest) = split_number(rest.strip_prefix(" +")?)?;
    let (new_count, rest) = split_optional_count(rest);
    let rest = rest.trim_start();
    // A `@@` closes the header; a producer that appends a section heading is
    // free to leave it out, but anything else means this was never a header.
    if !rest.is_empty() && !rest.starts_with("@@") {
        return None;
    }
    Some(HunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// A leading decimal number and the rest of the line.
fn split_number(s: &str) -> Option<(usize, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    s[..end].parse().ok().map(|n| (n, &s[end..]))
}

/// `,<count>` when present, defaulting to one line.
fn split_optional_count(s: &str) -> (usize, &str) {
    match s.strip_prefix(',').and_then(split_number) {
        Some(pair) => pair,
        None => (1, s),
    }
}

/// The path named by a `--- `/`+++ ` header line, `None` for `/dev/null`.
fn file_path(line: &str) -> Option<String> {
    let rest = line.get(3..)?.trim();
    // `git diff` writes `<path>` and may append a tab-separated timestamp;
    // newer versions quote a path that holds spaces or non-ASCII bytes.
    let rest = rest.split('\t').next().unwrap_or(rest).trim();
    let rest = rest
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(rest);
    if rest.is_empty() || rest == "/dev/null" {
        return None;
    }
    let rest = rest
        .strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest);
    Some(rest.to_string())
}

/// The line shapes a hunk body may hold.
enum Body<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
    NoNewline(&'a str),
}

/// Classify one hunk body line; `None` when its shape is not a diff line.
fn body_line(line: &str) -> Option<Body<'_>> {
    match line.chars().next() {
        // A producer that drops the prefix of an empty context line.
        None => Some(Body::Context("")),
        Some(' ') => Some(Body::Context(&line[1..])),
        Some('-') => Some(Body::Removed(&line[1..])),
        Some('+') => Some(Body::Added(&line[1..])),
        Some('\\') => Some(Body::NoNewline(line.trim_end())),
        Some(_) => None,
    }
}

/// Consume the declared number of old and new lines of a hunk.
fn parse_hunk_body<'a, I>(
    lines: &mut std::iter::Peekable<I>,
    header: HunkHeader,
    rows: &mut Vec<Row>,
) where
    I: Iterator<Item = &'a str>,
{
    let (mut old, mut new) = (header.old_start, header.new_start);
    let (mut old_seen, mut new_seen) = (0usize, 0usize);

    while old_seen < header.old_count || new_seen < header.new_count {
        let Some(line) = lines.peek().copied() else {
            return;
        };
        // The declared counts overran this hunk; leave the next header alone.
        if parse_hunk_header(line).is_some() {
            return;
        }
        let Some(body) = body_line(line) else {
            return;
        };
        lines.next();
        match body {
            Body::Context(text) => {
                rows.push(Row::Context {
                    old,
                    new,
                    text: text.to_string(),
                });
                old += 1;
                new += 1;
                old_seen += 1;
                new_seen += 1;
            }
            Body::Removed(text) => {
                rows.push(Row::Removed {
                    old,
                    text: text.to_string(),
                });
                old += 1;
                old_seen += 1;
            }
            Body::Added(text) => {
                rows.push(Row::Added {
                    new,
                    text: text.to_string(),
                });
                new += 1;
                new_seen += 1;
            }
            Body::NoNewline(text) => rows.push(Row::NoNewlineMarker(text.to_string())),
        }
    }

    // The marker trails the side it belongs to, so it can arrive once the
    // counters above are already satisfied.
    while let Some(line) = lines.peek().copied() {
        let Some(Body::NoNewline(text)) = body_line(line) else {
            break;
        };
        lines.next();
        rows.push(Row::NoNewlineMarker(text.to_string()));
    }
}

// ---------------------------------------------------------------------------
// Collapsing
// ---------------------------------------------------------------------------

/// Fold long unchanged runs into a single `… N unchanged` row.
fn collapse_context(rows: Vec<Row>) -> Vec<Row> {
    let mut out = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        if !matches!(rows[i], Row::Context { .. }) {
            out.push(rows[i].clone());
            i += 1;
            continue;
        }
        let mut end = i;
        while end < rows.len() && matches!(rows[end], Row::Context { .. }) {
            end += 1;
        }
        if end - i > 2 * CONTEXT_KEEP {
            out.extend_from_slice(&rows[i..i + CONTEXT_KEEP]);
            out.push(Row::Collapsed(end - i - 2 * CONTEXT_KEEP));
            out.extend_from_slice(&rows[end - CONTEXT_KEEP..end]);
        } else {
            out.extend_from_slice(&rows[i..end]);
        }
        i = end;
    }
    out
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Gutter geometry for the rows being rendered.
struct Layout {
    width: usize,
    digits: usize,
    /// Columns taken by `old new marker`.
    marker_columns: usize,
}

impl Layout {
    fn new(rows: &[Row], width: usize) -> Self {
        let digits = line_number_width(rows);
        Layout {
            width,
            digits,
            marker_columns: digits * 2 + 3,
        }
    }

    /// Whether the pane is wide enough for the gutter at all.
    fn has_gutter(&self) -> bool {
        self.width > self.marker_columns
    }

    /// Columns left for a row's text.
    fn text_columns(&self) -> usize {
        if self.has_gutter() {
            self.width - self.marker_columns
        } else {
            // Degenerate pane: the marker is all that survives.
            self.width.saturating_sub(1)
        }
    }

    /// The `old new marker` prefix, or the bare marker on a narrow pane.
    fn gutter(&self, old: Option<usize>, new: Option<usize>, marker: char) -> String {
        if self.width == 0 {
            return String::new();
        }
        if !self.has_gutter() {
            return marker.to_string();
        }
        let old_field = old.map(|n| n.to_string()).unwrap_or_default();
        let new_field = new.map(|n| n.to_string()).unwrap_or_default();
        format!(
            "{:>w$} {:>w$} {}",
            old_field,
            new_field,
            marker,
            w = self.digits
        )
    }

    /// A row's text, tabs expanded and cut to the pane.
    fn text(&self, text: &str) -> String {
        cut(&replace_tabs(text), self.text_columns())
    }

    /// A complete content row.
    fn row(&self, old: Option<usize>, new: Option<usize>, marker: char, text: &str) -> String {
        format!("{}{}", self.gutter(old, new, marker), self.text(text))
    }
}

/// Width of each line-number column: three digits, widened for line 1000+.
fn line_number_width(rows: &[Row]) -> usize {
    let largest = rows
        .iter()
        .filter_map(|row| match row {
            Row::Context { old, new, .. } => Some((*old).max(*new)),
            Row::Removed { old, .. } => Some(*old),
            Row::Added { new, .. } => Some(*new),
            Row::HunkHeader(_) | Row::NoNewlineMarker(_) | Row::Collapsed(_) => None,
        })
        .max()
        .unwrap_or_default();
    largest.to_string().len().max(GUTTER_DIGITS)
}

/// Cut `text` to `columns` cells, ending an over-long line with an ellipsis.
fn cut(text: &str, columns: usize) -> String {
    if visible_width(text) <= columns {
        return text.to_string();
    }
    if columns == 0 {
        return String::new();
    }
    let mut out = truncate_to_width(text, columns - 1);
    out.push(ELLIPSIS);
    out
}

/// Style every row of a parsed, collapsed diff.
fn render_rows(rows: &[Row], theme: &Theme, width: usize) -> Vec<String> {
    let layout = Layout::new(rows, width);
    let mut out = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        match &rows[i] {
            // The hunk header spans the block as a separator, so it stays
            // flush left instead of indenting under the gutter.
            Row::HunkHeader(text) => {
                out.push(theme.fg(ThemeColor::ToolDiffContext, &cut(text, width)));
                i += 1;
            }
            Row::Collapsed(hidden) => {
                let text = format!("{ELLIPSIS} {hidden} unchanged");
                out.push(theme.fg(
                    ThemeColor::ToolDiffContext,
                    &layout.row(None, None, ' ', &text),
                ));
                i += 1;
            }
            Row::NoNewlineMarker(text) => {
                out.push(theme.fg(
                    ThemeColor::ToolDiffContext,
                    &layout.row(None, None, ' ', text),
                ));
                i += 1;
            }
            Row::Context { old, new, text } => {
                out.push(theme.fg(
                    ThemeColor::ToolDiffContext,
                    &layout.row(Some(*old), Some(*new), ' ', text),
                ));
                i += 1;
            }
            Row::Added { new, text } => {
                out.push(theme.fg(
                    ThemeColor::ToolDiffAdded,
                    &layout.row(None, Some(*new), '+', text),
                ));
                i += 1;
            }
            Row::Removed { .. } => {
                let mut end = i;
                while end < rows.len() && matches!(rows[end], Row::Removed { .. }) {
                    end += 1;
                }
                let removed = &rows[i..end];
                // A no-newline marker sits between the removal and the
                // addition that replaced it, but belongs to neither row, so
                // keep the pair adjacent for intra-line emphasis.
                let mut markers = Vec::new();
                while let Some(Row::NoNewlineMarker(text)) = rows.get(end) {
                    markers.push(text.clone());
                    end += 1;
                }
                let added_start = end;
                while end < rows.len() && matches!(rows[end], Row::Added { .. }) {
                    end += 1;
                }
                let added = &rows[added_start..end];
                i = end;

                let pair = match (removed, added) {
                    ([Row::Removed { old, text: before }], [Row::Added { new, text: after }]) => {
                        Some((*old, before.as_str(), *new, after.as_str()))
                    }
                    _ => None,
                };

                if let Some((old, before, new, after)) = pair {
                    let (before, after) =
                        intra_line(&layout.text(before), &layout.text(after), theme);
                    out.push(theme.fg(
                        ThemeColor::ToolDiffRemoved,
                        &format!("{}{before}", layout.gutter(Some(old), None, '-')),
                    ));
                    for text in &markers {
                        out.push(theme.fg(
                            ThemeColor::ToolDiffContext,
                            &layout.row(None, None, ' ', text),
                        ));
                    }
                    out.push(theme.fg(
                        ThemeColor::ToolDiffAdded,
                        &format!("{}{after}", layout.gutter(None, Some(new), '+')),
                    ));
                } else {
                    for row in removed {
                        if let Row::Removed { old, text } = row {
                            out.push(theme.fg(
                                ThemeColor::ToolDiffRemoved,
                                &layout.row(Some(*old), None, '-', text),
                            ));
                        }
                    }
                    for text in &markers {
                        out.push(theme.fg(
                            ThemeColor::ToolDiffContext,
                            &layout.row(None, None, ' ', text),
                        ));
                    }
                    for row in added {
                        if let Row::Added { new, text } = row {
                            out.push(theme.fg(
                                ThemeColor::ToolDiffAdded,
                                &layout.row(None, Some(*new), '+', text),
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Intra-line emphasis
// ---------------------------------------------------------------------------

/// Style the two rows of a single-line replacement, emphasising the tokens
/// that differ and leaving the shared head and tail of the line plain.
fn intra_line(old: &str, new: &str, theme: &Theme) -> (String, String) {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let (old_changed, new_changed) = changed_tokens(&old_tokens, &new_tokens);
    (
        emphasise(&old_tokens, &old_changed, theme),
        emphasise(&new_tokens, &new_changed, theme),
    )
}

/// Split a line into alternating runs of whitespace and non-whitespace, so
/// that diffing never invents or drops a character: the runs concatenate back
/// to the input byte for byte.
fn tokenize(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut whitespace: Option<bool> = None;
    for (index, ch) in text.char_indices() {
        let is_whitespace = ch.is_whitespace();
        if whitespace.is_some_and(|previous| previous != is_whitespace) {
            tokens.push(&text[start..index]);
            start = index;
        }
        whitespace = Some(is_whitespace);
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

/// Mark which tokens of each side are not part of the common head, tail, or
/// longest common subsequence of the two lines.
fn changed_tokens(old: &[&str], new: &[&str]) -> (Vec<bool>, Vec<bool>) {
    let mut old_changed = vec![true; old.len()];
    let mut new_changed = vec![true; new.len()];

    let mut head = 0;
    while head < old.len() && head < new.len() && old[head] == new[head] {
        old_changed[head] = false;
        new_changed[head] = false;
        head += 1;
    }

    let mut tail = 0;
    while tail < old.len() - head
        && tail < new.len() - head
        && old[old.len() - 1 - tail] == new[new.len() - 1 - tail]
    {
        old_changed[old.len() - 1 - tail] = false;
        new_changed[new.len() - 1 - tail] = false;
        tail += 1;
    }

    let old_middle = &old[head..old.len() - tail];
    let new_middle = &new[head..new.len() - tail];
    // Longer than the limit: emphasise the whole middle rather than spend a
    // quadratic table on it.
    if old_middle.is_empty()
        || new_middle.is_empty()
        || old_middle.len() > INLINE_LCS_LIMIT
        || new_middle.len() > INLINE_LCS_LIMIT
    {
        return (old_changed, new_changed);
    }

    let (n, m) = (old_middle.len(), new_middle.len());
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            table[i][j] = if old_middle[i - 1] == new_middle[j - 1] {
                table[i - 1][j - 1] + 1
            } else {
                table[i - 1][j].max(table[i][j - 1])
            };
        }
    }
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if old_middle[i - 1] == new_middle[j - 1] {
            old_changed[head + i - 1] = false;
            new_changed[head + j - 1] = false;
            i -= 1;
            j -= 1;
        } else if table[i - 1][j] >= table[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    (old_changed, new_changed)
}

/// Join the tokens back, inverting each run of changed ones. A run's leading
/// whitespace stays outside the inverse so indentation is not highlighted
/// (omp `chrome/diff.ts:59-99`).
fn emphasise(tokens: &[&str], changed: &[bool], theme: &Theme) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < tokens.len() {
        if !changed[i] {
            out.push_str(tokens[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < tokens.len() && changed[i] {
            i += 1;
        }
        let run = tokens[start..i].concat();
        let lead = run.len() - run.trim_start().len();
        out.push_str(&run[..lead]);
        let body = &run[lead..];
        if !body.is_empty() {
            out.push_str(&theme.inverse(body));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, SymbolPreset};
    use serde_json::json;
    use std::collections::HashMap;

    /// The three diff tokens of the built-in `dark` theme, so the expected
    /// escape sequences below are the ones a user actually sees.
    const ADD: &str = "\x1b[38;2;137;210;129m";
    const DEL: &str = "\x1b[38;2;252;58;75m";
    const CTX: &str = "\x1b[38;2;119;125;136m";
    const FG_RESET: &str = "\x1b[39m";
    /// Inverse, as [`Theme::inverse`] emits it.
    const INV: &str = "\x1b[7m";
    const INV_OFF: &str = "\x1b[27m";

    fn theme() -> Theme {
        let mut fg = HashMap::new();
        fg.insert("toolDiffAdded".to_string(), json!("#89d281"));
        fg.insert("toolDiffRemoved".to_string(), json!("#fc3a4b"));
        fg.insert("toolDiffContext".to_string(), json!("#777d88"));
        let mut bg = HashMap::new();
        bg.insert("statusLineBg".to_string(), json!("#000000"));
        Theme::new(
            "test".to_string(),
            fg,
            bg,
            ColorMode::Truecolor,
            SymbolPreset::Unicode,
            HashMap::new(),
            None,
            None,
        )
        .expect("theme builds")
    }

    fn dim(text: &str) -> String {
        format!("{CTX}{text}{FG_RESET}")
    }

    /// Rows of a diff that is expected to parse.
    fn rows(text: &str, width: u16) -> Vec<String> {
        render_diff(text, &theme(), width)
            .expect("blob is a diff")
            .rows
    }

    #[test]
    fn two_hunks_render_context_removal_addition_and_line_numbers() {
        let diff = concat!(
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,4 +1,5 @@\n",
            " //! lib\n",
            "-use std::fmt;\n",
            "+use std::fmt::{self, Debug};\n",
            "+\n",
            " \n",
            " fn main() {}\n",
            "@@ -10,2 +11,2 @@\n",
            " fn tail() {}\n",
            "-// gone\n",
            "+// gone for real\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        assert_eq!(rendered.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -1,4 +1,5 @@"),
                dim("  1   1  //! lib"),
                // Two added lines against one removed: no token pairing.
                format!("{DEL}  2     -use std::fmt;{FG_RESET}"),
                format!("{ADD}      2 +use std::fmt::{{self, Debug}};{FG_RESET}"),
                format!("{ADD}      3 +{FG_RESET}"),
                dim("  3   4  "),
                dim("  4   5  fn main() {}"),
                dim("@@ -10,2 +11,2 @@"),
                dim(" 10  11  fn tail() {}"),
                format!("{DEL} 11     -// gone{FG_RESET}"),
                format!("{ADD}     12 +// gone {INV}for real{INV_OFF}{FG_RESET}"),
            ]
        );
        // Short context runs stay as they are.
        assert!(!rendered.rows.iter().any(|row| row.contains("unchanged")));
    }

    #[test]
    fn a_file_of_additions_has_a_blank_old_column() {
        let diff = concat!(
            "--- /dev/null\n",
            "+++ b/notes/new.txt\n",
            "@@ -0,0 +1,2 @@\n",
            "+first\n",
            "+second\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        assert_eq!(rendered.path.as_deref(), Some("notes/new.txt"));
        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -0,0 +1,2 @@"),
                format!("{ADD}      1 +first{FG_RESET}"),
                format!("{ADD}      2 +second{FG_RESET}"),
            ]
        );
    }

    #[test]
    fn a_file_of_removals_has_a_blank_new_column() {
        let diff = concat!(
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1,3 +0,0 @@\n",
            "-only\n",
            "-here\n",
            "-x\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        assert_eq!(rendered.path.as_deref(), Some("x"));
        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -1,3 +0,0 @@"),
                format!("{DEL}  1     -only{FG_RESET}"),
                format!("{DEL}  2     -here{FG_RESET}"),
                format!("{DEL}  3     -x{FG_RESET}"),
            ]
        );
    }

    #[test]
    fn a_long_line_is_cut_with_an_ellipsis_at_60_80_and_120_columns() {
        let (long_old, long_new) = ("o".repeat(200), "n".repeat(200));
        let diff = format!("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-{long_old}\n+{long_new}\n");

        for (width, text_columns) in [(60u16, 51usize), (80, 71), (120, 111)] {
            let rows = rows(&diff, width);
            for row in &rows {
                assert!(
                    visible_width(row) <= usize::from(width),
                    "{width}: {} cells in {row:?}",
                    visible_width(row)
                );
            }
            let cut_old = format!("{}…", "o".repeat(text_columns - 1));
            let cut_new = format!("{}…", "n".repeat(text_columns - 1));
            assert_eq!(
                rows[1],
                format!("{DEL}  1     -{INV}{cut_old}{INV_OFF}{FG_RESET}")
            );
            assert_eq!(
                rows[2],
                format!("{ADD}      1 +{INV}{cut_new}{INV_OFF}{FG_RESET}")
            );
            assert_eq!(visible_width(&rows[1]), usize::from(width));
        }
    }

    #[test]
    fn a_long_unchanged_run_collapses_to_an_exact_count() {
        let diff = concat!(
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1,12 +1,12 @@\n",
            " one\n",
            " two\n",
            " three\n",
            " four\n",
            " five\n",
            " six\n",
            " seven\n",
            " eight\n",
            " nine\n",
            "-old\n",
            "+new\n",
            " ten\n",
            " eleven\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        // Nine unchanged rows: the first three, the count of the six-row
        // middle minus the three kept at its end, then the last three.
        assert_eq!(rendered.rows.len(), 12);
        assert_eq!(rendered.rows[1], dim("  1   1  one"));
        assert_eq!(rendered.rows[2], dim("  2   2  two"));
        assert_eq!(rendered.rows[3], dim("  3   3  three"));
        assert_eq!(
            rendered.rows[4],
            dim(&format!("{}… 3 unchanged", " ".repeat(9)))
        );
        assert_eq!(rendered.rows[5], dim("  7   7  seven"));
        assert_eq!(rendered.rows[6], dim("  8   8  eight"));
        assert_eq!(rendered.rows[7], dim("  9   9  nine"));
        assert_eq!(
            rendered.rows[8],
            format!("{DEL} 10     -{INV}old{INV_OFF}{FG_RESET}")
        );
        assert_eq!(
            rendered.rows[9],
            format!("{ADD}     10 +{INV}new{INV_OFF}{FG_RESET}")
        );
        assert_eq!(rendered.rows[10], dim(" 11  11  ten"));
        assert_eq!(rendered.rows[11], dim(" 12  12  eleven"));
    }

    #[test]
    fn a_missing_newline_marker_is_a_dim_row_between_its_lines() {
        let diff = concat!(
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "\\ No newline at end of file\n",
            "+new file\n",
            "\\ No newline at end of file\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -1 +1 @@"),
                format!("{DEL}  1     -{INV}old{INV_OFF}{FG_RESET}"),
                dim(&format!("{}\\ No newline at end of file", " ".repeat(9))),
                format!("{ADD}      1 +{INV}new file{INV_OFF}{FG_RESET}"),
                dim(&format!("{}\\ No newline at end of file", " ".repeat(9))),
            ]
        );
    }

    #[test]
    fn a_replacement_of_one_line_emphasises_only_what_changed() {
        let diff = concat!(
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1 +1 @@\n",
            "-    let total = price * 3;\n",
            "+    let total = price * 4;\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        // The indentation and `let total = price * ` are shared, so only the
        // digit is inverted.
        assert_eq!(
            rendered.rows[1],
            format!("{DEL}  1     -    let total = price * {INV}3;{INV_OFF}{FG_RESET}")
        );
        assert_eq!(
            rendered.rows[2],
            format!("{ADD}      1 +    let total = price * {INV}4;{INV_OFF}{FG_RESET}")
        );
    }

    #[test]
    fn runs_of_unequal_length_are_not_compared_token_by_token() {
        let diff = concat!(
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1,2 +1,2 @@\n",
            "-alpha\n",
            "-beta\n",
            "+alpha one\n",
            "+beta two\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -1,2 +1,2 @@"),
                format!("{DEL}  1     -alpha{FG_RESET}"),
                format!("{DEL}  2     -beta{FG_RESET}"),
                format!("{ADD}      1 +alpha one{FG_RESET}"),
                format!("{ADD}      2 +beta two{FG_RESET}"),
            ]
        );
    }

    #[test]
    fn line_numbers_widen_past_three_digits() {
        let diff = concat!(
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1000,3 +1000,3 @@\n",
            " kept\n",
            "-a\n",
            "+b\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");

        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -1000,3 +1000,3 @@"),
                dim("1000 1000  kept"),
                format!("{DEL}1001      -{INV}a{INV_OFF}{FG_RESET}"),
                format!("{ADD}     1001 +{INV}b{INV_OFF}{FG_RESET}"),
            ]
        );
    }

    #[test]
    fn a_plain_message_is_not_a_diff() {
        let theme = theme();
        assert!(render_diff("All tests pass.\nNothing to see here.\n", &theme, 80).is_none());
        assert!(render_diff("", &theme, 80).is_none());
        // A markdown bullet list is not a hunk body.
        assert!(render_diff("- one\n- two\n+ three\n", &theme, 80).is_none());
        // File headers without a hunk have nothing to show either.
        assert!(render_diff("--- a/x\n+++ b/x\n", &theme, 80).is_none());
        // `@@` that is not a hunk header does not start a diff.
        assert!(render_diff("@@ someone's handle\n- nope\n", &theme, 80).is_none());
    }

    #[test]
    fn an_empty_hunk_still_names_its_file() {
        let diff = "--- a/notes/todo.md\n+++ b/notes/todo.md\n@@ -1 +1 @@\n";
        let rendered = render_diff(diff, &theme(), 120).expect("blob is a diff");
        assert_eq!(rendered.path.as_deref(), Some("notes/todo.md"));
        assert_eq!(rendered.rows, vec![dim("@@ -1 +1 @@")]);
    }

    #[test]
    fn a_quoted_path_loses_its_quotes_and_prefix() {
        let diff = concat!(
            "diff --git \"a/odd name.txt\" \"b/odd name.txt\"\n",
            "--- \"a/odd name.txt\"\n",
            "+++ \"b/odd name.txt\"\n",
            "@@ -1 +1 @@\n",
            "-a\n",
            "+b\n",
        );
        let rendered = render_diff(diff, &theme(), 80).expect("blob is a diff");
        assert_eq!(rendered.path.as_deref(), Some("odd name.txt"));
    }

    #[test]
    fn a_pane_narrower_than_the_gutter_keeps_rows_within_width() {
        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-aaa\n+bbb\n";
        for width in [0u16, 1, 5, 8, 10, 11] {
            for row in rows(diff, width) {
                assert!(
                    visible_width(&row) <= usize::from(width),
                    "{width}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn gutter_columns_are_right_aligned_and_blank_where_a_side_has_no_line() {
        let diff = "--- a/x\n+++ b/x\n@@ -8,1 +98,1 @@\n-eight\n+ninety eight\n";
        let rendered = render_diff(diff, &theme(), 40).expect("blob is a diff");
        assert_eq!(
            rendered.rows,
            vec![
                dim("@@ -8,1 +98,1 @@"),
                format!("{DEL}  8     -eight{FG_RESET}"),
                format!("{ADD}     98 +{INV}ninety {INV_OFF}eight{FG_RESET}"),
            ]
        );
    }
}
