//! The welcome: the first frame, before there is a conversation.
//!
//! The lockup, the facts about this build and this workspace, the chords and
//! the one tip a session is given — everything [`empty_state`] draws while the
//! transcript is empty. It reads the chat's state and writes none of it, and it
//! is the screen's one monochrome surface: the mark and the wordmark are gray
//! levels measured off the theme's own `statusLineBg` (`WelcomeGrays`), so no
//! palette can make the first screen loud.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use titi_tui::theme::{Theme, ThemeBg};

use crate::chat::{Chat, masthead_snapshot, page};
use crate::composer::fit_tail;
use crate::pickers::model_rows;

/// Recent sessions the welcome names before it stops.
const WELCOME_SESSIONS: usize = 3;

/// The widest the facts block grows: on a wide pane it stays a block a glance
/// takes in, not a row stretched to the far edge.
const WELCOME_MEASURE: usize = 60;

/// The TITI mark: two block-grid T's, ti·ti, in the grid omp draws its own
/// mark in. The first T's leg fades at the foot, so the pair reads as two
/// letters of one word rather than two equal pillars.
const WELCOME_MARK: [&str; 5] = [
    "██████ ██████",
    "  ██     ██  ",
    "  ██     ██  ",
    "  ██     ██  ",
    "  ▒▒     ██  ",
];

/// `titi` in half blocks: each `t` an ascender over a crossbar with a foot,
/// each `i` a dot over a stem. Set beside the mark from its second row.
const WELCOME_WORDMARK: [&str; 2] = ["▄█▄ ▀ ▄█▄ ▀", " █▄ █  █▄ █"];

/// Columns between the mark and the wordmark.
const WELCOME_GAP: usize = 4;

/// How long the intro's shine takes to cross the mark. The run loop draws a
/// frame at least every 50 ms, which is the intro's frame rate.
pub(crate) const WELCOME_INTRO: Duration = Duration::from_millis(1500);

/// Half the width of the shine band, along the mark's diagonal (0 to 1).
const WELCOME_SHINE: f64 = 0.2;

/// What the first screen states, every fact read from the source the surface
/// that owns it reads: the catalog plus the credential reader behind `/keys`,
/// the status-bar snapshot behind the masthead, and the session list behind
/// `/sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WelcomeFacts {
    pub(crate) version: &'static str,
    /// The model that will answer this turn, and what stands behind it.
    pub(crate) model: String,
    pub(crate) credential: Option<String>,
    pub(crate) path: String,
    pub(crate) git: Option<WelcomeGit>,
    /// Recent sessions, newest first, and whether the screen is already on one.
    pub(crate) sessions: Vec<(String, bool)>,
}

/// The git fact of the welcome, in the same shape the masthead states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WelcomeGit {
    pub(crate) branch: String,
    pub(crate) unstaged: u32,
    pub(crate) staged: u32,
    pub(crate) untracked: u32,
}

/// The facts the first screen can state right now.
///
/// A fact that is not available is `None` or empty, and the welcome omits it
/// rather than printing a placeholder: the credential of a provider that holds
/// none, the git state of a directory that is not a checkout, the session list
/// of an agent directory that has no sessions yet.
fn welcome_facts(chat: &Chat) -> WelcomeFacts {
    let snapshot = masthead_snapshot(chat);
    // The credential chip is the one the model picker wears (`oauth` for a
    // subscription, `key` for an API key, `env` for a variable), read through
    // the same reader `/keys` uses. A model the catalog does not offer has no
    // provider to ask, so there is no chip.
    let credential = model_rows(chat)
        .into_iter()
        .find(|row| row.id == chat.model)
        .and_then(|row| row.credential);
    WelcomeFacts {
        version: titi_tui::VERSION,
        model: chat.model.clone(),
        credential,
        path: snapshot.path.clone(),
        git: snapshot.git_branch.clone().map(|branch| WelcomeGit {
            branch,
            unstaged: snapshot.git_unstaged,
            staged: snapshot.git_staged,
            untracked: snapshot.git_untracked,
        }),
        sessions: chat
            .session_choices()
            .into_iter()
            .take(WELCOME_SESSIONS)
            .map(|id| {
                let current = id == chat.session_id;
                (id, current)
            })
            .collect(),
    }
}

/// The welcome's grays, measured off the page they are drawn on.
///
/// The first screen is black and white on every palette. A theme's accent is
/// a hue picked for chrome, and a brand drawn in it changes character from one
/// theme to the next; ink does not — it is the far end of the page's own
/// lightness, whatever the page is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WelcomeGrays {
    /// The ink: values, the wordmark, the mark's lit corner.
    pub(crate) bright: f64,
    /// The ink most of the way back to the page: labels, the build, the
    /// chords, the mark's far corner.
    pub(crate) faded: f64,
    /// The far end of the page's lightness, past the ink, where the intro's
    /// shine lifts a cell to.
    pub(crate) peak: f64,
}

impl WelcomeGrays {
    pub(crate) fn of(theme: &Theme) -> Self {
        // BT.601 luma of the page. A page with no hex to measure is taken at
        // the theme's own word for whether it is light.
        let page = titi_tui::theme::color::hex_to_rgb(&theme.get_bg_hex(ThemeBg::StatusLineBg))
            .map(|rgb| {
                (0.299 * f64::from(rgb.r) + 0.587 * f64::from(rgb.g) + 0.114 * f64::from(rgb.b))
                    / 255.0
            })
            .unwrap_or(if theme.is_light() { 1.0 } else { 0.0 });
        let (bright, peak) = if page < 0.5 { (0.96, 1.0) } else { (0.08, 0.0) };
        Self {
            bright,
            faded: bright + (page - bright) * 0.6,
            peak,
        }
    }

    fn bright(&self) -> Style {
        Style::default().fg(gray(self.bright))
    }

    fn faded(&self) -> Style {
        Style::default().fg(gray(self.faded))
    }
}

/// A level between black (0) and white (1) as a terminal colour.
fn gray(level: f64) -> Color {
    let value = (level.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::Rgb(value, value, value)
}

/// Cells the mark takes across.
fn welcome_mark_width() -> usize {
    WELCOME_MARK
        .iter()
        .map(|row| titi_tui::width::visible_width(row))
        .max()
        .unwrap_or(0)
}

/// Where the intro's shine is along the mark's diagonal `elapsed` into the
/// intro, or `None` once it has crossed.
///
/// It eases out — quick off the lit corner, slowing into the far one — and
/// travels from a band's width before the mark to a band's width past it, so
/// its first and last frames light nothing and the intro meets the resting
/// frame without a jump.
pub(crate) fn welcome_shine(elapsed: Duration) -> Option<f64> {
    let progress = elapsed.as_secs_f64() / WELCOME_INTRO.as_secs_f64();
    if progress >= 1.0 {
        return None;
    }
    // A quadratic, not omp's cubic: a cubic spends the last two fifths of the
    // intro creeping past the far corner, which on a mark this small reads as
    // a shine that is over before the intro is.
    let eased = 1.0 - (1.0 - progress).powi(2);
    Some(-WELCOME_SHINE + (1.0 + 2.0 * WELCOME_SHINE) * eased)
}

/// The mark, shaded along its diagonal from the ink in the top-left corner to
/// the faded gray in the bottom-right: each cell at the mean of how far across
/// and how far down it is, the way omp shades its own mark. While the intro
/// plays, the cells within a band of `shine` are lifted toward the peak.
pub(crate) fn welcome_mark(grays: &WelcomeGrays, shine: Option<f64>) -> Vec<Vec<Span<'static>>> {
    let across = welcome_mark_width().saturating_sub(1).max(1) as f64;
    let down = WELCOME_MARK.len().saturating_sub(1).max(1) as f64;
    WELCOME_MARK
        .iter()
        .enumerate()
        .map(|(y, row)| {
            row.chars()
                .enumerate()
                .map(|(x, glyph)| {
                    if glyph == ' ' {
                        return Span::raw(" ");
                    }
                    let along = (x as f64 / across + y as f64 / down) / 2.0;
                    let rest = grays.bright + (grays.faded - grays.bright) * along;
                    let lift = shine.map_or(0.0, |at| {
                        (1.0 - (along - at).abs() / WELCOME_SHINE).max(0.0)
                    });
                    let level = rest + (grays.peak - rest) * lift;
                    Span::styled(glyph.to_string(), Style::default().fg(gray(level)))
                })
                .collect()
        })
        .collect()
}

/// The build as the lockup states it.
fn welcome_build(facts: &WelcomeFacts) -> String {
    format!("v{}", facts.version)
}

/// Cells the lockup takes across: the mark, the gap, and the wider of the
/// wordmark and the build.
fn welcome_lockup_width(facts: &WelcomeFacts) -> usize {
    let beside = WELCOME_WORDMARK
        .iter()
        .map(|row| titi_tui::width::visible_width(row))
        .chain([titi_tui::width::visible_width(&welcome_build(facts))])
        .max()
        .unwrap_or(0);
    welcome_mark_width() + WELCOME_GAP + beside
}

/// The mark with the wordmark beside it from its second row and the build
/// under the wordmark, the way omp sets its own lockup.
fn welcome_lockup(
    facts: &WelcomeFacts,
    grays: &WelcomeGrays,
    shine: Option<f64>,
) -> Vec<Line<'static>> {
    let word = grays.bright().add_modifier(Modifier::BOLD);
    let beside: Vec<Span<'static>> = WELCOME_WORDMARK
        .iter()
        .map(|row| Span::styled(*row, word))
        .chain([Span::styled(welcome_build(facts), grays.faded())])
        .collect();
    welcome_mark(grays, shine)
        .into_iter()
        .enumerate()
        .map(|(y, mut spans)| {
            if let Some(span) = y.checked_sub(1).and_then(|at| beside.get(at)) {
                spans.push(Span::raw(" ".repeat(WELCOME_GAP)));
                spans.push(span.clone());
            }
            Line::from(spans)
        })
        .collect()
}

/// The lockup in one line, for a pane too small for the mark: the name in the
/// wordmark's weight with the build beside it.
fn welcome_brand(facts: &WelcomeFacts, grays: &WelcomeGrays) -> Line<'static> {
    Line::from(vec![
        Span::styled("titi", grays.bright().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}", welcome_build(facts)), grays.faded()),
    ])
}

/// `lines` as one block whose widest line is centred in `width` cells. Every
/// line moves by the same indent, so a left-aligned block stays aligned.
fn centre_block(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    let indent = " ".repeat(width.saturating_sub(widest) / 2);
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(indent.clone())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Cells a fact's label takes.
pub(crate) const WELCOME_LABEL: usize = 9;

/// A labelled fact: the label in the faded gray, the value in the ink.
///
/// A value too long for the row keeps its tail behind an ellipsis — the leaf of
/// a path and the last segment of a model id are what a reader needs — rather
/// than being cut at whatever cell the row happens to end on.
fn welcome_fact(label: &str, value: &str, room: usize, grays: &WelcomeGrays) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            titi_tui::width::truncate_to_width(&format!("{label:<WELCOME_LABEL$}"), WELCOME_LABEL),
            grays.faded(),
        ),
        Span::styled(
            fit_tail(value, room.saturating_sub(WELCOME_LABEL)),
            grays.bright(),
        ),
    ]
}

/// The width `spans` take.
fn welcome_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| titi_tui::width::visible_width(&span.content))
        .sum()
}

/// A fact row with an optional second fact behind it.
///
/// The tail is stated whole or not at all: a branch or a credential word cut in
/// half says something that is not true — a detached HEAD's short sha would read
/// as whatever cells happened to fit — and a welcome that cannot fit the git
/// state is better off without it, exactly as the masthead is.
fn welcome_with_tail(
    mut spans: Vec<Span<'static>>,
    tail: Option<Vec<Span<'static>>>,
    room: usize,
) -> Vec<Span<'static>> {
    if let Some(tail) = tail
        && welcome_width(&spans) + welcome_width(&tail) <= room
    {
        spans.extend(tail);
    }
    spans
}

/// What a short pane gives up, least load-bearing first: level `n` of the
/// welcome has given up the first `n` of these.
///
/// The lockup and the chords are not on the list. On a short pane the brand
/// with its build, and the keys that start something, are what a first screen
/// is for; a blank row is not a fact, so it goes before the model does, and a
/// tip is not a fact about this session at all, so it goes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WelcomePart {
    Tip,
    Sessions,
    Dir,
    Tagline,
    Spacing,
    Model,
}

impl WelcomePart {
    fn shown(self, level: u8) -> bool {
        self as u8 >= level
    }
}

/// The level that has given every [`WelcomePart`] up.
const WELCOME_BARE: u8 = WelcomePart::Model as u8 + 1;

/// The facts block at one degradation level, every row at most `room` cells:
/// the model and its credential, the directory and its git state, and the
/// recent sessions, each label over the same column.
pub(crate) fn welcome_fact_rows(
    facts: &WelcomeFacts,
    level: u8,
    room: usize,
    grays: &WelcomeGrays,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    if WelcomePart::Model.shown(level) && !facts.model.is_empty() {
        let spans = welcome_fact("model", &facts.model, room, grays);
        let credential = facts
            .credential
            .as_ref()
            .map(|credential| vec![Span::styled(format!("  ·  {credential}"), grays.faded())]);
        rows.push(Line::from(welcome_with_tail(spans, credential, room)));
    }
    if WelcomePart::Dir.shown(level) && !facts.path.is_empty() {
        let spans = welcome_fact("dir", &facts.path, room, grays);
        let git = facts.git.as_ref().map(|git| {
            let mut tail = vec![Span::styled("  ·  ", grays.faded())];
            tail.extend(welcome_git(git, grays));
            tail
        });
        rows.push(Line::from(welcome_with_tail(spans, git, room)));
    }
    if WelcomePart::Sessions.shown(level) {
        for (at, (name, current)) in facts.sessions.iter().enumerate() {
            // The first row carries the label; the rest align under it, so a
            // list of sessions reads as one fact rather than several.
            let label = if at == 0 { "recent" } else { "" };
            // The same mark in the same words as the switcher's row: one namer,
            // so a session is never named two ways on one screen. Its room is
            // taken off the name, which keeps an ellipsis, not off the mark.
            let mark = if *current { "  ✓ current" } else { "" };
            let mut spans = welcome_fact(
                label,
                name,
                room.saturating_sub(titi_tui::width::visible_width(mark)),
                grays,
            );
            if !mark.is_empty() {
                spans.push(Span::styled(mark, grays.faded()));
            }
            rows.push(Line::from(spans));
        }
    }
    rows
}

/// The chords the live screen answers to, and no others: the model picker's
/// chord is the one the crate's table binds and the live mapper yields, not the
/// `/model` command or a chord that reaches nothing.
const WELCOME_CHORDS: [&str; 3] = ["enter  send", "alt+m  models", "ctrl-c  quit"];

/// As many of [`WELCOME_CHORDS`] as fit in `width` cells, each one whole: a
/// chord cut after its key would name an action it does not take.
fn welcome_hint(width: usize) -> String {
    let mut hint = String::new();
    for chord in WELCOME_CHORDS {
        let next = if hint.is_empty() {
            chord.to_owned()
        } else {
            format!("{hint}      {chord}")
        };
        if titi_tui::width::visible_width(&next) > width {
            break;
        }
        hint = next;
    }
    hint
}

/// The narrowest pane that shows a tip.
pub(crate) const WELCOME_TIP_COLUMNS: usize = 50;

/// What the first screen can point at, each one true of this build — a chord
/// the live mapper yields or a command [`COMMANDS`] runs — and short enough to
/// fit whole at [`WELCOME_TIP_COLUMNS`].
pub(crate) const WELCOME_TIPS: [&str; 20] = [
    "enter during a turn steers it",
    "ctrl-c stops a turn, and twice quits",
    "alt+m picks a model, and typing filters",
    "ctrl-x switches to another session",
    "/checkpoint records a rewind point",
    "/rewind cuts back to a rewind point",
    "/recap says what this session did",
    "/plan reads the repo and changes nothing",
    "/done leaves plan or duck mode",
    "/duck talks it through, repo-blind",
    "/theme opens the palette picker",
    "/usage shows the tokens spent",
    "/keys shows which providers have a key",
    "/login signs in or stores a key",
    "/model <id> switches the model",
    "/compact folds the history now",
    "/goal codes and reviews until it passes",
    "/fork copies this session into a new one",
    "/export saves this session as markdown",
    "/help lists every command",
];

/// The tip a session's welcome offers. It is picked by the session's id, so
/// every frame of one session offers the same tip and a new session may offer
/// another.
pub(crate) fn welcome_tip(session_id: &str) -> Option<&'static str> {
    // FNV-1a: the same id picks the same tip on every run and every build,
    // which the standard library's hasher does not promise.
    let hash = session_id
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    let at = usize::try_from(hash % WELCOME_TIPS.len() as u64).ok()?;
    WELCOME_TIPS.get(at).copied()
}

/// The git state as the masthead states it: the branch, then one mark per kind
/// of change, in the ink.
fn welcome_git(git: &WelcomeGit, grays: &WelcomeGrays) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(git.branch.clone(), grays.bright())];
    for (count, mark) in [(git.unstaged, "*"), (git.staged, "+"), (git.untracked, "?")] {
        if count > 0 {
            spans.push(Span::styled(format!(" {mark}{count}"), grays.bright()));
        }
    }
    spans
}

/// What the welcome opens with.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WelcomeHead {
    /// The mark beside the wordmark, with the intro's shine where it is.
    Lockup { shine: Option<f64> },
    /// The brand in one line, for a pane with no room for the mark.
    Brand,
}

/// The welcome's rows at one degradation level, each centred in `width`
/// cells: the head; the tagline; the facts as one left-aligned block; the
/// chords; and the tip.
fn welcome_rows(
    facts: &WelcomeFacts,
    tip: Option<&str>,
    level: u8,
    head: WelcomeHead,
    width: usize,
    grays: &WelcomeGrays,
) -> Vec<Line<'static>> {
    let head = match head {
        WelcomeHead::Lockup { shine } => welcome_lockup(facts, grays, shine),
        WelcomeHead::Brand => vec![welcome_brand(facts, grays)],
    };
    let mut sections = vec![centre_block(head, width)];
    if WelcomePart::Tagline.shown(level) {
        sections.push(centre_block(
            vec![Line::from(Span::styled(
                "say what you want done",
                grays.bright(),
            ))],
            width,
        ));
    }
    let block = welcome_fact_rows(
        facts,
        level,
        width.saturating_sub(2).min(WELCOME_MEASURE),
        grays,
    );
    if !block.is_empty() {
        sections.push(centre_block(block, width));
    }
    sections.push(centre_block(
        vec![Line::from(Span::styled(welcome_hint(width), grays.faded()))],
        width,
    ));
    if let Some(tip) = tip
        && WelcomePart::Tip.shown(level)
        && width >= WELCOME_TIP_COLUMNS
    {
        let line = format!("Tip: {tip}");
        if titi_tui::width::visible_width(&line) + 2 <= width {
            sections.push(centre_block(
                vec![Line::from(Span::styled(
                    line,
                    grays.faded().add_modifier(Modifier::ITALIC),
                ))],
                width,
            ));
        }
    }
    let spaced = WelcomePart::Spacing.shown(level);
    let mut rows = Vec::new();
    for section in sections {
        if spaced && !rows.is_empty() {
            rows.push(Line::from(""));
        }
        rows.extend(section);
    }
    rows
}

/// The first screen, set the way omp opens: the TITI mark beside the `titi`
/// wordmark with the build under it, the tagline, the facts, the chords and a
/// tip, centred in the pane with no box around them, in black and white.
///
/// This is an empty state, not a panel: it is drawn only while the transcript
/// has no line, it never asks for more room than the pane has, and on a short
/// pane it gives things up through [`WelcomePart`]'s order rather than let the
/// composer's rows be squeezed by a paragraph that cannot fit.
pub(crate) fn empty_state(
    chat: &Chat,
    width: u16,
    height: u16,
    theme: &Theme,
) -> Paragraph<'static> {
    let facts = welcome_facts(chat);
    let tip = welcome_tip(&chat.session_id);
    let grays = WelcomeGrays::of(theme);
    let (width, height) = (usize::from(width), usize::from(height));
    // A column clear on each side of the mark; a pane narrower than that states
    // the brand in one line rather than cut the mark down the middle.
    let head = if width >= welcome_lockup_width(&facts) + 2 {
        WelcomeHead::Lockup {
            shine: chat.intro.and_then(|start| welcome_shine(start.elapsed())),
        }
    } else {
        WelcomeHead::Brand
    };
    let mut rows = Vec::new();
    for level in 0..=WELCOME_BARE {
        rows = welcome_rows(&facts, tip, level, head, width, &grays);
        if rows.len() <= height {
            break;
        }
    }
    if rows.len() > height {
        rows = welcome_rows(&facts, tip, WELCOME_BARE, WelcomeHead::Brand, width, &grays);
    }
    let pad = height.saturating_sub(rows.len()) / 2;
    let mut lines = vec![Line::from(""); pad];
    lines.extend(rows);
    Paragraph::new(lines).style(page(theme))
}
