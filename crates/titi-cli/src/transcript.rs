//! The transcript renderer: what turns a line into rows.
//!
//! One line arrives here as a [`TranscriptLine`] and leaves as the rows a frame
//! draws — the block a message opens with, the markdown answer, the diff under
//! a tool chip, the counted header a collapsed section folds into, the divider a
//! compaction leaves. Nothing here owns a terminal: [`transcript`] clones the
//! line list, renders it, and hands the rows back, so the screen keeps its own
//! scroll state and this module keeps no state at all beyond what a block needs
//! for its own width.
//!
//! The geometry of a block lives here too: [`MESSAGE_INDENT`], [`MARK_INDENT`]
//! and [`body_width`] are the only places an indent and a body's room are
//! spelled, and every block kind reads them.

use std::collections::HashSet;
use std::num::NonZeroU16;
use std::path::Path;

use ratatui::buffer::CellDiffOption;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use titi_tui::markdown::{Section, SectionMode, SectionVisibility};
use titi_tui::theme::{Theme, ThemeBg, ThemeColor};

use crate::chat::{Chat, TOOL_PREVIEW, bg, fg, page};
use crate::composer::{one_line, wrap_plain};

// ---------------------------------------------------------------------------
// The transcript's geometry
// ---------------------------------------------------------------------------
//
// One place decides where a transcript block's rows sit and how much room its
// body has. There are two kinds of block and so two indents — a message opens
// with its label (`  you  │ `), a marked block with its mark (`   ▸ `) — and
// every block kind, the markdown answer and the diff under a tool chip
// included, reads its indent and its width from here. Nothing else spells an
// indent of its own.
//
// The rows of a sign-in note's URL are the one deliberate exception: they are
// flush left on purpose, so together they spell the URL (`link_note`).

/// Cells a message block's body starts at, on its first row and on every
/// wrapped one: `  you  │ ` and `       │ `.
///
/// The bar's cells, the label's four and the air before them make it up;
/// `message_gutter` builds the pieces from this, and the tests measure that
/// they add up to it.
pub(crate) const MESSAGE_INDENT: usize = 9;

/// The bar a message block carries after its label, and at the end of every
/// wrapped row: one space, the bar, one space.
const MESSAGE_BAR: &str = " │ ";

/// Cells a marked block's body starts at: `   ▸ ` and the five cells a wrapped
/// row hangs under.
pub(crate) const MARK_INDENT: usize = 5;

/// One cell kept clear of the layout's right edge, so a body never touches it.
const BODY_MARGIN: usize = 1;

/// The columns a body has at `width`, sitting at `indent`: the layout less the
/// indent, less the margin.
///
/// A caller floors the result — a chip wraps at four columns where a message
/// wraps at eight — but none subtracts for itself.
fn body_width(width: usize, indent: usize) -> usize {
    width.saturating_sub(indent + BODY_MARGIN)
}

/// The surface a transcript block is drawn on: the page, or the band behind the
/// user's own question.
///
/// The band spans the column range every block is laid out in — the transcript's
/// own width, two cells clear of the pane's edge — and exactly the rows the
/// block occupies, so it cannot run into a neighbouring block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Page,
    User,
}

/// The style a block's own furniture carries — its air, its bar, the cells that
/// pad a row out to the layout's width.
fn surface_style(surface: Surface, theme: &Theme) -> Style {
    match surface {
        Surface::Page => page(theme),
        Surface::User => Style::default().bg(bg(theme, ThemeBg::UserMessageBg)),
    }
}

/// A token's colour on a block's surface. The user's band keeps the theme's
/// `userMessageText` for the body and the label's own colour; both have to sit
/// on the band rather than on the page behind it.
fn on_surface(surface: Surface, theme: &Theme, token: ThemeColor) -> Style {
    let style = fg(theme, token);
    match surface {
        Surface::Page => style,
        Surface::User => style.bg(bg(theme, ThemeBg::UserMessageBg)),
    }
}

/// Pad a row out to the layout's width when the block is on a band, so the band
/// covers the whole row. A block on the page needs nothing: the pane paints it.
fn banded(
    mut spans: Vec<Span<'static>>,
    surface: Surface,
    theme: &Theme,
    width: usize,
) -> Vec<Span<'static>> {
    if surface == Surface::Page {
        return spans;
    }
    let used: usize = spans
        .iter()
        .map(|span| titi_tui::width::visible_width(&span.content))
        .sum();
    if let Some(rest) = width.checked_sub(used).filter(|rest| *rest > 0) {
        spans.push(Span::styled(
            " ".repeat(rest),
            surface_style(surface, theme),
        ));
    }
    spans
}

/// The pieces a message block opens with, and what a wrapped row hangs under:
/// the air before the label, the label, the bar after it, and the hang.
///
/// The three openers add up to [`MESSAGE_INDENT`] cells, and so does the hang.
pub(crate) fn message_gutter(name: &str) -> (String, String, String, String) {
    // "you" and "titi" share a column so a short message stays one row.
    let tag = format!("{name:<4}");
    let air = " ".repeat(
        MESSAGE_INDENT
            .saturating_sub(tag.chars().count() + titi_tui::width::visible_width(MESSAGE_BAR)),
    );
    (
        air,
        tag,
        MESSAGE_BAR.to_owned(),
        format!("{:width$}│ ", "", width = MESSAGE_INDENT - 2),
    )
}

/// The pieces a marked block opens with, and what a wrapped row hangs under:
/// the cells before the mark, the mark, the space after it, and the hang.
///
/// Both the three openers and the hang are [`MARK_INDENT`] cells.
pub(crate) fn mark_gutter(mark: &str) -> (String, String, String, String) {
    (
        " ".repeat(MARK_INDENT - 2),
        mark.to_owned(),
        " ".to_owned(),
        " ".repeat(MARK_INDENT),
    )
}

/// Rows of a diff the screen draws before it stops and says what it hid.
///
/// A pane shows about twenty rows at a time, so two hundred is ten screens of
/// scrollback inside one transcript line; the transcript is rebuilt on every
/// frame, and an unbounded settled block would be paid for every frame for a
/// body nobody is reading. What is hidden is counted exactly, and the whole
/// result is in the session file.
pub(crate) const DIFF_MAX_ROWS: usize = 200;

/// Who a transcript line belongs to. Public because a cast replay renders
/// the same lines outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Tool,
    /// A tool result's unified diff, drawn by the diff renderer rather than as
    /// a chip (`crates/titi-tui/src/diff.rs`).
    Diff,
    Error,
    Note,
    /// A finished turn's usage footer: the dim row under the answer
    /// (`titi_tui::status::TurnFooter`). Not a line of the conversation, so it
    /// is never written to the session file.
    Usage,
    /// The reasoning row while it is the newest thing on screen. Its own kind
    /// because reasoning is a section of its own (`/details thinking …`).
    Thinking,
    /// What a subagent did, as the engine reported it. A section of its own
    /// (`/details subagents …`), because subagent chatter is what the default
    /// transcript folds away.
    Agent,
    /// A compaction's fold divider. Not a line of the conversation: it stands
    /// for the history above it, which is drawn only while the folded section
    /// is expanded.
    Fold,
}

impl LineKind {
    /// The word a surface without colour puts in front of the line.
    pub fn as_str(self) -> &'static str {
        match self {
            LineKind::User => "you",
            LineKind::Assistant => "titi",
            LineKind::Tool => "tool",
            LineKind::Diff => "diff",
            LineKind::Error => "error",
            LineKind::Note => "note",
            LineKind::Usage => "usage",
            LineKind::Thinking => "thinking",
            LineKind::Agent => "agent",
            LineKind::Fold => "fold",
        }
    }

    /// The named section this line belongs to, for `/details`.
    ///
    /// `None` is the conversation itself — the user's own questions and the
    /// assistant's answers — which no visibility switch touches: a section
    /// hides a class of the turn's furniture, never what was said. The fold
    /// divider is `None` here too; it has a switch of its own
    /// ([`Details::folded`]).
    fn section(self) -> Option<Section> {
        match self {
            LineKind::User | LineKind::Assistant | LineKind::Fold => None,
            LineKind::Thinking => Some(Section::Thinking),
            LineKind::Tool | LineKind::Diff => Some(Section::Tools),
            LineKind::Agent => Some(Section::Subagents),
            LineKind::Error | LineKind::Note | LineKind::Usage => Some(Section::Activity),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

/// What `/details` reaches: the transcript's four named sections
/// (`titi_tui::markdown`'s own types, the module the deleted `transcript.rs`
/// drove) and the compaction fold's divider.
///
/// A section's lines are visible while it is expanded, stand as one counted
/// header while it is collapsed, and are not drawn at all while it is hidden.
/// The conversation itself is not a section and is never hidden.
#[derive(Debug, Clone)]
pub(crate) struct Details {
    sections: SectionVisibility,
    /// The fold's own switch. The divider is not an entry of the four — it
    /// stands for the history above it — so it has a mode of its own, but
    /// `/details` reaches it by name like any other.
    folded: SectionMode,
}

impl Details {
    /// The defaults the old transcript module documented, with one departure.
    ///
    /// `SectionVisibility::default()` is that DoD exactly — thinking and tools
    /// expanded, subagents collapsed, activity hidden — and this surface keeps
    /// three of the four. Activity is the one it cannot keep: here those lines
    /// are the answers `/usage`, `/context`, `/jobs`, `/recap` and a switch to
    /// another session give, so a hidden default would be a command that
    /// prints nothing at all. The mode is still reachable, so `/details
    /// activity hidden` does what the DoD asked — on purpose, not by default.
    ///
    /// The fold starts collapsed: that is the point of the divider, and what
    /// keeps a compacted transcript a line instead of the wall it replaced.
    /// Before the first compaction there is no divider to show.
    pub(crate) fn new() -> Self {
        let mut sections = SectionVisibility::default();
        sections.set(Section::Activity, SectionMode::Expanded);
        Self {
            sections,
            folded: SectionMode::Collapsed,
        }
    }

    /// Apply one `/details` directive: `"<section> <mode>"`, `"<section>"`
    /// (which reports the mode it is on), or a whole-word `"<mode>"` over
    /// every section and the fold.
    ///
    /// `None` when the directive names nothing this screen knows, `Some("")`
    /// when it moved something the transcript itself now shows, and
    /// `Some(line)` when there is something to say back.
    pub(crate) fn apply(&mut self, directive: &str) -> Option<String> {
        let mut parts = directive.split_whitespace();
        let first = parts.next()?;
        let second = parts.next();
        // A third word is not a directive: nothing is guessed out of it.
        if parts.next().is_some() {
            return None;
        }
        match second {
            Some(mode) => {
                if !self.set(first, mode) {
                    return None;
                }
                // The mode is read back off the section, so what is confirmed
                // is what the next frame will do.
                Some(format!("details: {first} {}", self.mode_label(first)?))
            }
            None => {
                if let Some(mode) = self.mode_label(first) {
                    return Some(format!("details: {first} {mode}"));
                }
                if !is_mode_word(first) {
                    return None;
                }
                for section in [
                    Section::Thinking,
                    Section::Tools,
                    Section::Subagents,
                    Section::Activity,
                ] {
                    self.apply_to(section, first);
                }
                apply_mode(&mut self.folded, first);
                // The sections move together, but a cycle sends each from
                // wherever it was, so there is no single mode to report.
                Some(String::new())
            }
        }
    }

    /// The listing a bare `/details` answers with: every section and the mode
    /// it is on, which is the only way to see the state without guessing.
    pub(crate) fn list(&self) -> String {
        let parts: Vec<String> = ["thinking", "tools", "subagents", "activity", FOLDED_SECTION]
            .iter()
            .filter_map(|name| self.mode_label(name).map(|mode| format!("{name} {mode}")))
            .collect();
        format!("details: {}", parts.join(" · "))
    }

    /// `<name> <mode>` for one named section or the fold. `false` when either
    /// half is unknown, in which case nothing moved.
    fn set(&mut self, name: &str, mode: &str) -> bool {
        if name.eq_ignore_ascii_case(FOLDED_SECTION) {
            return apply_mode(&mut self.folded, mode);
        }
        match Section::parse(name) {
            Some(section) => self.apply_to(section, mode),
            None => false,
        }
    }

    fn apply_to(&mut self, section: Section, mode: &str) -> bool {
        let mut current = self.sections.get(section);
        if !apply_mode(&mut current, mode) {
            return false;
        }
        let changed = self.sections.get(section) != current;
        self.sections.set(section, current);
        changed
    }

    /// The mode a named section or the fold is on, or `None` for a name
    /// neither of them has.
    pub(crate) fn mode(&self, name: &str) -> Option<SectionMode> {
        if name.eq_ignore_ascii_case(FOLDED_SECTION) {
            return Some(self.folded);
        }
        Some(self.sections.get(Section::parse(name)?))
    }

    /// The name a mode is spelled with, for the line `/details` answers with.
    fn mode_label(&self, name: &str) -> Option<&'static str> {
        Some(match self.mode(name)? {
            SectionMode::Hidden => "hidden",
            SectionMode::Collapsed => "collapsed",
            SectionMode::Expanded => "expanded",
        })
    }
}

/// The fold's name in `/details`, beside the four the old module had.
const FOLDED_SECTION: &str = "folded";

/// The three mode words, as `/details` spells them.
fn parse_mode(word: &str) -> Option<SectionMode> {
    match word.to_lowercase().as_str() {
        "hidden" => Some(SectionMode::Hidden),
        "collapsed" => Some(SectionMode::Collapsed),
        "expanded" => Some(SectionMode::Expanded),
        _ => None,
    }
}

/// Whether a word is a `/details` mode at all — the three names or `cycle`.
fn is_mode_word(word: &str) -> bool {
    parse_mode(word).is_some() || word.eq_ignore_ascii_case("cycle")
}

/// One section's or the fold's mode moved by a `/details` word: one of the
/// three names, or `cycle` (hidden → collapsed → expanded → hidden, the order
/// the old `SectionVisibility::apply` walked). `false` for anything else.
fn apply_mode(current: &mut SectionMode, word: &str) -> bool {
    let next = if word.eq_ignore_ascii_case("cycle") {
        match current {
            SectionMode::Hidden => SectionMode::Collapsed,
            SectionMode::Collapsed => SectionMode::Expanded,
            SectionMode::Expanded => SectionMode::Hidden,
        }
    } else {
        match parse_mode(word) {
            Some(mode) => mode,
            None => return false,
        }
    };
    let changed = *current != next;
    *current = next;
    changed
}

/// The writer a transcript block belongs to, for the air between blocks.
///
/// The user's own question is one writer; everything a turn produces — the
/// assistant's text, its tool chips, the diffs under them, its notes and errors
/// — is the other. Air goes where the writer changes, and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockRole {
    User,
    Turn,
}

impl LineKind {
    /// Which writer this block came from. Every kind but the user's own message
    /// belongs to the turn the assistant is running.
    fn block_role(self) -> BlockRole {
        match self {
            LineKind::User => BlockRole::User,
            _ => BlockRole::Turn,
        }
    }
}

pub(crate) struct Photo {
    path: String,
    id: u32,
    pixels: Vec<u8>,
    pixel_w: u32,
    pixel_h: u32,
    transmitted: bool,
    placed: Option<(u16, u16)>,
}

struct PlacedPhoto {
    id: u32,
    columns: u16,
    rows: u16,
}

enum TranscriptRow {
    Text(Line<'static>),
    Photo {
        id: u32,
        columns: u16,
        image_row: u16,
    },
}

/// A rendered row that is part of a URL: the visible text and the URL it
/// stands for. `row` is the index of that row inside its line's rows.
#[derive(Debug)]
pub(crate) struct LinkRow {
    pub(crate) row: usize,
    pub(crate) url: String,
    pub(crate) text: String,
}

/// A link row where the last frame put it, ready for [`paint_links`]. `row`
/// is relative to the transcript pane.
pub(crate) struct LinkPaint {
    row: u16,
    url: String,
    text: String,
}

pub(crate) struct PhotoPaint {
    row: u16,
    id: u32,
    columns: u16,
    image_row: u16,
}

/// One reply's rendered rows, kept so the frames between two deltas of a
/// streaming answer do not re-render it ([`Chat::assistant_rows`]).
pub(crate) struct ReplyRender {
    /// The answer the rows were rendered from.
    source: String,
    /// The pane width they were rendered for: a resize re-renders.
    width: usize,
    rows: Vec<Line<'static>>,
}

pub(crate) fn transcript(
    chat: &mut Chat,
    width: u16,
    height: u16,
    theme: &Theme,
) -> (Paragraph<'static>, Vec<PhotoPaint>, Vec<LinkPaint>) {
    let inner = (width as usize).saturating_sub(2).max(8);
    let max_cols = width.saturating_sub(8).max(8);
    let owned = chat.lines.clone();
    // The newest answer is the one still arriving: it is the reply whose rows
    // are worth keeping between frames (see `Chat::assistant_rows`).
    let newest_reply = owned
        .iter()
        .rposition(|line| line.kind == LineKind::Assistant);
    let mut rows: Vec<TranscriptRow> = Vec::new();
    let mut links: Vec<(usize, LinkRow)> = Vec::new();
    // The section modes, read once: rendering wants `&mut chat` for the
    // newest answer's rows, so the state has to be out of the borrow before
    // the loop.
    let modes = SECTIONS.map(|section| chat.details.sections.get(section));
    let folded = chat.details.folded;
    // The newest compaction's divider. The lines before it are the history the
    // fold stands for: they are drawn while the folded section is expanded, so
    // a compacted transcript is short by default and whole on request.
    let fold_at = owned.iter().rposition(|line| line.kind == LineKind::Fold);
    let mut headers_drawn: HashSet<Section> = HashSet::new();
    let mut last_role: Option<BlockRole> = None;
    for (index, line) in owned.iter().enumerate() {
        if let Some(at) = fold_at {
            if index < at && folded != SectionMode::Expanded {
                continue;
            }
            if index == at && folded == SectionMode::Hidden {
                continue;
            }
        }
        // A section's own visibility, before anything is drawn: hidden lines
        // are not rows, and a collapsed section's lines become one counted
        // header where its first line would have been.
        if let Some(section) = line.kind.section() {
            let mode = modes[section_index(section)];
            match mode {
                SectionMode::Expanded => {}
                SectionMode::Hidden => continue,
                SectionMode::Collapsed => {
                    if headers_drawn.insert(section) {
                        let count = owned
                            .iter()
                            .filter(|line| line.kind.section() == Some(section))
                            .count();
                        if !rows.is_empty() {
                            rows.push(TranscriptRow::Text(Line::from("")));
                        }
                        rows.push(TranscriptRow::Text(section_header(
                            section, count, theme, inner,
                        )));
                        last_role = Some(BlockRole::Turn);
                    }
                    continue;
                }
            }
        }
        // Air where the writer changes, and nowhere else: one turn's own
        // blocks — its text, its tool chips, the diffs under them, its notes —
        // stay one body, and a wrapped row is not a block of its own.
        let changes_writer = last_role.is_some_and(|role| role != line.kind.block_role());
        if changes_writer && !rows.is_empty() {
            rows.push(TranscriptRow::Text(Line::from("")));
        }
        let base = rows.len();
        last_role = Some(line.kind.block_role());
        let (texts, line_links) = if line.kind == LineKind::Assistant {
            {
                // The newest answer is the one still arriving, so it is the one
                // the reveal paces (`display.smoothStreaming`); every other
                // line is what it is.
                let shown = if newest_reply == Some(index) {
                    chat.revealed_prefix(&line.text)
                } else {
                    &line.text
                };
                (
                    chat.assistant_rows(newest_reply == Some(index), shown, inner, theme),
                    Vec::new(),
                )
            }
        } else if line.kind == LineKind::Fold {
            // The divider carries the fold's own chevron, which the state
            // above — not the line — decides.
            (
                vec![fold_divider(&line.text, folded, theme, inner)],
                Vec::new(),
            )
        } else {
            message_rows(line, inner, theme, chat.mermaid)
        };
        for text in texts {
            rows.push(TranscriptRow::Text(text));
        }
        for link in line_links {
            links.push((base + link.row, link));
        }
        if chat.kitty {
            for path in image_paths(&line.text) {
                if let Some(photo) = chat.prepare_photo(&path, max_cols) {
                    for image_row in 0..photo.rows {
                        rows.push(TranscriptRow::Photo {
                            id: photo.id,
                            columns: photo.columns,
                            image_row,
                        });
                    }
                }
            }
        }
    }
    let keep = height as usize;
    let total = rows.len();
    chat.last_transcript_height = keep;
    let max_offset = total.saturating_sub(keep);
    chat.scroll_offset = chat.scroll_offset.clamp(0, max_offset);
    let start = total.saturating_sub(keep + chat.scroll_offset);
    let mut lines = Vec::new();
    let mut photos = Vec::new();
    let mut paints = Vec::new();
    let mut next_link = 0usize;
    for (index, row) in rows.into_iter().skip(start).take(keep).enumerate() {
        let at = start + index;
        while next_link < links.len() && links[next_link].0 < at {
            next_link += 1;
        }
        if let Some((row_at, link)) = links.get(next_link)
            && *row_at == at
        {
            paints.push(LinkPaint {
                row: index as u16,
                url: link.url.clone(),
                text: link.text.clone(),
            });
            next_link += 1;
        }
        match row {
            TranscriptRow::Text(line) => lines.push(line),
            TranscriptRow::Photo {
                id,
                columns,
                image_row,
            } => {
                photos.push(PhotoPaint {
                    row: index as u16,
                    id,
                    columns,
                    image_row,
                });
                lines.push(Line::from(""));
            }
        }
    }
    // What the frame draws, as characters. A copied selection reads this, not
    // the buffer, so style can never ride along on the way to the clipboard.
    chat.last_rows = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    (Paragraph::new(lines).style(page(theme)), photos, paints)
}

/// Draws the drag selection: the rows it covers, re-drawn with the theme's
/// `selectedBg` behind them.
///
/// The rows are re-rendered from the transcript's own text rather than by
/// repainting buffer cells, so the highlight is exactly the text a copy
/// carries: [`Selection::apply_background`] inserts the background into the
/// row's own styling and [`sgr_row`] reads that styling back into spans. The
/// highlight is not a mode — the next key takes it away — and the photos and
/// hyperlinks are painted after it, so a selected row that holds an image or a
/// link keeps both.
pub(crate) fn paint_selection(
    frame: &mut ratatui::Frame<'_>,
    chat: &Chat,
    area: ratatui::layout::Rect,
    theme: &Theme,
) {
    let Some(selection) = chat.transcript_selection() else {
        return;
    };
    // The rows are padded to the pane's width before the background is laid
    // on: a terminal paints the whole selection rectangle, and a row whose
    // text stops before the rectangle's right edge would otherwise highlight
    // only as far as its characters. The padding is spaces on the pane's own
    // surface, which is what those cells already hold.
    let width = area.width as usize;
    let padded: Vec<String> = chat
        .last_rows
        .iter()
        .map(|row| {
            let room = width.saturating_sub(titi_tui::width::visible_width(row));
            if room == 0 {
                row.clone()
            } else {
                format!("{row}{}", " ".repeat(room))
            }
        })
        .collect();
    let rows = selection.apply_background(&padded, theme);
    let lines: Vec<Line<'static>> = rows.iter().map(|row| Line::from(sgr_row(row))).collect();
    frame.render_widget(Paragraph::new(lines).style(page(theme)), area);
}

pub(crate) fn paint_photos(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    photos: &[PhotoPaint],
    theme: &Theme,
) {
    if photos.is_empty() {
        return;
    }
    let buf = frame.buffer_mut();
    for photo in photos {
        let y = area.y.saturating_add(photo.row);
        if y >= area.y.saturating_add(area.height) {
            continue;
        }
        let x0 = area.x.saturating_add(4);
        let (red, green, blue) = titi_tui::image::image_id_rgb(photo.id);
        for column in 0..photo.columns {
            let x = x0.saturating_add(column);
            if x >= area.x.saturating_add(area.width) {
                break;
            }
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            cell.set_symbol(&titi_tui::image::placeholder_symbol(
                column as usize,
                photo.image_row as usize,
            ));
            cell.set_fg((red, green, blue).into());
            cell.set_bg(bg(theme, ThemeBg::StatusLineBg));
        }
    }
}

/// Hangs an OSC 8 hyperlink on a URL row by writing the escapes into the
/// cells the row already holds.
///
/// The escapes cannot travel as span text: ratatui drops every grapheme that
/// carries a control character while it fills its buffer
/// (`Buffer::set_stringn`), so an OSC 8 inside a `Span` reaches the terminal
/// as bare `]8;;…` text. A cell written through `Cell::set_symbol` is not
/// filtered, and both escapes are zero-width, so each rides on a character
/// that is already there: the open on the first cell of the row, the close on
/// the last one. `CellDiffOption::ForcedWidth(1)` tells the diff the cell is
/// one column wide all the same — ratatui documents it for exactly this, "escape
/// sequences will have some computed width that does not match what is written
/// to the screen" — so the cursor the terminal advances is still the row's text.
///
/// Every row of a wrapped URL carries the whole URL, so clicking any row of
/// the link opens it. A terminal that ignores OSC 8 consumes the sequences
/// and shows the same text; the frame itself still holds that text, so a
/// redraw after a resize or a scroll re-emits the link with it.
pub(crate) fn paint_links(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    links: &[LinkPaint],
) {
    if links.is_empty() {
        return;
    }
    let buf = frame.buffer_mut();
    for link in links {
        let y = area.y.saturating_add(link.row);
        let first = area.x;
        // The columns the text really takes: a character is at least one cell.
        let mut cells = 0u16;
        let mut last = None;
        for ch in link.text.chars() {
            last = Some(cells);
            cells =
                cells.saturating_add(titi_tui::width::visible_width(&ch.to_string()).max(1) as u16);
        }
        let Some(offset) = last else {
            continue;
        };
        let Some(cell) = buf.cell_mut((first, y)) else {
            continue;
        };
        let mut open = titi_tui::caps::osc8_open(&link.url);
        open.push_str(cell.symbol());
        cell.set_symbol(&open);
        cell.set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
        let Some(cell) = buf.cell_mut((first.saturating_add(offset), y)) else {
            continue;
        };
        let mut close = cell.symbol().to_owned();
        close.push_str(titi_tui::caps::OSC8_CLOSE);
        cell.set_symbol(&close);
        cell.set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
    }
}

fn image_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        consider_image(&mut found, after[..end].trim());
        rest = &after[end + 1..];
    }
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>'
            )
        });
        consider_image(&mut found, trimmed);
    }
    found
}

fn consider_image(found: &mut Vec<String>, raw: &str) {
    if raw.is_empty() || found.iter().any(|path| path == raw) {
        return;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return;
    }
    let path = raw.strip_prefix("file://").unwrap_or(raw);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico") {
        found.push(path.to_owned());
    }
}

/// The fold divider's own words, from the `Compacted` payload: how many
/// messages went into the digest and how many tokens the request held when it
/// happened. The mark and whether the history under it is drawn come from
/// [`Details::folded`], so the same line reads `▸` collapsed and `▾` expanded.
pub(crate) fn fold_divider_label(folded: u32, tokens_before: u64) -> String {
    let tokens = u32::try_from(tokens_before).unwrap_or(u32::MAX);
    format!(
        "folded {folded} turns · {} tokens",
        titi_tui::status::compact_tokens(tokens)
    )
}

/// The fold divider as a row: the fold's mark, then the payload's own words.
fn fold_divider(text: &str, mode: SectionMode, theme: &Theme, width: usize) -> Line<'static> {
    let mark = match mode {
        SectionMode::Expanded => "▾",
        // The caller drops a hidden divider before it gets here, so a mark is
        // only ever the collapsed one; a collapsed means the history under it
        // is not drawn.
        SectionMode::Hidden | SectionMode::Collapsed => "▸",
    };
    chip(
        (mark, ThemeColor::Dim, text.to_owned(), ThemeColor::Dim),
        theme,
        width,
    )
    .into_iter()
    .next()
    .unwrap_or_default()
}

/// The four named sections, in the order `/details` lists them and the mode
/// snapshot below is indexed by.
const SECTIONS: [Section; 4] = [
    Section::Thinking,
    Section::Tools,
    Section::Subagents,
    Section::Activity,
];

fn section_index(section: Section) -> usize {
    match section {
        Section::Thinking => 0,
        Section::Tools => 1,
        Section::Subagents => 2,
        Section::Activity => 3,
    }
}

/// The name a section answers to, in `/details` and in the row a collapsed
/// section stands as.
fn section_name(section: Section) -> &'static str {
    match section {
        Section::Thinking => "thinking",
        Section::Tools => "tools",
        Section::Subagents => "subagents",
        Section::Activity => "activity",
    }
}

/// The one row a collapsed section takes the place of: `▸ tools (7)`, so the
/// count says how much is behind it rather than leaving the fold silent.
fn section_header(section: Section, count: usize, theme: &Theme, width: usize) -> Line<'static> {
    let label = if count == 0 {
        section_name(section).to_owned()
    } else {
        format!("{} ({count})", section_name(section))
    };
    chip(("▸", ThemeColor::Dim, label, ThemeColor::Dim), theme, width)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The rows of one transcript line, plus every row that carries a URL.
pub(crate) fn message_rows(
    line: &TranscriptLine,
    width: usize,
    theme: &Theme,
    mermaid: bool,
) -> (Vec<Line<'static>>, Vec<LinkRow>) {
    if line.kind == LineKind::Note
        && let Some((head, url, instructions)) = login_link(&line.text)
    {
        return link_note(head, url, instructions, theme, width);
    }
    let rows = match line.kind {
        // The user's own question is the one block with a surface of its own:
        // the theme's `userMessageBg` behind it, `userMessageText` on top, and
        // the label's colour unchanged.
        LineKind::User => speech(
            "you",
            ThemeColor::CustomMessageLabel,
            ThemeColor::UserMessageText,
            Surface::User,
            &line.text,
            width,
            theme,
        ),
        LineKind::Assistant => reply_rows(&line.text, width, theme, mermaid),
        LineKind::Tool => chip(tool_chip(&line.text), theme, width),
        LineKind::Diff => diff_rows(&line.text, theme, width),
        LineKind::Error => chip(
            ("✕", ThemeColor::Error, line.text.clone(), ThemeColor::Error),
            theme,
            width,
        ),
        LineKind::Note => chip(
            ("·", ThemeColor::Dim, line.text.clone(), ThemeColor::Dim),
            theme,
            width,
        ),
        LineKind::Usage => usage_row(&line.text, theme, width),
        // Reasoning reads exactly as a note does — the section it belongs to
        // is a `/details` axis, not a look.
        LineKind::Thinking => chip(
            ("·", ThemeColor::Dim, line.text.clone(), ThemeColor::Dim),
            theme,
            width,
        ),
        // A subagent's report is a tool-shaped chip like any other; that it
        // folds under `subagents` is the section's business.
        LineKind::Agent => chip(tool_chip(&line.text), theme, width),
        // The mode-less reading of a divider, for a caller that has no
        // `Details` at hand: the live frame asks [`fold_divider`] directly with
        // the fold's own mode.
        LineKind::Fold => vec![fold_divider(
            &line.text,
            SectionMode::Collapsed,
            theme,
            width,
        )],
    };
    (rows, Vec::new())
}

/// A finished turn's usage footer: one dim row indented to the text column,
/// with no mark of its own — it is metadata about the turn, not a line of it.
///
/// The row is only ever pushed for a turn that reported usage, so a screen
/// without one is a screen with nothing to report, and no zero is drawn as if
/// it were a fact.
fn usage_row(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let room = body_width(width, MARK_INDENT).max(4);
    let gutter = " ".repeat(MARK_INDENT);
    wrap_plain(text, room)
        .into_iter()
        .map(|piece| {
            Line::from(vec![
                Span::styled(gutter.clone(), page(theme)),
                Span::styled(piece, fg(theme, ThemeColor::Dim)),
            ])
        })
        .collect()
}

/// The sign-in note as its three parts: the head line, the authorize URL and
/// the trailing instructions.
fn login_link(text: &str) -> Option<(&str, &str, &str)> {
    let (head, rest) = text.split_once('\n')?;
    let (url, instructions) = rest.split_once('\n').unwrap_or((rest, ""));
    if !head.starts_with("login ") || !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    Some((head, url, instructions))
}

/// A sign-in note: the head and the instructions are ordinary chip rows, the
/// URL gets rows of its own.
///
/// [`chip`] would indent every row past the first and cut a word with no
/// space at the pane edge, so a long URL came out split across rows with five
/// spaces of chrome in the middle and no row holding a clickable link. Here
/// each URL row is exactly one slice of the URL, flush left on the pane
/// width, so the rows still spell the URL when a terminal has no hyperlinks
/// — and [`paint_links`] can hang that same URL on every row.
fn link_note(
    head: &str,
    url: &str,
    instructions: &str,
    theme: &Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<LinkRow>) {
    let mut rows = chip(
        ("·", ThemeColor::Dim, head.to_owned(), ThemeColor::Dim),
        theme,
        width,
    );
    let mut links = Vec::new();
    for piece in wrap_url(url, width) {
        links.push(LinkRow {
            row: rows.len(),
            url: url.to_owned(),
            text: piece.clone(),
        });
        rows.push(Line::from(Span::styled(piece, fg(theme, ThemeColor::Dim))));
    }
    if !instructions.is_empty() {
        // The continuation shape [`chip`] would have given the third
        // paragraph: five spaces under the text column, no mark of its own.
        let room = body_width(width, MARK_INDENT).max(4);
        for piece in wrap_plain(instructions, room) {
            rows.push(Line::from(vec![
                Span::styled("     ", page(theme)),
                Span::styled(piece, fg(theme, ThemeColor::Dim)),
            ]));
        }
    }
    (rows, links)
}

/// Cuts `url` into pane-wide rows without losing a character: every byte
/// lands on exactly one row, so the rows concatenate back to the URL.
pub(crate) fn wrap_url(url: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut piece = String::new();
    let mut col = 0usize;
    for ch in url.chars() {
        let cell = titi_tui::width::visible_width(&ch.to_string());
        if col + cell > width && !piece.is_empty() {
            rows.push(std::mem::take(&mut piece));
            col = 0;
        }
        piece.push(ch);
        col += cell;
    }
    if !piece.is_empty() || rows.is_empty() {
        rows.push(piece);
    }
    rows
}

/// The rows a message body has besides its gutter: one rule for the plain
/// block and for the markdown one, so an answer cannot re-wrap just because the
/// markdown path took it.
fn speech_width(width: usize) -> usize {
    body_width(width, MESSAGE_INDENT).max(8)
}

/// A message block: the name in its own colour, a bar, then the body rows under
/// the same gutter.
///
/// [`speech`] hands it one wrapped piece per row; the assistant's markdown path
/// hands it rows that already carry their own styles, so neither caller can
/// drift from the other's gutter.
fn message_block(
    name: &str,
    label: ThemeColor,
    surface: Surface,
    theme: &Theme,
    width: usize,
    bodies: Vec<Vec<Span<'static>>>,
) -> Vec<Line<'static>> {
    let (air, tag, bar, hang) = message_gutter(name);
    let mut rows = Vec::with_capacity(bodies.len());
    for (index, body) in bodies.into_iter().enumerate() {
        let mut row: Vec<Span<'static>> = Vec::with_capacity(body.len() + 3);
        if index == 0 {
            // The label carries the weight; the air before it and the bar after
            // it do not, so the three are styled apart rather than as one run.
            row.push(Span::styled(air.clone(), surface_style(surface, theme)));
            row.push(Span::styled(
                tag.clone(),
                on_surface(surface, theme, label).add_modifier(Modifier::BOLD),
            ));
            row.push(Span::styled(bar.clone(), on_surface(surface, theme, label)));
        } else {
            row.push(Span::styled(
                hang.clone(),
                on_surface(surface, theme, label),
            ));
        }
        row.extend(body);
        rows.push(Line::from(banded(row, surface, theme, width)));
    }
    rows
}

pub(crate) fn speech(
    name: &str,
    label: ThemeColor,
    body: ThemeColor,
    surface: Surface,
    text: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let bodies = wrap_plain(text, speech_width(width))
        .into_iter()
        .map(|piece| vec![Span::styled(piece, on_surface(surface, theme, body))])
        .collect();
    message_block(name, label, surface, theme, width, bodies)
}

/// The assistant's answer as frame rows: markdown when the answer carries any,
/// the plain block it has always been when it does not.
///
/// The answer is markdown from its first delta onwards, so the screen never
/// shows the raw syntax and never swaps renderings mid-answer; a construct that
/// is still half-typed (an unclosed fence, a lone `**`) renders literally until
/// its marker arrives, which is the renderer's own reading of partial markdown.
/// [`Chat::assistant_rows`] is what keeps that affordable while the answer is
/// still arriving.
fn reply_rows(text: &str, width: usize, theme: &Theme, mermaid: bool) -> Vec<Line<'static>> {
    if !has_markdown(text) {
        return speech(
            "titi",
            ThemeColor::Accent,
            ThemeColor::Text,
            Surface::Page,
            text,
            width,
            theme,
        );
    }
    let bodies = markdown_bodies(text, speech_width(width), theme, mermaid);
    message_block(
        "titi",
        ThemeColor::Accent,
        Surface::Page,
        theme,
        width,
        bodies,
    )
}

/// The renderer's rows as frame spans.
///
/// `render_markdown` styles by writing SGR into the row, and ratatui drops the
/// control characters that travel inside a span, so the escapes have to come
/// back out as [`Style`]s before a row can be drawn: a span carries the colour,
/// never the sequence. Only the renderer's own styling is applied — nothing here
/// wraps a row in a style of its own, so the two cannot fight.
fn markdown_bodies(
    text: &str,
    width: usize,
    theme: &Theme,
    mermaid: bool,
) -> Vec<Vec<Span<'static>>> {
    // `width` descends from a `u16` pane, so the cast only undoes the widening.
    titi_tui::markdown::render_markdown(text, theme, width as u16, mermaid)
        .into_iter()
        .map(|row| sgr_row(&row))
        .collect()
}

/// One rendered row — text with SGR — as ratatui spans. A run the renderer left
/// unstyled keeps the style of the pane it is drawn on.
pub(crate) fn sgr_row(row: &str) -> Vec<Span<'static>> {
    let base = Style::default();
    let mut style = base;
    let mut spans = Vec::new();
    for piece in titi_tui::width::spans(row) {
        match piece {
            titi_tui::width::Span::Text(text) if !text.is_empty() => {
                spans.push(Span::styled(text.to_owned(), style));
            }
            titi_tui::width::Span::Text(_) => {}
            titi_tui::width::Span::Escape(sequence) => apply_sgr(sequence, &mut style, base),
        }
    }
    spans
}

/// Apply one SGR sequence to the running style.
///
/// The renderer emits only what the theme helpers and the width wrapper
/// produce: a colour (`38;2;r;g;b`, `38;5;n`, `39`), a background (`48;…`,
/// `49`), a full reset, and the bold, italic, underline, inverse and
/// strikethrough switches. A `39`/`49` returns the channel to the style the row
/// was drawn over rather than to the terminal default, so what the renderer
/// left unstyled stays the pane's own colour.
fn apply_sgr(sequence: &str, style: &mut Style, base: Style) {
    let Some(params) = sequence
        .strip_prefix("\x1b[")
        .and_then(|body| body.strip_suffix('m'))
    else {
        return;
    };
    let mut parts = params.split(';');
    while let Some(part) = parts.next() {
        match part {
            "" | "0" => *style = base,
            "1" => *style = style.add_modifier(Modifier::BOLD),
            "3" => *style = style.add_modifier(Modifier::ITALIC),
            "4" => *style = style.add_modifier(Modifier::UNDERLINED),
            "7" => *style = style.add_modifier(Modifier::REVERSED),
            "9" => *style = style.add_modifier(Modifier::CROSSED_OUT),
            "22" => *style = style.remove_modifier(Modifier::BOLD),
            "23" => *style = style.remove_modifier(Modifier::ITALIC),
            "24" => *style = style.remove_modifier(Modifier::UNDERLINED),
            "27" => *style = style.remove_modifier(Modifier::REVERSED),
            "29" => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            "38" | "48" => {
                let foreground = part == "38";
                let colour = sgr_colour(&mut parts);
                *style = if foreground {
                    style.fg(colour)
                } else {
                    style.bg(colour)
                };
            }
            "39" => style.fg = base.fg,
            "49" => style.bg = base.bg,
            _ => {}
        }
    }
}

/// The colour of a `38`/`48` group, the `2`/`5` selector already consumed:
/// truecolor channels or a 256-palette index.
fn sgr_colour(parts: &mut std::str::Split<'_, char>) -> Color {
    match parts.next() {
        Some("2") => {
            let channel = |value: Option<&str>| value.and_then(|v| v.parse::<u8>().ok());
            let r = channel(parts.next());
            let g = channel(parts.next());
            let b = channel(parts.next());
            match (r, g, b) {
                (Some(r), Some(g), Some(b)) => (r, g, b).into(),
                _ => Color::Reset,
            }
        }
        Some("5") => match parts.next().and_then(|value| value.parse::<u8>().ok()) {
            Some(index) => Color::Indexed(index),
            None => Color::Reset,
        },
        _ => Color::Reset,
    }
}

/// Whether an answer carries markdown the renderer would consume.
///
/// The answer is rendered as markdown only when this is true: a reply that is
/// not markdown — a one-liner, a plain summary — keeps the exact rows it had
/// before the renderer existed, so nothing about it can change because a stray
/// `*` or `_` looked like emphasis. The markers below are the ones
/// [`titi_tui::markdown::render_markdown`] acts on, and maths is one of them:
/// `has_math` answers for the `$…$` and `$$…$$` the LaTeX renderer would
/// convert — and only for those, so `$5 and $6` and a lone `$` stay plain.
fn has_markdown(text: &str) -> bool {
    text.lines().any(markdown_block)
        || has_inline_markdown(text)
        || titi_tui::markdown::has_math(text)
}

/// A block construct on a row of its own, where the renderer drops the marker
/// and redraws the row.
fn markdown_block(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.starts_with("```") || matches!(trimmed, "---" | "***" | "___") {
        return true;
    }
    // A heading has to open the row, as the renderer requires.
    let hashes = raw.chars().take_while(|ch| *ch == '#').count();
    if hashes > 0 && (raw[hashes..].is_empty() || raw[hashes..].starts_with(' ')) {
        return true;
    }
    raw.starts_with('>') || list_marker(raw).is_some()
}

/// The list marker of a row — `- `, `* ` or `N. ` after any indentation. The
/// same three shapes the renderer splits a list item into.
fn list_marker(raw: &str) -> Option<&str> {
    let body = raw.trim_start_matches([' ', '\t']);
    if body.starts_with("- ") || body.starts_with("* ") {
        return Some(&body[..2]);
    }
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    (digits > 0 && body[digits..].starts_with(". ")).then(|| &body[..digits + 1])
}

/// The inline markers the renderer consumes wherever they sit: `` `code` ``,
/// `**bold**`, `[text](url)`, and an emphasis pair.
fn has_inline_markdown(text: &str) -> bool {
    if text.matches('`').count() >= 2 || text.matches("**").count() >= 2 {
        return true;
    }
    if text.contains('[') && text.contains("](") {
        return true;
    }
    emphasised(text, '*') || emphasised(text, '_')
}

/// An emphasis pair — `*text*` or `_text_` — that is flanked the way CommonMark
/// requires: a marker needs a word character beside it, and `_` also refuses to
/// open or close inside a word, the rule the renderer's own underscore scan
/// applies. `snake_case_name` and `2 * 3 * 4` therefore stay as they are
/// instead of dragging the answer into the markdown path.
fn emphasised(text: &str, marker: char) -> bool {
    let cells: Vec<char> = text.chars().collect();
    let mut open: Option<usize> = None;
    for (at, cell) in cells.iter().enumerate() {
        if *cell != marker {
            continue;
        }
        let before = at.checked_sub(1).map(|index| cells[index]);
        let after = cells.get(at + 1).copied();
        match open {
            None => {
                if marker == '_' && !before.is_none_or(|ch| !ch.is_alphanumeric()) {
                    continue;
                }
                if after.is_some_and(|ch| !ch.is_whitespace()) {
                    open = Some(at);
                }
            }
            Some(from) => {
                if marker == '_' && !after.is_none_or(|ch| !ch.is_alphanumeric()) {
                    continue;
                }
                if before.is_some_and(|ch| !ch.is_whitespace()) && at > from + 1 {
                    return true;
                }
            }
        }
    }
    false
}

fn tool_chip(text: &str) -> (&'static str, ThemeColor, String, ThemeColor) {
    // The engine records "tool <name>" and "tool done  <preview>".
    // The screen says the same thing without the debug prefix.
    if let Some(rest) = text.strip_prefix("tool error") {
        return (
            "✕",
            ThemeColor::Error,
            rest.trim().to_owned(),
            ThemeColor::Error,
        );
    }
    if let Some(rest) = text.strip_prefix("tool done") {
        let detail = rest.trim();
        let body = if detail.is_empty() {
            "done".to_owned()
        } else {
            detail.to_owned()
        };
        return ("✓", ThemeColor::Success, body, ThemeColor::Muted);
    }
    if let Some(rest) = text.strip_prefix("tool ") {
        return (
            "▸",
            ThemeColor::Warning,
            rest.trim().to_owned(),
            ThemeColor::CustomMessageLabel,
        );
    }
    ("▸", ThemeColor::Warning, text.to_owned(), ThemeColor::Muted)
}

/// A tool result that carries a detail: the chip names the file the diff
/// touches, and the renderer's rows follow under the block's inset.
///
/// A detail the renderer cannot read as a diff is still shown — a chip with
/// what the tool wanted drawn — so nothing a tool reported is dropped.
fn diff_rows(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let Some(diff) = titi_tui::diff::render_diff(text, theme, diff_width(width) as u16) else {
        // Not a diff — the todo checklist today. Its own rows are the point,
        // so they are kept, each cut to the preview width, and a long list
        // is capped like a long diff.
        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len().min(DIFF_MAX_ROWS);
        let mut kept: Vec<String> = lines[..shown]
            .iter()
            .map(|line| one_line(line, TOOL_PREVIEW))
            .collect();
        if lines.len() > shown {
            kept.push(format!("… {} more lines", lines.len() - shown));
        }
        return chip(
            ("✓", ThemeColor::Success, kept.join("\n"), ThemeColor::Muted),
            theme,
            width,
        );
    };
    let detail = diff.path.unwrap_or_else(|| "diff".to_owned());
    let mut rows = chip(
        ("✓", ThemeColor::Success, detail, ThemeColor::Muted),
        theme,
        width,
    );
    let shown = diff.rows.len().min(DIFF_MAX_ROWS);
    for row in &diff.rows[..shown] {
        rows.push(diff_row(row, theme));
    }
    if diff.rows.len() > shown {
        rows.push(diff_note(
            &format!("… {} more diff lines", diff.rows.len() - shown),
            theme,
        ));
    }
    rows
}

/// The columns a diff's rows have: the block's inset is spent before them, so a
/// row the renderer cut to this width still fits inside the pane.
fn diff_width(width: usize) -> usize {
    width.saturating_sub(MARK_INDENT).max(8)
}

/// One renderer row, inset under the block's chip.
fn diff_row(row: &str, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(" ".repeat(MARK_INDENT), page(theme))];
    spans.extend(sgr_row(row));
    Line::from(spans)
}

/// The dim row that says how much of a long diff the screen did not draw.
fn diff_note(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ".repeat(MARK_INDENT), page(theme)),
        Span::styled(text.to_owned(), fg(theme, ThemeColor::Dim)),
    ])
}

/// A marked block: the mark opens the first row, every following row is
/// indented under the text column.
///
/// One note carries a whole block — `/diagnose`, `/git diff` and `/settings`
/// each push one — so the text is split on its own newlines and every piece
/// is wrapped to the pane, exactly as [`speech`] does. Truncating to one row
/// threw everything past the first screen width away.
fn chip(
    parts: (&str, ThemeColor, String, ThemeColor),
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let (mark, mark_color, text, text_color) = parts;
    let room = body_width(width, MARK_INDENT).max(4);
    let pieces = wrap_plain(&text, room);
    let (air, mark, after, hang) = mark_gutter(mark);
    let mut rows = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.into_iter().enumerate() {
        let row = if index == 0 {
            // The mark keeps its own colour; the cells before it and the space
            // after it belong to the page.
            vec![
                Span::styled(air.clone(), page(theme)),
                Span::styled(mark.clone(), fg(theme, mark_color)),
                Span::styled(after.clone(), page(theme)),
                Span::styled(piece, fg(theme, text_color)),
            ]
        } else {
            vec![
                Span::styled(hang.clone(), page(theme)),
                Span::styled(piece, fg(theme, text_color)),
            ]
        };
        rows.push(Line::from(row));
    }
    rows
}
impl Chat {
    /// The assistant's answer as frame rows, re-using the last render when
    /// neither the answer nor the pane width has changed.
    ///
    /// `remember` marks the newest reply — the one still arriving. A delta
    /// grows it and the screen redraws at 20 fps, so most frames ask for an
    /// answer that has not moved; without the rows being kept here, every one
    /// of those frames would parse and re-wrap the whole answer, which over a
    /// long one is quadratic in its length. An older reply is settled: it is
    /// rendered from source each frame, exactly as the plain block was before
    /// the renderer existed.
    ///
    /// The source is kept next to the rows, so a cache hit cannot be a
    /// different answer that happens to be the same length.
    pub(crate) fn assistant_rows(
        &mut self,
        remember: bool,
        text: &str,
        width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        if remember
            && let Some(cached) = &self.reply_render
            && cached.width == width
            && cached.source == text
        {
            return cached.rows.clone();
        }
        let rows = reply_rows(text, width, theme, self.mermaid);
        if remember {
            self.reply_render = Some(ReplyRender {
                source: text.to_owned(),
                width,
                rows: rows.clone(),
            });
        }
        rows
    }

    /// Load a local photo once and queue its kitty setup when the size changes.
    fn prepare_photo(&mut self, path: &str, max_cols: u16) -> Option<PlacedPhoto> {
        if let Some(index) = self.photos.iter().position(|photo| photo.path == path) {
            return Some(self.place_cached(index, max_cols));
        }
        if self.misses.contains(path) {
            return None;
        }
        let Some(decoded) = titi_tui::image::decode_rgba_file(Path::new(path)) else {
            self.misses.insert(path.to_owned());
            return None;
        };
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.saturating_add(1);
        self.photos.push(Photo {
            path: path.to_owned(),
            id,
            pixels: decoded.pixels,
            pixel_w: decoded.width,
            pixel_h: decoded.height,
            transmitted: false,
            placed: None,
        });
        let index = self.photos.len() - 1;
        Some(self.place_cached(index, max_cols))
    }

    fn place_cached(&mut self, index: usize, max_cols: u16) -> PlacedPhoto {
        let slot = &self.photos[index];
        let (columns, rows) = titi_tui::image::photo_cells(slot.pixel_w, slot.pixel_h, max_cols);
        let size = Some((columns, rows));
        if !slot.transmitted || slot.placed != size {
            let setup = titi_tui::image::kitty_photo_setup(
                slot.id,
                &slot.pixels,
                slot.pixel_w,
                slot.pixel_h,
                columns,
                rows,
                self.tmux,
                !slot.transmitted,
            );
            let slot = &mut self.photos[index];
            slot.transmitted = true;
            slot.placed = size;
            self.kitty_flush.push_str(&setup);
        }
        let slot = &self.photos[index];
        PlacedPhoto {
            id: slot.id,
            columns,
            rows,
        }
    }
}
