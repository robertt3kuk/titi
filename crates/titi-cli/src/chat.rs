//! Full-screen chat.
//!
//! The state machine does not touch the terminal, so tests drive it with
//! keys and engine events. [`run`] is the only place that owns the screen.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Stdout, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::CellDiffOption;
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
use titi_engine::protocol::{JobInfo, SessionMode};
use titi_engine::{ContextPart, Engine, EngineCommand, EngineEvent};
use titi_tui::theme::{Theme, ThemeBg, ThemeColor};
use tokio::sync::mpsc::error::TryRecvError;

use crate::herdr::{self, AgentState};
use crate::hub::{HubSession, HubUpdate};
use crate::login::{LoginDriver, LoginEvent, LoginFlow, OAuthProvider};
use crate::session_log::SessionLog;

const QUIT_WINDOW: Duration = Duration::from_secs(2);
const TOOL_PREVIEW: usize = 120;

/// Deadline for the `git` call behind `/git` and `/diagnose`. The screen is
/// blocked while it runs, so it is far shorter than the git tool's own
/// minute: a hook waiting on a terminal this process never gives it must
/// not take the session with it.
const SLASH_GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the run loop looks at the child and the deadline.
const GIT_POLL: Duration = Duration::from_millis(10);

/// Most one slash command keeps from a git call. A diff longer than this is
/// a file to read, not a transcript line.
const GIT_OUTPUT_CAP: usize = 64 * 1024;

/// Fewest and most lines the picker above the composer takes. The floor keeps
/// a window on a short screen from showing a single row with two `… more`
/// lines around it; the ceiling keeps the conversation on screen, however
/// long the catalog is.
const PICKER_MIN_ROWS: usize = 3;
const PICKER_MAX_ROWS: usize = 14;

/// Braille spinner frames, in omp's glyph set, one step per loop tick: the
/// run loop wakes on a 50ms poll, so a step of 50ms means the glyph changes
/// on every frame it draws. A spinner that does not move reads as a frozen
/// screen, which is worse than no spinner at all.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_PERIOD: Duration = Duration::from_millis(50);

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
    PageUp,
    PageDown,
    PageUpHalf,
    PageDownHalf,
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

/// Who a transcript line belongs to. Public because a cast replay renders
/// the same lines outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Tool,
    Error,
    Note,
}

impl LineKind {
    /// The word a surface without colour puts in front of the line.
    pub fn as_str(self) -> &'static str {
        match self {
            LineKind::User => "you",
            LineKind::Assistant => "titi",
            LineKind::Tool => "tool",
            LineKind::Error => "error",
            LineKind::Note => "note",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingApproval {
    call_id: String,
    name: String,
}

/// What the running turn is doing, for the status row above the composer.
///
/// Every phase is entered because an engine event said so; none is guessed
/// from a flag. There is no request-sent and no first-token event, so
/// `Waiting` is the turn's state from the moment it is asked for until the
/// first `StreamDelta` or `ThinkingDelta` — the interval that used to be
/// invisible.
///
/// Ownership rule, so the screen never reports one fact twice: this row owns
/// the moving glyph, the elapsed seconds and the one fact that changes (the
/// phase, the characters received, the tool's name). The masthead keeps only
/// the slow state word, the mode and the model; the composer keeps the key
/// hints.
#[derive(Debug, Clone)]
enum WorkPhase {
    /// Asked for; the model has not answered with anything yet.
    Waiting,
    /// Assistant text is arriving. The count is read from the reply the
    /// deltas built, so it is what the chat received, not a token estimate.
    Streaming,
    /// Reasoning is arriving and no answer text has yet. The events tell
    /// these apart: `ThinkingDelta` is not `StreamDelta`
    /// (crates/titi-engine/src/protocol.rs:166-172).
    Thinking,
    /// One tool call is running. `call_id` is what a `ToolFinished` must
    /// carry before this phase may end, so a second call cannot end the
    /// first one's state.
    Tool {
        call_id: String,
        name: String,
        since: Instant,
    },
}

impl WorkPhase {
    /// Whether this phase is the given tool call, still running.
    fn is_tool(&self, call_id: &str) -> bool {
        matches!(self, Self::Tool { call_id: running, .. } if running == call_id)
    }
}

/// A login the screen started but has not finished.
///
/// The flow itself is a task; this is only the two ends the render loop
/// holds, so a frame never waits on a browser, a socket or a person.
struct OAuthLogin {
    provider: &'static OAuthProvider,
    flow: LoginFlow,
    /// How this provider is being signed in. The device grant has no code to
    /// paste back, so the composer must not ask for one.
    method: LoginMethod,
}

/// Conversation on screen. No terminal, no session file.
pub struct Chat {
    lines: Vec<TranscriptLine>,
    input: String,
    turn_active: bool,
    /// When the running turn was asked for. `Some` exactly while
    /// `turn_active`: the status row above the composer reads it for the
    /// spinner and the elapsed seconds, so a request in flight is visible
    /// before the first token.
    turn_started: Option<Instant>,
    /// Which phase the status row is in. Kept current on every turn so the
    /// row never shows a stale one; only read while `turn_active`.
    phase: WorkPhase,
    active_turn_id: Option<titi_engine::TurnId>,
    model: String,
    /// Live: a local server that answers after the first frame adds models,
    /// so the list is read when `/model` runs, not captured at startup.
    catalog: crate::engine::ModelCatalog,
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
    session_prompt_tokens: u32,
    session_completion_tokens: u32,
    last_prompt_tokens: u32,
    last_completion_tokens: u32,
    quit_armed: Option<Instant>,
    hint: String,
    /// Provider waiting for a key or an OAuth code. The composer masks
    /// whatever is typed in either case.
    login_for: Option<String>,
    /// The OAuth login behind `login_for`, when the provider is signed in
    /// through a browser rather than with a pasted key.
    oauth: Option<OAuthLogin>,
    /// Where a login is started. Production builds the terminal driver on
    /// first use; tests inject one so no socket, browser or provider is
    /// involved.
    login_driver: Option<Arc<dyn LoginDriver>>,
    /// Highlight in the leading-slash command list.
    picker: usize,
    /// Highlight in the bare-`/login` subscription picker; `None` = closed.
    login_picker: Option<usize>,
    /// The model browser bare `/model` and bare `/switch` open; `None` =
    /// closed.
    model_picker: Option<ModelPicker>,
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
    /// Background loops the engine reported, newest last.
    jobs: Vec<JobInfo>,
    /// Tokens the engine says this session has spent.
    spent_tokens: u64,
    /// The cap `/budget` set, as the engine confirmed it.
    budget: Option<u64>,
    /// The mode the engine confirmed it is in.
    mode: SessionMode,
    /// Membership in the local hub, when `/join` connected.
    hub: HubSession,
    /// Whether the roster panel is shown (`/hub`).
    hub_open: bool,
    scroll_offset: usize,
    last_transcript_height: usize,
    /// The active theme. Every colour the screen draws — the masthead, the
    /// transcript, the chips, the composer, the status row — is a token of it,
    /// so a theme change is a colour change on the whole screen.
    theme: Arc<Theme>,
}

impl Chat {
    pub fn new(model: impl Into<String>, session_id: &str, theme: Arc<Theme>) -> Self {
        let model = model.into();
        Self {
            lines: Vec::new(),
            input: String::new(),
            turn_active: false,
            turn_started: None,
            phase: WorkPhase::Waiting,
            active_turn_id: None,
            model: model.clone(),
            catalog: crate::engine::ModelCatalog::fixed(vec![model]),
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
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            last_prompt_tokens: 0,
            last_completion_tokens: 0,
            quit_armed: None,
            hint: String::new(),
            login_for: None,
            oauth: None,
            login_driver: None,
            picker: 0,
            login_picker: None,
            model_picker: None,
            skills: Vec::new(),
            kitty: false,
            tmux: false,
            photos: Vec::new(),
            misses: HashSet::new(),
            next_image_id: 1,
            kitty_flush: String::new(),
            jobs: Vec::new(),
            spent_tokens: 0,
            budget: None,
            mode: SessionMode::Agent,
            hub: HubSession::default(),
            hub_open: false,
            scroll_offset: 0,
            last_transcript_height: 0,
            theme,
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
        if self.login_picker.is_some() {
            return self.login_picker_key(key, now);
        }
        if self.model_picker.is_some() {
            return self.model_picker_key(key, now);
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
                self.submit(now)
            }
            Key::Backspace => {
                self.disarm();
                self.input.pop();
                self.picker = 0;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Char(ch) => {
                self.disarm();
                self.input.push(ch);
                self.picker = 0;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Esc if self.picking() => {
                self.input.clear();
                self.picker = 0;
                self.disarm();
                Applied::none()
            }
            Key::Up => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                Applied::none()
            }
            Key::Down => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                Applied::none()
            }
            Key::PageUp => {
                let h = self.last_transcript_height;
                self.scroll_offset = self.scroll_offset.saturating_add(h.saturating_sub(1));
                Applied::none()
            }
            Key::PageDown => {
                let h = self.last_transcript_height;
                self.scroll_offset = self.scroll_offset.saturating_sub(h.saturating_sub(1));
                Applied::none()
            }
            Key::PageUpHalf => {
                let h = self.last_transcript_height / 2;
                self.scroll_offset = self.scroll_offset.saturating_add(h.max(1));
                Applied::none()
            }
            Key::PageDownHalf => {
                let h = self.last_transcript_height / 2;
                self.scroll_offset = self.scroll_offset.saturating_sub(h.max(1));
                Applied::none()
            }
            Key::Esc | Key::CtrlD | Key::Tab => {
                self.disarm();
                Applied::none()
            }
        }
    }

    pub fn on_event(&mut self, event: EngineEvent) -> Applied {
        match event {
            EngineEvent::TurnStarted { turn_id, model, .. } => {
                self.turn_active = true;
                // A prompt sent from the composer already stamped the start;
                // one the engine began on its own (a goal, a loop) is stamped
                // here, so the status row has a time either way.
                self.turn_started.get_or_insert_with(Instant::now);
                // Whatever a previous turn left in the phase, this turn
                // starts before its first token: only a delta moves it on.
                self.phase = WorkPhase::Waiting;
                self.active_turn_id = Some(turn_id);
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
                // A tool cannot stream and run at once, so text is always
                // the newer fact.
                self.phase = WorkPhase::Streaming;
                Applied::none()
            }
            EngineEvent::ThinkingDelta { text, .. } if self.reply.is_empty() => {
                self.thinking.push_str(&text);
                self.show_thinking();
                self.phase = WorkPhase::Thinking;
                Applied::none()
            }
            EngineEvent::ToolStarted { call_id, name, .. } => {
                self.phase = WorkPhase::Tool {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    since: Instant::now(),
                };
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
                // The phase is left as it stands. The engine emits
                // `ToolStarted` before it asks (crates/titi-engine/src/
                // tool_loop.rs:145 then :271), so the running call keeps its
                // own clock while the person decides, and answering hands
                // the row back to that call without a second start time.
                self.hint.clear();
                Applied::none()
            }
            EngineEvent::ToolFinished {
                call_id,
                output,
                is_error,
                ..
            } => {
                // Only the call that owns the row may end it: a late
                // `ToolFinished` for another call must not wipe the elapsed
                // time of the one still running.
                if self.phase.is_tool(&call_id) {
                    // A tool only borrows the row. When it returns, the turn
                    // is the model's again — and after the turn's first
                    // token it stays `Streaming` between rounds, so a round
                    // that follows a tool is never mislabelled as the turn's
                    // first wait. The character count keeps growing from
                    // where it stood.
                    self.phase = WorkPhase::Streaming;
                }
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
            EngineEvent::Failed {
                turn_id, message, ..
            } => {
                self.push(LineKind::Error, one_line(&message, TOOL_PREVIEW));
                if turn_id.is_some() && turn_id == self.active_turn_id {
                    self.finish_turn()
                } else {
                    Applied::none()
                }
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
            EngineEvent::CouncilFinished { report } => {
                // Without an arm of its own the report falls into the
                // wildcard below and the council answers into the void.
                self.push(LineKind::Note, report.to_string());
                Applied::none()
            }
            EngineEvent::GraphFinished { report } => {
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
            EngineEvent::TurnUsage {
                prompt_tokens,
                completion_tokens,
                ..
            } => {
                self.last_prompt_tokens = prompt_tokens;
                self.last_completion_tokens = completion_tokens;
                self.session_prompt_tokens += prompt_tokens;
                self.session_completion_tokens += completion_tokens;
                Applied::none()
            }
            EngineEvent::MemoryResult { output } => {
                self.push(LineKind::Note, output.to_string());
                Applied::none()
            }
            EngineEvent::JobStarted { job } => {
                self.push(
                    LineKind::Note,
                    format!("{} started · every {}s", job.id, job.interval_secs),
                );
                self.jobs.retain(|known| known.id != job.id);
                self.jobs.push(job);
                Applied::none()
            }
            EngineEvent::JobList { jobs } => {
                self.show_jobs(&jobs);
                self.jobs = jobs;
                Applied::none()
            }
            EngineEvent::JobFinished { job_id } => {
                self.jobs.retain(|job| job.id != job_id);
                self.push(LineKind::Note, format!("{job_id} stopped"));
                Applied::none()
            }
            EngineEvent::AdvisorAnswer { text } if text.trim().is_empty() => {
                // An advisor that said nothing must not read as one that had
                // no objection.
                self.push(
                    LineKind::Error,
                    "failed consult: the advisor answered with nothing".to_owned(),
                );
                Applied::none()
            }
            EngineEvent::AdvisorAnswer { text } => {
                self.push(LineKind::Note, format!("advisor · {}", text.trim()));
                Applied::none()
            }
            EngineEvent::AdvisorFailed { reason } => {
                self.push(LineKind::Error, format!("failed consult: {reason}"));
                Applied::none()
            }
            EngineEvent::BudgetUpdated { spent, limit } => {
                self.spent_tokens = spent;
                self.budget = limit;
                Applied::none()
            }
            EngineEvent::BudgetExceeded { spent, limit } => {
                // The engine has already stopped starting turns; the screen
                // says so in the one state the user knows how to leave.
                self.spent_tokens = spent;
                self.budget = Some(limit);
                self.paused = true;
                self.push(
                    LineKind::Error,
                    format!(
                        "budget reached: {spent} of {limit} tokens · paused · /budget <amount> raises it"
                    ),
                );
                Applied::none()
            }
            EngineEvent::ModeChanged { mode } => {
                self.mode = mode;
                self.push(LineKind::Note, format!("mode: {}", mode.label()));
                Applied::none()
            }
            EngineEvent::SessionNamed { title, .. } => {
                self.session_label = title.to_string();
                Applied::none()
            }
            EngineEvent::AgentStarted { name, .. } => {
                self.push(LineKind::Tool, format!("tool agent {name}: started"));
                Applied::none()
            }
            EngineEvent::AgentFinished {
                agent_id,
                summary,
                success,
                ..
            } => {
                if success {
                    self.push(
                        LineKind::Tool,
                        format!("tool done  agent {agent_id}: {summary}"),
                    );
                } else {
                    self.push(
                        LineKind::Tool,
                        format!("tool error agent {agent_id}: {summary}"),
                    );
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
        // A pasted body is composer input, not a picker keystroke.
        self.login_picker = None;
        self.model_picker = None;
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

    fn submit(&mut self, now: Instant) -> Applied {
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
            self.turn_started = Some(now);
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
        // Once per command, not per frame: a local server may have joined
        // since the last time the list was looked at.
        let models = self.catalog.ids();
        // A provider that refused the key contributes nothing and looks
        // exactly like a provider that has nothing — say which it was, or
        // the user re-runs `/model` waiting for models that will never come.
        for failure in self.catalog.discovery_failures() {
            self.push(LineKind::Error, failure.to_string());
        }
        if models.is_empty() {
            self.push(LineKind::Error, "no models".to_owned());
            return Some(Applied::none());
        }
        // Bare `/model` opens the browser rather than cycling: a cycle hides
        // the list, and the list is what a model is chosen from.
        if rest.is_empty() {
            self.open_model_picker();
            return Some(Applied::none());
        }
        let next = if let Some(found) = models
            .iter()
            .find(|id| id.as_str() == rest || id.rsplit('/').next() == Some(rest))
        {
            found.clone()
        } else {
            self.push(LineKind::Error, format!("unknown model {rest}"));
            return Some(Applied::none());
        };
        self.model = next.clone();
        // The confirmation is the engine's: a `ModelSwitched` event is the
        // one place that knows the switch happened, and narrating it here as
        // well would print it twice.
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
            "fork" => self.fork(),
            "export" => self.export(args),
            "btw" => self.btw(args),
            "switch" => self.switch(args),
            "settings" => self.settings(),
            "duck" => self.duck(args),
            "hub" => self.toggle_hub(args),
            "join" => self.join_hub(args),
            "leave" => self.leave_hub(args),
            "loop" => self.start_loop(args),
            "jobs" => self.jobs(args),
            "advisor" => self.advisor(args),
            "budget" => self.budget(args),
            "plan" => self.plan(args),
            "done" => self.done(args),
            "goal" => self.goal(args),
            "council" => self.council(args),
            "graph" => self.graph(args),
            "memory" => self.memory(args),
            "usage" => self.usage(),
            "context" => self.describe_context(args),
            "compact" => self.compact(args),
            "help" => self.help(),
            "login" => self.login(args),
            "logout" => self.logout(args),
            "keys" | "whoami" => self.keys(),
            "git" => self.git(args),
            "diagnose" => self.diagnose(args),
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
    fn usage(&mut self) -> Applied {
        let text = format!(
            "Turn: {} prompt + {} completion. Session: {} / {}.",
            self.last_prompt_tokens,
            self.last_completion_tokens,
            self.session_prompt_tokens,
            self.session_completion_tokens
        );
        self.push(LineKind::Note, text);
        Applied::none()
    }
    fn memory(&mut self, args: &str) -> Applied {
        if args.is_empty() || args == "list" {
            return Applied::send(EngineCommand::MemoryList, None);
        }
        let (cmd, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        match cmd {
            "search" => {
                let query = rest.trim();
                if query.is_empty() {
                    self.push(LineKind::Error, "usage: /memory search <query>".to_owned());
                    Applied::none()
                } else {
                    Applied::send(
                        EngineCommand::MemorySearch {
                            query: query.into(),
                        },
                        None,
                    )
                }
            }
            "forget" => {
                let id_str = rest.trim();
                match id_str.parse::<i64>() {
                    Ok(id) => Applied::send(EngineCommand::MemoryForget { id }, None),
                    Err(_) => {
                        self.push(LineKind::Error, "usage: /memory forget <id>".to_owned());
                        Applied::none()
                    }
                }
            }
            _ => {
                self.push(LineKind::Error, format!("unknown memory command: {cmd}"));
                Applied::none()
            }
        }
    }
    fn fork(&mut self) -> Applied {
        self.session_note(crate::app::fork_session(&self.agent_dir, &self.session_id))
    }

    fn export(&mut self, args: &str) -> Applied {
        let path = args.trim();
        self.session_note(crate::app::export_session(
            &self.agent_dir,
            &self.session_id,
            path,
        ))
    }

    fn btw(&mut self, args: &str) -> Applied {
        if args.is_empty() {
            self.push(LineKind::Error, "usage: /btw <message>".to_owned());
            return Applied::none();
        }
        let text = format!("btw: {args}");
        self.push(LineKind::Note, text.clone());
        let log = None; // Do not log it into history
        if self.turn_active {
            Applied::send(EngineCommand::Steer { text: text.into() }, log)
        } else {
            self.turn_active = true;
            self.turn_started = Some(Instant::now());
            Applied::send(EngineCommand::SubmitPrompt { text: text.into() }, log)
        }
    }

    fn settings(&mut self) -> Applied {
        match titi_config::settings::Settings::load(
            &self.agent_dir,
            &crate::app::current_workspace(),
            &[],
        ) {
            Ok(settings) => {
                let mut lines = Vec::new();
                for (key, (source, val)) in settings.flatten() {
                    lines.push(format!("{key} = {val} ({source})"));
                }
                if settings.is_empty() {
                    self.push(
                        LineKind::Note,
                        titi_config::settings::NO_SETTINGS_NOTE.to_owned(),
                    );
                } else {
                    self.push(LineKind::Note, lines.join("\n"));
                }
            }
            Err(e) => {
                self.push(LineKind::Error, format!("failed to load settings: {e}"));
            }
        }
        Applied::none()
    }

    fn switch(&mut self, args: &str) -> Applied {
        if args.is_empty() {
            // Bare `/switch` is the same browser as bare `/model`: naming a
            // model by hand and picking it from the list are one action.
            if self.catalog.ids().is_empty() {
                self.push(LineKind::Error, "no models".to_owned());
                return Applied::none();
            }
            self.open_model_picker();
            return Applied::none();
        }

        let models = self.catalog.ids();
        if models.is_empty() {
            self.push(LineKind::Error, "no models".to_owned());
            return Applied::none();
        }

        let (base_query, level) = if models.contains(&args.to_owned()) {
            (args, None)
        } else if let Some((q, lvl)) = args.rsplit_once(':') {
            let lvl_lower = lvl.to_lowercase();
            if matches!(
                lvl_lower.as_str(),
                "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            ) {
                (q, Some(lvl))
            } else {
                (args, None)
            }
        } else {
            (args, None)
        };

        let search_query = if let Some(role) = base_query.strip_prefix('@') {
            if let Ok(settings) = titi_config::settings::Settings::load(
                &self.agent_dir,
                &crate::app::current_workspace(),
                &[],
            ) {
                if settings.get("modelRoles").is_none() {
                    self.push(LineKind::Error, "no model roles configured".to_owned());
                    return Applied::none();
                }
                match titi_config::roles::resolve_model_role(&settings, role, &self.model) {
                    Ok(resolved) => resolved,
                    Err(_) => {
                        self.push(LineKind::Error, format!("no such role @{role}"));
                        return Applied::none();
                    }
                }
            } else {
                self.push(LineKind::Error, "no model roles configured".to_owned());
                return Applied::none();
            }
        } else {
            base_query.to_owned()
        };

        let mut candidates = Vec::new();

        for model in &models {
            if model == &search_query {
                candidates.push(model);
                break;
            }
        }

        if candidates.is_empty() {
            for model in &models {
                if model.rsplit('/').next() == Some(&search_query) {
                    candidates.push(model);
                }
            }
        }

        if candidates.is_empty() {
            let query_lower = search_query.to_lowercase();
            for model in &models {
                if model.to_lowercase().contains(&query_lower) {
                    candidates.push(model);
                }
            }
        }

        if candidates.is_empty() {
            self.push(
                LineKind::Error,
                format!("no model matches \"{args}\"; try /model to see the list"),
            );
            return Applied::none();
        }

        if candidates.len() > 1 {
            let top3: Vec<_> = candidates.into_iter().take(3).map(|s| s.as_str()).collect();
            self.push(
                LineKind::Note,
                format!(
                    "multiple models match \"{args}\", candidates: {}",
                    top3.join(", ")
                ),
            );
            return Applied::none();
        }

        let mut next = candidates[0].clone();
        if let Some(lvl) = level {
            next = format!("{next}:{lvl}");
        }

        self.model = next.clone();
        // One confirmation per switch, and it is the engine's `ModelSwitched`
        // that prints it: a command that narrates its own switch announces
        // one the engine may refuse.
        Applied::send(EngineCommand::SwitchModel { model: next.into() }, None)
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
                    self.turn_started = None;
                    self.phase = WorkPhase::Waiting;
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
                    let mut text = format!("{} · {}", section.title, section.summary);
                    if !section.lines.is_empty() {
                        text.push_str("\n  ");
                        text.push_str(&section.lines.join("\n  "));
                    }
                    self.push(LineKind::Note, text);
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

    /// Typing while the subscription picker is up. Arrows move, Enter signs
    /// in to the highlighted row, Esc closes without writing; anything else
    /// closes the picker and is handled as ordinary composer input.
    fn login_picker_key(&mut self, key: Key, now: Instant) -> Applied {
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
    fn model_picker_key(&mut self, key: Key, now: Instant) -> Applied {
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
    fn open_model_picker(&mut self) {
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

    /// Typing while a login prompt is up. In OAuth mode the line is the
    /// pasted code or redirect URL and Enter hands it to the flow; otherwise
    /// it is the API key and Enter stores it.
    fn login_key(&mut self, key: Key) -> Applied {
        // The device grant finishes in the browser: there is no line to type,
        // so only the way out is read.
        let device = self
            .oauth
            .as_ref()
            .is_some_and(|login| login.method == LoginMethod::Device);
        match key {
            Key::Esc | Key::CtrlC => self.cancel_login(),
            _ if device => Applied::none(),
            Key::Char(ch) if !ch.is_control() => {
                self.input.push(ch);
                Applied::none()
            }
            Key::Backspace => {
                self.input.pop();
                Applied::none()
            }
            Key::Enter => self.store_login_secret(),
            _ => Applied::none(),
        }
    }

    /// Leaves the login prompt. Dropping the flow is the cancel: the task's
    /// code end sees the channel close and stops waiting.
    fn cancel_login(&mut self) -> Applied {
        self.input.clear();
        self.login_for = None;
        self.oauth = None;
        self.push(LineKind::Note, "login cancelled".to_owned());
        Applied::none()
    }

    fn store_login_secret(&mut self) -> Applied {
        let secret = std::mem::take(&mut self.input);
        let secret = secret.trim().to_owned();
        if self.oauth.is_some() {
            if secret.is_empty() {
                self.push(LineKind::Error, "login: a code is required".to_owned());
                return Applied::none();
            }
            let delivered = self
                .oauth
                .as_ref()
                .is_some_and(|oauth| oauth.flow.codes.send(secret).is_ok());
            if delivered {
                self.push(LineKind::Note, "login: code submitted".to_owned());
            } else {
                self.push(LineKind::Error, "login: the flow ended".to_owned());
            }
            return Applied::none();
        }
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
            // Bare `/login`: offer what the user can sign into rather than
            // make them remember a provider id.
            self.login_picker = Some(0);
            return Applied::none();
        };
        let second = parts.next();
        if parts.next().is_some() {
            self.push(
                LineKind::Error,
                "usage: /login <provider> [key|device]".to_owned(),
            );
            return Applied::none();
        }
        if !self.known_provider(provider) {
            self.push(
                LineKind::Error,
                format!("login: unknown provider {provider}"),
            );
            return Applied::none();
        }
        if second == Some("device") {
            return self.start_device_login(provider);
        }
        if let Some(secret) = second {
            return self.store_inline_key(provider, secret);
        }
        if let Some(descriptor) = crate::login::find(provider) {
            return self.start_oauth_login(descriptor, LoginMethod::Browser);
        }
        self.login_for = Some(provider.to_owned());
        self.push(
            LineKind::Note,
            format!("login {provider}: paste the key, enter stores it"),
        );
        Applied::none()
    }

    /// `/login <provider> device`: the grant that needs no callback server.
    /// Only a subscription descriptor that advertises one can run it, and a
    /// provider with no descriptor has no device flow at all — saying so
    /// beats typing a URL that does not exist.
    fn start_device_login(&mut self, provider: &str) -> Applied {
        match crate::login::find(provider) {
            Some(descriptor) if descriptor.supports_device => {
                self.start_oauth_login(descriptor, LoginMethod::Device)
            }
            Some(descriptor) => {
                self.push(
                    LineKind::Error,
                    format!("login: {} has no device flow", descriptor.name),
                );
                Applied::none()
            }
            None => {
                self.push(
                    LineKind::Error,
                    format!("login: {provider} has no device flow"),
                );
                Applied::none()
            }
        }
    }

    /// `/login <provider>` for a provider that signs in through a browser,
    /// or its device-code grant.
    ///
    /// Nothing here waits: the driver starts the flow on the runtime the CLI
    /// already entered, and the frames after this one read what it reports.
    fn start_oauth_login(
        &mut self,
        descriptor: &'static OAuthProvider,
        method: LoginMethod,
    ) -> Applied {
        let driver = match self.login_driver() {
            Ok(driver) => driver,
            Err(reason) => {
                self.push(LineKind::Error, format!("login: {reason}"));
                return Applied::none();
            }
        };
        let started = match method {
            LoginMethod::Browser => driver.begin(descriptor),
            LoginMethod::Device => driver.begin_device(descriptor),
        };
        match started {
            Ok(flow) => {
                self.login_for = Some(descriptor.id.to_owned());
                self.oauth = Some(OAuthLogin {
                    provider: descriptor,
                    flow,
                    method,
                });
                let waiting = match method {
                    LoginMethod::Browser => "waiting for the browser",
                    LoginMethod::Device => "waiting for the device code",
                };
                self.push(
                    LineKind::Note,
                    format!("login {}: {waiting}", descriptor.id),
                );
            }
            Err(reason) => self.push(LineKind::Error, format!("login: {reason}")),
        }
        Applied::none()
    }

    fn login_driver(&mut self) -> Result<Arc<dyn LoginDriver>, String> {
        if let Some(driver) = &self.login_driver {
            return Ok(Arc::clone(driver));
        }
        let driver: Arc<dyn LoginDriver> = Arc::new(crate::login::ChannelDriver::new()?);
        self.login_driver = Some(Arc::clone(&driver));
        Ok(driver)
    }

    /// Replace the OAuth driver. Tests inject a fake so no network is used;
    /// production leaves it unset and builds the terminal one on first login.
    pub fn set_login_driver(&mut self, driver: Arc<dyn LoginDriver>) {
        self.login_driver = Some(driver);
    }

    /// Point the screen at another agent directory — the store `/login`
    /// writes and `/keys` reads.
    pub fn set_agent_dir(&mut self, dir: impl Into<PathBuf>) {
        self.agent_dir = dir.into();
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

    /// Drains whatever the login reported since the last frame.
    ///
    /// Always `try_recv`: the flow runs on its own task, and the render loop
    /// must not wait on a browser, a socket or a person.
    pub fn poll_login(&mut self) {
        let mut reported = Vec::new();
        let mut ended = false;
        if let Some(login) = self.oauth.as_mut() {
            loop {
                match login.flow.events.try_recv() {
                    Ok(event) => reported.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
        }
        for event in reported {
            match event {
                LoginEvent::Url { url, instructions } => {
                    let (id, method) = self
                        .oauth
                        .as_ref()
                        .map(|login| (login.provider.id, login.method))
                        .unwrap_or(("", LoginMethod::Browser));
                    let opener = match method {
                        LoginMethod::Browser => "open this URL in your browser",
                        LoginMethod::Device => "open this URL on any device",
                    };
                    self.push(
                        LineKind::Note,
                        format!("login {id}: {opener}\n{url}\n{instructions}"),
                    );
                }
                LoginEvent::Progress(message) => self.push(LineKind::Note, message),
                LoginEvent::Done(tokens) => self.finish_login(&tokens),
                LoginEvent::Failed(reason) => {
                    self.login_for = None;
                    self.oauth = None;
                    self.push(LineKind::Error, format!("login: {reason}"));
                }
            }
        }
        if ended && self.oauth.is_some() {
            let login_for = self.login_for.take();
            self.oauth = None;
            let id = login_for.unwrap_or_else(|| "provider".to_owned());
            self.push(LineKind::Error, format!("login {id}: the flow ended"));
        }
    }

    /// The credential the flow came back with: store it, then say who signed
    /// in and let the model list catch up.
    fn finish_login(&mut self, tokens: &titi_providers::oauth::OAuthTokens) {
        let Some(login) = self.oauth.take() else {
            return;
        };
        self.login_for = None;
        let provider = login.provider;
        match crate::secrets::store_oauth(&self.agent_dir, provider.store_as, tokens) {
            Ok(()) => {
                self.push(
                    LineKind::Note,
                    crate::login::identity_line(provider, tokens),
                );
                self.catalog.refresh_after_login();
            }
            Err(reason) => self.push(LineKind::Error, format!("login: {reason}")),
        }
    }

    /// `/login <provider> <key>`. A provider with no `credential_env` reads
    /// its credential only from the store, which a sign-in writes: a key
    /// pasted for it would sit there and never be used, so it is refused.
    fn store_inline_key(&mut self, provider: &str, secret: &str) -> Applied {
        if !self.accepts_api_key(provider) {
            let hint = if crate::login::find(provider).is_some() {
                format!("login: {provider} takes no API key — use /login {provider} to sign in")
            } else {
                format!("login: {provider} takes no API key")
            };
            self.push(LineKind::Error, hint);
            return Applied::none();
        }
        match crate::secrets::store_key(&self.agent_dir, provider, secret) {
            Ok(()) => self.push(LineKind::Note, format!("{provider}: key stored")),
            Err(reason) => self.push(LineKind::Error, format!("login: {reason}")),
        }
        Applied::none()
    }

    /// Whether an API key is something this provider can read: a descriptor
    /// with no `credential_env` has no api-key path at all.
    fn accepts_api_key(&self, id: &str) -> bool {
        self.registry_providers()
            .iter()
            .any(|provider| provider.id.as_str() == id && provider.credential_env.is_some())
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
        if !self.known_provider(provider) {
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

    /// The providers the engine actually runs on: the builtins with this
    /// agent directory's `providers` config merged over them. A provider the
    /// user declared must be one `/login` accepts, or the key for a model
    /// the engine will call cannot be stored from the screen at all.
    fn registry_providers(&self) -> Vec<titi_engine::ProviderDescriptor> {
        crate::engine::registry_config_for(&self.agent_dir, &crate::app::current_workspace())
            .providers
    }

    fn known_provider(&self, id: &str) -> bool {
        self.registry_providers()
            .iter()
            .any(|provider| provider.id.as_str() == id)
    }

    /// `/keys` reads the store, not the environment: a signed-in provider
    /// looks exactly like a keyless one otherwise, and the kind and the
    /// remaining lifetime are what say whether it is about to stop working.
    fn keys(&mut self) -> Applied {
        let now = crate::secrets::now_secs();
        let stored = crate::secrets::list_keys(&self.agent_dir).unwrap_or_default();
        let mut listed: Vec<String> = Vec::new();
        for provider in self.registry_providers() {
            let id = provider.id.to_string();
            let credential = Credential::of(&provider, &stored);
            // Both facts matter: an environment variable hides neither the
            // sign-in under it nor its remaining lifetime, and a signed-in
            // provider is exactly the one whose token is about to expire.
            let status = match (&credential.stored, credential.from_env) {
                (Some(row), true) => format!("env + {}", crate::secrets::describe_key(row, now)),
                (Some(row), false) => crate::secrets::describe_key(row, now),
                (None, true) => "env".to_owned(),
                (None, false) => "no key".to_owned(),
            };
            listed.push(id.clone());
            self.push(LineKind::Note, format!("{id}  {status}"));
        }
        // A credential whose provider left the config still exists, and this
        // is the only place that would say so.
        let orphans: Vec<&crate::secrets::StoredKey> = stored
            .iter()
            .filter(|row| !listed.contains(&row.provider))
            .collect();
        for row in orphans {
            let status = crate::secrets::describe_key(row, now);
            self.push(LineKind::Note, format!("{}  {status}", row.provider));
        }
        Applied::none()
    }

    /// `/git [status|diff]` puts the read-only git view in the transcript.
    ///
    /// Only the two read verbs exist here. Committing is a write, and a
    /// write goes through the tool with its approval prompt; a slash command
    /// has no such prompt, so it must not become the way around one.
    fn git(&mut self, args: &str) -> Applied {
        let argv: &[&str] = match args.trim() {
            "" | "status" => &["status", "--short", "--branch", "--", "."],
            "diff" => &["diff", "--", "."],
            other => {
                self.push(
                    LineKind::Error,
                    format!(
                        "/git takes status or diff, not {other}: a commit or a push stays with \
                         the tool, behind its approval"
                    ),
                );
                return Applied::none();
            }
        };
        let op = argv.first().copied().unwrap_or("status");
        match run_git(&crate::app::current_workspace(), argv) {
            Ok(output) => {
                let body = output.trim_end();
                let text = if body.is_empty() {
                    format!("git {op}: nothing to show")
                } else {
                    format!("git {op}\n{}", redacted(body))
                };
                self.push(LineKind::Note, text);
            }
            Err(error) => self.push(LineKind::Error, format!("git {op}: {error}")),
        }
        Applied::none()
    }

    /// `/diagnose` prints, as one block to paste into a bug report, what a
    /// report needs: version, model, providers, where the settings came
    /// from, and the state of the repository.
    ///
    /// A provider is a name and whether a key is in reach; settings are
    /// their sources, never their values. This block is written to be
    /// pasted in public, so no stored value may enter it.
    fn diagnose(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /diagnose (got {args})"));
            return Applied::none();
        }
        let workspace = crate::app::current_workspace();
        let mut rows = vec![
            format!("titi {}", titi_tui::VERSION),
            format!("model: {} · mode: {}", self.model, self.mode.label()),
            format!("session: {}", self.session_id),
            format!("workspace: {}", workspace.display()),
            format!("agent dir: {}", self.agent_dir.display()),
            format!("providers: {}", self.provider_status().join(", ")),
        ];
        match titi_config::settings::Settings::load(&self.agent_dir, &workspace, &[]) {
            Ok(settings) => {
                let mut sources: Vec<String> = Vec::new();
                for (_, (source, _)) in settings.flatten() {
                    if !sources.iter().any(|seen| seen == &source) {
                        sources.push(source);
                    }
                }
                let sources = if settings.is_empty() {
                    titi_config::settings::NO_SETTINGS_NOTE.to_owned()
                } else {
                    sources.join(", ")
                };
                rows.push(format!("config: {sources}"));
            }
            Err(error) => rows.push(format!("config: unreadable ({error})")),
        }
        // The ranked map itself lives in the engine, behind its lock; what the
        // screen can say without indexing the repo is whether it is on, and
        // `/context` reports what this turn's map costs.
        let genome = if std::env::var_os("TITI_NO_GENOME").is_none() {
            "on"
        } else {
            "off (TITI_NO_GENOME)"
        };
        rows.push(format!("genome: {genome}"));
        rows.push(format!(
            "tokens: {} prompt + {} completion this session",
            self.session_prompt_tokens, self.session_completion_tokens
        ));
        rows.push(format!("repo: {}", repo_state(&workspace)));
        self.push(LineKind::Note, rows.join("\n"));
        Applied::none()
    }

    /// Every known provider and whether a credential is in reach — names,
    /// kinds and lifetimes only, never a value.
    fn provider_status(&self) -> Vec<String> {
        let now = crate::secrets::now_secs();
        let stored = crate::secrets::list_keys(&self.agent_dir).unwrap_or_default();
        self.registry_providers()
            .into_iter()
            .map(|provider| {
                let credential = Credential::of(&provider, &stored);
                let status = if credential.from_env {
                    "env".to_owned()
                } else if let Some(row) = &credential.stored {
                    crate::secrets::describe_key(row, now)
                } else {
                    "no key".to_owned()
                };
                format!("{} ({status})", provider.id)
            })
            .collect()
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

    /// `/council` puts a question to a council of briefs. Like `/goal`, it
    /// never becomes `SubmitPrompt`.
    fn council(&mut self, args: &str) -> Applied {
        let question = args.trim();
        if question.is_empty() {
            self.push(LineKind::Error, "usage: /council <question>".to_owned());
            return Applied::none();
        }
        self.push(LineKind::Note, format!("council: {question}"));
        Applied::effect(ChatEffect::Send(EngineCommand::RunCouncil {
            question: question.into(),
        }))
    }

    /// `/graph` runs the orchestrator graph over a task. Like `/goal`, it
    /// never becomes `SubmitPrompt`.
    fn graph(&mut self, args: &str) -> Applied {
        let task = args.trim();
        if task.is_empty() {
            self.push(LineKind::Error, "usage: /graph <task>".to_owned());
            return Applied::none();
        }
        self.push(LineKind::Note, format!("graph: {task}"));
        Applied::effect(ChatEffect::Send(EngineCommand::RunGraph {
            task: task.into(),
        }))
    }

    /// `/loop <interval> <prompt>` hands a repeating prompt to the engine.
    ///
    /// Nothing is scheduled here: the screen may be closed and reopened, and
    /// a timer living in the composer would die with it.
    fn start_loop(&mut self, args: &str) -> Applied {
        let (interval, prompt) = match parse_loop(args) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.push(LineKind::Error, error.to_string());
                return Applied::none();
            }
        };
        self.push(LineKind::Note, format!("loop every {interval}s: {prompt}"));
        Applied::effect(ChatEffect::Send(EngineCommand::StartLoop {
            interval_secs: interval,
            prompt: prompt.into(),
        }))
    }

    /// `/jobs` lists the engine's background loops, `/jobs cancel <id>`
    /// stops one.
    fn jobs(&mut self, args: &str) -> Applied {
        if args.is_empty() || args == "list" {
            return Applied::effect(ChatEffect::Send(EngineCommand::ListJobs));
        }
        let (cmd, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let id = rest.trim();
        if cmd != "cancel" && cmd != "stop" {
            self.push(LineKind::Error, format!("unknown jobs command: {cmd}"));
            return Applied::none();
        }
        if id.is_empty() {
            self.push(LineKind::Error, "usage: /jobs cancel <id>".to_owned());
            return Applied::none();
        }
        Applied::effect(ChatEffect::Send(EngineCommand::CancelJob {
            job_id: id.into(),
        }))
    }

    /// Renders what the engine reported for `/jobs`.
    fn show_jobs(&mut self, jobs: &[JobInfo]) {
        if jobs.is_empty() {
            self.push(LineKind::Note, "no background jobs".to_owned());
            return;
        }
        for job in jobs {
            self.push(
                LineKind::Note,
                format!(
                    "{}  every {}s  ran {}  ·  {}",
                    job.id,
                    job.interval_secs,
                    job.runs,
                    one_line(&job.prompt, TOOL_PREVIEW)
                ),
            );
        }
    }

    /// `/plan` hands the engine a read-only mode: the next turns can look
    /// at the repository but not change it.
    fn plan(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /plan (got {args})"));
            return Applied::none();
        }
        if self.mode == SessionMode::Plan {
            self.push(LineKind::Note, "already planning · /done exits".to_owned());
            return Applied::none();
        }
        Applied::effect(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Plan,
        }))
    }

    /// `/duck` is the repo-blind partner: no file or shell tool at all.
    fn duck(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /duck (got {args})"));
            return Applied::none();
        }
        if self.mode == SessionMode::Duck {
            self.push(LineKind::Note, "already ducking · /done exits".to_owned());
            return Applied::none();
        }
        Applied::effect(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Duck,
        }))
    }

    /// `/done` leaves plan or duck mode and acts again.
    fn done(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /done (got {args})"));
            return Applied::none();
        }
        if self.mode == SessionMode::Agent {
            self.push(LineKind::Note, "already in agent mode".to_owned());
            return Applied::none();
        }
        Applied::effect(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Agent,
        }))
    }

    /// `/join [name]` puts this session on the local hub roster. The name
    /// defaults to the session id, which is what other peers address.
    fn join_hub(&mut self, args: &str) -> Applied {
        let name = args.trim();
        let name = if name.is_empty() {
            self.session_id.clone()
        } else {
            name.to_owned()
        };
        let agent_dir = self.agent_dir.clone();
        match self.hub.join(&agent_dir, &name) {
            Ok(()) => {
                let peers = self.hub.peers().len();
                self.hub_open = true;
                self.push(
                    LineKind::Note,
                    format!("joined the hub as {name} · {peers} on the roster"),
                );
            }
            // A missing broker is the ordinary case, not a broken screen.
            Err(reason) => self.push(LineKind::Note, format!("hub: {reason}")),
        }
        Applied::none()
    }

    /// `/leave` drops the hub connection, which unregisters this peer.
    fn leave_hub(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /leave (got {args})"));
            return Applied::none();
        }
        match self.hub.leave() {
            Some(id) => {
                self.hub_open = false;
                self.push(LineKind::Note, format!("left the hub as {id}"));
            }
            None => self.push(LineKind::Note, "hub: not joined".to_owned()),
        }
        Applied::none()
    }

    /// `/hub` shows or hides the roster panel.
    fn toggle_hub(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Error, format!("usage: /hub (got {args})"));
            return Applied::none();
        }
        self.hub_open = !self.hub_open;
        if self.hub_open && !self.hub.joined() {
            self.push(
                LineKind::Note,
                "hub: not joined · /join connects".to_owned(),
            );
        }
        Applied::none()
    }

    /// Drains whatever the broker pushed since the last frame.
    ///
    /// Always `try_recv`: the hub is a convenience, and the chat loop must
    /// not wait on a socket that may have no one behind it.
    pub fn poll_hub(&mut self) {
        for update in self.hub.poll() {
            match update {
                HubUpdate::Roster => {}
                HubUpdate::Message { from, to, message } => {
                    let scope = if to.is_some() { "" } else { " (all)" };
                    self.push(
                        LineKind::Note,
                        format!("hub {from}{scope}: {}", one_line(&message, TOOL_PREVIEW)),
                    );
                }
                HubUpdate::Refused(reason) => {
                    self.push(LineKind::Error, format!("hub: {reason}"));
                }
                HubUpdate::Disconnected => {
                    self.hub_open = false;
                    self.push(LineKind::Error, "hub: the broker went away".to_owned());
                }
            }
        }
    }

    /// `/advisor [question]` asks a toolless second opinion about this
    /// conversation. It is not a turn: nothing it says is acted on.
    fn advisor(&mut self, args: &str) -> Applied {
        let question = args.trim();
        self.push(
            LineKind::Note,
            if question.is_empty() {
                "consulting the advisor".to_owned()
            } else {
                format!("consulting the advisor: {question}")
            },
        );
        Applied::effect(ChatEffect::Send(EngineCommand::Consult {
            question: (!question.is_empty()).then(|| question.into()),
        }))
    }

    /// `/budget [amount|off]` caps what this session may spend.
    ///
    /// The cap is counted in tokens. Money is not offered: nothing in the
    /// project knows what a model costs, and a dollar figure derived from a
    /// made-up rate would be a number the user could not act on.
    fn budget(&mut self, args: &str) -> Applied {
        let args = args.trim();
        if args.is_empty() {
            self.show_budget();
            return Applied::none();
        }
        if matches!(args, "off" | "none" | "clear") {
            self.push(LineKind::Note, "budget: no cap".to_owned());
            return Applied::send(EngineCommand::SetBudget { tokens: None }, None);
        }
        match parse_budget(args) {
            Ok(tokens) => {
                self.push(LineKind::Note, format!("budget: {tokens} tokens"));
                Applied::send(
                    EngineCommand::SetBudget {
                        tokens: Some(tokens),
                    },
                    None,
                )
            }
            Err(error) => {
                self.push(LineKind::Error, error.to_string());
                Applied::none()
            }
        }
    }

    /// What has been spent, against the cap if there is one.
    fn show_budget(&mut self) {
        let text = match self.budget {
            Some(limit) => format!(
                "budget: {} of {limit} tokens spent ({}%), estimated",
                self.spent_tokens,
                share(self.spent_tokens, limit)
            ),
            None => format!(
                "budget: no cap · {} tokens spent (estimated)",
                self.spent_tokens
            ),
        };
        self.push(LineKind::Note, text);
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
        self.turn_started = None;
        // No phase outlives its turn: the next one starts in `Waiting`, and
        // an idle chat must not be left holding a tool's clock.
        self.phase = WorkPhase::Waiting;
        self.active_turn_id = None;
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
        self.scroll_offset = 0;
    }

    /// The transcript as it stands. A cast replay renders these lines; the
    /// live screen draws them.
    pub fn transcript(&self) -> &[TranscriptLine] {
        &self.lines
    }

    /// Puts a line in the transcript as if the user had typed and sent it.
    /// Replay has no keyboard, and a cast without its prompts is half a
    /// conversation.
    pub fn push_user(&mut self, text: &str) {
        self.push(LineKind::User, text.to_owned());
    }

    /// How long the running turn has been in flight. `None` when none is.
    fn turn_elapsed(&self) -> Option<Duration> {
        self.turn_started.map(|started| started.elapsed())
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
///
/// `cast` is where `--record` writes the session: every engine event and
/// every prompt the user sends, in the order the screen saw them.
pub fn run(
    mut engine: Engine,
    session_log: Option<SessionLog>,
    catalog: crate::engine::ModelCatalog,
    session_id: String,
    mut cast: Option<crate::ompcast::CastWriter>,
) -> io::Result<()> {
    let models = catalog.ids();
    let model = models
        .first()
        .cloned()
        .unwrap_or_else(|| "model".to_owned());
    // The screen's colours come from the theme, so the theme is resolved before
    // the first frame: the same resolver the rest of the CLI uses, which maps
    // the terminal's appearance onto the dark (`titanium`) or light slot and
    // lets `{agent_dir}/themes/<name>.json` stand in for any name the built-in
    // registry does not have.
    let theme = crate::app::default_theme().map_err(io::Error::other)?;
    let mut chat = Chat::new(model, &session_id, theme);
    chat.catalog = catalog;
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
        if pump(&mut engine, &mut chat, &session_log, &mut cast)? {
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
        about: "which providers have a key or a sign-in",
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

/// A way to sign in to a subscription provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginMethod {
    /// Authorization code over the loopback callback, with the manual paste
    /// as the fallback.
    Browser,
    /// The device grant: a code entered at a URL, on this machine or another.
    Device,
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

/// What credential a provider has in reach, without its value.
///
/// `/keys`, `/diagnose` and the model picker all ask the same question — is
/// there a token, a key or a variable behind this provider — so they read it
/// here once, in one order: the environment first, the auth store second.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Credential {
    from_env: bool,
    stored: Option<crate::secrets::StoredKey>,
}

impl Credential {
    fn of(
        provider: &titi_engine::ProviderDescriptor,
        stored: &[crate::secrets::StoredKey],
    ) -> Self {
        let from_env = provider
            .credential_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .is_some_and(|value| !value.trim().is_empty());
        Self {
            from_env,
            stored: stored
                .iter()
                .find(|row| row.provider == provider.id.as_str())
                .cloned(),
        }
    }

    /// The kind in one word — `oauth` for a subscription, `key` for an API
    /// key, `env` for a variable — and `None` when the provider has nothing,
    /// which is not worth a chip on every row of its group.
    fn label(&self) -> Option<&str> {
        if self.from_env {
            return Some("env");
        }
        let kind = self.stored.as_ref()?.kind.as_str();
        Some(match kind {
            "api_key" => "key",
            other => other,
        })
    }
}

/// One catalog model as the picker states it: the id `/switch` takes, the
/// provider that runs it, the window its descriptor declares, and the
/// credential that provider holds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelRow {
    id: String,
    provider: String,
    context_window: Option<u64>,
    credential: Option<String>,
}

/// One offer in the model picker: a configured role, or a catalog model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelOffer {
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
    fn target(&self) -> &str {
        match self {
            Self::Role { model, .. } => model,
            Self::Model(row) => &row.id,
        }
    }
}

/// The model browser: bare `/model` and bare `/switch` open it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelPicker {
    /// Roles first, then the catalog in catalog order — the order the screen
    /// falls back to when nothing ranks above anything else.
    offers: Vec<ModelOffer>,
    /// The typed filter. Matched as a subsequence against `provider/model`,
    /// so `cdx` finds `openai-codex/…`; a query starting with `@` means the
    /// roles and nothing else.
    query: String,
    /// The selection, as an index into [`ModelPicker::matched`].
    selected: usize,
}

impl ModelPicker {
    /// The offers the query keeps, best match first. Ties keep catalog
    /// order, so rows never swap under the cursor while a query grows.
    fn matched(&self) -> Vec<usize> {
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
fn model_rows(chat: &Chat) -> Vec<ModelRow> {
    let config =
        crate::engine::registry_config_for(&chat.agent_dir, &crate::app::current_workspace());
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
        &crate::app::current_workspace(),
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
fn fuzzy_score(query: &str, haystack: &str) -> Option<i32> {
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
fn ellipsis_label(text: &str, room: usize) -> String {
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
fn model_row_label(row: &ModelRow, current: bool, room: usize) -> String {
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

/// Why `/loop` could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopArgError {
    Missing,
    BadInterval(String),
    ZeroInterval,
    NoPrompt,
}

impl std::fmt::Display for LoopArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing | Self::NoPrompt => {
                f.write_str("usage: /loop <interval> <prompt>  (interval: 90s, 5m, 2h)")
            }
            Self::BadInterval(word) => write!(f, "loop: {word} is not an interval (90s, 5m, 2h)"),
            Self::ZeroInterval => f.write_str("loop: the interval must be at least one second"),
        }
    }
}

/// `<interval> <prompt>` as seconds and the prompt behind it.
///
/// A bare number means seconds; `s`, `m`, and `h` suffixes are the same
/// number scaled. An unreadable interval is refused instead of defaulted:
/// a loop that fires on a guessed schedule is worse than one that never
/// started.
fn parse_loop(args: &str) -> Result<(u64, &str), LoopArgError> {
    let args = args.trim();
    if args.is_empty() {
        return Err(LoopArgError::Missing);
    }
    let (head, rest) = args
        .split_once(char::is_whitespace)
        .ok_or(LoopArgError::NoPrompt)?;
    let prompt = rest.trim();
    if prompt.is_empty() {
        return Err(LoopArgError::NoPrompt);
    }
    let interval =
        parse_interval(head).ok_or_else(|| LoopArgError::BadInterval(head.to_owned()))?;
    if interval == 0 {
        return Err(LoopArgError::ZeroInterval);
    }
    Ok((interval, prompt))
}

/// `90`, `90s`, `5m`, `2h` in seconds. `None` for anything else.
fn parse_interval(word: &str) -> Option<u64> {
    let (digits, scale) = match word.as_bytes().last()? {
        b's' => (&word[..word.len() - 1], 1),
        b'm' => (&word[..word.len() - 1], 60),
        b'h' => (&word[..word.len() - 1], 3_600),
        _ => (word, 1),
    };
    digits.parse::<u64>().ok()?.checked_mul(scale)
}

/// Why `/budget` could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BudgetArgError {
    /// A cap in money, which nothing here can convert into tokens.
    Money,
    Unreadable(String),
    Zero,
}

impl std::fmt::Display for BudgetArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Money => f.write_str(
                "budget: no price table, so a cap in money cannot be enforced — cap tokens instead (e.g. /budget 200k)",
            ),
            Self::Unreadable(word) => {
                write!(f, "budget: {word} is not an amount (200000, 200k, 1.5m, off)")
            }
            Self::Zero => f.write_str("budget: the cap must be at least one token"),
        }
    }
}

/// `200000`, `200k`, `1.5m` as tokens.
fn parse_budget(word: &str) -> Result<u64, BudgetArgError> {
    if word.starts_with('$') {
        return Err(BudgetArgError::Money);
    }
    let unreadable = || BudgetArgError::Unreadable(word.to_owned());
    let (digits, scale) = match word.as_bytes().last().ok_or_else(unreadable)? {
        b'k' | b'K' => (&word[..word.len() - 1], 1_000.0),
        b'm' | b'M' => (&word[..word.len() - 1], 1_000_000.0),
        _ => (word, 1.0),
    };
    let amount: f64 = digits.parse().map_err(|_| unreadable())?;
    if !amount.is_finite() || amount < 0.0 {
        return Err(unreadable());
    }
    let tokens = (amount * scale).round() as u64;
    if tokens == 0 {
        return Err(BudgetArgError::Zero);
    }
    Ok(tokens)
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

/// Why a `/git` or `/diagnose` call produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitRunError {
    Spawn { reason: String },
    TimedOut { seconds: u64 },
    Failed { output: String },
}

impl std::fmt::Display for GitRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitRunError::Spawn { reason } => write!(f, "git could not be run: {reason}"),
            GitRunError::TimedOut { seconds } => {
                write!(f, "timed out after {seconds}s and was killed")
            }
            GitRunError::Failed { output } => write!(f, "{output}"),
        }
    }
}

/// One read-only `git` call in `root`: no shell, no pager, no editor, and no
/// credential prompt.
///
/// Both pipes are drained on their own threads because a diff larger than the
/// pipe buffer would otherwise block the child forever and turn every big
/// diff into a timeout.
fn run_git(root: &Path, argv: &[&str]) -> Result<String, GitRunError> {
    let mut child = std::process::Command::new("git")
        .arg("--no-pager")
        .args(argv)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitRunError::Spawn {
            reason: error.to_string(),
        })?;

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || out_pipe.map(drain_pipe).unwrap_or_default());
    let err_reader = std::thread::spawn(move || err_pipe.map(drain_pipe).unwrap_or_default());

    let deadline = Instant::now() + SLASH_GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitRunError::Spawn {
                    reason: error.to_string(),
                });
            }
            Ok(Some(status)) => break status,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitRunError::TimedOut {
                seconds: SLASH_GIT_TIMEOUT.as_secs(),
            });
        }
        std::thread::sleep(GIT_POLL);
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let output = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(GitRunError::Failed {
        output: one_line(output.trim(), 200),
    })
}

/// Reads a pipe to the end, keeping at most [`GIT_OUTPUT_CAP`] bytes. The
/// tail is still read and dropped so the child never blocks on a full pipe.
fn drain_pipe(mut source: impl Read) -> String {
    let mut buffer = [0_u8; 4096];
    let mut kept: Vec<u8> = Vec::new();
    while let Ok(read) = source.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let room = GIT_OUTPUT_CAP.saturating_sub(kept.len());
        let take = room.min(read);
        kept.extend_from_slice(&buffer[..take]);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// The masking the engine puts on tool output before it reaches this
/// transcript. `/git` shells out on its own, so it applies the same pass by
/// hand: a diff that touches a key reads `[redacted]` here too.
fn redacted(text: &str) -> String {
    titi_memory::redact::redact_for_model(text).text
}

/// Branch and cleanliness, the way the `diagnose` tool reports them.
fn repo_state(root: &Path) -> String {
    let branch = match run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(out) => out.trim().to_owned(),
        Err(error) => return format!("unknown ({error})"),
    };
    match run_git(root, &["status", "--porcelain", "--", "."]) {
        Ok(changes) => {
            let changed = changes
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            let tree = if changed == 0 {
                "clean".to_owned()
            } else {
                format!("dirty ({changed} changed)")
            };
            format!("branch {branch} · {tree}")
        }
        Err(error) => format!("branch {branch} · tree unknown ({error})"),
    }
}

/// The most lines the picker above the composer may take: what is left of
/// the screen after the masthead, one line of conversation and the composer,
/// capped so a tall window never lets a picker eat the session it sits in.
fn panel_body(total: u16) -> usize {
    ((total as usize).saturating_sub(6)).clamp(PICKER_MIN_ROWS, PICKER_MAX_ROWS)
}

/// Cells a row label may use: the panel has `room` and spends three of it on
/// the cursor and the spaces around it.
fn panel_label_room(width: u16) -> usize {
    (width as usize).saturating_sub(2).max(8).saturating_sub(3)
}

/// One line above the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PanelLine {
    /// A section heading. Not selectable: it names the group below it.
    Heading(String),
    /// A selectable offer. `accent` is the green the repo gives a skill;
    /// the model picker marks roles with it too.
    Row { text: String, accent: bool },
}

/// The slice of a picker's lines that is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanelWindow {
    start: usize,
    count: usize,
    /// Lines the window hides above and below itself.
    above: usize,
    below: usize,
}

/// The picker above the composer, windowed and sized by the same code that
/// draws it, so the space the layout reserves and the lines that land in it
/// cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelView {
    /// The line above the rows, when the picker has something to say: the
    /// model picker puts the query and the counts there.
    title: Option<String>,
    lines: Vec<PanelLine>,
    /// The line the cursor is on; `None` when nothing is selectable.
    selected: Option<usize>,
    window: PanelWindow,
}

impl PanelView {
    /// Lines this takes, title and `… N more` included.
    fn height(&self) -> u16 {
        (usize::from(self.title.is_some())
            + self.window.count
            + usize::from(self.window.above > 0)
            + usize::from(self.window.below > 0)) as u16
    }
}

/// The window a panel shows: the selected line kept in view, at most `room`
/// lines, and how many lines hide on each side. A `… N more` line is paid for
/// out of the same room, and only when there is something for it to hide.
fn panel_window(len: usize, selected: usize, room: usize) -> PanelWindow {
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

/// The picker above the composer for the state on screen: the login picker,
/// the model browser, or the slash/skill list.
fn panel_view_for(chat: &Chat, total: u16, width: u16) -> Option<PanelView> {
    if chat.login_picker.is_some() {
        return Some(login_panel(chat, total));
    }
    if chat.model_picker.is_some() {
        return Some(model_panel(chat, total, width));
    }
    if picker_rows(chat).is_empty() {
        return None;
    }
    Some(command_panel(chat, total))
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

/// The picker above the composer. `view` carries its own window, so the rows
/// drawn are exactly the rows the layout made room for.
fn picker_panel(view: &PanelView, width: u16, theme: &Theme) -> Paragraph<'static> {
    let room = (width as usize).saturating_sub(2).max(8);
    let mut rows: Vec<Line<'static>> = Vec::new();
    if let Some(title) = &view.title {
        rows.push(Line::from(Span::styled(
            titi_tui::width::truncate_to_width(&format!(" {title}"), room),
            fg(theme, ThemeColor::Dim),
        )));
    }
    if view.window.above > 0 {
        rows.push(hidden_line(view.window.above, "above", room, theme));
    }
    for (at, line) in view
        .lines
        .iter()
        .enumerate()
        .skip(view.window.start)
        .take(view.window.count)
    {
        let selected = view.selected == Some(at);
        let (text, style) = match line {
            PanelLine::Heading(text) => (
                format!("  {text}"),
                fg(theme, ThemeColor::Accent).add_modifier(Modifier::BOLD),
            ),
            PanelLine::Row { text, .. } if selected => (
                format!(" ▶ {text}"),
                fg(theme, ThemeColor::CustomMessageLabel).add_modifier(Modifier::BOLD),
            ),
            PanelLine::Row { text, accent: true } => {
                (format!("   {text}"), fg(theme, ThemeColor::Success))
            }
            PanelLine::Row { text, .. } => (format!("   {text}"), fg(theme, ThemeColor::Muted)),
        };
        rows.push(Line::from(Span::styled(
            titi_tui::width::truncate_to_width(&text, room),
            style,
        )));
    }
    if view.window.below > 0 {
        rows.push(hidden_line(view.window.below, "below", room, theme));
    }
    Paragraph::new(rows).style(page(theme))
}

/// `… 12 more below`: a windowed list says how much of itself is out of
/// sight, rather than ending as if that were all of it.
fn hidden_line(count: usize, side: &str, room: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        titi_tui::width::truncate_to_width(&format!("   … {count} more {side}"), room),
        fg(theme, ThemeColor::Dim),
    ))
}

fn draw(frame: &mut ratatui::Frame<'_>, chat: &mut Chat) {
    let area = frame.area();
    // The theme is cloned out of the chat: the helpers below borrow the chat
    // mutably (the transcript clips its own scroll), and a second borrow of
    // `chat.theme` cannot live across that.
    let theme = Arc::clone(&chat.theme);
    frame.render_widget(Block::default().style(page(&theme)), area);
    if area.height < 6 || area.width < 16 {
        return;
    }
    let panel = panel_view_for(chat, area.height, area.width);
    let picker_h = panel.as_ref().map(PanelView::height).unwrap_or(0);
    let roster_h = roster_height(chat, area.height);
    let status = work_row(chat, area.width, &theme);
    let cols = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(roster_h),
        Constraint::Min(1),
        Constraint::Length(picker_h),
        // Zero while nothing is running, so an idle screen keeps every row it
        // had before this row existed.
        Constraint::Length(u16::from(status.is_some())),
        Constraint::Length(4),
    ])
    .split(area);
    frame.render_widget(masthead(chat, cols[0].width, &theme), cols[0]);
    if roster_h > 0 {
        frame.render_widget(roster(chat, &theme), cols[1]);
    }
    let (body, photos, links) = if chat.lines.is_empty() {
        (empty_state(cols[2].height, &theme), Vec::new(), Vec::new())
    } else {
        transcript(chat, cols[2].width, cols[2].height, &theme)
    };
    frame.render_widget(body, cols[2]);
    paint_photos(frame, cols[2], &photos, &theme);
    paint_links(frame, cols[2], &links);
    if let Some(view) = &panel {
        frame.render_widget(picker_panel(view, cols[3].width, &theme), cols[3]);
    }
    if let Some(status) = status {
        frame.render_widget(status, cols[4]);
    }
    frame.render_widget(composer(chat, cols[5].width, &theme), cols[5]);
}

/// Rows the roster panel takes: one per peer plus its heading, capped so a
/// crowded hub cannot squeeze the conversation off the screen.
fn roster_height(chat: &Chat, total: u16) -> u16 {
    if !chat.hub_open {
        return 0;
    }
    let rows = chat.hub.peers().len().max(1) + 1;
    let cap = (total / 3).max(2);
    (rows as u16).min(cap)
}

/// Who is on the hub right now, this session marked as itself.
fn roster(chat: &Chat, theme: &Theme) -> Paragraph<'static> {
    let mine = chat.hub.agent_id().unwrap_or_default().to_owned();
    let mut rows = vec![Line::from(Span::styled(
        format!(" hub · {} peer(s)", chat.hub.peers().len()),
        fg(theme, ThemeColor::Accent).add_modifier(Modifier::BOLD),
    ))];
    if chat.hub.peers().is_empty() {
        rows.push(Line::from(Span::styled(
            "  nobody here · /join connects",
            fg(theme, ThemeColor::Dim),
        )));
    }
    for peer in chat.hub.peers() {
        let (mark, color) = if peer == &mine {
            ("you", ThemeColor::CustomMessageLabel)
        } else {
            ("·", ThemeColor::Muted)
        };
        rows.push(Line::from(vec![
            Span::styled(format!("  {mark} "), fg(theme, color)),
            Span::styled(peer.clone(), fg(theme, ThemeColor::Text)),
        ]));
    }
    Paragraph::new(rows).style(page(theme))
}

// Every colour on this screen is a token of the active theme, one per role the
// palette here used to hold:
//
//   accent             the brand, the assistant, a running turn, the caret,
//                      picker headings
//   customMessageLabel the user's own name, the thinking phase, a pending
//                      tool, the highlighted picker row — the theme's colour
//                      for a message that is not the assistant's
//   warning            needs you: an approval, a pause, a sign-in, and a
//                      pending tool's mark
//   success            a tool that finished
//   error              a failed tool or a failed command
//   text               the body of a message and of the composer
//   muted              the right half of the masthead, a finished tool's
//                      detail, an unselected picker row
//   dim                the ready state, a composer caption, a note, the empty
//                      state's hint
//   border             the composer's frame while idle
//   statusLineBg       the screen behind everything
//   customMessageBg    the composer's own surface, the one raised surface the
//                      theme has to spare; `userMessageBg` belongs to the
//                      user's block

/// One theme colour token as a ratatui style. The theme resolves a token to
/// CSS hex, so this is the only place a token becomes a terminal colour.
fn fg(theme: &Theme, token: ThemeColor) -> Style {
    Style::default().fg(rgb(&theme.get_color_hex(token)))
}

/// One theme background token as a ratatui colour.
fn bg(theme: &Theme, token: ThemeBg) -> Color {
    rgb(&theme.get_bg_hex(token))
}

/// The screen behind everything: the theme's chrome surface with its body text
/// on it. The status-line background is the one surface token that stands for
/// the whole screen, and it is darker than every other one in the presets.
fn page(theme: &Theme) -> Style {
    Style::default()
        .bg(bg(theme, ThemeBg::StatusLineBg))
        .fg(rgb(&theme.get_color_hex(ThemeColor::Text)))
}

/// The composer's own surface: the theme's raised panel colour with body text
/// on it. `userMessageBg` belongs to the user's own block and is not spent
/// here.
fn surface(theme: &Theme) -> Style {
    Style::default()
        .bg(bg(theme, ThemeBg::CustomMessageBg))
        .fg(rgb(&theme.get_color_hex(ThemeColor::Text)))
}

/// A resolved token hex as a ratatui colour. A token the theme resolves to the
/// terminal default still answers with a hex (`get_color_hex`), so the fallback
/// only covers a hex a theme cannot resolve at all.
fn rgb(hex: &str) -> Color {
    match titi_tui::theme::color::hex_to_rgb(hex) {
        Some(rgb) => (rgb.r, rgb.g, rgb.b).into(),
        None => Color::Reset,
    }
}

/// The spinner frame for an elapsed time: ten glyphs 80ms apart, wrapping.
fn spinner_frame(elapsed: Duration) -> &'static str {
    SPINNER[(elapsed.as_millis() / SPINNER_PERIOD.as_millis()) as usize % SPINNER.len()]
}

/// The elapsed seconds of a running turn, one decimal deep: coarse enough to
/// stay readable, fine enough to move every frame.
fn elapsed_label(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

fn masthead(chat: &Chat, width: u16, theme: &Theme) -> Paragraph<'static> {
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
        ThemeColor::Warning
    } else if chat.turn_active {
        ThemeColor::Accent
    } else {
        ThemeColor::Dim
    };
    let ctx = match chat.context_percent {
        Some(percent) => format!("  {percent}%"),
        None => String::new(),
    };
    let left = " titi";
    // Beside the state, not on the right: the right half is what gets
    // truncated first, and a loop running unseen is the whole problem.
    let loops = if chat.jobs.is_empty() {
        String::new()
    } else {
        format!("  {} loop(s)", chat.jobs.len())
    };
    // The badge is the engine's mode, not a local toggle: it says what the
    // next turn may actually do.
    let mode = match chat.mode {
        SessionMode::Agent => String::new(),
        other => format!("  {}", other.label()),
    };
    // A turn in flight is not described here beyond the word `working`:
    // the moving glyph, the elapsed seconds and every changing fact live in
    // the status row directly above the composer (`work_row`), which is
    // where the person is looking and where the prompt they typed came
    // from. Two lines animating the same clock would make a stall harder to
    // spot, not easier.
    let mid = format!("  {state}{mode}{loops}");
    let mut right = format!("{}{ctx}  {} ", chat.model, chat.session_label);
    let fixed = titi_tui::width::visible_width(left) + titi_tui::width::visible_width(&mid) + 2;
    // One cell is kept as the gutter before the right half: without it a
    // full line runs the model into the state beside it (`planopenai-codex/…`).
    let room = (width as usize).saturating_sub(fixed + 1);
    if titi_tui::width::visible_width(&right) > room {
        // The session label is the least informative thing on this side, so
        // it is what goes first: a truncated session id still names a file,
        // while `open -codex/gpt-5.5` names nothing at all.
        right = format!("{}{ctx} ", chat.model);
    }
    if titi_tui::width::visible_width(&right) > room {
        // Even the model does not fit: cut it where it stands, with an
        // ellipsis, so a short id never reads as a whole one.
        let head = titi_tui::width::truncate_to_width(&chat.model, room.saturating_sub(1));
        right = format!("{head}…");
    }
    let used = fixed + titi_tui::width::visible_width(&right);
    let gap = (width as usize).saturating_sub(used).max(1);
    let line = Line::from(vec![
        Span::styled(
            left,
            fg(theme, ThemeColor::Accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(mid, fg(theme, state_color)),
        Span::styled(" ".repeat(gap), page(theme)),
        Span::styled(right, fg(theme, ThemeColor::Muted)),
    ]);
    Paragraph::new(line).style(page(theme))
}

/// The live status row, drawn on the single line between the conversation and
/// the composer box, or `None` when there is nothing to report.
///
/// `None` is what makes an idle screen identical to a screen from before this
/// row existed: the caller gives it no height, so it cannot even leave a blank
/// line behind.
///
/// Honest limit for the wait: the Responses/Codex decoder does emit
/// `ThinkingDelta` for reasoning deltas (crates/titi-providers/src/openai.rs:286),
/// but a Codex request does not ask for a reasoning summary
/// (crates/titi-providers/src/wire.rs:459), so on that provider the first
/// seconds of a turn are spent in `Waiting` with nothing to show for them.
/// The glyph and the seconds are the whole signal there, and they are the
/// reason this row exists.
fn work_row(chat: &Chat, width: u16, theme: &Theme) -> Option<Paragraph<'static>> {
    if !chat.turn_active && chat.approval.is_none() {
        return None;
    }
    // An approval outranks the phase it interrupted: the engine is stopped
    // on a person, and which tool it is stopped on is the fact that matters.
    // The masthead's `needs you` is the session state; this row says what the
    // person is being asked about, and the composer below says what to press.
    if let Some(pending) = &chat.approval {
        return Some(work_line(
            "⚠",
            &format!("needs you · {}", pending.name),
            ThemeColor::Warning,
            width,
            theme,
        ));
    }
    let elapsed = chat.turn_elapsed().unwrap_or_default();
    let (glyph, fact, color) = match &chat.phase {
        WorkPhase::Waiting => (
            spinner_frame(elapsed),
            format!("waiting for the first token · {}", elapsed_label(elapsed)),
            ThemeColor::Accent,
        ),
        WorkPhase::Streaming => (
            spinner_frame(elapsed),
            format!(
                "streaming · {} · {} chars",
                elapsed_label(elapsed),
                chat.reply.chars().count()
            ),
            ThemeColor::Accent,
        ),
        WorkPhase::Thinking => (
            spinner_frame(elapsed),
            format!(
                "thinking · {} · {} chars",
                elapsed_label(elapsed),
                chat.thinking.chars().count()
            ),
            ThemeColor::CustomMessageLabel,
        ),
        WorkPhase::Tool { name, since, .. } => (
            "⚙",
            format!("{name} · {}", elapsed_label(since.elapsed())),
            ThemeColor::CustomMessageLabel,
        ),
    };
    Some(work_line(glyph, &fact, color, width, theme))
}

/// One status row, cut to the screen with an ellipsis. A row that does not fit
/// is truncated here rather than wrapped: a wrapped line would push the
/// composer down and read as part of the conversation.
fn work_line(
    glyph: &str,
    fact: &str,
    color: ThemeColor,
    width: u16,
    theme: &Theme,
) -> Paragraph<'static> {
    let row = format!(" {glyph} {fact}");
    let room = (width as usize).saturating_sub(1);
    let mut text = titi_tui::width::truncate_to_width(&row, room.max(1));
    if titi_tui::width::visible_width(&row) > room.max(1) {
        text.push('…');
    }
    Paragraph::new(Line::from(Span::styled(text, fg(theme, color)))).style(page(theme))
}

fn empty_state(height: u16, theme: &Theme) -> Paragraph<'static> {
    let block = 5usize;
    let pad = (height as usize).saturating_sub(block) / 2;
    let mut rows = vec![Line::from(""); pad];
    rows.push(Line::from(Span::styled(
        "titi",
        fg(theme, ThemeColor::Accent).add_modifier(Modifier::BOLD),
    )));
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "say what you want done",
        fg(theme, ThemeColor::Muted),
    )));
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "enter  send      /model  switch      ctrl-c  quit",
        fg(theme, ThemeColor::Dim),
    )));
    Paragraph::new(rows)
        .alignment(Alignment::Center)
        .style(page(theme))
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

/// A rendered row that is part of a URL: the visible text and the URL it
/// stands for. `row` is the index of that row inside its line's rows.
#[derive(Debug)]
struct LinkRow {
    row: usize,
    url: String,
    text: String,
}

/// A link row where the last frame put it, ready for [`paint_links`]. `row`
/// is relative to the transcript pane.
struct LinkPaint {
    row: u16,
    url: String,
    text: String,
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
    theme: &Theme,
) -> (Paragraph<'static>, Vec<PhotoPaint>, Vec<LinkPaint>) {
    let inner = (width as usize).saturating_sub(2).max(8);
    let max_cols = width.saturating_sub(8).max(8);
    let owned = chat.lines.clone();
    let mut rows: Vec<TranscriptRow> = Vec::new();
    let mut links: Vec<(usize, LinkRow)> = Vec::new();
    for (index, line) in owned.iter().enumerate() {
        let gap = matches!(line.kind, LineKind::User | LineKind::Assistant) && index > 0;
        if gap && !rows.is_empty() {
            rows.push(TranscriptRow::Text(Line::from("")));
        }
        let base = rows.len();
        let (texts, line_links) = message_rows(line, inner, theme);
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
    (Paragraph::new(lines).style(page(theme)), photos, paints)
}

fn paint_photos(
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
fn paint_links(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, links: &[LinkPaint]) {
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

/// The rows of one transcript line, plus every row that carries a URL.
fn message_rows(
    line: &TranscriptLine,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<LinkRow>) {
    if line.kind == LineKind::Note
        && let Some((head, url, instructions)) = login_link(&line.text)
    {
        return link_note(head, url, instructions, theme, width);
    }
    let rows = match line.kind {
        LineKind::User => speech(
            "you",
            ThemeColor::CustomMessageLabel,
            ThemeColor::Text,
            &line.text,
            width,
            theme,
        ),
        LineKind::Assistant => speech(
            "titi",
            ThemeColor::Accent,
            ThemeColor::Text,
            &line.text,
            width,
            theme,
        ),
        LineKind::Tool => chip(tool_chip(&line.text), theme, width),
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
    };
    (rows, Vec::new())
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
        let room = width.saturating_sub(6).max(4);
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
fn wrap_url(url: &str, width: usize) -> Vec<String> {
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

fn speech(
    name: &str,
    label: ThemeColor,
    body: ThemeColor,
    text: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // "you" and "titi" share a column so a short message stays one row.
    let tag = format!("{name:<4}");
    let wrap_at = width.saturating_sub(10).max(8);
    let pieces = wrap_plain(text, wrap_at);
    let mut rows = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.into_iter().enumerate() {
        let row = if index == 0 {
            vec![
                Span::styled("  ", page(theme)),
                Span::styled(tag.clone(), fg(theme, label).add_modifier(Modifier::BOLD)),
                Span::styled(" │ ", fg(theme, label)),
                Span::styled(piece, fg(theme, body)),
            ]
        } else {
            vec![
                Span::styled("       │ ", fg(theme, label)),
                Span::styled(piece, fg(theme, body)),
            ]
        };
        rows.push(Line::from(row));
    }
    rows
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
    let room = width.saturating_sub(6).max(4);
    let pieces = wrap_plain(&text, room);
    let mut rows = Vec::with_capacity(pieces.len());
    for (index, piece) in pieces.into_iter().enumerate() {
        let row = if index == 0 {
            vec![
                Span::styled("   ", page(theme)),
                Span::styled(mark.to_owned(), fg(theme, mark_color)),
                Span::styled(" ", page(theme)),
                Span::styled(piece, fg(theme, text_color)),
            ]
        } else {
            vec![
                Span::styled("     ", page(theme)),
                Span::styled(piece, fg(theme, text_color)),
            ]
        };
        rows.push(Line::from(row));
    }
    rows
}

fn composer(chat: &Chat, width: u16, theme: &Theme) -> Paragraph<'static> {
    let (border, caption_color) = if chat.approval.is_some() || chat.login_for.is_some() {
        (ThemeColor::Warning, ThemeColor::Warning)
    } else if chat.turn_active {
        (ThemeColor::Accent, ThemeColor::Accent)
    } else {
        (ThemeColor::Border, ThemeColor::Dim)
    };
    let caption = titi_tui::width::truncate_to_width(
        &composer_caption(chat),
        (width as usize).saturating_sub(4),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(theme, border))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {caption} "),
                fg(theme, caption_color),
            ))
            .centered(),
        )
        .padding(Padding::horizontal(1))
        .style(surface(theme));
    let inner = (width as usize).saturating_sub(6).max(4);
    let line = if let Some(pending) = &chat.approval {
        Line::from(Span::styled(
            titi_tui::width::truncate_to_width(
                &format!("{}   y allow    n refuse", pending.name),
                inner,
            ),
            fg(theme, ThemeColor::Warning).add_modifier(Modifier::BOLD),
        ))
    } else if let Some(provider) = &chat.login_for {
        let device = chat
            .oauth
            .as_ref()
            .is_some_and(|login| login.method == LoginMethod::Device);
        let shown = if !chat.input.is_empty() {
            "•".repeat(chat.input.chars().count().min(32))
        } else if device {
            "waiting for the device code".to_owned()
        } else if chat.oauth.is_some() {
            "paste the code or the redirect URL".to_owned()
        } else {
            format!("paste the {provider} key")
        };
        let color = if chat.input.is_empty() {
            ThemeColor::Dim
        } else {
            ThemeColor::Text
        };
        Line::from(vec![
            Span::styled("› ", fg(theme, ThemeColor::Accent)),
            Span::styled(shown, fg(theme, color)),
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
            Span::styled("› ", fg(theme, ThemeColor::Accent)),
            Span::styled(placeholder, fg(theme, ThemeColor::Dim)),
        ])
    } else {
        let room = inner.saturating_sub(4).max(1);
        Line::from(vec![
            Span::styled("› ", fg(theme, ThemeColor::Accent)),
            Span::styled(fit_tail(&chat.input, room), fg(theme, ThemeColor::Text)),
            Span::styled("▍", fg(theme, ThemeColor::Accent)),
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
    } else if chat.model_picker.is_some() {
        "↑↓ move  ·  enter switches  ·  esc clears or closes"
    } else if chat
        .oauth
        .as_ref()
        .is_some_and(|login| login.method == LoginMethod::Device)
    {
        // The device grant finishes in the browser: there is nothing to
        // submit here, only the way out.
        "esc cancels"
    } else if chat.login_for.is_some() && chat.oauth.is_some() {
        "enter submits  ·  esc cancels"
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
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        KeyCode::Char('u') if control => Some(Key::PageUpHalf),
        KeyCode::Char('d') if control => Some(Key::PageDownHalf),
        _ => None,
    }
}

/// Polls the keyboard and the engine once. `Ok(true)` means the user quit.
fn pump(
    engine: &mut Engine,
    chat: &mut Chat,
    session_log: &Option<SessionLog>,
    cast: &mut Option<crate::ompcast::CastWriter>,
) -> io::Result<bool> {
    if event::poll(Duration::from_millis(50))? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(mapped) = map_key(key.code, key.modifiers) {
                    let applied = chat.on_key(mapped, Instant::now());
                    if dispatch(engine, chat, session_log, cast, applied) {
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
                // Recorded before the screen folds it into lines: the cast is
                // the stream the surface received, and it is already masked.
                cast_write(chat, cast, |writer| writer.event(&event));
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
    // Same tick as the engine, and just as non-blocking: an unhosted hub
    // costs one `try_recv` that returns nothing.
    chat.poll_hub();
    chat.poll_login();
    Ok(false)
}

/// Writes one cast record, and stops recording if the file has gone bad.
///
/// A recording is a convenience; a session that dies because a disk filled
/// up is not. The failure is said once, in the transcript, and the writer is
/// dropped so the next event costs nothing.
fn cast_write(
    chat: &mut Chat,
    cast: &mut Option<crate::ompcast::CastWriter>,
    write: impl FnOnce(&mut crate::ompcast::CastWriter) -> Result<(), crate::ompcast::CastError>,
) {
    let Some(writer) = cast.as_mut() else {
        return;
    };
    if let Err(error) = write(writer) {
        chat.push(
            LineKind::Error,
            format!("record: {error} · recording stopped"),
        );
        *cast = None;
    }
}

fn dispatch(
    engine: &mut Engine,
    chat: &mut Chat,
    session_log: &Option<SessionLog>,
    cast: &mut Option<crate::ompcast::CastWriter>,
    applied: Applied,
) -> bool {
    if let Some(write) = &applied.log
        && write.role == Role::User
    {
        let text = write.text.clone();
        cast_write(chat, cast, |writer| writer.input(&text));
    }
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
        Chat::new("openai/gpt-4.1", "session-123", test_theme())
    }

    /// A built-in theme, with the colour depth pinned so an assertion is about
    /// the theme's tokens and not about this machine's `TERM`. Built-in names
    /// win over `{agent_dir}/themes` (`theme::loader::load_theme_json_in`), so
    /// a custom theme on the machine that runs the tests cannot change them.
    fn test_theme_named(name: &str) -> Arc<Theme> {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        };
        let theme = titi_tui::theme::loader::load_theme(name, &options);
        match theme {
            Ok(theme) => Arc::new(theme),
            Err(reason) => panic!("built-in theme {name}: {reason}"),
        }
    }

    /// The dark slot the live screen lands on by default (`AUTO_DARK_THEME`).
    fn test_theme() -> Arc<Theme> {
        test_theme_named("titanium")
    }

    fn frame_text(chat: &mut Chat) -> String {
        frame_rows(chat, 80, 24).join("")
    }

    /// A chat whose catalog and settings are the test's own: its agent
    /// directory is a fresh temp dir, so no machine's `config.yml` decides
    /// what a picker row says. The dir is returned with the chat because
    /// dropping it would take the agent directory away mid-test.
    fn picker_chat(model: &str, session: &str) -> (tempfile::TempDir, Chat) {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = Chat::new(model, session, test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        (dir, chat)
    }

    /// The transcript's model confirmations, in the order they landed.
    fn confirmations(chat: &Chat) -> Vec<String> {
        chat.lines
            .iter()
            .filter(|line| line.text.starts_with("model "))
            .map(|line| line.text.clone())
            .collect()
    }

    /// Plays the `ModelSwitched` the engine answers a switch with, and
    /// returns the confirmations *this* switch added: a switch that adds two
    /// lines is a switch that was narrated twice.
    fn confirmations_after_switch(chat: &mut Chat, to: &str) -> Vec<String> {
        let before = confirmations(chat).len();
        chat.on_event(EngineEvent::ModelSwitched {
            turn_id: None,
            from: chat.model.clone().into(),
            to: to.into(),
        });
        let mut after = confirmations(chat);
        after.split_off(before)
    }

    /// The elapsed-seconds token the status row is showing, if it shows one.
    /// The row is the only place a live elapsed time is rendered.
    fn shown_seconds(frame: &str) -> Option<f64> {
        frame
            .split([' ', '·'])
            .filter_map(|token| token.strip_suffix('s'))
            .find_map(|token| token.parse().ok())
    }

    /// The row directly above the composer box: the one line the status row
    /// is drawn on. Read from the rendered frame, so a test sees what a user
    /// sees and not what a helper promised.
    fn above_composer(chat: &mut Chat, width: u16, height: u16) -> String {
        let rows = frame_rows(chat, width, height);
        rows[height as usize - 5].clone()
    }

    fn type_text(chat: &mut Chat, text: &str) {
        let now = Instant::now();
        for ch in text.chars() {
            chat.on_key(Key::Char(ch), now);
        }
    }

    /// The picker, the masthead and the composer hold at 60, 80 and 120
    /// columns — every row exactly as wide as the screen, the composer's
    /// border intact — and a screen too small to lay out still draws instead
    /// of panicking.
    #[test]
    fn the_screen_holds_at_60_80_and_120_columns() {
        let (dir, mut chat) =
            picker_chat("openai-codex/gpt-daybreak-blue-latest-wm", "session-1234");
        crate::secrets::store_key(dir.path(), "openai-codex", "sk-test").expect("store");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai-codex/gpt-daybreak-blue-latest-wm".to_owned(),
            "openai-codex/gpt-5.5".to_owned(),
            "anthropic/claude-sonnet-4-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        type_text(&mut chat, "codex");

        for (width, height) in [(60u16, 20u16), (80, 20), (120, 30)] {
            let rows = frame_rows(&mut chat, width, height);
            assert_eq!(rows.len(), height as usize);
            for row in &rows {
                assert_eq!(
                    titi_tui::width::visible_width(row),
                    width as usize,
                    "{width}x{height}: {row:?}"
                );
            }
            let top = &rows[height as usize - 4];
            let bottom = &rows[height as usize - 1];
            assert!(top.starts_with('╭') && top.ends_with('╮'), "{top:?}");
            assert!(
                bottom.starts_with('╰') && bottom.ends_with('╯'),
                "{bottom:?}"
            );
        }

        // Smaller than the picker, the composer and the masthead can share.
        for (width, height) in [(20u16, 6u16), (16, 6), (60, 7)] {
            let rows = frame_rows(&mut chat, width, height);
            assert_eq!(rows.len(), height as usize);
            for row in &rows {
                assert_eq!(titi_tui::width::visible_width(row), width as usize);
            }
        }
    }

    /// The rendered text of each row, the way the transcript stacks them.
    fn row_texts(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
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
    fn failed_event_without_turn_id_does_not_finish_turn() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.approval = Some(PendingApproval {
            call_id: "call-1".into(),
            name: "bash".into(),
        });

        chat.on_event(EngineEvent::Failed {
            turn_id: None,
            reason: titi_providers::ErrorReason::Rejected,
            message: "nope".into(),
        });

        assert!(chat.turn_active);
        assert!(chat.approval.is_some());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text == "nope")
        );
    }

    #[test]
    fn failed_event_with_matching_turn_id_finishes_turn() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.approval = Some(PendingApproval {
            call_id: "call-1".into(),
            name: "bash".into(),
        });

        chat.on_event(EngineEvent::Failed {
            turn_id: Some(TurnId(1)),
            reason: titi_providers::ErrorReason::Rejected,
            message: "nope".into(),
        });

        assert!(!chat.turn_active);
        assert!(chat.approval.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text == "nope")
        );
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

    /// Bare `/model` is the browser, not a cycle: Enter takes the row the
    /// cursor is on, which starts as the model in use.
    #[test]
    fn bare_model_opens_the_browser_on_the_model_in_use() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "opencode-go/glm-5.3-flash".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        let opened = chat.on_key(Key::Enter, Instant::now());
        assert!(opened.effect.is_none(), "opening sends nothing: {opened:?}");
        assert!(chat.model_picker.is_some());
        let frame = frame_text(&mut chat);
        assert!(frame.contains("models · 2"), "{frame}");
        assert!(frame.contains("✓ current"), "{frame}");

        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::SwitchModel { model })) => {
                assert_eq!(model.as_str(), "openai/gpt-4.1");
            }
            other => panic!("expected a model switch, got {other:?}"),
        }
        assert!(applied.log.is_none());
        assert!(!chat.turn_active);
    }

    /// Rows are grouped, and each row states what titi knows: the id, the
    /// declared window when there is one, and the credential behind the
    /// provider.
    #[test]
    fn the_browser_groups_by_provider_and_states_the_known_facts() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "opencode-go/glm-5.3-flash".to_owned(),
            "anthropic/claude-sonnet-4-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());

        let frame = frame_text(&mut chat);
        for expected in [
            "▾ openai",
            "▾ opencode-go",
            "▾ anthropic",
            "openai/gpt-4.1",
            "·openai",
            "1M",
            "·anthropic",
            "200k",
            // A model that declares no window gets no chip; the row still
            // names its provider.
            "opencode-go/glm-5.3-flash  ·opencode-go",
        ] {
            assert!(frame.contains(expected), "{expected} is missing: {frame}");
        }
    }

    /// A stored credential is named on its provider's rows, so a
    /// subscription-backed model is recognizable without `/keys`.
    #[test]
    fn a_row_names_the_credential_the_provider_holds() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        crate::secrets::store_key(dir.path(), "openai", "sk-test").expect("store");
        crate::secrets::store_oauth(
            dir.path(),
            "openai-codex",
            &titi_providers::oauth::OAuthTokens {
                access: "sk-test".to_owned(),
                refresh: Some("sk-test-refresh".to_owned()),
                expires_at: Some(crate::secrets::now_secs() + 3_600),
                account_id: None,
                email: None,
                org_id: None,
                org_name: None,
            },
        )
        .expect("store oauth");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "openai-codex/gpt-5.5".to_owned(),
        ]);

        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("openai/gpt-4.1  ·openai  1M  key"),
            "an api key reads as a key: {frame}"
        );
        assert!(
            frame.contains("openai-codex/gpt-5.5  ·openai-codex  oauth"),
            "a subscription reads as oauth: {frame}"
        );
    }

    /// Typing narrows the list, and the title says what is left and what was
    /// typed. Matching is a subsequence, the repo's habit for `/switch`.
    #[test]
    fn typing_narrows_the_browser_and_the_title_shows_the_query() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-sonnet-4-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        type_text(&mut chat, "snnt");

        let frame = frame_text(&mut chat);
        assert!(frame.contains("anthropic/claude-sonnet-4-5"), "{frame}");
        assert!(
            !frame.contains("·openai  1M"),
            "the filtered row is gone, the masthead keeps naming the model: {frame}"
        );
        assert!(frame.contains("1 of 2"), "{frame}");
        assert!(frame.contains("\"snnt\""), "{frame}");
    }

    /// A provider name spelled out ranks that provider's rows above a model
    /// whose letters only happen to sit in the same order.
    #[test]
    fn a_provider_prefix_outranks_a_scattered_match() {
        let (_dir, mut chat) = picker_chat("openai-codex/gpt-5.5", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "cerebras/qwen3-coder-x".to_owned(),
            "openai-codex/gpt-5.5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        type_text(&mut chat, "codex");

        let picker = chat.model_picker.as_ref().expect("open");
        let matched = picker.matched();
        assert_eq!(matched.len(), 2, "both match, one ranks higher");
        assert_eq!(picker.offers[matched[0]].target(), "openai-codex/gpt-5.5");
        // The cursor starts on the best match, so Enter takes it.
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "openai-codex/gpt-5.5".into()
            }))
        );
    }

    /// Esc takes the query back first and closes on the second press: a
    /// filter is cheap to undo, and closing on one key would make a narrow
    /// search cost a reopen.
    #[test]
    fn esc_clears_the_query_then_closes_without_switching() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-sonnet-4-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        type_text(&mut chat, "sonnet");
        assert!(frame_text(&mut chat).contains("1 of 2"));

        let first = chat.on_key(Key::Esc, Instant::now());
        assert!(first.effect.is_none());
        assert!(chat.model_picker.is_some(), "the picker is still open");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("models · 2"), "the query is gone: {frame}");
        assert!(frame.contains("openai/gpt-4.1"), "{frame}");

        let second = chat.on_key(Key::Esc, Instant::now());
        assert!(second.effect.is_none());
        assert!(chat.model_picker.is_none(), "the second esc closes it");
        assert_eq!(chat.model, "openai/gpt-4.1", "nothing switched");
        assert!(
            !chat
                .lines
                .iter()
                .any(|line| line.text.starts_with("model ")),
            "closing says nothing"
        );
    }

    /// Enter switches to the highlighted row and confirms in the words
    /// `/model <id>` uses — the argument form keeps working unchanged.
    #[test]
    fn enter_switches_and_the_engine_confirms_once() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-sonnet-4-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        type_text(&mut chat, "sonnet");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-sonnet-4-5".into()
            }))
        );
        assert_eq!(chat.model, "anthropic/claude-sonnet-4-5");
        assert!(chat.model_picker.is_none());
        assert!(
            confirmations(&chat).is_empty(),
            "the engine owns the confirmation: {:?}",
            chat.lines
        );
        assert_eq!(
            confirmations_after_switch(&mut chat, "anthropic/claude-sonnet-4-5"),
            ["model anthropic/claude-sonnet-4-5"]
        );
    }

    /// Bare `/switch` is the same browser, and one switch reads as one line —
    /// the engine's, whether the model was picked or named.
    #[test]
    fn bare_switch_opens_the_browser_and_confirms_once() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "/switch");
        let opened = chat.on_key(Key::Enter, Instant::now());
        assert!(opened.effect.is_none());
        assert!(chat.model_picker.is_some());
        type_text(&mut chat, "opus");

        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
        assert_eq!(
            confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
            ["model anthropic/claude-opus-5"]
        );
    }

    /// `/model <sel>` resolves the way it always did — a whole id or the last
    /// segment — and an id it cannot place is the refusal it always was.
    #[test]
    fn a_model_argument_resolves_by_id_and_by_last_segment() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-sonnet-4-5".to_owned(),
        ]);

        type_text(&mut chat, "/model anthropic/claude-sonnet-4-5");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-sonnet-4-5".into()
            }))
        );
        assert_eq!(chat.model, "anthropic/claude-sonnet-4-5");

        type_text(&mut chat, "/model gpt-4.1");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "openai/gpt-4.1".into()
            }))
        );

        type_text(&mut chat, "/model nope");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.model_picker.is_none(), "an argument is not a picker");
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "unknown model nope"),
            "{:?}",
            chat.lines
        );
    }

    /// The engine's `ModelSwitched` is the only confirmation, on every path:
    /// a command that also narrated its own switch printed the line twice.
    #[test]
    fn a_switch_confirms_once_from_the_command_path_too() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "/model anthropic/claude-opus-5");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
        assert!(confirmations(&chat).is_empty(), "{:?}", chat.lines);
        assert_eq!(
            confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
            ["model anthropic/claude-opus-5"]
        );

        type_text(&mut chat, "/switch opus");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
        assert_eq!(
            confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
            ["model anthropic/claude-opus-5"]
        );
    }

    /// An argument is not a picker: the resolution tests above must not have
    /// come to depend on a browser being open.
    #[test]
    fn a_command_with_an_argument_does_not_open_the_picker() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "/model anthropic/claude-opus-5");
        chat.on_key(Key::Enter, Instant::now());
        assert!(chat.model_picker.is_none());

        type_text(&mut chat, "/switch opus");
        chat.on_key(Key::Enter, Instant::now());
        assert!(chat.model_picker.is_none());
    }

    /// A configured role is a row of its own, first in the list, and it
    /// switches to the model it resolves to — `/switch @role`, spelled out.
    #[test]
    fn configured_roles_lead_the_list_and_switch_to_their_model() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        std::fs::write(
            dir.path().join("config.yml"),
            "modelRoles:\n  review: anthropic/claude-opus-5\n",
        )
        .expect("config");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);

        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(frame.contains("▾ roles  1"), "{frame}");
        assert!(
            frame.contains("@review  →  anthropic/claude-opus-5"),
            "{frame}"
        );

        // `@` is the roles and nothing else.
        type_text(&mut chat, "@");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("@review"), "{frame}");
        assert!(
            !frame.contains("·openai  1M"),
            "no model row survives an `@` query: {frame}"
        );

        // The cursor moves to the only match as the query narrows, so Enter
        // takes the role's model.
        type_text(&mut chat, "rev");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
        assert_eq!(chat.model, "anthropic/claude-opus-5");
    }

    /// A long list is windowed around the cursor and says how much of itself
    /// is out of sight, rather than ending as if that were all of it.
    #[test]
    fn a_long_list_is_windowed_and_says_what_it_hides() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(
            (0..30)
                .map(|at| format!("openai/gpt-4.{at}"))
                .collect::<Vec<_>>(),
        );
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(frame.contains("more below"), "something is hidden: {frame}");
        assert!(!frame.contains("more above"), "nothing above yet: {frame}");
        assert!(
            frame.contains("✓ current"),
            "the cursor row is shown: {frame}"
        );

        for _ in 0..15 {
            chat.on_key(Key::Down, Instant::now());
        }
        let frame = frame_text(&mut chat);
        assert!(frame.contains("more above"), "the window scrolled: {frame}");
        assert!(
            frame.contains("more below"),
            "and it still hides the rest: {frame}"
        );
    }

    /// The window keeps the cursor in view and never exceeds the lines it was
    /// given, whatever the selection and the list length.
    #[test]
    fn the_window_always_contains_the_selection() {
        for len in 1..40usize {
            for selected in 0..len {
                for room in 1..=PICKER_MAX_ROWS {
                    let window = panel_window(len, selected, room);
                    let lines = window.count
                        + usize::from(window.above > 0)
                        + usize::from(window.below > 0);
                    assert!(
                        window.count > 0,
                        "len {len} selected {selected} room {room}"
                    );
                    assert!(
                        selected >= window.start && selected < window.start + window.count,
                        "len {len} selected {selected} room {room}: {window:?}"
                    );
                    assert!(window.start + window.count <= len, "{window:?}");
                    assert!(lines <= room.max(PICKER_MIN_ROWS), "{window:?}");
                }
            }
        }
    }

    /// The rows of a narrowing screen give up the least useful part first,
    /// and a cut id says it was cut.
    #[test]
    fn a_narrow_row_drops_the_provider_before_it_cuts_the_model() {
        let row = ModelRow {
            id: "openai-codex/gpt-daybreak-blue-latest-wm".to_owned(),
            provider: "openai-codex".to_owned(),
            context_window: Some(272_000),
            credential: Some("oauth".to_owned()),
        };
        assert_eq!(
            model_row_label(&row, false, 78),
            "openai-codex/gpt-daybreak-blue-latest-wm  ·openai-codex  272k  oauth"
        );
        // The provider is the id's own prefix and the heading above it, so it
        // goes first; then the window.
        assert_eq!(
            model_row_label(&row, false, 60),
            "openai-codex/gpt-daybreak-blue-latest-wm  272k  oauth"
        );
        assert_eq!(
            model_row_label(&row, false, 50),
            "openai-codex/gpt-daybreak-blue-latest-wm  oauth"
        );
        // The credential and the mark are what a narrow row must keep.
        let cut = model_row_label(&row, true, 40);
        assert!(titi_tui::width::visible_width(&cut) <= 40, "{cut}");
        assert!(cut.contains('…'), "{cut}");
        assert!(cut.ends_with("oauth  ✓ current"), "{cut}");
    }

    /// A turn in flight shows a moving glyph and the seconds it has been
    /// running: `working` alone cannot tell a live turn from a stalled one.
    #[test]
    fn a_running_turn_shows_a_spinner_and_its_elapsed_seconds() {
        let mut chat = chat();
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now() - Duration::from_secs(3));
        let frame = frame_text(&mut chat);
        assert!(frame.contains("working"), "{frame}");
        assert!(
            SPINNER.iter().any(|glyph| frame.contains(glyph)),
            "no spinner frame: {frame}"
        );
        let shown = shown_seconds(&frame).unwrap_or_else(|| panic!("no seconds: {frame}"));
        assert!((3.0..4.0).contains(&shown), "{shown} in {frame}");
    }

    /// Nothing about a turn is claimed when none is running.
    #[test]
    fn an_idle_frame_has_no_spinner_and_no_seconds() {
        let mut chat = chat();
        let frame = frame_text(&mut chat);
        assert!(!frame.contains("working"), "{frame}");
        assert!(
            !SPINNER.iter().any(|glyph| frame.contains(glyph)),
            "{frame}"
        );
        assert!(shown_seconds(&frame).is_none(), "{frame}");
    }

    /// The spinner steps on every loop tick, so consecutive frames differ.
    #[test]
    fn the_spinner_moves_frame_by_frame() {
        assert_eq!(spinner_frame(Duration::ZERO), SPINNER[0]);
        assert_eq!(spinner_frame(SPINNER_PERIOD), SPINNER[1]);
        assert_ne!(
            spinner_frame(SPINNER_PERIOD * 3),
            spinner_frame(SPINNER_PERIOD * 4)
        );
        assert_eq!(spinner_frame(SPINNER_PERIOD * 10), SPINNER[0], "it wraps");
        assert_eq!(elapsed_label(Duration::from_millis(3_200)), "3.2s");
        assert_eq!(elapsed_label(Duration::from_secs(12)), "12.0s");
    }

    /// A turn the engine started on its own still gets a start time, and one
    /// that ends forgets it: no state outlives its turn.
    #[test]
    fn the_turn_clock_starts_and_stops_with_the_turn() {
        let mut chat = chat();
        assert!(chat.turn_elapsed().is_none());
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        assert!(chat.turn_elapsed().is_some());
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        assert!(chat.turn_elapsed().is_none());
        assert!(!chat.turn_active);
        assert!(!frame_text(&mut chat).contains("working"));
    }

    /// The key hints are not what the status row replaces: whatever the row
    /// says, the composer's caption is still there.
    #[test]
    fn the_caption_survives_every_phase() {
        let mut chat = chat();
        assert!(frame_text(&mut chat).contains("enter sends  ·  /model  ·  ctrl-c quits"));
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        assert!(frame_text(&mut chat).contains("enter steers  ·  ctrl-c stops"));
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "hello".into(),
        });
        assert!(frame_text(&mut chat).contains("enter steers  ·  ctrl-c stops"));
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
        });
        assert!(frame_text(&mut chat).contains("enter steers  ·  ctrl-c stops"));
    }

    /// An idle screen is the screen from before this row existed: no row is
    /// laid out at all, so not even a blank line is left above the composer.
    #[test]
    fn an_idle_screen_has_no_status_row() {
        let mut chat = chat();
        assert!(work_row(&chat, 80, &test_theme()).is_none());
        let rows = frame_rows(&mut chat, 80, 24);
        // The composer still starts where it did: four rows from the bottom.
        assert!(rows[20].starts_with('╭'), "{:?}", rows[20]);
        assert!(rows[23].starts_with('╰'), "{:?}", rows[23]);
        for row in &rows {
            assert!(!row.contains("streaming"), "{row:?}");
            assert!(!row.contains("thinking"), "{row:?}");
            assert!(!row.contains("waiting for the first token"), "{row:?}");
            assert!(!row.contains("chars"), "{row:?}");
        }
    }

    /// From the prompt to the first token the row says what it is waiting
    /// for, and for how long: `working` alone cannot tell a live turn from a
    /// stalled one.
    #[test]
    fn a_submitted_turn_waits_for_the_first_token() {
        let mut chat = chat();
        type_text(&mut chat, "read Cargo.toml");
        chat.on_key(Key::Enter, Instant::now());
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("waiting for the first token"), "{row:?}");
        assert!(!row.contains("chars"), "nothing has arrived yet: {row:?}");
        assert!(shown_seconds(&row).is_some_and(|s| s < 1.0), "{row:?}");
        assert!(
            SPINNER.iter().any(|glyph| row.contains(glyph)),
            "no spinner: {row:?}"
        );
        // The masthead keeps the state word but not the clock.
        let masthead = frame_rows(&mut chat, 80, 20)[0].clone();
        assert!(masthead.contains("working"), "{masthead:?}");
        assert!(
            shown_seconds(&masthead).is_none(),
            "the clock is the row's alone: {masthead:?}"
        );
    }

    /// Text arriving moves the row to streaming, and the count is the text
    /// the chat received — it grows with every delta.
    #[test]
    fn the_first_delta_streams_and_the_count_grows() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "Hello".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("streaming"), "{row:?}");
        assert!(row.contains("5 chars"), "{row:?}");

        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: ", world".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("12 chars"), "{row:?}");
    }

    /// Reasoning is not the answer: `ThinkingDelta` gets its own word, and
    /// the first answer text takes it back.
    #[test]
    fn reasoning_is_a_phase_of_its_own() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "anthropic/claude-sonnet-4-5".into(),
        });
        chat.on_event(EngineEvent::ThinkingDelta {
            turn_id: TurnId(1),
            text: "weighing it".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("thinking"), "{row:?}");
        assert!(row.contains("11 chars"), "{row:?}");
        assert!(!row.contains("streaming"), "{row:?}");

        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "Hi".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("streaming"), "{row:?}");
        assert!(!row.contains("thinking"), "{row:?}");
    }

    /// A tool borrows the row and gives it back when its own call finishes.
    #[test]
    fn a_running_tool_owns_the_row_until_its_call_finishes() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "reading".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "read".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("read ·"), "{row:?}");
        assert!(shown_seconds(&row).is_some(), "{row:?}");

        // A result for a call that is not the one on the row leaves it be:
        // two calls can never overwrite each other's state.
        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "c2".into(),
            output: "something else".into(),
            is_error: false,
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("read ·"), "{row:?}");

        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            output: "fn main() {}".into(),
            is_error: false,
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("streaming"), "{row:?}");
        assert!(row.contains("7 chars"), "{row:?}");
        assert!(!row.contains("read ·"), "{row:?}");
    }

    /// While the engine holds a call for an answer, the row is the engine
    /// waiting on a person and names the tool it stopped on.
    #[test]
    fn a_pending_approval_names_the_tool_and_hands_the_row_back() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
        });
        chat.on_event(EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("needs you"), "{row:?}");
        assert!(row.contains("bash"), "{row:?}");

        // Answering it is the composer's `y`; the row goes back to the call
        // that was interrupted, still running.
        chat.on_key(Key::Char('y'), Instant::now());
        let row = above_composer(&mut chat, 80, 20);
        assert!(!row.contains("needs you"), "{row:?}");
        assert!(row.contains("bash ·"), "{row:?}");
    }

    /// No phase outlives its turn, however the turn ended.
    #[test]
    fn no_status_row_survives_a_finished_turn() {
        let started = |chat: &mut Chat| {
            chat.on_event(EngineEvent::TurnStarted {
                turn_id: TurnId(1),
                model: "openai/gpt-4.1".into(),
            });
            chat.on_event(EngineEvent::ToolStarted {
                turn_id: TurnId(1),
                call_id: "c1".into(),
                name: "bash".into(),
            });
            let row = above_composer(chat, 80, 20);
            assert!(row.contains("bash ·"), "{row:?}");
            row
        };

        let mut finished = chat();
        started(&mut finished);
        finished.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        let row = above_composer(&mut finished, 80, 20);
        assert!(!row.contains("bash ·"), "{row:?}");
        assert!(work_row(&finished, 80, &test_theme()).is_none());

        let mut failed = chat();
        started(&mut failed);
        failed.on_event(EngineEvent::Failed {
            turn_id: Some(TurnId(1)),
            message: "no such model".into(),
            reason: titi_providers::ErrorReason::Rejected,
        });
        assert!(work_row(&failed, 80, &test_theme()).is_none());

        let mut cancelled = chat();
        started(&mut cancelled);
        cancelled.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) });
        assert!(work_row(&cancelled, 80, &test_theme()).is_none());
    }

    /// A long tool name is cut with an ellipsis on the row, never wrapped:
    /// every row is exactly as wide as the screen and the composer keeps its
    /// four rows at 60, 80 and 120 columns.
    #[test]
    fn the_status_row_is_cut_to_fit_at_60_80_and_120_columns() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "mcp__a_tool_name_that_is_far_too_long_for_any_narrow_screen_to_show_whole"
                .into(),
        });
        for (width, height) in [(60u16, 20u16), (80, 20), (120, 30)] {
            let rows = frame_rows(&mut chat, width, height);
            let at = height as usize;
            for row in &rows {
                assert_eq!(
                    titi_tui::width::visible_width(row),
                    width as usize,
                    "{width}x{height}: {row:?}"
                );
            }
            let row = &rows[at - 5];
            assert!(row.contains("mcp__a_tool_name"), "{width}: {row:?}");
            if width == 120 {
                assert!(!row.contains('…'), "{width} fits it whole: {row:?}");
            } else {
                assert!(row.contains('…'), "{width}: {row:?}");
            }
            // Nothing of the row leaked onto the line above it, and the
            // composer's borders are still its own four rows.
            assert!(
                !rows[at - 6].contains("mcp__"),
                "{width}: {:?}",
                rows[at - 6]
            );
            assert!(rows[at - 4].starts_with('╭'), "{width}: {:?}", rows[at - 4]);
            assert!(rows[at - 1].starts_with('╰'), "{width}: {:?}", rows[at - 1]);
        }
    }

    /// The extra row must not break a screen too small to hold it: the layout
    /// still draws, every row is exactly as wide as the screen, and nothing
    /// panics with a turn running.
    #[test]
    fn a_tiny_screen_still_draws_while_a_turn_runs() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
        });
        for (width, height) in [(20u16, 6u16), (16, 6), (60, 7), (16, 3)] {
            let rows = frame_rows(&mut chat, width, height);
            assert_eq!(rows.len(), height as usize);
            for row in &rows {
                assert_eq!(
                    titi_tui::width::visible_width(row),
                    width as usize,
                    "{width}x{height}: {row:?}"
                );
            }
        }
    }

    /// The model label is the session's own name, so it goes before the model
    /// does: a truncated session id still names a file, `open -codex/…`
    /// names nothing.
    #[test]
    fn a_narrow_masthead_drops_the_session_label_before_it_cuts_the_model() {
        let mut chat = chat();
        chat.model = "openai-codex/gpt-5.5".to_owned();
        chat.session_label = "SESSIONLABEL".to_owned();

        let wide = frame_rows(&mut chat, 80, 20).join("");
        assert!(wide.contains("openai-codex/gpt-5.5"), "{wide}");
        assert!(wide.contains("SESSIONLABEL"), "{wide}");

        let narrow = frame_rows(&mut chat, 40, 20).join("");
        assert!(
            narrow.contains("openai-codex/gpt-5.5"),
            "the model survived: {narrow}"
        );
        assert!(!narrow.contains("SESSIONLABEL"), "the label went: {narrow}");
    }

    /// What is left after that is cut where it stands, with an ellipsis.
    #[test]
    fn a_model_id_that_still_does_not_fit_is_cut_visibly() {
        let mut chat = chat();
        chat.model = "some-provider/a-very-long-model-name-here".to_owned();
        chat.session_label = "SESSIONLABEL".to_owned();
        let narrow = frame_rows(&mut chat, 44, 20).join("");
        assert!(!narrow.contains("some-provider/a-very-long-model-name-here"));
        assert!(narrow.contains('…'), "the cut is visible: {narrow}");
    }

    /// A provider that refused the key contributes no models and looks
    /// exactly like a provider that has none. `/model` has to say which.
    #[test]
    fn slash_model_shows_why_a_provider_listed_nothing() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed_with_failures(
            vec!["openai/gpt-4.1".to_owned()],
            vec![titi_providers::DiscoveryError::Unauthorized {
                provider: "opencode-go".into(),
                status: 401,
            }],
        );
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());

        let reason = chat
            .lines
            .iter()
            .find(|line| line.text.contains("opencode-go"))
            .unwrap_or_else(|| panic!("no reason in the transcript: {:?}", chat.lines));
        assert_eq!(reason.kind, LineKind::Error);
        assert!(reason.text.contains("401"), "{}", reason.text);
        assert!(
            reason.text.contains("titi --set-key"),
            "the user is not told what to do: {}",
            reason.text
        );
    }

    /// An empty list with a reason behind it must not read as "no models"
    /// alone: that is the case the reason exists for.
    #[test]
    fn slash_model_with_nothing_left_still_names_the_refusal() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed_with_failures(
            Vec::new(),
            vec![titi_providers::DiscoveryError::Forbidden {
                provider: "openai".into(),
                status: 403,
            }],
        );
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());

        let texts: Vec<&str> = chat.lines.iter().map(|line| line.text.as_str()).collect();
        assert!(
            texts.iter().any(|text| text.contains("openai")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|text| *text == "no models"), "{texts:?}");
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
        assert!(view.contains("/usage"), "{view}");
        assert!(view.contains("show token usage"), "{view}");
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
    fn guard_every_listed_command_dispatches_and_does_something() {
        let dispatched = [
            "checkpoint",
            "checkpoints",
            "compact",
            "context",
            "goal",
            "help",
            "keys",
            "usage",
            "login",
            "logout",
            "model",
            "pause",
            "memory",
            "advisor",
            "loop",
            "jobs",
            "recap",
            "rewind",
            "fork",
            "export",
            "btw",
            "settings",
            "switch",
            "budget",
            "duck",
            "hub",
            "join",
            "leave",
            "plan",
            "done",
            "whoami",
            "council",
            "graph",
            "git",
            "diagnose",
        ];

        for name in dispatched {
            assert!(
                COMMANDS.iter().any(|command| command.name == name),
                "/{name} dispatches but is not listed"
            );
        }

        for command in COMMANDS {
            let mut chat = chat();
            let arg = match command.name {
                "loop" => "90s ping",
                "budget" => "200k",
                "goal" | "council" | "graph" | "btw" => "task",
                "rewind" => "1",
                "login" | "logout" => "openai",
                "switch" => "openai/gpt-4.1",
                "memory" => "list",
                "export" => "path.md",
                "git" => "status",
                "jobs" => "list",
                _ => "",
            };

            let text = if arg.is_empty() {
                format!("/{}", command.name)
            } else {
                format!("/{} {}", command.name, arg)
            };

            let lines_before = chat.lines.len();
            let applied = chat.slash(&text).expect("failed to parse slash command");

            let is_unknown = chat.lines.iter().any(|l| {
                l.text
                    .contains(&format!("unknown command /{}", command.name))
            });
            assert!(
                !is_unknown,
                "/{} is listed but not dispatched",
                command.name
            );

            let did_something = applied.effect.is_some()
                || applied.log.is_some()
                || chat.lines.len() > lines_before
                // A picker is an answer too: `/model` and `/login` open one
                // instead of printing.
                || chat.model_picker.is_some()
                || chat.login_picker.is_some();
            assert!(
                did_something,
                "/{} does nothing (no effect, no log, no output)",
                command.name
            );
        }
    }

    /// The badge is the engine's answer, not the keystroke: a mode the
    /// engine never entered must not show as entered.
    #[test]
    fn plan_mode_enters_on_the_engines_word_and_done_leaves() {
        /// The masthead row, where the badge lives. The transcript below it
        /// also says "mode: plan", and that line is not the badge; the test
        /// backend is 80 columns wide, so the first row is the first 80
        /// characters of the frame.
        fn badge(chat: &mut Chat) -> String {
            frame_text(chat).chars().take(80).collect()
        }

        let mut chat = chat();
        type_text(&mut chat, "/plan");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetMode {
                mode: SessionMode::Plan
            }))
        );
        assert_eq!(chat.mode, SessionMode::Agent);
        assert!(!badge(&mut chat).contains("plan"), "badge moved too early");

        chat.on_event(EngineEvent::ModeChanged {
            mode: SessionMode::Plan,
        });
        assert_eq!(chat.mode, SessionMode::Plan);
        let shown = badge(&mut chat);
        assert!(shown.contains("plan"), "{shown}");

        type_text(&mut chat, "/done");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetMode {
                mode: SessionMode::Agent
            }))
        );
        chat.on_event(EngineEvent::ModeChanged {
            mode: SessionMode::Agent,
        });
        assert_eq!(chat.mode, SessionMode::Agent);
        let shown = badge(&mut chat);
        assert!(!shown.contains("plan"), "{shown}");
    }

    #[test]
    fn done_outside_plan_mode_says_so_and_sends_nothing() {
        let mut chat = chat();
        type_text(&mut chat, "/done");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("already in agent mode"))
        );
    }

    #[test]
    fn duck_enters_its_own_mode_and_done_leaves_it() {
        let mut chat = chat();
        type_text(&mut chat, "/duck");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetMode {
                mode: SessionMode::Duck
            }))
        );
        chat.on_event(EngineEvent::ModeChanged {
            mode: SessionMode::Duck,
        });
        let badge: String = frame_text(&mut chat).chars().take(80).collect();
        assert!(badge.contains("duck"), "{badge}");

        type_text(&mut chat, "/done");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetMode {
                mode: SessionMode::Agent
            }))
        );
    }

    /// With host-on-demand, joining an empty hub binds the broker automatically.
    #[test]
    fn join_without_a_broker_hosts_on_demand() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/join");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.hub.joined());
        assert!(chat.hub.peers().contains(&"session-123".to_string()));
    }

    #[test]
    fn presence_fills_the_roster_panel_and_hub_toggles_it() {
        let mut chat = chat();
        chat.hub.ingest(titi_core::hub::HubEvent::Presence {
            agents: vec!["session-123".into(), "scout".into()],
        });
        // The panel is hidden until /hub asks for it.
        assert!(!frame_text(&mut chat).contains("scout"));

        type_text(&mut chat, "/hub");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        let view = frame_text(&mut chat);
        assert!(view.contains("scout"), "{view}");
        assert!(view.contains("2 peer(s)"), "{view}");

        type_text(&mut chat, "/hub");
        chat.on_key(Key::Enter, Instant::now());
        assert!(!frame_text(&mut chat).contains("scout"));
    }

    /// A peer that leaves has to leave the roster too.
    #[test]
    fn a_peer_leaving_drops_off_the_roster() {
        let mut chat = chat();
        chat.hub.ingest(titi_core::hub::HubEvent::Presence {
            agents: vec!["scout".into(), "builder".into()],
        });
        chat.hub.ingest(titi_core::hub::HubEvent::Left {
            agent_id: "scout".into(),
        });
        chat.hub_open = true;
        let view = frame_text(&mut chat);
        assert!(view.contains("builder"), "{view}");
        assert!(!view.contains("scout"), "{view}");
    }

    /// The whole path against a real broker: `/join` registers, the roster
    /// fills, and a peer's broadcast reaches the transcript through the same
    /// non-blocking poll the pump runs.
    #[test]
    fn a_joined_session_hears_its_peers() {
        let dir = tempfile::tempdir().expect("temp");
        let broker = titi_core::hub::HubBroker::bind(dir.path()).expect("broker");
        let peer = titi_core::hub::HubClient::connect(dir.path(), "scout").expect("peer");

        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/join");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(chat.hub.joined());
        assert_eq!(chat.hub.agent_id(), Some("session-123"));
        let view = frame_text(&mut chat);
        assert!(view.contains("scout"), "{view}");

        peer.broadcast("ci is red").expect("broadcast");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            chat.poll_hub();
            if chat
                .lines
                .iter()
                .any(|line| line.text.contains("ci is red"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            chat.lines.iter().any(
                |line| line.kind == LineKind::Note && line.text == "hub scout (all): ci is red"
            ),
            "{:?}",
            chat.lines
        );

        type_text(&mut chat, "/leave");
        chat.on_key(Key::Enter, Instant::now());
        assert!(!chat.hub.joined());
        drop(peer);
        broker.shutdown();
    }

    #[test]
    fn leave_without_a_hub_says_so() {
        let mut chat = chat();
        type_text(&mut chat, "/leave");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("hub: not joined"))
        );
    }

    #[test]
    fn budget_sets_clears_and_reports_a_cap() {
        let mut chat = chat();
        type_text(&mut chat, "/budget 200k");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetBudget {
                tokens: Some(200_000)
            }))
        );

        type_text(&mut chat, "/budget off");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetBudget { tokens: None }))
        );

        chat.on_event(EngineEvent::BudgetUpdated {
            spent: 1_200,
            limit: Some(4_000),
        });
        type_text(&mut chat, "/budget");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("1200 of 4000 tokens spent (30%)")),
            "{:?}",
            chat.lines.last()
        );
    }

    /// A cap in money cannot be enforced without a price table, so it is
    /// refused instead of being converted from a guess.
    #[test]
    fn budget_refuses_money_and_nonsense() {
        let mut chat = chat();
        type_text(&mut chat, "/budget $5");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text.contains("no price table"))
        );

        type_text(&mut chat, "/budget plenty");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text.contains("plenty"))
        );
    }

    /// Hitting the cap pauses: the next prompt is held instead of sent.
    #[test]
    fn a_reached_budget_pauses_the_screen() {
        let mut chat = chat();
        chat.on_event(EngineEvent::BudgetExceeded {
            spent: 4_100,
            limit: 4_000,
        });
        assert!(chat.paused);
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text.contains("budget reached"))
        );

        type_text(&mut chat, "carry on then");
        let blocked = chat.on_key(Key::Enter, Instant::now());
        assert!(blocked.effect.is_none());
        assert!(blocked.log.is_none());

        // Raising the cap is the way out, and it goes to the engine.
        type_text(&mut chat, "/budget 1m");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::SetBudget {
                tokens: Some(1_000_000)
            }))
        );
    }

    #[test]
    fn loop_hands_the_interval_and_prompt_to_the_engine() {
        let mut chat = chat();
        type_text(&mut chat, "/loop 5m check the CI run");
        match chat.on_key(Key::Enter, Instant::now()).effect {
            Some(ChatEffect::Send(EngineCommand::StartLoop {
                interval_secs,
                prompt,
            })) => {
                assert_eq!(interval_secs, 300);
                assert_eq!(prompt.as_str(), "check the CI run");
            }
            other => panic!("expected a loop, got {other:?}"),
        }
    }

    /// A guessed schedule is worse than none: an unreadable interval has to
    /// refuse instead of falling back to a default.
    #[test]
    fn loop_refuses_an_unreadable_interval_and_a_missing_prompt() {
        let mut chat = chat();
        type_text(&mut chat, "/loop soon do the thing");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text.contains("soon"))
        );

        type_text(&mut chat, "/loop 30s");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /loop"))
        );

        type_text(&mut chat, "/loop 0s tick");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("at least one second"))
        );
    }

    #[test]
    fn advisor_consults_with_and_without_a_question() {
        let mut chat = chat();
        type_text(&mut chat, "/advisor");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::Consult { question: None }))
        );

        type_text(&mut chat, "/advisor is the migration safe?");
        match chat.on_key(Key::Enter, Instant::now()).effect {
            Some(ChatEffect::Send(EngineCommand::Consult {
                question: Some(question),
            })) => assert_eq!(question.as_str(), "is the migration safe?"),
            other => panic!("expected a consult, got {other:?}"),
        }
    }

    /// The advisor answers, it never acts: a consult is not a turn and
    /// nothing it says is logged as the assistant's.
    #[test]
    fn an_advisor_answer_is_shown_without_starting_a_turn() {
        let mut chat = chat();
        let applied = chat.on_event(EngineEvent::AdvisorAnswer {
            text: "  you skipped the migration  ".into(),
        });
        assert!(applied.effect.is_none());
        assert!(applied.log.is_none());
        assert!(!chat.turn_active);
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "advisor · you skipped the migration")
        );
    }

    /// Silence from an advisor reads like agreement, so it is reported as a
    /// failure instead.
    #[test]
    fn an_empty_or_failed_consult_is_reported_as_a_failure() {
        let mut silent = chat();
        silent.on_event(EngineEvent::AdvisorAnswer { text: "  ".into() });
        assert!(
            silent
                .lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text.contains("failed consult"))
        );

        let mut broken = chat();
        broken.on_event(EngineEvent::AdvisorFailed {
            reason: "advisor model gpt-x is unavailable: no key".into(),
        });
        assert!(broken.lines.iter().any(|line| {
            line.kind == LineKind::Error
                && line.text.contains("failed consult")
                && line.text.contains("no key")
        }));
    }

    #[test]
    fn jobs_lists_and_cancels() {
        let mut chat = chat();
        type_text(&mut chat, "/jobs");
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Send(EngineCommand::ListJobs))
        );

        type_text(&mut chat, "/jobs cancel job-2");
        match chat.on_key(Key::Enter, Instant::now()).effect {
            Some(ChatEffect::Send(EngineCommand::CancelJob { job_id })) => {
                assert_eq!(job_id.as_str(), "job-2");
            }
            other => panic!("expected a cancel, got {other:?}"),
        }

        type_text(&mut chat, "/jobs cancel");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /jobs cancel"))
        );
    }

    /// A background loop the user cannot see is a loop they cannot stop.
    #[test]
    fn a_running_loop_shows_in_the_status_bar_until_it_stops() {
        let mut chat = chat();
        chat.on_event(EngineEvent::JobStarted {
            job: JobInfo {
                id: "job-1".into(),
                prompt: "watch CI".into(),
                interval_secs: 60,
                runs: 0,
            },
        });
        let view = frame_text(&mut chat);
        assert!(view.contains("1 loop(s)"), "{view}");

        chat.on_event(EngineEvent::JobFinished {
            job_id: "job-1".into(),
        });
        let view = frame_text(&mut chat);
        assert!(!view.contains("loop(s)"), "{view}");
    }

    #[test]
    fn an_empty_job_list_says_so() {
        let mut chat = chat();
        chat.on_event(EngineEvent::JobList { jobs: Vec::new() });
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("no background jobs"))
        );
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
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
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
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
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
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
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

    /// A flow that never answers. The screen's own half of a login is all
    /// this test drives, and it must not open a socket to do it.
    struct NoNetworkFlow;

    impl LoginDriver for NoNetworkFlow {
        fn begin(&self, _provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
            let (_urls, events) = tokio::sync::mpsc::unbounded_channel();
            let (codes, _lines) = tokio::sync::mpsc::unbounded_channel();
            Ok(LoginFlow { events, codes })
        }

        fn begin_device(&self, _provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
            let (_urls, events) = tokio::sync::mpsc::unbounded_channel();
            let (codes, _lines) = tokio::sync::mpsc::unbounded_channel();
            Ok(LoginFlow { events, codes })
        }
    }

    /// Bare `/login` is the subscription picker: the rows sit above the
    /// composer and go away on Esc.
    #[test]
    fn bare_login_paints_the_subscription_picker() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        chat.set_login_driver(Arc::new(NoNetworkFlow));

        type_text(&mut chat, "/login");
        chat.on_key(Key::Enter, Instant::now());
        let frame = frame_text(&mut chat);
        for expected in [
            "Anthropic (Claude Pro/Max)",
            "ChatGPT Plus/Pro (Codex Subscription)",
            "·browser",
            "·device code",
        ] {
            assert!(frame.contains(expected), "{expected} is missing: {frame}");
        }

        chat.on_key(Key::Esc, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(
            !frame.contains("device code"),
            "esc closed the picker: {frame}"
        );
    }

    /// The device grant finishes in the browser, so the composer asks for no
    /// code and offers no Enter: only the way out.
    #[test]
    fn the_device_login_asks_for_no_code() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        chat.set_login_driver(Arc::new(NoNetworkFlow));

        type_text(&mut chat, "/login openai-codex device");
        chat.on_key(Key::Enter, Instant::now());
        type_text(&mut chat, "abc");

        let frame = frame_text(&mut chat);
        assert!(!frame.contains("paste the code"), "{frame}");
        assert!(!frame.contains("enter submits"), "{frame}");
        assert!(
            !frame.contains('•'),
            "no line is typed in device mode: {frame}"
        );
        assert!(frame.contains("esc cancels"), "{frame}");
    }

    /// A provider with an OAuth descriptor asks for a code, not a key: the
    /// composer says which, and Enter hands the line to the flow — never to
    /// the store, which would keep a half of the login as a credential.
    #[test]
    fn login_for_an_oauth_provider_enters_code_mode() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        chat.set_login_driver(Arc::new(NoNetworkFlow));

        type_text(&mut chat, "/login anthropic");
        chat.on_key(Key::Enter, Instant::now());

        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("paste the code or the redirect URL"),
            "{frame}"
        );
        assert!(frame.contains("enter submits"), "{frame}");

        type_text(&mut chat, "sk-test-code");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.login_for.is_some(), "the login is still open");
        assert!(
            crate::secrets::list_keys(dir.path())
                .expect("keys")
                .is_empty(),
            "a pasted code is not a key"
        );
    }

    /// An agent directory whose config declares a provider the builtin table
    /// does not know — how a user adds a gateway of their own.
    fn agent_dir_with_extra_provider() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(
            dir.path().join("config.yml"),
            "providers:\n  \
             - id: zai\n    \
             api: openai-completions\n    \
             base_url: https://api.example.invalid/v1\n    \
             credential_env: ZAI_API_KEY\n    \
             credential_required: true\n\
             models:\n  \
             - id: zai/glm-4.6\n    \
             provider: zai\n    \
             wire_model: glm-4.6\n",
        )
        .expect("config");
        dir
    }

    /// The engine runs on the merged registry, so the screen must too: a
    /// provider the user declared is one `/login` has to take a key for.
    #[test]
    fn login_accepts_a_provider_the_config_declares() {
        let dir = agent_dir_with_extra_provider();
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/login zai");
        chat.on_key(Key::Enter, Instant::now());
        assert_eq!(chat.login_for.as_deref(), Some("zai"));
        assert!(
            !chat
                .lines
                .iter()
                .any(|line| line.text.contains("unknown provider")),
            "{:?}",
            chat.lines
        );
        type_text(&mut chat, "sk-test");
        chat.on_key(Key::Enter, Instant::now());
        let keys = crate::secrets::list_keys(dir.path()).expect("keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].provider, "zai");
    }

    #[test]
    fn keys_lists_a_provider_the_config_declares() {
        let dir = agent_dir_with_extra_provider();
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/keys");
        chat.on_key(Key::Enter, Instant::now());
        for provider in ["zai ", "openai "] {
            assert!(
                chat.lines
                    .iter()
                    .any(|line| line.text.starts_with(provider)),
                "{provider}missing: {:?}",
                chat.lines
            );
        }
    }

    #[test]
    fn diagnose_lists_a_provider_the_config_declares() {
        let dir = agent_dir_with_extra_provider();
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/diagnose");
        chat.on_key(Key::Enter, Instant::now());
        let summary = chat.lines.last().expect("a transcript line");
        for provider in ["zai (", "openai ("] {
            assert!(
                summary.text.contains(provider),
                "{provider} missing: {summary:?}"
            );
        }
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
        let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
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
    fn usage_command_prints_tokens() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 100,
            completion_tokens: 50,
        });
        type_text(&mut chat, "/usage");
        chat.on_key(Key::Enter, Instant::now());
        let view = frame_text(&mut chat);
        assert!(
            view.contains("Turn: 100 prompt + 50 completion. Session: 100 / 50."),
            "View: {view}"
        );
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

    #[test]
    fn council_is_reserved_and_does_not_submit() {
        let mut chat = chat();
        type_text(&mut chat, "/council do we rewrite the parser?");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::RunCouncil { question })) => {
                assert_eq!(question.as_str(), "do we rewrite the parser?");
            }
            other => panic!("expected RunCouncil, got {other:?}"),
        }
        assert!(applied.log.is_none(), "a council is not a user prompt");
        assert!(!chat.turn_active);
    }

    #[test]
    fn council_without_a_question_does_not_submit() {
        let mut chat = chat();
        type_text(&mut chat, "/council");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /council")),
            "{:?}",
            chat.lines
        );
    }

    #[test]
    fn graph_is_reserved_and_does_not_submit() {
        let mut chat = chat();
        type_text(&mut chat, "/graph ship the release");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::RunGraph { task })) => {
                assert_eq!(task.as_str(), "ship the release");
            }
            other => panic!("expected RunGraph, got {other:?}"),
        }
        assert!(applied.log.is_none(), "a graph is not a user prompt");
        assert!(!chat.turn_active);
    }

    #[test]
    fn graph_without_a_task_does_not_submit() {
        let mut chat = chat();
        type_text(&mut chat, "/graph");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /graph")),
            "{:?}",
            chat.lines
        );
    }

    #[test]
    fn a_graph_report_lands_on_the_transcript() {
        let mut chat = chat();
        chat.on_event(EngineEvent::GraphFinished {
            report: "graph: council → goal · verdict pass".into(),
        });
        assert!(chat.lines.iter().any(|line| line.text.contains("verdict")));
        assert!(!chat.turn_active);
    }

    /// `/git` answers on the spot: nothing goes to the engine, and the git
    /// view lands in the transcript under the op that produced it.
    #[test]
    fn git_status_reports_in_the_transcript() {
        let mut chat = chat();
        type_text(&mut chat, "/git");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none(), "/git never reaches the engine");
        assert!(applied.log.is_none(), "/git is not a user prompt");
        assert!(!chat.turn_active);
        let last = chat.lines.last().expect("a transcript line");
        assert!(last.text.starts_with("git status"), "{:?}", chat.lines);
    }

    /// The bare `/git` and `/git status` are the same view.
    #[test]
    fn git_status_is_the_default_op() {
        let mut bare = chat();
        type_text(&mut bare, "/git");
        bare.on_key(Key::Enter, Instant::now());
        let mut named = chat();
        type_text(&mut named, "/git status");
        named.on_key(Key::Enter, Instant::now());
        assert_eq!(bare.lines.last(), named.lines.last());
    }

    /// A commit is a write, and writes stay behind the tool's approval
    /// prompt. The slash command must not become the way around it.
    #[test]
    fn git_commit_is_refused_and_stays_tool_gated() {
        let mut chat = chat();
        type_text(&mut chat, "/git commit -m oops");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(!chat.turn_active);
        let last = chat.lines.last().expect("a transcript line");
        assert_eq!(last.kind, LineKind::Error);
        assert!(
            last.text.contains("status or diff") && last.text.contains("approval"),
            "{:?}",
            last
        );
    }

    #[test]
    fn git_push_is_refused_too() {
        let mut chat = chat();
        type_text(&mut chat, "/git push");
        chat.on_key(Key::Enter, Instant::now());
        let last = chat.lines.last().expect("a transcript line");
        assert_eq!(last.kind, LineKind::Error);
        assert!(last.text.contains("status or diff"), "{:?}", last);
    }

    /// The block exists to be pasted into a public bug report, so a stored
    /// key must not be anywhere in it.
    #[test]
    fn diagnose_summarises_and_never_prints_a_key() {
        let dir = tempfile::tempdir().expect("temp");
        crate::secrets::store_key(dir.path(), "openai", "sk-test").expect("store");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/diagnose");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(
            applied.effect.is_none(),
            "/diagnose never reaches the engine"
        );
        assert!(applied.log.is_none());
        let summary = chat.lines.last().expect("a transcript line");
        assert_eq!(summary.kind, LineKind::Note);
        for part in [
            "titi ",
            "model: openai/gpt-4.1",
            "session: session-123",
            "providers: ",
            "openai (",
            "config: ",
            "genome: ",
            "repo: ",
        ] {
            assert!(summary.text.contains(part), "{part} missing: {summary:?}");
        }
        assert!(
            !chat.lines.iter().any(|line| line.text.contains("sk-test")),
            "a stored key reached the diagnostics block: {:?}",
            chat.lines
        );
    }

    #[test]
    fn diagnose_refuses_a_stray_argument() {
        let mut chat = chat();
        type_text(&mut chat, "/diagnose everything");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        let last = chat.lines.last().expect("a transcript line");
        assert_eq!(last.kind, LineKind::Error);
        assert!(last.text.contains("usage: /diagnose"), "{:?}", last);
    }

    /// 2×2 red PNG. Small enough to keep the kitty transmit in the test.
    const PNG_2X2: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6,
        0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 132, 25, 96, 12, 0, 71, 202, 7, 249, 103, 89, 110, 183, 0, 0, 0, 0, 73, 69, 78, 68,
        174, 66, 96, 130,
    ];

    #[test]
    fn switch_exact_id() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "anthropic/claude-opus-5".to_owned(),
            "openai/gpt-4.1".to_owned(),
        ]);
        type_text(&mut chat, "/switch anthropic/claude-opus-5");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
        // The command sends the switch; the engine's event is what confirms
        // it, once.
        assert!(confirmations(&chat).is_empty(), "{:?}", chat.lines);
        assert_eq!(
            confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
            ["model anthropic/claude-opus-5"]
        );
    }

    #[test]
    fn switch_fuzzy_opus() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "anthropic/claude-opus-5".to_owned(),
            "openai/gpt-4.1".to_owned(),
        ]);
        type_text(&mut chat, "/switch opus");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
    }

    #[test]
    fn switch_fuzzy_does_not_match_subsequences() {
        let mut chat = chat();
        chat.catalog =
            crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-sonnet-4-5".to_owned()]);
        type_text(&mut chat, "/switch opus");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("no model matches \"opus\""))
        );
    }

    #[test]
    fn switch_with_colon_id() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "myco/llama3:8b".to_owned(),
            "openai/gpt-4.1".to_owned(),
        ]);
        type_text(&mut chat, "/switch myco/llama3:8b");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "myco/llama3:8b".into()
            }))
        );
    }

    #[test]
    fn switch_with_level() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "anthropic/claude-opus-5".to_owned(),
            "openai/gpt-4.1".to_owned(),
        ]);
        type_text(&mut chat, "/switch opus:high");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5:high".into()
            }))
        );
    }

    #[test]
    fn switch_role_alias() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        chat.catalog =
            crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-opus-5".to_owned()]);
        std::fs::create_dir_all(&chat.agent_dir).unwrap();
        std::fs::write(
            chat.agent_dir.join("config.yml"),
            "modelRoles:\n  review: anthropic/claude-opus-5\n",
        )
        .unwrap();

        type_text(&mut chat, "/switch @review");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SwitchModel {
                model: "anthropic/claude-opus-5".into()
            }))
        );
    }

    #[test]
    fn switch_role_without_model_roles_fails() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        chat.catalog =
            crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-opus-5".to_owned()]);
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        type_text(&mut chat, "/switch @review");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("no model roles configured"))
        );
    }

    #[test]
    fn switch_role_unknown_fails() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        chat.catalog =
            crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-opus-5".to_owned()]);
        std::fs::create_dir_all(&chat.agent_dir).unwrap();
        std::fs::write(
            chat.agent_dir.join("config.yml"),
            "modelRoles:\n  review: anthropic/claude-opus-5\n",
        )
        .unwrap();

        type_text(&mut chat, "/switch @unknown");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("no such role @unknown"))
        );
    }

    #[test]
    fn switch_multiple_matches_prints_candidates() {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "anthropic/claude-opus-5".to_owned(),
            "aws/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "/switch opus");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());

        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("multiple models match"))
        );
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("anthropic/claude-opus-5"))
        );
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("aws/claude-opus-5"))
        );
    }

    #[test]
    fn memory_list() {
        let mut chat = chat();
        type_text(&mut chat, "/memory list");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::MemoryList))
        );

        type_text(&mut chat, "/memory");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::MemoryList))
        );
    }

    #[test]
    fn memory_search() {
        let mut chat = chat();
        type_text(&mut chat, "/memory search rust");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::MemorySearch {
                query: "rust".into()
            }))
        );
    }

    #[test]
    fn memory_forget() {
        let mut chat = chat();
        type_text(&mut chat, "/memory forget 42");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::MemoryForget { id: 42 }))
        );

        type_text(&mut chat, "/memory forget not_an_id");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("usage: /memory forget <id>"))
        );
    }

    #[test]
    fn memory_result_prints_note() {
        let mut chat = chat();
        chat.on_event(EngineEvent::MemoryResult {
            output: "memory data".into(),
        });
        let view = frame_text(&mut chat);
        assert!(view.contains("memory data"), "{view}");
    }

    #[test]
    fn session_named_updates_label() {
        let mut chat = chat();
        chat.on_event(EngineEvent::SessionNamed {
            session_id: "session-123".into(),
            title: "blue-otter".into(),
        });
        assert_eq!(chat.session_label, "blue-otter");
    }

    #[test]
    fn agent_events_produce_transcript_lines() {
        let mut chat = chat();
        chat.on_event(EngineEvent::AgentStarted {
            agent_id: "agent-1".into(),
            name: "worker".into(),
            parent_id: None,
            kind: titi_engine::protocol::AgentKind::Subagent,
        });
        assert!(
            chat.lines
                .last()
                .unwrap()
                .text
                .contains("agent worker: started")
        );

        chat.on_event(EngineEvent::AgentFinished {
            agent_id: "agent-1".into(),
            summary: "all done".into(),
            success: true,
        });
        assert!(
            chat.lines
                .last()
                .unwrap()
                .text
                .contains("agent agent-1: all done")
        );
        assert_eq!(chat.lines.last().unwrap().kind, LineKind::Tool);
    }

    #[test]
    fn btw_sends_without_log() {
        let mut chat = chat();
        type_text(&mut chat, "/btw hello there");
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::SubmitPrompt { text })) => {
                assert_eq!(text.as_str(), "btw: hello there");
            }
            _ => panic!("expected SubmitPrompt"),
        }
        assert!(applied.log.is_none());
    }

    /// Every physical line of a note is its own row, and a row too long for
    /// the pane wraps instead of being cut: `/diagnose`, `/git diff` and
    /// `/settings` each push one multi-line note, and a single truncated row
    /// threw everything past the first screen width away.
    #[test]
    fn a_multi_line_note_is_one_row_per_line() {
        let theme = test_theme();
        let line = TranscriptLine {
            kind: LineKind::Note,
            text: "alpha\nbeta\n\ngamma".to_owned(),
        };
        let rows = row_texts(&message_rows(&line, 40, &theme).0);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows[0].contains("alpha"), "{rows:?}");
        assert!(rows[1].contains("beta"), "{rows:?}");
        assert!(rows[2].trim().is_empty(), "blank line kept: {rows:?}");
        assert!(rows[3].contains("gamma"), "{rows:?}");
    }

    /// A long line is wrapped over several rows, and no row overflows the
    /// pane — the old renderer dropped the tail instead.
    #[test]
    fn a_long_note_wraps_within_the_width() {
        let theme = test_theme();
        let words = std::iter::repeat_n("token", 60)
            .collect::<Vec<_>>()
            .join(" ");
        let line = TranscriptLine {
            kind: LineKind::Note,
            text: words.clone(),
        };
        let rows = row_texts(&message_rows(&line, 40, &theme).0);
        assert!(rows.len() >= 8, "{rows:?}");
        for row in &rows {
            assert!(
                titi_tui::width::visible_width(row) <= 40,
                "row wider than the pane: {row:?}"
            );
        }
        let joined: String = rows
            .iter()
            .map(|row| row.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            joined.split_whitespace().filter(|w| *w == "token").count(),
            60
        );
    }

    /// An error and a tool chip split the same way a note does.
    #[test]
    fn errors_and_tool_chips_split_too() {
        let theme = test_theme();
        for kind in [LineKind::Error, LineKind::Tool] {
            let line = TranscriptLine {
                kind,
                text: "first\nsecond".to_owned(),
            };
            let rows = row_texts(&message_rows(&line, 40, &theme).0);
            assert_eq!(rows.len(), 2, "{kind:?}: {rows:?}");
            assert!(rows[1].contains("second"), "{kind:?}: {rows:?}");
        }
    }

    /// The URL of a sign-in note is never cut mid-token and never carries the
    /// chip's mark or indent: every row of it is a slice of the URL, so the
    /// rows join back to the URL byte for byte.
    #[test]
    fn a_long_login_url_is_one_slice_per_row() {
        let theme = test_theme();
        let url = format!(
            "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann\
             &response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
             &scope=openid%20profile%20email%20offline_access&state={}",
            "b".repeat(60)
        );
        let line = TranscriptLine {
            kind: LineKind::Note,
            text: format!(
                "login openai-codex: open this URL in your browser\n{url}\nEnter code: WXYZ"
            ),
        };
        let (rows, links) = message_rows(&line, 78, &theme);
        let texts = row_texts(&rows);
        assert!(
            links.len() >= 4,
            "a 300-char URL spans rows: {}",
            links.len()
        );
        let visible: String = links.iter().map(|link| link.text.as_str()).collect();
        assert_eq!(visible, url, "rows lost or changed a byte");
        for link in &links {
            assert_eq!(link.url, url, "every row targets the whole URL");
            assert_eq!(texts[link.row], link.text, "the drawn row is the URL slice");
            assert!(
                titi_tui::width::visible_width(&texts[link.row]) <= 78,
                "row over the pane: {:?}",
                texts[link.row]
            );
            assert!(
                !texts[link.row].contains('·'),
                "the chip mark is not part of the link: {:?}",
                texts[link.row]
            );
        }
        assert!(texts[0].contains("login openai-codex"), "{texts:?}");
        assert!(texts[0].contains('·'), "{texts:?}");
        assert!(
            texts[rows.len() - 1].contains("Enter code: WXYZ"),
            "{texts:?}"
        );
    }

    /// A URL that fits one row is one link row, and the row is the URL.
    #[test]
    fn a_short_login_url_is_one_row() {
        let theme = test_theme();
        let url = "https://auth.openai.com/codex/device";
        let line = TranscriptLine {
            kind: LineKind::Note,
            text: format!(
                "login openai-codex: open this URL on any device\n{url}\nEnter code: WXYZ"
            ),
        };
        let (rows, links) = message_rows(&line, 78, &theme);
        assert_eq!(links.len(), 1, "{:?}", row_texts(&rows));
        assert_eq!(row_texts(&rows)[links[0].row], url);
        assert_eq!(links[0].url, url);
    }

    /// Every other note, and every error, stays exactly as it was: no OSC 8,
    /// even when its text happens to hold a URL.
    #[test]
    fn other_lines_get_no_link_rows() {
        let theme = test_theme();
        for (kind, text) in [
            (
                LineKind::Note,
                "see https://example.invalid/docs for the rest",
            ),
            (LineKind::Error, "login: https://example.invalid/failed"),
            (
                LineKind::Note,
                "login openai-codex: waiting for the browser",
            ),
            (
                LineKind::Note,
                "login openai-codex: no URL\nEnter code: WXYZ",
            ),
            (
                LineKind::Assistant,
                "login x: open this URL\nhttps://example.invalid\nnow",
            ),
        ] {
            let line = TranscriptLine {
                kind,
                text: text.to_owned(),
            };
            let (_, links) = message_rows(&line, 78, &theme);
            assert!(links.is_empty(), "{kind:?} {text:?} grew a link: {links:?}");
        }
    }

    /// The frame's own cells carry the link: the open sequence and the whole
    /// URL sit on the first cell of every row the URL spans, the close on the
    /// last, and what the rows spell is still exactly the URL.
    #[test]
    fn a_frame_hangs_the_whole_url_on_every_row() {
        let url = long_authorize_url('c');
        let mut chat = chat();
        chat.push(
            LineKind::Note,
            format!("login openai-codex: open this URL in your browser\n{url}\nEnter code: WXYZ"),
        );
        let rows = frame_rows(&mut chat, 80, 20);
        let open = format!("\x1b]8;;{url}\x1b\\");
        let link_rows: Vec<&String> = rows.iter().filter(|row| row.contains("\x1b]8;;")).collect();
        let chunks = wrap_url(&url, 78);
        assert!(chunks.len() >= 4, "the URL spans rows: {chunks:?}");
        assert_eq!(link_rows.len(), chunks.len(), "{link_rows:?}");
        let mut shown = String::new();
        for (row, chunk) in link_rows.iter().zip(&chunks) {
            assert_eq!(
                row.matches(&open).count(),
                1,
                "the row targets the whole URL: {row:?}"
            );
            assert_eq!(
                row.matches(titi_tui::caps::OSC8_CLOSE).count(),
                1,
                "{row:?}"
            );
            let visible = strip_escapes(row);
            assert_eq!(visible.trim_end(), *chunk, "row shows its slice: {row:?}");
            assert!(!visible.contains('…'), "nothing elided: {row:?}");
            shown.push_str(visible.trim_end());
        }
        assert_eq!(shown, url, "the rows spell the URL byte for byte");
        assert!(
            rows.iter()
                .any(|row| row.contains("login openai-codex: open this URL")),
            "the head line is still there: {rows:?}"
        );
    }

    /// A URL that fits one row gets exactly one open and one close.
    #[test]
    fn a_one_row_link_has_one_pair() {
        let url = "https://auth.openai.com/codex/device";
        let mut chat = chat();
        chat.push(
            LineKind::Note,
            format!("login openai-codex: open this URL on any device\n{url}\nEnter code: WXYZ"),
        );
        let rows = frame_rows(&mut chat, 80, 20);
        let link_rows: Vec<&String> = rows.iter().filter(|row| row.contains("\x1b]8;;")).collect();
        assert_eq!(link_rows.len(), 1, "{link_rows:?}");
        assert_eq!(link_rows[0].matches("\x1b]8;;").count(), 2);
        assert_eq!(strip_escapes(link_rows[0]).trim_end(), url);
    }

    /// End to end through a real backend: the bytes the terminal receives
    /// carry the whole URL on every row, and a screen that ignores OSC 8
    /// shows the URL, whole, with no ellipsis.
    #[test]
    fn a_frame_and_the_backend_leave_the_url_whole() {
        let url = long_authorize_url('d');
        let mut chat = chat();
        chat.push(
            LineKind::Note,
            format!("login openai-codex: open this URL in your browser\n{url}\nEnter code: WXYZ"),
        );
        let sink = Sink::default();
        // A fixed viewport, not the fullscreen one: `Terminal::new` asks the
        // backend for its size, and a real crossterm backend answers that by
        // querying the terminal, which a CI runner without a tty refuses with
        // `EAGAIN`. The bytes that leave the backend are the same either way.
        let viewport = ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 20));
        let mut terminal = match Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            ratatui::TerminalOptions { viewport },
        ) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
        let raw = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
        let open = format!("\x1b]8;;{url}\x1b\\");
        let chunks = wrap_url(&url, 78);
        assert_eq!(
            raw.matches(&open).count(),
            chunks.len(),
            "every row targets the whole URL"
        );
        assert_eq!(
            raw.matches(titi_tui::caps::OSC8_CLOSE).count(),
            chunks.len()
        );
        let view = screen(&raw, 80, 20);
        let first = match view.iter().position(|row| row.starts_with("https://")) {
            Some(first) => first,
            None => panic!("no URL row on the screen: {view:?}"),
        };
        let shown: String = view[first..first + chunks.len()]
            .iter()
            .map(|row| row.trim_end())
            .collect();
        assert_eq!(
            shown, url,
            "escapes ignored, the screen shows the URL whole"
        );
        assert!(
            view.iter().any(|row| row.contains("login openai-codex")),
            "the head line is on the screen: {view:?}"
        );
    }

    /// A frame with an ordinary note and an error carries no hyperlink at all,
    /// even when the text holds a URL.
    #[test]
    fn a_frame_without_a_login_url_has_no_osc8() {
        let mut chat = chat();
        chat.push(LineKind::Note, "model openai/gpt-4.1".to_owned());
        chat.push(
            LineKind::Error,
            "login: see https://example.invalid/trouble".to_owned(),
        );
        for row in frame_rows(&mut chat, 80, 20) {
            assert!(!row.contains("\x1b]8;;"), "an OSC 8 leaked: {row:?}");
        }
    }

    fn long_authorize_url(fill: char) -> String {
        format!(
            "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann\
             &response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
             &scope=openid%20profile%20email%20offline_access&state={}",
            fill.to_string().repeat(40)
        )
    }

    /// The rendered cells of every row of a frame, escapes included.
    fn frame_rows(chat: &mut Chat, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Every colour a frame's cells carry, foreground and background, in row
    /// order.
    fn frame_colors(chat: &mut Chat, width: u16, height: u16) -> Vec<(Color, Color)> {
        let backend = ratatui::backend::TestBackend::new(width, height);
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
            .map(|cell| (cell.fg, cell.bg))
            .collect()
    }

    /// A screen of every kind of row, so one frame exercises every colour role
    /// the chat has: a user block, a pending tool, a finished tool, an error and
    /// a note, under the masthead, the composer and its caption.
    fn colored_chat(theme: Arc<Theme>) -> Chat {
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme);
        chat.push(LineKind::User, "hi".to_owned());
        chat.push(LineKind::Tool, "tool bash".to_owned());
        chat.push(LineKind::Tool, "tool done  read".to_owned());
        chat.push(LineKind::Error, "broken".to_owned());
        chat.push(LineKind::Note, "a note".to_owned());
        chat
    }

    /// Every colour the live screen draws is a token of the active theme: the
    /// same frame under two themes carries two palettes, each theme's own
    /// accent lands in the cells, and each role resolves to the value its
    /// theme file declares.
    #[test]
    fn a_frame_takes_every_colour_from_the_theme() {
        let mut titanium = colored_chat(test_theme_named("titanium"));
        let mut light = colored_chat(test_theme_named("light"));
        let titanium_colors = frame_colors(&mut titanium, 80, 20);
        let light_colors = frame_colors(&mut light, 80, 20);
        assert_ne!(
            titanium_colors, light_colors,
            "the frame ignored the theme it was given"
        );

        // The accent lands in the cells, and the same role under the other
        // theme is the other theme's accent: the wiring, not just a palette.
        assert!(
            titanium_colors
                .iter()
                .any(|(fg, _)| *fg == Color::Rgb(0, 180, 255)),
            "titanium's accent is not on the screen"
        );
        assert!(
            light_colors
                .iter()
                .any(|(fg, _)| *fg == Color::Rgb(90, 128, 128)),
            "light's accent is not on the screen"
        );

        // The escapes those cells become are crossterm's, not the screen's —
        // and crossterm honours `NO_COLOR` through a process-wide switch
        // (`style::force_color_output`), which a test has no business throwing
        // for every other test in this binary. The colour a cell carries is the
        // part this module decides, so that is what is asserted.

        // One role per token, with the token's own value: the theme files
        // declare these, and a change to one has to be a change here too.
        let theme = test_theme_named("titanium");
        for (token, rgb) in [
            (ThemeColor::Accent, (0, 180, 255)),               // electricBlue
            (ThemeColor::CustomMessageLabel, (212, 192, 144)), // titaniumGold
            (ThemeColor::Warning, (255, 179, 71)),             // warningAmber
            (ThemeColor::Success, (0, 255, 136)),              // readoutGreen
            (ThemeColor::Error, (255, 71, 87)),                // alertRed
            (ThemeColor::Muted, (156, 163, 176)),              // dimAluminum
            (ThemeColor::Dim, (107, 114, 128)),
            (ThemeColor::Border, (42, 48, 56)), // subtleGray
            // `text` is the terminal default on a dark page; the theme answers
            // with the dark default so the screen never loses its body colour.
            (ThemeColor::Text, (229, 229, 231)),
        ] {
            let (r, g, b) = rgb;
            assert_eq!(fg(&theme, token).fg, Some(Color::Rgb(r, g, b)), "{token:?}");
        }
        assert_eq!(bg(&theme, ThemeBg::StatusLineBg), Color::Rgb(15, 18, 22));
        assert_eq!(bg(&theme, ThemeBg::CustomMessageBg), Color::Rgb(42, 48, 56));
    }

    /// A backend that keeps what it was given, so a test can read the bytes
    /// the terminal would have received.
    #[derive(Clone, Default)]
    struct Sink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// What a screen renderer shows: CSI and OSC sequences consumed, CUP
    /// moves the cursor, every other character lands in the grid. A terminal
    /// that ignores OSC 8 sees exactly this.
    fn screen(text: &str, width: usize, height: usize) -> Vec<String> {
        let mut grid = vec![vec![' '; width]; height];
        let mut row = 0usize;
        let mut col = 0usize;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                if row < height && col < width {
                    grid[row][col] = ch;
                }
                col += 1;
                if col >= width {
                    col = 0;
                    row += 1;
                }
                continue;
            }
            match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut command = ' ';
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            command = next;
                            break;
                        }
                        params.push(next);
                    }
                    if command == 'H' {
                        let mut parts = params.split(';');
                        row = parts
                            .next()
                            .and_then(|part| part.parse::<usize>().ok())
                            .unwrap_or(1)
                            .saturating_sub(1);
                        col = parts
                            .next()
                            .and_then(|part| part.parse::<usize>().ok())
                            .unwrap_or(1)
                            .saturating_sub(1);
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        grid.into_iter()
            .map(|row| row.into_iter().collect())
            .collect()
    }

    /// What a screen renderer shows: CSI and OSC sequences removed, the rest
    /// kept in order.
    fn strip_escapes(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The whole block reaches the screen, each piece on a row of its own.
    #[test]
    fn a_multi_line_note_reaches_the_frame() {
        let mut chat = chat();
        chat.push(LineKind::Note, "one\ntwo\nthree".to_owned());
        let view = frame_text(&mut chat);
        let rows: Vec<String> = view
            .chars()
            .collect::<Vec<_>>()
            .chunks(80)
            .map(|row| row.iter().collect())
            .collect();
        let mut at = Vec::new();
        for want in ["one", "two", "three"] {
            let found: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.contains(want))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(found.len(), 1, "{want} once: {rows:?}");
            at.push(found[0]);
        }
        assert!(
            at[0] < at[1] && at[1] < at[2],
            "the three lines share a row: {at:?} {rows:?}"
        );
    }
    #[test]
    fn recap_reports_sections() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();

        let store = titi_core::session::SessionStore::new(&chat.agent_dir).unwrap();
        let session_id = store
            .create(titi_core::session::SessionMeta::default())
            .unwrap();
        chat.session_id = session_id;

        type_text(&mut chat, "/recap");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.lines.iter().any(|line| line.text.contains("Session")));
        assert!(chat.lines.iter().any(|line| line.text.contains("Turns")));
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("roles: 0 user, 0 assistant, 0 system"))
        );
    }
    #[test]
    fn fork_creates_a_new_session_and_says_so() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();

        let store = titi_core::session::SessionStore::new(&chat.agent_dir).unwrap();
        let session_id = store
            .create(titi_core::session::SessionMeta::default())
            .unwrap();
        chat.session_id = session_id;

        type_text(&mut chat, "/fork");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines.iter().any(|line| line.text.contains("forked to")
                && line.text.contains("restart to resume it"))
        );
    }

    #[test]
    fn export_defaults_to_agent_dir_exports() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();

        let store = titi_core::session::SessionStore::new(&chat.agent_dir).unwrap();
        let session_id = store
            .create(titi_core::session::SessionMeta::default())
            .unwrap();
        chat.session_id = session_id;

        type_text(&mut chat, "/export");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.lines.iter().any(|line| line.text.contains("exports")));
    }
    #[test]
    fn transcript_scrolls_with_page_keys() {
        let mut chat = chat();
        for i in 0..50 {
            chat.push(LineKind::Note, format!("line {i}"));
        }

        let view = frame_text(&mut chat);
        assert!(view.contains("line 49"), "bottom line visible");
        assert!(!view.contains("line 0"), "top line hidden");

        chat.on_key(Key::PageUp, Instant::now());
        chat.on_key(Key::PageUp, Instant::now());
        chat.on_key(Key::PageUp, Instant::now());
        let view_scrolled = frame_text(&mut chat);
        assert!(
            view_scrolled.contains("line 0"),
            "top line visible after scroll"
        );
        assert!(
            !view_scrolled.contains("line 49"),
            "bottom line hidden after scroll"
        );

        chat.on_key(Key::PageDown, Instant::now());
        chat.on_key(Key::PageDown, Instant::now());
        chat.on_key(Key::PageDown, Instant::now());
        let view_down = frame_text(&mut chat);
        assert!(view_down.contains("line 49"), "bottom line visible again");

        chat.on_key(Key::PageUp, Instant::now());
        chat.on_key(Key::Char('a'), Instant::now());
        let view_reset = frame_text(&mut chat);
        assert!(view_reset.contains("line 49"), "typing resets to bottom");
    }
}
