//! Full-screen chat.
//!
//! The state machine does not touch the terminal, so tests drive it with
//! keys and engine events. [`run`] is the only place that owns the screen.

use std::collections::HashSet;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use titi_core::session::Role;
use titi_engine::{ContextPart, Engine, EngineCommand, EngineEvent};
use tokio::sync::mpsc::error::TryRecvError;

use crate::herdr::{self, AgentState};
use crate::session_log::SessionLog;

const QUIT_WINDOW: Duration = Duration::from_secs(2);
const TOOL_PREVIEW: usize = 120;

/// One key the state machine understands. The terminal loop translates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    CtrlC,
    CtrlD,
    Esc,
    Up,
    Down,
    Tab,
}

/// What the screen asks the engine or the process to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEffect {
    Send(EngineCommand),
    Quit,
}

/// One conversation entry the screen hands to the session file.
///
/// A tool round is three entries — the call, its output, the answer — so the
/// role alone no longer says what to write.
#[derive(Debug, Clone, PartialEq)]
pub struct LogWrite {
    pub role: Role,
    pub text: String,
    /// Tool calls this assistant message issued, if any.
    pub tool_calls: Vec<titi_providers::ToolCallRef>,
}

impl LogWrite {
    fn text(role: Role, text: String) -> Self {
        Self {
            role,
            text,
            tool_calls: Vec::new(),
        }
    }
}

/// A command for the engine, plus the transcript line that should be stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub effect: Option<ChatEffect>,
    pub log: Option<LogWrite>,
}

impl Applied {
    fn none() -> Self {
        Self {
            effect: None,
            log: None,
        }
    }

    fn effect(effect: ChatEffect) -> Self {
        Self {
            effect: Some(effect),
            log: None,
        }
    }

    fn send(command: EngineCommand, log: Option<LogWrite>) -> Self {
        Self {
            effect: Some(ChatEffect::Send(command)),
            log,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    User,
    Assistant,
    Tool,
    Error,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptLine {
    kind: LineKind,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingApproval {
    call_id: String,
    name: String,
}

/// Conversation on screen. No terminal, no session file.
pub struct Chat {
    lines: Vec<TranscriptLine>,
    input: String,
    turn_active: bool,
    model: String,
    models: Vec<String>,
    session_id: String,
    session_label: String,
    agent_dir: PathBuf,
    paused: bool,
    context_percent: Option<u8>,
    reply: String,
    /// Bytes of `reply` already written to the session file. A tool call
    /// splits the turn's text into segments, and each is recorded once.
    recorded_reply: usize,
    thinking: String,
    assistant_at: Option<usize>,
    thinking_at: Option<usize>,
    approval: Option<PendingApproval>,
    quit_armed: Option<Instant>,
    hint: String,
    /// Provider waiting for a key. The composer masks whatever is typed.
    login_for: Option<String>,
    /// Highlight in the leading-slash command list.
    picker: usize,
    /// Skills the engine discovered, offered by the same picker.
    skills: Vec<SkillRow>,
    /// Kitty or Ghostty unicode placeholders are available.
    kitty: bool,
    tmux: bool,
    photos: Vec<Photo>,
    misses: HashSet<String>,
    next_image_id: u32,
    /// Transmit and placement sequences to write before the next frame.
    kitty_flush: String,
}

impl Chat {
    pub fn new(model: impl Into<String>, session_id: &str) -> Self {
        let model = model.into();
        Self {
            lines: Vec::new(),
            input: String::new(),
            turn_active: false,
            model: model.clone(),
            models: vec![model],
            session_id: session_id.to_owned(),
            session_label: short_session(session_id),
            agent_dir: titi_config::agent_dir(),
            paused: false,
            context_percent: None,
            reply: String::new(),
            recorded_reply: 0,
            thinking: String::new(),
            assistant_at: None,
            thinking_at: None,
            approval: None,
            quit_armed: None,
            hint: String::new(),
            login_for: None,
            picker: 0,
            skills: Vec::new(),
            kitty: false,
            tmux: false,
            photos: Vec::new(),
            misses: HashSet::new(),
            next_image_id: 1,
            kitty_flush: String::new(),
        }
    }

    fn take_kitty_flush(&mut self) -> String {
        std::mem::take(&mut self.kitty_flush)
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

    pub fn on_key(&mut self, key: Key, now: Instant) -> Applied {
        if self.approval.is_some() {
            return self.approval_key(key);
        }
        if self.login_for.is_some() {
            return self.login_key(key);
        }
        match key {
            Key::CtrlC if self.turn_active => {
                self.disarm();
                Applied::effect(ChatEffect::Send(EngineCommand::Cancel))
            }
            Key::CtrlC => self.arm_quit(now),
            Key::CtrlD if self.input.is_empty() => Applied::effect(ChatEffect::Quit),
            Key::Up if self.picking() => {
                self.move_picker(-1);
                Applied::none()
            }
            Key::Down if self.picking() => {
                self.move_picker(1);
                Applied::none()
            }
            Key::Tab if self.picking() => {
                self.accept_picker();
                Applied::none()
            }
            Key::Enter => {
                let token = slash_token(&self.input).map(|(start, name)| (start, name.to_owned()));
                if let Some((start, name)) = token {
                    let at_line_start = self.input[..start].trim().is_empty();
                    if at_line_start && name.is_empty() {
                        return Applied::none();
                    }
                    let rows = picker_rows(self);
                    let exact = rows.iter().any(|row| self.row_name(row) == name);
                    if !exact && !rows.is_empty() {
                        self.accept_picker();
                        // Mid-sentence the message is not finished: complete
                        // the token and let the next Enter send it.
                        if !at_line_start {
                            return Applied::none();
                        }
                    }
                }
                self.submit()
            }
            Key::Backspace => {
                self.disarm();
                self.input.pop();
                self.picker = 0;
                Applied::none()
            }
            Key::Char(ch) => {
                self.disarm();
                self.input.push(ch);
                self.picker = 0;
                Applied::none()
            }
            Key::Esc if self.picking() => {
                self.input.clear();
                self.picker = 0;
                self.disarm();
                Applied::none()
            }
            Key::Esc | Key::CtrlD | Key::Up | Key::Down | Key::Tab => {
                self.disarm();
                Applied::none()
            }
        }
    }

    pub fn on_event(&mut self, event: EngineEvent) -> Applied {
        match event {
            EngineEvent::TurnStarted { model, .. } => {
                self.turn_active = true;
                self.model = model.to_string();
                self.reply.clear();
                self.recorded_reply = 0;
                self.assistant_at = None;
                self.drop_thinking();
                Applied::none()
            }
            EngineEvent::StreamDelta { text, .. } => {
                self.drop_thinking();
                self.reply.push_str(&text);
                self.show_reply();
                Applied::none()
            }
            EngineEvent::ThinkingDelta { text, .. } if self.reply.is_empty() => {
                self.thinking.push_str(&text);
                self.show_thinking();
                Applied::none()
            }
            EngineEvent::ToolStarted { call_id, name, .. } => {
                self.push(LineKind::Tool, format!("tool {name}"));
                // The call goes to the session file now, not at the end of
                // the turn: the result below it has to follow its own call,
                // or a restore replays an orphan.
                Applied {
                    effect: None,
                    log: Some(LogWrite {
                        role: Role::Assistant,
                        text: self.unrecorded_reply(),
                        tool_calls: vec![titi_providers::ToolCallRef { call_id, name }],
                    }),
                }
            }
            EngineEvent::ToolApprovalNeeded { call_id, name, .. } => {
                self.approval = Some(PendingApproval {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                });
                self.hint.clear();
                Applied::none()
            }
            EngineEvent::ToolFinished {
                output, is_error, ..
            } => {
                let preview = one_line(&output, TOOL_PREVIEW);
                let text = if is_error {
                    format!("tool error  {preview}")
                } else if preview.is_empty() {
                    "tool done".to_owned()
                } else {
                    format!("tool done  {preview}")
                };
                let kind = if is_error {
                    LineKind::Error
                } else {
                    LineKind::Tool
                };
                self.push(kind, text);
                // `output` is what the engine masked before it emitted the
                // event, so no secret reaches the session file here.
                Applied {
                    effect: None,
                    log: Some(LogWrite::text(Role::Tool, output.to_string())),
                }
            }
            EngineEvent::ContextUsage { tokens, window, .. } if window > 0 => {
                let percent = tokens.saturating_mul(100) / window;
                self.context_percent = Some(u8::try_from(percent.min(100)).unwrap_or(100));
                Applied::none()
            }
            EngineEvent::ModelSwitched { to, .. } => {
                self.model = to.to_string();
                self.push(LineKind::Note, format!("model {to}"));
                Applied::none()
            }
            EngineEvent::Compacted { folded, .. } => {
                self.push(LineKind::Note, format!("folded {folded} earlier messages"));
                Applied::none()
            }
            EngineEvent::ContextBreakdown { parts, window } => {
                self.show_context(&parts, window);
                Applied::none()
            }
            EngineEvent::Failed { message, .. } => {
                self.push(LineKind::Error, one_line(&message, TOOL_PREVIEW));
                self.finish_turn()
            }
            EngineEvent::Cancelled { .. } => {
                self.push(LineKind::Note, "cancelled".to_owned());
                self.finish_turn()
            }
            EngineEvent::TurnFinished { .. } => self.finish_turn(),
            EngineEvent::GoalFinished { report } => {
                self.push(LineKind::Note, report.to_string());
                Applied::none()
            }
            EngineEvent::Notice { message } => {
                self.push(LineKind::Note, one_line(&message, TOOL_PREVIEW));
                Applied::none()
            }
            EngineEvent::PromptReturned { text } => {
                // The cancel stopped this prompt before it ran. Dropping it
                // loses what the user typed; pasting it over a composer they
                // have already started filling loses that instead. So the
                // composer takes it only when it is empty, and the transcript
                // records it either way — the text is never gone silently.
                let preview = one_line(&text, TOOL_PREVIEW);
                if self.input.is_empty() && self.approval.is_none() {
                    self.input = text.to_string();
                    self.picker = 0;
                    self.push(
                        LineKind::Note,
                        format!("not sent, back in the composer: {preview}"),
                    );
                } else {
                    self.push(LineKind::Note, format!("not sent: {preview}"));
                }
                Applied::none()
            }
            _ => Applied::none(),
        }
    }

    /// Insert pasted text into the composer. Newlines become spaces.
    pub fn paste(&mut self, text: &str) {
        if self.approval.is_some() {
            return;
        }
        self.disarm();
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                if !self.input.ends_with(' ') {
                    self.input.push(' ');
                }
            } else if !ch.is_control() {
                self.input.push(ch);
            }
        }
    }

    fn approval_key(&mut self, key: Key) -> Applied {
        let Some(pending) = self.approval.clone() else {
            return Applied::none();
        };
        match key {
            Key::Char('y') | Key::Char('Y') | Key::Enter => {
                self.approval = None;
                Applied::effect(ChatEffect::Send(EngineCommand::ApproveTool {
                    call_id: pending.call_id.into(),
                    approved: true,
                }))
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                self.approval = None;
                Applied::effect(ChatEffect::Send(EngineCommand::ApproveTool {
                    call_id: pending.call_id.into(),
                    approved: false,
                }))
            }
            Key::CtrlC => {
                self.approval = None;
                self.disarm();
                Applied::effect(ChatEffect::Send(EngineCommand::Cancel))
            }
            _ => Applied::none(),
        }
    }

    fn submit(&mut self) -> Applied {
        let text = self.input.trim().to_owned();
        if text.is_empty() {
            return Applied::none();
        }
        if let Some(applied) = self.slash(&text) {
            self.input.clear();
            self.disarm();
            return applied;
        }
        if self.paused {
            self.input.clear();
            self.disarm();
            self.push(LineKind::Note, "paused · /pause resumes".to_owned());
            return Applied::none();
        }
        self.input.clear();
        self.disarm();
        self.push(LineKind::User, text.clone());
        let log = Some(LogWrite::text(Role::User, text.clone()));
        if self.turn_active {
            Applied::send(EngineCommand::Steer { text: text.into() }, log)
        } else {
            self.turn_active = true;
            Applied::send(EngineCommand::SubmitPrompt { text: text.into() }, log)
        }
    }

    fn switch_model(&mut self, text: &str) -> Option<Applied> {
        let Some(raw) = text.strip_prefix("/model") else {
            return None;
        };
        if !raw.is_empty() && !raw.starts_with(char::is_whitespace) {
            return None;
        }
        let rest = raw.trim();
        if self.models.is_empty() {
            self.push(LineKind::Error, "no models".to_owned());
            return Some(Applied::none());
        }
        let next = if rest.is_empty() {
            let index = self
                .models
                .iter()
                .position(|id| id == &self.model)
                .unwrap_or(0);
            self.models[(index + 1) % self.models.len()].clone()
        } else if let Some(found) = self
            .models
            .iter()
            .find(|id| id.as_str() == rest || id.rsplit('/').next() == Some(rest))
        {
            found.clone()
        } else {
            self.push(LineKind::Error, format!("unknown model {rest}"));
            return Some(Applied::none());
        };
        self.model = next.clone();
        self.push(LineKind::Note, format!("model {next}"));
        Some(Applied::send(
            EngineCommand::SwitchModel { model: next.into() },
            None,
        ))
    }

    /// A leading slash is a command when the first word has no extra slash.
    /// `/tmp/photo.png` stays a prompt, because that is a path.
    fn slash(&mut self, text: &str) -> Option<Applied> {
        if let Some(applied) = self.switch_model(text) {
            return Some(applied);
        }
        let Some(raw) = text.strip_prefix('/') else {
            return None;
        };
        if raw.is_empty() {
            return None;
        }
        let (name, args) = raw
            .split_once(char::is_whitespace)
            .map(|(name, args)| (name, args.trim()))
            .unwrap_or((raw, ""));
        if name.contains('/')
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return None;
        }
        let applied = match name {
            "checkpoint" => self.session_note(crate::app::checkpoint_session(
                &self.agent_dir,
                &crate::app::current_workspace(),
                &self.session_id,
            )),
            "checkpoints" => self.session_note(crate::app::list_checkpoints(
                &self.agent_dir,
                &self.session_id,
            )),
            "rewind" => self.rewind(args),
            "recap" => self.recap(),
            "pause" => self.toggle_pause(),
            "goal" => self.goal(args),
            "context" => self.describe_context(args),
            "compact" => self.compact(args),
            "help" => self.help(),
            "login" => self.login(args),
            "logout" => self.logout(args),
            "keys" | "whoami" => self.keys(),
            _ => {
                // A known skill is not a command: it goes to the model as a
                // prompt, and the engine expands it there.
                if self.skills.iter().any(|skill| skill.name == name) {
                    return None;
                }
                self.push(LineKind::Error, format!("unknown command /{name}"));
                Applied::none()
            }
        };
        Some(applied)
    }

    fn session_note(&mut self, result: Result<String, String>) -> Applied {
        match result {
            Ok(summary) => self.push(LineKind::Note, summary),
            Err(reason) => self.push(LineKind::Error, reason),
        }
        Applied::none()
    }

    fn rewind(&mut self, args: &str) -> Applied {
        let index = match args {
            "" => Ok(None),
            other => other
                .parse::<usize>()
                .map(Some)
                .map_err(|_| format!("usage: /rewind [n] (got {other})")),
        };
        let Ok(index) = index else {
            self.push(
                LineKind::Error,
                index.err().unwrap_or_else(|| "rewind".into()),
            );
            return Applied::none();
        };
        match crate::app::rewind_session(
            &self.agent_dir,
            &crate::app::current_workspace(),
            &self.session_id,
            index,
        ) {
            Ok(summary) => match crate::app::session_history(&self.agent_dir, &self.session_id) {
                Ok(messages) => {
                    self.show_history(&messages);
                    self.turn_active = false;
                    self.approval = None;
                    self.push(LineKind::Note, summary);
                    Applied::send(EngineCommand::RestoreHistory { messages }, None)
                }
                Err(reason) => {
                    self.push(
                        LineKind::Error,
                        format!("rewind: history not restored ({reason})"),
                    );
                    Applied::none()
                }
            },
            Err(reason) => {
                self.push(LineKind::Error, format!("rewind: {reason}"));
                Applied::none()
            }
        }
    }

    fn show_history(&mut self, messages: &[titi_providers::ChatMessage]) {
        self.lines.clear();
        self.assistant_at = None;
        self.thinking_at = None;
        self.reply.clear();
        self.thinking.clear();
        for message in messages {
            let kind = match message.role {
                titi_providers::Role::User => LineKind::User,
                titi_providers::Role::Assistant => LineKind::Assistant,
                titi_providers::Role::System | titi_providers::Role::Tool => continue,
            };
            let text = message.content.trim();
            if text.is_empty() {
                continue;
            }
            self.push(kind, text.to_owned());
        }
    }

    fn recap(&mut self) -> Applied {
        match crate::recap::build(&self.agent_dir, &self.session_id) {
            Ok(sections) => {
                if sections.is_empty() {
                    self.push(LineKind::Note, "recap: empty".to_owned());
                }
                for section in sections {
                    self.push(
                        LineKind::Note,
                        format!("{} · {}", section.title, section.summary),
                    );
                }
            }
            Err(reason) => self.push(LineKind::Error, format!("recap: {reason}")),
        }
        Applied::none()
    }

    fn toggle_pause(&mut self) -> Applied {
        self.paused = !self.paused;
        if self.paused {
            self.push(LineKind::Note, "paused · /pause resumes".to_owned());
            if self.turn_active {
                return Applied::effect(ChatEffect::Send(EngineCommand::Cancel));
            }
        } else {
            self.push(LineKind::Note, "resumed".to_owned());
        }
        Applied::none()
    }

    fn help(&mut self) -> Applied {
        for command in COMMANDS {
            self.push(
                LineKind::Note,
                format!("/{}  {}", command.name, command.about),
            );
        }
        Applied::none()
    }

    fn row_name(&self, row: &PickRow) -> &str {
        match row {
            PickRow::Command(command) => command.name,
            PickRow::Skill(index) => self
                .skills
                .get(*index)
                .map(|skill| skill.name.as_str())
                .unwrap_or_default(),
        }
    }

    fn picking(&self) -> bool {
        !picker_rows(self).is_empty()
    }

    fn move_picker(&mut self, delta: isize) {
        let len = picker_rows(self).len();
        if len == 0 {
            return;
        }
        let current = self.picker % len;
        self.picker = (current as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Replace the token being typed with the highlighted name. Everything
    /// before it stays, so a skill named mid-sentence keeps its sentence.
    fn accept_picker(&mut self) {
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

    fn login_key(&mut self, key: Key) -> Applied {
        match key {
            Key::Char(ch) if !ch.is_control() => {
                self.input.push(ch);
                Applied::none()
            }
            Key::Backspace => {
                self.input.pop();
                Applied::none()
            }
            Key::Enter => self.store_login_key(),
            Key::Esc | Key::CtrlC => {
                self.input.clear();
                self.login_for = None;
                self.push(LineKind::Note, "login cancelled".to_owned());
                Applied::none()
            }
            _ => Applied::none(),
        }
    }

    fn store_login_key(&mut self) -> Applied {
        let secret = std::mem::take(&mut self.input);
        let secret = secret.trim().to_owned();
        let Some(provider) = self.login_for.take() else {
            return Applied::none();
        };
        if secret.is_empty() {
            self.push(LineKind::Error, "login: a key is required".to_owned());
            return Applied::none();
        }
        match crate::secrets::store_key(&self.agent_dir, &provider, &secret) {
            Ok(()) => self.push(LineKind::Note, format!("{provider}: key stored")),
            Err(reason) => self.push(LineKind::Error, format!("login: {reason}")),
        }
        Applied::none()
    }

    fn login(&mut self, args: &str) -> Applied {
        let mut parts = args.split_whitespace();
        let Some(provider) = parts.next() else {
            return self.keys();
        };
        let inline = parts.next();
        if parts.next().is_some() {
            self.push(LineKind::Error, "usage: /login <provider> [key]".to_owned());
            return Applied::none();
        }
        if !known_provider(provider) {
            self.push(
                LineKind::Error,
                format!("login: unknown provider {provider}"),
            );
            return Applied::none();
        }
        if let Some(secret) = inline {
            return self.store_inline_key(provider, secret);
        }
        self.login_for = Some(provider.to_owned());
        self.push(
            LineKind::Note,
            format!("login {provider}: paste the key, enter stores it"),
        );
        Applied::none()
    }

    fn store_inline_key(&mut self, provider: &str, secret: &str) -> Applied {
        match crate::secrets::store_key(&self.agent_dir, provider, secret) {
            Ok(()) => self.push(LineKind::Note, format!("{provider}: key stored")),
            Err(reason) => self.push(LineKind::Error, format!("login: {reason}")),
        }
        Applied::none()
    }

    fn logout(&mut self, args: &str) -> Applied {
        let mut parts = args.split_whitespace();
        let Some(provider) = parts.next() else {
            self.push(LineKind::Error, "usage: /logout <provider>".to_owned());
            return Applied::none();
        };
        if parts.next().is_some() {
            self.push(LineKind::Error, "usage: /logout <provider>".to_owned());
            return Applied::none();
        }
        if !known_provider(provider) {
            self.push(
                LineKind::Error,
                format!("logout: unknown provider {provider}"),
            );
            return Applied::none();
        }
        match crate::secrets::remove_key(&self.agent_dir, provider) {
            Ok(true) => self.push(LineKind::Note, format!("{provider}: signed out")),
            Ok(false) => self.push(LineKind::Note, format!("{provider}: no stored key")),
            Err(reason) => self.push(LineKind::Error, format!("logout: {reason}")),
        }
        Applied::none()
    }

    fn keys(&mut self) -> Applied {
        let stored = crate::secrets::list_keys(&self.agent_dir)
            .map(|rows| rows.into_iter().map(|row| row.provider).collect::<Vec<_>>())
            .unwrap_or_default();
        for provider in crate::engine::default_registry_config().providers {
            let status = if provider
                .credential_env
                .as_deref()
                .and_then(|name| std::env::var(name).ok())
                .is_some_and(|value| !value.trim().is_empty())
            {
                "env"
            } else if stored.iter().any(|id| id == provider.id.as_str()) {
                "stored"
            } else {
                "no key"
            };
            self.push(LineKind::Note, format!("{}  {status}", provider.id));
        }
        Applied::none()
    }

    /// `/goal` runs the coder/reviewer loop. It never becomes `SubmitPrompt`.
    fn goal(&mut self, args: &str) -> Applied {
        let text = args.trim();
        if text.is_empty() {
            self.push(LineKind::Error, "usage: /goal <text>".to_owned());
            return Applied::none();
        }
        self.push(LineKind::Note, format!("goal: {text}"));
        Applied::effect(ChatEffect::Send(EngineCommand::RunGoal {
            text: text.into(),
        }))
    }

    /// `/context` asks the engine what fills the window. It takes no
    /// argument: the breakdown is the whole answer, and quietly ignoring a
    /// stray word would hide the typo behind a plausible screen.
    fn describe_context(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /context (got {args})"));
            return Applied::none();
        }
        Applied::effect(ChatEffect::Send(EngineCommand::DescribeContext))
    }

    /// `/compact [focus]` folds the history now instead of waiting for the
    /// threshold. The focus is free text: it biases what the digest keeps.
    fn compact(&mut self, args: &str) -> Applied {
        let focus = args.trim();
        let note = if focus.is_empty() {
            "compacting the context".to_owned()
        } else {
            format!("compacting the context · focus: {focus}")
        };
        self.push(LineKind::Note, note);
        Applied::effect(ChatEffect::Send(EngineCommand::Compact {
            focus: (!focus.is_empty()).then(|| focus.into()),
        }))
    }

    /// The context breakdown, part by part. The share is of what is in the
    /// window now, so the parts add up to the total on the last line, and
    /// that total is what is reported against the window.
    fn show_context(&mut self, parts: &[ContextPart], window: u64) {
        let total: u64 = parts.iter().map(|part| part.tokens).sum();
        let width = parts
            .iter()
            .map(|part| part.label.chars().count())
            .max()
            .unwrap_or(0);
        self.push(
            LineKind::Note,
            "context · token estimates, not provider counts".to_owned(),
        );
        for part in parts {
            let label = &part.label;
            let pad = width.saturating_sub(label.chars().count());
            self.push(
                LineKind::Note,
                format!(
                    "{label}{:pad$}  {} tokens · {}%",
                    "",
                    part.tokens,
                    share(part.tokens, total)
                ),
            );
        }
        let pad = width.saturating_sub("total".chars().count());
        self.push(
            LineKind::Note,
            format!(
                "total{:pad$}  {total} tokens · {}% of {window}",
                "",
                share(total, window)
            ),
        );
    }

    fn arm_quit(&mut self, now: Instant) -> Applied {
        if let Some(armed) = self.quit_armed
            && now.saturating_duration_since(armed) <= QUIT_WINDOW
        {
            return Applied::effect(ChatEffect::Quit);
        }
        self.quit_armed = Some(now);
        self.hint = "ctrl-c again to quit".to_owned();
        Applied::none()
    }

    fn disarm(&mut self) {
        self.quit_armed = None;
        self.hint.clear();
    }

    /// The reply text streamed since the last entry written for this turn.
    fn unrecorded_reply(&mut self) -> String {
        let text = self
            .reply
            .get(self.recorded_reply..)
            .unwrap_or_default()
            .to_owned();
        self.recorded_reply = self.reply.len();
        text
    }

    fn finish_turn(&mut self) -> Applied {
        let reply = self.unrecorded_reply();
        self.reply.clear();
        self.recorded_reply = 0;
        self.turn_active = false;
        self.approval = None;
        self.assistant_at = None;
        self.drop_thinking();
        if reply.trim().is_empty() {
            Applied::none()
        } else {
            Applied {
                effect: None,
                log: Some(LogWrite::text(Role::Assistant, reply)),
            }
        }
    }

    fn show_reply(&mut self) {
        let text = self.reply.clone();
        if let Some(at) = self.assistant_at
            && let Some(line) = self.lines.get_mut(at)
        {
            line.text = text;
            return;
        }
        self.lines.push(TranscriptLine {
            kind: LineKind::Assistant,
            text,
        });
        self.assistant_at = Some(self.lines.len() - 1);
    }

    fn show_thinking(&mut self) {
        let text = format!("thinking  {}", tail_chars(&self.thinking, 100));
        if let Some(at) = self.thinking_at
            && let Some(line) = self.lines.get_mut(at)
        {
            line.text = text;
            return;
        }
        self.lines.push(TranscriptLine {
            kind: LineKind::Note,
            text,
        });
        self.thinking_at = Some(self.lines.len() - 1);
    }

    fn drop_thinking(&mut self) {
        if let Some(index) = self.thinking_at.take()
            && index < self.lines.len()
        {
            self.lines.remove(index);
            if let Some(at) = self.assistant_at.as_mut()
                && *at > index
            {
                *at -= 1;
            }
        }
        self.thinking.clear();
    }

    fn push(&mut self, kind: LineKind, text: String) {
        self.lines.push(TranscriptLine { kind, text });
    }

    fn agent_state(&self) -> AgentState {
        if self.approval.is_some() || self.quit_armed.is_some() || self.login_for.is_some() {
            AgentState::Blocked
        } else if self.turn_active {
            AgentState::Working
        } else {
            AgentState::Idle
        }
    }
}

/// Draws the chat until the user quits. Restores the terminal on the way out.
pub fn run(
    mut engine: Engine,
    session_log: Option<SessionLog>,
    models: Vec<String>,
    session_id: String,
) -> io::Result<()> {
    let model = models
        .first()
        .cloned()
        .unwrap_or_else(|| "model".to_owned());
    let mut chat = Chat::new(model, &session_id);
    if !models.is_empty() {
        chat.models = models;
    }
    chat.skills = discovered_skills(&chat.agent_dir);
    let detect = titi_tui::image::PlaceholderDetect::from_env();
    if detect.supported() {
        chat.kitty = true;
        chat.tmux = detect.tmux;
    }
    let mut herdr_reporter = herdr::Reporter::from_env();
    if let Some(reporter) = &mut herdr_reporter {
        reporter.set_session(&session_id);
        reporter.report(AgentState::Idle, None);
    }
    let mut reported = AgentState::Idle;
    let mut screen = Screen::open()?;
    let result = loop {
        let state = chat.agent_state();
        if state != reported
            && let Some(reporter) = &herdr_reporter
        {
            reporter.report(state, None);
            reported = state;
        }
        screen.terminal.draw(|frame| draw(frame, &mut chat))?;
        let kitty = chat.take_kitty_flush();
        if !kitty.is_empty() {
            let backend = screen.terminal.backend_mut();
            backend.write_all(kitty.as_bytes())?;
            backend.flush()?;
            screen.terminal.draw(|frame| draw(frame, &mut chat))?;
        }
        if pump(&mut engine, &mut chat, &session_log)? {
            break Ok(());
        }
    };
    if let Some(reporter) = &herdr_reporter {
        reporter.report(AgentState::Idle, None);
    }
    result
}

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn open() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            ratatui::crossterm::cursor::Hide
        )?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            ratatui::crossterm::cursor::Show,
            LeaveAlternateScreen
        );
    }
}

struct Command {
    name: &'static str,
    about: &'static str,
}

const COMMANDS: &[Command] = &[
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
        name: "keys",
        about: "which providers have a key",
    },
    Command {
        name: "login",
        about: "store a provider key",
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
        name: "recap",
        about: "what this session did",
    },
    Command {
        name: "rewind",
        about: "cut back to a rewind point",
    },
    Command {
        name: "whoami",
        about: "which providers have a key",
    },
];

/// A skill the composer can complete, mirroring what the engine discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillRow {
    name: String,
    about: String,
}

/// Discovery lives in the engine, so the picker offers exactly the names a
/// `/name` reference can expand.
fn discovered_skills(agent_dir: &Path) -> Vec<SkillRow> {
    titi_engine::skills::catalog(Some(&crate::app::current_workspace()), Some(agent_dir))
        .into_iter()
        .map(|skill| SkillRow {
            name: skill.name,
            about: skill.description,
        })
        .collect()
}

/// One offer in the `/` picker.
enum PickRow {
    Command(&'static Command),
    Skill(usize),
}

/// The `/token` under the cursor: the trailing word, when it opens with a
/// slash at the start of the line or after whitespace and holds nothing but
/// name characters. That is what keeps `/tmp/photo.png` and `a/b` out.
fn slash_token(input: &str) -> Option<(usize, &str)> {
    let start = input.rfind('/')?;
    if start > 0 && !input[..start].ends_with(char::is_whitespace) {
        return None;
    }
    let name = &input[start + 1..];
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some((start, name))
}

/// Commands first, then skills. A command only counts at the start of the
/// line, so a slash inside a sentence can only name a skill.
fn picker_rows(chat: &Chat) -> Vec<PickRow> {
    if chat.login_for.is_some() {
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

/// `part` as a whole percent of `whole`. An empty whole is 0%, not a panic.
fn share(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        0
    } else {
        (part.saturating_mul(100) / whole).min(100)
    }
}

fn known_provider(id: &str) -> bool {
    crate::engine::default_registry_config()
        .providers
        .iter()
        .any(|provider| provider.id.as_str() == id)
}

fn picker_height(chat: &Chat) -> u16 {
    picker_rows(chat).len().min(8) as u16
}

fn command_picker(chat: &Chat, width: u16, ink: &Ink) -> Paragraph<'static> {
    let rows = picker_rows(chat);
    let window = 8usize;
    let selected = if rows.is_empty() {
        0
    } else {
        chat.picker % rows.len()
    };
    let start = if rows.len() <= window {
        0
    } else {
        selected.saturating_sub(window / 2).min(rows.len() - window)
    };
    let room = (width as usize).saturating_sub(2).max(8);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(window)
        .map(|(index, row)| {
            let (name, about, is_skill) = match row {
                PickRow::Command(command) => (command.name, command.about, false),
                PickRow::Skill(at) => chat
                    .skills
                    .get(*at)
                    .map(|skill| (skill.name.as_str(), skill.about.as_str(), true))
                    .unwrap_or(("", "", true)),
            };
            let mark = if index == selected { "▶" } else { " " };
            let style = if index == selected {
                ink.fg(ink.gold).add_modifier(Modifier::BOLD)
            } else if is_skill {
                ink.fg(ink.green)
            } else {
                ink.fg(ink.muted)
            };
            let label = if is_skill {
                format!(" {mark} /{name:<12} ·skill {about}")
            } else {
                format!(" {mark} /{name:<12} {about}")
            };
            Line::from(Span::styled(
                titi_tui::width::truncate_to_width(&label, room),
                style,
            ))
        })
        .collect();
    Paragraph::new(lines).style(ink.page())
}

fn draw(frame: &mut ratatui::Frame<'_>, chat: &mut Chat) {
    let area = frame.area();
    let ink = Ink::titanium();
    frame.render_widget(Block::default().style(ink.page()), area);
    if area.height < 6 || area.width < 16 {
        return;
    }
    let picker_h = picker_height(chat);
    let cols = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(picker_h),
        Constraint::Length(4),
    ])
    .split(area);
    frame.render_widget(masthead(chat, cols[0].width, &ink), cols[0]);
    let (body, photos) = if chat.lines.is_empty() {
        (empty_state(cols[1].height, &ink), Vec::new())
    } else {
        transcript(chat, cols[1].width, cols[1].height, &ink)
    };
    frame.render_widget(body, cols[1]);
    paint_photos(frame, cols[1], &photos, &ink);
    if picker_h > 0 {
        frame.render_widget(command_picker(chat, cols[2].width, &ink), cols[2]);
    }
    frame.render_widget(composer(chat, cols[3].width, &ink), cols[3]);
}

/// Dark red. Body text stays warm white so a long reply is still readable.
struct Ink {
    page: Color,
    card: Color,
    line: Color,
    text: Color,
    muted: Color,
    dim: Color,
    accent: Color,
    gold: Color,
    green: Color,
    amber: Color,
    red: Color,
}

impl Ink {
    fn titanium() -> Self {
        Self {
            page: Color::Rgb(18, 8, 10),
            card: Color::Rgb(36, 16, 20),
            line: Color::Rgb(92, 42, 50),
            text: Color::Rgb(255, 236, 234),
            muted: Color::Rgb(196, 150, 154),
            dim: Color::Rgb(132, 90, 96),
            accent: Color::Rgb(255, 64, 84),
            gold: Color::Rgb(255, 176, 176),
            green: Color::Rgb(125, 211, 168),
            amber: Color::Rgb(255, 120, 128),
            red: Color::Rgb(255, 96, 112),
        }
    }

    fn page(&self) -> Style {
        Style::default().bg(self.page).fg(self.text)
    }

    fn fg(&self, color: Color) -> Style {
        Style::default().fg(color)
    }
}

fn masthead(chat: &Chat, width: u16, ink: &Ink) -> Paragraph<'static> {
    let state = if chat.approval.is_some() {
        "needs you"
    } else if chat.login_for.is_some() {
        "sign in"
    } else if chat.paused {
        "paused"
    } else if chat.turn_active {
        "working"
    } else {
        "ready"
    };
    let state_color = if chat.approval.is_some() || chat.paused || chat.login_for.is_some() {
        ink.amber
    } else if chat.turn_active {
        ink.accent
    } else {
        ink.dim
    };
    let ctx = match chat.context_percent {
        Some(percent) => format!("  {percent}%"),
        None => String::new(),
    };
    let left = " titi";
    let mid = format!("  {state}");
    let mut right = format!("{}{ctx}  {} ", chat.model, chat.session_label);
    let fixed = titi_tui::width::visible_width(left) + titi_tui::width::visible_width(&mid) + 2;
    let room = (width as usize).saturating_sub(fixed);
    if titi_tui::width::visible_width(&right) > room {
        right = titi_tui::width::truncate_to_width(&right, room);
    }
    let used = fixed + titi_tui::width::visible_width(&right);
    let gap = (width as usize).saturating_sub(used);
    let line = Line::from(vec![
        Span::styled(left, ink.fg(ink.accent).add_modifier(Modifier::BOLD)),
        Span::styled(mid, ink.fg(state_color)),
        Span::styled(" ".repeat(gap), ink.page()),
        Span::styled(right, ink.fg(ink.muted)),
    ]);
    Paragraph::new(line).style(ink.page())
}

fn empty_state(height: u16, ink: &Ink) -> Paragraph<'static> {
    let block = 5usize;
    let pad = (height as usize).saturating_sub(block) / 2;
    let mut rows = vec![Line::from(""); pad];
    rows.push(Line::from(Span::styled(
        "titi",
        ink.fg(ink.accent).add_modifier(Modifier::BOLD),
    )));
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "say what you want done",
        ink.fg(ink.muted),
    )));
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "enter  send      /model  switch      ctrl-c  quit",
        ink.fg(ink.dim),
    )));
    Paragraph::new(rows)
        .alignment(Alignment::Center)
        .style(ink.page())
}

struct Photo {
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

struct PhotoPaint {
    row: u16,
    id: u32,
    columns: u16,
    image_row: u16,
}

fn transcript(
    chat: &mut Chat,
    width: u16,
    height: u16,
    ink: &Ink,
) -> (Paragraph<'static>, Vec<PhotoPaint>) {
    let inner = (width as usize).saturating_sub(2).max(8);
    let max_cols = width.saturating_sub(8).max(8);
    let owned = chat.lines.clone();
    let mut rows: Vec<TranscriptRow> = Vec::new();
    for (index, line) in owned.iter().enumerate() {
        let gap = matches!(line.kind, LineKind::User | LineKind::Assistant) && index > 0;
        if gap && !rows.is_empty() {
            rows.push(TranscriptRow::Text(Line::from("")));
        }
        for text in message_rows(line, inner, ink) {
            rows.push(TranscriptRow::Text(text));
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
    let start = rows.len().saturating_sub(keep);
    let mut lines = Vec::new();
    let mut photos = Vec::new();
    for (index, row) in rows.into_iter().skip(start).enumerate() {
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
    (Paragraph::new(lines).style(ink.page()), photos)
}

fn paint_photos(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    photos: &[PhotoPaint],
    ink: &Ink,
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
            cell.set_fg(Color::Rgb(red, green, blue));
            cell.set_bg(ink.page);
        }
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

fn message_rows(line: &TranscriptLine, width: usize, ink: &Ink) -> Vec<Line<'static>> {
    match line.kind {
        LineKind::User => speech("you", ink.gold, ink.text, &line.text, width, ink),
        LineKind::Assistant => speech("titi", ink.accent, ink.text, &line.text, width, ink),
        LineKind::Tool => vec![chip(tool_chip(&line.text, ink), ink, width)],
        LineKind::Error => vec![chip(("✕", ink.red, line.text.clone(), ink.red), ink, width)],
        LineKind::Note => vec![chip(("·", ink.dim, line.text.clone(), ink.dim), ink, width)],
    }
}

fn speech(
    name: &str,
    label: Color,
    body: Color,
    text: &str,
    width: usize,
    ink: &Ink,
) -> Vec<Line<'static>> {
    // "you" and "titi" share a column so a short message stays one row.
    let tag = format!("{name:<4}");
    let wrap_at = width.saturating_sub(10).max(8);
    let pieces = wrap_plain(text, wrap_at);
    let mut rows = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.into_iter().enumerate() {
        let row = if index == 0 {
            vec![
                Span::styled("  ", ink.page()),
                Span::styled(tag.clone(), ink.fg(label).add_modifier(Modifier::BOLD)),
                Span::styled(" │ ", ink.fg(label)),
                Span::styled(piece, ink.fg(body)),
            ]
        } else {
            vec![
                Span::styled("       │ ", ink.fg(label)),
                Span::styled(piece, ink.fg(body)),
            ]
        };
        rows.push(Line::from(row));
    }
    rows
}

fn tool_chip(text: &str, ink: &Ink) -> (&'static str, Color, String, Color) {
    // The engine records "tool <name>" and "tool done  <preview>".
    // The screen says the same thing without the debug prefix.
    if let Some(rest) = text.strip_prefix("tool error") {
        return ("✕", ink.red, rest.trim().to_owned(), ink.red);
    }
    if let Some(rest) = text.strip_prefix("tool done") {
        let detail = rest.trim();
        let body = if detail.is_empty() {
            "done".to_owned()
        } else {
            detail.to_owned()
        };
        return ("✓", ink.green, body, ink.muted);
    }
    if let Some(rest) = text.strip_prefix("tool ") {
        return ("▸", ink.amber, rest.trim().to_owned(), ink.gold);
    }
    ("▸", ink.amber, text.to_owned(), ink.muted)
}

fn chip(parts: (&str, Color, String, Color), ink: &Ink, width: usize) -> Line<'static> {
    let (mark, mark_color, text, text_color) = parts;
    let room = width.saturating_sub(6).max(4);
    let shown = titi_tui::width::truncate_to_width(&text, room);
    Line::from(vec![
        Span::styled("   ", ink.page()),
        Span::styled(mark.to_owned(), ink.fg(mark_color)),
        Span::styled(" ", ink.page()),
        Span::styled(shown, ink.fg(text_color)),
    ])
}

fn composer(chat: &Chat, width: u16, ink: &Ink) -> Paragraph<'static> {
    let (border, caption_color) = if chat.approval.is_some() || chat.login_for.is_some() {
        (ink.amber, ink.amber)
    } else if chat.turn_active {
        (ink.accent, ink.accent)
    } else {
        (ink.line, ink.dim)
    };
    let caption = titi_tui::width::truncate_to_width(
        &composer_caption(chat),
        (width as usize).saturating_sub(4),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(ink.fg(border))
        .title_bottom(
            Line::from(Span::styled(format!(" {caption} "), ink.fg(caption_color))).centered(),
        )
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(ink.card).fg(ink.text));
    let inner = (width as usize).saturating_sub(6).max(4);
    let line = if let Some(pending) = &chat.approval {
        Line::from(Span::styled(
            titi_tui::width::truncate_to_width(
                &format!("{}   y allow    n refuse", pending.name),
                inner,
            ),
            ink.fg(ink.amber).add_modifier(Modifier::BOLD),
        ))
    } else if let Some(provider) = &chat.login_for {
        let shown = if chat.input.is_empty() {
            format!("paste the {provider} key")
        } else {
            "•".repeat(chat.input.chars().count().min(32))
        };
        let color = if chat.input.is_empty() {
            ink.dim
        } else {
            ink.text
        };
        Line::from(vec![
            Span::styled("› ", ink.fg(ink.accent)),
            Span::styled(shown, ink.fg(color)),
        ])
    } else if chat.input.is_empty() {
        let placeholder = if chat.paused {
            "paused…"
        } else if chat.turn_active {
            "steer this turn…"
        } else {
            "ask titi…"
        };
        Line::from(vec![
            Span::styled("› ", ink.fg(ink.accent)),
            Span::styled(placeholder, ink.fg(ink.dim)),
        ])
    } else {
        let room = inner.saturating_sub(4).max(1);
        Line::from(vec![
            Span::styled("› ", ink.fg(ink.accent)),
            Span::styled(fit_tail(&chat.input, room), ink.fg(ink.text)),
            Span::styled("▍", ink.fg(ink.accent)),
        ])
    };
    Paragraph::new(line).block(block)
}

fn composer_caption(chat: &Chat) -> String {
    let ctx = match chat.context_percent {
        Some(percent) => format!("{percent}%  ·  "),
        None => String::new(),
    };
    let keys = if chat.approval.is_some() {
        "y allow  ·  n refuse"
    } else if chat.login_for.is_some() {
        "enter stores  ·  esc cancels"
    } else if chat.paused {
        "/pause resumes"
    } else if !chat.hint.is_empty() {
        chat.hint.as_str()
    } else if chat.turn_active {
        "enter steers  ·  ctrl-c stops"
    } else {
        "enter sends  ·  /model  ·  ctrl-c quits"
    };
    format!("{ctx}{keys}")
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            rows.push(String::new());
            continue;
        }
        let chars: Vec<char> = paragraph.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            let mut col = 0usize;
            let mut last_space = None;
            let mut end = index;
            while end < chars.len() {
                let cell = titi_tui::width::visible_width(&chars[end].to_string());
                if col + cell > width && end > index {
                    break;
                }
                if chars[end] == ' ' {
                    last_space = Some(end);
                }
                col += cell;
                end += 1;
            }
            let cut = if end < chars.len() {
                last_space.filter(|at| *at > index).unwrap_or(end)
            } else {
                end
            };
            let piece: String = chars[index..cut].iter().collect();
            rows.push(piece.trim_end().to_owned());
            index = cut;
            while index < chars.len() && chars[index] == ' ' {
                index += 1;
            }
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn fit_tail(text: &str, width: usize) -> String {
    if titi_tui::width::visible_width(text) <= width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut end = chars.len();
    let mut col = 0usize;
    let room = width.saturating_sub(1);
    while end > 0 {
        let cell = titi_tui::width::visible_width(&chars[end - 1].to_string());
        if col + cell > room {
            break;
        }
        col += cell;
        end -= 1;
    }
    format!("…{}", chars[end..].iter().collect::<String>())
}

fn short_session(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 14 {
        id.to_owned()
    } else {
        chars[chars.len() - 12..].iter().collect()
    }
}

fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for ch in text.chars() {
        if count >= max {
            out.push('…');
            break;
        }
        if ch.is_control() {
            if !out.ends_with(' ') {
                out.push(' ');
                count += 1;
            }
        } else {
            out.push(ch);
            count += 1;
        }
    }
    out
}

fn tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_owned()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}

fn map_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char('c') if control => Some(Key::CtrlC),
        KeyCode::Char('d') if control => Some(Key::CtrlD),
        KeyCode::Char(ch) if !control && !modifiers.contains(KeyModifiers::ALT) => {
            Some(Key::Char(ch))
        }
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Tab => Some(Key::Tab),
        _ => None,
    }
}

/// Polls the keyboard and the engine once. `Ok(true)` means the user quit.
fn pump(
    engine: &mut Engine,
    chat: &mut Chat,
    session_log: &Option<SessionLog>,
) -> io::Result<bool> {
    if event::poll(Duration::from_millis(50))? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(mapped) = map_key(key.code, key.modifiers) {
                    let applied = chat.on_key(mapped, Instant::now());
                    if dispatch(engine, chat, session_log, applied) {
                        return Ok(true);
                    }
                }
            }
            Event::Paste(text) => chat.paste(&text),
            _ => {}
        }
    }
    loop {
        match engine.try_recv() {
            Ok(event) => {
                let applied = chat.on_event(event);
                record(chat, session_log, applied.log);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                chat.push(LineKind::Error, "engine stopped".to_owned());
                break;
            }
        }
    }
    Ok(false)
}

fn dispatch(
    engine: &mut Engine,
    chat: &mut Chat,
    session_log: &Option<SessionLog>,
    applied: Applied,
) -> bool {
    record(chat, session_log, applied.log);
    match applied.effect {
        Some(ChatEffect::Quit) => {
            let _ = engine.try_send(EngineCommand::Shutdown);
            true
        }
        Some(ChatEffect::Send(command)) => {
            if engine.try_send(command).is_err() {
                chat.push(LineKind::Error, "engine stopped".to_owned());
            }
            false
        }
        None => false,
    }
}

fn record(chat: &mut Chat, session_log: &Option<SessionLog>, write: Option<LogWrite>) {
    let Some(log) = session_log else {
        return;
    };
    let Some(write) = write else {
        return;
    };
    let result = match write.role {
        Role::User => log.user(&write.text),
        Role::Assistant if write.tool_calls.is_empty() => log.assistant(&write.text),
        Role::Assistant => log.assistant_tool_calls(&write.text, write.tool_calls),
        Role::System => log.system(&write.text),
        Role::Tool => log.tool_result(&write.text),
    };
    if let Err(reason) = result {
        chat.push(LineKind::Error, format!("session: not saved ({reason})"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use titi_engine::TurnId;
    use titi_providers::StopReason;

    fn chat() -> Chat {
        Chat::new("openai/gpt-4.1", "session-123")
    }

    fn frame_text(chat: &mut Chat) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn type_text(chat: &mut Chat, text: &str) {
        let now = Instant::now();
        for ch in text.chars() {
            chat.on_key(Key::Char(ch), now);
        }
    }

    #[test]
    fn enter_while_idle_submits_and_logs_the_user() {
        let mut chat = chat();
        type_text(&mut chat, "hi");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SubmitPrompt { .. }))
        ));
        assert_eq!(
            applied.log,
            Some(LogWrite::text(Role::User, "hi".to_owned()))
        );
        assert!(chat.turn_active);
    }

    #[test]
    fn enter_during_a_turn_steers_and_leaves_it_active() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        type_text(&mut chat, "look again");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::Steer { text })) => {
                assert_eq!(text.as_str(), "look again");
            }
            other => panic!("expected steer, got {other:?}"),
        }
        assert!(chat.turn_active);
        assert_eq!(
            applied.log,
            Some(LogWrite::text(Role::User, "look again".to_owned()))
        );
    }

    /// The user cancelled, so the prompt waiting behind that turn never ran.
    /// It has to come back somewhere the user can see it.
    #[test]
    fn a_returned_prompt_lands_in_an_empty_composer() {
        let mut chat = chat();
        chat.on_event(EngineEvent::PromptReturned {
            text: "the question nobody asked".into(),
        });
        assert_eq!(chat.input, "the question nobody asked");
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("the question nobody asked"),
            "the returned prompt is on screen: {frame}"
        );
    }

    /// The user already started typing something else. Overwriting that is
    /// the same silent loss the event exists to prevent, so the composer is
    /// left alone and the text goes to the transcript instead.
    #[test]
    fn a_returned_prompt_never_overwrites_what_the_user_is_typing() {
        let mut chat = chat();
        type_text(&mut chat, "already typing this");
        chat.on_event(EngineEvent::PromptReturned {
            text: "the question nobody asked".into(),
        });
        assert_eq!(chat.input, "already typing this");
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("already typing this"),
            "what the user typed survives: {frame}"
        );
        assert!(
            frame.contains("the question nobody asked"),
            "the returned prompt is still shown: {frame}"
        );
    }

    #[test]
    fn approval_yes_and_no() {
        let mut chat = chat();
        chat.on_event(EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "bash".into(),
        });
        let yes = chat.on_key(Key::Char('y'), Instant::now());
        match yes.effect {
            Some(ChatEffect::Send(EngineCommand::ApproveTool { call_id, approved })) => {
                assert_eq!(call_id.as_str(), "call-1");
                assert!(approved);
            }
            other => panic!("expected approval, got {other:?}"),
        }
        assert!(chat.approval.is_none());

        chat.on_event(EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "call-2".into(),
            name: "edit".into(),
        });
        chat.on_key(Key::Char('x'), Instant::now());
        assert!(chat.input.is_empty());
        let no = chat.on_key(Key::Char('n'), Instant::now());
        match no.effect {
            Some(ChatEffect::Send(EngineCommand::ApproveTool { approved, .. })) => {
                assert!(!approved);
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn stream_accumulates_and_finish_logs_the_assistant() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(7),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(7),
            text: "hel".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(7),
            text: "lo".into(),
        });
        let applied = chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(7),
            reason: StopReason::Stop,
        });
        assert_eq!(
            applied.log,
            Some(LogWrite::text(Role::Assistant, "hello".to_owned()))
        );
        assert!(!chat.turn_active);
    }

    /// A turn with a tool round is three entries, and the text before the
    /// call is written once, not again at the end of the turn.
    #[test]
    fn a_tool_round_logs_the_call_its_output_and_the_answer() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(3),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(3),
            text: "let me look".into(),
        });
        let call = chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(3),
            call_id: "call-1".into(),
            name: "read".into(),
        });
        assert_eq!(
            call.log,
            Some(LogWrite {
                role: Role::Assistant,
                text: "let me look".to_owned(),
                tool_calls: vec![titi_providers::ToolCallRef {
                    call_id: "call-1".into(),
                    name: "read".into(),
                }],
            })
        );
        let result = chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(3),
            call_id: "call-1".into(),
            output: "[package]".into(),
            is_error: false,
        });
        assert_eq!(
            result.log,
            Some(LogWrite::text(Role::Tool, "[package]".to_owned()))
        );
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(3),
            text: " it is the workspace".into(),
        });
        let finished = chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(3),
            reason: StopReason::Stop,
        });
        assert_eq!(
            finished.log,
            Some(LogWrite::text(
                Role::Assistant,
                " it is the workspace".to_owned()
            ))
        );
    }

    #[test]
    fn second_ctrl_c_within_two_seconds_quits() {
        let mut chat = chat();
        let start = Instant::now();
        let first = chat.on_key(Key::CtrlC, start);
        assert!(first.effect.is_none());
        let second = chat.on_key(Key::CtrlC, start + Duration::from_millis(500));
        assert_eq!(second.effect, Some(ChatEffect::Quit));
    }

    #[test]
    fn ctrl_c_during_a_turn_cancels() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        let applied = chat.on_key(Key::CtrlC, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::Cancel))
        );
    }

    #[test]
    fn slash_model_cycles_without_sending_a_prompt() {
        let mut chat = chat();
        chat.models = vec![
            "openai/gpt-4.1".to_owned(),
            "opencode-go/glm-5.3-flash".to_owned(),
        ];
        type_text(&mut chat, "/model");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::SwitchModel { model })) => {
                assert_eq!(model.as_str(), "opencode-go/glm-5.3-flash");
            }
            other => panic!("expected a model switch, got {other:?}"),
        }
        assert!(applied.log.is_none());
        assert!(!chat.turn_active);
    }

    #[test]
    fn slash_pause_blocks_a_prompt_until_resumed() {
        let mut chat = chat();
        type_text(&mut chat, "/pause");
        let paused = chat.on_key(Key::Enter, Instant::now());
        assert!(paused.effect.is_none());
        assert!(chat.paused);
        type_text(&mut chat, "hi");
        let blocked = chat.on_key(Key::Enter, Instant::now());
        assert!(blocked.effect.is_none());
        assert!(blocked.log.is_none());
        type_text(&mut chat, "/pause");
        chat.on_key(Key::Enter, Instant::now());
        assert!(!chat.paused);
    }

    #[test]
    fn slash_pause_cancels_a_running_turn() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        type_text(&mut chat, "/pause");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::Cancel))
        );
        assert!(chat.paused);
    }

    #[test]
    fn a_leading_slash_lists_commands() {
        let mut chat = chat();
        type_text(&mut chat, "/");
        let view = frame_text(&mut chat);
        assert!(view.contains("/login"), "{view}");
        assert!(view.contains("store a provider key"), "{view}");
    }

    #[test]
    fn goal_is_listed_after_a_slash() {
        let mut chat = chat();
        type_text(&mut chat, "/go");
        let view = frame_text(&mut chat);
        assert!(view.contains("/goal"), "{view}");
        assert!(view.contains("coder and reviewer"), "{view}");
    }

    /// A command that dispatches but is missing from the listing works yet
    /// cannot be discovered; /goal shipped that way once.
    #[test]
    fn every_dispatched_command_is_listed() {
        for name in [
            "checkpoint",
            "checkpoints",
            "compact",
            "context",
            "rewind",
            "recap",
            "pause",
            "goal",
            "help",
            "login",
            "logout",
            "keys",
            "whoami",
        ] {
            assert!(
                COMMANDS.iter().any(|command| command.name == name),
                "/{name} dispatches but is not listed"
            );
        }
    }

    /// The tokens of one breakdown line, `None` for anything else.
    fn tokens_in(line: &str) -> Option<u64> {
        line.split(" tokens")
            .next()?
            .split_whitespace()
            .next_back()?
            .parse()
            .ok()
    }

    /// A breakdown whose parts do not add up to its total is worse than no
    /// breakdown: it reads as if something were hiding in the window.
    #[test]
    fn context_renders_parts_that_sum_to_the_reported_total() {
        let mut chat = chat();
        chat.on_event(EngineEvent::ContextBreakdown {
            parts: vec![
                ContextPart {
                    label: "system prompt".into(),
                    tokens: 300,
                },
                ContextPart {
                    label: "genome map".into(),
                    tokens: 500,
                },
                ContextPart {
                    label: "history".into(),
                    tokens: 200,
                },
            ],
            window: 10_000,
        });

        let rendered: Vec<String> = chat.lines.iter().map(|line| line.text.clone()).collect();
        let Some(total) = rendered.iter().find(|line| line.starts_with("total")) else {
            panic!("no total line: {rendered:?}");
        };
        let summed: u64 = rendered
            .iter()
            .filter(|line| !line.starts_with("total"))
            .filter_map(|line| tokens_in(line))
            .sum();

        assert_eq!(tokens_in(total), Some(summed), "{rendered:?}");
        assert_eq!(summed, 1000, "{rendered:?}");
        assert!(total.contains("10% of 10000"), "{rendered:?}");
        assert!(
            rendered.iter().any(|line| line.contains("estimates")),
            "the numbers are estimates and must say so: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.starts_with("genome map") && line.contains("50%")),
            "{rendered:?}"
        );
    }

    #[test]
    fn context_takes_no_argument() {
        let mut chat = chat();
        type_text(&mut chat, "/context");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::DescribeContext))
        );

        type_text(&mut chat, "/context now");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none(), "{:?}", applied.effect);
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /context")),
            "a stray argument was swallowed"
        );
    }

    #[test]
    fn compact_dispatches_with_and_without_a_focus() {
        let mut chat = chat();
        type_text(&mut chat, "/compact");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::Compact { focus: None }))
        );

        type_text(&mut chat, "/compact the auth refactor");
        match chat.on_key(Key::Enter, Instant::now()).effect {
            Some(ChatEffect::Send(EngineCommand::Compact { focus: Some(focus) })) => {
                assert_eq!(focus.as_str(), "the auth refactor");
            }
            other => panic!("expected a focused compaction, got {other:?}"),
        }
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("focus: the auth refactor")),
            "the focus never reached the transcript"
        );
    }

    #[test]
    fn a_slash_inside_a_sentence_does_not_list_commands() {
        let mut chat = chat();
        type_text(&mut chat, "see /rewind");
        let view = frame_text(&mut chat);
        assert!(!view.contains("cut back to a rewind point"), "{view}");
    }

    #[test]
    fn tab_fills_the_highlighted_command() {
        let mut chat = chat();
        type_text(&mut chat, "/");
        chat.on_key(Key::Down, Instant::now());
        chat.on_key(Key::Tab, Instant::now());
        assert_eq!(chat.input, "/checkpoints ");
    }

    #[test]
    fn enter_on_a_prefix_runs_the_highlighted_command() {
        let mut chat = chat();
        type_text(&mut chat, "/re");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.lines.iter().any(|line| line.text.contains("recap")));
    }

    #[test]
    fn esc_clears_a_leading_slash() {
        let mut chat = chat();
        type_text(&mut chat, "/mo");
        chat.on_key(Key::Esc, Instant::now());
        assert!(chat.input.is_empty());
    }

    fn chat_with_skills() -> Chat {
        let mut chat = chat();
        chat.skills = vec![SkillRow {
            name: "code-review".to_owned(),
            about: "check a diff".to_owned(),
        }];
        chat
    }

    #[test]
    fn a_slash_inside_a_sentence_lists_skills() {
        let mut chat = chat_with_skills();
        type_text(&mut chat, "please run /cod");
        let view = frame_text(&mut chat);
        assert!(view.contains("code-review"), "{view}");
        assert!(view.contains("·skill"), "{view}");
    }

    #[test]
    fn completing_a_skill_keeps_the_rest_of_the_line() {
        let mut chat = chat_with_skills();
        type_text(&mut chat, "please run /cod");
        chat.on_key(Key::Tab, Instant::now());
        assert_eq!(chat.input, "please run /code-review ");
    }

    #[test]
    fn enter_mid_sentence_completes_the_skill_instead_of_sending() {
        let mut chat = chat_with_skills();
        type_text(&mut chat, "please run /cod");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert_eq!(chat.input, "please run /code-review ");
    }

    #[test]
    fn a_leading_skill_name_is_sent_as_typed() {
        let mut chat = chat_with_skills();
        type_text(&mut chat, "/code-review this diff");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SubmitPrompt {
                text: "/code-review this diff".into(),
            }))
        );
        assert_eq!(
            applied.log,
            Some(LogWrite::text(
                Role::User,
                "/code-review this diff".to_owned()
            ))
        );
        assert!(
            !chat
                .lines
                .iter()
                .any(|line| line.text.contains("unknown command")),
            "a known skill must not be refused as a command"
        );
    }

    #[test]
    fn a_command_still_wins_over_a_skill_of_the_same_prefix() {
        let mut chat = chat_with_skills();
        chat.skills.push(SkillRow {
            name: "recap-notes".to_owned(),
            about: "notes".to_owned(),
        });
        type_text(&mut chat, "/recap");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.lines.iter().any(|line| line.text.contains("recap")));
    }

    #[test]
    fn login_masks_the_key_and_stores_it() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = Chat::new("openai/gpt-4.1", "session-123");
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/login openai");
        chat.on_key(Key::Enter, Instant::now());
        assert_eq!(chat.login_for.as_deref(), Some("openai"));
        type_text(&mut chat, "sk-secret-value");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.log.is_none());
        assert!(chat.login_for.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("key stored"))
        );
        assert!(
            !chat
                .lines
                .iter()
                .any(|line| line.text.contains("sk-secret"))
        );
        let keys = crate::secrets::list_keys(dir.path()).expect("keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].provider, "openai");
    }

    #[test]
    fn logout_forgets_the_stored_key() {
        let dir = tempfile::tempdir().expect("temp");
        crate::secrets::store_key(dir.path(), "openai", "sk-test").expect("store");
        let mut chat = Chat::new("openai/gpt-4.1", "session-123");
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/logout openai");
        chat.on_key(Key::Enter, Instant::now());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("signed out"))
        );
        assert!(crate::secrets::list_keys(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn an_inline_login_does_not_echo_the_key() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = Chat::new("openai/gpt-4.1", "session-123");
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/login openai sk-one-line");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.log.is_none());
        assert!(
            !chat
                .lines
                .iter()
                .any(|line| line.text.contains("sk-one-line"))
        );
        assert_eq!(
            crate::secrets::list_keys(dir.path()).unwrap()[0].provider,
            "openai"
        );
    }

    #[test]
    fn a_path_is_not_a_slash_command() {
        let mut chat = chat();
        type_text(&mut chat, "/tmp/photo.png");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SubmitPrompt { .. }))
        ));
    }

    #[test]
    fn unknown_slash_is_not_sent_to_the_model() {
        let mut chat = chat();
        type_text(&mut chat, "/nope");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.lines.iter().any(|line| line.text.contains("unknown")));
    }

    #[test]
    fn rewind_restores_the_history_and_tells_the_engine() {
        let dir = tempfile::tempdir().expect("temp");
        let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
        let id = store
            .create(titi_core::session::SessionMeta::default())
            .expect("session");
        store.append(&id, Role::User, "keep").expect("keep");
        store.checkpoint(&id).expect("checkpoint");
        store.append(&id, Role::User, "drop").expect("drop");
        let mut chat = Chat::new("openai/gpt-4.1", &id);
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/rewind");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].content.as_str(), "keep");
            }
            other => panic!("expected restore, got {other:?}"),
        }
        assert!(chat.lines.iter().any(|line| line.text == "keep"));
        assert!(!chat.lines.iter().any(|line| line.text == "drop"));
    }

    #[test]
    fn a_late_ctrl_c_does_not_quit() {
        let mut chat = chat();
        let start = Instant::now();
        chat.on_key(Key::CtrlC, start);
        let later = chat.on_key(Key::CtrlC, start + Duration::from_secs(3));
        assert!(later.effect.is_none());
        assert!(chat.quit_armed.is_some());
    }

    #[test]
    fn a_frame_shows_the_model_the_reply_and_the_composer() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        let mut chat = chat();
        type_text(&mut chat, "hi");
        chat.on_key(Key::Enter, Instant::now());
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "hello".into(),
        });
        assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
        let view: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(view.contains("titi"), "{view}");
        assert!(view.contains("openai/gpt-4.1"), "{view}");
        assert!(view.contains("you"), "{view}");
        assert!(view.contains("hi"), "{view}");
        assert!(view.contains("hello"), "{view}");
    }

    #[test]
    fn a_local_png_is_placed_when_kitty_is_on() {
        let path = std::env::temp_dir().join(format!("titi-kitty-{}.png", std::process::id()));
        std::fs::write(&path, PNG_2X2).expect("write png");
        let mut chat = chat();
        chat.kitty = true;
        chat.push(LineKind::User, path.display().to_string());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
        let flush = chat.take_kitty_flush();
        assert!(flush.contains("f=32"), "{flush}");
        assert!(flush.contains("a=p,U=1"), "{flush}");
        let symbols: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(symbols.contains('\u{10EEEE}'), "{symbols}");
        assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
        assert!(chat.take_kitty_flush().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn goal_is_reserved_and_does_not_submit() {
        let mut chat = chat();
        type_text(&mut chat, "/goal fix the parser");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::RunGoal { text })) => {
                assert_eq!(text.as_str(), "fix the parser");
            }
            other => panic!("expected RunGoal, got {other:?}"),
        }
        assert!(applied.log.is_none(), "a goal is not a user prompt");
        assert!(!chat.turn_active);
    }

    #[test]
    fn goal_without_text_does_not_submit() {
        let mut chat = chat();
        type_text(&mut chat, "/goal");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /goal")),
            "{:?}",
            chat.lines
        );
    }

    #[test]
    fn a_goal_report_lands_on_the_transcript() {
        let mut chat = chat();
        chat.on_event(EngineEvent::GoalFinished {
            report: "goal: passed · 1 round · verdict pass".into(),
        });
        assert!(chat.lines.iter().any(|line| line.text.contains("passed")));
        assert!(!chat.turn_active);
    }

    /// 2×2 red PNG. Small enough to keep the kitty transmit in the test.
    const PNG_2X2: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6,
        0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 132, 25, 96, 12, 0, 71, 202, 7, 249, 103, 89, 110, 183, 0, 0, 0, 0, 73, 69, 78, 68,
        174, 66, 96, 130,
    ];
}
