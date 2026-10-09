//! Full-screen chat.
//!
//! The state machine does not touch the terminal, so tests drive it with
//! keys and engine events. [`run`] is the only place that owns the screen.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use titi_core::session::Role;
use titi_engine::protocol::{JobInfo, SessionMode};
use titi_engine::{ContextPart, Engine, EngineCommand, EngineEvent};
use titi_tui::caps::MousePreset;
use titi_tui::selection::Selection;
use titi_tui::status_bar::{
    StatusLinePreset, StatusLineStyle, StatusSnapshot, live_snapshot, short_model,
};
use titi_tui::theme::appearance::{self, Appearance, AppearanceEvent, AppearanceInputs};
use titi_tui::theme::{Theme, ThemeBg, ThemeColor};
use tokio::sync::mpsc::error::TryRecvError;

use crate::composer::*;
use crate::herdr::{self, AgentState};
use crate::hub::{HubSession, HubUpdate};
use crate::keys::*;
use crate::login::{LoginDriver, LoginEvent, LoginFlow, OAuthProvider};
use crate::pickers::*;
use crate::session_log::SessionLog;
use crate::transcript::*;
use crate::welcome::*;

pub(crate) const TOOL_PREVIEW: usize = 120;

/// How long after an OSC 11 query a reply's characters are recognized as one.
///
/// The query is answered in microseconds by a terminal that speaks OSC 11, so
/// the window is the slack for a slow one — and it is also the window in which
/// typing could be mistaken for a reply, which is why it is short.
const PROBE_REPLY_WINDOW: Duration = Duration::from_millis(300);

/// Longest byte string still treated as an appearance reply: `OSC 11 ; rgb:…`
/// with four-digit components is well under this.
const PROBE_REPLY_MAX: usize = 32;

/// What a probed appearance did to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// No payload, or an appearance the palette already matches.
    Unchanged,
    /// Mode 2031: the appearance moved, so a fresh OSC 11 query is owed.
    NeedOsc11Query,
    /// The palette for the reported appearance is now on screen.
    ThemeChanged,
}

/// Which of the terminal's own channels this run may use.
///
/// Resolved once, before the first frame: the config says which of them the
/// user wants ([`titi_config::settings::switch_off`] on the keys this build
/// names) and [`titi_tui::caps::TermEnv`] says which of them the terminal
/// offers. Both halves are needed before anything is written, because a
/// sequence sent to a terminal that does not know it is again a byte stream
/// with rubbish in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalFeatures {
    /// A turn that finished cleanly raises a notification (`notify.completion`).
    notify_completion: bool,
    /// A turn that ended in a failure raises one (`notify.error`).
    notify_error: bool,
    /// A turn that stopped on an approval raises one (`notify.ask`).
    notify_ask: bool,
    /// The terminal shows its own progress for a running turn
    /// (`terminal.progress`).
    progress: bool,
    /// The working row shows the generation-rate estimate
    /// (`composer.tokenRate`).
    token_rate: bool,
    /// The channel a notification takes: OSC 777, the BEL, or nothing.
    channel: titi_tui::caps::NotifyChannel,
}

impl Default for TerminalFeatures {
    /// Everything on, over the richest channel: what a chat built without
    /// reading the config — a test's, a cast's — would do on a terminal that
    /// takes all three.
    fn default() -> Self {
        Self {
            notify_completion: true,
            notify_error: true,
            notify_ask: true,
            progress: true,
            token_rate: true,
            channel: titi_tui::caps::NotifyChannel::Osc777,
        }
    }
}

impl TerminalFeatures {
    /// The switches the config sets, over the channels the terminal offers.
    fn resolve(
        settings: Option<&titi_config::settings::Settings>,
        env: &titi_tui::caps::TermEnv,
    ) -> Self {
        let on = |key: &str| {
            !settings.is_some_and(|settings| titi_config::settings::switch_off(settings, key))
        };
        Self {
            notify_completion: on(titi_config::settings::NOTIFY_COMPLETION_KEY),
            notify_error: on(titi_config::settings::NOTIFY_ERROR_KEY),
            notify_ask: on(titi_config::settings::NOTIFY_ASK_KEY),
            // The bar is the terminal's to draw: the switch alone is not
            // enough, and a terminal without one is left quiet.
            progress: on(titi_config::settings::TERMINAL_PROGRESS_KEY) && env.shows_progress(),
            token_rate: on(titi_config::settings::COMPOSER_TOKEN_RATE_KEY),
            channel: env.notification_channel(),
        }
    }
}

/// Which parts of a finished turn's footer the settings leave on.
///
/// Unset means on, the way every other cosmetic switch here reads: a config
/// that cannot be read, or says nothing, leaves the row exactly as it was. The
/// three keys are omp's three `display.*` switches for the same row, named for
/// the surface and the part (`statusLine.preset`, `composer.tokenRate`).
fn footer_switches(
    settings: Option<&titi_config::settings::Settings>,
) -> titi_tui::status::TurnFooterSwitches {
    let off = |key: &str| {
        settings.is_some_and(|settings| titi_config::settings::switch_off(settings, key))
    };
    titi_tui::status::TurnFooterSwitches {
        time: !off(titi_config::settings::DISPLAY_TURN_FOOTER_TIME_KEY),
        tokens: !off(titi_config::settings::DISPLAY_TURN_FOOTER_TOKENS_KEY),
        cache_miss: !off(titi_config::settings::DISPLAY_TURN_FOOTER_CACHE_MISS_KEY),
    }
}

/// Which entries `/tree` opens showing (`treeFilterMode`).
///
/// Unset, unknown, or anything that is not one of the filter's names is the
/// whole tree: a typo in a cosmetic key leaves the panel as it was rather than
/// refusing to start, the way every other cosmetic key here reads.
fn tree_filter(settings: Option<&titi_config::settings::Settings>) -> TreeFilter {
    setting_string(settings, titi_config::settings::TREE_FILTER_MODE_KEY)
        .as_deref()
        .and_then(TreeFilter::from_id)
        .unwrap_or_default()
}

/// How the pinned strip of live agents behaves (`display.pinnedAgents`).
///
/// A strip with nothing live draws no rows in any mode, so the default only
/// decides what happens once an agent starts — omp's default is `collapsed`
/// too, and an idle frame is the frame it always was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PinnedAgents {
    /// No strip at all.
    Off,
    /// Up to [`PINNED_ROWS`] rows, then `… N more`.
    #[default]
    Collapsed,
    /// Every live agent the pane has room for.
    Full,
}

/// Rows the collapsed strip shows before it counts the rest.
const PINNED_ROWS: usize = 3;

impl PinnedAgents {
    /// The name the setting writes (omp's spellings). The resolver reads the
    /// names through [`PinnedAgents::parse`]; this side is what the tests hold
    /// the two to.
    #[cfg(test)]
    pub(crate) fn id(self) -> &'static str {
        match self {
            PinnedAgents::Off => "off",
            PinnedAgents::Collapsed => "collapsed",
            PinnedAgents::Full => "full",
        }
    }

    /// The mode a setting name asks for; `None` for anything else, so a typo in
    /// a cosmetic key leaves the strip as it was.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(PinnedAgents::Off),
            "collapsed" => Some(PinnedAgents::Collapsed),
            "full" => Some(PinnedAgents::Full),
            _ => None,
        }
    }

    /// Whether the preview is drawn on the rows. The switch is separate
    /// (`display.subagentLivePreview`), so this only reports the mode.
    fn shows_rows(self) -> bool {
        self != PinnedAgents::Off
    }
}

/// The pinned mode and the preview switch, read together: the strip is the
/// engine's five agent events made visible, and the two keys are what decide
/// how much of it a screen draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PinnedStrip {
    pub(crate) mode: PinnedAgents,
    /// `display.subagentLivePreview`, unset = off.
    pub(crate) preview: bool,
}

/// One live agent's pinned row: what the strip draws and what its pane needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PinnedAgent {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) kind: titi_engine::AgentKind,
    pub(crate) status: titi_engine::AgentStatus,
    /// The last `AgentActivity` — what it is doing (`tools: read, grep`).
    pub(crate) activity: String,
    /// The agent's own text, as `AgentProgress` streamed it. This is the pane's
    /// body and never a line of the parent's transcript.
    pub(crate) answer: String,
    /// When it started, for the spinner.
    pub(crate) since: Instant,
}

impl PinnedAgent {
    /// The one line the preview shows: what it is doing when the engine said,
    /// and the tail of what it has said when it has not.
    pub(crate) fn preview(&self) -> Option<String> {
        let text = if self.activity.is_empty() {
            self.answer.trim_end()
        } else {
            self.activity.as_str()
        };
        if text.is_empty() {
            return None;
        }
        Some(one_line(text, PREVIEW_CHARS))
    }
}

/// Characters a pinned row's preview keeps, before the strip cuts it to the
/// pane anyway.
const PREVIEW_CHARS: usize = 60;

/// The pinned mode the settings ask for: `display.pinnedAgents` (unset =
/// collapsed), and `display.subagentLivePreview` (unset = off).
fn pinned_agents(settings: Option<&titi_config::settings::Settings>) -> PinnedStrip {
    PinnedStrip {
        mode: setting_string(settings, titi_config::settings::DISPLAY_PINNED_AGENTS_KEY)
            .as_deref()
            .and_then(PinnedAgents::parse)
            .unwrap_or_default(),
        preview: settings.is_some_and(|settings| {
            titi_config::settings::switch_on(
                settings,
                titi_config::settings::DISPLAY_SUBAGENT_PREVIEW_KEY,
            )
        }),
    }
}

/// One notification the run state owes the terminal, from an event the screen
/// saw once.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NotifyKind {
    /// A turn finished, cleanly.
    Completion,
    /// A turn ended in a failure.
    Error,
    /// The agent is blocked on an approval for this tool.
    Ask { tool: String },
}

impl NotifyKind {
    /// The switch this notification answers to.
    fn enabled_in(&self, features: &TerminalFeatures) -> bool {
        match self {
            NotifyKind::Completion => features.notify_completion,
            NotifyKind::Error => features.notify_error,
            NotifyKind::Ask { .. } => features.notify_ask,
        }
    }

    /// The one short fact the notification carries — never a prompt, never a
    /// file's contents, never a secret.
    fn fact(&self) -> String {
        match self {
            NotifyKind::Completion => "turn finished".to_owned(),
            NotifyKind::Error => "turn failed".to_owned(),
            NotifyKind::Ask { tool } => format!("needs approval: {tool}"),
        }
    }
}

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
    /// `app.session.switch`: ctrl+x.
    CtrlX,
    /// `app.model.select`: alt+m.
    AltM,
    /// The `/tree` filter: alt+f, while the tree is open.
    AltF,
    /// The pinned agents: alt+a moves the view through them and back.
    AltA,
    /// `app.history.search`: ctrl+r.
    CtrlR,
    /// Delete the word before the caret: alt+backspace or ctrl+w.
    DeleteWord,
    Esc,
    Up,
    Down,
    Tab,
    /// The caret, in the draft: one character at a time.
    Left,
    Right,
    /// The caret, one word at a time (`tui.editor.cursorWord*`).
    WordLeft,
    WordRight,
    /// The ends of the line (`tui.editor.cursorLine*`).
    Home,
    End,
    /// Delete the character after the caret (`tui.editor.deleteCharForward`).
    Delete,
    /// Delete everything before the caret (`tui.editor.deleteToLineStart`).
    DeleteToStart,
    PageUp,
    PageDown,
    PageUpHalf,
    PageDownHalf,
}

/// What the screen asks the engine or the process to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEffect {
    Send(EngineCommand),
    /// Several commands from one keystroke, sent in order. `/budget off` is
    /// the case: the token bound and the money bound are the engine's two
    /// independent commands, so lifting both takes two of them.
    SendAll(Vec<EngineCommand>),
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
    pub(crate) fn text(role: Role, text: String) -> Self {
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
    pub(crate) fn none() -> Self {
        Self {
            effect: None,
            log: None,
        }
    }

    pub(crate) fn effect(effect: ChatEffect) -> Self {
        Self {
            effect: Some(effect),
            log: None,
        }
    }

    pub(crate) fn send(command: EngineCommand, log: Option<LogWrite>) -> Self {
        Self {
            effect: Some(ChatEffect::Send(command)),
            log,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingApproval {
    pub(crate) call_id: String,
    pub(crate) name: String,
    /// The tool's own one-line description of the call (`bash rm -rf
    /// build`), taken from the `ToolStarted` the engine sent before it
    /// asked. An approval that named only the tool asked for a blind yes.
    pub(crate) detail: Option<String>,
}

/// A question the model asked, waiting on the user.
///
/// The engine stops the turn until [`EngineCommand::AnswerAsk`] arrives, so
/// this is the screen's half of the `ask` tool (`titi_tools::ask`): the
/// question, the choices, and whether more than one may be taken. An approval
/// is a yes or a no; this one is the user's to word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingAsk {
    /// What the answer is correlated with: the engine waits on this id.
    pub(crate) request_id: String,
    pub(crate) question: String,
    /// The choices offered, in the model's order. Empty is a question with no
    /// list, which the user answers in their own words.
    pub(crate) options: Vec<String>,
    /// Whether more than one choice may be taken.
    pub(crate) multi: bool,
    /// Whether the user may answer in their own words besides the list.
    pub(crate) free_text: bool,
    /// One flag per option: the rows ticked. Only a `multi` question uses them.
    pub(crate) chosen: Vec<bool>,
    /// The row the cursor is on.
    pub(crate) selected: usize,
    /// Whether the composer is the answer field, because the user started
    /// typing into it.
    pub(crate) typing: bool,
}

impl PendingAsk {
    /// Whether this question is being answered in the composer: the user has
    /// started typing, or there is no list to pick from — a question with no
    /// choices has nothing else to say it with.
    pub(crate) fn answering(&self) -> bool {
        self.typing || self.options.is_empty()
    }

    /// The rows ticked, in the order they were offered.
    pub(crate) fn ticked(&self) -> Vec<String> {
        self.options
            .iter()
            .zip(&self.chosen)
            .filter(|(_, ticked)| **ticked)
            .map(|(option, _)| option.clone())
            .collect()
    }
}

impl PendingApproval {
    /// What the person is asked to allow: the description when the tool gave
    /// one, else the tool's name.
    pub(crate) fn subject(&self) -> &str {
        self.detail.as_deref().unwrap_or(&self.name)
    }
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
pub(crate) enum WorkPhase {
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
        /// What the tool said it was about to do (`read docs/README.md`), when
        /// it had anything to say: the call's arguments are the model's, and
        /// this is the tool's own one-line description of them.
        detail: Option<String>,
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
pub(crate) struct OAuthLogin {
    provider: &'static OAuthProvider,
    flow: LoginFlow,
    /// How this provider is being signed in. The device grant has no code to
    /// paste back, so the composer must not ask for one.
    pub(crate) method: LoginMethod,
}

/// Conversation on screen. No terminal, no session file.
pub struct Chat {
    pub(crate) lines: Vec<TranscriptLine>,
    pub(crate) input: String,
    /// Where the caret is in `input`: a byte offset on a char boundary, and
    /// never inside a `[Paste #N · …]` marker — a marker stands for a body the
    /// person pasted, so the caret crosses it as one unit. Every write to
    /// `input` goes through the helpers below, which is what keeps both true;
    /// [`Chat::caret`] reads it clamped, so a draft replaced wholesale leaves
    /// the caret at the end of it rather than panicking.
    caret: usize,
    /// The bodies the collapsed markers in `input` stand for, by marker text.
    /// A paste too long to sit in the draft leaves a marker here instead, and
    /// [`Chat::submit`] swaps it for the body; taking the draft away takes
    /// these with it ([`Chat::clear_input`]).
    pub(crate) pastes: HashMap<String, String>,
    /// The number the next paste marker carries: monotonic for the run, so two
    /// markers in one draft can never stand for the same body.
    pub(crate) next_paste: u32,
    /// The vim keys, when `editor.vim` asks for them: which mode the draft is
    /// in and what is half-typed. `None` — the setting off, which is the
    /// default — is what makes every path in [`crate::vim`] unreachable, so
    /// the composer answers keys exactly as it did before the mode existed.
    pub(crate) vim: Option<crate::vim::VimState>,
    pub(crate) turn_active: bool,
    /// When the running turn was asked for. `Some` exactly while
    /// `turn_active`: the status row above the composer reads it for the
    /// spinner and the elapsed seconds, so a request in flight is visible
    /// before the first token.
    pub(crate) turn_started: Option<Instant>,
    /// Which phase the status row is in. Kept current on every turn so the
    /// row never shows a stale one; only read while `turn_active`.
    pub(crate) phase: WorkPhase,
    active_turn_id: Option<titi_engine::TurnId>,
    pub(crate) model: String,
    /// Live: a local server that answers after the first frame adds models,
    /// so the list is read when `/model` runs, not captured at startup.
    pub(crate) catalog: crate::engine::ModelCatalog,
    pub(crate) session_id: String,
    session_label: String,
    pub(crate) agent_dir: PathBuf,
    pub(crate) paused: bool,
    pub(crate) context_percent: Option<u8>,
    /// The model's context window in tokens, as the engine reported it with the
    /// percentage. The gauge is drawn from it, so `None` — before any turn has
    /// stated one — is what keeps the line between the groups blank.
    context_window: Option<u64>,
    /// Which pre-built status line the masthead paints, and what its middle does
    /// with the context. Read from the settings at startup and changed by
    /// `/statusline`.
    pub(crate) status_line: StatusLineStyle,
    pub(crate) reply: String,
    /// Bytes of `reply` already written to the session file. A tool call
    /// splits the turn's text into segments, and each is recorded once.
    recorded_reply: usize,
    /// Where the reply line on screen starts in `reply`. A tool call closes
    /// the line, so the next round's text opens one under the call and its
    /// result instead of being appended above them.
    shown_from: usize,
    thinking: String,
    assistant_at: Option<usize>,
    thinking_at: Option<usize>,
    pub(crate) approval: Option<PendingApproval>,
    /// The question the model is waiting on, if any: answered in the panel
    /// above the composer, or in the composer's own row.
    pub(crate) pending_ask: Option<PendingAsk>,
    session_prompt_tokens: u32,
    session_completion_tokens: u32,
    last_prompt_tokens: u32,
    last_completion_tokens: u32,
    /// The part of the prompt counts above the provider read from its cache.
    session_cached_tokens: u32,
    last_cached_tokens: u32,
    pub(crate) quit_armed: Option<Instant>,
    /// When Esc was last pressed on an empty composer: the first press arms
    /// this, a second inside [`QUIT_WINDOW`] is the rewind chord, and typing
    /// clears it. One field, so the two-press shape has one window.
    pub(crate) esc_armed: Option<Instant>,
    pub(crate) hint: String,
    /// Provider waiting for a key or an OAuth code. The composer masks
    /// whatever is typed in either case.
    pub(crate) login_for: Option<String>,
    /// The OAuth login behind `login_for`, when the provider is signed in
    /// through a browser rather than with a pasted key.
    pub(crate) oauth: Option<OAuthLogin>,
    /// Where a login is started. Production builds the terminal driver on
    /// first use; tests inject one so no socket, browser or provider is
    /// involved.
    login_driver: Option<Arc<dyn LoginDriver>>,
    /// Highlight in the leading-slash command list.
    pub(crate) picker: usize,
    /// Esc hid the list this draft opened. The list is a function of the
    /// draft, so hiding it has to be remembered: it stays hidden until the
    /// draft changes again, which is what lets Esc close the list without
    /// taking the text with it.
    pub(crate) picker_hidden: bool,
    /// Highlight in the bare-`/login` subscription picker; `None` = closed.
    pub(crate) login_picker: Option<usize>,
    /// Ctrl+X: the session the screen is on, in the list of stored sessions.
    pub(crate) session_picker: Option<usize>,
    /// The workspace this screen's session belongs to: where a paste attached
    /// as a file lands, and the root the tools read it back from. Resolved once
    /// (`session_fs::current_workspace`), so a caller that is not the live run —
    /// a test, a cast — can point it somewhere harmless.
    pub(crate) workspace: PathBuf,
    /// `/tree`: the session's own entries as a tree, the leaf marked; `None` =
    /// closed.
    pub(crate) tree_picker: Option<TreePicker>,
    /// Which entries `/tree` opens showing (`treeFilterMode`), and what alt+f
    /// cycles from there. Read from the settings at startup; unset is the whole
    /// tree, which is what `/tree` has always shown.
    pub(crate) tree_filter: TreeFilter,
    /// Whether the screen drops one cell of horizontal padding from its boxes
    /// and its status row (`tui.tight`). Unset is off, so an unset key leaves
    /// every frame exactly as it was.
    pub(crate) tight: bool,
    /// Whether a streamed answer is revealed at a readable rate
    /// (`display.smoothStreaming`), how much of it is on screen, and when the
    /// last frame was. Unset is off, so the delta path is exactly what it was.
    pub(crate) smooth: bool,
    /// Characters of the running answer that are on screen. It only means
    /// anything while `smooth` is on and a turn is streaming.
    pub(crate) revealed: usize,
    /// When the last reveal frame happened, for the next one's frames.
    pub(crate) reveal_at: Option<Instant>,
    /// How the pinned strip above the composer behaves
    /// (`display.pinnedAgents`), and whether its rows preview what the agent is
    /// doing (`display.subagentLivePreview`).
    pub(crate) pinned: PinnedStrip,
    /// The live agents, in the order they started, fed by the engine's five
    /// agent events. Several can be live at once (one `agent` call may spawn up
    /// to four, and a wave up to thirty-two), so this is a list and not a slot.
    pub(crate) agents: Vec<PinnedAgent>,
    /// The agent whose pane has the view (`AgentFocused`); `None` is the
    /// main turn.
    pub(crate) agent_focus: Option<String>,
    /// The screen row the strip starts on, so a click can name the agent it
    /// landed on ([`Chat::agent_at_row`]).
    pub(crate) pinned_top: u16,
    /// How many rows the strip drew, for the same reason.
    pub(crate) pinned_rows: u16,
    /// The large-paste menu: a paste long enough for `paste.menuThreshold`,
    /// held while the panel offers the ways to attach it; `None` = closed.
    pub(crate) paste_menu: Option<PasteMenu>,
    /// How many lines a paste must reach for that menu: the settings' own
    /// count, or [`PASTE_MENU_AFTER`]. `0` never opens it.
    pub(crate) paste_menu_after: u32,
    /// `/sessions <query>`: the hits over stored sessions, filtered as the
    /// query is typed; `None` = closed.
    pub(crate) session_search: Option<SessionSearch>,
    /// `/theme`: the palettes this build carries, filtered by typing.
    pub(crate) theme_picker: Option<ThemePicker>,
    /// The model browser bare `/model` and bare `/switch` open; `None` =
    /// closed.
    pub(crate) model_picker: Option<ModelPicker>,
    /// The emoji suggestion picker: visible while the caret sits after a
    /// `:xx` (2+ name characters and no closing colon), the fourth picker
    /// beside the model, theme and login ones.
    pub(crate) emoji_picker: titi_tui::emoji::EmojiPicker,
    /// Skills the engine discovered, offered by the same picker.
    pub(crate) skills: Vec<SkillRow>,
    /// Kitty or Ghostty unicode placeholders are available.
    pub(crate) kitty: bool,
    pub(crate) tmux: bool,
    pub(crate) photos: Vec<Photo>,
    pub(crate) misses: HashSet<String>,
    pub(crate) next_image_id: u32,
    /// Transmit and placement sequences to write before the next frame.
    pub(crate) kitty_flush: String,
    /// Background loops the engine reported, newest last.
    jobs: Vec<JobInfo>,
    /// Tokens the engine says this session has spent.
    spent_tokens: u64,
    /// The cap `/budget` set, as the engine confirmed it.
    budget: Option<u64>,
    /// What this session has spent in money, as the engine's ledger reported
    /// it (micro-dollars), and the money cap in force. The two are the
    /// engine's own numbers: it bills each round at that round's model price,
    /// so a surface that counted for itself could disagree with the ledger the
    /// cap is tripped against.
    money_spent_micro: u64,
    money_budget_micro: Option<u64>,
    /// The mode the engine confirmed it is in.
    mode: SessionMode,
    /// Membership in the local hub, when `/join` connected.
    hub: HubSession,
    /// Whether the roster panel is shown (`/hub`).
    hub_open: bool,
    pub(crate) scroll_offset: usize,
    pub(crate) last_transcript_height: usize,
    /// The active theme. Every colour the screen draws — the masthead, the
    /// transcript, the chips, the composer, the status row — is a token of it,
    /// so a theme change is a colour change on the whole screen.
    theme: Arc<Theme>,
    /// The newest reply rendered, and what it was rendered from. A streaming
    /// answer is rebuilt every time a delta lands, so the frame that draws it
    /// again between two deltas reads these rows instead of re-parsing the
    /// whole answer ([`Chat::assistant_rows`]).
    pub(crate) reply_render: Option<ReplyRender>,
    /// When the welcome's intro started. Only `run` starts it, as the first
    /// frame reaches a terminal; a chat that never starts it — every test's —
    /// draws the resting frame.
    pub(crate) intro: Option<Instant>,
    /// The running turn's usage as the engine reported it, `(prompt, cached,
    /// completion)`. `Some` only from the turn's `TurnUsage` to its end: the
    /// footer under the answer is built from it, and a cancelled turn that
    /// never reached a round leaves it `None` — which is what keeps the screen
    /// from showing zeros as if they were data.
    turn_usage: Option<(u32, u32, u32)>,
    /// Whether the running turn's request carried a non-empty history. Read
    /// once at the turn's start (see [`Chat::begin_usage_ledger`]).
    turn_history: bool,
    /// What the running turn has cost, in micro-dollars, when its model has a
    /// price. Kept beside `turn_usage` because the money is a fact about that
    /// same report; `None` for an unpriced model, whose footer then states no
    /// figure at all.
    turn_cost_micro: Option<u64>,
    /// What this screen's priced turns have cost, in micro-dollars. `None`
    /// until a priced turn reports: a session whose models are all unpriced
    /// has no total, and `$0.00` would be a number pretending to be a fact.
    session_cost_micro: Option<u64>,
    /// Some turn in this session went unpriced, so the total above is a floor
    /// and not the bill. A session that switches from a priced model to a
    /// local one says so rather than quietly dropping the earlier turns.
    session_cost_partial: bool,
    /// The running turn ended in a failure the screen showed. It holds until
    /// the next turn starts, so the tab title says the turn broke rather than
    /// that it is your turn.
    turn_failed: bool,
    /// The title the terminal was last given, so a tick that changes nothing
    /// writes nothing (`crate::title`).
    last_title: Option<String>,
    /// The channels this run may use: which of the settings are on and what
    /// the terminal itself supports.
    terminal: TerminalFeatures,
    /// Which parts of a finished turn's footer the settings leave on
    /// (`display.turnFooter.time`/`.tokens`/`.cacheMiss`), resolved once with
    /// the terminal's own switches. Unset means on.
    pub(crate) turn_footer: titi_tui::status::TurnFooterSwitches,
    /// Whether the terminal is currently showing the turn's progress bar, so
    /// the tick writes the clear exactly once.
    progress_on: bool,
    /// A notification the run state owes and the next tick will write. Set by
    /// the one event that is the notification's own — a turn's end, a failure,
    /// an approval — never by a streaming delta, and taken by the tick so it
    /// cannot be written twice.
    pending_notify: Option<NotifyKind>,
    /// The generation-rate estimate the working row shows, fed from the
    /// character counts that row already keeps.
    token_rate: titi_tui::status::TokenRate,
    /// The drag selection over the transcript, in screen coordinates; `None`
    /// when nothing is selected. The model is `titi_tui::selection`.
    pub(crate) selection: Option<Selection>,
    /// The transcript's rows as the last frame drew them, as plain text: what
    /// a copy of a selection carries, style and padding left behind.
    pub(crate) last_rows: Vec<String>,
    /// The screen row the transcript starts on. A mouse event arrives in
    /// screen coordinates; this is what turns one into a row of
    /// [`Chat::last_rows`].
    pub(crate) transcript_top: u16,
    /// The mouse preset this run has enabled. `/mouse` changes it, and the
    /// way out disables mouse reporting whatever it is.
    mouse_preset: MousePreset,
    /// Sequences to write before the next frame, outside ratatui's diff: the
    /// mouse preset switching over, an OSC 11 query, an OSC 52 copy.
    pub(crate) output_flush: String,
    /// The appearance the palette on screen was chosen for. A probe reply that
    /// names the same one changes nothing, so a terminal that answers every
    /// focus gain does not repaint the screen each time.
    appearance: Option<Appearance>,
    /// Whether the terminal's appearance may move the theme. `--theme` names
    /// one palette for the run, and a probe must not undo a choice the user
    /// made on the command line.
    appearance_auto: bool,
    /// When the last OSC 11 query went out. A reply arrives on the same stream
    /// the keyboard does, so this is the window in which a reply's characters
    /// are recognized as one and kept out of the composer.
    probe_sent: Option<Instant>,
    /// The reply being reassembled, byte for byte, from the events it arrived
    /// as: `None` when no reply is part-way in.
    probe_bytes: Option<Vec<u8>>,
    /// The Ctrl+R / ↑ browser over this session's own prompts; `None` = closed.
    pub(crate) history_picker: Option<HistoryPicker>,
    /// Which transcript sections are drawn, and how: the `/details` state.
    pub(crate) details: Details,
}

impl Chat {
    pub fn new(model: impl Into<String>, session_id: &str, theme: Arc<Theme>) -> Self {
        let model = model.into();
        Self {
            lines: Vec::new(),
            input: String::new(),
            caret: 0,
            pastes: HashMap::new(),
            next_paste: 0,
            vim: None,
            turn_active: false,
            turn_started: None,
            phase: WorkPhase::Waiting,
            active_turn_id: None,
            model: model.clone(),
            catalog: crate::engine::ModelCatalog::fixed(vec![model]),
            session_id: session_id.to_owned(),
            // Empty until the session has a name: the engine announces one
            // when it makes it, and `run` reads one a resumed session already
            // has. A session with no name has no name segment.
            session_label: String::new(),
            agent_dir: titi_config::agent_dir(),
            paused: false,
            context_percent: None,
            context_window: None,
            status_line: StatusLineStyle::default(),
            reply: String::new(),
            recorded_reply: 0,
            shown_from: 0,
            thinking: String::new(),
            assistant_at: None,
            thinking_at: None,
            approval: None,
            pending_ask: None,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            last_prompt_tokens: 0,
            last_completion_tokens: 0,
            session_cached_tokens: 0,
            last_cached_tokens: 0,
            quit_armed: None,
            esc_armed: None,
            hint: String::new(),
            login_for: None,
            oauth: None,
            login_driver: None,
            picker: 0,
            picker_hidden: false,
            login_picker: None,
            session_picker: None,
            tree_picker: None,
            tree_filter: TreeFilter::default(),
            tight: false,
            smooth: false,
            revealed: 0,
            reveal_at: None,
            pinned: PinnedStrip::default(),
            agents: Vec::new(),
            agent_focus: None,
            pinned_top: 0,
            pinned_rows: 0,
            workspace: crate::session_fs::current_workspace(),
            paste_menu: None,
            paste_menu_after: PASTE_MENU_AFTER,
            session_search: None,
            theme_picker: None,
            model_picker: None,
            emoji_picker: titi_tui::emoji::EmojiPicker::default(),
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
            money_spent_micro: 0,
            money_budget_micro: None,
            mode: SessionMode::Agent,
            hub: HubSession::default(),
            hub_open: false,
            scroll_offset: 0,
            last_transcript_height: 0,
            theme,
            reply_render: None,
            intro: None,
            turn_usage: None,
            turn_history: false,
            turn_cost_micro: None,
            session_cost_micro: None,
            session_cost_partial: false,
            turn_failed: false,
            last_title: None,
            terminal: TerminalFeatures::default(),
            turn_footer: titi_tui::status::TurnFooterSwitches::default(),
            progress_on: false,
            pending_notify: None,
            token_rate: titi_tui::status::TokenRate::new(),
            selection: None,
            last_rows: Vec::new(),
            transcript_top: 0,
            mouse_preset: MousePreset::Buttons,
            output_flush: String::new(),
            appearance: None,
            appearance_auto: true,
            probe_sent: None,
            probe_bytes: None,
            history_picker: None,
            details: Details::new(),
        }
    }

    /// Plays the welcome's intro from `now`: the shine crosses the mark over
    /// the next [`WELCOME_INTRO`], and the frame settles after it.
    fn start_intro(&mut self, now: Instant) {
        self.intro = Some(now);
    }

    /// The sequences the next frame owes outside ratatui's diff: a kitty
    /// graphic's setup, the mouse preset switching over, an OSC 52 copy. One
    /// drain, so the run loop has one place to write them and no sequence can
    /// sit in a field until the next frame that happens to draw.
    fn take_output_flush(&mut self) -> String {
        let kitty = std::mem::take(&mut self.kitty_flush);
        let rest = std::mem::take(&mut self.output_flush);
        if rest.is_empty() {
            return kitty;
        }
        let mut out = kitty;
        out.push_str(&rest);
        out
    }

    // ---- Editing ----------------------------------------------------------

    /// Where the caret is, clamped to the draft and to a char boundary.
    ///
    /// The stored offset is kept honest by the helpers below; this is what
    /// reads it, so a draft assigned wholesale — a test, a replay — cannot
    /// turn a stale offset into a panic.
    pub(crate) fn caret(&self) -> usize {
        let mut at = self.caret.min(self.input.len());
        while at > 0 && !self.input.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    /// Delete the word before the caret: the run of spaces first, then the word
    /// itself — omp's `deleteBeforeCursor`, which the space-hold gesture's
    /// retract used and the live composer had no key for.
    ///
    /// Counted in characters, so a multi-byte or wide character is one
    /// character and not one byte, and taken from where the caret is rather
    /// than from the end of the draft.
    pub(crate) fn delete_word(&mut self) {
        let prefix = &self.input[..self.caret()];
        let trailing = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_whitespace())
            .count();
        let word = prefix
            .chars()
            .rev()
            .skip(trailing)
            .take_while(|ch| !ch.is_whitespace())
            .count();
        if trailing + word == 0 {
            return;
        }
        let start = byte_offset_back(&self.input, self.caret(), trailing + word);
        // A marker holds spaces, so a word taken out of the draft can be a
        // piece of one: the cut is widened to the whole marker (and to nothing
        // else — the spaces before it are not part of the word the caret is
        // in).
        let (start, end) = self.whole_markers(start, self.caret());
        self.input.replace_range(start..end, "");
        self.set_caret(start);
        self.forget_cut_markers();
    }

    /// Puts the caret at `at`, clamped to the draft by [`Chat::caret`].
    ///
    /// For the places outside this module that splice the draft themselves: a
    /// path that replaces a paste marker, say, has to say where the caret
    /// lands rather than reaching for the field.
    pub(crate) fn set_caret(&mut self, at: usize) {
        self.caret = at.min(self.input.len());
    }

    /// Puts the caret at the start of the draft.
    pub(crate) fn caret_to_start(&mut self) {
        self.caret = 0;
    }

    /// Puts the caret at the end of the draft: what a write that replaces the
    /// whole buffer leaves behind.
    pub(crate) fn caret_to_end(&mut self) {
        self.caret = self.input.len();
    }

    /// Inserts `text` at the caret and leaves the caret after it.
    pub(crate) fn insert_at_caret(&mut self, text: &str) {
        let at = self.caret().min(self.input.len());
        self.input.insert_str(at, text);
        self.caret = at + text.len();
    }

    /// The spans of the paste markers this draft holds, left to right.
    ///
    /// A marker is one unit to the caret: it stands for a body the person
    /// pasted, so a caret inside one, or a backspace through one, would leave
    /// text that no longer stands for anything.
    pub(crate) fn marker_spans(&self) -> Vec<(usize, usize)> {
        let mut spans: Vec<(usize, usize)> = self
            .pastes
            .keys()
            .filter_map(|marker| {
                self.input
                    .find(marker.as_str())
                    .map(|at| (at, at + marker.len()))
            })
            .collect();
        spans.sort_unstable();
        spans
    }

    /// The nearest offset outside every marker, travelling `forward`: an offset
    /// that would land inside one is pushed to the end it was heading for.
    pub(crate) fn skip_markers(&self, at: usize, forward: bool) -> usize {
        for (start, end) in self.marker_spans() {
            if at > start && at < end {
                return if forward { end } else { start };
            }
        }
        at
    }

    /// Widens a range that is about to be cut so it never cuts a paste marker
    /// in half: a marker stands for a body, and half a marker is text that
    /// stands for nothing — [`Chat::expand_pastes`] would no longer find it,
    /// and the half would be what the model is sent.
    ///
    /// Every path that takes text out of the draft goes through this — the
    /// character, the word, the whole prefix — so the rule is stated once. A
    /// range that merely *touches* a marker (ends where it begins, begins
    /// where it ends) is left alone: it cuts nothing of it.
    pub(crate) fn whole_markers(&self, start: usize, end: usize) -> (usize, usize) {
        let mut start = start;
        let mut end = end;
        for (at, to) in self.marker_spans() {
            if start < to && at < end {
                start = start.min(at);
                end = end.max(to);
            }
        }
        (start, end)
    }

    /// Forgets the registered bodies whose markers the draft no longer holds.
    ///
    /// A marker that was cut away stands for nothing, so its body goes with
    /// it: a paste that is gone from the screen must not stay alive in the
    /// registry of a draft that can no longer expand it.
    pub(crate) fn forget_cut_markers(&mut self) {
        let gone: Vec<String> = self
            .pastes
            .keys()
            .filter(|marker| !self.input.contains(marker.as_str()))
            .cloned()
            .collect();
        for marker in gone {
            self.pastes.remove(&marker);
        }
    }

    /// Moves the caret one character, skipping a paste marker whole: entering
    /// one from either side lands on its far end.
    pub(crate) fn move_caret(&mut self, delta: isize) {
        let at = if delta < 0 {
            self.input[..self.caret()]
                .char_indices()
                .next_back()
                .map(|(at, _)| at)
                .unwrap_or(0)
        } else {
            self.input[self.caret()..]
                .chars()
                .next()
                .map(|ch| self.caret() + ch.len_utf8())
                .unwrap_or(self.input.len())
        };
        self.caret = self.skip_markers(at, delta >= 0);
    }

    /// Moves the caret one word: the run of spaces and then the run of
    /// non-spaces, the same shape [`Chat::delete_word`] takes out.
    pub(crate) fn move_caret_word(&mut self, delta: isize) {
        let at = if delta < 0 {
            let prefix = &self.input[..self.caret()];
            let spaces = prefix
                .chars()
                .rev()
                .take_while(|ch| ch.is_whitespace())
                .count();
            let word = prefix
                .chars()
                .rev()
                .skip(spaces)
                .take_while(|ch| !ch.is_whitespace())
                .count();
            byte_offset_back(&self.input, self.caret(), spaces + word)
        } else {
            let rest = &self.input[self.caret()..];
            let spaces = rest.chars().take_while(|ch| ch.is_whitespace()).count();
            let word = rest
                .chars()
                .skip(spaces)
                .take_while(|ch| !ch.is_whitespace())
                .count();
            byte_offset_forward(&self.input, self.caret(), spaces + word)
        };
        self.caret = self.skip_markers(at, delta >= 0);
    }

    /// Removes the character before the caret — or the whole paste marker the
    /// caret sits after, because a marker is one unit.
    pub(crate) fn backspace(&mut self) {
        if let Some(at) = self.input[..self.caret()]
            .char_indices()
            .next_back()
            .map(|(at, _)| at)
        {
            // The character behind the caret — or the whole marker it is part
            // of, when that character is inside one.
            let (start, end) = self.whole_markers(at, self.caret());
            self.input.replace_range(start..end, "");
            self.set_caret(start);
            self.forget_cut_markers();
        }
    }

    /// Removes the character after the caret — or the whole marker the caret
    /// sits before.
    pub(crate) fn delete_forward(&mut self) {
        if let Some(ch) = self.input[self.caret()..].chars().next() {
            // The character ahead of the caret — or the whole marker it is part
            // of, when that character is inside one.
            let (start, end) = self.whole_markers(self.caret(), self.caret() + ch.len_utf8());
            self.input.replace_range(start..end, "");
            self.set_caret(start);
            self.forget_cut_markers();
        }
    }

    /// Removes everything before the caret: ctrl+u, `deleteToLineStart` in the
    /// crate's own keybinding table.
    pub(crate) fn delete_to_start(&mut self) {
        let (start, end) = self.whole_markers(0, self.caret());
        self.input.replace_range(start..end, "");
        self.set_caret(start);
        self.forget_cut_markers();
    }

    // ---- Prompt history ---------------------------------------------------
    //
    // What this session has been asked, from the session's own store — the same
    // file the transcript writes and a resume replays. There is no second
    // history file, and nothing here is remembered that the session did not
    // already keep.

    // ---- Terminal appearance --------------------------------------------
    //
    // A terminal can change its background while titi runs — a person flips
    // their OS between light and dark, or switches a terminal profile — and the
    // screen should follow. The reply to an OSC 11 query is the authority; the
    // window's focus is the moment to ask, because that is when a person has
    // just come back to it.

    /// Focus came back: ask the terminal what its background is now.
    pub fn on_focus_gained(&mut self, now: Instant) {
        if self.appearance_auto {
            self.query_appearance(now);
        }
    }

    /// Ask for the background: write the query and open the window a reply is
    /// recognized in.
    fn query_appearance(&mut self, now: Instant) {
        self.probe_sent = Some(now);
        self.probe_bytes = None;
        self.output_flush.push_str(titi_tui::caps::OSC11_QUERY);
    }

    /// Feed an OSC 11 / Mode 2031 probe reply into the theme on screen.
    ///
    /// Mode 2031 is a re-query trigger, not a luminance source: a terminal that
    /// pushes it is telling us the appearance moved, so the reply to a fresh
    /// OSC 11 is what decides the palette.
    pub fn ingest_probe_reply(&mut self, bytes: &[u8]) -> ProbeOutcome {
        match appearance::classify_appearance_bytes(bytes) {
            None => ProbeOutcome::Unchanged,
            Some(event) => self.apply_appearance(event),
        }
    }

    /// Take a key event that is really part of an OSC 11 reply.
    ///
    /// There is one input stream, and the reply travels on it: the event layer
    /// reads `ESC ]` as alt-`]` and the rest as a run of characters. A query
    /// this run just sent arms the machine for [`PROBE_REPLY_WINDOW`]; while it
    /// is armed, the reply's own shape is reassembled byte for byte and handed
    /// to the classifier, and no part of it reaches the composer. Returns true
    /// when the event was the reply's and the caller must not also handle it as
    /// a key.
    pub fn absorb_probe_key(&mut self, key: &KeyEvent, now: Instant) -> bool {
        let armed = self
            .probe_sent
            .is_some_and(|at| now.saturating_duration_since(at) <= PROBE_REPLY_WINDOW);
        if !armed {
            self.probe_bytes = None;
            return false;
        }
        let KeyEvent {
            code, modifiers, ..
        } = *key;
        let KeyCode::Char(ch) = code else {
            // Anything that is not a character is not a probe reply.
            self.probe_bytes = None;
            return false;
        };
        let mut bytes = self.probe_bytes.take().unwrap_or_default();
        if bytes.is_empty() && (ch != ']' || !modifiers.contains(KeyModifiers::ALT)) {
            // Only a reply's first character is taken out of the keyboard's
            // way: `ESC ]`, which the event layer reads as alt-`]`. A person
            // typing in the window after coming back to the window keeps every
            // key, because nothing else can begin a reply.
            return false;
        }
        let Some(carried) = Self::probe_bytes_of(ch, modifiers) else {
            // Not a byte a reply is spelled with.
            self.probe_sent = None;
            return false;
        };
        bytes.extend_from_slice(&carried);
        if bytes.len() > PROBE_REPLY_MAX {
            // Nothing this long is an OSC 11 reply: stop swallowing input.
            self.probe_sent = None;
            return true;
        }
        if let Some(event) = appearance::classify_appearance_bytes(&bytes) {
            self.probe_sent = None;
            if let ProbeOutcome::NeedOsc11Query = self.apply_appearance(event) {
                self.query_appearance(now);
            }
            return true;
        }
        self.probe_bytes = Some(bytes);
        true
    }

    /// The bytes one key event carried on the wire.
    ///
    /// The reply is read as if it were typed, so its bytes have to be spelled
    /// back: a character the layer read as alt-modified had an ESC in front of
    /// it, and the C0 bytes `0x00`–`0x1F` come back as the control chords they
    /// are the key codes for — the reply's BEL terminator arrives as ctrl-`g`.
    /// `None` when the event cannot be part of a reply.
    fn probe_bytes_of(ch: char, modifiers: KeyModifiers) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(2);
        if modifiers.contains(KeyModifiers::ALT) {
            bytes.push(0x1b);
        }
        if modifiers.contains(KeyModifiers::CONTROL) {
            let byte = match ch {
                'a'..='z' => ch as u8 - b'a' + 0x01,
                '4'..='7' => ch as u8 - b'4' + 0x1c,
                ' ' => 0x00,
                _ => return None,
            };
            bytes.push(byte);
        } else {
            let mut encoded = [0u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
        }
        Some(bytes)
    }

    /// The classifier's verdict on one appearance report.
    fn apply_appearance(&mut self, event: AppearanceEvent) -> ProbeOutcome {
        match event {
            AppearanceEvent::Mode2031Requery => ProbeOutcome::NeedOsc11Query,
            AppearanceEvent::Osc11(mode) => {
                if self.appearance == Some(mode) {
                    return ProbeOutcome::Unchanged;
                }
                self.appearance = Some(mode);
                if !self.appearance_auto {
                    return ProbeOutcome::Unchanged;
                }
                match self.theme_for_appearance(mode) {
                    Ok(theme) => {
                        self.theme = theme;
                        ProbeOutcome::ThemeChanged
                    }
                    Err(reason) => {
                        self.push(LineKind::Error, format!("theme: {reason}"));
                        ProbeOutcome::Unchanged
                    }
                }
            }
        }
    }

    /// The palette one appearance's slot holds: the user's choice for it when
    /// they made one, the crate's own pick for it otherwise.
    fn theme_for_appearance(&self, mode: Appearance) -> Result<Arc<Theme>, String> {
        let workspace = crate::session_fs::current_workspace();
        let settings = titi_config::settings::Settings::load(&self.agent_dir, &workspace, &[])
            .map_err(|reason| reason.to_string())?;
        let chosen = settings
            .get(crate::themes::slot_for(mode))
            .and_then(|value| value.as_str().map(str::to_owned));
        let name = chosen.unwrap_or_else(|| match mode {
            Appearance::Light => appearance::AUTO_LIGHT_THEME.to_owned(),
            Appearance::Dark => appearance::AUTO_DARK_THEME.to_owned(),
        });
        crate::themes::theme_named(&name)
    }

    /// Assume the appearance the run started on: the palette in use was chosen
    /// for it, so the first probe reply that names it changes nothing.
    pub fn set_starting_appearance(&mut self, appearance: Appearance) {
        self.appearance = Some(appearance);
    }

    /// Whether an explicit `--theme` took the appearance out of the loop.
    pub fn set_appearance_auto(&mut self, auto: bool) {
        self.appearance_auto = auto;
    }

    /// The mouse preset in force, and whether the terminal is reporting drags.
    pub fn mouse_preset(&self) -> MousePreset {
        self.mouse_preset
    }

    /// `/mouse`: the preset is persisted where the next run reads it and
    /// switched over now.
    fn mouse(&mut self, args: &str) -> Applied {
        let Some(preset) = MousePreset::parse(args.trim()) else {
            self.push(
                LineKind::Note,
                "mouse: off, wheel, buttons, all (or on/off)".to_owned(),
            );
            return Applied::none();
        };
        let saved = crate::session_fs::save_mouse_preset_to(&self.agent_dir, preset);
        self.mouse_preset = preset;
        // Off and then on: a preset that narrows must not leave the wider one's
        // modes set. `Off`'s own enable sequence is the disable for all four.
        self.output_flush.push_str(MousePreset::Off.enable());
        self.output_flush.push_str(preset.enable());
        match saved {
            Ok(()) => self.push(
                LineKind::Note,
                format!(
                    "mouse: {}{}",
                    preset.name(),
                    if preset == MousePreset::Off {
                        " (the terminal's own selection is back)"
                    } else {
                        " (drag selects · release copies)"
                    }
                ),
            ),
            Err(reason) => self.push(LineKind::Error, format!("mouse: not saved ({reason})")),
        }
        Applied::none()
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
                self.begin_usage_ledger();
                self.begin_rate();
                self.active_turn_id = Some(turn_id);
                self.model = model.to_string();
                self.reply.clear();
                self.recorded_reply = 0;
                self.shown_from = 0;
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
            EngineEvent::ToolStarted {
                call_id,
                name,
                detail,
                ..
            } => {
                // The answer's line closes here, so it settles here too: a
                // reveal still in flight would otherwise keep drawing into a
                // line the tool call has already ended.
                self.reveal_all();
                self.phase = WorkPhase::Tool {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    detail: detail.as_ref().map(ToString::to_string),
                    since: Instant::now(),
                };
                self.assistant_at = None;
                self.shown_from = self.reply.len();
                // The chip keeps what the call did (`bash cargo test`), not
                // only which tool it was: once the row moves on, the
                // transcript is the only place that says what ran.
                let shown = detail.as_deref().unwrap_or(name.as_str());
                self.push(LineKind::Tool, format!("tool {shown}"));
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
                let detail = match &self.phase {
                    WorkPhase::Tool {
                        call_id: running,
                        detail,
                        ..
                    } if running.as_str() == call_id.as_str() => detail.clone(),
                    _ => None,
                };
                self.approval = Some(PendingApproval {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    detail,
                });
                // The engine stops here until a person answers, and the person
                // may be in another window: this is the one event that owes
                // the `ask` notification, emitted once for the one approval.
                self.pending_notify = Some(NotifyKind::Ask {
                    tool: name.to_string(),
                });
                // The phase is left as it stands. The engine emits
                // `ToolStarted` before it asks (crates/titi-engine/src/
                // tool_loop.rs:145 then :271), so the running call keeps its
                // own clock while the person decides, and answering hands
                // the row back to that call without a second start time.
                self.hint.clear();
                Applied::none()
            }
            EngineEvent::AskRequested {
                request_id,
                question,
                options,
                multi,
                free_text,
            } => {
                let options: Vec<String> =
                    options.iter().map(|option| option.to_string()).collect();
                // The transcript keeps what was asked, whole, so scrollback
                // shows the question and the choice that answered it once the
                // panel is gone: the tool's own chip carries only the head of
                // a question (`titi_tools::ask`'s `CHIP_CHARS`).
                self.push(LineKind::Note, format!("ask · {question}"));
                if !options.is_empty() {
                    self.push(
                        LineKind::Note,
                        format!(
                            "ask · options: {}{}",
                            options.join(" · "),
                            if multi { " · choose any" } else { "" }
                        ),
                    );
                }
                // The composer becomes the answer field, so a half-typed
                // prompt goes: an answer and a prompt are different things,
                // and the turn is stopped until the question is answered.
                self.clear_input();
                self.pending_ask = Some(PendingAsk {
                    request_id: request_id.to_string(),
                    question: question.to_string(),
                    chosen: vec![false; options.len()],
                    options,
                    multi,
                    free_text,
                    selected: 0,
                    typing: false,
                });
                Applied::none()
            }
            EngineEvent::ToolFinished {
                call_id,
                output,
                is_error,
                detail,
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
                if let Some(detail) = detail {
                    // A tool that has something to show besides its answer:
                    // the diff of what it changed today. It is presentation
                    // only — the model was told the answer alone — and the
                    // renderer decides what to draw from it.
                    self.push(LineKind::Diff, detail.to_string());
                } else {
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
                }
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
                // The window is the gauge's scale, and the label's second half:
                // the same number the percentage was taken against, kept rather
                // than recomputed.
                self.context_window = Some(window);
                Applied::none()
            }
            EngineEvent::ModelSwitched { turn_id, from, to } => {
                self.model = to.to_string();
                // A switch inside a turn is the engine giving up on the model
                // the user chose; naming only the new one would pass it off
                // as their own switch.
                let line = if turn_id.is_some() {
                    format!("model {to} · fallback from {from}")
                } else {
                    format!("model {to}")
                };
                self.push(LineKind::Note, line);
                Applied::none()
            }
            EngineEvent::Compacted {
                folded,
                tokens_before,
                ..
            } => {
                // A divider, not a note: it stands for the history above it,
                // which the fold keeps off the screen until `/details folded
                // expanded`. Both numbers come from this payload — the count
                // and the tokens the engine held when it folded — because the
                // transcript cannot know either by looking at itself.
                self.push(LineKind::Fold, fold_divider_label(folded, tokens_before));
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
                self.turn_failed = true;
                if turn_id.is_some() && turn_id == self.active_turn_id {
                    // A failure that ends the turn the user is waiting on is
                    // the `error` notification; one that belongs to some other
                    // turn is not this screen's turn and raises nothing.
                    self.pending_notify = Some(NotifyKind::Error);
                    self.finish_turn()
                } else {
                    Applied::none()
                }
            }
            EngineEvent::Cancelled { .. } => {
                // A cancel is the user's own hand on the screen, and they are
                // looking at it: no notification.
                self.push(LineKind::Note, "cancelled".to_owned());
                self.finish_turn()
            }
            EngineEvent::TurnFinished { .. } => {
                self.pending_notify = Some(NotifyKind::Completion);
                self.finish_turn()
            }
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
                    self.pastes.clear();
                    self.input = text.to_string();
                    self.caret_to_end();
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
                cached_tokens,
                cost_micro_usd,
                ..
            } => {
                self.last_prompt_tokens = prompt_tokens;
                self.last_completion_tokens = completion_tokens;
                self.last_cached_tokens = cached_tokens;
                // The running turn's own ledger, for the footer under its
                // answer; the totals below outlive it.
                self.turn_usage = Some((prompt_tokens, cached_tokens, completion_tokens));
                // Money is the engine's own figure for the turn: its ledger
                // bills each round at that round's model price, so a surface
                // that computed its own from the model the turn ended on could
                // disagree with the cap the engine trips. `None` is unpriced
                // — or a turn whose rounds were not all priced — and the
                // session total then says it is a floor, not a bill.
                match cost_micro_usd {
                    Some(cost) => {
                        self.turn_cost_micro = Some(cost);
                        self.session_cost_micro = Some(self.session_cost_micro.unwrap_or(0) + cost);
                    }
                    None => {
                        self.turn_cost_micro = None;
                        self.session_cost_partial = true;
                    }
                }
                self.session_prompt_tokens += prompt_tokens;
                self.session_completion_tokens += completion_tokens;
                self.session_cached_tokens += cached_tokens;
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
            EngineEvent::MoneyBudgetUpdated {
                spent_micro_usd,
                limit_micro_usd,
            } => {
                self.money_spent_micro = spent_micro_usd;
                self.money_budget_micro = limit_micro_usd;
                Applied::none()
            }
            EngineEvent::MoneyBudgetExceeded {
                spent_micro_usd,
                limit_micro_usd,
            } => {
                // The engine has already stopped starting turns: the same
                // paused state the token cap leaves, in money.
                self.money_spent_micro = spent_micro_usd;
                self.money_budget_micro = Some(limit_micro_usd);
                self.paused = true;
                self.push(
                    LineKind::Error,
                    format!(
                        "budget reached: {} of {} in money · paused · /budget <amount> raises it",
                        usd(spent_micro_usd),
                        usd(limit_micro_usd)
                    ),
                );
                Applied::none()
            }
            EngineEvent::MoneyBudgetUnpriced { model } => {
                // An unpriced model is not a free one: the engine cannot
                // measure it, so it will not pretend the cap binds.
                self.push(
                    LineKind::Error,
                    format!(
                        "budget: {model} has no price, so a cap in money cannot be enforced over it"
                    ),
                );
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
            // The five agent events, in one place: the strip is their only
            // surface, so the screen's job is to keep the rows current and let
            // the pane read them. `AgentProgress` is the agent's own text and
            // never touches the parent's transcript; `AgentActivity` is the one
            // status line that replaces in place.
            EngineEvent::AgentStarted {
                agent_id,
                name,
                kind,
                ..
            } => {
                // The note line stays: it is what builds the transcript's
                // `subagents` section (the same `LineKind::Agent` a finished
                // agent writes), and the strip does not replace that — it says
                // who is alive *now*, which is the one thing a line that
                // scrolls cannot. The two are complementary, not duplicates.
                self.push(LineKind::Agent, format!("tool agent {name}: started"));
                self.agents.retain(|agent| agent.id != agent_id);
                self.agents.push(PinnedAgent {
                    id: agent_id.to_string(),
                    name: name.to_string(),
                    kind,
                    status: titi_engine::AgentStatus::Running,
                    activity: String::new(),
                    answer: String::new(),
                    since: Instant::now(),
                });
                Applied::none()
            }
            EngineEvent::AgentProgress { agent_id, text } => {
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
                    agent.answer.push_str(&text);
                }
                Applied::none()
            }
            EngineEvent::AgentActivity { agent_id, text } => {
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
                    agent.activity = text.to_string();
                }
                Applied::none()
            }
            EngineEvent::AgentStatusChanged { agent_id, status } => {
                let terminal = !matches!(
                    status,
                    titi_engine::AgentStatus::Running
                        | titi_engine::AgentStatus::Idle
                        | titi_engine::AgentStatus::Parked
                );
                if terminal {
                    self.forget_agent(&agent_id);
                } else if let Some(agent) =
                    self.agents.iter_mut().find(|agent| agent.id == agent_id)
                {
                    agent.status = status;
                }
                Applied::none()
            }
            EngineEvent::AgentFocused { agent_id } => {
                // The engine is the one that knows whether an agent exists; the
                // screen only shows what it is told, and a focus on nothing is
                // the main turn.
                self.agent_focus = agent_id.as_ref().map(|id| id.to_string());
                Applied::none()
            }
            EngineEvent::AgentFinished {
                agent_id,
                summary,
                success,
                ..
            } => {
                self.forget_agent(&agent_id);
                // The outcome stays a note: it is the one thing about a
                // finished agent the strip cannot show, because the row goes
                // when the agent does.
                if success {
                    self.push(
                        LineKind::Agent,
                        format!("tool done  agent {agent_id}: {summary}"),
                    );
                } else {
                    self.push(
                        LineKind::Agent,
                        format!("tool error agent {agent_id}: {summary}"),
                    );
                }
                Applied::none()
            }
            _ => Applied::none(),
        }
    }

    /// Drop the draft — and the pasted bodies its markers stood for, so a
    /// marker cannot outlive the message it was pasted into.
    pub(crate) fn clear_input(&mut self) {
        self.input.clear();
        self.pastes.clear();
        self.caret = 0;
        // A half-typed vim command (`2d`) is about the draft that just went
        // away, so it goes with it.
        if let Some(state) = self.vim.as_mut() {
            *state = state.cleared();
        }
    }

    pub(crate) fn submit(&mut self, now: Instant) -> Applied {
        // The draft as the screen has it — markers, not the walls they stand
        // for. Every decision below reads this, so a collapsed paste can never
        // be mistaken for a command the user typed.
        let draft = self.input.trim().to_owned();
        if draft.is_empty() {
            return Applied::none();
        }
        // A bare word that means "leave" is the one prompt the composer
        // answers itself. It is matched whole: a prompt that merely starts
        // with those letters (`exit code`) goes to the model like any other.
        if matches!(draft.as_str(), "exit" | "quit" | "q") {
            return self.exit_word(now);
        }
        if let Some(applied) = self.slash(&draft) {
            self.clear_input();
            self.disarm();
            return applied;
        }
        if self.paused {
            self.clear_input();
            self.disarm();
            self.push(LineKind::Note, "paused · /pause resumes".to_owned());
            return Applied::none();
        }
        // What the model reads: the markers swapped for the bodies they stand
        // for, so the whole paste is sent while the screen keeps the marker.
        let text = self.expand_pastes(&draft);
        self.clear_input();
        self.disarm();
        self.push(LineKind::User, draft.clone());
        let log = Some(LogWrite::text(Role::User, text.clone()));
        if self.turn_active {
            Applied::send(EngineCommand::Steer { text: text.into() }, log)
        } else {
            self.turn_active = true;
            self.turn_started = Some(now);
            self.begin_usage_ledger();
            self.begin_rate();
            Applied::send(EngineCommand::SubmitPrompt { text: text.into() }, log)
        }
    }

    /// A new turn for the rate estimate: no reading until this turn's own
    /// deltas arrive. Called where a turn opens, so a stale figure from the
    /// last one cannot be read as this one's.
    fn begin_rate(&mut self) {
        self.token_rate.begin();
    }

    /// Feed the rate estimate with the characters the working row already
    /// counts — the answer's, or the reasoning's while no answer has started —
    /// so there is no second counter to drift from the row's own number.
    fn sample_rate(&mut self, now: Instant) {
        if !self.turn_active {
            return;
        }
        let chars = self.reply.chars().count() + self.thinking.chars().count();
        self.token_rate.observe(chars, now);
    }

    /// The rate segment for the working row: the last reading, or nothing when
    /// the switch is off or no deltas have arrived yet.
    fn rate_segment(&self) -> Option<String> {
        if !self.terminal.token_rate {
            return None;
        }
        self.token_rate.reading().map(titi_tui::status::format_rate)
    }

    /// Opens the running turn's usage ledger: nothing reported yet, and
    /// whether the request this turn is about to make carries history.
    ///
    /// The engine reports a turn's prompt, completion and cached tokens and
    /// nothing about the messages behind them, so "non-empty history" is read
    /// from what it has reported so far: a session that has already paid for a
    /// request (`session_prompt_tokens`, which every earlier `TurnUsage`
    /// added to) or a resumed session whose answers are already on screen. The
    /// first request of a fresh session has no history to re-read, so a cold
    /// cache there is a provider's norm and not a miss worth naming.
    fn begin_usage_ledger(&mut self) {
        self.turn_usage = None;
        self.turn_cost_micro = None;
        self.turn_failed = false;
        self.turn_history = self.session_prompt_tokens > 0
            || self
                .lines
                .iter()
                .any(|line| line.kind == LineKind::Assistant);
    }

    /// The run state the tab title should show.
    fn title_state(&self) -> crate::title::TitleState {
        use crate::title::TitleState;
        if self.approval.is_some() || self.quit_armed.is_some() || self.login_for.is_some() {
            return TitleState::Blocked;
        }
        if self.turn_failed {
            return TitleState::Error;
        }
        if !self.turn_active {
            return TitleState::Idle;
        }
        match self.phase {
            WorkPhase::Waiting => TitleState::Waiting,
            WorkPhase::Streaming => TitleState::Streaming,
            WorkPhase::Thinking => TitleState::Thinking,
            WorkPhase::Tool { .. } => TitleState::Tool,
        }
    }

    /// The OSC 2 sequence for the current run state, or `None` when the
    /// terminal is already showing it.
    ///
    /// Called from the run loop's own tick — the 50 ms poll the progress row
    /// already rides — so the tab follows the turn without a timer of its own.
    /// The title is a function of the state and the label and not of the
    /// clock, so a tick that changes neither writes nothing at all.
    fn title_sequence(&mut self) -> Option<String> {
        let glyphs = crate::title::TitleGlyphs::for_theme(&self.theme);
        let label = if self.session_label.is_empty() {
            short_model(&self.model)
        } else {
            self.session_label.clone()
        };
        let composed = crate::title::title(self.title_state(), &label, &glyphs);
        if self.last_title.as_deref() == Some(composed.as_str()) {
            return None;
        }
        self.last_title = Some(composed.clone());
        Some(crate::title::set_title(&composed))
    }

    /// Everything this tick owes the terminal's own channels, in one string:
    /// the tab title when the run state changed, the OSC 9;4 progress bar
    /// raised with the turn and cleared on every way out of it, and any
    /// notification the run state earned. `None` when there is nothing to
    /// write, so an unchanged tick writes no bytes at all.
    ///
    /// One method owns all three because they are one lifecycle: the bar is
    /// raised and cleared exactly where the title moves between "your turn"
    /// and "working", and the clear rides the same exit the title does. A
    /// second hook for the bar would be a second place to forget on a failure
    /// or a cancel, and a bar left running says the agent is still working
    /// when nobody is.
    fn terminal_tick(&mut self) -> Option<String> {
        let mut out = String::new();
        if let Some(title) = self.title_sequence() {
            out.push_str(&title);
        }
        // Level-triggered on the turn's own flag, so it is written once when
        // the turn starts and once when it ends — cancel, failure and finish
        // all land in `turn_active == false` before the next tick.
        let want = self.turn_active && self.terminal.progress;
        if want != self.progress_on {
            self.progress_on = want;
            out.push_str(if want {
                crate::title::PROGRESS_SET
            } else {
                crate::title::PROGRESS_CLEAR
            });
        }
        // Edge-triggered from the events themselves: `take` is what makes it
        // exactly one notification per event.
        if let Some(pending) = self.pending_notify.take()
            && let Some(sequence) = self.notify_sequence(&pending)
        {
            out.push_str(&sequence);
        }
        (!out.is_empty()).then_some(out)
    }

    /// The bytes one notification takes, or `None` when its switch is off or
    /// the terminal has no channel for it.
    ///
    /// The title is the brand and the body is the session's label (or, before
    /// it has one, the model) plus one short fact. Nothing here is a prompt, a
    /// file path or any other text the user's work put on the screen.
    fn notify_sequence(&self, pending: &NotifyKind) -> Option<String> {
        use titi_tui::caps::{BEL, NotifyChannel, osc777_notify};
        if !pending.enabled_in(&self.terminal) {
            return None;
        }
        let label = if self.session_label.is_empty() {
            short_model(&self.model)
        } else {
            self.session_label.clone()
        };
        let body = format!("{} · {}", label, pending.fact());
        match self.terminal.channel {
            NotifyChannel::Osc777 => Some(osc777_notify("titi", &body)),
            NotifyChannel::Bell => Some(BEL.to_owned()),
            NotifyChannel::None => None,
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
            "checkpoint" => self.session_note(crate::session_fs::checkpoint_session(
                &self.agent_dir,
                &crate::session_fs::current_workspace(),
                &self.session_id,
            )),
            "checkpoints" => self.session_note(crate::session_fs::list_checkpoints(
                &self.agent_dir,
                &self.session_id,
            )),
            "rewind" => self.rewind(args),
            "exit" | "quit" => self.exit_word(Instant::now()),
            "recap" => self.recap(),
            "sessions" => self.sessions(args),
            "tree" => self.open_tree(),
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
            "genome" => self.genome(args),
            "usage" => self.usage(),
            "context" => self.describe_context(args),
            "compact" => self.compact(args),
            "details" => self.details(args),
            "help" => self.help(),
            "hotkeys" => self.hotkeys(),
            "login" => self.login(args),
            "logout" => self.logout(args),
            "keys" | "whoami" => self.keys(),
            "theme" => self.theme(args),
            "statusline" => self.statusline(args),
            "changelog" => self.changelog(args),
            "mouse" => self.mouse(args),
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
        // Cached input bills cheaper; it is named only when there was some.
        let cached = |tokens: u32| {
            if tokens > 0 {
                format!(" ({tokens} cached)")
            } else {
                String::new()
            }
        };
        let text = format!(
            "Turn: {} prompt{} + {} completion. Session: {}{} / {}{}.",
            self.last_prompt_tokens,
            cached(self.last_cached_tokens),
            self.last_completion_tokens,
            self.session_prompt_tokens,
            cached(self.session_cached_tokens),
            self.session_completion_tokens,
            self.session_cost()
        );
        self.push(LineKind::Note, text);
        Applied::none()
    }

    /// The money that joins `/usage`'s token totals, or nothing.
    ///
    /// Nothing is the answer for a session whose models have no price: no
    /// figure was ever computed, and `$0.00` would state one that was. An
    /// exact zero — a model priced at zero — is a figure, so it prints.
    ///
    /// A session that mixed a priced model with an unpriced one has a floor
    /// and not a bill, and says so: the turns it could not price are not in
    /// the number, so the number is the least it spent.
    fn session_cost(&self) -> String {
        match self.session_cost_micro {
            Some(micro) if micro > 0 || !self.session_cost_partial => {
                let total =
                    titi_tui::status::format_usd(micro, titi_tui::status::SESSION_COST_DECIMALS);
                if self.session_cost_partial {
                    format!(" · session total {total}+ (unpriced turns excluded)")
                } else {
                    format!(" · session total {total}")
                }
            }
            _ => String::new(),
        }
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
    fn genome(&mut self, args: &str) -> Applied {
        // The same effective view the engine reads, so a note cannot disagree
        // with what a run would do: project `.titi/config.yml` wins.
        let workspace = crate::session_fs::current_workspace();
        let settings = match titi_config::settings::Settings::load(&self.agent_dir, &workspace, &[])
        {
            Ok(settings) => Some(settings),
            Err(reason) => {
                // A broken config is one error line, not a panic; the note
                // about the switch still states what the load could salvage.
                self.push(
                    LineKind::Error,
                    format!("genome: config load said: {reason}"),
                );
                None
            }
        };

        let (bound, rest) = match args.split_once(char::is_whitespace) {
            Some((cmd, rest)) => (cmd, rest.trim()),
            None => (args, ""),
        };
        match bound {
            "" => {
                self.push(
                    LineKind::Note,
                    crate::engine::genome_note(&settings, &self.agent_dir),
                );
            }
            "on" | "off" => {
                let enabled = bound == "on";
                match genome_set_enabled(settings, enabled) {
                    // The refreshed view is what a later status reads, so the
                    // note is the file's own word, not the input echoed back.
                    Ok(saved) => self.push(
                        LineKind::Note,
                        if saved { "genome: on" } else { "genome: off" }.to_owned(),
                    ),
                    Err(why) => self.push(LineKind::Error, format!("genome: not saved ({why})")),
                };
            }
            "limit" => match rest.parse::<i64>() {
                // Out of range or not an integer is refused before anything
                // is written, so a typo cannot seed `genome.limit` with a
                // value every later run has to ignore.
                Ok(n) if (1..=64).contains(&n) => match genome_set_limit(settings, n) {
                    Ok(()) => self.push(LineKind::Note, format!("genome limit: {n}")),
                    Err(why) => self.push(LineKind::Error, format!("genome: not saved ({why})")),
                },
                _ => {
                    self.push(
                        LineKind::Error,
                        "genome limit: expected an integer from 1 to 64".to_owned(),
                    );
                }
            },
            "check" | "lsp" => {
                let workspace = crate::session_fs::current_workspace();
                local_genome_note(self, bound, &workspace)
            }
            name => {
                self.push(LineKind::Error, format!("genome: unknown command {name}"));
                self.push(
                    LineKind::Error,
                    "usage: titi genome [on|off|limit <n>|check|lsp]".to_owned(),
                );
            }
        }
        Applied::none()
    }
    fn fork(&mut self) -> Applied {
        self.session_note(crate::session_fs::fork_session(
            &self.agent_dir,
            &self.session_id,
        ))
    }

    fn export(&mut self, args: &str) -> Applied {
        let path = args.trim();
        self.session_note(crate::session_fs::export_session(
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
            &crate::session_fs::current_workspace(),
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
                &crate::session_fs::current_workspace(),
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

    pub(crate) fn rewind(&mut self, args: &str) -> Applied {
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
        match crate::session_fs::rewind_session(
            &self.agent_dir,
            &crate::session_fs::current_workspace(),
            &self.session_id,
            index,
        ) {
            Ok(summary) => {
                match crate::session_fs::session_history(&self.agent_dir, &self.session_id) {
                    Ok(messages) => {
                        self.show_history(&messages);
                        self.turn_active = false;
                        self.turn_started = None;
                        self.phase = WorkPhase::Waiting;
                        self.approval = None;
                        self.pending_ask = None;
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
                }
            }
            Err(reason) => {
                self.push(LineKind::Error, format!("rewind: {reason}"));
                Applied::none()
            }
        }
    }

    /// Puts the stored conversation of this chat's session on the screen —
    /// the same window the engine replays to the model at startup. A session
    /// with nothing stored, or one that cannot be read, leaves the screen as
    /// it is, so a fresh start still opens on the welcome.
    pub fn show_stored_history(&mut self) {
        if let Ok(messages) = crate::session_fs::session_history(&self.agent_dir, &self.session_id)
            && !messages.is_empty()
        {
            self.show_history(&messages);
        }
    }

    pub(crate) fn show_history(&mut self, messages: &[titi_providers::ChatMessage]) {
        self.lines.clear();
        self.assistant_at = None;
        self.thinking_at = None;
        self.reply.clear();
        self.shown_from = 0;
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

    /// `/hotkeys`: every key the screen answers, grouped, one row each. The
    /// table is `keys.rs`'s, beside the `on_key` that answers those keys, so
    /// the listing and the screen cannot be written twice (`hotkey_lines`).
    /// `/changelog`: what changed in this build, from the notes embedded in
    /// it ([`crate::changelog`]). Bare, the newest few; `full`, all of them;
    /// `last n`, n of them.
    fn changelog(&mut self, args: &str) -> Applied {
        match crate::changelog::parse_args(args) {
            Ok(view) => {
                for line in crate::changelog::render(view) {
                    self.push(LineKind::Note, line);
                }
            }
            Err(usage) => self.push(LineKind::Error, usage),
        }
        Applied::none()
    }

    fn hotkeys(&mut self) -> Applied {
        // The vim rows only while the mode is on: a mode the config did not
        // ask for must not read as a binding the screen answers.
        let vim = if self.vim.is_some() {
            crate::vim::VIM_HOTKEYS
        } else {
            &[]
        };
        for line in hotkey_lines(vim) {
            self.push(LineKind::Note, line);
        }
        Applied::none()
    }

    // -----------------------------------------------------------------------
    // Emoji shortcodes and emoticons
    // -----------------------------------------------------------------------

    /// Type `ch` into the composer, expanding the shortcode a closing `:`
    /// closes or the emoticon a terminating space ends. The table and the
    /// guards are `titi_tui::emoji`'s; this is the live composer's own buffer,
    /// so the expansion runs over it here.
    ///
    /// The token before the caret is the one that can expand — a shortcode
    /// written mid-sentence expands where the person is typing, not at the end
    /// of the draft.
    pub(crate) fn type_char(&mut self, ch: char) {
        let terminator = matches!(ch, ' ' | '\n' | '\r');
        let expansion = if terminator {
            titi_tui::emoji::try_expand_emoticon(&self.input[..self.caret()])
        } else if ch == ':' {
            titi_tui::emoji::try_expand_shortcode(&self.input[..self.caret()])
        } else {
            None
        };
        self.insert_at_caret(&ch.to_string());
        if let Some((start, glyph)) = expansion {
            // The token and the trigger just typed become the glyph. The
            // closing colon of a shortcode *is* the trigger, so it goes with
            // it; an emoticon's terminator is kept after the glyph, the way it
            // was typed.
            self.input.replace_range(start..self.caret(), glyph);
            self.caret = start + glyph.len();
            if terminator {
                self.insert_at_caret(&ch.to_string());
            }
        }
        self.sync_emoji_picker();
    }

    /// Expand an emoticon sitting just before the caret, for the Enter
    /// terminator: the space case is handled as the space is typed, and Enter
    /// does the same before the line is sent.
    pub(crate) fn expand_trailing_emoticon(&mut self) {
        if let Some((start, glyph)) =
            titi_tui::emoji::try_expand_emoticon(&self.input[..self.caret()])
        {
            self.input.replace_range(start..self.caret(), glyph);
            self.caret = start + glyph.len();
        }
    }

    /// `/sessions [query]`: bare is the same list Ctrl+X opens — the sessions
    /// this agent directory holds — and a query searches them.
    ///
    /// The search is the FTS index `SessionStore::search` exposes, which was
    /// built and populated on every append with nothing in the CLI reading it.
    /// The query narrows as it is typed, the way the model, theme and history
    /// browsers narrow theirs, and Enter takes the row the cursor is on
    /// through the same switch the Ctrl+X list uses.
    fn sessions(&mut self, args: &str) -> Applied {
        let query = args.trim();
        if query.is_empty() {
            return self.open_session_picker();
        }
        self.session_search = Some(SessionSearch::open(&self.agent_dir, query));
        Applied::none()
    }

    /// Moves the screen to `id`: its stored history replaces the transcript
    /// and the engine is told to replay it. The one path both the Ctrl+X list
    /// and a search hit take, so two ways into a session cannot drift.
    pub(crate) fn switch_to_session(&mut self, id: String) -> Applied {
        if id == self.session_id {
            return Applied::none();
        }
        match crate::session_fs::session_history(&self.agent_dir, &id) {
            Ok(messages) => {
                self.show_history(&messages);
                self.session_id = id.clone();
                self.session_label = stored_session_title(&self.agent_dir, &id);
                self.turn_active = false;
                self.turn_started = None;
                self.phase = WorkPhase::Waiting;
                self.approval = None;
                self.pending_ask = None;
                // The live agents belonged to the session being left: their
                // rows are that session's, and so is any focus on one of them.
                // The engine's own supervisor stops them; the screen forgets
                // them here so a switch cannot show a dead session's strip.
                self.agents.clear();
                self.agent_focus = None;
                self.push(LineKind::Note, format!("session {id}"));
                Applied::send(EngineCommand::RestoreHistory { messages }, None)
            }
            Err(reason) => {
                self.push(
                    LineKind::Error,
                    format!("session {id}: history not restored ({reason})"),
                );
                Applied::none()
            }
        }
    }

    /// `/theme [name]`: bare opens the picker, a name applies it directly.
    fn theme(&mut self, args: &str) -> Applied {
        let name = args.trim();
        if name.is_empty() {
            self.theme_picker = Some(ThemePicker::open());
            return Applied::none();
        }
        self.apply_theme(name)
    }

    /// Applies a palette and remembers it — `auto` to go back to the terminal
    /// probe's own pick.
    ///
    /// The name is checked before anything is written, so a typo is neither
    /// remembered nor painted; the write happens before the swap, so the screen
    /// never shows a theme the next run would not; and the swap is followed by
    /// a repaint the moment this returns, which is the frame after the key.
    pub(crate) fn apply_theme(&mut self, name: &str) -> Applied {
        let workspace = crate::session_fs::current_workspace();
        let inputs = titi_tui::theme::appearance::AppearanceInputs::from_env();
        let key = crate::themes::theme_slot(&inputs);
        let mut settings =
            match titi_config::settings::Settings::load(&self.agent_dir, &workspace, &[]) {
                Ok(settings) => settings,
                Err(reason) => {
                    self.push(LineKind::Error, format!("theme: {reason}"));
                    return Applied::none();
                }
            };
        let auto = name == THEME_AUTO;
        if !auto
            && !crate::themes::theme_names()
                .iter()
                .any(|known| known == name)
        {
            self.push(LineKind::Error, crate::themes::unknown_theme(name));
            return Applied::none();
        }
        let written = if auto {
            settings.reset(key)
        } else {
            settings.set(key, serde_json::json!(name))
        };
        if let Err(reason) = written {
            self.push(LineKind::Error, format!("theme: not saved ({reason})"));
            return Applied::none();
        }
        match crate::themes::theme_for(&self.agent_dir, &workspace, None) {
            Ok(theme) => {
                self.theme = theme;
                let shown = self.theme_state().name;
                let note = if auto {
                    format!("theme {shown} (auto)")
                } else {
                    format!("theme {shown}")
                };
                self.push(LineKind::Note, note);
                Applied::none()
            }
            Err(reason) => {
                self.push(LineKind::Error, reason);
                Applied::none()
            }
        }
    }

    /// `/statusline [preset]`: bare states the preset in force and lists them
    /// all; a name sets it and remembers it, exactly as `/theme` does.
    ///
    /// The name is checked before anything is written, so a typo is neither
    /// remembered nor painted; the write happens before the swap, so the screen
    /// never shows a preset the next run would not; and the swap is followed by
    /// a repaint the moment this returns, which is the frame after the key.
    fn statusline(&mut self, args: &str) -> Applied {
        let name = args.trim();
        if name.is_empty() {
            self.push(
                LineKind::Note,
                format!(
                    "status line: {} · context {}",
                    self.status_line.preset.id(),
                    self.status_line.context_line.id()
                ),
            );
            for id in StatusLinePreset::IDS {
                let about = StatusLinePreset::from_id(id)
                    .map(|preset| titi_tui::status_bar::preset(preset).about)
                    .unwrap_or_default();
                self.push(LineKind::Note, format!("/{id}  {about}"));
            }
            return Applied::none();
        }
        let Some(preset) = StatusLinePreset::from_id(name) else {
            self.push(
                LineKind::Error,
                format!(
                    "no status line preset \"{name}\"; try {}",
                    StatusLinePreset::IDS
                        .iter()
                        .map(|id| format!("/{id}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            return Applied::none();
        };
        let workspace = crate::session_fs::current_workspace();
        let mut settings =
            match titi_config::settings::Settings::load(&self.agent_dir, &workspace, &[]) {
                Ok(settings) => settings,
                Err(reason) => {
                    self.push(LineKind::Error, format!("status line: {reason}"));
                    return Applied::none();
                }
            };
        if let Err(reason) = settings.set(
            titi_config::settings::STATUS_LINE_PRESET_KEY,
            serde_json::json!(preset.id()),
        ) {
            self.push(
                LineKind::Error,
                format!("status line: not saved ({reason})"),
            );
            return Applied::none();
        }
        self.status_line.preset = preset;
        self.push(LineKind::Note, format!("status line {name}"));
        Applied::none()
    }

    /// Typing while a login prompt is up. In OAuth mode the line is the
    /// pasted code or redirect URL and Enter hands it to the flow; otherwise
    /// it is the API key and Enter stores it.
    pub(crate) fn login_key(&mut self, key: Key) -> Applied {
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
                self.insert_at_caret(&ch.to_string());
                Applied::none()
            }
            Key::Backspace => {
                self.backspace();
                Applied::none()
            }
            Key::Enter => self.store_login_secret(),
            _ => Applied::none(),
        }
    }

    /// Leaves the login prompt. Dropping the flow is the cancel: the task's
    /// code end sees the channel close and stops waiting.
    fn cancel_login(&mut self) -> Applied {
        self.clear_input();
        self.login_for = None;
        self.oauth = None;
        self.push(LineKind::Note, "login cancelled".to_owned());
        Applied::none()
    }

    fn store_login_secret(&mut self) -> Applied {
        let secret = std::mem::take(&mut self.input);
        self.caret = 0;
        self.pastes.clear();
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
    pub(crate) fn start_oauth_login(
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
        crate::engine::registry_config_for(&self.agent_dir, &crate::session_fs::current_workspace())
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
                (None, false) if !provider.credential_required => "no key needed".to_owned(),
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
        match run_git(&crate::session_fs::current_workspace(), argv) {
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
        let workspace = crate::session_fs::current_workspace();
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
                } else if !provider.credential_required {
                    "no key needed".to_owned()
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

    /// `/budget [amount|off]` caps what this session may spend: tokens
    /// (`/budget 200k`) or money (`/budget $2`).
    ///
    /// Both bounds are the engine's and they are independent: a token cap is
    /// tripped against `prompt + completion` (`runtime.rs`'s `budget`), a money
    /// cap against its cost ledger, which bills each round at that round's
    /// model price. `off` lifts both — one keystroke, two commands — because
    /// "no cap" is one intent and neither bound restates the other. A money cap
    /// over a model with no price is not accepted silently: the engine names
    /// the model (`MoneyBudgetUnpriced`) and the cap's spend becomes a floor.
    fn budget(&mut self, args: &str) -> Applied {
        let args = args.trim();
        if args.is_empty() {
            self.show_budget();
            return Applied::none();
        }
        if matches!(args, "off" | "none" | "clear") {
            self.push(LineKind::Note, "budget: no cap".to_owned());
            return Applied::effect(ChatEffect::SendAll(vec![
                EngineCommand::SetBudget { tokens: None },
                EngineCommand::SetMoneyBudget { micro_usd: None },
            ]));
        }
        match parse_budget(args) {
            Ok(Budget::Tokens(tokens)) => {
                self.push(LineKind::Note, format!("budget: {tokens} tokens"));
                Applied::send(
                    EngineCommand::SetBudget {
                        tokens: Some(tokens),
                    },
                    None,
                )
            }
            Ok(Budget::Money(micro)) => {
                self.push(LineKind::Note, format!("budget: {}", usd(micro)));
                Applied::send(
                    EngineCommand::SetMoneyBudget {
                        micro_usd: Some(micro),
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
        let tokens = match self.budget {
            Some(limit) => format!(
                "{} of {limit} tokens spent ({}%)",
                self.spent_tokens,
                share(self.spent_tokens, limit)
            ),
            None => format!("no token cap · {} tokens spent", self.spent_tokens),
        };
        // The money bound rides beside it, when the engine has reported one:
        // either a cap with what it measured against it, or what it measured
        // with no cap to measure against.
        let money = match self.money_budget_micro {
            Some(limit) => format!(
                " · {} of {} in money",
                usd(self.money_spent_micro),
                usd(limit)
            ),
            None if self.money_spent_micro > 0 => {
                format!(" · {} spent in money", usd(self.money_spent_micro))
            }
            None => String::new(),
        };
        self.push(LineKind::Note, format!("budget: {tokens}{money}"));
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

    /// `/details`: bare lists every section and the mode it is on; a section
    /// alone reports it; `"<section> <mode>"` moves one; a whole-word mode or
    /// `cycle` moves every section and the fold.
    ///
    /// The render reads the same state the next frame is drawn from, so the
    /// line answers with what the screen does rather than with what was asked.
    fn details(&mut self, args: &str) -> Applied {
        let directive = args.trim();
        if directive.is_empty() {
            self.push(LineKind::Note, self.details.list());
            return Applied::none();
        }
        match self.details.apply(directive) {
            Some(line) if line.is_empty() => Applied::none(),
            Some(line) => {
                self.push(LineKind::Note, line);
                Applied::none()
            }
            None => {
                self.push(
                    LineKind::Error,
                    format!(
                        "details: no such section or mode ({directive}) · \
                         try /details thinking|tools|subagents|activity|folded \
                         hidden|collapsed|expanded|cycle"
                    ),
                );
                Applied::none()
            }
        }
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

    /// Say one thing above the composer until the next key.
    pub(crate) fn set_hint(&mut self, text: String) {
        self.hint = text;
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
        // The turn's clock is read before it is dropped: the footer says how
        // long the turn took, and nothing else keeps that time.
        let elapsed = self.turn_started.map(|started| started.elapsed());
        self.reply.clear();
        self.recorded_reply = 0;
        self.shown_from = 0;
        self.turn_active = false;
        self.turn_started = None;
        // No phase outlives its turn: the next one starts in `Waiting`, and
        // an idle chat must not be left holding a tool's clock.
        self.phase = WorkPhase::Waiting;
        self.active_turn_id = None;
        self.approval = None;
        self.pending_ask = None;
        self.assistant_at = None;
        self.drop_thinking();
        // The turn's usage footer, under the last line of the turn. Only a
        // turn that reported usage has one: a turn cancelled before its first
        // round has nothing to say, and a row of zeros would say it wrong.
        if let (Some(elapsed), Some((prompt_tokens, cached_tokens, completion_tokens))) =
            (elapsed, self.turn_usage.take())
        {
            let footer = titi_tui::status::TurnFooter {
                elapsed,
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                // The engine reports no message count, so "the request carried
                // history" was read at the turn's start from what it had
                // already reported (`begin_usage_ledger`).
                cache_miss: cached_tokens == 0 && self.turn_history,
                // `None` for an unpriced model: the row states no money rather
                // than `$0.000`, which would read as free.
                cost_micro_usd: self.turn_cost_micro,
            };
            if let Some(row) = footer.row(self.turn_footer) {
                self.push(LineKind::Usage, row);
            }
        }
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
        let text = self
            .reply
            .get(self.shown_from..)
            .unwrap_or_default()
            .to_owned();
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
            kind: LineKind::Thinking,
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

    pub(crate) fn push(&mut self, kind: LineKind, text: String) {
        self.lines.push(TranscriptLine { kind, text });
        self.scroll_offset = 0;
    }

    /// How many leading transcript lines can no longer change: everything
    /// above the reply or the reasoning still streaming, which grow in place.
    /// A surface that writes each line once — the cast replay — waits for a
    /// line to settle before writing it.
    pub fn settled_len(&self) -> usize {
        [self.assistant_at, self.thinking_at]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.lines.len())
            .min(self.lines.len())
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
        if self.approval.is_some()
            || self.pending_ask.is_some()
            || self.quit_armed.is_some()
            || self.login_for.is_some()
        {
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
    mut session_log: Option<SessionLog>,
    catalog: crate::engine::ModelCatalog,
    session_id: String,
    mut cast: Option<crate::ompcast::CastWriter>,
    theme_name: Option<String>,
    startup_note: Option<String>,
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
    let theme = crate::themes::theme_for(
        &titi_config::agent_dir(),
        &crate::session_fs::current_workspace(),
        theme_name.as_deref(),
    )
    .map_err(io::Error::other)?;
    let mut chat = Chat::new(model, &session_id, theme);
    // Which of the terminal's own channels this run may use — the switches the
    // config sets over what the terminal says it supports — resolved before
    // the first frame, because a sequence sent to a terminal that does not
    // know it is rubbish in the byte stream. A config that fails to load
    // leaves every switch at its default (`on`), exactly as `/genome` reads it.
    let settings = titi_config::settings::Settings::load(
        &chat.agent_dir,
        &crate::session_fs::current_workspace(),
        &[],
    )
    .ok();
    let term_env = titi_tui::caps::TermEnv::from_env();
    chat.terminal = TerminalFeatures::resolve(settings.as_ref(), &term_env);
    chat.turn_footer = footer_switches(settings.as_ref());
    chat.paste_menu_after = settings
        .as_ref()
        .and_then(|settings| settings.paste_menu_threshold())
        .unwrap_or(PASTE_MENU_AFTER);
    chat.tree_filter = tree_filter(settings.as_ref());
    chat.tight = settings.as_ref().is_some_and(|settings| {
        titi_config::settings::switch_on(settings, titi_config::settings::TUI_TIGHT_KEY)
    });
    chat.pinned = pinned_agents(settings.as_ref());
    chat.smooth = settings.as_ref().is_some_and(|settings| {
        titi_config::settings::switch_on(
            settings,
            titi_config::settings::DISPLAY_SMOOTH_STREAMING_KEY,
        )
    });
    // The vim keys, off unless `editor.vim` asks for them: a switch that
    // changes what typing does is not turned on by a config that says nothing.
    chat.vim = settings
        .as_ref()
        .is_some_and(|settings| {
            titi_config::settings::switch_on(settings, titi_config::settings::EDITOR_VIM_KEY)
        })
        .then(crate::vim::VimState::default);
    // The status line's preset and gauge come from the same settings, resolved
    // before the first frame: an unknown or unset name is `default`/`off`, so a
    // typo in a cosmetic key changes nothing and never refuses to start.
    let preset = setting_string(
        settings.as_ref(),
        titi_config::settings::STATUS_LINE_PRESET_KEY,
    );
    let context_line = setting_string(
        settings.as_ref(),
        titi_config::settings::STATUS_LINE_CONTEXT_LINE_KEY,
    );
    // The three keys `StatusLineStyle` carries beyond the preset: the
    // separator (unset = the preset's own), the session accent and the
    // transparent background (unset = off, so an unset key leaves the frame
    // exactly as it was).
    let separator = setting_string(
        settings.as_ref(),
        titi_config::settings::STATUS_LINE_SEPARATOR_KEY,
    );
    let session_accent = settings.as_ref().is_some_and(|settings| {
        titi_config::settings::switch_on(
            settings,
            titi_config::settings::STATUS_LINE_SESSION_ACCENT_KEY,
        )
    });
    let transparent = settings.as_ref().is_some_and(|settings| {
        titi_config::settings::switch_on(
            settings,
            titi_config::settings::STATUS_LINE_TRANSPARENT_KEY,
        )
    });
    chat.status_line = StatusLineStyle::resolve(
        preset.as_deref(),
        context_line.as_deref(),
        separator.as_deref(),
        session_accent,
        transparent,
        chat.tight,
    );
    // A resumed session already has a name; the engine only announces one it
    // has just made, so read the one it has (the same index `/sessions` and the
    // switcher read) instead of showing no name for the whole run.
    chat.session_label = stored_session_title(&chat.agent_dir, &session_id);
    // The engine resumed this session's history; the screen shows the same.
    chat.show_stored_history();
    // `--continue` that found nothing: the screen says so rather than opening
    // on a welcome that reads as a resume which quietly did nothing.
    // What changed since the last run, once: the version this build carries
    // against the one the marker holds. A first run writes the marker and says
    // nothing — nothing has changed *for* a user who has never run this.
    let seen = crate::changelog::last_seen(&chat.agent_dir);
    if settings.as_ref().is_none_or(|settings| {
        !titi_config::settings::switch_off(settings, titi_config::settings::STARTUP_CHANGELOG_KEY)
    }) {
        if let Some(line) = crate::changelog::notice(seen.as_deref(), titi_tui::VERSION) {
            chat.push(LineKind::Note, line);
        }
        if seen.as_deref() != Some(titi_tui::VERSION) {
            crate::changelog::remember(&chat.agent_dir, titi_tui::VERSION);
        }
    }
    if let Some(note) = startup_note {
        chat.push(LineKind::Note, note);
    }
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
    // The terminal's appearance decided the palette above, so the screen
    // already matches it: the first probe reply that names the same appearance
    // changes nothing. An explicit `--theme` is the user's own choice and is
    // not the probe's to move.
    chat.set_starting_appearance(appearance::detect_terminal_background(
        &AppearanceInputs::from_env(),
    ));
    chat.set_appearance_auto(theme_name.is_none());
    // The mouse preset the user persisted, or drag-select on a machine that
    // has never chosen one: the transcript's selection needs button *and* drag
    // reporting, which is `Buttons`.
    chat.mouse_preset =
        crate::session_fs::load_mouse_preset_from(&chat.agent_dir).unwrap_or(MousePreset::Buttons);
    let mut screen = Screen::open(chat.terminal.progress, chat.mouse_preset)?;
    chat.start_intro(Instant::now());
    let result = loop {
        // The rate the working row shows is sampled from the character counts
        // that row already keeps, once per tick rather than once per frame, so
        // the number it prints is the row's own text seen over time.
        chat.sample_rate(Instant::now());
        let state = chat.agent_state();
        if state != reported
            && let Some(reporter) = &herdr_reporter
        {
            reporter.report(state, None);
            reported = state;
        }
        // The reveal's own frame, before the draw that shows it: the same tick
        // the spinner and the progress row ride.
        chat.reveal_tick(Instant::now());
        screen.terminal.draw(|frame| draw(frame, &mut chat))?;
        let flush = chat.take_output_flush();
        if !flush.is_empty() {
            let backend = screen.terminal.backend_mut();
            backend.write_all(flush.as_bytes())?;
            backend.flush()?;
            screen.terminal.draw(|frame| draw(frame, &mut chat))?;
        }
        if pump(&mut engine, &mut chat, &session_log, &mut cast)? {
            break Ok(());
        }
        // The tab title, the terminal's own progress bar and any notification
        // ride the same tick as the progress row: each is written only when
        // the run state changed, so a tick in an unchanged state writes
        // nothing (crate::title).
        if let Some(sequence) = chat.terminal_tick() {
            let backend = screen.terminal.backend_mut();
            backend.write_all(sequence.as_bytes())?;
            backend.flush()?;
        }
        // The screen can end up on another session mid-run (the switcher does),
        // and the file this run appends to has to move with it: a transcript
        // written to the session the user left is a conversation that is lost
        // when it is resumed.
        session_log = session_log_for(session_log, &chat);
    };
    if let Some(reporter) = &herdr_reporter {
        reporter.report(AgentState::Idle, None);
    }
    result
}

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Whether the run raised the terminal's own progress bar, so the way out
    /// clears it even when the loop never ticked after the turn.
    progress: bool,
}

impl Screen {
    /// Take the terminal over: raw mode, the alternate screen, the cursor's
    /// hiding, and the mouse preset the run was given.
    ///
    /// Mouse reporting is asked for in the crate's own vocabulary
    /// ([`MousePreset`]) rather than through crossterm's blanket capture, so
    /// the preset a user persisted with `/mouse` is the one the terminal gets.
    fn open(progress: bool, mouse: MousePreset) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            ratatui::crossterm::cursor::Hide,
            EnableFocusChange,
            // Without this the terminal never marks a paste: a wall of text
            // arrives as keystrokes, and every line break in it is an Enter
            // that sends the half-typed prompt. With it, the whole paste
            // arrives as one `Event::Paste` and `Chat::paste` can collapse it.
            EnableBracketedPaste
        )?;
        stdout.write_all(mouse.enable().as_bytes())?;
        stdout.flush()?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal, progress })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            ratatui::crossterm::cursor::Show,
            LeaveAlternateScreen,
            DisableFocusChange,
            DisableBracketedPaste
        );
        // The terminal's own channels go back with the screen: mouse reporting
        // is turned off for every mode (`Off`'s sequence is the disable for all
        // four, so an exit cannot leave the terminal reporting drags to a
        // program that is gone), an empty OSC 2 hands the title to the shell,
        // and the OSC 9;4 clear takes the progress bar down, so a Ctrl+C
        // mid-turn cannot leave a bar running in a tab whose agent is gone.
        let backend = self.terminal.backend_mut();
        let _ = backend.write_all(MousePreset::Off.enable().as_bytes());
        let _ = backend.write_all(crate::title::restore(self.progress).as_bytes());
        let _ = backend.flush();
    }
}

/// A skill the composer can complete, mirroring what the engine discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRow {
    pub(crate) name: String,
    pub(crate) about: String,
}

/// Discovery lives in the engine, so the picker offers exactly the names a
/// `/name` reference can expand.
fn discovered_skills(agent_dir: &Path) -> Vec<SkillRow> {
    titi_engine::skills::catalog(
        Some(&crate::session_fs::current_workspace()),
        Some(agent_dir),
    )
    .into_iter()
    .map(|skill| SkillRow {
        name: skill.name,
        about: skill.description,
    })
    .collect()
}

/// A way to sign in to a subscription provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginMethod {
    /// Authorization code over the loopback callback, with the manual paste
    /// as the fallback.
    Browser,
    /// The device grant: a code entered at a URL, on this machine or another.
    Device,
}

/// What credential a provider has in reach, without its value.
///
/// `/keys`, `/diagnose` and the model picker all ask the same question — is
/// there a token, a key or a variable behind this provider — so they read it
/// here once, in one order: the environment first, the auth store second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credential {
    from_env: bool,
    stored: Option<crate::secrets::StoredKey>,
}

impl Credential {
    pub(crate) fn of(
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
    pub(crate) fn label(&self) -> Option<&str> {
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

/// The `/token` the caret is in, when it opens with a slash at the start of
/// the line or after whitespace and holds nothing but name characters. That is
/// what keeps `/tmp/photo.png` and `a/b` out.
pub(crate) struct SlashToken<'a> {
    /// Where the token opens: the slash.
    pub(crate) start: usize,
    /// Where it ends: the next whitespace, or the end of the draft.
    pub(crate) end: usize,
    /// The whole word after the slash — what accepting the token replaces.
    pub(crate) name: &'a str,
    /// What the person has typed of it: between the slash and the caret, which
    /// is what a list filters by.
    pub(crate) prefix: &'a str,
}

pub(crate) fn slash_token(input: &str, caret: usize) -> Option<SlashToken<'_>> {
    let caret = caret.min(input.len());
    let start = input[..caret].rfind('/')?;
    if start > 0 && !input[..start].ends_with(char::is_whitespace) {
        return None;
    }
    let end = input[caret..]
        .find(char::is_whitespace)
        .map(|at| caret + at)
        .unwrap_or(input.len());
    let name = &input[start + 1..end];
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some(SlashToken {
        start,
        end,
        name,
        prefix: &input[start + 1..caret],
    })
}

/// The byte offset `count` characters back from `at`, clamped to the start.
fn byte_offset_back(text: &str, at: usize, count: usize) -> usize {
    let mut offset = at;
    let mut left = count;
    while left > 0 {
        match text[..offset].char_indices().next_back() {
            Some((at, _)) => {
                offset = at;
                left -= 1;
            }
            None => return 0,
        }
    }
    offset
}

/// The byte offset `count` characters forward from `at`, clamped to the end.
fn byte_offset_forward(text: &str, at: usize, count: usize) -> usize {
    let mut offset = at;
    let mut left = count;
    while left > 0 {
        match text[offset..].chars().next() {
            Some(ch) => {
                offset += ch.len_utf8();
                left -= 1;
            }
            None => return text.len(),
        }
    }
    offset
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
    /// A figure that is not a cap in either unit.
    Unreadable(String),
    /// A money figure finer than a micro-dollar — the unit a cap is kept in,
    /// so it cannot be rounded away without capping a number nobody wrote.
    Finer(String),
    Zero,
}

impl std::fmt::Display for BudgetArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(word) => write!(
                f,
                "budget: {word} is neither tokens (200000, 200k, 1.5m) nor money ($2, $0.50)"
            ),
            Self::Finer(word) => write!(
                f,
                "budget: {word} is finer than a micro-dollar, the unit a money cap is kept in"
            ),
            Self::Zero => {
                f.write_str("budget: the cap must be more than zero; /budget off lifts it instead")
            }
        }
    }
}

/// `200000`, `200k`, `1.5m` as tokens.
/// A cap read off the command line, in the unit it was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budget {
    /// Tokens: `/budget 200k`, `/budget 500`.
    Tokens(u64),
    /// Micro-dollars, a millionth of a dollar: `/budget $2`, `/budget $0.50`.
    Money(u64),
}

/// Reads a cap in tokens or in money.
///
/// Money is parsed digit by digit into micro-dollars, never through a float:
/// `$2` is exactly 2 000 000 and `$0.50` exactly 500 000, and a figure finer
/// than a micro-dollar — or one carrying a sign — is refused rather than
/// rounded into a cap nobody wrote. The token form keeps its own suffixes
/// (`k`, `m`) and its own rounding.
fn parse_budget(word: &str) -> Result<Budget, BudgetArgError> {
    if let Some(money) = word.strip_prefix('$') {
        return parse_money(money).map(Budget::Money);
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
    Ok(Budget::Tokens(tokens))
}

/// The digits after a `$`, as micro-dollars: `2`, `0.50`, `0.000001`.
///
/// Six decimal places is the whole of it — a micro-dollar is the unit the
/// engine's ledger keeps and a price is stated in — so a seventh is refused
/// rather than dropped.
fn parse_money(word: &str) -> Result<u64, BudgetArgError> {
    let unreadable = || BudgetArgError::Unreadable(format!("${word}"));
    let (whole, fraction) = match word.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (word, ""),
    };
    // A sign is not part of an amount: `$-2` is a typo, not a negative cap.
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(unreadable());
    }
    if whole.is_empty() && fraction.is_empty() {
        return Err(unreadable());
    }
    if fraction.len() > 6 {
        return Err(BudgetArgError::Finer(format!("${word}")));
    }
    let units: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| unreadable())?
    };
    let part: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| unreadable())?
    };
    let micro = units
        .checked_mul(1_000_000)
        .and_then(|whole| whole.checked_add(part * 10u64.pow(6 - fraction.len() as u32)))
        .ok_or_else(unreadable)?;
    if micro == 0 {
        return Err(BudgetArgError::Zero);
    }
    Ok(micro)
}

/// A money figure as the screen states it: `$0.38`, `$2.00`.
///
/// Two decimals is the session's own precision (`SESSION_COST_DECIMALS`);
/// `format_usd` takes more only to keep a real fraction of a cent from reading
/// as `$0.00`, which the `/budget` cap wants as much as the footer does.
fn usd(micro_usd: u64) -> String {
    titi_tui::status::format_usd(micro_usd, titi_tui::status::SESSION_COST_DECIMALS)
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

/// Writes `genome.enabled` on the chat's own view: the canonical agent file,
/// never the project's `.titi`, which this API cannot write.
fn genome_set_enabled(
    settings: Option<titi_config::settings::Settings>,
    enabled: bool,
) -> Result<bool, String> {
    let mut settings = settings.ok_or_else(|| "config could not be loaded".to_owned())?;
    settings
        .set(
            titi_config::settings::GENOME_ENABLED_KEY,
            serde_json::json!(enabled),
        )
        .map_err(|why| why.to_string())?;
    Ok(enabled)
}

/// Writes `genome.limit` after the range check, on the same canonical file.
fn genome_set_limit(
    settings: Option<titi_config::settings::Settings>,
    limit: i64,
) -> Result<(), String> {
    let mut settings = settings.ok_or_else(|| "config could not be loaded".to_owned())?;
    settings
        .set(
            titi_config::settings::GENOME_LIMIT_KEY,
            serde_json::json!(limit),
        )
        .map_err(|why| why.to_string())
}

/// `/genome check` and `/genome lsp`.
///
/// Check runs the same index + diagnostics pass the terminal verb runs, over
/// the workspace passed in — the live caller passes
/// [`crate::session_fs::current_workspace`] — and pushes the same `path:line: code:
/// message` lines — or one error line when the index itself fails. Lsp never
/// starts a stdio server inside the chat: the pipe is the terminal's, so the
/// note names the command instead.
fn local_genome_note(chat: &mut Chat, verb: &str, workspace: &Path) {
    if verb == "lsp" {
        chat.push(
            LineKind::Note,
            "genome: lsp is 'titi genome lsp', not a chat command".to_owned(),
        );
        return;
    }
    let genome = match titi_genome::Genome::index(workspace) {
        Ok(genome) => genome,
        Err(reason) => {
            chat.push(LineKind::Error, format!("genome: check failed ({reason})"));
            return;
        }
    };
    let diagnostics = genome.check();
    if diagnostics.is_empty() {
        chat.push(LineKind::Note, "genome: clean".to_owned());
        return;
    }
    for diagnostic in &diagnostics {
        chat.push(
            LineKind::Note,
            format!(
                "{}:{}: {}: {}",
                diagnostic.path, diagnostic.line, diagnostic.code, diagnostic.message
            ),
        );
    }
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
    let pinned_h = pinned_height(chat, area.height);
    let cols = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(roster_h),
        Constraint::Min(1),
        Constraint::Length(picker_h),
        // Zero while no agent is live, so an idle screen keeps every row it had
        // before the strip existed.
        Constraint::Length(pinned_h),
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
    // A focused agent's pane replaces the transcript body: its own text, which
    // the engine streamed as `AgentProgress` and which never entered the
    // parent's transcript.
    let (body, photos, links) = match chat.focused_agent() {
        Some(agent) => (
            agent_pane(agent, cols[2].width, cols[2].height, &theme),
            Vec::new(),
            Vec::new(),
        ),
        None if chat.lines.is_empty() => (
            empty_state(chat, cols[2].width, cols[2].height, &theme),
            Vec::new(),
            Vec::new(),
        ),
        None => transcript(chat, cols[2].width, cols[2].height, &theme),
    };
    chat.transcript_top = cols[2].y;
    frame.render_widget(body, cols[2]);
    paint_selection(frame, chat, cols[2], &theme);
    paint_photos(frame, cols[2], &photos, &theme);
    paint_links(frame, cols[2], &links);
    if let Some(view) = &panel {
        frame.render_widget(panel_box(view, cols[3].width, chat.tight, &theme), cols[3]);
    }
    // The strip draws here and remembers its own rows, so a click on one can
    // name the agent it landed on.
    chat.pinned_top = cols[4].y;
    chat.pinned_rows = pinned_h;
    if pinned_h > 0 {
        frame.render_widget(pinned(chat, &theme), cols[4]);
    }
    if let Some(status) = status {
        frame.render_widget(status, cols[5]);
    }
    frame.render_widget(composer(chat, cols[6].width, &theme), cols[6]);
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

/// How many rows the strip draws: one per visible agent, plus the `… N more`
/// line when the mode collapsed the rest away.
fn pinned_rows_for(chat: &Chat) -> (usize, usize) {
    if !chat.pinned.mode.shows_rows() || chat.agents.is_empty() {
        return (0, 0);
    }
    let shown = match chat.pinned.mode {
        PinnedAgents::Off => 0,
        PinnedAgents::Collapsed => chat.agents.len().min(PINNED_ROWS),
        PinnedAgents::Full => chat.agents.len(),
    };
    (shown, chat.agents.len() - shown)
}

/// Rows the pinned strip takes: its agents, its `… N more` line when there is
/// one, and nothing at all when no agent is live — which is what keeps an idle
/// frame exactly the frame it was.
fn pinned_height(chat: &Chat, total: u16) -> u16 {
    let (shown, hidden) = pinned_rows_for(chat);
    if shown == 0 {
        return 0;
    }
    let rows = shown + usize::from(hidden > 0);
    // A crowded strip cannot take the conversation's room: the same third the
    // hub roster is capped to.
    (rows as u16).min((total / 3).max(2))
}

/// The glyph a row leads with: the spinner for a running agent, and a mark for
/// the states that are not moving.
///
/// A running agent's glyph moves with the same clock the working row's does, so
/// a strip of live agents reads as alive rather than as a frozen list.
pub(crate) fn agent_glyph(status: titi_engine::AgentStatus, elapsed: Duration) -> &'static str {
    match status {
        titi_engine::AgentStatus::Running => spinner_frame(elapsed),
        titi_engine::AgentStatus::Idle => "·",
        titi_engine::AgentStatus::Parked => "‖",
        titi_engine::AgentStatus::Aborted | titi_engine::AgentStatus::Failed => "✗",
        titi_engine::AgentStatus::Completed => "✓",
    }
}

/// The row colour of a state: the accent while it runs, dim when it waits, the
/// two outcome tokens when it ended.
fn agent_color(status: titi_engine::AgentStatus) -> ThemeColor {
    match status {
        titi_engine::AgentStatus::Running => ThemeColor::Accent,
        titi_engine::AgentStatus::Failed | titi_engine::AgentStatus::Aborted => ThemeColor::Warning,
        titi_engine::AgentStatus::Completed => ThemeColor::Success,
        titi_engine::AgentStatus::Idle | titi_engine::AgentStatus::Parked => ThemeColor::Dim,
    }
}

/// The live agents, pinned above the composer: a jump list that says who is
/// running without opening anything.
///
/// The cursor wall is the same one the hub roster uses, and the strip never
/// takes more than a third of the screen: a wave of thirty-two agents shows its
/// head and counts the rest rather than pushing the conversation off.
fn pinned(chat: &Chat, theme: &Theme) -> Paragraph<'static> {
    let (shown, hidden) = pinned_rows_for(chat);
    let mut lines = Vec::new();
    for agent in chat.agents.iter().take(shown) {
        let elapsed = Instant::now().saturating_duration_since(agent.since);
        let focused = chat.agent_focus.as_deref() == Some(agent.id.as_str());
        let mut row = vec![Span::styled(
            format!("{} ", if focused { "▸" } else { " " }),
            fg(theme, ThemeColor::Accent),
        )];
        row.push(Span::styled(
            format!("{} ", agent_glyph(agent.status, elapsed)),
            fg(theme, agent_color(agent.status)),
        ));
        row.push(Span::styled(
            agent.name.clone(),
            fg(theme, ThemeColor::Text),
        ));
        if chat.pinned.preview
            && let Some(preview) = agent.preview()
        {
            row.push(Span::styled(
                format!(" · {preview}"),
                fg(theme, ThemeColor::Dim),
            ));
        }
        lines.push(Line::from(row));
    }
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("  … {hidden} more"),
            fg(theme, ThemeColor::Dim),
        )));
    }
    Paragraph::new(lines).style(page(theme))
}

/// The pane a focused agent has: its header, what it is doing, and the answer
/// it has streamed so far.
///
/// This is the agent's text as the engine sent it — `AgentProgress` goes here
/// and never into the parent's transcript, so a subagent's answer cannot be
/// mistaken for the model's. The body follows the tail: the last rows of a long
/// answer are what a reader watching it wants.
fn agent_pane(agent: &PinnedAgent, width: u16, height: u16, theme: &Theme) -> Paragraph<'static> {
    let elapsed = Instant::now().saturating_duration_since(agent.since);
    let status = match agent.status {
        titi_engine::AgentStatus::Running => "running".to_owned(),
        other => format!("{other:?}").to_lowercase(),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", agent_glyph(agent.status, elapsed)),
                fg(theme, agent_color(agent.status)),
            ),
            Span::styled(
                format!(
                    "agent {} · {status} · {:.1}s",
                    agent.name,
                    elapsed.as_secs_f64()
                ),
                fg(theme, ThemeColor::Accent),
            ),
        ]),
        Line::from(Span::styled(
            match agent.preview() {
                Some(activity) => format!("  {activity}"),
                None => "  (nothing yet)".to_owned(),
            },
            fg(theme, ThemeColor::Dim),
        )),
        Line::from(""),
    ];
    let room = (width as usize).saturating_sub(4).max(8);
    let answer = agent.answer.trim();
    if answer.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting for its first words",
            fg(theme, ThemeColor::Dim),
        )));
    } else {
        let rows = wrap_plain(answer, room);
        // The tail: the header takes three rows, so a long answer keeps its
        // last ones on screen rather than its first.
        let keep = (height as usize).saturating_sub(lines.len() + 1).max(1);
        for row in rows.iter().skip(rows.len().saturating_sub(keep)) {
            lines.push(Line::from(Span::styled(
                row.clone(),
                fg(theme, ThemeColor::Text),
            )));
        }
    }
    Paragraph::new(lines).style(page(theme))
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
//   dim                the ready state, a composer caption, a note
//   border             the composer's frame while idle
//   statusLineBg       the screen behind everything
//   customMessageBg    the composer's own surface, the one raised surface the
//                      theme has to spare; `userMessageBg` belongs to the
//                      user's block
//
// The welcome is the one exception: it is black and white on every palette,
// in grays measured off `statusLineBg` (`WelcomeGrays`).

/// One theme colour token as a ratatui style. The theme resolves a token to
/// CSS hex, so this is the only place a token becomes a terminal colour.
pub(crate) fn fg(theme: &Theme, token: ThemeColor) -> Style {
    Style::default().fg(rgb(&theme.get_color_hex(token)))
}

/// One theme background token as a ratatui colour.
pub(crate) fn bg(theme: &Theme, token: ThemeBg) -> Color {
    rgb(&theme.get_bg_hex(token))
}

/// The screen behind everything: the theme's chrome surface with its body text
/// on it. The status-line background is the one surface token that stands for
/// the whole screen, and it is darker than every other one in the presets.
pub(crate) fn page(theme: &Theme) -> Style {
    Style::default()
        .bg(bg(theme, ThemeBg::StatusLineBg))
        .fg(rgb(&theme.get_color_hex(ThemeColor::Text)))
}

/// The composer's own surface: the theme's raised panel colour with body text
/// on it. `userMessageBg` belongs to the user's own block and is not spent
/// here.
pub(crate) fn surface(theme: &Theme) -> Style {
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

/// The word the masthead shows for the session's state.
fn state_word(chat: &Chat) -> &'static str {
    if chat.approval.is_some() || chat.pending_ask.is_some() {
        "needs you"
    } else if chat.login_for.is_some() {
        "sign in"
    } else if chat.paused {
        "paused"
    } else if chat.turn_active {
        "working"
    } else {
        "ready"
    }
}

/// The colour of the state word — and of the mode and loop count beside it,
/// which belong to the same cluster of "what this session is doing".
fn state_color(chat: &Chat) -> ThemeColor {
    if chat.approval.is_some()
        || chat.pending_ask.is_some()
        || chat.paused
        || chat.login_for.is_some()
    {
        ThemeColor::Warning
    } else if chat.turn_active {
        ThemeColor::Accent
    } else {
        ThemeColor::Dim
    }
}

/// The masthead: the brand and the session's state, then the working directory
/// and its git state on the left, and the session's name, the model and the
/// context slot on the right.
fn masthead(chat: &Chat, width: u16, theme: &Theme) -> Paragraph<'static> {
    let snapshot = masthead_snapshot(chat);
    let line = Line::from(masthead_spans(chat, width, theme, &snapshot));
    // `statusLine.transparent` leaves this row's background to the terminal.
    // The screen already painted `StatusLineBg` behind every cell, so the row
    // has to *clear* it (`Color::Reset`, which overrides what is under it)
    // rather than simply not set one.
    let style = if chat.status_line.transparent {
        page(theme).bg(Color::Reset)
    } else {
        page(theme)
    };
    Paragraph::new(line).style(style)
}

/// One string out of the settings, when the layer that set it holds a string.
///
/// A number, a list or a mapping at a key that wants a name is not a name, so
/// the caller falls back exactly as it does for an unset key.
fn setting_string(settings: Option<&titi_config::settings::Settings>, key: &str) -> Option<String> {
    settings?
        .get(key)
        .and_then(|value| value.as_str().map(str::to_owned))
}

/// The masthead's facts in the crate's own status-line shape, so the working
/// directory and the git state come from the reader the other surface paints
/// rather than from a second one here.
///
/// `live_snapshot` is that reader. Its git call is cached on the mtimes of
/// `.git/HEAD` and `.git/index`, so a frame that changes nothing costs two
/// `stat`s instead of a `git` process.
pub(crate) fn masthead_snapshot(chat: &Chat) -> StatusSnapshot {
    let mut snapshot = live_snapshot(&chat.model, chat.session_label.trim());
    // The mark and the state word open the line as one cluster: the word is the
    // session's state, in the colour of what it is doing, and the preset table
    // paints the pair only when its row names them.
    snapshot.brand = Some("titi".to_owned());
    snapshot.state = Some(state_word(chat).to_owned());
    snapshot.state_color = state_color(chat);
    // The badge is the engine's mode, not a local toggle: it says what the
    // next turn may actually do.
    snapshot.mode = match chat.mode {
        SessionMode::Agent => None,
        other => Some(other.label().to_owned()),
    };
    // A loop running unseen is the whole problem, which is why this sits beside
    // the state word; `None` hides the segment when there is nothing running.
    snapshot.loops = (!chat.jobs.is_empty()).then_some(chat.jobs.len());
    snapshot.context_pct = chat.context_percent;
    // The window is what the gauge needs and what only a turn reports; the
    // totals are what the `full` preset prints.
    snapshot.context_window = chat.context_window;
    snapshot.tokens = (chat.session_prompt_tokens > 0 || chat.session_completion_tokens > 0)
        .then_some((chat.session_prompt_tokens, chat.session_completion_tokens));
    snapshot
}

/// The masthead's spans at `width`, measured off `snapshot`.
///
/// Split from [`masthead`] so a test can measure the layout: this machine's
/// working directory and git state must not decide what a test sees.
///
/// The line itself is the crate's status line, painted from the preset table
/// (`titi_tui::status_bar::PRESETS`) in the chat's own style. That table owns
/// the segment set, the separator and the order the segments are shed in when
/// the pane is too narrow, so a preset is a row there — with `default` the
/// row that reproduces this line exactly — and never a branch here. What the
/// line gives up, in order, is the git state (the most cells for the least
/// actionable fact), the directory, the session's name, the mode, the loop
/// count, and last the model's provider prefix, then the model itself, cut
/// with an ellipsis so a cut can never be read as a whole id.
fn masthead_spans(
    chat: &Chat,
    width: u16,
    theme: &Theme,
    snapshot: &StatusSnapshot,
) -> Vec<Span<'static>> {
    sgr_row(&titi_tui::status_bar::render_status_line(
        theme,
        width,
        chat.status_line,
        snapshot,
    ))
}

/// The live status row, drawn on the single line between the conversation and
/// the composer box, or `None` when there is nothing to report.
///
/// `None` is what makes an idle screen identical to a screen from before this
/// row existed: the caller gives it no height, so it cannot even leave a blank
/// line behind.
///
/// The `~N tok/s` a streaming phase may carry is an **estimate**: it is the
/// characters this row already counts — the answer's, or the reasoning's —
/// divided by four, because the provider's real token counts arrive only with
/// the turn's usage report, after the number is needed. The `~` is the row's
/// own mark for that; a phase with nothing streamed yet shows no number at
/// all. See [`titi_tui::status::TokenRate`].
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
        let (head, argument) = match pending.subject().split_once(' ') {
            Some((head, rest)) => (head, Some(rest.to_owned())),
            None => (pending.subject(), None),
        };
        return Some(
            Paragraph::new(work_line(
                "⚠",
                &WorkFact {
                    wording: format!("needs you · {head}"),
                    argument,
                    compact: "needs you".to_owned(),
                    seconds: None,
                },
                ThemeColor::Warning,
                width,
                theme,
            ))
            .style(page(theme)),
        );
    }
    let elapsed = chat.turn_elapsed().unwrap_or_default();
    // The generation rate stands next to the phase word, where "how fast"
    // belongs. It is the last reading the estimator earned, so a row between
    // two bursts keeps its number instead of blinking out; before the turn's
    // first delta there is no reading and the wording is exactly what it was.
    let rate = chat.rate_segment();
    let streaming_fact = |phase: &str, chars: usize| match &rate {
        Some(rate) => format!("{phase} · {rate} · {chars} chars"),
        None => format!("{phase} · {chars} chars"),
    };
    let (glyph, fact, color) = match &chat.phase {
        // The three states are told apart by colour as well as by glyph: the
        // activity spinner in the accent the rest of the screen uses for a
        // running turn, a tool in the theme's own token for a tool's title,
        // and an approval in the warning token.
        WorkPhase::Waiting => (
            spinner_frame(elapsed),
            WorkFact {
                // Long on purpose — it says what is being waited for — and the
                // row shortens it itself when the pane is narrow.
                wording: "waiting for the first token".to_owned(),
                argument: None,
                compact: "waiting".to_owned(),
                seconds: Some(elapsed_label(elapsed)),
            },
            ThemeColor::Accent,
        ),
        WorkPhase::Streaming => (
            spinner_frame(elapsed),
            WorkFact {
                wording: streaming_fact("streaming", chat.reply.chars().count()),
                argument: None,
                compact: "streaming".to_owned(),
                seconds: Some(elapsed_label(elapsed)),
            },
            ThemeColor::Accent,
        ),
        WorkPhase::Thinking => (
            spinner_frame(elapsed),
            WorkFact {
                wording: streaming_fact("thinking", chat.thinking.chars().count()),
                argument: None,
                compact: "thinking".to_owned(),
                seconds: Some(elapsed_label(elapsed)),
            },
            ThemeColor::Accent,
        ),
        WorkPhase::Tool {
            name,
            detail,
            since,
            ..
        } => (
            "⚙",
            tool_fact(name, detail.as_deref(), &elapsed_label(since.elapsed())),
            // `toolOutput`, not `toolTitle`: five of the shipped themes set
            // `toolTitle` to the same value as `accent` or `warning`, and only
            // one sets `toolOutput` to either — the row has to read as a third
            // state whatever palette is loaded.
            ThemeColor::ToolOutput,
        ),
    };
    Some(Paragraph::new(work_line(glyph, &fact, color, width, theme)).style(page(theme)))
}

/// One fact of the status row, in the forms the pane can shorten it to.
struct WorkFact {
    /// What the row says in full: `waiting for the first token`,
    /// `streaming · 335 chars`, `needs you · write`, a tool's name.
    wording: String,
    /// The rest of a tool's own sentence — the argument of the call
    /// (`docs/README.md`), shown after the wording with a space. It is the
    /// first thing to go: the tool's name still says what kind of call it is.
    argument: Option<String>,
    /// The wording the row falls back to before it cuts anything: short enough
    /// to fit almost any pane.
    compact: String,
    /// The elapsed time, or nothing for a row with no clock.
    seconds: Option<String>,
}

/// A running tool's fact: what the tool said about the call it is making
/// (`read docs/README.md`), split into the tool's name and the rest.
///
/// The split is the trait's own convention (`ToolHandler::describe` starts with
/// the tool's name), so the row can shorten the *argument* — the thing that
/// tells two `read` calls apart — without inventing a second description.
fn tool_fact(name: &str, described: Option<&str>, seconds: &str) -> WorkFact {
    let (wording, argument) = match described.and_then(|text| text.split_once(' ')) {
        Some((head, rest)) => (head.to_owned(), Some(rest.to_owned())),
        None => (described.unwrap_or(name).to_owned(), None),
    };
    WorkFact {
        wording,
        argument,
        compact: name.to_owned(),
        seconds: Some(seconds.to_owned()),
    }
}

/// One status row: the activity glyph, one fact, and the elapsed seconds.
///
/// The fact is shortened before it is cut, so a narrow pane loses words rather
/// than whole facts. In order:
///
/// 1. the wording with the argument (`read docs/README.md · 0.4s`)
/// 2. the argument shortened to its head (`read docs/… · 0.4s`)
/// 3. the wording without the argument (`read · 0.4s`)
/// 4. the compact wording (`waiting · 0.1s`)
/// 5. the compact wording cut with an ellipsis — and the seconds kept whole,
///    because the moving clock is what says the screen is alive.
///
/// A row that does not fit is cut here rather than wrapped: a wrapped line
/// would push the composer down and read as part of the conversation.
fn work_line(
    glyph: &str,
    fact: &WorkFact,
    color: ThemeColor,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let head = format!(" {glyph} ");
    let room = (width as usize)
        .saturating_sub(titi_tui::width::visible_width(&head))
        .max(1);
    let with_seconds = |wording: &str| match fact.seconds.as_deref() {
        Some(seconds) if !seconds.is_empty() => format!("{wording} · {seconds}"),
        _ => wording.to_owned(),
    };
    let mut forms = Vec::new();
    match &fact.argument {
        Some(argument) => {
            // The tool's own sentence, with the seconds as its own clause.
            forms.push(with_seconds(&format!("{} {argument}", fact.wording)));
            forms.push(with_seconds(&format!(
                "{} {}",
                fact.wording,
                short_argument(argument)
            )));
        }
        None => forms.push(with_seconds(&fact.wording)),
    }
    forms.push(with_seconds(&fact.wording));
    forms.push(with_seconds(&fact.compact));
    let fact = forms
        .into_iter()
        .find(|form| titi_tui::width::visible_width(form) <= room)
        .unwrap_or_else(|| cut_keeping_seconds(fact, room));
    Line::from(Span::styled(format!("{head}{fact}"), fg(theme, color)))
}

/// The shortened argument: the leading segment of a path, or the first word of
/// a command — `docs/README.md` becomes `docs/…`, `cargo test -p titi-core`
/// becomes `cargo …`. Either way it still says *what kind* of thing the call is
/// about, which is what a glance at the row is for.
fn short_argument(argument: &str) -> String {
    match argument.find(['/', ' ']) {
        Some(at) => format!("{}…", &argument[..at + 1]),
        None => argument.to_owned(),
    }
}

/// The last resort: the compact wording cut, with the seconds kept whole.
///
/// A pane too narrow even for that — under about eleven cells, which no
/// terminal has — cuts the row, seconds and all: there is nothing left to give.
fn cut_keeping_seconds(fact: &WorkFact, room: usize) -> String {
    let tail = match fact.seconds.as_deref() {
        Some(seconds) if !seconds.is_empty() => format!(" · {seconds}"),
        _ => String::new(),
    };
    let wording = room.saturating_sub(titi_tui::width::visible_width(&tail) + 1);
    titi_tui::width::truncate_to_width(
        &format!(
            "{}…{tail}",
            titi_tui::width::truncate_to_width(&fact.compact, wording)
        ),
        room,
    )
}

/// The name a session already has, read once at startup.
///
/// The engine announces a name it has just made and nothing else, so a session
/// that is being resumed would otherwise spend the run showing no name at all.
/// The index is the same one `/sessions` and the switcher read, and
/// `needs_auto_title` is what tells a real name from the product name a session
/// is created with (`title: Some("titi")`), which is not a name anyone chose.
fn stored_session_title(agent_dir: &Path, session_id: &str) -> String {
    let Ok(index) = titi_core::session::SessionIndex::open(&agent_dir.join("state.db")) else {
        return String::new();
    };
    if index.needs_auto_title(session_id).unwrap_or(true) {
        return String::new();
    }
    index.title(session_id).ok().flatten().unwrap_or_default()
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
                // A reply to a query this run sent arrives on this same stream,
                // read as if it were typed. It is taken first, so no character
                // of it can land in the composer.
                if !chat.absorb_probe_key(&key, Instant::now())
                    && let Some(mapped) = map_key(key.code, key.modifiers)
                {
                    let applied = chat.on_key(mapped, Instant::now());
                    if dispatch(engine, chat, session_log, cast, applied) {
                        return Ok(true);
                    }
                }
            }
            Event::FocusGained => chat.on_focus_gained(Instant::now()),
            Event::Paste(text) => chat.paste(&text),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(applied) = chat.mouse_press(mouse.column, mouse.row)
                        && dispatch(engine, chat, session_log, cast, applied)
                    {
                        return Ok(true);
                    }
                }
                // A drag moves the selection's corner and nothing else: the
                // transcript must not scroll out from under the highlight.
                MouseEventKind::Drag(MouseButton::Left) => chat.mouse_drag(mouse.column, mouse.row),
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(text) = chat.mouse_release() {
                        let path = std::env::var("PATH").unwrap_or_default();
                        copy_selection(chat, &text, &path);
                    }
                }
                MouseEventKind::ScrollUp => chat.mouse_wheel(1, Instant::now()),
                MouseEventKind::ScrollDown => chat.mouse_wheel(-1, Instant::now()),
                _ => {}
            },
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
        Some(ChatEffect::SendAll(commands)) => {
            for command in commands {
                if engine.try_send(command).is_err() {
                    // One word per run: an engine that has gone does not take
                    // the rest either.
                    chat.push(LineKind::Error, "engine stopped".to_owned());
                    break;
                }
            }
            false
        }
        None => false,
    }
}

/// The log a run should be appending to, given the session the screen is on.
///
/// The log is opened for one session id and stamps every write with it, so a run
/// whose screen has moved — `Ctrl+X` and Enter — has to re-open it for the new
/// session. A run whose writes were already off stays off: the start path said
/// so once, and turning them on mid-run would be a different promise.
fn session_log_for(log: Option<SessionLog>, chat: &Chat) -> Option<SessionLog> {
    let log = log?;
    if log.session_id() == chat.session_id {
        return Some(log);
    }
    let opened = SessionLog::open(&chat.agent_dir, &chat.session_id);
    if opened.is_none() {
        eprintln!("session: transcript writes are off (store unavailable)");
    }
    opened
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
#[path = "chat_tests.rs"]
mod tests;
