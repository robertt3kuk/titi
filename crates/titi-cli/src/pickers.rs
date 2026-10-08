//! The pickers: the panels that open above the composer.
//!
//! Every one of them is the same shape — a panel with a query, a selection, an
//! accept and an Esc — so they share one view ([`PanelView`]), one box
//! ([`panel_box`]), one window ([`panel_window`]) and one bar ([`panel_bar`]):
//! the model browser, the theme picker, the login picker, the prompt-history
//! browser, the session search, the emoji picker and the `/` command list.
//!
//! A picker owns no key of its own beyond the ones its own idiom claims: the
//! composer's routing hands it the key while it is open and takes it back the
//! moment it closes.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use titi_tui::theme::{Theme, ThemeBg, ThemeColor};

use crate::chat::*;
use crate::composer::one_line;
use crate::login::OAuthProvider;
use crate::transcript::*;
use titi_engine::EngineCommand;

/// Fewest and most lines the picker above the composer takes. The floor keeps
/// a window on a short screen from showing a single row with two `… more`
/// lines around it; the ceiling keeps the conversation on screen, however
/// long the catalog is.
pub(crate) const PICKER_MIN_ROWS: usize = 3;
pub(crate) const PICKER_MAX_ROWS: usize = 14;

pub(crate) struct Command {
    pub(crate) name: &'static str,
    pub(crate) about: &'static str,
}

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        name: "checkpoint",
        about: "record a rewind point",
    },
    Command {
        name: "checkpoints",
        about: "list rewind points",
    },
    Command {
        name: "compact",
        about: "fold the history now, optionally around a focus",
    },
    Command {
        name: "details",
        about: "show or set a transcript section (thinking, tools, subagents, activity, folded)",
    },
    Command {
        name: "context",
        about: "what fills the context window",
    },
    Command {
        name: "goal",
        about: "run coder and reviewer until the goal passes",
    },
    Command {
        name: "help",
        about: "list these commands",
    },
    Command {
        name: "hotkeys",
        about: "list the keys the screen answers",
    },
    Command {
        name: "keys",
        about: "which providers have a key or a sign-in",
    },
    Command {
        name: "theme",
        about: "choose a palette (bare opens the picker)",
    },
    Command {
        name: "usage",
        about: "show token usage",
    },
    Command {
        name: "login",
        about: "sign in to a provider, or store a key",
    },
    Command {
        name: "logout",
        about: "forget a stored key",
    },
    Command {
        name: "model",
        about: "switch model",
    },
    Command {
        name: "pause",
        about: "hold input and stop the turn",
    },
    Command {
        name: "memory",
        about: "list, search, or forget memories",
    },
    Command {
        name: "genome",
        about: "manage the prompt map: on, off, or limit",
    },
    Command {
        name: "advisor",
        about: "a toolless second opinion on this conversation",
    },
    Command {
        name: "loop",
        about: "repeat a prompt in the background (usage: /loop 5m <prompt>)",
    },
    Command {
        name: "jobs",
        about: "list background loops, or /jobs cancel <id>",
    },
    Command {
        name: "sessions",
        about: "list stored sessions, or search them (usage: /sessions <query>)",
    },
    Command {
        name: "tree",
        about: "navigate the session tree, switching branches",
    },
    Command {
        name: "recap",
        about: "what this session did",
    },
    Command {
        name: "rewind",
        about: "cut back to a rewind point",
    },
    Command {
        name: "fork",
        about: "fork current session into a new one",
    },
    Command {
        name: "export",
        about: "export session (usage: /export [path])",
    },
    Command {
        name: "btw",
        about: "send a prompt without recording it in history",
    },
    Command {
        name: "settings",
        about: "show configuration settings",
    },
    Command {
        name: "switch",
        about: "switch model with fuzzy search or role",
    },
    Command {
        name: "budget",
        about: "cap the tokens this session may spend (usage: /budget 200k|off)",
    },
    Command {
        name: "duck",
        about: "duck mode: talk it through, repo-blind and toolless",
    },
    Command {
        name: "hub",
        about: "show or hide the hub roster",
    },
    Command {
        name: "join",
        about: "join the local hub (usage: /join [name])",
    },
    Command {
        name: "leave",
        about: "leave the local hub",
    },
    Command {
        name: "plan",
        about: "plan mode: read the repo, change nothing",
    },
    Command {
        name: "done",
        about: "leave plan or duck mode and act again",
    },
    Command {
        name: "whoami",
        about: "show your signed-in providers (alias of /keys)",
    },
    Command {
        name: "council",
        about: "put a question to a council of briefs",
    },
    Command {
        name: "graph",
        about: "run the orchestrator graph: council decides, goal loop works",
    },
    Command {
        name: "git",
        about: "show git status or diff, read-only",
    },
    Command {
        name: "diagnose",
        about: "a diagnostics block to paste into a bug report",
    },
    Command {
        name: "statusline",
        about: "choose the status line preset (default, minimal, compact, full, ascii)",
    },
    Command {
        name: "mouse",
        about: "mouse reporting: off, wheel, buttons, all (drag selects, release copies)",
    },
    Command {
        name: "exit",
        about: "leave titi (bare exit, quit or q leave too)",
    },
    Command {
        name: "quit",
        about: "leave titi, the same as /exit",
    },
];

/// One offer in the `/` picker.
pub(crate) enum PickRow {
    Command(&'static Command),
    Skill(usize),
}

/// One row of the bare-`/login` picker: a subscription provider and how to
/// sign in to it.
#[derive(Debug, Clone, Copy)]
struct LoginChoice {
    provider: &'static OAuthProvider,
    method: LoginMethod,
}

/// What the bare-`/login` picker offers, in presentation order. A provider
/// that can sign in both ways gets a row each, so the method is a choice the
/// user makes rather than one the flow assumes.
fn login_choices() -> Vec<LoginChoice> {
    let mut rows = Vec::new();
    for provider in crate::login::providers() {
        rows.push(LoginChoice {
            provider,
            method: LoginMethod::Browser,
        });
        if provider.supports_device {
            rows.push(LoginChoice {
                provider,
                method: LoginMethod::Device,
            });
        }
    }
    rows
}

/// One login row as it reads: the descriptor's display name and the method.
fn login_choice_label(choice: LoginChoice) -> String {
    let method = match choice.method {
        LoginMethod::Browser => "browser",
        LoginMethod::Device => "device code",
    };
    format!("{}  ·{method}", choice.provider.name)
}

/// One catalog model as the picker states it: the id `/switch` takes, the
/// provider that runs it, the window its descriptor declares, and the
/// credential that provider holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRow {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) context_window: Option<u64>,
    pub(crate) credential: Option<String>,
}

/// One offer in the model picker: a configured role, or a catalog model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelOffer {
    /// A `modelRoles` name and the model it resolves to right now. This is
    /// `/switch @name`, spelled out.
    Role {
        name: String,
        model: String,
    },
    Model(ModelRow),
}

impl ModelOffer {
    /// What a query is matched against: `@role` and the model it means, or
    /// the model id, which reads `provider/model`.
    fn haystack(&self) -> String {
        match self {
            Self::Role { name, model } => format!("@{name} {model}"),
            Self::Model(row) => row.id.clone(),
        }
    }

    /// The section a row belongs to: roles share one, models group by the
    /// provider that runs them.
    fn group(&self) -> &str {
        match self {
            Self::Role { .. } => "roles",
            Self::Model(row) => row.provider.as_str(),
        }
    }

    /// The model the picker switches to.
    pub(crate) fn target(&self) -> &str {
        match self {
            Self::Role { model, .. } => model,
            Self::Model(row) => &row.id,
        }
    }
}

/// The model browser: bare `/model` and bare `/switch` open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPicker {
    /// Roles first, then the catalog in catalog order — the order the screen
    /// falls back to when nothing ranks above anything else.
    pub(crate) offers: Vec<ModelOffer>,
    /// The typed filter. Matched as a subsequence against `provider/model`,
    /// so `cdx` finds `openai-codex/…`; a query starting with `@` means the
    /// roles and nothing else.
    pub(crate) query: String,
    /// The selection, as an index into [`ModelPicker::matched`].
    pub(crate) selected: usize,
}

impl ModelPicker {
    /// The offers the query keeps, best match first. Ties keep catalog
    /// order, so rows never swap under the cursor while a query grows.
    pub(crate) fn matched(&self) -> Vec<usize> {
        let roles_only = self.query.starts_with('@');
        let needle = self.query.trim_start_matches('@');
        let mut scored: Vec<(i32, usize)> = self
            .offers
            .iter()
            .enumerate()
            .filter(|(_, offer)| !roles_only || matches!(offer, ModelOffer::Role { .. }))
            .filter_map(|(at, offer)| {
                fuzzy_score(needle, &offer.haystack()).map(|score| (score, at))
            })
            .collect();
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored.into_iter().map(|(_, at)| at).collect()
    }

    /// The matched offers as sections: one per provider, roles as their own.
    /// A section sits where its best match does, so searching `codex` puts
    /// the whole `openai-codex` group at the top instead of scattering its
    /// rows between the providers above it.
    fn sections(&self) -> Vec<(String, Vec<usize>)> {
        let mut order: Vec<String> = Vec::new();
        let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
        for at in self.matched() {
            let group = self.offers[at].group().to_owned();
            if !buckets.contains_key(&group) {
                order.push(group.clone());
            }
            buckets.entry(group).or_default().push(at);
        }
        order
            .into_iter()
            .filter_map(|group| buckets.remove(&group).map(|offers| (group, offers)))
            .collect()
    }
}

/// The catalog as picker rows: every id the catalog offers, with the
/// provider, declared window and credential behind it.
///
/// The declared facts come from the same registry config the engine builds
/// its catalog from, so a model that declares a window in settings is one
/// the picker states. A model a local server discovered declares nothing:
/// its provider is the id's own prefix and its window is unknown.
pub(crate) fn model_rows(chat: &Chat) -> Vec<ModelRow> {
    let config = crate::engine::registry_config_for(
        &chat.agent_dir,
        &crate::session_fs::current_workspace(),
    );
    let stored = crate::secrets::list_keys(&chat.agent_dir).unwrap_or_default();
    let credentials: Vec<(String, String)> = config
        .providers
        .iter()
        .filter_map(|provider| {
            Credential::of(provider, &stored)
                .label()
                .map(|label| (provider.id.to_string(), label.to_owned()))
        })
        .collect();
    chat.catalog
        .ids()
        .into_iter()
        .map(|id| {
            let declared = config.models.iter().find(|model| model.id == id.as_str());
            let provider = declared
                .map(|model| model.provider.to_string())
                .unwrap_or_else(|| id.split('/').next().unwrap_or_default().to_owned());
            ModelRow {
                credential: credentials
                    .iter()
                    .find(|(name, _)| name == &provider)
                    .map(|(_, label)| label.clone()),
                context_window: declared.and_then(|model| model.context_window),
                id,
                provider,
            }
        })
        .collect()
}

/// The `modelRoles` the settings declare, each with the model it resolves to
/// now — the same resolution `/switch @role` uses.
fn picker_roles(chat: &Chat) -> Vec<(String, String)> {
    let Ok(settings) = titi_config::settings::Settings::load(
        &chat.agent_dir,
        &crate::session_fs::current_workspace(),
        &[],
    ) else {
        return Vec::new();
    };
    let Some(roles) = settings
        .get("modelRoles")
        .and_then(|value| value.as_object().cloned())
    else {
        return Vec::new();
    };
    let mut rows: Vec<(String, String)> = roles
        .keys()
        .filter_map(|name| {
            let model =
                titi_config::roles::resolve_model_role(&settings, name, &chat.model).ok()?;
            (!model.trim().is_empty()).then(|| (name.clone(), model))
        })
        .collect();
    rows.sort();
    rows
}

/// Where a character may start a word: the start of the string, or the far
/// side of a separator. A hit there is a name being spelled, not letters
/// that happen to sit in the same order.
fn is_word_start(hay: &[char], at: usize) -> bool {
    at == 0 || matches!(hay[at - 1], '/' | '-' | '.' | '_' | ' ' | '@')
}

/// How well `query` matches `haystack`: `None` when the query is not a
/// subsequence of it at all, otherwise a score that puts a provider prefix
/// (`codex` → `openai-codex/…`) and a word start above a loose scattering of
/// the same letters.
pub(crate) fn fuzzy_score(query: &str, haystack: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let needle: Vec<char> = query.to_lowercase().chars().collect();
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut cursor = 0usize;
    let mut previous: Option<usize> = None;
    for ch in needle {
        let found = cursor + hay.get(cursor..)?.iter().position(|cell| *cell == ch)?;
        score += 1;
        if is_word_start(&hay, found) {
            score += 3;
        }
        if previous == Some(found.saturating_sub(1)) && found > 0 {
            score += 2;
        }
        previous = Some(found);
        cursor = found + 1;
    }
    // The query spelled out in order, not one letter per word: `codex` is
    // the provider's name, and that is what the user meant.
    if haystack.to_lowercase().contains(&query.to_lowercase()) {
        score += 6;
    }
    Some(score)
}

/// A context window in the fewest cells that stay exact: `272k`, `1M`.
fn context_label(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// A row's label, cut with an ellipsis when even its mandatory part does not
/// fit, so a narrow screen shows that something was dropped rather than
/// quietly printing half a model id.
pub(crate) fn ellipsis_label(text: &str, room: usize) -> String {
    if titi_tui::width::visible_width(text) <= room {
        return text.to_owned();
    }
    format!(
        "{}…",
        titi_tui::width::truncate_to_width(text, room.saturating_sub(1))
    )
}

/// One model row: the id, the window the model declares, the credential its
/// provider holds, and the mark for the model in use.
///
/// Parts leave from the least important as the terminal narrows: the provider
/// first (it is the id's own prefix and the heading of the group), then the
/// window. The credential and the mark stay, and an id that no longer fits is
/// cut with an ellipsis — never silently, and never into a string that reads
/// like a whole model id.
pub(crate) fn model_row_label(row: &ModelRow, current: bool, room: usize) -> String {
    let credential = match &row.credential {
        Some(label) => format!("  {label}"),
        None => String::new(),
    };
    let window = match row.context_window {
        Some(tokens) => format!("  {}", context_label(tokens)),
        None => String::new(),
    };
    let provider = format!("  ·{}", row.provider);
    let marker = if current { "  ✓ current" } else { "" };
    for section in [format!("{provider}{window}"), window.clone(), String::new()] {
        let label = format!("{}{section}{credential}{marker}", row.id);
        if titi_tui::width::visible_width(&label) <= room {
            return label;
        }
    }
    let tail = format!("{credential}{marker}");
    let head = titi_tui::width::truncate_to_width(
        &row.id,
        room.saturating_sub(titi_tui::width::visible_width(&tail) + 1),
    );
    format!("{head}…{tail}")
}

/// Commands first, then skills. A command only counts at the start of the
/// line, so a slash inside a sentence can only name a skill.
pub(crate) fn picker_rows(chat: &Chat) -> Vec<PickRow> {
    if chat.login_for.is_some() || chat.picker_hidden {
        return Vec::new();
    }
    let Some((start, prefix)) = slash_token(&chat.input) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    if chat.input[..start].trim().is_empty() {
        rows.extend(
            COMMANDS
                .iter()
                .filter(|command| command.name.starts_with(prefix))
                .map(PickRow::Command),
        );
    }
    rows.extend(
        chat.skills
            .iter()
            .enumerate()
            .filter(|(_, skill)| skill.name.starts_with(prefix))
            .map(|(index, _)| PickRow::Skill(index)),
    );
    rows
}

/// The most lines the picker above the composer may take: what is left of
/// the screen after the masthead, one line of conversation and the composer,
/// capped so a tall window never lets a picker eat the session it sits in.
fn panel_body(total: u16) -> usize {
    // Two of the room belongs to the box's rules, so the panel on screen is the
    // same height it was before it had a frame.
    ((total as usize)
        .saturating_sub(6)
        .clamp(PICKER_MIN_ROWS, PICKER_MAX_ROWS))
    .saturating_sub(2)
    .max(1)
}

/// Cells a row label may use: the panel has `room` and spends three of it on
/// the cursor and the spaces around it.
fn panel_label_room(width: u16) -> usize {
    (width as usize).saturating_sub(2).max(8).saturating_sub(3)
}

/// What the screen is showing: the palette's name and, when the user chose it
/// for this appearance slot, which choice it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThemeState {
    pub(crate) name: String,
    pub(crate) chosen: Option<String>,
}

/// One line above the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanelLine {
    /// A section heading. Not selectable: it names the group below it.
    Heading(String),
    /// A selectable offer. `accent` is the green the repo gives a skill;
    /// the model picker marks roles with it too.
    Row { text: String, accent: bool },
}

/// The slice of a picker's lines that is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PanelWindow {
    pub(crate) start: usize,
    pub(crate) count: usize,
    /// Lines the window hides above and below itself.
    pub(crate) above: usize,
    pub(crate) below: usize,
}

/// The picker above the composer, windowed and sized by the same code that
/// draws it, so the space the layout reserves and the lines that land in it
/// cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelView {
    /// The line above the rows, when the picker has something to say: the
    /// model picker puts the query and the counts there.
    pub(crate) title: Option<String>,
    pub(crate) lines: Vec<PanelLine>,
    /// The line the cursor is on; `None` when nothing is selectable.
    pub(crate) selected: Option<usize>,
    pub(crate) window: PanelWindow,
}

impl PanelView {
    /// Rows this takes: the box's two rules, the window's rows and the
    /// `… N more` rows. The title costs nothing here — it is inset in the top
    /// rule — which is why the body is asked for two rows fewer than the box
    /// is tall.
    pub(crate) fn height(&self) -> u16 {
        (2 + self.window.count
            + usize::from(self.window.above > 0)
            + usize::from(self.window.below > 0)) as u16
    }
}

/// The window a panel shows: the selected line kept in view, at most `room`
/// lines, and how many lines hide on each side. A `… N more` line is paid for
/// out of the same room, and only when there is something for it to hide.
pub(crate) fn panel_window(len: usize, selected: usize, room: usize) -> PanelWindow {
    let asked = room.max(1);
    if len <= asked {
        return PanelWindow {
            start: 0,
            count: len,
            above: 0,
            below: 0,
        };
    }
    // Each marker line costs a row, and giving one back can move a row past
    // the end — which is a marker appearing or going away in turn — so the
    // window is settled by shrinking until what it shows fits in `asked`.
    let mut room = asked;
    loop {
        let window = panel_slice(len, selected, room);
        let lines = room + usize::from(window.above > 0) + usize::from(window.below > 0);
        if lines <= asked || room == 1 {
            return window;
        }
        room -= 1;
    }
}

/// One window of `room` rows around `selected`, before the `… N more` lines
/// are paid for: the selection centred, then pulled back so the window never
/// runs past either end.
fn panel_slice(len: usize, selected: usize, room: usize) -> PanelWindow {
    let room = room.max(1);
    let start = selected
        .min(len - 1)
        .saturating_sub(room / 2)
        .min(len.saturating_sub(room));
    PanelWindow {
        start,
        count: room.min(len),
        above: start,
        below: len.saturating_sub(start + room),
    }
}

/// A panel from its lines: the window around the selected line.
fn panel_view(
    title: Option<String>,
    lines: Vec<PanelLine>,
    selected: Option<usize>,
    body: usize,
) -> PanelView {
    let body = body.saturating_sub(usize::from(title.is_some())).max(1);
    let window = panel_window(lines.len(), selected.unwrap_or(0), body);
    PanelView {
        title,
        lines,
        selected,
        window,
    }
}

/// One entry of the session tree, as `/tree` offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeRow {
    /// The entry this row stands for: what Enter moves the leaf to.
    pub(crate) entry_id: String,
    /// The row as the panel draws it: indented by depth, the path to the leaf
    /// marked, the leaf named.
    pub(crate) text: String,
}

/// The `/tree` picker: one session's stored entries as the tree they are.
///
/// The store is append-only, so every branch ever taken is still in it — this
/// is the screen that shows them. The rows are built once, when `/tree` runs:
/// the picker cannot see an append while it is open, because nothing appends
/// while a panel holds the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreePicker {
    pub(crate) rows: Vec<TreeRow>,
    /// The row the cursor is on, an index into [`TreePicker::rows`].
    pub(crate) selected: usize,
    /// Entries the path to the leaf does not hold, for the title: what a move
    /// away from where the screen is would put behind it.
    pub(crate) off_path: usize,
}

impl TreePicker {
    /// Reads one session's entries and lays them out as a tree, the leaf's own
    /// path marked and the cursor on the leaf.
    pub(crate) fn open(agent_dir: &std::path::Path, session_id: &str) -> Result<Self, String> {
        let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
        let entries = store.open(session_id).map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Err("this session has no entries yet".to_owned());
        }
        let path = store.walk(session_id, None).map_err(|e| e.to_string())?;
        let leaf = path.last().map(|entry| entry.id.clone());
        let on_path: std::collections::HashSet<&str> =
            path.iter().map(|entry| entry.id.as_str()).collect();

        // parent → its children, in the order they were appended; an entry
        // whose parent is not in the log is a root (a torn write, or a branch
        // whose head was pruned).
        let mut children: std::collections::HashMap<&str, Vec<usize>> =
            std::collections::HashMap::new();
        let index: std::collections::HashMap<&str, usize> = entries
            .iter()
            .enumerate()
            .map(|(at, entry)| (entry.id.as_str(), at))
            .collect();
        let mut roots: Vec<usize> = Vec::new();
        for (at, entry) in entries.iter().enumerate() {
            match entry.parent_id.as_deref().and_then(|id| index.get(id)) {
                Some(parent) => children
                    .entry(entries[*parent].id.as_str())
                    .or_default()
                    .push(at),
                None => roots.push(at),
            }
        }

        let mut rows = Vec::new();
        let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|at| (*at, 0)).collect();
        while let Some((at, depth)) = stack.pop() {
            let entry = &entries[at];
            let mark = if on_path.contains(entry.id.as_str()) {
                "• "
            } else {
                "  "
            };
            let current = if leaf.as_deref() == Some(entry.id.as_str()) {
                "  ✓ current"
            } else {
                ""
            };
            rows.push(TreeRow {
                entry_id: entry.id.clone(),
                text: format!(
                    "{indent}{mark}{role}  {label}{current}",
                    indent = "  ".repeat(depth),
                    role = tree_role(entry.role),
                    label = one_line(entry.content.trim(), 60),
                ),
            });
            if let Some(kids) = children.get(entry.id.as_str()) {
                for kid in kids.iter().rev() {
                    stack.push((*kid, depth + 1));
                }
            }
        }
        let selected = rows
            .iter()
            .position(|row| leaf.as_deref() == Some(row.entry_id.as_str()))
            .unwrap_or(rows.len() - 1);
        let off_path = entries.len().saturating_sub(on_path.len());
        Ok(Self {
            rows,
            selected,
            off_path,
        })
    }

    /// The entry the cursor is on.
    pub(crate) fn selected_id(&self) -> Option<&str> {
        self.rows
            .get(self.selected)
            .map(|row| row.entry_id.as_str())
    }
}

/// How a tree row names the writer of an entry, in the transcript's own words.
fn tree_role(role: titi_core::session::Role) -> &'static str {
    match role {
        titi_core::session::Role::User => "you ",
        titi_core::session::Role::Assistant => "titi",
        titi_core::session::Role::Tool => "tool",
        titi_core::session::Role::System => "sys ",
    }
}

/// The `/tree` panel: the session's entries, one row each, the leaf marked.
fn tree_panel(chat: &Chat, total: u16) -> PanelView {
    let Some(picker) = chat.tree_picker.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let lines: Vec<PanelLine> = picker
        .rows
        .iter()
        .map(|row| PanelLine::Row {
            text: row.text.clone(),
            accent: false,
        })
        .collect();
    let title = format!(
        "tree · {} entries · {} off this path",
        picker.rows.len(),
        picker.off_path
    );
    panel_view(Some(title), lines, Some(picker.selected), panel_body(total))
}

/// The picker above the composer for the state on screen: the login picker,
/// the model browser, or the slash/skill list.
pub(crate) fn panel_view_for(chat: &Chat, total: u16, width: u16) -> Option<PanelView> {
    if chat.theme_picker.is_some() {
        return Some(theme_panel(chat, total));
    }
    if chat.session_picker.is_some() {
        return Some(session_panel(chat, total));
    }
    if chat.tree_picker.is_some() {
        return Some(tree_panel(chat, total));
    }
    if chat.session_search.is_some() {
        return Some(session_search_panel(chat, total));
    }
    if chat.login_picker.is_some() {
        return Some(login_panel(chat, total));
    }
    if chat.model_picker.is_some() {
        return Some(model_panel(chat, total, width));
    }
    if chat.emoji_picker.is_visible() {
        return Some(emoji_panel(chat, total));
    }
    if chat.history_picker.is_some() {
        return Some(history_panel(chat, total));
    }
    if picker_rows(chat).is_empty() {
        return None;
    }
    Some(command_panel(chat, total))
}

/// A session's row, named the same way wherever a session is offered: its id,
/// and the mark that says the screen is already on it.
pub(crate) fn session_row_text(id: &str, current: bool) -> String {
    if current {
        format!("{id}  ✓ current")
    } else {
        id.to_owned()
    }
}

/// The `/theme` picker: the palettes this build carries, `auto` first, the one
/// the screen is on marked where the model browser marks the model in use.
fn theme_panel(chat: &Chat, total: u16) -> PanelView {
    let Some(picker) = chat.theme_picker.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let state = chat.theme_state();
    let matched = picker.matched();
    let lines: Vec<PanelLine> = matched
        .iter()
        .map(|at| {
            let name = picker.names[*at].as_str();
            let current = if state.chosen.is_some() {
                state.chosen.as_deref() == Some(name)
            } else {
                name == THEME_AUTO
            };
            PanelLine::Row {
                text: if current {
                    format!("{name}  ✓ current")
                } else {
                    name.to_owned()
                },
                accent: false,
            }
        })
        .collect();
    let title = if picker.query.is_empty() {
        format!("themes · {}", lines.len().min(picker.names.len()))
    } else {
        format!(
            "themes · {} of {} · {}",
            lines.len(),
            picker.names.len(),
            picker.query
        )
    };
    panel_view(
        Some(title),
        lines,
        Some(picker.selected % matched.len().max(1)),
        panel_body(total),
    )
}

/// One stored session a search matched: the session, when it was last written,
/// and the entry text that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionHit {
    session_id: String,
    /// The session's own name when the index holds a real one, its id when it
    /// has none: a row is a way to say which session, so it says the name a
    /// person would use.
    label: String,
    /// When the session's file was last written, seconds since the epoch.
    /// `None` when the file cannot be stat'ed — a hit whose session was
    /// deleted between the query and the row.
    at: Option<u64>,
    /// The matching entry, flattened to the one line a picker row holds.
    line: String,
}

/// The `/sessions <query>` browser: the index's hits for the query typed so
/// far, in the order the index ranked them.
///
/// The query lives here and not in the composer — the same shape the model,
/// theme and history browsers have — so `/sessions` is one command with two
/// faces: bare lists the stored sessions, and a query searches them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionSearch {
    pub(crate) query: String,
    pub(crate) hits: Vec<SessionHit>,
    pub(crate) selected: usize,
    /// Why the search returned nothing, when it was not "nothing matched": an
    /// unreadable index is a different sentence from no hits.
    pub(crate) broken: Option<String>,
}

/// How many matching lines one search shows. A common word can match every
/// entry of every session, and a picker that becomes the whole screen is not a
/// picker.
const SESSION_HITS_MAX: usize = 40;

impl SessionSearch {
    /// The hits for `query` over the agent directory's index. An index that
    /// cannot be read is carried as [`SessionSearch::broken`] rather than
    /// failing: either way the panel is what the user sees, and one says why
    /// it is empty.
    pub(crate) fn open(agent_dir: &Path, query: &str) -> Self {
        let mut search = Self {
            query: query.to_owned(),
            hits: Vec::new(),
            selected: 0,
            broken: None,
        };
        search.retype(agent_dir, query.to_owned());
        search
    }

    /// The query as it is typed: the hits are recomputed, and the cursor goes
    /// back to the top row because the list under it is a different list.
    fn retype(&mut self, agent_dir: &Path, query: String) {
        self.query = query;
        self.selected = 0;
        match search_sessions(agent_dir, &self.query) {
            Ok(hits) => {
                self.hits = hits;
                self.broken = None;
            }
            Err(reason) => {
                self.hits.clear();
                self.broken = Some(reason);
            }
        }
    }

    fn selected_hit(&self) -> Option<&SessionHit> {
        self.hits.get(self.selected % self.hits.len().max(1))
    }
}

/// The index's hits for `query`: one row per matching entry, capped at
/// [`SESSION_HITS_MAX`].
///
/// `SessionStore::search` was the capability with no caller — the index is
/// built and populated on every append, and nothing in the CLI read it. This
/// is that caller. The session's own time comes from its file, which is what
/// the session list already sorts by, so a hit row and a list row cannot
/// disagree about when a session was last written.
fn search_sessions(agent_dir: &Path, query: &str) -> Result<Vec<SessionHit>, String> {
    // An empty query is not a search: it is the list every stored session,
    // which is what the panel shows before a word is typed and what Esc
    // clears back to.
    if query.is_empty() {
        return Ok(all_sessions(agent_dir));
    }
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|why| why.to_string())?;
    let index = titi_core::session::SessionIndex::open(&agent_dir.join("state.db"))
        .map_err(|why| why.to_string())?;
    // Unscoped on both dimensions, which is what the bare list is: this
    // searches every session the agent directory holds, including one written
    // before sessions recorded where they were started — a scope the row list
    // would then disagree with.
    let hits = store
        .search(query, None, None)
        .map_err(|why| why.to_string())?;
    let mut out = Vec::with_capacity(hits.len().min(SESSION_HITS_MAX));
    for hit in hits.into_iter().take(SESSION_HITS_MAX) {
        out.push(SessionHit {
            label: session_label(&index, &hit.session_id),
            at: session_written_at(agent_dir, &hit.session_id),
            line: one_line(hit.text.trim(), 60),
            session_id: hit.session_id,
        });
    }
    Ok(out)
}

/// Every stored session, newest first — the list Ctrl+X offers, with the time
/// each was last written and no matching line, because nothing matched.
fn all_sessions(agent_dir: &Path) -> Vec<SessionHit> {
    let index = titi_core::session::SessionIndex::open(&agent_dir.join("state.db")).ok();
    crate::session_fs::list_sessions_from(agent_dir)
        .into_iter()
        .map(|id| SessionHit {
            label: match &index {
                Some(index) => session_label(index, &id),
                None => id.clone(),
            },
            at: session_written_at(agent_dir, &id),
            line: String::new(),
            session_id: id,
        })
        .collect()
}

/// The name a row offers for a session: the title the index holds when someone
/// or something gave it one, and the id otherwise.
///
/// The same rule [`stored_session_title`] applies to the masthead, except that
/// a row has to say *something*: an unnamed session is offered by id.
fn session_label(index: &titi_core::session::SessionIndex, session_id: &str) -> String {
    if index.needs_auto_title(session_id).unwrap_or(true) {
        return session_id.to_owned();
    }
    match index.title(session_id) {
        Ok(Some(title)) if !title.trim().is_empty() => title,
        _ => session_id.to_owned(),
    }
}

/// When a session's file was last written, seconds since the epoch.
fn session_written_at(agent_dir: &Path, session_id: &str) -> Option<u64> {
    let path = agent_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0),
    )
}

/// How long ago a session was last written, in the one-word shape a picker row
/// can hold: `just now`, `5m ago`, `3h ago`, `4d ago`, `2w ago`.
///
/// A clock in the future (a file with a timestamp ahead of this machine) reads
/// as `just now` rather than as a negative age.
pub(crate) fn age_label(now: std::time::SystemTime, at: u64) -> String {
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH + Duration::from_secs(at))
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let (value, unit) = if seconds < 60 {
        return "just now".to_owned();
    } else if seconds < 3_600 {
        (seconds / 60, "m")
    } else if seconds < 86_400 {
        (seconds / 3_600, "h")
    } else if seconds < 604_800 {
        (seconds / 86_400, "d")
    } else {
        (seconds / 604_800, "w")
    };
    format!("{value}{unit} ago")
}

/// One hit as a row reads: which session, when it was written, and the line
/// that matched.
fn session_hit_row(hit: &SessionHit, now: std::time::SystemTime, current: bool) -> String {
    let when = match hit.at {
        Some(at) => age_label(now, at),
        None => "unwritten".to_owned(),
    };
    let mut row = format!("{} · {when}", hit.label);
    if !hit.line.is_empty() {
        row.push_str(" · ");
        row.push_str(&hit.line);
    }
    if current {
        row.push_str("  ✓ current");
    }
    row
}

/// The `/sessions <query>` browser: one row per matching line, and one row
/// that says so when nothing matched.
fn session_search_panel(chat: &Chat, total: u16) -> PanelView {
    let Some(search) = chat.session_search.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    if let Some(reason) = &search.broken {
        return panel_view(
            Some(format!("sessions · {}", search.query)),
            vec![PanelLine::Heading(format!(
                "the index could not be read: {reason}"
            ))],
            None,
            panel_body(total),
        );
    }
    let now = std::time::SystemTime::now();
    let lines: Vec<PanelLine> = search
        .hits
        .iter()
        .map(|hit| PanelLine::Row {
            text: session_hit_row(hit, now, hit.session_id == chat.session_id),
            accent: false,
        })
        .collect();
    // The empty query is the whole list, so its title is the list's: the query
    // is only worth naming once there is one.
    let title = if search.query.is_empty() {
        format!("sessions · {}", lines.len())
    } else {
        format!("sessions · {} · {}", lines.len(), search.query)
    };
    if lines.is_empty() {
        return panel_view(
            Some(title),
            vec![PanelLine::Heading(format!(
                "no sessions match \"{}\"",
                search.query
            ))],
            None,
            panel_body(total),
        );
    }
    panel_view(
        Some(title),
        lines,
        Some(search.selected % search.hits.len()),
        panel_body(total),
    )
}

/// The Ctrl+X switcher: one row per stored session, newest first, the session
/// on screen marked where the model browser marks the model in use.
fn session_panel(chat: &Chat, total: u16) -> PanelView {
    let lines: Vec<PanelLine> = chat
        .session_choices()
        .into_iter()
        .map(|id| PanelLine::Row {
            text: session_row_text(&id, id == chat.session_id),
            accent: false,
        })
        .collect();
    let title = format!("sessions · {}", lines.len());
    panel_view(Some(title), lines, chat.session_picker, panel_body(total))
}

/// The row that stands for the probe's own pick rather than a palette: it is
/// what a user who never chose a theme has, and what `/theme auto` goes back to.
pub(crate) const THEME_AUTO: &str = "auto";

/// The `/theme` picker: every palette this build carries — the crate's registry
/// plus `{agent_dir}/themes` — with `auto` first for the probe's own pick.
///
/// The query filters the way the model browser's does, so a name can be typed
/// down to one row without knowing where it is in a list of a hundred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThemePicker {
    pub(crate) names: Vec<String>,
    pub(crate) query: String,
    pub(crate) selected: usize,
}

impl ThemePicker {
    pub(crate) fn open() -> Self {
        let mut names = vec![THEME_AUTO.to_owned()];
        names.extend(titi_tui::theme::loader::get_available_themes());
        Self {
            names,
            query: String::new(),
            selected: 0,
        }
    }

    /// The rows the query keeps, best score first and the list's own order
    /// breaking ties, so a list that is re-filtered never jumps between frames.
    pub(crate) fn matched(&self) -> Vec<usize> {
        let mut scored: Vec<(i32, usize)> = self
            .names
            .iter()
            .enumerate()
            .filter_map(|(at, name)| fuzzy_score(&self.query, name).map(|score| (score, at)))
            .collect();
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored.into_iter().map(|(_, at)| at).collect()
    }

    fn selected_name(&self) -> Option<&str> {
        let matched = self.matched();
        let at = *matched.get(self.selected % matched.len().max(1))?;
        self.names.get(at).map(String::as_str)
    }
}

/// The Ctrl+R / ↑ browser: the prompts this session has carried, newest first,
/// filtered the way the model browser filters — a prompt can be typed down to
/// one row without knowing where it is in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryPicker {
    pub(crate) entries: Vec<String>,
    pub(crate) query: String,
    pub(crate) selected: usize,
}

impl HistoryPicker {
    pub(crate) fn open(entries: Vec<String>) -> Self {
        Self {
            entries,
            query: String::new(),
            selected: 0,
        }
    }

    /// The rows the query keeps, best score first and the list's own order
    /// breaking ties, so a list that is re-filtered never jumps between frames.
    pub(crate) fn matched(&self) -> Vec<usize> {
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(at, text)| fuzzy_score(&self.query, text).map(|score| (score, at)))
            .collect();
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored.into_iter().map(|(_, at)| at).collect()
    }

    fn selected_text(&self) -> Option<&str> {
        let matched = self.matched();
        let at = *matched.get(self.selected % matched.len().max(1))?;
        self.entries.get(at).map(String::as_str)
    }
}

/// The history browser's rows: one line per prompt, flattened so a prompt that
/// was typed over several lines is still one row.
fn history_panel(chat: &Chat, total: u16) -> PanelView {
    let Some(picker) = chat.history_picker.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let matched = picker.matched();
    let lines: Vec<PanelLine> = matched
        .iter()
        .map(|at| PanelLine::Row {
            text: one_line(picker.entries[*at].trim(), 72),
            accent: false,
        })
        .collect();
    let title = if picker.query.is_empty() {
        format!("history · {}", picker.entries.len())
    } else {
        format!(
            "history · {} of {} · {}",
            lines.len(),
            picker.entries.len(),
            picker.query
        )
    };
    panel_view(
        Some(title),
        lines,
        Some(picker.selected % matched.len().max(1)),
        panel_body(total),
    )
}

/// The bare-`/login` picker: a provider and a method per row. Nothing is
/// typed into it, so it has no title.
fn login_panel(chat: &Chat, total: u16) -> PanelView {
    let lines = login_choices()
        .into_iter()
        .map(|choice| PanelLine::Row {
            text: login_choice_label(choice),
            accent: false,
        })
        .collect();
    panel_view(None, lines, chat.login_picker, panel_body(total))
}

/// The emoji suggestion picker: `:query` typed at the caret, one row per
/// matching shortcode with its glyph. The rows are plain text, not the
/// picker's pre-styled `item_rows` — the live host paints its own buffer, so
/// the selection has to be a real row here.
fn emoji_panel(chat: &Chat, total: u16) -> PanelView {
    let matched: Vec<(&str, &str)> = chat.emoji_picker.matches().collect();
    let lines: Vec<PanelLine> = matched
        .iter()
        .map(|(name, glyph)| PanelLine::Row {
            text: format!("{name}  {glyph}"),
            accent: false,
        })
        .collect();
    let selected = Some(
        chat.emoji_picker
            .selected()
            .min(matched.len().saturating_sub(1)),
    );
    panel_view(
        Some(format!("emoji · {}", lines.len())),
        lines,
        selected,
        panel_body(total),
    )
}

/// The slash/skill list, in the order the arrows walk it.
fn command_panel(chat: &Chat, total: u16) -> PanelView {
    let rows = picker_rows(chat);
    let selected = if rows.is_empty() {
        0
    } else {
        chat.picker % rows.len()
    };
    let lines: Vec<PanelLine> = rows
        .iter()
        .map(|row| match row {
            PickRow::Command(command) => PanelLine::Row {
                text: format!("/{:<12} {}", command.name, command.about),
                accent: false,
            },
            PickRow::Skill(at) => PanelLine::Row {
                text: chat
                    .skills
                    .get(*at)
                    .map(|skill| format!("/{:<12} ·skill {}", skill.name, skill.about))
                    .unwrap_or_default(),
                accent: true,
            },
        })
        .collect();
    panel_view(None, lines, Some(selected), panel_body(total))
}

/// The model browser: a heading per provider group, its rows under it, and
/// the query and the counts on the title line.
fn model_panel(chat: &Chat, total: u16, width: u16) -> PanelView {
    let body = panel_body(total);
    let Some(picker) = &chat.model_picker else {
        return panel_view(None, Vec::new(), None, body);
    };
    let matched = picker.matched();
    let selected_offer = matched.get(picker.selected % matched.len().max(1)).copied();
    let title = if picker.query.is_empty() {
        format!("models · {}", picker.offers.len())
    } else {
        format!(
            "models · {} of {} · \"{}\"",
            matched.len(),
            picker.offers.len(),
            picker.query
        )
    };
    let room = panel_label_room(width);
    let mut lines: Vec<PanelLine> = Vec::new();
    let mut selected = None;
    let sections = picker.sections();
    if sections.is_empty() {
        lines.push(PanelLine::Heading(format!(
            "no model matches \"{}\"",
            picker.query
        )));
    }
    for (group, offers) in sections {
        lines.push(PanelLine::Heading(format!("▾ {group}  {}", offers.len())));
        for at in offers {
            if selected_offer == Some(at) {
                selected = Some(lines.len());
            }
            let offer = &picker.offers[at];
            lines.push(PanelLine::Row {
                text: model_offer_label(offer, &chat.model, room),
                accent: matches!(offer, ModelOffer::Role { .. }),
            });
        }
    }
    panel_view(Some(title), lines, selected, body)
}

/// One offer as a row reads: a role with the model it means, or a model with
/// its declared window and its provider's credential.
fn model_offer_label(offer: &ModelOffer, current: &str, room: usize) -> String {
    match offer {
        ModelOffer::Role { name, model } => ellipsis_label(&format!("@{name}  →  {model}"), room),
        ModelOffer::Model(row) => model_row_label(row, row.id == current, room),
    }
}

/// The panel above the composer.
///
/// `view` carries its own window, so the rows drawn are exactly the rows the
/// layout made room for. The look is the panel crate's (`titi_tui::panels`): a
/// box whose title is inset in the top rule, a `▶` on the selected row, and —
/// because the chat's pickers window a list where the crate's own panel scrolls
/// a capped one — the crate's scrollbar beside the body when the list does not
/// fit. The selected row also carries the theme's `SelectedBg`, so the
/// highlight survives a terminal where the marker alone is easy to miss.
pub(crate) fn panel_box(view: &PanelView, width: u16, theme: &Theme) -> Paragraph<'static> {
    let width = width as usize;
    let body =
        view.window.count + usize::from(view.window.above > 0) + usize::from(view.window.below > 0);
    // The bar takes the pane's last column, and only when there is something to
    // scroll to (`thumb_span` answers `None` when the list fits), so the box
    // gives that column up only when the bar is there.
    let bar = panel_bar(view, body, theme);
    let box_width = width.saturating_sub(usize::from(bar.is_some())).max(8);
    let inner = box_width.saturating_sub(2).max(4);
    let border = fg(theme, ThemeColor::Border);
    let mut bodies: Vec<Vec<Span<'static>>> = Vec::new();
    if view.window.above > 0 {
        bodies.push(hidden_spans(view.window.above, "above", inner, theme));
    }
    for (at, line) in view
        .lines
        .iter()
        .enumerate()
        .skip(view.window.start)
        .take(view.window.count)
    {
        bodies.push(panel_row(line, view.selected == Some(at), inner, theme));
    }
    if view.window.below > 0 {
        bodies.push(hidden_spans(view.window.below, "below", inner, theme));
    }

    let mut rows: Vec<Line<'static>> = Vec::with_capacity(bodies.len() + 2);
    // The rules carry no bar cell: the bar is exactly as tall as the body it
    // scrolls.
    rows.push(Line::from(Span::styled(
        titi_tui::panels::box_top_title(inner, view.title.as_deref().unwrap_or("")),
        border,
    )));
    for (at, spans) in bodies.into_iter().enumerate() {
        let mut row = spans;
        if let Some(cell) = bar.as_ref().and_then(|cells| cells.get(at)) {
            row.push(cell.clone());
        }
        rows.push(Line::from(row));
    }
    rows.push(Line::from(Span::styled(
        titi_tui::panels::box_bot(inner),
        border,
    )));
    Paragraph::new(rows).style(page(theme))
}

/// One row inside the box: the border, the cursor's column and the label, with
/// the fill out to the right border. A heading names a section rather than being
/// a choice, so its own `▾` stands where a choice has its cursor and it is never
/// selected; both put their label in the same column.
fn panel_row(line: &PanelLine, selected: bool, inner: usize, theme: &Theme) -> Vec<Span<'static>> {
    let (text, style) = match line {
        PanelLine::Heading(text) => (
            text.clone(),
            fg(theme, ThemeColor::Accent).add_modifier(Modifier::BOLD),
        ),
        PanelLine::Row { text, .. } if selected => (
            format!("▶ {text}"),
            fg(theme, ThemeColor::CustomMessageLabel).add_modifier(Modifier::BOLD),
        ),
        PanelLine::Row { text, accent: true } => {
            (format!("  {text}"), fg(theme, ThemeColor::Success))
        }
        PanelLine::Row { text, .. } => (format!("  {text}"), fg(theme, ThemeColor::Muted)),
    };
    // The selected row keeps the marker *and* carries the theme's selection
    // band: a marker is easy to miss on a terminal whose colours are dim.
    let band = if selected {
        style.bg(bg(theme, ThemeBg::SelectedBg))
    } else {
        style
    };
    // The content fills the cells between the borders: the cursor's two cells,
    // the label, and the fill up to the right border.
    let content = inner.saturating_sub(2);
    let shown = titi_tui::width::truncate_to_width(&text, content.saturating_sub(2));
    let pad = content.saturating_sub(titi_tui::width::visible_width(&shown));
    vec![
        Span::styled("│ ", fg(theme, ThemeColor::Border)),
        Span::styled(format!("{shown}{}", " ".repeat(pad)), band),
        Span::styled(" │", fg(theme, ThemeColor::Border)),
    ]
}

/// `… N more above` inside the box: the dim row the window pays for, saying how
/// much of the list is out of sight where the bar says where it is.
fn hidden_spans(count: usize, side: &str, inner: usize, theme: &Theme) -> Vec<Span<'static>> {
    let room = inner.saturating_sub(2);
    let text = titi_tui::width::truncate_to_width(&format!("  … {count} more {side}"), room);
    let pad = room.saturating_sub(titi_tui::width::visible_width(&text));
    vec![
        Span::styled("│ ", fg(theme, ThemeColor::Border)),
        Span::styled(
            format!("{text}{}", " ".repeat(pad)),
            fg(theme, ThemeColor::Dim),
        ),
        Span::styled(" │", fg(theme, ThemeColor::Border)),
    ]
}

/// The crate's scrollbar beside a panel body, or `None` when the list fits: one
/// cell per body row, the thumb tracking the window. The crate's `render` is the
/// same cells as ANSI for a host that emits text; this host paints a ratatui
/// buffer, so it takes the cells and applies the theme's colours itself.
fn panel_bar(view: &PanelView, body: usize, theme: &Theme) -> Option<Vec<Span<'static>>> {
    let total = view.lines.len();
    titi_tui::scrollbar::thumb_span(body, view.window.start, view.window.count, total)?;
    Some(
        titi_tui::scrollbar::cells(theme, body, view.window.start, view.window.count, total)
            .into_iter()
            .map(|(glyph, thumb)| {
                let token = if thumb {
                    ThemeColor::Accent
                } else {
                    ThemeColor::Muted
                };
                Span::styled(glyph, fg(theme, token))
            })
            .collect(),
    )
}

impl Chat {
    /// Whether a panel holds the bottom of the screen: the same set
    /// [`Chat::on_key`] routes to before the composer sees a key.
    pub(crate) fn panel_open(&self) -> bool {
        self.approval.is_some()
            || self.login_for.is_some()
            || self.theme_picker.is_some()
            || self.session_picker.is_some()
            || self.tree_picker.is_some()
            || self.login_picker.is_some()
            || self.model_picker.is_some()
            || self.emoji_picker.is_visible()
            || self.picking()
    }

    /// The prompts this session carried, newest first.
    fn prompt_history(&self) -> Vec<String> {
        let mut prompts: Vec<String> =
            crate::session_fs::session_history(&self.agent_dir, &self.session_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|message| message.role == titi_providers::Role::User)
                .map(|message| message.content.to_string())
                .filter(|text| !text.trim().is_empty())
                .collect();
        prompts.reverse();
        prompts
    }

    /// Ctrl+R, or ↑ at an empty composer: browse the prompts this session has
    /// carried. An empty history says so instead of opening an empty panel.
    pub(crate) fn open_history(&mut self) -> Applied {
        let entries = self.prompt_history();
        if entries.is_empty() {
            self.push(
                LineKind::Note,
                "history: this session has no prompts yet".to_owned(),
            );
            return Applied::none();
        }
        self.history_picker = Some(HistoryPicker::open(entries));
        Applied::none()
    }

    /// Typing while the history browser is up: the model browser's own idiom —
    /// arrows move, a printable key narrows, Backspace takes a character back,
    /// Esc closes.
    pub(crate) fn history_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_history_picker(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_history_picker(1);
                Applied::none()
            }
            Key::Enter => self.accept_history_picker(),
            Key::Esc => {
                self.history_picker = None;
                self.disarm();
                Applied::none()
            }
            Key::Backspace
                if self
                    .history_picker
                    .as_ref()
                    .is_some_and(|picker| !picker.query.is_empty()) =>
            {
                if let Some(picker) = self.history_picker.as_mut() {
                    picker.query.pop();
                    picker.selected = 0;
                }
                Applied::none()
            }
            Key::Char(ch) if !ch.is_control() => {
                if let Some(picker) = self.history_picker.as_mut() {
                    picker.query.push(ch);
                    picker.selected = 0;
                }
                Applied::none()
            }
            other => {
                self.history_picker = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_history_picker(&mut self, delta: isize) {
        let Some(picker) = self.history_picker.as_mut() else {
            return;
        };
        let len = picker.matched().len();
        if len == 0 {
            return;
        }
        let current = picker.selected % len;
        picker.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Enter on a prompt: it lands in the composer, and the picker closes with
    /// nothing sent — no engine command, no log line. A prompt that was sent
    /// once is usually sent again only after being changed, so the composer is
    /// where it belongs.
    fn accept_history_picker(&mut self) -> Applied {
        let Some(picker) = self.history_picker.take() else {
            return Applied::none();
        };
        match picker.selected_text().map(str::to_owned) {
            Some(text) => {
                self.pastes.clear();
                self.input = text;
                Applied::none()
            }
            None => {
                self.push(
                    LineKind::Note,
                    format!("history: nothing matches \"{}\"", picker.query),
                );
                Applied::none()
            }
        }
    }

    pub(crate) fn row_name(&self, row: &PickRow) -> &str {
        match row {
            PickRow::Command(command) => command.name,
            PickRow::Skill(index) => self
                .skills
                .get(*index)
                .map(|skill| skill.name.as_str())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn picking(&self) -> bool {
        !picker_rows(self).is_empty()
    }

    pub(crate) fn move_picker(&mut self, delta: isize) {
        let len = picker_rows(self).len();
        if len == 0 {
            return;
        }
        let current = self.picker % len;
        self.picker = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Replace the token being typed with the highlighted name. Everything
    /// before it stays, so a skill named mid-sentence keeps its sentence.
    pub(crate) fn accept_picker(&mut self) {
        let rows = picker_rows(self);
        let Some(row) = rows.get(self.picker % rows.len().max(1)) else {
            return;
        };
        let name = self.row_name(row).to_owned();
        let Some((start, _)) = slash_token(&self.input) else {
            return;
        };
        self.input.truncate(start);
        self.input.push('/');
        self.input.push_str(&name);
        self.input.push(' ');
        self.picker = 0;
    }

    /// Open or close the emoji picker to match the trailing `:query` under the
    /// caret. It opens on 2+ name characters with at least one match, and a
    /// slash list already up keeps it shut, so the two never show at once.
    pub(crate) fn sync_emoji_picker(&mut self) {
        let query = titi_tui::emoji::trailing_query(&self.input)
            .filter(|query| query.chars().count() >= 2)
            .filter(|_| !self.picking())
            .map(str::to_owned);
        match query {
            Some(query) => {
                self.emoji_picker.open(&query);
                if self.emoji_picker.matches().next().is_none() {
                    self.emoji_picker.hide();
                }
            }
            None => self.emoji_picker.hide(),
        }
    }

    /// Typing while the emoji picker is up. Arrows move, Tab or Enter takes the
    /// highlighted shortcode, Esc closes and leaves the text exactly as typed;
    /// anything else closes the picker and is handled as ordinary composer
    /// input, the way the `/login` picker hands a key back.
    pub(crate) fn emoji_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.emoji_picker.move_selection(true);
                Applied::none()
            }
            Key::Down => {
                self.emoji_picker.move_selection(false);
                Applied::none()
            }
            Key::Tab | Key::Enter => self.accept_emoji_picker(),
            Key::Esc => {
                self.emoji_picker.hide();
                self.disarm();
                Applied::none()
            }
            other => {
                self.emoji_picker.hide();
                self.on_key(other, now)
            }
        }
    }

    /// Tab or Enter on the emoji picker: replace the `:query` under the caret
    /// with the highlighted glyph.
    fn accept_emoji_picker(&mut self) -> Applied {
        let glyph = self.emoji_picker.accept();
        self.emoji_picker.hide();
        let Some(glyph) = glyph else {
            return Applied::none();
        };
        if let Some(colon) = self.input.rfind(':') {
            self.input.truncate(colon);
        }
        self.input.push_str(glyph);
        self.picker = 0;
        Applied::none()
    }

    /// Typing while the subscription picker is up. Arrows move, Enter signs
    /// in to the highlighted row, Esc closes without writing; anything else
    /// closes the picker and is handled as ordinary composer input.
    pub(crate) fn login_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_login_picker(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_login_picker(1);
                Applied::none()
            }
            Key::Enter => self.accept_login_picker(),
            Key::Esc => {
                self.login_picker = None;
                self.disarm();
                Applied::none()
            }
            other => {
                self.login_picker = None;
                self.on_key(other, now)
            }
        }
    }

    /// Ctrl+X: the sessions this agent directory holds — the list `/sessions`
    /// shows — with the row the screen is on selected.
    pub(crate) fn open_session_picker(&mut self) -> Applied {
        let sessions = self.session_choices();
        if sessions.is_empty() {
            self.push(LineKind::Note, "no sessions to switch to".to_owned());
            return Applied::none();
        }
        self.session_picker = Some(
            sessions
                .iter()
                .position(|id| *id == self.session_id)
                .unwrap_or(0),
        );
        Applied::none()
    }

    pub(crate) fn session_choices(&self) -> Vec<String> {
        crate::session_fs::list_sessions_from(&self.agent_dir)
    }

    /// Typing while the session switcher is up. Like the login picker: arrows
    /// move, Enter switches, Esc closes, and anything else closes and is
    /// handled as composer input.
    pub(crate) fn session_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_session_picker(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_session_picker(1);
                Applied::none()
            }
            Key::Enter => self.accept_session_picker(),
            Key::Esc => {
                self.session_picker = None;
                Applied::none()
            }
            other => {
                self.session_picker = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_session_picker(&mut self, delta: isize) {
        let len = self.session_choices().len();
        if len == 0 {
            return;
        }
        let current = self.session_picker.unwrap_or(0) % len;
        self.session_picker = Some((current as isize + delta).rem_euclid(len as isize) as usize);
    }

    /// Enter on a session: its history replaces the screen and the engine is
    /// told to replay it, the way a rewind does. Moving the id without the
    /// history would leave the model on a conversation the screen is not
    /// showing.
    fn accept_session_picker(&mut self) -> Applied {
        let choice = self
            .session_picker
            .and_then(|at| self.session_choices().get(at).cloned());
        self.session_picker = None;
        let Some(id) = choice else {
            return Applied::none();
        };
        self.switch_to_session(id)
    }

    /// `/tree`: the session's entries as the tree they are, the leaf marked.
    /// A session with nothing in it says so rather than opening an empty panel,
    /// the way the prompt history does.
    pub(crate) fn open_tree(&mut self) -> Applied {
        match TreePicker::open(&self.agent_dir, &self.session_id) {
            Ok(picker) => {
                self.tree_picker = Some(picker);
                Applied::none()
            }
            Err(reason) => {
                self.push(LineKind::Note, format!("tree: {reason}"));
                Applied::none()
            }
        }
    }

    /// Typing while the tree is up: the session switcher's own idiom — arrows
    /// move, Enter branches there, Esc closes; anything else closes the panel
    /// and is handled as composer input.
    pub(crate) fn tree_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_tree_picker(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_tree_picker(1);
                Applied::none()
            }
            Key::Enter => self.accept_tree_picker(),
            Key::Esc => {
                self.tree_picker = None;
                Applied::none()
            }
            other => {
                self.tree_picker = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_tree_picker(&mut self, delta: isize) {
        let Some(picker) = self.tree_picker.as_mut() else {
            return;
        };
        let len = picker.rows.len();
        if len == 0 {
            return;
        }
        let current = picker.selected % len;
        picker.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Enter on a row: the leaf moves there, and the path through it replaces
    /// the screen and the engine's history — the replay `/rewind` and a session
    /// switch already use. The branch left behind stays in the store, which is
    /// what makes this a branch and not a rewind.
    pub(crate) fn accept_tree_picker(&mut self) -> Applied {
        let Some(picker) = self.tree_picker.take() else {
            return Applied::none();
        };
        let Some(entry_id) = picker.selected_id().map(str::to_owned) else {
            return Applied::none();
        };
        match crate::session_fs::branch_at(&self.agent_dir, &self.session_id, &entry_id) {
            Ok((messages, note)) => {
                self.show_history(&messages);
                self.turn_active = false;
                self.turn_started = None;
                self.phase = WorkPhase::Waiting;
                self.approval = None;
                self.push(LineKind::Note, note);
                Applied::send(EngineCommand::RestoreHistory { messages }, None)
            }
            Err(reason) => {
                self.push(LineKind::Error, format!("tree: {reason}"));
                Applied::none()
            }
        }
    }

    /// Typing while the `/sessions <query>` browser is up. Like the model
    /// browser: arrows move, Enter switches, a printable key widens the query
    /// (and re-runs it), Backspace takes the last character back, and Esc
    /// clears the query and closes only on the second press.
    pub(crate) fn session_search_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_session_search(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_session_search(1);
                Applied::none()
            }
            Key::Enter => self.accept_session_search(),
            Key::Esc => {
                match self.session_search.as_mut() {
                    Some(search) if !search.query.is_empty() => {
                        let dir = self.agent_dir.clone();
                        search.retype(&dir, String::new());
                    }
                    _ => self.session_search = None,
                }
                self.disarm();
                Applied::none()
            }
            Key::Backspace
                if self
                    .session_search
                    .as_ref()
                    .is_some_and(|search| !search.query.is_empty()) =>
            {
                let dir = self.agent_dir.clone();
                if let Some(search) = self.session_search.as_mut() {
                    let mut query = search.query.clone();
                    query.pop();
                    search.retype(&dir, query);
                }
                Applied::none()
            }
            Key::Char(ch) if !ch.is_control() => {
                let dir = self.agent_dir.clone();
                if let Some(search) = self.session_search.as_mut() {
                    let mut query = search.query.clone();
                    query.push(ch);
                    search.retype(&dir, query);
                }
                Applied::none()
            }
            other => {
                self.session_search = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_session_search(&mut self, delta: isize) {
        let Some(search) = self.session_search.as_mut() else {
            return;
        };
        let len = search.hits.len();
        if len == 0 {
            return;
        }
        let current = search.selected % len;
        search.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Enter on a hit: the session the matching line belongs to, through the
    /// switch the list behind bare `/sessions` already uses. Enter with nothing
    /// to take says so rather than closing the panel in silence.
    fn accept_session_search(&mut self) -> Applied {
        let Some(search) = self.session_search.take() else {
            return Applied::none();
        };
        let Some(id) = search.selected_hit().map(|hit| hit.session_id.clone()) else {
            self.push(
                LineKind::Note,
                format!("sessions: no match for \"{}\"", search.query),
            );
            return Applied::none();
        };
        self.switch_to_session(id)
    }

    fn move_login_picker(&mut self, delta: isize) {
        let len = login_choices().len();
        if len == 0 {
            return;
        }
        let current = self.login_picker.unwrap_or(0) % len;
        self.login_picker = Some((current as isize + delta).rem_euclid(len as isize) as usize);
    }

    /// Signs in with the highlighted row. The picker is a way to name a
    /// provider and a method, nothing more: it writes no credential itself.
    fn accept_login_picker(&mut self) -> Applied {
        let choice = self
            .login_picker
            .and_then(|at| login_choices().get(at).copied());
        self.login_picker = None;
        match choice {
            Some(choice) => self.start_oauth_login(choice.provider, choice.method),
            None => Applied::none(),
        }
    }

    /// Typing while the model picker is up.
    ///
    /// Arrows move through the matched rows, Enter switches, a printable key
    /// narrows the query, Backspace takes back the last character. Esc clears
    /// the query and closes only on the second press: a filter is cheap to
    /// undo, but a picker that closed on the first Esc would make a narrow
    /// search cost a reopen.
    pub(crate) fn model_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_model_picker(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_model_picker(1);
                Applied::none()
            }
            Key::Enter => self.accept_model_picker(),
            Key::Esc => {
                match self.model_picker.as_mut() {
                    Some(picker) if !picker.query.is_empty() => {
                        picker.query.clear();
                        picker.selected = 0;
                    }
                    _ => self.model_picker = None,
                }
                self.disarm();
                Applied::none()
            }
            Key::Backspace
                if self
                    .model_picker
                    .as_ref()
                    .is_some_and(|p| !p.query.is_empty()) =>
            {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.query.pop();
                    picker.selected = 0;
                }
                Applied::none()
            }
            Key::Char(ch) if !ch.is_control() => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.query.push(ch);
                    picker.selected = 0;
                }
                Applied::none()
            }
            other => {
                // Anything else — Backspace with an empty query, Ctrl-C,
                // Ctrl-D — closes the picker and is handled as ordinary
                // composer input, the way the `/login` picker hands a key
                // back.
                self.model_picker = None;
                self.on_key(other, now)
            }
        }
    }

    /// Typing while the theme picker is up. Like the model browser: arrows
    /// move, Enter applies, a printable key narrows, Backspace takes back a
    /// character, and Esc closes without touching the theme the screen is on —
    /// including one the cursor has been arrowed past.
    pub(crate) fn theme_picker_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_theme_picker(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_theme_picker(1);
                Applied::none()
            }
            Key::Enter => self.accept_theme_picker(),
            Key::Esc => {
                match self.theme_picker.as_mut() {
                    Some(picker) if !picker.query.is_empty() => {
                        picker.query.clear();
                        picker.selected = 0;
                    }
                    _ => self.theme_picker = None,
                }
                self.disarm();
                Applied::none()
            }
            Key::Backspace
                if self
                    .theme_picker
                    .as_ref()
                    .is_some_and(|picker| !picker.query.is_empty()) =>
            {
                if let Some(picker) = self.theme_picker.as_mut() {
                    picker.query.pop();
                    picker.selected = 0;
                }
                Applied::none()
            }
            Key::Char(ch) if !ch.is_control() => {
                if let Some(picker) = self.theme_picker.as_mut() {
                    picker.query.push(ch);
                    picker.selected = 0;
                }
                Applied::none()
            }
            other => {
                self.theme_picker = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_theme_picker(&mut self, delta: isize) {
        let Some(picker) = self.theme_picker.as_mut() else {
            return;
        };
        let len = picker.matched().len();
        if len == 0 {
            return;
        }
        let current = picker.selected % len;
        picker.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Enter on a row: the palette the next frame is painted with, remembered
    /// for this appearance slot.
    fn accept_theme_picker(&mut self) -> Applied {
        let Some(picker) = self.theme_picker.take() else {
            return Applied::none();
        };
        match picker.selected_name().map(str::to_owned) {
            Some(name) => self.apply_theme(&name),
            None => {
                self.push(
                    LineKind::Error,
                    format!(
                        "no theme matches \"{}\"; try /theme to see the list",
                        picker.query
                    ),
                );
                Applied::none()
            }
        }
    }

    /// What the screen is showing and why: the palette's name, and whether it
    /// is the user's own choice for this appearance slot or the probe's pick.
    ///
    /// Pure: it reads the settings and the terminal probe, and loads nothing, so
    /// the picker can mark its row without touching the palette on screen.
    pub(crate) fn theme_state(&self) -> ThemeState {
        let inputs = titi_tui::theme::appearance::AppearanceInputs::from_env();
        let chosen = titi_config::settings::Settings::load(
            &self.agent_dir,
            &crate::session_fs::current_workspace(),
            &[],
        )
        .ok()
        .and_then(|settings| {
            settings
                .get(crate::themes::theme_slot(&inputs))
                .and_then(|value| value.as_str().map(str::to_owned))
        });
        ThemeState {
            name: chosen.clone().unwrap_or_else(|| {
                titi_tui::theme::appearance::resolve_auto_theme(
                    titi_tui::theme::appearance::AUTO_DARK_THEME,
                    titi_tui::theme::appearance::AUTO_LIGHT_THEME,
                    &inputs,
                )
            }),
            chosen,
        }
    }

    fn move_model_picker(&mut self, delta: isize) {
        let Some(picker) = self.model_picker.as_mut() else {
            return;
        };
        let len = picker.matched().len();
        if len == 0 {
            return;
        }
        let current = picker.selected % len;
        picker.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Switches to the highlighted offer. A query that matches nothing is
    /// refused in the transcript, in the words `/switch` refuses it with;
    /// a switch that goes through is announced once, by the engine.
    fn accept_model_picker(&mut self) -> Applied {
        let Some(picker) = self.model_picker.take() else {
            return Applied::none();
        };
        let matched = picker.matched();
        let Some(offer) = matched
            .get(picker.selected % matched.len().max(1))
            .map(|at| &picker.offers[*at])
        else {
            self.push(
                LineKind::Error,
                format!(
                    "no model matches \"{}\"; try /model to see the list",
                    picker.query
                ),
            );
            return Applied::none();
        };
        let model = offer.target().to_owned();
        self.model = model.clone();
        Applied::send(
            EngineCommand::SwitchModel {
                model: model.into(),
            },
            None,
        )
    }

    /// Opens the model picker over the catalog, roles first.
    ///
    /// The selection starts on the model in use, and the window pins the
    /// selection, so the picker never opens with the current model scrolled
    /// out of sight.
    pub(crate) fn open_model_picker(&mut self) {
        let mut picker = ModelPicker {
            offers: picker_roles(self)
                .into_iter()
                .map(|(name, model)| ModelOffer::Role { name, model })
                .chain(model_rows(self).into_iter().map(ModelOffer::Model))
                .collect(),
            query: String::new(),
            selected: 0,
        };
        if let Some(at) = picker
            .matched()
            .iter()
            .position(|at| picker.offers[*at].target() == self.model)
        {
            picker.selected = at;
        }
        self.model_picker = Some(picker);
    }

    /// The bare-`/login` picker's rows, in the order the arrow keys walk
    /// them, each `Display ·method`. Empty when the picker is closed. A
    /// surface without a screen can read what is on offer.
    pub fn login_picker_rows(&self) -> Vec<String> {
        if self.login_picker.is_none() {
            return Vec::new();
        }
        login_choices()
            .into_iter()
            .map(login_choice_label)
            .collect()
    }
}
