//! Transcript markdown renderer with theme tokens and section-visibility model.
//!
//! # Markdown supported
//!
//! No markdown syntax character reaches the screen: every construct is turned
//! into theme colour plus SGR style, and every emitted row fits `width`.
//!
//! - Headings (`#` … `######`) — the `#` run is dropped; levels are told
//!   apart by an SGR stack (1 bold+underline, 2 bold, 3 italic, 4 underline,
//!   5 bold+italic, 6 italic+underline) over `ThemeColor::MdHeading`, with a
//!   blank row before a heading that does not already start a block
//! - Bold (`**text**`) — with `Theme::bold`
//! - Italic (`*text*`, `_text_`; `_` only opens at a word boundary) — with
//!   `Theme::italic`
//! - Inline code (`` `code` ``) — styled with `ThemeColor::MdCode`
//! - Fenced code blocks (```` ```lang ````) — drawn as a box whose border is
//!   `ThemeColor::MdCodeBlockBorder`, the language label appears once in the
//!   top rule, and the body is `ThemeColor::MdCodeBlock`, hard-wrapped to fit
//! - Blockquotes (`> `) — a `MdQuoteBorder` gutter plus `MdQuote` text
//! - Unordered lists (`- `, `* `) — `ThemeColor::MdListBullet`, continuation
//!   rows hang-indented under the first
//! - Ordered lists (`1. `) — numbered, same hanging indent
//! - Horizontal rules (`---`) — styled with `ThemeColor::MdHr`
//! - Links (`[text](url)`) — text with `MdLink`, url with `MdLinkUrl`
//! - GFM tables — a header row, a delimiter row (`|---|:--:|--:|`) and body
//!   rows drawn as a `boxSharp` grid in `ThemeColor::MdCodeBlockBorder`, with
//!   one space of padding per side, column widths from the widest cell's
//!   display width, and per-column alignment; a ragged or malformed block
//!   stays literal text rather than losing a cell
//! - Paragraphs — wrapped to `width`
//!
//! # Section visibility
//!
//! The transcript is split into named sections: `thinking`, `tools`,
//! `subagents`, `activity`.  Each has a default mode per the DoD (thinking
//! and tools expanded, subagents collapsed, activity hidden).
//! `SectionVisibility::apply` implements `/details <section> <mode>`.

use crate::theme::{Theme, ThemeColor};
use crate::width::{replace_tabs, truncate_to_width, visible_width, wrap_text_with_ansi};

// ---------------------------------------------------------------------------
// Section visibility
// ---------------------------------------------------------------------------

/// Per-section display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionMode {
    Hidden,
    Collapsed,
    Expanded,
}

/// Named transcript sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Thinking,
    Tools,
    Subagents,
    Activity,
}

impl Section {
    /// Parse a section name (`thinking`, `tools`, `subagents`, `activity`).
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "thinking" => Some(Section::Thinking),
            "tools" => Some(Section::Tools),
            "subagents" => Some(Section::Subagents),
            "activity" => Some(Section::Activity),
            _ => None,
        }
    }
}

/// Visibility state for each transcript section.
///
/// Defaults per DoD: thinking and tools expanded, subagents collapsed,
/// activity hidden.
#[derive(Debug, Clone)]
pub struct SectionVisibility {
    thinking: SectionMode,
    tools: SectionMode,
    subagents: SectionMode,
    activity: SectionMode,
}

impl Default for SectionVisibility {
    fn default() -> Self {
        SectionVisibility {
            thinking: SectionMode::Expanded,
            tools: SectionMode::Expanded,
            subagents: SectionMode::Collapsed,
            activity: SectionMode::Hidden,
        }
    }
}

impl SectionVisibility {
    /// Get the mode for a section.
    pub fn get(&self, section: Section) -> SectionMode {
        match section {
            Section::Thinking => self.thinking,
            Section::Tools => self.tools,
            Section::Subagents => self.subagents,
            Section::Activity => self.activity,
        }
    }

    /// Set the mode for a section.
    pub fn set(&mut self, section: Section, mode: SectionMode) {
        match section {
            Section::Thinking => self.thinking = mode,
            Section::Tools => self.tools = mode,
            Section::Subagents => self.subagents = mode,
            Section::Activity => self.activity = mode,
        }
    }

    /// Apply a `/details` directive.  Returns `true` if the state changed.
    ///
    /// Accepts `"hidden"`, `"collapsed"`, `"expanded"`, and `"cycle"` (next
    /// in the order: hidden → collapsed → expanded → hidden).
    pub fn apply(&mut self, section: Section, mode_str: &str) -> bool {
        let mode = match mode_str.trim().to_lowercase().as_str() {
            "hidden" => Some(SectionMode::Hidden),
            "collapsed" => Some(SectionMode::Collapsed),
            "expanded" => Some(SectionMode::Expanded),
            "cycle" => {
                let current = self.get(section);
                Some(match current {
                    SectionMode::Hidden => SectionMode::Collapsed,
                    SectionMode::Collapsed => SectionMode::Expanded,
                    SectionMode::Expanded => SectionMode::Hidden,
                })
            }
            _ => None,
        };
        match mode {
            Some(m) => {
                let old = self.get(section);
                self.set(section, m);
                old != m
            }
            None => false,
        }
    }

    /// Whether all sections are hidden — the app should show a floating alert.
    pub fn all_hidden(&self) -> bool {
        matches!(self.thinking, SectionMode::Hidden)
            && matches!(self.tools, SectionMode::Hidden)
            && matches!(self.subagents, SectionMode::Hidden)
            && matches!(self.activity, SectionMode::Hidden)
    }
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

/// Render a markdown string to themed terminal lines.
///
/// `text` — raw markdown; `theme` — the active theme (provides token colours
/// and bold/italic helpers); `width` — column width to wrap paragraphs to.
pub fn render_markdown(text: &str, theme: &Theme, width: u16) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let w = width as usize;
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_lines = Vec::new();

    // Indexed rather than `for … in lines()`: a table needs the rows after the
    // header, so the loop must be able to look ahead and consume several.
    let source: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < source.len() {
        let idx = i;
        let raw = source[idx];
        i += 1;
        if in_code_block {
            if raw.trim().starts_with("```") {
                // End of code block.
                lines.append(&mut render_code_block(&code_lines, &code_lang, theme, w));
                code_lines.clear();
                code_lang.clear();
                in_code_block = false;
                continue;
            }
            code_lines.push(raw);
            continue;
        }

        // Fenced code block start.
        if let Some(lang) = raw.trim().strip_prefix("```") {
            in_code_block = true;
            code_lang = lang.trim().to_owned();
            code_lines.clear();
            continue;
        }

        let trimmed = raw.trim();

        // Horizontal rule.
        if matches!(trimmed, "---" | "***" | "___") {
            lines.push(theme.fg(ThemeColor::MdHr, &"─".repeat(w.saturating_sub(1))));
            continue;
        }

        // Heading.  The `#` run is syntax, so it never reaches the screen;
        // the level is carried by the SGR stack instead.
        let hashes = raw.chars().take_while(|c| *c == '#').count();
        if hashes > 0 {
            let rest = &raw[hashes..];
            if rest.is_empty() || rest.starts_with(' ') {
                let content = style_inline(rest.trim(), theme);
                let body = styled(
                    theme,
                    ThemeColor::MdHeading,
                    heading_styles(hashes),
                    &content,
                );
                // A heading opens a block; give it a leading blank row unless
                // it is the first row or one is already there.
                if lines.last().is_some_and(|l| !l.is_empty()) {
                    lines.push(String::new());
                }
                lines.extend(wrap_text_with_ansi(&body, w));
                continue;
            }
        }

        // GFM table: a header row, a delimiter row, then body rows.  A
        // mismatched delimiter or a ragged row is not a table, so the literal
        // text still reaches the screen instead of losing a cell.
        if let Some((table, used)) = parse_table(&source[idx..]) {
            let raw = &source[idx..idx + used];
            if table_fits(table.header.len(), w) {
                // A table opens a block, like a heading: a leading blank row
                // unless it already starts the answer or follows one.
                if lines.last().is_some_and(|l| !l.is_empty()) {
                    lines.push(String::new());
                }
                lines.append(&mut render_table(&table, theme, w));
            } else {
                lines.append(&mut literal_rows(raw, theme, w));
            }
            i = idx + used;
            continue;
        }

        // Blockquote: a gutter, never a literal `>`.
        if let Some(content) = raw.strip_prefix('>') {
            let content = content.trim();
            let styled = style_inline(content, theme);
            let wrapped = wrap_text_with_ansi(&styled, w.saturating_sub(2).max(1));
            for (i, wline) in wrapped.iter().enumerate() {
                let border = if i == 0 { "▎ " } else { "  " };
                lines.push(format!(
                    "{}{}",
                    theme.fg(ThemeColor::MdQuoteBorder, border),
                    theme.fg(ThemeColor::MdQuote, wline)
                ));
            }
            continue;
        }

        // Lists (ordered and unordered, at any indent).  Continuation rows
        // hang under the first so the markers of a level stay aligned.
        if let Some((indent, marker, body)) = parse_list_item(raw) {
            let indent = indent.min(w.saturating_sub(2));
            let marker_w = visible_width(&marker);
            let prefix_w = indent + marker_w + 1;
            let body_w = w.saturating_sub(prefix_w).max(1);
            let content = style_inline(body, theme);
            let wrapped = wrap_text_with_ansi(&content, body_w);
            let bullet = theme.fg(ThemeColor::MdListBullet, &marker);
            let hang = " ".repeat(prefix_w);
            let pad = " ".repeat(indent);
            for (i, wline) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(format!("{pad}{bullet} {wline}"));
                } else {
                    lines.push(format!("{hang}{wline}"));
                }
            }
            continue;
        }

        // Empty line = paragraph break.
        if trimmed.is_empty() {
            lines.push(String::new());
            continue;
        }

        // Plain paragraph.
        let styled = style_inline(trimmed, theme);
        lines.append(&mut wrap_text_with_ansi(&styled, w));
    }

    // Flush any trailing code block.
    if in_code_block && !code_lines.is_empty() {
        lines.append(&mut render_code_block(&code_lines, &code_lang, theme, w));
    }

    lines
}

/// Render a fenced code block as a box spanning exactly `w` columns.
///
/// The language label rides in the top rule (once, never per line) and the
/// body is hard-wrapped to the interior, so an over-long code line spills onto
/// further interior rows instead of crossing the border.
fn render_code_block(code: &[&str], lang: &str, theme: &Theme, w: usize) -> Vec<String> {
    let bar = theme.fg(ThemeColor::MdCodeBlockBorder, "│");
    let inner = w.saturating_sub(4);
    let mut out = Vec::new();
    if inner == 0 {
        // Too narrow for a frame: keep the body, drop the box.
        for line in code {
            let body = theme.fg(ThemeColor::MdCodeBlock, &replace_tabs(line));
            out.extend(wrap_text_with_ansi(&body, w.max(1)));
        }
        return out;
    }
    let head = if lang.is_empty() {
        "─".to_owned()
    } else {
        format!("─ {lang} ")
    };
    let fill = w.saturating_sub(2 + visible_width(&head));
    out.push(theme.fg(
        ThemeColor::MdCodeBlockBorder,
        &format!("╭{head}{}╮", "─".repeat(fill)),
    ));
    for line in code {
        let body = theme.fg(ThemeColor::MdCodeBlock, &replace_tabs(line));
        for row in wrap_text_with_ansi(&body, inner) {
            let pad = " ".repeat(inner.saturating_sub(visible_width(&row)));
            out.push(format!("{bar} {row}{pad} {bar}"));
        }
    }
    out.push(theme.fg(
        ThemeColor::MdCodeBlockBorder,
        &format!("╰{}╯", "─".repeat(w.saturating_sub(2))),
    ));
    out
}

// ---------------------------------------------------------------------------
// GFM tables
// ---------------------------------------------------------------------------

/// Column alignment taken from a delimiter cell (`---`, `:--`, `--:`, `:-:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableAlign {
    Left,
    Center,
    Right,
}

/// A parsed GFM table.
///
/// Cells hold raw inline markdown: styling happens at render time through
/// [`style_inline`], the same path prose takes, so a second inline parser
/// never has to agree with the first.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TableBlock {
    header: Vec<String>,
    aligns: Vec<TableAlign>,
    rows: Vec<Vec<String>>,
}

/// Body rows folded into one table before the rest falls back to literal text.
const MAX_TABLE_ROWS: usize = 512;
/// Columns a row may carry and still count as a table; wider rows are prose.
const MAX_TABLE_COLS: usize = 32;
/// A column never demands more than this many cells before it wraps.
const MAX_WORD_WIDTH: usize = 30;

/// Split one GFM table row into its cells.
///
/// A `|` escaped as `\|` is literal text, so it neither opens nor closes a
/// cell; the backslash is dropped so the pipe itself reaches the screen.  The
/// leading and trailing pipes are optional and are not cells.  `None` when the
/// line holds no unescaped pipe at all.
fn split_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut pipes = 0usize;
    let mut ended_on_pipe = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cell.push('|');
                ended_on_pipe = false;
            }
            '|' => {
                pipes += 1;
                cells.push(std::mem::take(&mut cell).trim().to_owned());
                ended_on_pipe = true;
            }
            _ => {
                cell.push(c);
                ended_on_pipe = false;
            }
        }
    }
    if pipes == 0 {
        return None;
    }
    if !ended_on_pipe {
        cells.push(cell.trim().to_owned());
    }
    // A leading pipe opens an empty first cell; drop it so `| a |` and `a`
    // name the same single column.
    if trimmed.starts_with('|') {
        cells.remove(0);
    }
    Some(cells)
}

/// The alignment a delimiter cell declares, or `None` when it is not a
/// delimiter (`-` runs only, at most one colon per side).
fn delimiter_align(cell: &str) -> Option<TableAlign> {
    let cell = cell.trim();
    let left = cell.starts_with(':');
    let right = cell.len() > 1 && cell.ends_with(':');
    let body = cell.strip_prefix(':').unwrap_or(cell);
    let body = body.strip_suffix(':').unwrap_or(body);
    if body.is_empty() || !body.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => TableAlign::Center,
        (false, true) => TableAlign::Right,
        _ => TableAlign::Left,
    })
}

/// Parse a table starting at `source[0]` — a header row, a delimiter row, then
/// body rows — returning the block and how many source lines it consumed.
///
/// `None` when the lines are not a table: no header pipe, a delimiter that is
/// not dashes/colons, a delimiter whose cell count differs from the header, or
/// a header wider than [`MAX_TABLE_COLS`].  Body rows stop at the first line
/// that is not a row of the same shape (a blank line, prose, or a ragged row);
/// that line is left to the caller, so nothing is dropped and no cell is
/// invented.
fn parse_table(source: &[&str]) -> Option<(TableBlock, usize)> {
    let header = split_table_row(source.first()?)?;
    if header.is_empty() || header.len() > MAX_TABLE_COLS {
        return None;
    }
    let delim = split_table_row(source.get(1)?)?;
    if delim.len() != header.len() {
        return None;
    }
    let mut aligns = Vec::with_capacity(delim.len());
    for cell in &delim {
        aligns.push(delimiter_align(cell)?);
    }
    let mut rows = Vec::new();
    let mut used = 2;
    for line in &source[2..] {
        if rows.len() >= MAX_TABLE_ROWS {
            break;
        }
        let Some(cells) = split_table_row(line) else {
            break;
        };
        if cells.len() != header.len() {
            break;
        }
        rows.push(cells);
        used += 1;
    }
    Some((
        TableBlock {
            header,
            aligns,
            rows,
        },
        used,
    ))
}

/// Whether a `cols`-column table fits `w`: one cell per column plus the
/// `│ `…` │` border, which is `3n + 1` columns of chrome.
fn table_fits(cols: usize, w: usize) -> bool {
    cols > 0 && w >= 4 * cols + 1
}

/// Render source lines as ordinary paragraph text — the fallback for a block
/// that is not a table, or a table too narrow to draw.
fn literal_rows(raw: &[&str], theme: &Theme, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw {
        let styled = style_inline(line.trim(), theme);
        out.extend(wrap_text_with_ansi(&styled, w.max(1)));
    }
    out
}

/// Display width of the longest whitespace-delimited run in a styled cell.
fn longest_word_width(cell: &str) -> usize {
    cell.split_whitespace()
        .map(visible_width)
        .max()
        .unwrap_or(1)
}

/// Shrink natural column widths to `avail` cells.
///
/// Slack (the room above each column's longest word) is given up
/// proportionally, so columns that wrap anyway wrap together; when even the
/// minimums overflow, every column gets one cell and the rest is handed out by
/// weight.  The result never sums to more than `avail`.
fn fit_widths(natural: &[usize], min: &[usize], avail: usize) -> Vec<usize> {
    let n = natural.len();
    let total: usize = natural.iter().sum();
    if total <= avail {
        return natural.to_vec();
    }
    let mut widths = natural.to_vec();
    let slack: Vec<usize> = (0..n).map(|i| natural[i].saturating_sub(min[i])).collect();
    let total_slack: usize = slack.iter().sum();
    let need = total - avail;
    if total_slack >= need {
        let mut taken = 0usize;
        for i in 0..n {
            let share = slack[i] * need / total_slack;
            widths[i] -= share;
            taken += share;
        }
        // Flooring loses at most one cell per column; shave the widest of
        // those that still have slack.
        let mut left = need - taken;
        while left > 0 {
            let mut best: Option<usize> = None;
            for i in 0..n {
                if widths[i] > min[i] && best.is_none_or(|b| widths[b] < widths[i]) {
                    best = Some(i);
                }
            }
            match best {
                Some(i) => {
                    widths[i] -= 1;
                    left -= 1;
                }
                None => break,
            }
        }
        return widths;
    }
    // Even the longest words do not fit: one cell each, then by weight.
    let mut widths = vec![1usize; n];
    let left = avail - n;
    let weight: Vec<usize> = min.iter().map(|m| m.saturating_sub(1)).collect();
    let total_weight: usize = weight.iter().sum();
    let mut rest = left;
    if total_weight > 0 {
        let mut given = 0usize;
        for i in 0..n {
            let share = weight[i] * left / total_weight;
            widths[i] += share;
            given += share;
        }
        rest = left - given;
    }
    let mut i = 0;
    while rest > 0 {
        widths[i % n] += 1;
        rest -= 1;
        i += 1;
    }
    widths
}

/// Whether the last SGR in `s` leaves a style in effect.
///
/// A cell is assembled from several styled spans, so a cut inside one span can
/// leave a colour opened by an *earlier* span running; the padding and the
/// border glyph that follow would inherit it.
fn style_is_open(s: &str) -> bool {
    let mut open = false;
    for span in crate::width::spans(s) {
        let crate::width::Span::Escape(seq) = span else {
            continue;
        };
        let Some(params) = seq.strip_prefix("\x1b[").and_then(|p| p.strip_suffix('m')) else {
            continue; // not SGR: an OSC 8, a charset switch, …
        };
        if !params.chars().all(|c| c.is_ascii_digit() || c == ';') {
            continue;
        }
        open = !matches!(params, "0" | "39" | "22" | "23" | "24");
    }
    open
}

/// Replace every whitespace-delimited run wider than `width` with a hard cut
/// plus `…`, so [`wrap_text_with_ansi`] never silently clamps one and the cut
/// is visible.
fn cap_long_words(text: &str, width: usize) -> String {
    if visible_width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut first = true;
    for seg in text.split(' ') {
        if !first {
            out.push(' ');
        }
        first = false;
        if visible_width(seg) > width {
            out.push_str(&truncate_to_width(seg, width.saturating_sub(1)));
            out.push('…');
            // `truncate_to_width` closes only a style it saw in its own input.
            if style_is_open(&out) {
                out.push_str("\x1b[39m");
            }
        } else {
            out.push_str(seg);
        }
    }
    out
}

/// Wrap a styled cell to `width` columns on word boundaries, hard-cutting a
/// word that cannot fit.  Always returns at least one (possibly empty) row.
fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let capped = cap_long_words(cell, width);
    let mut rows = wrap_text_with_ansi(&capped, width);
    while rows.len() > 1 && rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// Pad a styled cell to `width` columns, honouring the column alignment.
fn pad_cell(text: &str, width: usize, align: TableAlign) -> String {
    let pad = width.saturating_sub(visible_width(text));
    match align {
        TableAlign::Left => format!("{text}{}", " ".repeat(pad)),
        TableAlign::Right => format!("{}{text}", " ".repeat(pad)),
        TableAlign::Center => {
            let left = pad / 2;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(pad - left))
        }
    }
}

/// Draw a parsed table as a `boxSharp` grid spanning at most `w` columns.
///
/// Column widths come from the widest cell's display width (UAX#11, never
/// bytes), so a CJK or emoji cell pads correctly.  Each cell is wrapped inside
/// its column; the frame is exactly `3n + 1 + Σwidth` columns, which
/// [`table_fits`] has already checked against the pane.
fn render_table(table: &TableBlock, theme: &Theme, w: usize) -> Vec<String> {
    let n = table.header.len();
    debug_assert!(table_fits(n, w));
    let avail = w.saturating_sub(3 * n + 1);
    let border = |s: &str| theme.fg(ThemeColor::MdCodeBlockBorder, s);
    let h = theme.symbol("boxSharp.horizontal").to_owned();
    let v = theme.symbol("boxSharp.vertical").to_owned();

    // Render every cell through the prose inline path once, then measure.
    // Tabs are expanded first: a raw `\t` in a cell would otherwise measure
    // eight columns and knock the frame out of alignment.
    let header_cells: Vec<String> = table
        .header
        .iter()
        .map(|c| style_inline(&replace_tabs(c), theme))
        .collect();
    let body_cells: Vec<Vec<String>> = table
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| style_inline(&replace_tabs(c), theme))
                .collect()
        })
        .collect();

    let mut natural = vec![0usize; n];
    let mut min = vec![1usize; n];
    for cells in std::iter::once(&header_cells).chain(body_cells.iter()) {
        for (i, cell) in cells.iter().enumerate() {
            natural[i] = natural[i].max(visible_width(cell));
            min[i] = min[i].max(longest_word_width(cell).clamp(1, MAX_WORD_WIDTH));
        }
    }
    let widths = fit_widths(&natural, &min, avail);

    // A rule with `left`/`mid`/`right` joints (symbol keys), one segment per
    // column.
    let rule = |left: &str, mid: &str, right: &str| -> String {
        let mut s = String::new();
        s.push_str(theme.symbol(left));
        s.push_str(&h);
        for (i, cw) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(&h);
                s.push_str(theme.symbol(mid));
                s.push_str(&h);
            }
            s.push_str(&h.repeat(*cw));
        }
        s.push_str(&h);
        s.push_str(theme.symbol(right));
        border(&s)
    };

    // One logical row, its cells wrapped and padded line by line.  The bars
    // carry the border token like the rules, so a cell's own colour never
    // bleeds into the frame.
    let vbar = border(&v);
    let row_lines = |cells: &[String], bold: bool| -> Vec<String> {
        let wrapped: Vec<Vec<String>> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| wrap_cell(c, widths[i]))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        let mut rows = Vec::with_capacity(height);
        for line in 0..height {
            let mut s = String::new();
            s.push_str(&vbar);
            for (i, cell_lines) in wrapped.iter().enumerate() {
                let text = cell_lines.get(line).map(String::as_str).unwrap_or("");
                let padded = pad_cell(text, widths[i], table.aligns[i]);
                s.push(' ');
                s.push_str(&if bold { theme.bold(&padded) } else { padded });
                s.push(' ');
                s.push_str(&vbar);
            }
            rows.push(s);
        }
        rows
    };

    let mut out = Vec::new();
    out.push(rule(
        "boxSharp.topLeft",
        "boxSharp.teeDown",
        "boxSharp.topRight",
    ));
    out.extend(row_lines(&header_cells, true));
    out.push(rule(
        "boxSharp.teeRight",
        "boxSharp.cross",
        "boxSharp.teeLeft",
    ));
    for (i, row) in body_cells.iter().enumerate() {
        out.extend(row_lines(row, false));
        if i + 1 < body_cells.len() {
            out.push(rule(
                "boxSharp.teeRight",
                "boxSharp.cross",
                "boxSharp.teeLeft",
            ));
        }
    }
    out.push(rule(
        "boxSharp.bottomLeft",
        "boxSharp.teeUp",
        "boxSharp.bottomRight",
    ));
    out
}

/// Inline text style that can be composed into one SGR open sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Bold,
    Italic,
    Underline,
}

impl Style {
    fn on(self) -> &'static str {
        match self {
            Style::Bold => "1",
            Style::Italic => "3",
            Style::Underline => "4",
        }
    }

    fn off(self) -> &'static str {
        match self {
            Style::Bold => "22",
            Style::Italic => "23",
            Style::Underline => "24",
        }
    }
}

/// The SGR stack per heading level: a level is told apart by style, since the
/// `#` run itself is never printed.
fn heading_styles(level: usize) -> &'static [Style] {
    use Style::{Bold, Italic, Underline};
    match level {
        1 => &[Bold, Underline],
        2 => &[Bold],
        3 => &[Italic],
        4 => &[Underline],
        5 => &[Bold, Italic],
        _ => &[Italic, Underline],
    }
}

/// Wrap `text` in theme token `color` plus `styles`.
///
/// Opens with one combined SGR so `wrap_text_with_ansi` re-emits the whole
/// style on a continued row, and closes each attribute on its own in reverse
/// order so an enclosing span survives a nested one.
fn styled(theme: &Theme, color: ThemeColor, styles: &[Style], text: &str) -> String {
    if styles.is_empty() {
        return theme.fg(color, text);
    }
    let fg = theme.get_fg_ansi(color);
    // `\x1b[38;2;r;g;bm` / `\x1b[39m` -> `38;2;r;g;b` / `39`.
    let params = fg
        .strip_prefix("\x1b[")
        .and_then(|s| s.strip_suffix('m'))
        .unwrap_or("39");
    let mut out = String::with_capacity(text.len() + 32);
    out.push_str("\x1b[");
    for s in styles {
        out.push_str(s.on());
        out.push(';');
    }
    out.push_str(params);
    out.push('m');
    out.push_str(text);
    out.push_str("\x1b[39m");
    for s in styles.iter().rev() {
        out.push_str("\x1b[");
        out.push_str(s.off());
        out.push('m');
    }
    out
}

/// Split a list row into `(indent columns, marker, body)`.
///
/// Recognises `- `, `* `, and `N. ` after any leading indentation, so nested
/// lists keep their depth instead of collapsing to column zero.
fn parse_list_item(raw: &str) -> Option<(usize, String, &str)> {
    let body = raw.trim_start_matches([' ', '\t']);
    let indent = raw.len() - body.len();
    if let Some(rest) = body.strip_prefix("- ").or_else(|| body.strip_prefix("* ")) {
        return Some((indent, "•".to_owned(), rest.trim_end()));
    }
    let digits: &str = &body[..body.chars().take_while(char::is_ascii_digit).count()];
    if !digits.is_empty()
        && let Some(rest) = body[digits.len()..].strip_prefix(". ")
    {
        return Some((indent, format!("{digits}."), rest.trim_end()));
    }
    None
}

/// Style inline markdown in a single line of text.
///
/// Handles `` `code` ``, `[text](url)`, `**bold**`, `*italic*` and
/// `_italic_`.  Processes the earliest marker first and recurses into prefixes
/// so nested/staged markers (e.g. bold before an inline code span) all render.
fn style_inline(text: &str, theme: &Theme) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let code_at = rest.find('`');
        let link_at = rest.find('[');
        let bold_at = rest.find("**");
        let italic_at = rest.find('*');
        let underscore_at = underscore_italic_at(rest);

        // Earliest marker wins; on ties code > link > bold > italic.
        let mut best: Option<(usize, &str)> = None;
        for (i, kind) in [
            (code_at, "code"),
            (link_at, "link"),
            (bold_at, "bold"),
            (italic_at, "italic"),
            (underscore_at, "underscore"),
        ] {
            let Some(i) = i else { continue };
            if kind == "italic" && bold_at == Some(i) {
                continue; // part of a bold pair
            }
            if best.is_none_or(|(b, _)| i < b) {
                best = Some((i, kind));
            }
        }

        let Some((i, kind)) = best else {
            out.push_str(rest);
            break;
        };

        // Recurse into the plain prefix so markers before this one render.
        out.push_str(&style_inline(&rest[..i], theme));
        rest = &rest[i..];

        match kind {
            "code" => {
                rest = &rest[1..];
                if let Some(end) = rest.find('`') {
                    out.push_str(&theme.fg(ThemeColor::MdCode, &rest[..end]));
                    rest = &rest[end + 1..];
                } else {
                    out.push('`');
                }
            }
            "link" => {
                rest = &rest[1..];
                if let Some(end) = rest.find(']') {
                    let link_text = &rest[..end];
                    rest = &rest[end + 1..];
                    if rest.starts_with('(') {
                        rest = &rest[1..];
                        if let Some(url_end) = rest.find(')') {
                            let url = &rest[..url_end];
                            out.push_str(&theme.fg(ThemeColor::MdLink, link_text));
                            out.push_str(&theme.fg(ThemeColor::MdLinkUrl, &format!(" ({url})")));
                            rest = &rest[url_end + 1..];
                            continue;
                        }
                    }
                    // No matching URL — literal.
                    out.push('[');
                    out.push_str(link_text);
                    out.push(']');
                } else {
                    out.push('[');
                }
            }
            "bold" => {
                rest = &rest[2..];
                if let Some(end) = rest.find("**") {
                    let inner = style_inline(&rest[..end], theme);
                    out.push_str(&theme.bold(&inner));
                    rest = &rest[end + 2..];
                } else {
                    out.push_str("**");
                }
            }
            "italic" => {
                rest = &rest[1..];
                if let Some(end) = rest.find('*') {
                    // Do not treat a `**` closing as a lone italic marker.
                    let inner = style_inline(&rest[..end], theme);
                    out.push_str(&theme.italic(&inner));
                    rest = &rest[end + 1..];
                } else {
                    out.push('*');
                }
            }
            "underscore" => {
                rest = &rest[1..];
                if let Some(end) = rest.find('_') {
                    let inner = style_inline(&rest[..end], theme);
                    out.push_str(&theme.italic(&inner));
                    rest = &rest[end + 1..];
                } else {
                    out.push('_');
                }
            }
            _ => unreachable!(),
        }
    }
    out
}

/// Byte offset of the first `_` that can open emphasis: at a word boundary and
/// followed by a matching `_` that itself ends at a word boundary.  Identifiers
/// such as `snake_case_idents` are therefore left alone.
fn underscore_italic_at(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = text[from..].find('_') {
        let open = from + offset;
        let opens_at_boundary = !text[..open]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        if opens_at_boundary && let Some(offset) = text[open + 1..].find('_') {
            let close = open + 1 + offset;
            let closes_at_boundary = !text[close + 1..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
            // `__` is not a valid single-underscore closer.
            if closes_at_boundary && !text[..close].ends_with('_') {
                return Some(open);
            }
        }
        from = open + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use std::collections::HashMap;

    fn test_theme() -> Theme {
        Theme::new(
            "test".into(),
            HashMap::new(),
            HashMap::new(),
            crate::theme::ColorMode::Color256,
            crate::theme::SymbolPreset::Unicode,
            HashMap::new(),
            None,
            None,
        )
        .expect("theme builds")
    }

    // ---- SectionVisibility ------------------------------------------------

    #[test]
    fn section_defaults() {
        let v = SectionVisibility::default();
        assert_eq!(v.get(Section::Thinking), SectionMode::Expanded);
        assert_eq!(v.get(Section::Tools), SectionMode::Expanded);
        assert_eq!(v.get(Section::Subagents), SectionMode::Collapsed);
        assert_eq!(v.get(Section::Activity), SectionMode::Hidden);
        assert!(!v.all_hidden());
    }

    #[test]
    fn section_apply_hidden() {
        let mut v = SectionVisibility::default();
        assert!(v.apply(Section::Thinking, "hidden"));
        assert_eq!(v.get(Section::Thinking), SectionMode::Hidden);
    }

    #[test]
    fn section_apply_cycle_through() {
        let mut v = SectionVisibility::default();
        assert_eq!(v.get(Section::Thinking), SectionMode::Expanded);
        assert!(v.apply(Section::Thinking, "cycle"));
        assert_eq!(v.get(Section::Thinking), SectionMode::Hidden);
        assert!(v.apply(Section::Thinking, "cycle"));
        assert_eq!(v.get(Section::Thinking), SectionMode::Collapsed);
        assert!(v.apply(Section::Thinking, "cycle"));
        assert_eq!(v.get(Section::Thinking), SectionMode::Expanded);
    }

    #[test]
    fn section_apply_invalid_noop() {
        let mut v = SectionVisibility::default();
        assert!(!v.apply(Section::Thinking, "bogus"));
        assert_eq!(v.get(Section::Thinking), SectionMode::Expanded);
    }

    #[test]
    fn section_invalid_name() {
        assert!(Section::parse("bogus").is_none());
        assert_eq!(Section::parse("thinking"), Some(Section::Thinking));
        assert_eq!(Section::parse("tools"), Some(Section::Tools));
        assert_eq!(Section::parse("subagents"), Some(Section::Subagents));
        assert_eq!(Section::parse("activity"), Some(Section::Activity));
    }

    #[test]
    fn all_hidden_true_when_everything_hidden() {
        let mut v = SectionVisibility::default();
        v.apply(Section::Thinking, "hidden");
        v.apply(Section::Tools, "hidden");
        v.apply(Section::Subagents, "hidden");
        v.apply(Section::Activity, "hidden");
        assert!(v.all_hidden());
    }

    // ---- Markdown rendering -----------------------------------------------

    /// Distinct colour per markdown token, so a row assertion can tell the
    /// heading, frame, body, bullet and quote spans apart.
    fn colored_theme() -> Theme {
        let mut fg = HashMap::new();
        for (k, v) in [
            ("mdHeading", "#ffcc00"),
            ("mdLink", "#4da6ff"),
            ("mdLinkUrl", "#7f7f7f"),
            ("mdCode", "#ff7b72"),
            ("mdCodeBlock", "#c9d1d9"),
            ("mdCodeBlockBorder", "#444444"),
            ("mdQuote", "#8b949e"),
            ("mdQuoteBorder", "#58a6ff"),
            ("mdListBullet", "#ffcc00"),
        ] {
            fg.insert(k.to_string(), serde_json::json!(v));
        }
        Theme::new(
            "colored".into(),
            fg,
            HashMap::new(),
            crate::theme::ColorMode::Truecolor,
            crate::theme::SymbolPreset::Unicode,
            HashMap::new(),
            None,
            None,
        )
        .expect("colored theme builds")
    }

    /// Rows with every escape sequence dropped, for structural assertions.
    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                crate::width::spans(l)
                    .filter_map(|s| match s {
                        crate::width::Span::Text(t) => Some(t),
                        crate::width::Span::Escape(_) => None,
                    })
                    .collect()
            })
            .collect()
    }

    /// A realistic assistant answer using every construct the renderer knows.
    const MIXED: &str = "\
# Report

Short paragraph with `code`, *emphasis* and **weight**.

- first item
  - nested item

> quoted rule

```rust
let plan = app.plan_frame(input, height);
let wider_than_the_renderer_width = plan + 1;
```

Done in `AGENTS.md`.";

    #[test]
    fn empty_text() {
        let lines = render_markdown("", &test_theme(), 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn heading_has_no_hash_prefix() {
        let theme = colored_theme();
        let lines = render_markdown("# Hello", &theme, 80);
        assert_eq!(
            lines,
            vec!["\x1b[1;4;38;2;255;204;0mHello\x1b[39m\x1b[24m\x1b[22m".to_string()]
        );
    }

    #[test]
    fn heading_levels_are_distinguishable() {
        let theme = colored_theme();
        let l1 = render_markdown("# A", &theme, 80);
        let l2 = render_markdown("## A", &theme, 80);
        let l3 = render_markdown("### A", &theme, 80);
        assert_eq!(plain(&l1), vec!["A"]);
        assert_eq!(plain(&l2), vec!["A"]);
        assert_eq!(plain(&l3), vec!["A"]);
        assert!(l1[0].starts_with("\x1b[1;4;38;2;255;204;0m"), "{:?}", l1[0]);
        assert!(l2[0].starts_with("\x1b[1;38;2;255;204;0m"), "{:?}", l2[0]);
        assert!(l3[0].starts_with("\x1b[3;38;2;255;204;0m"), "{:?}", l3[0]);
    }

    #[test]
    fn heading_opens_a_block_with_a_blank_row() {
        let theme = test_theme();
        assert_eq!(
            plain(&render_markdown("text\n## Head", &theme, 80)),
            vec!["text", "", "Head"]
        );
        assert_eq!(
            plain(&render_markdown("# Head\n\ntext", &theme, 80)),
            vec!["Head", "", "text"]
        );
        assert_eq!(
            plain(&render_markdown("# A\n## B", &theme, 80)),
            vec!["A", "", "B"]
        );
    }

    #[test]
    fn bold_rendered() {
        let theme = test_theme();
        let lines = render_markdown("this is **bold** text", &theme, 80);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("\x1b[1m"),
            "bold should use ANSI bold: {lines:?}"
        );
        // The bold word itself must appear without its markers.
        assert!(lines[0].contains("bold"), "bold word should appear");
    }

    #[test]
    fn italic_rendered() {
        let theme = test_theme();
        let lines = render_markdown("this is *italic* text", &theme, 80);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("\x1b[3m"),
            "italic should use ANSI italic: {lines:?}"
        );
    }

    #[test]
    fn inline_code_rendered() {
        let theme = test_theme();
        let lines = render_markdown("use `ffmpeg` to convert", &theme, 80);
        // mdCode token not in test theme (empty map), so returns to default.
        // The word `ffmpeg` should be present.
        assert!(
            lines[0].contains("ffmpeg"),
            "code word should appear: {lines:?}"
        );
    }

    #[test]
    fn inline_code_italic_and_bold_in_one_paragraph() {
        let theme = colored_theme();
        let lines = render_markdown("use `cargo test` *now* and **never** later", &theme, 80);
        assert_eq!(
            lines,
            vec![
                "use \x1b[38;2;255;123;114mcargo test\x1b[39m \x1b[3mnow\x1b[23m \
                 and \x1b[1mnever\x1b[22m later"
                    .to_string()
            ]
        );
    }

    #[test]
    fn underscore_italic_skips_identifiers() {
        let theme = test_theme();
        let lines = render_markdown("say _hello_ to snake_case_idents", &theme, 80);
        assert_eq!(plain(&lines), vec!["say hello to snake_case_idents"]);
        assert!(lines[0].contains("\x1b[3mhello\x1b[23m"), "{:?}", lines[0]);
    }

    #[test]
    fn link_rendered() {
        let theme = test_theme();
        let lines = render_markdown("click [here](https://example.com)", &theme, 80);
        assert!(
            lines[0].contains("here"),
            "link text should appear: {lines:?}"
        );
        assert!(
            lines[0].contains("example.com"),
            "url should appear: {lines:?}"
        );
    }

    #[test]
    fn unordered_list_rendered() {
        let theme = test_theme();
        let lines = render_markdown("- item one\n- item two", &theme, 80);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("item one"), "first item: {lines:?}");
        assert!(lines[1].contains("item two"), "second item: {lines:?}");
    }

    #[test]
    fn ordered_list_rendered() {
        let theme = test_theme();
        let lines = render_markdown("1. first\n2. second", &theme, 80);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("first"), "first item: {lines:?}");
        assert!(lines[1].contains("second"), "second item: {lines:?}");
    }

    #[test]
    fn nested_list_markers_align() {
        let theme = test_theme();
        let rows = plain(&render_markdown(
            "- outer\n  - inner\n    - deeper\n10. tenth",
            &theme,
            80,
        ));
        assert_eq!(
            rows,
            vec!["• outer", "  • inner", "    • deeper", "10. tenth"]
        );
    }

    #[test]
    fn list_continuation_hangs_under_the_first_row() {
        let theme = test_theme();
        let rows = plain(&render_markdown("10. alpha beta gamma delta", &theme, 16));
        // The wrap keeps the space it broke on; the hang indent follows the
        // marker width, so `gamma` starts under `alpha`.
        assert_eq!(rows, vec!["10. alpha beta ", "    gamma delta"]);
        for row in render_markdown("10. alpha beta gamma delta", &theme, 16) {
            assert!(visible_width(&row) <= 16, "{row:?}");
        }
    }

    #[test]
    fn blockquote_rendered() {
        let theme = test_theme();
        let lines = render_markdown("> quoted text", &theme, 80);
        assert!(lines[0].contains("quoted text"), "blockquote: {lines:?}");
        // The border character should be present.
        assert!(lines[0].contains("▎"), "blockquote border: {lines:?}");
    }

    #[test]
    fn blockquote_uses_a_gutter_not_a_literal_gt() {
        let theme = colored_theme();
        let lines = render_markdown("> one **two**", &theme, 80);
        assert_eq!(
            lines,
            vec![
                "\x1b[38;2;88;166;255m▎ \x1b[39m\x1b[38;2;139;148;158mone \x1b[1mtwo\x1b[22m\x1b[39m"
                    .to_string()
            ]
        );
        assert!(!lines[0].contains('>'));
    }

    #[test]
    fn code_block_draws_a_frame_with_the_language_once() {
        let theme = colored_theme();
        for w in [60usize, 80usize] {
            let lines = render_markdown("```rust\nfn main() {}\n```", &theme, w as u16);
            let expected = vec![
                format!("\x1b[38;2;68;68;68m╭─ rust {}╮\x1b[39m", "─".repeat(w - 9)),
                format!(
                    "\x1b[38;2;68;68;68m│\x1b[39m \x1b[38;2;201;209;217mfn main() {{}}\x1b[39m{} \x1b[38;2;68;68;68m│\x1b[39m",
                    " ".repeat(w - 16)
                ),
                format!("\x1b[38;2;68;68;68m╰{}╯\x1b[39m", "─".repeat(w - 2)),
            ];
            assert_eq!(lines, expected, "w={w}");
            for row in &lines {
                assert_eq!(visible_width(row), w, "w={w} row={row:?}");
            }
            assert_eq!(
                plain(&lines)[0].matches("rust").count(),
                1,
                "language label is printed once, at w={w}"
            );
        }
    }

    #[test]
    fn code_block_without_language_has_a_plain_top_rule() {
        let theme = colored_theme();
        let lines = render_markdown("```\nlet x = 1;\n```", &theme, 60);
        assert_eq!(
            plain(&lines),
            vec![
                format!("╭{}╮", "─".repeat(58)),
                format!("│ let x = 1;{} │", " ".repeat(46)),
                format!("╰{}╯", "─".repeat(58)),
            ]
        );
        for row in &lines {
            assert_eq!(visible_width(row), 60, "{row:?}");
        }
    }

    #[test]
    fn overlong_code_line_wraps_inside_the_frame() {
        let theme = colored_theme();
        let long = "a".repeat(70);
        let lines = render_markdown(&format!("```\n{long}\n```"), &theme, 60);
        assert_eq!(lines.len(), 4, "top, two body rows, bottom: {lines:?}");
        for row in &lines {
            assert_eq!(visible_width(row), 60, "{row:?}");
        }
        let rows = plain(&lines);
        assert!(rows[0].starts_with("╭"));
        assert_eq!(rows[1], format!("│ {} │", "a".repeat(56)));
        assert_eq!(rows[2], format!("│ {}{} │", "a".repeat(14), " ".repeat(42)));
        assert!(rows[3].starts_with("╰"));
    }

    #[test]
    fn horizontal_rule_rendered() {
        let theme = test_theme();
        let lines = render_markdown("---", &theme, 80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains('─'), "hr: {lines:?}");
    }

    #[test]
    fn paragraph_wrapping() {
        let theme = test_theme();
        let long = "This is a very long paragraph that should wrap at the given width limit.";
        let lines = render_markdown(long, &theme, 20);
        // At width 20, should produce at least 2 lines.
        assert!(lines.len() >= 2, "should wrap: {lines:?}");
    }

    #[test]
    fn multiple_heading_levels() {
        let theme = test_theme();
        let lines = render_markdown("## Subheading\n### Subsub", &theme, 80);
        assert_eq!(plain(&lines), vec!["Subheading", "", "Subsub"]);
        assert!(lines.iter().all(|l| !l.contains('#')), "{lines:?}");
    }

    #[test]
    fn bold_and_italic_together() {
        let theme = test_theme();
        let lines = render_markdown("**bold** and *italic*", &theme, 80);
        assert!(lines[0].contains("\x1b[1m"), "bold marker");
        assert!(lines[0].contains("\x1b[3m"), "italic marker");
    }

    #[test]
    fn mixed_document_renders_every_construct() {
        let theme = colored_theme();
        let lines = render_markdown(MIXED, &theme, 60);
        assert_eq!(
            plain(&lines),
            vec![
                "Report".to_string(),
                String::new(),
                "Short paragraph with code, emphasis and weight.".to_string(),
                String::new(),
                "• first item".to_string(),
                "  • nested item".to_string(),
                String::new(),
                "▎ quoted rule".to_string(),
                String::new(),
                format!("╭─ rust {}╮", "─".repeat(51)),
                format!(
                    "│ let plan = app.plan_frame(input, height);{} │",
                    " ".repeat(15)
                ),
                format!(
                    "│ let wider_than_the_renderer_width = plan + 1;{} │",
                    " ".repeat(11)
                ),
                format!("╰{}╯", "─".repeat(58)),
                String::new(),
                "Done in AGENTS.md.".to_string(),
            ]
        );
        // No markdown syntax character reaches the screen.  (`_` is not in the
        // list: it is legitimate content inside identifiers like
        // `plan_frame`.)
        for row in plain(&lines) {
            for syntax in ['#', '`', '>', '*'] {
                assert!(!row.contains(syntax), "{row:?} still carries {syntax:?}");
            }
        }
        // Inline styles ride on SGR, not on literal markers.
        assert!(
            lines[2].contains("\x1b[38;2;255;123;114mcode\x1b[39m"),
            "{:?}",
            lines[2]
        );
        assert!(
            lines[2].contains("\x1b[3memphasis\x1b[23m"),
            "{:?}",
            lines[2]
        );
        assert!(lines[2].contains("\x1b[1mweight\x1b[22m"), "{:?}", lines[2]);
        assert!(
            lines[0].starts_with("\x1b[1;4;38;2;255;204;0mReport"),
            "{:?}",
            lines[0]
        );
        assert!(
            lines[4].contains("\x1b[38;2;255;204;0m•\x1b[39m"),
            "{:?}",
            lines[4]
        );
        // Frame border and code body colours.
        assert!(lines[9].contains("\x1b[38;2;68;68;68m"), "{:?}", lines[9]);
        assert!(
            lines[10].contains("\x1b[38;2;201;209;217m"),
            "{:?}",
            lines[10]
        );
    }

    #[test]
    fn wrapped_heading_keeps_its_style_without_broken_escapes() {
        let theme = colored_theme();
        let lines = render_markdown(
            "# A heading that is long enough to wrap at forty columns `code`",
            &theme,
            40,
        );
        assert!(lines.len() >= 2, "should wrap: {lines:?}");
        for row in &lines {
            assert!(visible_width(row) <= 40, "{row:?}");
            // The combined SGR is re-emitted after a break, so a continued
            // heading row keeps the heading style.
            assert!(row.starts_with("\x1b[1;4;"), "{row:?}");
            // Stripping the escapes leaves no ESC behind: none was split.
            assert!(
                !plain(std::slice::from_ref(row))[0].contains('\x1b'),
                "{row:?}"
            );
        }
        assert!(
            plain(&lines).concat().contains("code"),
            "inline code survives the wrap: {lines:?}"
        );
    }

    #[test]
    fn no_row_exceeds_the_pane_width() {
        let theme = colored_theme();
        for w in [20u16, 40, 60, 80, 120] {
            for row in render_markdown(MIXED, &theme, w) {
                assert!(
                    visible_width(&row) <= w as usize,
                    "w={w} width={} row={row:?}",
                    visible_width(&row)
                );
            }
        }
    }

    // ---- GFM tables -------------------------------------------------------

    /// A two-column table with a CJK cell and an emoji cell: every column must
    /// be as wide as the widest cell's *display* width, so the borders line up.
    /// Counting chars instead would make the CJK column two cells too narrow.
    #[test]
    fn table_column_widths_come_from_display_width() {
        let theme = colored_theme();
        let rows = plain(&render_markdown(
            "| a | 日本 |\n|---|---|\n| b | 🎉c |",
            &theme,
            80,
        ));
        // col0 = 1, col1 = max(4, 3) = 4 → chrome 7 + 5 = 12 columns.
        assert_eq!(
            rows,
            vec![
                "┌───┬──────┐",
                "│ a │ 日本 │",
                "├───┼──────┤",
                "│ b │ 🎉c  │",
                "└───┴──────┘",
            ]
        );
        for row in render_markdown("| a | 日本 |\n|---|---|\n| b | 🎉c |", &theme, 80) {
            assert_eq!(visible_width(&row), 12, "{row:?}");
        }
    }

    /// A table drawn at any pane width stays inside it, however wide the cells
    /// are: an over-wide column wraps, and a word that cannot fit is cut.
    #[test]
    fn table_never_exceeds_the_pane() {
        let theme = colored_theme();
        let md = "| Name | 日本語のテキスト | Note |\n\
                  |:-----|:--------------:|-----:|\n\
                  | supercalifragilisticexpialidocious | 🎉🎉🎉 | ok |\n\
                  | b | c | a much longer note than the header |";
        for w in 12u16..=80 {
            let lines = render_markdown(md, &theme, w);
            for row in &lines {
                assert!(
                    visible_width(row) <= w as usize,
                    "w={w} width={} row={row:?}",
                    visible_width(row)
                );
            }
            // The frame is drawn only when a cell per column fits.
            if w >= 13 {
                assert!(plain(&lines)[0].starts_with('┌'), "w={w} {lines:?}");
            }
        }
    }

    /// `\|` is literal text inside a cell, not a column separator.
    #[test]
    fn escaped_pipe_is_literal_text() {
        let theme = colored_theme();
        let lines = render_markdown("| a \\| b | c |\n|---|---|\n| 1 | 2 |", &theme, 40);
        let rows = plain(&lines);
        assert_eq!(
            rows,
            vec![
                "┌───────┬───┐",
                "│ a | b │ c │",
                "├───────┼───┤",
                "│ 1     │ 2 │",
                "└───────┴───┘",
            ]
        );
    }

    /// The alignment colons of the delimiter row pad the cell: left against
    /// the left edge, center split, right against the right edge.
    #[test]
    fn alignment_pads_each_column() {
        let theme = colored_theme();
        let rows = plain(&render_markdown(
            "| L | C | R |\n|:--|:-:|--:|\n| a | b | c |",
            &theme,
            40,
        ));
        assert_eq!(rows[3], "│ a │ b │ c │");
        // Wider cells make the padding visible.
        let rows = plain(&render_markdown(
            "| Left | Center | Right |\n|:-----|:------:|------:|\n| aa | bb | cc |",
            &theme,
            40,
        ));
        assert_eq!(rows[3], "│ aa   │   bb   │    cc │");
    }

    /// A word wider than its column is hard-cut with `…`, so the cut shows and
    /// the frame still closes.
    #[test]
    fn over_wide_word_is_cut_with_an_ellipsis() {
        let theme = colored_theme();
        let lines = render_markdown(
            "| a | b |\n|---|---|\n| supercalifragilistic | x |",
            &theme,
            20,
        );
        let rows = plain(&lines);
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert!(rows[3].contains('…'), "the cut is marked: {rows:?}");
        assert!(!rows[3].contains("supercalifragilistic"), "{rows:?}");
        for row in &lines {
            assert!(visible_width(row) <= 20, "{row:?}");
        }
    }

    /// A malformed block is not a table: the delimiter must match the header
    /// cell count, and a ragged body row ends the table instead of losing the
    /// cell it does not have.
    #[test]
    fn malformed_tables_stay_literal() {
        let theme = colored_theme();
        assert_eq!(
            plain(&render_markdown("| a | b |\n|---|", &theme, 40)),
            vec!["| a | b |", "|---|"]
        );
        assert_eq!(
            plain(&render_markdown(
                "| a | b |\n|---|---|\n| only one |",
                &theme,
                40
            )),
            vec![
                "┌───┬───┐",
                "│ a │ b │",
                "├───┼───┤",
                "└───┴───┘",
                "| only one |",
            ]
        );
        // A row with no pipe at all is never a table row.
        assert_eq!(
            plain(&render_markdown("a\n|---|", &theme, 40)),
            vec!["a", "|---|"]
        );
    }

    /// A table opens a block like a heading: a blank row before it unless it
    /// already starts the answer or follows one.
    #[test]
    fn table_opens_a_block() {
        let theme = colored_theme();
        let rows = plain(&render_markdown(
            "Summary:\n| a |\n|---|\n| 1 |",
            &theme,
            40,
        ));
        assert_eq!(rows[0], "Summary:");
        assert_eq!(rows[1], "");
        assert!(rows[2].starts_with('┌'), "{rows:?}");
        // First row of the answer: no leading blank.
        let rows = plain(&render_markdown("| a |\n|---|\n| 1 |", &theme, 40));
        assert!(rows[0].starts_with('┌'), "{rows:?}");
    }

    /// Header plus delimiter and nothing else still draws a closed frame.
    #[test]
    fn header_only_table_is_a_closed_frame() {
        let theme = colored_theme();
        assert_eq!(
            plain(&render_markdown("| a | b |\n|---|---|", &theme, 40)),
            vec!["┌───┬───┐", "│ a │ b │", "├───┼───┤", "└───┴───┘"]
        );
    }

    /// Empty cells pad to their column; the header is bold, the border is the
    /// code-block token.
    #[test]
    fn empty_cells_and_theme_tokens() {
        let theme = colored_theme();
        let lines = render_markdown("| a |  |\n|---|---|\n|  | 2 |", &theme, 40);
        assert_eq!(
            plain(&lines),
            vec![
                "┌───┬───┐",
                "│ a │   │",
                "├───┼───┤",
                "│   │ 2 │",
                "└───┴───┘"
            ]
        );
        let border = "\x1b[38;2;68;68;68m";
        assert!(lines[0].starts_with(border), "{:?}", lines[0]);
        assert!(lines[1].contains("\x1b[1ma\x1b[22m"), "{:?}", lines[1]);
    }

    /// Bold, italic and inline code inside a cell keep their prose styling.
    #[test]
    fn inline_styling_survives_inside_a_cell() {
        let theme = colored_theme();
        let lines = render_markdown("| K | V |\n|---|---|\n| **b** | *i* and `c` |", &theme, 40);
        let body = &lines[3];
        assert!(body.contains("\x1b[1mb\x1b[22m"), "{body:?}");
        assert!(body.contains("\x1b[3mi\x1b[23m"), "{body:?}");
        assert!(body.contains("\x1b[38;2;255;123;114mc\x1b[39m"), "{body:?}");
        assert_eq!(
            plain(&lines),
            vec![
                "┌───┬─────────┐",
                "│ K │ V       │",
                "├───┼─────────┤",
                "│ b │ i and c │",
                "└───┴─────────┘",
            ]
        );
    }

    /// A runaway answer cannot grow one table without bound: the body stops at
    /// the row cap and the rest is literal text, and a row wider than the
    /// column cap is prose from the start.
    #[test]
    fn table_input_is_capped() {
        let theme = colored_theme();
        let mut md = String::from("| a |\n|---|\n");
        for i in 0..MAX_TABLE_ROWS + 3 {
            md.push_str(&format!("| {i} |\n"));
        }
        let rows = plain(&render_markdown(&md, &theme, 40));
        assert_eq!(
            rows.iter().filter(|r| r.starts_with("│ ")).count(),
            1 + MAX_TABLE_ROWS,
            "header plus exactly the capped body rows"
        );
        assert_eq!(
            rows.iter().filter(|r| r.starts_with('└')).count(),
            1,
            "one frame: {rows:?}"
        );
        assert!(
            rows.contains(&format!("| {} |", MAX_TABLE_ROWS + 2)),
            "the rows past the cap are still on screen"
        );

        let header = format!("|{}", " c |".repeat(MAX_TABLE_COLS + 1));
        let delim = format!("|{}", "---|".repeat(MAX_TABLE_COLS + 1));
        let rows = plain(&render_markdown(&format!("{header}\n{delim}"), &theme, 200));
        assert_eq!(rows, vec![header, delim], "a row that wide is prose");
    }

    /// A cut inside a styled cell closes the style it was in.  The link's
    /// colour is opened by one span and the cut lands in the next one, so the
    /// padding after the `…` must not still be painted in it.
    #[test]
    fn a_cut_cell_closes_its_style() {
        let theme = colored_theme();
        let lines = render_markdown(
            "| Where |\n|-------|\n| see [docs](https://docs.rs/very/long/path) |",
            &theme,
            16,
        );
        assert_eq!(
            plain(&lines),
            vec![
                "┌──────────────┐",
                "│ Where        │",
                "├──────────────┤",
                "│ see docs     │",
                "│ (https://do… │",
                "└──────────────┘",
            ]
        );
        for row in &lines {
            assert!(visible_width(row) <= 16, "{row:?}");
        }
        // The cut cell's text is closed before its padding, so the `…` and the
        // spaces after it are not left painted in the link's colour.
        let cut = lines
            .iter()
            .find(|row| row.contains('…'))
            .expect("the cut is drawn");
        // Everything the row paints between its bars, minus the closing bar's
        // own colour escape.
        let content = cut
            .rsplit_once('│')
            .and_then(|(head, _)| head.rsplit_once('│'))
            .map(|(_, content)| content.trim_end())
            .map(|c| c.strip_suffix("\x1b[38;2;68;68;68m").unwrap_or(c))
            .expect("a bar on each side");
        assert!(
            !style_is_open(content.trim_end()),
            "the style is closed before the padding: {cut:?}"
        );
    }
}
