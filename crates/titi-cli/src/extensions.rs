//! Skills and file commands on the screen: the rows `/` and Ctrl+K offer, and
//! the `/skills` panel that says what was found, where, and why anything was
//! left out.
//!
//! Discovery is the engine's (`titi_engine::skills`, `titi_engine::commands`),
//! so the `/` list names exactly what `/name` can expand. This module only
//! turns the engine's listing into rows. Nothing here executes a file.

use std::cell::Cell;
use std::path::Path;
use std::time::Instant;

use titi_engine::commands::{self, Command};
use titi_engine::skills::{self, Skill};
use titi_engine::sources::Sources;

use crate::chat::{Applied, Chat, Key, SkillRow};
use crate::composer::{one_line, wrap_plain};
use crate::pickers::{COMMANDS, PanelLine, PanelView, panel_body, panel_label_room, panel_view};
use crate::transcript::LineKind;

/// A file command the composer can complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandRow {
    pub(crate) name: String,
    pub(crate) about: String,
    /// The `argument-hint`, or empty.
    pub(crate) hint: String,
}

/// What discovery should read for this session.
pub(crate) fn sources(
    agent_dir: &Path,
    workspace: &Path,
    settings: Option<&titi_config::settings::Settings>,
) -> Sources {
    Sources {
        cwd: Some(workspace.to_path_buf()),
        agent_dir: Some(agent_dir.to_path_buf()),
        home: std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(Into::into),
        foreign_user_dirs: settings.is_some_and(titi_config::settings::foreign_user_dirs),
    }
}

/// The names the screen answers itself. A file command of one of these never
/// runs.
pub(crate) fn reserved_commands() -> Vec<String> {
    COMMANDS
        .iter()
        .map(|command| command.name.to_owned())
        .collect()
}

/// Skills a person may type: `user-invocable: false` stays off the list.
pub(crate) fn skill_rows(sources: &Sources) -> Vec<SkillRow> {
    skills::catalog(sources)
        .into_iter()
        .filter(|skill| skill.user_invocable)
        .map(|skill| SkillRow {
            name: skill.name,
            about: skill.description,
        })
        .collect()
}

pub(crate) fn command_rows(sources: &Sources) -> Vec<CommandRow> {
    commands::list(sources, &reserved_commands())
        .commands
        .into_iter()
        .map(|command| CommandRow {
            name: command.name,
            about: command.description,
            hint: command.argument_hint.unwrap_or_default(),
        })
        .collect()
}

/// One line of the `/skills` panel and the text behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtensionRow {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) source: String,
    pub(crate) status: String,
    pub(crate) detail: Vec<String>,
}

impl ExtensionRow {
    fn line(&self) -> String {
        format!(
            "{:<18} {:<8} {:<28} {}",
            self.name,
            self.kind,
            short_dir(&self.source, 28),
            self.status
        )
    }
}

/// Every skill and command discovery saw, with its status: the ones in use
/// first, then the shadowed, then the refused.
pub(crate) fn extension_rows(sources: &Sources, reserved: &[String]) -> Vec<ExtensionRow> {
    let skills = skills::list(sources);
    let commands = commands::list(sources, reserved);
    let mut rows = Vec::new();
    for skill in &skills.skills {
        rows.push(skill_row(skill, active_status(skill)));
    }
    for command in &commands.commands {
        rows.push(command_row(command, command_status(command)));
    }
    for shadowed in &skills.shadowed {
        let status = format!("shadowed by {}", shadowed.by);
        rows.push(skill_row(&shadowed.skill, status));
    }
    for shadowed in &commands.shadowed {
        let status = format!("shadowed by {}", shadowed.by);
        rows.push(command_row(&shadowed.command, status));
    }
    for (kind, refused) in skills
        .refused
        .iter()
        .map(|refused| ("skill", refused))
        .chain(commands.refused.iter().map(|refused| ("command", refused)))
    {
        rows.push(ExtensionRow {
            name: refused.name.clone(),
            kind,
            source: refused.origin.label.clone(),
            status: format!("refused: {}", refused.reason),
            detail: vec![
                format!("{} · {kind}", refused.name),
                format!("source: {}", refused.origin.label),
                format!("refused: {}", refused.reason),
            ],
        });
    }
    rows
}

fn active_status(skill: &Skill) -> String {
    let mut parts = vec!["active".to_owned()];
    if skill.disable_model_invocation {
        parts.push("hidden from the model".to_owned());
    }
    if !skill.user_invocable {
        parts.push("hidden from /".to_owned());
    }
    flags(&mut parts, &skill.ignored, &skill.notes);
    parts.join(" · ")
}

fn command_status(command: &Command) -> String {
    let mut parts = vec!["active".to_owned()];
    flags(&mut parts, &command.ignored, &command.notes);
    parts.join(" · ")
}

fn flags(parts: &mut Vec<String>, ignored: &[String], notes: &[String]) {
    if !ignored.is_empty() {
        parts.push(format!("ignored: {}", ignored.join(", ")));
    }
    if let Some(note) = notes.first() {
        parts.push(format!("note: {note}"));
    }
}

fn skill_row(skill: &Skill, status: String) -> ExtensionRow {
    let mut detail = vec![
        format!("{} · skill", skill.name),
        format!("file: {}", skill.dir().join("SKILL.md").display()),
        format!("source: {}", skill.origin.label),
        format!("status: {status}"),
    ];
    if !skill.description.is_empty() {
        detail.push(format!("description: {}", skill.description));
    }
    if let Some(hint) = &skill.argument_hint {
        detail.push(format!("argument hint: {hint}"));
    }
    if let Some(license) = &skill.license {
        detail.push(format!("license: {license}"));
    }
    if let Some(compatibility) = &skill.compatibility {
        detail.push(format!("compatibility: {compatibility}"));
    }
    for (key, value) in &skill.metadata {
        detail.push(format!("metadata {key}: {value}"));
    }
    detail_notes(&mut detail, &skill.notes, &skill.ignored);
    ExtensionRow {
        name: skill.name.clone(),
        kind: "skill",
        source: skill.origin.label.clone(),
        status,
        detail,
    }
}

fn command_row(command: &Command, status: String) -> ExtensionRow {
    let mut detail = vec![
        format!("/{} · command", command.name),
        format!("file: {}", command.path().display()),
        format!("source: {}", command.origin.label),
        format!("status: {status}"),
    ];
    if !command.description.is_empty() {
        detail.push(format!("description: {}", command.description));
    }
    if let Some(hint) = &command.argument_hint {
        detail.push(format!("argument hint: {hint}"));
    }
    if command.disable_model_invocation {
        detail.push(
            "disable-model-invocation: true (commands are never offered to the model)".to_owned(),
        );
    }
    detail_notes(&mut detail, &command.notes, &command.ignored);
    ExtensionRow {
        name: command.name.clone(),
        kind: "command",
        source: command.origin.label.clone(),
        status,
        detail,
    }
}

fn detail_notes(detail: &mut Vec<String>, notes: &[String], ignored: &[String]) {
    for note in notes {
        detail.push(format!("note: {note}"));
    }
    if !ignored.is_empty() {
        detail.push(format!("ignored, never honoured: {}", ignored.join(", ")));
    }
}

/// A directory label cut from the left to `max` characters: the end of a
/// path says which directory it is.
fn short_dir(label: &str, max: usize) -> String {
    let count = label.chars().count();
    if count <= max {
        return label.to_owned();
    }
    let tail: String = label.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

/// The panel's cursor: a row, or the whole text behind one.
#[derive(Debug, Clone)]
pub(crate) struct ExtensionsPicker {
    pub(crate) rows: Vec<ExtensionRow>,
    pub(crate) selected: usize,
    pub(crate) detail: bool,
    pub(crate) scroll: usize,
    drawn_lines: Cell<usize>,
}

impl Chat {
    /// Re-read what `/` and Ctrl+K offer from disk.
    pub(crate) fn refresh_extensions(&mut self) {
        self.sources_now();
    }

    /// Discover again and keep the rows the `/` list and the palette offer in
    /// step with the disk. Returns what it read. The workspace is the
    /// screen's own, so a test can point it at a temporary directory.
    fn sources_now(&mut self) -> Sources {
        let settings =
            titi_config::settings::Settings::load(&self.agent_dir, &self.workspace, &[]).ok();
        let sources = sources(&self.agent_dir, &self.workspace, settings.as_ref());
        self.skills = skill_rows(&sources);
        self.commands = command_rows(&sources);
        sources
    }

    /// `/skills`: open the panel over every skill and command found.
    pub(crate) fn skills_command(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Note, format!("usage: /skills (got {args})"));
            return Applied::none();
        }
        let sources = self.sources_now();
        let rows = extension_rows(&sources, &reserved_commands());
        if rows.is_empty() {
            let looked = [
                ".titi/skills",
                ".agents/skills",
                ".claude/skills",
                ".titi/commands",
                ".agents/commands",
                ".claude/commands",
            ]
            .join(", ");
            self.push(
                LineKind::Note,
                format!("skills: none found (looked in {looked}, and the agent directory)"),
            );
            return Applied::none();
        }
        self.extensions = Some(ExtensionsPicker {
            rows,
            selected: 0,
            detail: false,
            scroll: 0,
            drawn_lines: Cell::new(0),
        });
        Applied::none()
    }

    /// A key while the panel is up: arrows move, Enter opens or closes the
    /// detail, Esc closes. Any other key closes the panel and does its work.
    pub(crate) fn extensions_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => self.move_extensions_cursor(-1),
            Key::Down => self.move_extensions_cursor(1),
            Key::Enter => {
                if let Some(picker) = self.extensions.as_mut() {
                    picker.detail = !picker.detail;
                    picker.scroll = 0;
                }
            }
            Key::Esc => {
                self.extensions = None;
                self.disarm();
            }
            other => {
                self.extensions = None;
                return self.on_key(other, now);
            }
        }
        Applied::none()
    }

    fn move_extensions_cursor(&mut self, delta: isize) {
        let Some(picker) = self.extensions.as_mut() else {
            return;
        };
        if picker.detail {
            let last = picker.drawn_lines.get().saturating_sub(1);
            picker.scroll = picker.scroll.saturating_add_signed(delta).min(last);
        } else if !picker.rows.is_empty() {
            let len = picker.rows.len() as isize;
            picker.selected = (picker.selected as isize + delta).rem_euclid(len) as usize;
        }
    }
}

/// The `/skills` panel: a row per skill or command, or the detail of one.
pub(crate) fn extensions_panel(chat: &Chat, total: u16, width: u16) -> PanelView {
    let Some(picker) = chat.extensions.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let count = picker.rows.len();
    let selected = picker.selected.min(count.saturating_sub(1));
    let room = panel_label_room(width).saturating_sub(3);
    if picker.detail
        && let Some(row) = picker.rows.get(selected)
    {
        let lines: Vec<PanelLine> = row
            .detail
            .iter()
            .flat_map(|line| wrap_plain(line, room))
            .map(|text| PanelLine::Row {
                text,
                accent: false,
            })
            .collect();
        picker.drawn_lines.set(lines.len());
        let cursor = picker.scroll.min(lines.len().saturating_sub(1));
        return panel_view(
            Some(format!("{} {} of {count}", row.kind, selected + 1)),
            lines,
            Some(cursor),
            panel_body(total),
        );
    }
    let lines: Vec<PanelLine> = picker
        .rows
        .iter()
        .map(|row| PanelLine::Row {
            text: one_line(&row.line(), room),
            accent: false,
        })
        .collect();
    panel_view(
        Some(format!("skills and commands · {count}")),
        lines,
        Some(selected),
        panel_body(total),
    )
}
