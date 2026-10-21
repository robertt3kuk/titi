//! The command palette (Ctrl+K) and the value lists behind commands that take
//! one of a few words.
//!
//! One list holds everything a person might go looking for by name: every
//! slash command, every picker-backed panel and every setting the config
//! knows. It is the same panel the other pickers use, filtered with the same
//! [`fuzzy_score`]. Enter on a command that needs nothing runs it; on one that
//! takes a word from a short list (`/theme`, `/details`, `/mouse`, `/genome`,
//! `/statusline`) it opens that list instead, with the value in force marked.
//! Typing the command and a space in the composer opens the same list.

use std::time::Instant;

use titi_config::settings as cfg;
use titi_tui::markdown::SectionMode;
use titi_tui::status_bar::{ContextLine, StatusLinePreset};

use crate::chat::*;
use crate::pickers::{
    COMMANDS, PanelLine, PanelView, THEME_AUTO, ThemePicker, fuzzy_score, panel_body, panel_view,
};
use crate::transcript::*;

/// The mark a list puts on the value that is in force. The same words the
/// theme and session pickers use.
const CURRENT: &str = "✓ current";

/// Commands that fail without text after them. Enter on one in the palette
/// leaves `/name ` in the composer for the person to finish, as Tab would.
const NEEDS_TEXT: [&str; 5] = ["btw", "loop", "goal", "council", "graph"];

/// Commands whose argument is one of a short list: [`command_values`] knows
/// the list for each.
pub(crate) const VALUE_COMMANDS: [&str; 6] = [
    "theme",
    "statusline",
    "mouse",
    "genome",
    "details",
    "effort",
];

/// What `/effort` and `model.effort` take: `off` sends nothing, and each
/// other word is a level of [`titi_providers::Effort`].
pub(crate) const EFFORT_WORDS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The sections `/details` names, and the three modes each can be in.
const DETAIL_SECTIONS: [&str; 5] = ["thinking", "tools", "subagents", "activity", "folded"];
const DETAIL_MODES: [&str; 3] = ["hidden", "collapsed", "expanded"];

const MOUSE_PRESETS: [&str; 4] = ["off", "wheel", "buttons", "all"];

// ---------------------------------------------------------------------------
// What the palette lists
// ---------------------------------------------------------------------------

/// A panel the palette can open. Each is also reachable from the keyboard or a
/// command; the palette is the place to find them by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    Models,
    Themes,
    Sessions,
    History,
    Tree,
    Login,
}

/// What a value list holds for a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Values {
    /// `true` or `false`.
    Bool,
    /// One of a fixed set of words.
    OneOf(&'static [&'static str]),
    /// One of the palettes this build carries.
    Themes,
    /// Anything: the palette prints the value and does not offer a list.
    Free,
}

/// A setting the config knows by name. `titi-config` owns only the key names
/// (its defaults live in the binaries that read them), so this table is the
/// palette's own, and a test holds it to that crate's `*_KEY` constants.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Setting {
    pub(crate) key: &'static str,
    pub(crate) about: &'static str,
    pub(crate) values: Values,
}

const fn setting(key: &'static str, about: &'static str, values: Values) -> Setting {
    Setting { key, about, values }
}

pub(crate) const SETTINGS: &[Setting] = &[
    setting(
        cfg::THEME_DARK_KEY,
        "theme on a dark terminal",
        Values::Themes,
    ),
    setting(
        cfg::THEME_LIGHT_KEY,
        "theme on a light terminal",
        Values::Themes,
    ),
    setting(
        cfg::GENOME_ENABLED_KEY,
        "build the repo map for the prompt",
        Values::Bool,
    ),
    setting(
        cfg::GENOME_LIMIT_KEY,
        "files the repo map keeps",
        Values::Free,
    ),
    setting(
        cfg::NOTIFY_COMPLETION_KEY,
        "notify when a turn finishes",
        Values::Bool,
    ),
    setting(
        cfg::NOTIFY_ERROR_KEY,
        "notify when a turn fails",
        Values::Bool,
    ),
    setting(
        cfg::NOTIFY_ASK_KEY,
        "notify when a turn waits on you",
        Values::Bool,
    ),
    setting(
        cfg::TERMINAL_PROGRESS_KEY,
        "the terminal's own progress bar",
        Values::Bool,
    ),
    setting(
        cfg::COMPOSER_TOKEN_RATE_KEY,
        "generation rate on the working row",
        Values::Bool,
    ),
    setting(
        cfg::EDITOR_VIM_KEY,
        "edit the draft the vim way",
        Values::Bool,
    ),
    setting(
        cfg::TUI_RENDER_MERMAID_KEY,
        "draw mermaid flowcharts",
        Values::Bool,
    ),
    setting(
        cfg::TUI_AUTOGRAPH_KEY,
        "chart numeric tables",
        Values::OneOf(&["smart", "always", "off"]),
    ),
    setting(
        cfg::MODEL_DEFAULT_ROLE_KEY,
        "the model a session starts on",
        Values::Free,
    ),
    setting(
        cfg::MODEL_EFFORT_KEY,
        "how hard the model is asked to think",
        Values::OneOf(&EFFORT_WORDS),
    ),
    setting(
        cfg::DISPLAY_SMOOTH_STREAMING_KEY,
        "reveal answers at a steady rate",
        Values::Bool,
    ),
    setting(
        cfg::STARTUP_CHANGELOG_KEY,
        "say once when the build changed",
        Values::Bool,
    ),
    setting(
        cfg::DISPLAY_PINNED_AGENTS_KEY,
        "the strip of live agents",
        Values::OneOf(&["off", "collapsed", "full"]),
    ),
    setting(
        cfg::DISPLAY_SUBAGENT_PREVIEW_KEY,
        "what a pinned agent is doing",
        Values::Bool,
    ),
    setting(
        cfg::TUI_TIGHT_KEY,
        "drop one cell of horizontal padding",
        Values::Bool,
    ),
    setting(
        cfg::STATUS_LINE_SEPARATOR_KEY,
        "glyph between status line segments",
        Values::OneOf(&[
            "powerline",
            "powerline-thin",
            "slash",
            "pipe",
            "block",
            "none",
            "ascii",
        ]),
    ),
    setting(
        cfg::STATUS_LINE_SESSION_ACCENT_KEY,
        "accent on the composer border",
        Values::Bool,
    ),
    setting(
        cfg::STATUS_LINE_TRANSPARENT_KEY,
        "let the terminal show through the status line",
        Values::Bool,
    ),
    setting(
        cfg::STATUS_LINE_EDIT_COUNTER_KEY,
        "the edit counter segment",
        Values::Bool,
    ),
    setting(
        cfg::STATUS_LINE_PRESET_KEY,
        "which status line preset",
        Values::OneOf(&StatusLinePreset::IDS),
    ),
    setting(
        cfg::STATUS_LINE_CONTEXT_LINE_KEY,
        "context gauge between segment groups",
        Values::OneOf(&ContextLine::IDS),
    ),
    setting(
        cfg::TREE_FILTER_MODE_KEY,
        "what /tree opens showing",
        Values::OneOf(&["default", "no-tools", "user-only"]),
    ),
    setting(
        cfg::TRACE_THINKING_KEY,
        "write reasoning text into trace spans",
        Values::Bool,
    ),
    setting(
        cfg::SKILLS_FOREIGN_USER_DIRS_KEY,
        "also read skills and commands from ~/.agents and ~/.claude",
        Values::Bool,
    ),
    setting(
        cfg::DISPLAY_TURN_FOOTER_TIME_KEY,
        "wall time in the turn footer",
        Values::Bool,
    ),
    setting(
        cfg::DISPLAY_TURN_FOOTER_TOKENS_KEY,
        "token counts in the turn footer",
        Values::Bool,
    ),
    setting(
        cfg::DISPLAY_TURN_FOOTER_CACHE_MISS_KEY,
        "cache-miss mark in the turn footer",
        Values::Bool,
    ),
    setting(
        cfg::SESSION_AUTO_RESUME_KEY,
        "resume the newest session on launch",
        Values::Bool,
    ),
    setting(
        cfg::BASH_AUTO_BACKGROUND_KEY,
        "ms before a bash call goes to the background",
        Values::Free,
    ),
    setting(
        cfg::AGENT_MAX_TOOL_ROUNDS_KEY,
        "tool rounds a turn may spend, 1 to 500",
        Values::Free,
    ),
    setting(
        cfg::AGENT_SUBAGENT_MAX_TOOL_ROUNDS_KEY,
        "tool rounds a subagent may spend, 1 to 500",
        Values::Free,
    ),
    setting(
        cfg::PASTE_MENU_THRESHOLD_KEY,
        "lines before a paste offers a menu",
        Values::Free,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Run `/name` as if it had been typed and sent.
    Command(&'static str),
    /// A skill or file command (`"skill"`, `"command"`): Enter leaves
    /// `/name ` in the composer for its arguments.
    Slash(&'static str),
    Panel(Panel),
    Setting(&'static Setting),
}

/// One row of the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// What is shown and matched: `/context`, `models`, `genome.enabled`.
    pub(crate) label: String,
    pub(crate) about: String,
    pub(crate) action: Action,
}

impl Entry {
    /// The row's text: the label in a column, then what it is. Panels and
    /// settings say which they are, so a name shared with a command is told
    /// apart.
    fn row(&self) -> String {
        let kind = match self.action {
            Action::Command(_) => "",
            Action::Slash(kind) => return format!("{:<24} {kind} · {}", self.label, self.about),
            Action::Panel(_) => "panel · ",
            Action::Setting(_) => "setting · ",
        };
        format!("{:<24} {kind}{}", self.label, self.about)
    }

    /// The part of the label a query is matched against.
    fn name(&self) -> &str {
        self.label.trim_start_matches('/')
    }
}

const PANELS: [(&str, &str, Panel); 6] = [
    ("models", "browse and switch models · alt+m", Panel::Models),
    ("themes", "choose a palette", Panel::Themes),
    (
        "sessions",
        "switch to a stored session · ctrl+x",
        Panel::Sessions,
    ),
    (
        "history",
        "search this project's prompts · ctrl+r",
        Panel::History,
    ),
    ("tree", "navigate the session tree", Panel::Tree),
    ("login", "sign in to a provider", Panel::Login),
];

/// Every command, panel and setting, in the order a bare palette shows them.
pub(crate) fn entries() -> Vec<Entry> {
    entries_with(Vec::new())
}

/// [`entries`] with the skills and file commands of this session listed
/// after the built-in commands.
pub(crate) fn entries_with(extra: Vec<Entry>) -> Vec<Entry> {
    let commands = COMMANDS.iter().map(|command| Entry {
        label: format!("/{}", command.name),
        about: command.about.to_owned(),
        action: Action::Command(command.name),
    });
    let panels = PANELS.iter().map(|(label, about, panel)| Entry {
        label: (*label).to_owned(),
        about: (*about).to_owned(),
        action: Action::Panel(*panel),
    });
    let settings = SETTINGS.iter().map(|setting| Entry {
        label: setting.key.to_owned(),
        about: setting.about.to_owned(),
        action: Action::Setting(setting),
    });
    // The preset is a command's value list, reachable by this name too.
    let preset = Entry {
        label: "statusline preset".to_owned(),
        about: "choose the status line".to_owned(),
        action: Action::Command("statusline"),
    };
    commands
        .chain(extra)
        .chain(panels)
        .chain(std::iter::once(preset))
        .chain(settings)
        .collect()
}

/// Which entries `query` keeps, best first: the name itself, a name it starts,
/// a name that contains it, a name it is a [`fuzzy_score`] subsequence of
/// (`ctx` finds `/context`), then the entries whose description it fits. From
/// two characters on for the fuzzy tiers, because one letter matches nearly
/// everything. Ties keep the list's own order, so a filtered list never jumps
/// between frames.
pub(crate) fn rank(entries: &[Entry], query: &str) -> Vec<usize> {
    let query = query.trim().trim_start_matches('/').to_lowercase();
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let fuzzy = query.chars().count() >= 2;
    let mut kept: Vec<(u8, i32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| {
            let name = entry.name().to_lowercase();
            if name == query {
                Some((0, 0, at))
            } else if name.starts_with(&query) {
                Some((1, 0, at))
            } else if name.contains(&query) {
                Some((2, 0, at))
            } else if !fuzzy {
                None
            } else if let Some(score) = fuzzy_score(&query, &name) {
                Some((3, -score, at))
            } else {
                fuzzy_score(&query, &entry.about).map(|score| (4, -score, at))
            }
        })
        .collect();
    kept.sort_unstable();
    kept.into_iter().map(|(_, _, at)| at).collect()
}

// ---------------------------------------------------------------------------
// The value lists
// ---------------------------------------------------------------------------

/// One word a list offers, and whether it is the one in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArgValue {
    pub(crate) arg: String,
    pub(crate) current: bool,
}

impl ArgValue {
    fn new(arg: impl Into<String>, current: bool) -> Self {
        Self {
            arg: arg.into(),
            current,
        }
    }

    fn row(&self) -> String {
        if self.current {
            format!("{}  {CURRENT}", self.arg)
        } else {
            self.arg.clone()
        }
    }
}

/// Whose value a list chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// A command's argument: a pick runs `/name <arg>`.
    Command(&'static str),
    /// A setting's value: a pick writes it to the agent config.
    Setting(&'static Setting),
}

/// A list of the words a command or setting takes, opened from the palette;
/// Esc goes back to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueList {
    pub(crate) target: Target,
    pub(crate) items: Vec<ArgValue>,
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// Where the value in force came from, for the title.
    pub(crate) source: Option<String>,
}

impl ValueList {
    pub(crate) fn matched(&self) -> Vec<usize> {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(at, item)| fuzzy_score(&self.query, &item.arg).map(|score| (score, at)))
            .collect();
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored.into_iter().map(|(_, at)| at).collect()
    }

    fn selected_arg(&self) -> Option<&str> {
        let matched = self.matched();
        let at = *matched.get(self.selected % matched.len().max(1))?;
        self.items.get(at).map(|item| item.arg.as_str())
    }
}

/// The palette, its value list and the composer's argument list, as the chat
/// holds them.
#[derive(Debug, Default)]
pub(crate) struct PaletteState {
    pub(crate) list: Option<Palette>,
    pub(crate) values: Option<ValueList>,
    /// The draft the highlight of the argument list was last moved on. Enter
    /// picks the highlighted word only while the draft is still that one: a
    /// typed `/details thinking` is a command of its own, and an Enter that
    /// swapped it for the first row would change what it does.
    pub(crate) arg_moved: Option<String>,
}

/// The words offered under a draft of `/command `, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArgRows {
    pub(crate) command: &'static str,
    pub(crate) rows: Vec<ArgValue>,
}

impl PaletteState {
    pub(crate) fn is_open(&self) -> bool {
        self.list.is_some() || self.values.is_some()
    }
}

/// The palette's query and cursor over [`entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Palette {
    pub(crate) entries: Vec<Entry>,
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// Opened by `/help`: the commands alone, and a title that says so.
    pub(crate) help: bool,
}

impl Palette {
    fn open(extra: Vec<Entry>) -> Self {
        Self {
            entries: entries_with(extra),
            query: String::new(),
            selected: 0,
            help: false,
        }
    }

    /// `/help`: every command and nothing else, with `query` already typed.
    fn help(query: &str) -> Self {
        Self {
            entries: entries()
                .into_iter()
                .filter(|entry| {
                    matches!(entry.action, Action::Command(_)) && entry.label.starts_with('/')
                })
                .collect(),
            query: query.to_owned(),
            selected: 0,
            help: true,
        }
    }

    fn matched(&self) -> Vec<usize> {
        rank(&self.entries, &self.query)
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let matched = self.matched();
        let at = *matched.get(self.selected % matched.len().max(1))?;
        self.entries.get(at)
    }
}

/// The words `/name` takes, with the one in force marked; `None` for a command
/// that is not in [`VALUE_COMMANDS`].
pub(crate) fn command_values(chat: &Chat, name: &str) -> Option<Vec<ArgValue>> {
    let values = match name {
        "theme" => {
            let state = chat.theme_state();
            ThemePicker::open()
                .names
                .into_iter()
                .map(|name| {
                    let current = if state.chosen.is_some() {
                        state.chosen.as_deref() == Some(name.as_str())
                    } else {
                        name == THEME_AUTO
                    };
                    ArgValue::new(name, current)
                })
                .collect()
        }
        "statusline" => StatusLinePreset::IDS
            .iter()
            .map(|id| ArgValue::new(*id, chat.status_line.preset.id() == *id))
            .collect(),
        "mouse" => MOUSE_PRESETS
            .iter()
            .map(|name| ArgValue::new(*name, chat.mouse_preset.name() == *name))
            .collect(),
        "genome" => {
            let on = titi_config::settings::Settings::load(
                &chat.agent_dir,
                &crate::session_fs::current_workspace(),
                &[],
            )
            .ok()
            .map(|settings| !cfg::switch_off(&settings, cfg::GENOME_ENABLED_KEY));
            vec![
                ArgValue::new("on", on == Some(true)),
                ArgValue::new("off", on == Some(false)),
            ]
        }
        "details" => {
            let mut items = Vec::new();
            for section in DETAIL_SECTIONS {
                let now = match chat.details.mode(section) {
                    Some(SectionMode::Hidden) => "hidden",
                    Some(SectionMode::Collapsed) => "collapsed",
                    Some(SectionMode::Expanded) => "expanded",
                    None => "",
                };
                for mode in DETAIL_MODES {
                    items.push(ArgValue::new(format!("{section} {mode}"), now == mode));
                }
            }
            items.push(ArgValue::new("cycle", false));
            items
        }
        "effort" => EFFORT_WORDS
            .iter()
            .map(|word| ArgValue::new(*word, chat.effort_word() == *word))
            .collect(),
        _ => return None,
    };
    Some(values)
}

/// The words a setting offers: `None` for one that takes anything.
fn setting_values(setting: &Setting, now: Option<&str>) -> Option<Vec<ArgValue>> {
    let words: Vec<String> = match setting.values {
        Values::Bool => vec!["true".to_owned(), "false".to_owned()],
        Values::OneOf(words) => words.iter().map(|word| (*word).to_owned()).collect(),
        Values::Themes => crate::themes::theme_names(),
        Values::Free => return None,
    };
    Some(
        words
            .into_iter()
            .map(|word| {
                let current = now == Some(word.as_str());
                ArgValue::new(word, current)
            })
            .collect(),
    )
}

/// A config value as its word: a string as itself, anything else as JSON.
fn value_word(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Panel views
// ---------------------------------------------------------------------------

/// The palette's or the value list's panel, when one is open.
pub(crate) fn palette_view(chat: &Chat, total: u16) -> Option<PanelView> {
    if let Some(list) = &chat.palette.values {
        return Some(values_view(list, total));
    }
    if let Some(found) = chat.arg_rows() {
        let selected = chat.picker % found.rows.len();
        let lines = found
            .rows
            .iter()
            .map(|value| PanelLine::Row {
                text: value.row(),
                accent: false,
            })
            .collect();
        return Some(panel_view(
            Some(format!("/{}", found.command)),
            lines,
            Some(selected),
            panel_body(total),
        ));
    }
    let palette = chat.palette.list.as_ref()?;
    let matched = palette.matched();
    let lines: Vec<PanelLine> = if matched.is_empty() {
        vec![PanelLine::Heading("nothing matches".to_owned())]
    } else {
        matched
            .iter()
            .map(|at| PanelLine::Row {
                text: palette.entries[*at].row(),
                accent: false,
            })
            .collect()
    };
    let (name, noun) = if palette.help {
        ("help", " commands")
    } else {
        ("palette", "")
    };
    let title = if palette.query.is_empty() {
        format!("{name} · {}{noun}", palette.entries.len())
    } else {
        format!(
            "{name} · {} of {}{noun} · {}",
            matched.len(),
            palette.entries.len(),
            palette.query
        )
    };
    let selected = (!matched.is_empty()).then(|| palette.selected % matched.len());
    Some(panel_view(Some(title), lines, selected, panel_body(total)))
}

fn values_view(list: &ValueList, total: u16) -> PanelView {
    let matched = list.matched();
    let lines: Vec<PanelLine> = if matched.is_empty() {
        vec![PanelLine::Heading("nothing matches".to_owned())]
    } else {
        matched
            .iter()
            .map(|at| PanelLine::Row {
                text: list.items[*at].row(),
                accent: false,
            })
            .collect()
    };
    let name = match list.target {
        Target::Command(name) => format!("/{name}"),
        Target::Setting(setting) => setting.key.to_owned(),
    };
    let mut title = name;
    if let Some(source) = &list.source {
        title.push_str(&format!(" · {source}"));
    }
    if !list.query.is_empty() {
        title.push_str(&format!(" · {}", list.query));
    }
    let selected = (!matched.is_empty()).then(|| list.selected % matched.len());
    panel_view(Some(title), lines, selected, panel_body(total))
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

impl Chat {
    /// Ctrl+K: on an empty draft the palette opens; on a draft the key keeps
    /// the editor meaning the crate's table gave it (`deleteToLineEnd`) and
    /// removes everything after the caret. The one rule means a person who
    /// has typed something never loses it to a menu.
    pub(crate) fn ctrl_k(&mut self) -> Applied {
        self.disarm();
        if self.input.is_empty() {
            self.palette.list = Some(Palette::open(self.extension_entries()));
            return Applied::none();
        }
        let (start, end) = self.whole_markers(self.caret(), self.input.len());
        self.input.replace_range(start..end, "");
        self.set_caret(start);
        self.forget_cut_markers();
        self.sync_emoji_picker();
        self.picker = 0;
        self.picker_hidden = false;
        self.scroll_offset = 0;
        Applied::none()
    }

    /// The skills a person may type and the file commands, as palette rows.
    fn extension_entries(&self) -> Vec<Entry> {
        let skills = self.skills.iter().map(|skill| Entry {
            label: format!("/{}", skill.name),
            about: skill.about.clone(),
            action: Action::Slash("skill"),
        });
        let commands = self.commands.iter().map(|command| Entry {
            label: format!("/{}", command.name),
            about: if command.hint.is_empty() {
                command.about.clone()
            } else {
                format!("{} {}", command.hint, command.about)
            },
            action: Action::Slash("command"),
        });
        skills.chain(commands).collect()
    }

    /// `/help`: the palette on the commands alone, `query` typed in.
    pub(crate) fn open_help(&mut self, query: &str) -> Applied {
        self.palette.list = Some(Palette::help(query.trim()));
        Applied::none()
    }

    /// Typing while the palette is up: the history browser's idiom — arrows
    /// move, a printable key narrows, Backspace takes a character back, Esc or
    /// Ctrl+K closes, Enter acts. Any other key closes it and is the
    /// composer's own.
    pub(crate) fn palette_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => {
                self.move_palette(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_palette(1);
                Applied::none()
            }
            Key::Enter => self.accept_palette(),
            Key::Esc | Key::CtrlK => {
                self.palette.list = None;
                self.disarm();
                Applied::none()
            }
            Key::Backspace => {
                if let Some(palette) = self.palette.list.as_mut() {
                    palette.query.pop();
                    palette.selected = 0;
                }
                Applied::none()
            }
            Key::Char(ch) if !ch.is_control() => {
                if let Some(palette) = self.palette.list.as_mut() {
                    palette.query.push(ch);
                    palette.selected = 0;
                }
                Applied::none()
            }
            other => {
                self.palette.list = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_palette(&mut self, delta: isize) {
        let Some(palette) = self.palette.list.as_mut() else {
            return;
        };
        let len = palette.matched().len();
        if len == 0 {
            return;
        }
        let current = palette.selected % len;
        palette.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    fn accept_palette(&mut self) -> Applied {
        let Some(entry) = self
            .palette
            .list
            .as_ref()
            .and_then(Palette::selected_entry)
            .cloned()
        else {
            return Applied::none();
        };
        match entry.action {
            Action::Command(name) if VALUE_COMMANDS.contains(&name) => {
                self.open_values(name);
                Applied::none()
            }
            Action::Command(name) if NEEDS_TEXT.contains(&name) => {
                self.palette.list = None;
                self.input = format!("/{name} ");
                self.caret_to_end();
                Applied::none()
            }
            Action::Slash(_) => {
                self.palette.list = None;
                self.input = format!("{} ", entry.label);
                self.caret_to_end();
                Applied::none()
            }
            Action::Command(name) => {
                self.palette.list = None;
                self.slash(&format!("/{name}"))
                    .unwrap_or_else(Applied::none)
            }
            Action::Panel(panel) => {
                self.palette.list = None;
                self.open_panel(panel)
            }
            Action::Setting(setting) => self.open_setting(setting),
        }
    }

    fn open_panel(&mut self, panel: Panel) -> Applied {
        match panel {
            Panel::Models => self.slash("/model").unwrap_or_else(Applied::none),
            Panel::Themes => {
                self.theme_picker = Some(ThemePicker::open());
                Applied::none()
            }
            Panel::Sessions => self.open_session_picker(),
            Panel::History => self.open_history(),
            Panel::Tree => self.open_tree(),
            Panel::Login => self.slash("/login").unwrap_or_else(Applied::none),
        }
    }

    /// Opens the list of words `/name` takes over the palette, so Esc goes
    /// back to it.
    fn open_values(&mut self, name: &'static str) {
        let Some(items) = command_values(self, name) else {
            return;
        };
        self.palette.values = Some(ValueList {
            target: Target::Command(name),
            items,
            query: String::new(),
            selected: 0,
            source: None,
        });
    }

    /// Enter on a setting: say what it is now and where that came from, then
    /// offer its words when it has a short list.
    fn open_setting(&mut self, setting: &'static Setting) -> Applied {
        let settings = match cfg::Settings::load(
            &self.agent_dir,
            &crate::session_fs::current_workspace(),
            &[],
        ) {
            Ok(settings) => settings,
            Err(reason) => {
                self.palette.list = None;
                self.push(
                    LineKind::Error,
                    format!("{}: config load said: {reason}", setting.key),
                );
                return Applied::none();
            }
        };
        let resolved = settings.resolve_source(setting.key);
        let now = resolved.as_ref().map(|(_, value)| value_word(value));
        let note = match &resolved {
            Some((source, value)) => {
                format!("{} = {} ({source})", setting.key, value_word(value))
            }
            None => format!("{}: not set · {}", setting.key, setting.about),
        };
        self.push(LineKind::Note, note);
        let Some(items) = setting_values(setting, now.as_deref()) else {
            self.palette.list = None;
            return Applied::none();
        };
        self.palette.values = Some(ValueList {
            target: Target::Setting(setting),
            items,
            query: String::new(),
            selected: 0,
            source: Some(
                resolved
                    .map(|(source, _)| source.to_owned())
                    .unwrap_or_else(|| "not set".to_owned()),
            ),
        });
        Applied::none()
    }

    /// Typing while a value list is up. The same idiom as the palette; Esc
    /// and Backspace on an empty query step back to the palette.
    pub(crate) fn values_key(&mut self, key: Key, now: Instant) -> Applied {
        let query_empty = self
            .palette
            .values
            .as_ref()
            .is_none_or(|list| list.query.is_empty());
        match key {
            Key::Up => {
                self.move_values(-1);
                Applied::none()
            }
            Key::Down => {
                self.move_values(1);
                Applied::none()
            }
            Key::Enter => self.accept_value(),
            Key::Esc => {
                self.palette.values = None;
                self.disarm();
                Applied::none()
            }
            Key::CtrlK => {
                self.palette.values = None;
                self.palette.list = None;
                self.disarm();
                Applied::none()
            }
            Key::Backspace if !query_empty => {
                if let Some(list) = self.palette.values.as_mut() {
                    list.query.pop();
                    list.selected = 0;
                }
                Applied::none()
            }
            Key::Backspace => {
                self.palette.values = None;
                Applied::none()
            }
            Key::Char(ch) if !ch.is_control() => {
                if let Some(list) = self.palette.values.as_mut() {
                    list.query.push(ch);
                    list.selected = 0;
                }
                Applied::none()
            }
            other => {
                self.palette.values = None;
                self.on_key(other, now)
            }
        }
    }

    fn move_values(&mut self, delta: isize) {
        let Some(list) = self.palette.values.as_mut() else {
            return;
        };
        let len = list.matched().len();
        if len == 0 {
            return;
        }
        let current = list.selected % len;
        list.selected = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Enter on a word: the command runs with it, or the setting is written.
    /// A list with nothing under the cursor stays open.
    fn accept_value(&mut self) -> Applied {
        let Some(list) = self.palette.values.as_ref() else {
            return Applied::none();
        };
        let Some(arg) = list.selected_arg().map(str::to_owned) else {
            return Applied::none();
        };
        let target = list.target;
        self.palette.values = None;
        self.palette.list = None;
        match target {
            Target::Command(name) => self
                .slash(&format!("/{name} {arg}"))
                .unwrap_or_else(Applied::none),
            Target::Setting(setting) => self.write_setting(setting, &arg),
        }
    }

    /// Writes one setting to the agent config. The status line preset goes
    /// through `/statusline`, which also applies it to the screen now; the
    /// rest are read when titi starts, and the note says so.
    fn write_setting(&mut self, setting: &'static Setting, word: &str) -> Applied {
        if setting.key == cfg::STATUS_LINE_PRESET_KEY {
            return self
                .slash(&format!("/statusline {word}"))
                .unwrap_or_else(Applied::none);
        }
        let mut settings = match cfg::Settings::load(
            &self.agent_dir,
            &crate::session_fs::current_workspace(),
            &[],
        ) {
            Ok(settings) => settings,
            Err(reason) => {
                self.push(
                    LineKind::Error,
                    format!("{}: config load said: {reason}", setting.key),
                );
                return Applied::none();
            }
        };
        let value = match setting.values {
            Values::Bool => serde_json::json!(word == "true"),
            _ => serde_json::json!(word),
        };
        match settings.set(setting.key, value) {
            Ok(()) => self.push(
                LineKind::Note,
                format!(
                    "{} = {word} · saved to the agent config · applies on the next launch",
                    setting.key
                ),
            ),
            Err(reason) => self.push(
                LineKind::Error,
                format!("{}: not saved ({reason})", setting.key),
            ),
        }
        Applied::none()
    }

    /// The words the draft's command takes, when the draft is `/command ` and
    /// whatever has been typed after it: the list follows the draft the way
    /// the `/` list follows the word, so a command typed out in full is still
    /// the draft's own. `None` when nothing is offered — another command, no
    /// space yet, a word no value fits, the list hidden with Esc, or the
    /// palette up.
    pub(crate) fn arg_rows(&self) -> Option<ArgRows> {
        if self.picker_hidden || self.palette.is_open() || self.caret() != self.input.len() {
            return None;
        }
        let (name, typed) = self.input.strip_prefix('/')?.split_once(' ')?;
        let command = VALUE_COMMANDS.iter().find(|command| **command == name)?;
        let typed = typed.trim_start();
        let mut scored: Vec<(i32, ArgValue)> = command_values(self, command)?
            .into_iter()
            .filter_map(|value| fuzzy_score(typed, &value.arg).map(|score| (score, value)))
            .collect();
        // A stable sort: values with the same score keep the list's order.
        scored.sort_by(|left, right| right.0.cmp(&left.0));
        let rows: Vec<ArgValue> = scored.into_iter().map(|(_, value)| value).collect();
        (!rows.is_empty()).then_some(ArgRows { command, rows })
    }

    /// The keys the argument list owns: the arrows move its highlight, Tab
    /// writes the highlighted word into the draft, Esc hides it. Enter is the
    /// draft's own — it sends what was typed — unless the highlight was moved
    /// and no other key has come since, in which case the highlighted word is
    /// the choice. Every other key is `None`: typing narrows the list through
    /// the draft, and forgets a highlight that was moved.
    pub(crate) fn arg_picker_key(&mut self, key: Key) -> Option<Applied> {
        if !matches!(key, Key::Up | Key::Down | Key::Tab | Key::Enter | Key::Esc) {
            self.palette.arg_moved = None;
            return None;
        }
        let found = self.arg_rows()?;
        let len = found.rows.len();
        let at = self.picker % len;
        match key {
            Key::Up | Key::Down => {
                let delta: isize = if key == Key::Up { -1 } else { 1 };
                self.picker = (at as isize + delta).rem_euclid(len as isize) as usize;
                self.palette.arg_moved = Some(self.input.clone());
                Some(Applied::none())
            }
            Key::Tab => {
                self.input = format!("/{} {}", found.command, found.rows[at].arg);
                self.caret_to_end();
                self.picker = 0;
                self.disarm();
                Some(Applied::none())
            }
            Key::Enter => {
                if self.palette.arg_moved.as_deref() != Some(self.input.as_str()) {
                    return None;
                }
                self.palette.arg_moved = None;
                self.clear_input();
                self.disarm();
                let line = format!("/{} {}", found.command, found.rows[at].arg);
                Some(self.slash(&line).unwrap_or_else(Applied::none))
            }
            _ => {
                self.picker_hidden = true;
                self.disarm();
                Some(Applied::none())
            }
        }
    }
}
