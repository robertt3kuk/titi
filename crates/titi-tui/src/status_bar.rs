//! The status line: one table of presets, one painter, one context gauge.
//!
//! A preset is **data** — a segment set, a separator style and the order the
//! segments are shed in when the pane is too narrow ([`PRESETS`]). The painter
//! ([`render_status_line`]) walks the table; a new preset is a row in it, not a
//! fifth `match` arm painting a row of its own. The `default` row is today's
//! layout segment for segment and byte for byte: a user who sets nothing sees
//! exactly what they saw before the table existed.
//!
//! The gap between the left and right groups is also the **gauge** when
//! `statusLine.contextLine` asks for one: a rule filled in the accent up to the
//! used share of the model's context window, the rest in the border colour,
//! with `72% · 128k` embedded at its right end ([`ContextLine`]).
//!
//! What this crate does **not** paint, and why: omp's `nerd`/`custom` presets
//! need Nerd Font glyphs and a user-written segment list, and its `status`,
//! `stream`, `vim`, `subagent`, `cache_*`, `token_rate` and `time*` segments
//! have no fact behind them here. `cost` and `pr` have none either — titi
//! tracks no spend and opens no pull request — so the snapshot carries neither.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use crate::status::compact_tokens;
use crate::theme::{SymbolPreset, Theme, ThemeColor};
use crate::width::{truncate_to_width, visible_width};

/// One fact the status line can paint.
///
/// The catalog is exactly what this product can state; an id a preset names but
/// the snapshot cannot fill is simply hidden, which is what keeps a preset
/// usable on a machine with no git remote, no session name and no spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// The product mark, bold in the accent.
    Brand,
    /// The session's state word, in the state's own colour.
    State,
    /// The active model, provider prefix and all until the pane is too narrow.
    Model,
    /// Plan / duck / …, hidden in plain agent mode.
    Mode,
    /// Background loops the engine reported, hidden when there are none.
    Loops,
    /// The working directory, `~`-folded.
    Path,
    /// The git branch and its dirty counts, hidden outside a repository.
    Git,
    /// The session's name, hidden until the engine has named it.
    Session,
    /// Session token totals, hidden before a turn reports any.
    Tokens,
    /// The context window fill: the `NNN%` slot, or the gauge's label.
    Context,
}

/// The glyph between two segments, and whether the whole line stays ASCII.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    /// The crate's thin powerline (`sep.powerlineThinLeft`), the default.
    Thin,
    /// ASCII throughout — separators **and** icons — for a terminal the
    /// default's glyphs do not fit.
    Ascii,
}

/// How the line between the left and right groups reflects the context.
///
/// `Off` is the default: the gap stays air, exactly as it was before this key
/// existed, which is what makes the compatibility promise keepable — a user who
/// sets nothing sees no new pixels anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextLine {
    /// No gauge: the gap between the groups is blank, as it always was.
    #[default]
    Off,
    /// The rule, filled in the accent up to the used share; no label.
    Percentage,
    /// The rule with `72% · 128k` embedded at its right end.
    Embedded,
}

impl ContextLine {
    /// Every name the setting accepts, in the order a listing shows them.
    pub const IDS: [&'static str; 3] = ["off", "percentage", "embedded"];

    /// The setting's own name for this mode.
    pub fn id(self) -> &'static str {
        match self {
            ContextLine::Off => "off",
            ContextLine::Percentage => "percentage",
            ContextLine::Embedded => "embedded",
        }
    }

    /// Parse a setting name; `None` for anything else.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "off" => Some(ContextLine::Off),
            "percentage" => Some(ContextLine::Percentage),
            "embedded" => Some(ContextLine::Embedded),
            _ => None,
        }
    }
}

/// A pre-built status line, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusLinePreset {
    /// Today's layout, unchanged.
    #[default]
    Default,
    /// The model and the context only.
    Minimal,
    /// The model, the mode, the directory and the context.
    Compact,
    /// Every fact this product has, token totals included.
    Full,
    /// The default set in ASCII.
    Ascii,
}

impl StatusLinePreset {
    /// Every name the setting accepts, in the order a listing shows them.
    pub const IDS: [&'static str; 5] = ["default", "minimal", "compact", "full", "ascii"];

    /// The setting's own name for this preset.
    pub fn id(self) -> &'static str {
        match self {
            StatusLinePreset::Default => "default",
            StatusLinePreset::Minimal => "minimal",
            StatusLinePreset::Compact => "compact",
            StatusLinePreset::Full => "full",
            StatusLinePreset::Ascii => "ascii",
        }
    }

    /// Parse a setting name; `None` for anything else — the caller falls back
    /// to `default` rather than refusing to start.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "default" => Some(StatusLinePreset::Default),
            "minimal" => Some(StatusLinePreset::Minimal),
            "compact" => Some(StatusLinePreset::Compact),
            "full" => Some(StatusLinePreset::Full),
            "ascii" => Some(StatusLinePreset::Ascii),
            _ => None,
        }
    }
}

/// One row of the table: what a preset paints, in what order, and in what it
/// gives up when the pane is too narrow.
#[derive(Debug, Clone, Copy)]
pub struct PresetDef {
    /// Segments of the left group, in reading order.
    pub left: &'static [Segment],
    /// Segments of the right group, in reading order.
    pub right: &'static [Segment],
    /// The glyph between two segments.
    pub separator: Separator,
    /// What one line of prose says this preset is, for `/statusline`.
    pub about: &'static str,
    /// The order segments are shed in when the pane cannot hold the line. A
    /// segment named here but absent costs one pass of the fit loop and nothing
    /// else, so a preset drops what it has without knowing what the machine has.
    pub drop: &'static [Segment],
}

/// The default preset's shed order: the git state first (the most cells for the
/// least actionable fact), then the directory, the session's name, the mode and
/// the loop count.
const DEFAULT_DROP: &[Segment] = &[
    Segment::Git,
    Segment::Path,
    Segment::Session,
    Segment::Mode,
    Segment::Loops,
];

/// The whole table. One row per preset — the presets are these rows.
const PRESETS: [(StatusLinePreset, PresetDef); 5] = [
    (
        StatusLinePreset::Default,
        PresetDef {
            left: &[
                Segment::Brand,
                Segment::State,
                Segment::Mode,
                Segment::Loops,
                Segment::Path,
                Segment::Git,
            ],
            right: &[Segment::Session, Segment::Model, Segment::Context],
            separator: Separator::Thin,
            about: "the mark and the state, the mode, the directory, git, the name, the model, the context",
            drop: DEFAULT_DROP,
        },
    ),
    (
        StatusLinePreset::Minimal,
        PresetDef {
            left: &[Segment::Model],
            right: &[Segment::Context],
            separator: Separator::Thin,
            about: "the model and the context only",
            drop: &[],
        },
    ),
    (
        StatusLinePreset::Compact,
        PresetDef {
            left: &[Segment::Model, Segment::Mode, Segment::Path],
            right: &[Segment::Context],
            separator: Separator::Thin,
            about: "the model, the mode, the directory and the context",
            drop: &[Segment::Mode, Segment::Path],
        },
    ),
    (
        StatusLinePreset::Full,
        PresetDef {
            left: &[
                Segment::Brand,
                Segment::State,
                Segment::Mode,
                Segment::Loops,
                Segment::Path,
                Segment::Git,
            ],
            right: &[
                Segment::Session,
                Segment::Model,
                Segment::Tokens,
                Segment::Context,
            ],
            separator: Separator::Thin,
            about: "every fact titi has, token totals and the git branch included",
            drop: &[
                Segment::Git,
                Segment::Path,
                Segment::Tokens,
                Segment::Session,
                Segment::Loops,
                Segment::Mode,
            ],
        },
    ),
    (
        StatusLinePreset::Ascii,
        PresetDef {
            left: &[
                Segment::Brand,
                Segment::State,
                Segment::Mode,
                Segment::Loops,
                Segment::Path,
                Segment::Git,
            ],
            right: &[Segment::Session, Segment::Model, Segment::Context],
            separator: Separator::Ascii,
            about: "the default set in ASCII glyphs and separators",
            drop: DEFAULT_DROP,
        },
    ),
];

/// The row for `preset`. The table cannot be missing it; a row added to the
/// enum without a row here falls back to `default` rather than panicking.
pub fn preset(preset: StatusLinePreset) -> &'static PresetDef {
    PRESETS
        .iter()
        .find_map(|(name, def)| (*name == preset).then_some(def))
        .unwrap_or_else(|| &PRESETS[0].1)
}

/// What the status line is set to: a preset, and what its middle does with the
/// context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusLineStyle {
    pub preset: StatusLinePreset,
    pub context_line: ContextLine,
}

impl StatusLineStyle {
    /// From the settings' own names. An unset name is the default and an
    /// unknown one falls back to it too: a typo in a cosmetic key must not
    /// change the screen, and must never refuse to start.
    pub fn resolve(preset: Option<&str>, context_line: Option<&str>) -> Self {
        Self {
            preset: preset
                .and_then(StatusLinePreset::from_id)
                .unwrap_or_default(),
            context_line: context_line
                .and_then(ContextLine::from_id)
                .unwrap_or_default(),
        }
    }
}

/// Cells kept between the left and right groups: at least one, so a full line
/// cannot run one group into the other.
const MIN_GAP: usize = 1;

/// Cells the session's name may take before it is cut with an ellipsis. The
/// namer caps a title at forty characters (`titi_core::session::namer`), which
/// is more than a narrow pane can give away.
const SESSION_SLOT: usize = 18;

/// Cells the `NNN%` context slot always occupies: ` 3%`, `42%`, `100%`.
const CONTEXT_SLOT: usize = 4;

/// Snapshot of the values a preset can paint. Empty optionals hide.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnapshot {
    /// The product mark, when the surface wants one.
    pub brand: Option<String>,
    /// The session's state word.
    pub state: Option<String>,
    /// The colour of the state word, the mode and the loop count — one cluster
    /// of "what this session is doing", so one colour.
    pub state_color: ThemeColor,
    /// Active model id, as the provider names it.
    pub model: String,
    /// Plan/duck/… label; `None` hides the mode segment (plain agent).
    pub mode: Option<String>,
    /// Background loops the engine reported; `None` hides the segment.
    pub loops: Option<usize>,
    /// Working directory, already absolute or `~` form.
    pub path: String,
    /// Git branch name.
    pub git_branch: Option<String>,
    pub git_unstaged: u32,
    pub git_staged: u32,
    pub git_untracked: u32,
    /// Context window fill 0–100.
    pub context_pct: Option<u8>,
    /// The model's context window in tokens, as the engine reported it. `None`
    /// until a turn states it — and a gauge cannot be drawn without it.
    pub context_window: Option<u64>,
    /// Session token totals, `(prompt, completion)`, as the engine has summed
    /// them. `None` before the first turn reports any.
    pub tokens: Option<(u32, u32)>,
    /// Right-group session title.
    pub session_name: String,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        StatusSnapshot {
            brand: None,
            state: None,
            state_color: ThemeColor::Dim,
            model: "no-model".to_owned(),
            mode: None,
            loops: None,
            path: String::new(),
            git_branch: None,
            git_unstaged: 0,
            git_staged: 0,
            git_untracked: 0,
            context_pct: None,
            context_window: None,
            tokens: None,
            session_name: String::new(),
        }
    }
}

/// Build a snapshot from the live process (cwd + git HEAD + porcelain dirty).
///
/// The model is kept exactly as the caller names it: which form to show is the
/// painter's decision (the full id, then the bare one, then a cut), not the
/// reader's.
pub fn live_snapshot(model: &str, session_name: &str) -> StatusSnapshot {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = abbreviate_path(&cwd.to_string_lossy(), 40);
    let git = git_info(&cwd);
    StatusSnapshot {
        model: model.to_owned(),
        path,
        git_branch: git.branch,
        git_unstaged: git.unstaged,
        git_staged: git.staged,
        git_untracked: git.untracked,
        session_name: session_name.to_owned(),
        ..StatusSnapshot::default()
    }
}

/// Paint `style`'s status line, padded/truncated to `width` cells.
///
/// The line is the left group, the gap, then the right group. The gap is air
/// unless `style.context_line` asks for the gauge; the gauge is the rule with
/// the used share lit, and — when the window is known and the gap can hold it —
/// `72% · 128k` embedded at its right end.
pub fn render_status_line(
    theme: &Theme,
    width: u16,
    style: StatusLineStyle,
    snap: &StatusSnapshot,
) -> String {
    let cells = width as usize;
    if cells == 0 {
        return String::new();
    }
    let def = preset(style.preset);
    let ascii = def.separator == Separator::Ascii;
    let sep = separator_text(theme, def.separator);
    // `embedded` moves the number out of the group and into the gauge's label,
    // so the context segment leaves the group — but only when the window is
    // known, because with no window there is no gauge and no label to carry it,
    // and the group must keep showing the slot it always did.
    let absorbed = style.context_line == ContextLine::Embedded && snap.context_window.is_some();
    let kept = |seg: &Segment| !(absorbed && *seg == Segment::Context);

    let mut left: Vec<(Segment, String)> = def
        .left
        .iter()
        .copied()
        .filter(kept)
        .filter_map(|seg| segment_text(seg, theme, ascii, snap).map(|text| (seg, text)))
        .collect();
    let mut right: Vec<(Segment, String)> = def
        .right
        .iter()
        .copied()
        .filter(kept)
        .filter_map(|seg| segment_text(seg, theme, ascii, snap).map(|text| (seg, text)))
        .collect();

    // The label's own cells, when `embedded` will draw one: the fit loop
    // reserves them, so a narrow pane sheds a segment rather than losing the
    // number entirely. When the pane cannot hold the label at all the
    // reservation falls back to one cell — the labels cannot render at that
    // width either way, and the line must still be drawn.
    let label_cells = if style.context_line == ContextLine::Embedded {
        embedded_label_cells(snap)
    } else {
        0
    };
    let min_gap = if label_cells > 0 && label_cells + 2 <= cells {
        label_cells + 2
    } else {
        MIN_GAP
    };

    // What the line gives up, in order, when the pane is too narrow: the
    // preset's shed list, then the model's provider prefix, then the model
    // itself, cut with an ellipsis so a cut can never be read as a whole id.
    let mut shed: Vec<Segment> = Vec::new();
    let mut shortened = false;
    loop {
        let left_s = join(&left, &sep);
        let right_s = join(&right, &sep);
        if visible_width(&left_s) + min_gap + visible_width(&right_s) <= cells {
            break;
        }
        if let Some(next) = def.drop.iter().find(|seg| !shed.contains(seg)) {
            shed.push(*next);
            left.retain(|(seg, _)| seg != next);
            right.retain(|(seg, _)| seg != next);
            continue;
        }
        if !shortened {
            shortened = true;
            set_part(
                &mut left,
                &mut right,
                Segment::Model,
                theme.fg(
                    ThemeColor::Muted,
                    &model_part(theme, ascii, &short_model(&snap.model)),
                ),
            );
            continue;
        }
        let others = join(&without(&right, Segment::Model), &sep);
        let room = cells.saturating_sub(visible_width(&left_s) + min_gap + visible_width(&others));
        set_part(
            &mut left,
            &mut right,
            Segment::Model,
            fitted_model(theme, ascii, &snap.model, room),
        );
        break;
    }

    let left_s = join(&left, &sep);
    let right_s = join(&right, &sep);
    let gap = cells
        .saturating_sub(visible_width(&left_s) + visible_width(&right_s))
        .max(min_gap);
    let fill = match style.context_line {
        ContextLine::Off => " ".repeat(gap),
        mode => gauge(theme, ascii, gap, mode, snap),
    };
    truncate_to_width(&format!("{left_s}{fill}{right_s}"), cells)
}

/// Cells the embedded label takes: `72% · 128k`, or the window alone before a
/// percentage is known. Zero when there is no window to label — and a label is
/// what the fit loop reserves room for, so this is the one place its width is
/// decided.
fn embedded_label_cells(snap: &StatusSnapshot) -> usize {
    let Some(window) = snap.context_window else {
        return 0;
    };
    let window_text = compact_tokens(u32::try_from(window).unwrap_or(u32::MAX));
    let percent = snap
        .context_pct
        .map(|pct| visible_width(&format!("{pct}%")) + 3)
        .unwrap_or(0);
    percent + visible_width(&window_text)
}

/// The gap's rule, and the gauge with it: `mode` cells of the rule, the first
/// `used` of them in the accent and the rest in the border colour, with the
/// label embedded at the right end when one fits.
///
/// The label is a unit anchored at the right, so a percent that gains a digit
/// (`9%` → `10%`) grows into the rule instead of moving anything: the groups on
/// either side of the gap keep their columns. 0% lights nothing and 100% lights
/// every cell — the two ends are drawn as they are stated.
fn gauge(
    theme: &Theme,
    ascii: bool,
    cells: usize,
    mode: ContextLine,
    snap: &StatusSnapshot,
) -> String {
    // No window, no gauge: a rule with nothing to say would read as a fact.
    let Some(window) = snap.context_window else {
        return " ".repeat(cells);
    };
    let rule = rule_glyph(theme, ascii);
    // The separator inside the label is three cells in both styles, so the
    // width the fit loop reserves is one number (`embedded_label_cells`); what
    // changes with the style is only whether it is printable.
    let label_sep = if ascii { " - " } else { " · " };
    let (percent, window_text) = match mode {
        // One rule cell on each side, so the label reads as embedded in the
        // line rather than as the end of it.
        ContextLine::Embedded if embedded_label_cells(snap) + 2 <= cells => {
            let window_text = compact_tokens(u32::try_from(window).unwrap_or(u32::MAX));
            (
                snap.context_pct.map(|pct| format!("{pct}%")),
                Some(window_text),
            )
        }
        _ => (None, None),
    };
    let label_cells = match &window_text {
        Some(text) => {
            percent
                .as_ref()
                .map_or(0, |p| visible_width(p) + visible_width(label_sep))
                + visible_width(text)
        }
        None => 0,
    };
    let label_start = if label_cells > 0 {
        cells - 1 - label_cells
    } else {
        cells
    };

    let used = snap
        .context_pct
        .map(|pct| (usize::from(pct) * cells + 50) / 100)
        .unwrap_or(0)
        .min(cells);
    let mut out = String::new();
    let mut at = 0usize;
    while at < cells {
        if label_cells > 0 && at == label_start {
            if let Some(text) = &percent {
                out.push_str(&theme.fg(ThemeColor::StatusLineContext, text));
                out.push_str(&theme.fg(ThemeColor::Muted, label_sep));
            }
            if let Some(text) = &window_text {
                out.push_str(&theme.fg(ThemeColor::Muted, text));
            }
            at += label_cells;
            continue;
        }
        let lit = at < used;
        let mut end = at + 1;
        while end < cells && (end < used) == lit && !(label_cells > 0 && end == label_start) {
            end += 1;
        }
        let color = if lit {
            ThemeColor::BorderAccent
        } else {
            ThemeColor::Border
        };
        out.push_str(&theme.fg(color, &rule.repeat(end - at)));
        at = end;
    }
    out
}

/// The rule's own glyph: the box-drawing bar the crate's frames use, or the
/// ASCII hyphen for a line that must stay printable.
fn rule_glyph(theme: &Theme, ascii: bool) -> String {
    let glyph = sym(theme, ascii, "boxRound.horizontal");
    if glyph.is_empty() {
        "-".to_owned()
    } else {
        glyph.to_owned()
    }
}

/// One segment's text, or `None` when the fact behind it is absent.
fn segment_text(seg: Segment, theme: &Theme, ascii: bool, snap: &StatusSnapshot) -> Option<String> {
    match seg {
        // The mark and the state word open the line as one cluster: a leading
        // space before the mark and two plain spaces between the two, so the
        // brand reads as a unit rather than as a segment.
        Segment::Brand => snap
            .brand
            .as_ref()
            .map(|brand| format!(" {}", theme.fg(ThemeColor::Accent, &theme.bold(brand)))),
        Segment::State => snap
            .state
            .as_ref()
            .map(|state| theme.fg(snap.state_color, state)),
        Segment::Model => Some(theme.fg(ThemeColor::Muted, &model_part(theme, ascii, &snap.model))),
        Segment::Mode => snap
            .mode
            .as_ref()
            .map(|mode| theme.fg(snap.state_color, mode)),
        Segment::Loops => snap
            .loops
            .map(|loops| theme.fg(snap.state_color, &format!("{loops} loop(s)"))),
        Segment::Path => path_part(theme, ascii, &snap.path),
        Segment::Git => git_part(theme, ascii, snap),
        Segment::Session => session_part(theme, &snap.session_name),
        Segment::Tokens => snap
            .tokens
            .filter(|(prompt, completion)| *prompt > 0 || *completion > 0)
            .map(|(prompt, completion)| {
                theme.fg(
                    ThemeColor::Muted,
                    &format!(
                        "{} in · {} out",
                        compact_tokens(prompt),
                        compact_tokens(completion)
                    ),
                )
            }),
        Segment::Context => Some(context_part(theme, snap)),
    }
}

/// The model with its icon.
fn model_part(theme: &Theme, ascii: bool, model: &str) -> String {
    let icon = sym(theme, ascii, "icon.model");
    if icon.is_empty() {
        model.to_owned()
    } else {
        format!("{icon} {model}")
    }
}

/// The model cut to `room` cells, with an ellipsis: the last resort, when even
/// the short form does not fit beside the rest of the line.
fn fitted_model(theme: &Theme, ascii: bool, model: &str, room: usize) -> String {
    let icon = sym(theme, ascii, "icon.model");
    let prefix = if icon.is_empty() {
        String::new()
    } else {
        format!("{icon} ")
    };
    let room = room.saturating_sub(visible_width(&prefix));
    let head = truncate_to_width(&short_model(model), room.saturating_sub(1));
    theme.fg(ThemeColor::Muted, &format!("{prefix}{head}…"))
}

/// The working directory, hidden when empty.
fn path_part(theme: &Theme, ascii: bool, path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let icon = sym(theme, ascii, "icon.folder");
    let text = if icon.is_empty() {
        path.to_owned()
    } else {
        format!("{icon} {path}")
    };
    Some(theme.fg(ThemeColor::StatusLinePath, &text))
}

/// The git branch and its dirty counts, hidden outside a repository.
fn git_part(theme: &Theme, ascii: bool, snap: &StatusSnapshot) -> Option<String> {
    let branch = snap.git_branch.as_ref()?;
    let icon = sym(theme, ascii, "icon.branch");
    let mut git = if icon.is_empty() {
        branch.clone()
    } else {
        format!("{icon} {branch}")
    };
    for (count, mark, token) in [
        (snap.git_unstaged, "*", ThemeColor::StatusLineDirty),
        (snap.git_staged, "+", ThemeColor::StatusLineStaged),
        (snap.git_untracked, "?", ThemeColor::StatusLineUntracked),
    ] {
        if count > 0 {
            git.push(' ');
            git.push_str(&theme.fg(token, &format!("{mark}{count}")));
        }
    }
    let dirty = snap.git_unstaged > 0 || snap.git_staged > 0 || snap.git_untracked > 0;
    let color = if dirty {
        ThemeColor::StatusLineGitDirty
    } else {
        ThemeColor::StatusLineGitClean
    };
    Some(theme.fg(color, &git))
}

/// The session's name, cut to [`SESSION_SLOT`] with an ellipsis, hidden while
/// the engine has not named the session yet.
fn session_part(theme: &Theme, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let shown = if visible_width(name) > SESSION_SLOT {
        format!(
            "{}…",
            truncate_to_width(name, SESSION_SLOT.saturating_sub(1))
        )
    } else {
        name.to_owned()
    };
    Some(theme.fg(ThemeColor::Muted, &shown))
}

/// The context slot: always [`CONTEXT_SLOT`] cells, so the model beside it
/// cannot move when the first percentage arrives. Blank until one has been
/// reported, so the digits appear where the space already was.
fn context_part(theme: &Theme, snap: &StatusSnapshot) -> String {
    let text = match snap.context_pct {
        Some(percent) => format!("{percent:>3}%"),
        None => String::new(),
    };
    theme.fg(
        ThemeColor::StatusLineContext,
        &format!("{text:>CONTEXT_SLOT$}"),
    )
}

/// Join a group's segments, with the mark and the state word as one cluster.
fn join(parts: &[(Segment, String)], sep: &str) -> String {
    let mut out = String::new();
    for (index, (seg, text)) in parts.iter().enumerate() {
        if index > 0 {
            if parts[index - 1].0 == Segment::Brand && *seg == Segment::State {
                out.push_str("  ");
            } else {
                out.push_str(sep);
            }
        }
        out.push_str(text);
    }
    out
}

/// The same group without `seg`.
fn without(parts: &[(Segment, String)], seg: Segment) -> Vec<(Segment, String)> {
    parts
        .iter()
        .filter(|(part, _)| *part != seg)
        .cloned()
        .collect()
}

/// Replace a group's `seg` in place, whichever group holds it.
fn set_part(
    left: &mut [(Segment, String)],
    right: &mut [(Segment, String)],
    seg: Segment,
    text: String,
) {
    for part in left.iter_mut().chain(right.iter_mut()) {
        if part.0 == seg {
            part.1 = text.clone();
        }
    }
}

/// The separator between two segments, as cells: a space, the glyph in the
/// crate's separator colour, a space.
fn separator_text(theme: &Theme, style: Separator) -> String {
    let glyph = match style {
        Separator::Thin => sym(theme, false, "sep.powerlineThinLeft"),
        Separator::Ascii => sym(theme, true, "sep.asciiLeft"),
    };
    if glyph.is_empty() {
        " > ".to_owned()
    } else {
        format!(" {} ", theme.fg(ThemeColor::StatusLineSep, glyph))
    }
}

/// A symbol by key, out of the ASCII table when the line must stay printable.
fn sym<'a>(theme: &'a Theme, ascii: bool, key: &str) -> &'a str {
    if !ascii {
        return theme.symbol(key);
    }
    crate::theme::symbols::symbols(SymbolPreset::Ascii)
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, glyph)| *glyph)
        .unwrap_or_default()
}

/// A model id without its provider prefix (`openai-codex/gpt-5.5` → `gpt-5.5`).
pub fn short_model(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_owned()
}

fn abbreviate_path(raw: &str, max_len: usize) -> String {
    let mut s = raw.to_owned();
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = s.strip_prefix(&home)
    {
        s = format!("~{rest}");
    }
    let count = s.chars().count();
    if count <= max_len {
        return s;
    }
    let chars: Vec<char> = s.chars().collect();
    let keep = max_len.saturating_sub(1);
    let start = chars.len().saturating_sub(keep);
    format!("…{}", chars[start..].iter().collect::<String>())
}

struct GitInfo {
    root: PathBuf,
    head_mtime: Option<SystemTime>,
    index_mtime: Option<SystemTime>,
    branch: Option<String>,
    unstaged: u32,
    staged: u32,
    untracked: u32,
}

/// The last git read, on the mtimes of `.git/HEAD` and `.git/index`: a frame
/// that changes nothing costs two `stat`s instead of a `git` process.
static GIT_CACHE: LazyLock<Mutex<Option<GitInfo>>> = LazyLock::new(|| Mutex::new(None));

fn git_info(start: &Path) -> GitInfo {
    let empty = GitInfo {
        root: PathBuf::new(),
        head_mtime: None,
        index_mtime: None,
        branch: None,
        unstaged: 0,
        staged: 0,
        untracked: 0,
    };
    let Some((root, head_path, index_path)) = find_git(start) else {
        return empty;
    };
    let head_mtime = std::fs::metadata(&head_path)
        .ok()
        .and_then(|m| m.modified().ok());
    let index_mtime = std::fs::metadata(&index_path)
        .ok()
        .and_then(|m| m.modified().ok());
    if let Ok(guard) = GIT_CACHE.lock()
        && let Some(cached) = guard.as_ref()
        && cached.root == root
        && cached.head_mtime == head_mtime
        && cached.index_mtime == index_mtime
    {
        return GitInfo {
            root: cached.root.clone(),
            head_mtime,
            index_mtime,
            branch: cached.branch.clone(),
            unstaged: cached.unstaged,
            staged: cached.staged,
            untracked: cached.untracked,
        };
    }
    let branch = read_branch(&head_path);
    let (unstaged, staged, untracked) = git_porcelain_counts(&root);
    let fresh = GitInfo {
        root,
        head_mtime,
        index_mtime,
        branch,
        unstaged,
        staged,
        untracked,
    };
    if let Ok(mut guard) = GIT_CACHE.lock() {
        *guard = Some(GitInfo {
            root: fresh.root.clone(),
            head_mtime: fresh.head_mtime,
            index_mtime: fresh.index_mtime,
            branch: fresh.branch.clone(),
            unstaged: fresh.unstaged,
            staged: fresh.staged,
            untracked: fresh.untracked,
        });
    }
    fresh
}

fn find_git(start: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let mut dir = start.to_path_buf();
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some((dir, git.join("HEAD"), git.join("index")));
        }
        if git.is_file() {
            let text = std::fs::read_to_string(&git).ok()?;
            let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
            let gitdir = if gitdir.is_absolute() {
                gitdir
            } else {
                dir.join(gitdir)
            };
            return Some((dir, gitdir.join("HEAD"), gitdir.join("index")));
        }
        dir = dir.parent()?.to_path_buf();
    }
}

fn read_branch(head_path: &Path) -> Option<String> {
    let head = std::fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_owned());
    }
    if head.len() >= 7 {
        return Some(head.chars().take(7).collect());
    }
    None
}

fn git_porcelain_counts(repo: &Path) -> (u32, u32, u32) {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "status",
            "--porcelain=v1",
            "-unormal",
        ])
        .env("GIT_OPTIONAL_LOCKS", "1")
        .output();
    let Ok(out) = out else {
        return (0, 0, 0);
    };
    if !out.status.success() {
        return (0, 0, 0);
    }
    parse_porcelain(&String::from_utf8_lossy(&out.stdout))
}

fn parse_porcelain(text: &str) -> (u32, u32, u32) {
    let mut unstaged = 0u32;
    let mut staged = 0u32;
    let mut untracked = 0u32;
    for line in text.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            continue;
        }
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        if x == '?' && y == '?' {
            untracked = untracked.saturating_add(1);
            continue;
        }
        if x == '!' && y == '!' {
            continue;
        }
        if x != ' ' && x != '?' {
            staged = staged.saturating_add(1);
        }
        if y != ' ' && y != '?' {
            unstaged = unstaged.saturating_add(1);
        }
    }
    (unstaged, staged, untracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Theme, global};

    fn theme() -> std::sync::Arc<Theme> {
        global().init("titanium");
        global().current().expect("titanium")
    }

    /// The frame a plain agent in a repository paints: every default segment
    /// present and nothing hidden, so a preset's set is what the test measures.
    fn snap() -> StatusSnapshot {
        StatusSnapshot {
            brand: Some("titi".to_owned()),
            state: Some("ready".to_owned()),
            state_color: ThemeColor::Dim,
            model: "glm-5.3-flash".to_owned(),
            mode: Some("plan".to_owned()),
            loops: Some(1),
            path: "~/proj/notes".to_owned(),
            git_branch: Some("main".to_owned()),
            git_unstaged: 2,
            git_staged: 1,
            session_name: "blue-otter".to_owned(),
            tokens: Some((3_400, 250)),
            ..StatusSnapshot::default()
        }
    }

    fn style(preset: StatusLinePreset) -> StatusLineStyle {
        StatusLineStyle {
            preset,
            context_line: ContextLine::Off,
        }
    }

    fn visible(line: &str) -> String {
        // The painted line carries SGR escapes; what a terminal shows is the
        // text between them.
        crate::width::spans(line)
            .into_iter()
            .filter_map(|piece| match piece {
                crate::width::Span::Text(text) => Some(text),
                crate::width::Span::Escape(_) => None,
            })
            .collect()
    }

    #[test]
    fn the_default_preset_is_todays_line() {
        let theme = theme();
        let line = visible(&render_status_line(
            &theme,
            120,
            style(StatusLinePreset::Default),
            &snap(),
        ));
        for expected in [
            " titi",
            "ready",
            "plan",
            "1 loop(s)",
            "~/proj/notes",
            "main",
            "blue-otter",
            "glm-5.3-flash",
        ] {
            assert!(line.contains(expected), "{expected:?} missing: {line:?}");
        }
        let at = |needle: &str| {
            line.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing: {line:?}"))
        };
        assert!(
            at("titi") < at("ready")
                && at("ready") < at("plan")
                && at("plan") < at("~/proj/notes")
                && at("~/proj/notes") < at("main"),
            "left order: {line:?}"
        );
        assert!(
            at("blue-otter") < at("glm-5.3-flash"),
            "right order: {line:?}"
        );
    }

    #[test]
    fn every_preset_paints_the_segments_its_row_names() {
        let theme = theme();
        // (preset, must be present, must be absent)
        let cases: [(StatusLinePreset, &[&str], &[&str]); 5] = [
            (
                StatusLinePreset::Default,
                &[
                    "titi",
                    "ready",
                    "plan",
                    "~/proj/notes",
                    "main",
                    "blue-otter",
                    "glm-5.3-flash",
                ],
                &[" in · "],
            ),
            (
                StatusLinePreset::Minimal,
                &["glm-5.3-flash"],
                &[
                    "titi",
                    "ready",
                    "plan",
                    "~/proj/notes",
                    "main",
                    "blue-otter",
                    " in · ",
                ],
            ),
            (
                StatusLinePreset::Compact,
                &["glm-5.3-flash", "plan", "~/proj/notes"],
                &["titi", "ready", "main", "blue-otter", " in · "],
            ),
            (
                StatusLinePreset::Full,
                &[
                    "titi",
                    "ready",
                    "plan",
                    "~/proj/notes",
                    "main",
                    "blue-otter",
                    "glm-5.3-flash",
                    "3.4k in · 250 out",
                ],
                &[],
            ),
            (
                StatusLinePreset::Ascii,
                &["titi", "ready", "glm-5.3-flash"],
                &[" in · "],
            ),
        ];
        for (preset, present, absent) in cases {
            let line = visible(&render_status_line(&theme, 120, style(preset), &snap()));
            for needle in present {
                assert!(
                    line.contains(needle),
                    "{preset:?}: {needle:?} missing: {line:?}"
                );
            }
            for needle in absent {
                assert!(
                    !line.contains(needle),
                    "{preset:?}: {needle:?} shown: {line:?}"
                );
            }
            assert_eq!(
                visible_width(&render_status_line(&theme, 120, style(preset), &snap())),
                120,
                "{preset:?} fills the pane"
            );
        }
    }

    #[test]
    fn the_ascii_preset_stays_printable() {
        // With a window and a percentage, so the gauge and its label are part
        // of what has to stay printable — the label's own separator included.
        let mut s = snap();
        s.context_pct = Some(49);
        s.context_window = Some(400);
        for width in [60u16, 80, 120] {
            let line = visible(&render_status_line(
                &theme(),
                width,
                StatusLineStyle {
                    preset: StatusLinePreset::Ascii,
                    context_line: ContextLine::Embedded,
                },
                &s,
            ));
            assert!(line.is_ascii(), "{width}: not ASCII: {line:?}");
            assert!(!line.contains('─'), "{width}: box glyph: {line:?}");
            assert!(line.contains("49% - 400"), "{width}: {line:?}");
        }
    }

    #[test]
    fn the_segments_are_shed_in_the_presets_order() {
        let theme = theme();
        let line = visible(&render_status_line(
            &theme,
            60,
            style(StatusLinePreset::Default),
            &snap(),
        ));
        // At 60 the git state goes first, the directory next, the name with it;
        // the model and the facts beside it stay.
        assert!(!line.contains("main"), "git shed first: {line:?}");
        assert!(
            !line.contains("~/proj/notes"),
            "the directory next: {line:?}"
        );
        assert!(line.contains("glm-5.3-flash"), "the model stays: {line:?}");
        assert!(
            line.contains("plan"),
            "the mode outlives the directory: {line:?}"
        );
        assert!(
            line.contains("1 loop(s)"),
            "and so does the loop count: {line:?}"
        );
    }

    #[test]
    fn no_window_means_no_gauge() {
        let theme = theme();
        for mode in [ContextLine::Percentage, ContextLine::Embedded] {
            let line = render_status_line(
                &theme,
                120,
                StatusLineStyle {
                    preset: StatusLinePreset::Default,
                    context_line: mode,
                },
                &snap(),
            );
            let text = visible(&line);
            assert!(
                !text.contains('─'),
                "{mode:?}: a rule with no window: {text:?}"
            );
            assert!(
                !text.contains('%'),
                "{mode:?}: a percent with no window: {text:?}"
            );
        }
    }

    #[test]
    fn embedded_absorbs_the_context_segment_into_the_label() {
        let theme = theme();
        let mut s = snap();
        s.context_pct = Some(72);
        s.context_window = Some(128_000);
        let line = visible(&render_status_line(
            &theme,
            120,
            StatusLineStyle {
                preset: StatusLinePreset::Default,
                context_line: ContextLine::Embedded,
            },
            &s,
        ));
        assert!(line.contains("72% · 128k"), "{line:?}");
        assert!(!line.contains(" 72%"), "the slot left the group: {line:?}");
        assert!(line.contains('─'), "the rule is drawn: {line:?}");
    }

    #[test]
    fn the_percentage_gauge_carries_no_label() {
        let theme = theme();
        let mut s = snap();
        s.context_pct = Some(50);
        s.context_window = Some(128_000);
        let line = visible(&render_status_line(
            &theme,
            120,
            StatusLineStyle {
                preset: StatusLinePreset::Default,
                context_line: ContextLine::Percentage,
            },
            &s,
        ));
        assert!(line.contains('─'), "{line:?}");
        assert!(!line.contains("128k"), "{line:?}");
        assert!(line.contains(" 50%"), "the slot stays: {line:?}");
    }

    #[test]
    fn the_gauge_anchors_its_label_at_the_right() {
        let theme = theme();
        let at = |pct: u8| {
            let mut s = snap();
            s.context_pct = Some(pct);
            s.context_window = Some(128_000);
            visible(&render_status_line(
                &theme,
                120,
                StatusLineStyle {
                    preset: StatusLinePreset::Default,
                    context_line: ContextLine::Embedded,
                },
                &s,
            ))
        };
        let nine = at(9);
        let ten = at(10);
        assert!(nine.contains("9% · 128k"), "{nine:?}");
        assert!(ten.contains("10% · 128k"), "{ten:?}");
        // The groups do not move: the label grows into the rule, leftwards, and
        // the model keeps its column.
        let column = |line: &str, needle: &str| {
            let text = visible(line);
            let at = text.find(needle).expect("the needle is on the line");
            visible_width(&text[..at])
        };
        assert_eq!(
            column(&nine, "glm-5.3-flash"),
            column(&ten, "glm-5.3-flash"),
            "{nine:?}\n{ten:?}"
        );
        assert_eq!(visible_width(&visible(&nine)), 120);
        assert_eq!(visible_width(&visible(&ten)), 120);
    }

    #[test]
    fn the_gauge_draws_zero_and_hundred_honestly() {
        let theme = theme();
        let mut s = snap();
        s.context_window = Some(128_000);
        let mut line = |pct: u8| {
            s.context_pct = Some(pct);
            render_status_line(
                &theme,
                120,
                StatusLineStyle {
                    preset: StatusLinePreset::Default,
                    context_line: ContextLine::Percentage,
                },
                &s,
            )
        };
        let open = |color: ThemeColor| {
            theme
                .fg(color, "\u{1}")
                .split('\u{1}')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let rule = theme.symbol("boxRound.horizontal").to_owned();
        let lit =
            |line: &str, color: ThemeColor| line.matches(&format!("{}{rule}", open(color))).count();
        let none = line(0);
        let all = line(100);
        assert_eq!(
            lit(&none, ThemeColor::BorderAccent),
            0,
            "0% lights nothing: {none:?}"
        );
        assert!(lit(&none, ThemeColor::Border) > 0, "{none:?}");
        assert_eq!(
            lit(&all, ThemeColor::Border),
            0,
            "100% lights every cell: {all:?}"
        );
        assert!(lit(&all, ThemeColor::BorderAccent) > 0, "{all:?}");
        assert_ne!(none, all);
    }

    #[test]
    fn a_one_cell_pane_neither_panics_nor_overflows() {
        let theme = theme();
        let mut s = snap();
        s.context_pct = Some(72);
        s.context_window = Some(128_000);
        for width in [0u16, 1, 2, 3, 8] {
            for context_line in [
                ContextLine::Off,
                ContextLine::Percentage,
                ContextLine::Embedded,
            ] {
                for preset in StatusLinePreset::IDS {
                    let preset = StatusLinePreset::from_id(preset).expect("a known name");
                    let line = render_status_line(
                        &theme,
                        width,
                        StatusLineStyle {
                            preset,
                            context_line,
                        },
                        &s,
                    );
                    assert!(
                        visible_width(&line) <= width as usize,
                        "{width} {preset:?} {context_line:?}: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_narrow_pane_sheds_a_segment_before_it_loses_the_label() {
        let theme = theme();
        let mut s = snap();
        s.context_pct = Some(49);
        s.context_window = Some(400);
        let style = StatusLineStyle {
            preset: StatusLinePreset::Ascii,
            context_line: ContextLine::Embedded,
        };
        let line = visible(&render_status_line(&theme, 80, style, &s));
        assert!(line.contains("49% - 400"), "the number survives: {line:?}");
        assert!(line.is_ascii(), "{line:?}");
        assert!(
            !line.contains("main"),
            "the git state went instead of the number: {line:?}"
        );
        // A pane too narrow for the label at all gives the reservation up and
        // still draws the line.
        for width in [16u16, 24, 30] {
            let tiny = render_status_line(&theme, width, style, &s);
            assert!(visible_width(&tiny) <= width as usize, "{width}: {tiny:?}");
        }
    }

    #[test]
    fn an_unknown_name_falls_back_rather_than_failing() {
        let style = StatusLineStyle::resolve(Some("nope"), Some("nope"));
        assert_eq!(style, StatusLineStyle::default());
        assert_eq!(style.preset, StatusLinePreset::Default);
        assert_eq!(style.context_line, ContextLine::Off);
        assert_eq!(
            StatusLineStyle::resolve(None, None),
            StatusLineStyle::default()
        );
        assert_eq!(
            StatusLineStyle::resolve(Some("minimal"), Some("embedded")).context_line,
            ContextLine::Embedded
        );
    }

    #[test]
    fn the_reader_keeps_the_model_as_named_and_the_painter_shortens_it() {
        assert_eq!(short_model("opencode-go/glm-5.3-flash"), "glm-5.3-flash");
        let live = live_snapshot("opencode-go/glm-5.3-flash", "titi");
        assert_eq!(live.model, "opencode-go/glm-5.3-flash");
        // Room for the whole id: the provider prefix is shown.
        let wide = visible(&render_status_line(
            &theme(),
            120,
            style(StatusLinePreset::Default),
            &live,
        ));
        assert!(wide.contains("opencode-go/glm-5.3-flash"), "{wide:?}");
    }

    #[test]
    fn a_narrow_pane_shortens_the_model_before_cutting_it() {
        let theme = theme();
        let mut s = snap();
        s.model = "opencode-go/glm-5.3-flash".to_owned();
        // Narrow enough that the prefix has to go, but the bare id still fits.
        let line = visible(&render_status_line(
            &theme,
            40,
            style(StatusLinePreset::Default),
            &s,
        ));
        assert!(line.contains("glm-5.3-flash"), "{line:?}");
        assert!(!line.contains("opencode-go"), "{line:?}");
        // Narrower still: the id is cut, with an ellipsis.
        let cut = visible(&render_status_line(
            &theme,
            24,
            style(StatusLinePreset::Default),
            &s,
        ));
        assert!(cut.contains('…'), "{cut:?}");
    }

    #[test]
    fn porcelain_counts_unstaged_staged_untracked() {
        let src = " M a.rs\nM  b.rs\nMM c.rs\n?? d.rs\n!! ignored\n";
        assert_eq!(parse_porcelain(src), (2, 2, 1));
    }
}
