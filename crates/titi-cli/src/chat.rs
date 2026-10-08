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
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
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

use crate::herdr::{self, AgentState};
use crate::hub::{HubSession, HubUpdate};
use crate::login::{LoginDriver, LoginEvent, LoginFlow, OAuthProvider};
use crate::pickers::*;
use crate::session_log::SessionLog;
use crate::transcript::*;

const QUIT_WINDOW: Duration = Duration::from_secs(2);
pub(crate) const TOOL_PREVIEW: usize = 120;

/// Pastes longer than this many lines collapse to a marker instead of filling
/// the draft (the deleted `composer.rs`'s `PASTE_INLINE_MAX_LINES`, restored).
///
/// Six is where a paste stops reading as something the user typed: a prompt, a
/// path, a couple of log lines stay inline, while a stack trace or a file no
/// longer becomes the prompt verbatim.
const PASTE_INLINE_MAX_LINES: usize = 6;

/// Where a collapsed paste's marker starts. Only a registered marker is ever
/// expanded, and only from this prefix, so a literal in prose is never one.
const PASTE_MARKER_HEAD: &str = "[Paste #";

/// The line above the composer while a second press is owed, one per key: a
/// two-press exit names the key that confirms *it*.
const CTRL_C_HINT: &str = "ctrl-c again to quit";
const EXIT_HINT: &str = "press Enter again to quit";

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
    /// `app.history.search`: ctrl+r.
    CtrlR,
    /// Delete the word before the caret: alt+backspace or ctrl+w.
    DeleteWord,
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
struct OAuthLogin {
    provider: &'static OAuthProvider,
    flow: LoginFlow,
    /// How this provider is being signed in. The device grant has no code to
    /// paste back, so the composer must not ask for one.
    method: LoginMethod,
}

/// Conversation on screen. No terminal, no session file.
pub struct Chat {
    pub(crate) lines: Vec<TranscriptLine>,
    pub(crate) input: String,
    /// The bodies the collapsed markers in `input` stand for, by marker text.
    /// A paste too long to sit in the draft leaves a marker here instead, and
    /// [`Chat::submit`] swaps it for the body; taking the draft away takes
    /// these with it ([`Chat::clear_input`]).
    pub(crate) pastes: HashMap<String, String>,
    /// The number the next paste marker carries: monotonic for the run, so two
    /// markers in one draft can never stand for the same body.
    next_paste: u32,
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
    pub(crate) model: String,
    /// Live: a local server that answers after the first frame adds models,
    /// so the list is read when `/model` runs, not captured at startup.
    pub(crate) catalog: crate::engine::ModelCatalog,
    pub(crate) session_id: String,
    session_label: String,
    pub(crate) agent_dir: PathBuf,
    paused: bool,
    context_percent: Option<u8>,
    /// The model's context window in tokens, as the engine reported it with the
    /// percentage. The gauge is drawn from it, so `None` — before any turn has
    /// stated one — is what keeps the line between the groups blank.
    context_window: Option<u64>,
    /// Which pre-built status line the masthead paints, and what its middle does
    /// with the context. Read from the settings at startup and changed by
    /// `/statusline`.
    status_line: StatusLineStyle,
    reply: String,
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
    session_prompt_tokens: u32,
    session_completion_tokens: u32,
    last_prompt_tokens: u32,
    last_completion_tokens: u32,
    /// The part of the prompt counts above the provider read from its cache.
    session_cached_tokens: u32,
    last_cached_tokens: u32,
    quit_armed: Option<Instant>,
    /// When Esc was last pressed on an empty composer: the first press arms
    /// this, a second inside [`QUIT_WINDOW`] is the rewind chord, and typing
    /// clears it. One field, so the two-press shape has one window.
    esc_armed: Option<Instant>,
    hint: String,
    /// Provider waiting for a key or an OAuth code. The composer masks
    /// whatever is typed in either case.
    pub(crate) login_for: Option<String>,
    /// The OAuth login behind `login_for`, when the provider is signed in
    /// through a browser rather than with a pasted key.
    oauth: Option<OAuthLogin>,
    /// Where a login is started. Production builds the terminal driver on
    /// first use; tests inject one so no socket, browser or provider is
    /// involved.
    login_driver: Option<Arc<dyn LoginDriver>>,
    /// Highlight in the leading-slash command list.
    pub(crate) picker: usize,
    /// Highlight in the bare-`/login` subscription picker; `None` = closed.
    pub(crate) login_picker: Option<usize>,
    /// Ctrl+X: the session the screen is on, in the list of stored sessions.
    pub(crate) session_picker: Option<usize>,
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
    intro: Option<Instant>,
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
    selection: Option<Selection>,
    /// The transcript's rows as the last frame drew them, as plain text: what
    /// a copy of a selection carries, style and padding left behind.
    pub(crate) last_rows: Vec<String>,
    /// The screen row the transcript starts on. A mouse event arrives in
    /// screen coordinates; this is what turns one into a row of
    /// [`Chat::last_rows`].
    transcript_top: u16,
    /// The mouse preset this run has enabled. `/mouse` changes it, and the
    /// way out disables mouse reporting whatever it is.
    mouse_preset: MousePreset,
    /// Sequences to write before the next frame, outside ratatui's diff: the
    /// mouse preset switching over, an OSC 11 query, an OSC 52 copy.
    output_flush: String,
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
            pastes: HashMap::new(),
            next_paste: 0,
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
            login_picker: None,
            session_picker: None,
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

    // ---- Mouse selection -------------------------------------------------
    //
    // The transcript is the only surface with a selection: the composer has a
    // caret, the pickers have a cursor. A press anchors, a drag moves the
    // anchor's other corner, a release copies. Nothing here scrolls — a drag
    // that scrolled would move the text out from under the selection.

    /// Mouse press: anchor a drag-select at a screen cell.
    pub fn mouse_press(&mut self, x: u16, y: u16) {
        self.selection = Some(Selection::anchor(x, y));
    }

    /// Mouse drag: move the selection's far corner.
    pub fn mouse_drag(&mut self, x: u16, y: u16) {
        if let Some(selection) = &mut self.selection {
            selection.drag(x, y);
        }
    }

    /// Mouse release: the selection stands, and its text is what was copied.
    ///
    /// Returns `None` for a click (an empty selection) so nothing reaches the
    /// clipboard on a press that selected nothing.
    pub fn mouse_release(&mut self) -> Option<String> {
        let selection = self.selection.as_mut()?;
        selection.release();
        let text = self.selection_text();
        (!text.is_empty()).then_some(text)
    }

    /// The wheel: a panel's cursor when one is open — the wheel moves it the
    /// way the arrows do — and the transcript's scroll otherwise. The wheel
    /// never *opens* anything: ↑ at an empty composer is the history's, and a
    /// wheel is not a key.
    pub fn mouse_wheel(&mut self, delta: isize, now: Instant) {
        if self.panel_open() {
            let key = if delta > 0 { Key::Up } else { Key::Down };
            self.on_key(key, now);
            return;
        }
        if delta > 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(delta as usize);
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(delta.unsigned_abs());
        }
    }

    /// Forget the selection — a key or a new press takes it away.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The committed selection, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// The selection as the transcript sees it: the screen rows translated to
    /// the transcript's own rows, and `None` when the selection never reached
    /// the transcript at all (`None` after a release is the same as a selection
    /// that holds no text).
    pub(crate) fn transcript_selection(&self) -> Option<Selection> {
        let selection = self.selection?;
        if !selection.is_non_empty() {
            return None;
        }
        let (_, top, _, bottom) = selection.rect()?;
        let origin = self.transcript_top;
        let last = origin.saturating_add(self.last_rows.len() as u16);
        if bottom < origin || top >= last {
            return None;
        }
        let shift = |y: u16| y.saturating_sub(origin);
        Some(Selection {
            anchor: (selection.anchor.0, shift(selection.anchor.1)),
            current: (selection.current.0, shift(selection.current.1)),
            active: selection.active,
        })
    }

    /// What a copy of the selection carries: the plain text of the selected
    /// columns, with no styling and no padding.
    pub fn selection_text(&self) -> String {
        match self.transcript_selection() {
            Some(selection) => selection.text(&self.last_rows),
            None => String::new(),
        }
    }

    // ---- Editing ----------------------------------------------------------

    /// Delete the word before the caret: the run of spaces first, then the word
    /// itself — omp's `deleteBeforeCursor`, which the space-hold gesture's
    /// retract used and the live composer had no key for.
    ///
    /// The primitive is `titi_tui::space_hold`'s, character-counted, so a
    /// multi-byte or wide character is one character and not one byte.
    fn delete_word(&mut self) {
        let trailing = self
            .input
            .chars()
            .rev()
            .take_while(|ch| ch.is_whitespace())
            .count();
        let word = self
            .input
            .chars()
            .rev()
            .skip(trailing)
            .take_while(|ch| !ch.is_whitespace())
            .count();
        if trailing + word == 0 {
            return;
        }
        titi_tui::space_hold::delete_before_cursor(&mut self.input, trailing + word);
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

    pub fn on_key(&mut self, key: Key, now: Instant) -> Applied {
        // The next key takes a standing selection away, the way every terminal
        // does: the highlight is about the copy that just happened, not a mode.
        self.clear_selection();
        if self.approval.is_some() {
            return self.approval_key(key);
        }
        if self.login_for.is_some() {
            return self.login_key(key);
        }
        if self.theme_picker.is_some() {
            return self.theme_picker_key(key, now);
        }
        if self.session_picker.is_some() {
            return self.session_picker_key(key, now);
        }
        if self.session_search.is_some() {
            return self.session_search_key(key, now);
        }
        if self.login_picker.is_some() {
            return self.login_picker_key(key, now);
        }
        if self.model_picker.is_some() {
            return self.model_picker_key(key, now);
        }
        if self.emoji_picker.is_visible() {
            return self.emoji_picker_key(key, now);
        }
        if self.history_picker.is_some() {
            return self.history_picker_key(key, now);
        }
        match key {
            Key::CtrlC if self.turn_active => {
                self.disarm();
                Applied::effect(ChatEffect::Send(EngineCommand::Cancel))
            }
            Key::CtrlC => self.arm_quit(now, CTRL_C_HINT),
            Key::CtrlR => self.open_history(),
            Key::CtrlX => self.open_session_picker(),
            Key::AltM => {
                self.open_model_picker();
                Applied::none()
            }
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
                // A space terminates an emoticon as it is typed; Enter is the
                // other terminator, so the line reaches the transcript as the
                // glyph rather than the keystrokes.
                self.expand_trailing_emoticon();
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
            Key::DeleteWord => {
                self.disarm();
                self.delete_word();
                self.sync_emoji_picker();
                Applied::none()
            }
            Key::Backspace => {
                self.disarm();
                self.input.pop();
                // The query may still stand after the pop (`:sm` from `:smi`),
                // so the picker follows the text here too.
                self.sync_emoji_picker();
                self.picker = 0;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Char(ch) => {
                self.disarm();
                self.type_char(ch);
                self.picker = 0;
                self.scroll_offset = 0;
                Applied::none()
            }
            Key::Esc => self.escape(now),
            // ↑ at an empty composer is the prompt history's (omp
            // `app.history.search`); with text in the composer it scrolls the
            // transcript, as it always has.
            Key::Up if self.input.is_empty() => self.open_history(),
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
            Key::CtrlD | Key::Tab => {
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
                ..
            } => {
                self.last_prompt_tokens = prompt_tokens;
                self.last_completion_tokens = completion_tokens;
                self.last_cached_tokens = cached_tokens;
                // The running turn's own ledger, for the footer under its
                // answer; the totals below outlive it.
                self.turn_usage = Some((prompt_tokens, cached_tokens, completion_tokens));
                // Money is a property of the model the turn ran on, so it is
                // read here, where the turn's tokens and the current model are
                // both in hand. An unpriced model costs nothing to state: the
                // footer drops the figure rather than inventing one, and the
                // session total says it is a floor.
                match self.catalog.price(&self.model) {
                    Some(price) => {
                        let cost =
                            price.cost_micro_usd(prompt_tokens, cached_tokens, completion_tokens);
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
                self.push(LineKind::Agent, format!("tool agent {name}: started"));
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

    /// Insert pasted text into the composer. A paste is usually code or a
    /// log, so its line breaks and tabs are kept — a `\r\n` or lone `\r`
    /// becomes `\n` — and every other control character is dropped.
    ///
    /// More than [`PASTE_INLINE_MAX_LINES`] lines do not go into the draft at
    /// all: the draft takes a one-line marker and the body is kept aside, so a
    /// pasted stack trace cannot read as the prompt the user is writing (and
    /// the one-row composer can show the whole draft). [`Chat::submit`] swaps
    /// the two, and the transcript echoes the marker rather than the wall.
    pub fn paste(&mut self, text: &str) {
        if self.approval.is_some() {
            return;
        }
        self.disarm();
        // A pasted body is composer input, not a picker keystroke.
        self.login_picker = None;
        self.model_picker = None;
        self.emoji_picker.hide();
        let body = paste_body(text);
        let lines = body.lines().count();
        if lines <= PASTE_INLINE_MAX_LINES {
            self.input.push_str(&body);
            return;
        }
        self.next_paste += 1;
        let marker = paste_marker(self.next_paste, lines);
        self.pastes.insert(marker.clone(), body);
        self.input.push_str(&marker);
    }

    /// The draft as it will be sent: every marker this draft holds replaced by
    /// the body it stands for.
    ///
    /// A marker is expanded only where the registry has it, and a substituted
    /// body is never scanned again, so text that merely looks like a marker —
    /// or a marker left over from a draft the user has moved on from — stays
    /// literal and can never ship a body from somewhere else.
    fn expand_pastes(&self, draft: &str) -> String {
        if self.pastes.is_empty() {
            return draft.to_owned();
        }
        let mut out = String::with_capacity(draft.len());
        let mut rest = draft;
        while let Some(at) = rest.find(PASTE_MARKER_HEAD) {
            out.push_str(&rest[..at]);
            let tail = &rest[at..];
            match self
                .pastes
                .iter()
                .find(|(marker, _)| tail.starts_with(marker.as_str()))
            {
                Some((marker, body)) => {
                    out.push_str(body);
                    rest = &tail[marker.len()..];
                }
                None => {
                    out.push_str(PASTE_MARKER_HEAD);
                    rest = &tail[PASTE_MARKER_HEAD.len()..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Drop the draft — and the pasted bodies its markers stood for, so a
    /// marker cannot outlive the message it was pasted into.
    fn clear_input(&mut self) {
        self.input.clear();
        self.pastes.clear();
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

    /// Esc. On a draft it clears the composer, as it always has; on an empty
    /// composer a second press inside [`QUIT_WINDOW`] is the rewind chord
    /// (omp `doubleEscapeAction`, default `rewind`), which is exactly what
    /// `/rewind` does, so the chord and the command cannot drift.
    fn escape(&mut self, now: Instant) -> Applied {
        let armed = self.esc_armed.take();
        self.disarm();
        if !self.input.is_empty() {
            self.clear_input();
            self.picker = 0;
            return Applied::none();
        }
        if armed.is_some_and(|at| now.saturating_duration_since(at) <= QUIT_WINDOW) {
            return self.rewind("");
        }
        self.esc_armed = Some(now);
        Applied::none()
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
            "login" => self.login(args),
            "logout" => self.logout(args),
            "keys" | "whoami" => self.keys(),
            "theme" => self.theme(args),
            "statusline" => self.statusline(args),
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

    fn show_history(&mut self, messages: &[titi_providers::ChatMessage]) {
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

    // -----------------------------------------------------------------------
    // Emoji shortcodes and emoticons
    // -----------------------------------------------------------------------

    /// Type `ch` into the composer, expanding the shortcode a closing `:`
    /// closes or the emoticon a terminating space ends. The table and the
    /// guards are `titi_tui::emoji`'s; this is the live composer's own buffer,
    /// so the expansion runs over it here.
    ///
    /// The caret is the end of the buffer, so an expansion lands it directly
    /// after the glyph and nothing else has to move.
    fn type_char(&mut self, ch: char) {
        let terminator = matches!(ch, ' ' | '\n' | '\r');
        let expansion = if terminator {
            titi_tui::emoji::try_expand_emoticon(&self.input)
        } else if ch == ':' {
            titi_tui::emoji::try_expand_shortcode(&self.input)
        } else {
            None
        };
        self.input.push(ch);
        if let Some((start, glyph)) = expansion {
            self.input.truncate(start);
            self.input.push_str(glyph);
            // The closing colon of a shortcode *is* the trigger: the
            // expansion consumed it. An emoticon's terminator is kept after
            // the glyph, the way it was typed.
            if terminator {
                self.input.push(ch);
            }
        }
        self.sync_emoji_picker();
    }

    /// Expand an emoticon sitting at the end of the composer, for the Enter
    /// terminator: the space case is handled as the space is typed, and Enter
    /// does the same before the line is sent.
    fn expand_trailing_emoticon(&mut self) {
        if let Some((start, glyph)) = titi_tui::emoji::try_expand_emoticon(&self.input) {
            self.input.truncate(start);
            self.input.push_str(glyph);
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
        self.clear_input();
        self.login_for = None;
        self.oauth = None;
        self.push(LineKind::Note, "login cancelled".to_owned());
        Applied::none()
    }

    fn store_login_secret(&mut self) -> Applied {
        let secret = std::mem::take(&mut self.input);
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

    /// `/budget [amount|off]` caps what this session may spend.
    ///
    /// The cap is counted in tokens, and that is the engine's
    /// (`runtime.rs`'s `budget`, tripped against `prompt + completion`). A cap
    /// in money is still refused, and now the refusal can say exactly what is
    /// missing: the engine counts tokens, not dollars, and prompt and
    /// completion tokens bill at different rates, so no single dollar figure
    /// converts into one token cap. Enforcing it needs a cost ledger in the
    /// engine, which is a change to a file this surface does not own.
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
                let said = match error {
                    BudgetArgError::Money => self.money_budget_refusal(),
                    other => other.to_string(),
                };
                self.push(LineKind::Error, said);
                Applied::none()
            }
        }
    }

    /// Why a cap in money cannot be honoured here, said with whatever this
    /// machine knows about the model.
    ///
    /// The refusal is not a shrug: when the current model has a price, the
    /// refusal states it, so the user can see what a dollar would have bought
    /// and that the missing piece is the engine's tally and not the price.
    /// Nobody's price is guessed into a token cap — a rate the user did not
    /// state would be a number they could not act on.
    fn money_budget_refusal(&self) -> String {
        match self.catalog.price(&self.model) {
            Some(price) => format!(
                "budget: {} costs {} in / {} out per MTok, but the engine's cap counts tokens, \
                 and those bill at different rates — a cap in money needs the engine's cost \
                 ledger; cap tokens instead (e.g. /budget 200k)",
                self.model,
                titi_tui::status::format_usd(price.input, 2),
                titi_tui::status::format_usd(price.output, 2),
            ),
            None => format!(
                "budget: {} has no price here — the engine's cap counts tokens, not money, so \
                 cap tokens instead (e.g. /budget 200k)",
                self.model,
            ),
        }
    }

    /// What has been spent, against the cap if there is one.
    fn show_budget(&mut self) {
        let text = match self.budget {
            Some(limit) => format!(
                "budget: {} of {limit} tokens spent ({}%)",
                self.spent_tokens,
                share(self.spent_tokens, limit)
            ),
            None => format!("budget: no cap · {} tokens spent", self.spent_tokens),
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

    fn arm_quit(&mut self, now: Instant, hint: &str) -> Applied {
        if let Some(armed) = self.quit_armed
            && now.saturating_duration_since(armed) <= QUIT_WINDOW
        {
            return Applied::effect(ChatEffect::Quit);
        }
        self.quit_armed = Some(now);
        self.hint = hint.to_owned();
        Applied::none()
    }

    /// Leaving from the composer: the bare word `exit`/`quit`/`q`, or
    /// `/exit`/`/quit` (omp `input.bareExitOnEmptySession`).
    ///
    /// A session with nothing in it has nothing to keep, so the first word
    /// leaves; once a turn is on the screen — finished or in flight — the
    /// same word only arms the exit, and a second press inside
    /// [`QUIT_WINDOW`] leaves. The window is the one Ctrl+C uses: one shape
    /// for "press it twice to be sure", so both keys confirm the same intent.
    fn exit_word(&mut self, now: Instant) -> Applied {
        if !self.has_conversation() {
            return Applied::effect(ChatEffect::Quit);
        }
        self.arm_quit(now, EXIT_HINT)
    }

    /// Whether the screen holds something a leave would give up: a finished
    /// turn's lines, or one still in flight.
    fn has_conversation(&self) -> bool {
        self.turn_active
            || self
                .lines
                .iter()
                .any(|line| matches!(line.kind, LineKind::User | LineKind::Assistant))
    }

    /// Say one thing above the composer until the next key.
    fn set_hint(&mut self, text: String) {
        self.hint = text;
    }

    pub(crate) fn disarm(&mut self) {
        self.quit_armed = None;
        self.esc_armed = None;
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
            self.push(LineKind::Usage, footer.row());
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
    chat.status_line = StatusLineStyle::resolve(preset.as_deref(), context_line.as_deref());
    // A resumed session already has a name; the engine only announces one it
    // has just made, so read the one it has (the same index `/sessions` and the
    // switcher read) instead of showing no name for the whole run.
    chat.session_label = stored_session_title(&chat.agent_dir, &session_id);
    // The engine resumed this session's history; the screen shows the same.
    chat.show_stored_history();
    // `--continue` that found nothing: the screen says so rather than opening
    // on a welcome that reads as a resume which quietly did nothing.
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

/// The `/token` under the cursor: the trailing word, when it opens with a
/// slash at the start of the line or after whitespace and holds nothing but
/// name characters. That is what keeps `/tmp/photo.png` and `a/b` out.
pub(crate) fn slash_token(input: &str) -> Option<(usize, &str)> {
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
    /// A cap in money, which no token cap can stand for: prompt and
    /// completion tokens bill at different rates, so one dollar figure has no
    /// single token answer. [`Chat::money_budget_refusal`] says this with the
    /// model's own price; this is the reading's own sentence, for callers
    /// that only parse.
    Money,
    Unreadable(String),
    Zero,
}

impl std::fmt::Display for BudgetArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Money => f.write_str(
                "budget: the engine's cap counts tokens, not money, so a cap in dollars cannot \
                 be enforced — cap tokens instead (e.g. /budget 200k)",
            ),
            Self::Unreadable(word) => {
                write!(
                    f,
                    "budget: {word} is not an amount (200000, 200k, 1.5m, off)"
                )
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
        (
            empty_state(chat, cols[2].width, cols[2].height, &theme),
            Vec::new(),
            Vec::new(),
        )
    } else {
        transcript(chat, cols[2].width, cols[2].height, &theme)
    };
    chat.transcript_top = cols[2].y;
    frame.render_widget(body, cols[2]);
    paint_selection(frame, chat, cols[2], &theme);
    paint_photos(frame, cols[2], &photos, &theme);
    paint_links(frame, cols[2], &links);
    if let Some(view) = &panel {
        frame.render_widget(panel_box(view, cols[3].width, &theme), cols[3]);
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

/// The word the masthead shows for the session's state.
fn state_word(chat: &Chat) -> &'static str {
    if chat.approval.is_some() {
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
    if chat.approval.is_some() || chat.paused || chat.login_for.is_some() {
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
    Paragraph::new(line).style(page(theme))
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
fn masthead_snapshot(chat: &Chat) -> StatusSnapshot {
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
const WELCOME_INTRO: Duration = Duration::from_millis(1500);

/// Half the width of the shine band, along the mark's diagonal (0 to 1).
const WELCOME_SHINE: f64 = 0.2;

/// What the first screen states, every fact read from the source the surface
/// that owns it reads: the catalog plus the credential reader behind `/keys`,
/// the status-bar snapshot behind the masthead, and the session list behind
/// `/sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WelcomeFacts {
    version: &'static str,
    /// The model that will answer this turn, and what stands behind it.
    model: String,
    credential: Option<String>,
    path: String,
    git: Option<WelcomeGit>,
    /// Recent sessions, newest first, and whether the screen is already on one.
    sessions: Vec<(String, bool)>,
}

/// The git fact of the welcome, in the same shape the masthead states it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WelcomeGit {
    branch: String,
    unstaged: u32,
    staged: u32,
    untracked: u32,
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
struct WelcomeGrays {
    /// The ink: values, the wordmark, the mark's lit corner.
    bright: f64,
    /// The ink most of the way back to the page: labels, the build, the
    /// chords, the mark's far corner.
    faded: f64,
    /// The far end of the page's lightness, past the ink, where the intro's
    /// shine lifts a cell to.
    peak: f64,
}

impl WelcomeGrays {
    fn of(theme: &Theme) -> Self {
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
fn welcome_shine(elapsed: Duration) -> Option<f64> {
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
fn welcome_mark(grays: &WelcomeGrays, shine: Option<f64>) -> Vec<Vec<Span<'static>>> {
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
const WELCOME_LABEL: usize = 9;

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
fn welcome_fact_rows(
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
const WELCOME_TIP_COLUMNS: usize = 50;

/// What the first screen can point at, each one true of this build — a chord
/// the live mapper yields or a command [`COMMANDS`] runs — and short enough to
/// fit whole at [`WELCOME_TIP_COLUMNS`].
const WELCOME_TIPS: [&str; 20] = [
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
fn welcome_tip(session_id: &str) -> Option<&'static str> {
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
fn empty_state(chat: &Chat, width: u16, height: u16, theme: &Theme) -> Paragraph<'static> {
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
        const KEYS: &str = "   y allow    n refuse";
        let room = inner.saturating_sub(titi_tui::width::visible_width(KEYS));
        // A cut command says so: an approval must not read as the whole
        // command when it is only its head.
        let subject = ellipsis_label(pending.subject(), room.max(1));
        Line::from(Span::styled(
            titi_tui::width::truncate_to_width(&format!("{subject}{KEYS}"), inner),
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
            Span::styled(
                fit_tail(&composer_view(&chat.input), room),
                fg(theme, ThemeColor::Text),
            ),
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
    } else if chat.emoji_picker.is_visible() {
        "↑↓ move  ·  tab takes  ·  esc closes"
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

pub(crate) fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    // A cell holding a tab is drawn as nothing, so pasted indentation is
    // spelled out before the text is measured.
    let text = text.replace('\t', "    ");
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

/// The composer's one row: a pasted line break is shown as `↵` and a tab as
/// four spaces, so a multi-line paste reads as what it is without the box
/// growing. The input itself keeps both.
fn composer_view(input: &str) -> String {
    input.replace('\n', "↵").replace('\t', "    ")
}

/// A pasted body as the composer keeps it: `\r\n` and a lone `\r` become `\n`,
/// tabs and newlines are kept — a paste is usually code or a log, so its line
/// breaks and indentation are part of it — and every other control character
/// is dropped.
fn paste_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(ch),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// The one-line stand-in a collapsed paste leaves in the draft:
/// `[Paste #2 · 14 lines]`. One line, so the one-row composer can show the
/// whole draft, and bracketed so it cannot read as prose the user typed.
fn paste_marker(seq: u32, lines: usize) -> String {
    format!("[Paste #{seq} · {lines} lines]")
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

pub(crate) fn one_line(text: &str, max: usize) -> String {
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
        // The crate's table binds `app.session.switch` to ctrl+x and
        // `app.model.select` to alt+m; a live screen that drops the modifier
        // leaves both chords with nothing to reach.
        KeyCode::Char('x') if control => Some(Key::CtrlX),
        KeyCode::Char('m') if modifiers.contains(KeyModifiers::ALT) => Some(Key::AltM),
        KeyCode::Char('r') if control => Some(Key::CtrlR),
        KeyCode::Char('w') if control => Some(Key::DeleteWord),
        // The two ends of the keyboard's own word delete: the macOS chord and
        // the readline one. Both are a word at a time, which the composer
        // otherwise cannot do — backspace takes exactly one character.
        KeyCode::Backspace if modifiers.contains(KeyModifiers::ALT) => Some(Key::DeleteWord),
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
                    chat.mouse_press(mouse.column, mouse.row)
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

/// The OS clipboard writers this build knows, in the order they are asked for.
///
/// The same three the old `App` read the clipboard with, so a copy lands in
/// the one clipboard a terminal, a browser and an editor all share.
const CLIPBOARD_WRITERS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
];

/// The file `bin` would be run from, if one is on `path`.
///
/// The capability check for the OS clipboard: a build with no `pbcopy` and no
/// selection tool falls back to OSC 52 rather than spawning a program that is
/// not there. `path` is passed in (not read from the environment here) so the
/// check is a pure function of what it is given.
fn executable_path(bin: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(bin))
        .find(|candidate| candidate.is_file())
}

/// The first OS clipboard writer on `path`, resolved to the file that will be
/// run — never the bare name, so the check and the spawn cannot disagree about
/// which `pbcopy` answered.
fn clipboard_writer(path: &str) -> Option<(&'static str, &'static [&'static str], PathBuf)> {
    CLIPBOARD_WRITERS
        .iter()
        .find_map(|(bin, args)| executable_path(bin, path).map(|program| (*bin, *args, program)))
}

/// Hand `text` to the OS clipboard writer at `program`.
///
/// The child's stdin is taken and dropped before the wait: leaving the pipe
/// open would leave `pbcopy` waiting for an end of input that never comes.
fn write_to_clipboard(program: &Path, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().ok_or_else(|| "no stdin".to_owned())?;
    stdin
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    drop(stdin);
    let status = child.wait().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {status}", program.display()))
    }
}

/// Put a copied selection where the user can paste it, and say where.
///
/// The OS writer when this machine has one; otherwise OSC 52, which the
/// terminal itself puts on the clipboard the user is at — the route that works
/// over SSH. A copy with no route at all still names itself, so a screen that
/// copied nothing does not look like one that did. `path` is the search path
/// the OS writer is looked for on.
fn copy_selection(chat: &mut Chat, text: &str, path: &str) {
    let chars = text.chars().count();
    let route = match clipboard_writer(path) {
        Some((bin, args, program)) => match write_to_clipboard(&program, args, text) {
            Ok(()) => format!("copied {chars} chars · {bin}"),
            Err(reason) => {
                chat.output_flush
                    .push_str(&titi_tui::caps::osc52_copy(text));
                format!("copied {chars} chars · OSC 52 ({reason})")
            }
        },
        None => {
            chat.output_flush
                .push_str(&titi_tui::caps::osc52_copy(text));
            format!("copied {chars} chars · OSC 52")
        }
    };
    chat.set_hint(route);
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
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use titi_engine::TurnId;
    use titi_providers::StopReason;
    use titi_tui::markdown::SectionMode;

    /// A chat with the theme a test names, for the ones that need a palette
    /// where two tokens are two different colours.
    ///
    /// Every test chat gets its own fresh agent directory under a
    /// process-lifetime temp root: a helper that left `Chat::new`'s default
    /// (`~/.titi/agent`) in place let tests like the slash-command sweep run
    /// `/logout openai` against the operator's real key store. The root is
    /// owned by a `LazyLock` (never `Box::leak`); tests that set `agent_dir`
    /// explicitly still override it.
    fn chat_with_theme(theme: Arc<Theme>) -> Chat {
        static ROOT: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
            tempfile::TempDir::with_prefix("titi-cli-test-agent").expect("temp agent root")
        });
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(ROOT.path()).join(format!("agent-{n}"));
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme);
        chat.set_agent_dir(&dir);
        chat
    }

    fn chat() -> Chat {
        chat_with_theme(test_theme())
    }

    /// Pins the invariant the helper above exists for: no helper-built chat
    /// may ever point at the real agent directory, or any mutating command in
    /// a test (`/logout`, `/export`, `/checkpoint`, `/fork`, ...) operates on
    /// the developer's own `~/.titi`.
    #[test]
    fn helper_chat_isolated_from_real_agent_dir() {
        let chat = chat();
        let real = titi_config::agent_dir();
        assert_ne!(chat.agent_dir, real);
        assert!(
            chat.agent_dir.starts_with(std::env::temp_dir()),
            "agent dir {:?} not under {}",
            chat.agent_dir,
            std::env::temp_dir().display()
        );
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

    /// $3/MTok in, $15/MTok out, $0.30/MTok cached read — the shape of a
    /// price the engine's descriptor carries.
    fn test_price() -> titi_engine::ModelPrice {
        titi_engine::ModelPrice {
            input: 3_000_000,
            output: 15_000_000,
            cached_input: Some(300_000),
        }
    }

    /// A chat whose current model is priced. No built-in model ships with a
    /// price (`NO_PRICE_MODELS`), so the money paths are driven with one
    /// written in by hand — the same route a user's `models` settings entry
    /// takes.
    fn priced_chat() -> Chat {
        let mut chat = chat();
        chat.catalog = crate::engine::ModelCatalog::fixed_priced(
            vec![chat.model.clone()],
            vec![(chat.model.clone(), test_price())],
        );
        chat
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
            detail: None,
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
            detail: None,
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
            detail: None,
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
            detail: None,
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

    /// A bare `exit` is the composer's own word for leaving (omp
    /// `input.bareExitOnEmptySession`): it never becomes a prompt, and once
    /// the session has something to lose the screen asks for a second Enter.
    #[test]
    fn a_bare_exit_asks_once_and_then_leaves() {
        // Nothing on the screen yet, so there is nothing to keep: the first
        // word leaves.
        let mut fresh = chat();
        type_text(&mut fresh, "exit");
        assert_eq!(
            fresh.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Quit)
        );

        // A turn in the session is worth asking about.
        let mut chat = chat();
        type_text(&mut chat, "hello");
        chat.on_key(Key::Enter, Instant::now());
        let before = chat.lines.len();
        type_text(&mut chat, "exit");
        let at = Instant::now();
        let first = chat.on_key(Key::Enter, at);
        assert!(
            first.effect.is_none(),
            "the first Enter only asks: {first:?}"
        );
        assert!(first.log.is_none(), "and writes nothing to the session");
        assert_eq!(chat.lines.len(), before, "no prompt reached the transcript");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("press Enter again to quit"), "{frame}");
        let second = chat.on_key(Key::Enter, at + Duration::from_millis(200));
        assert_eq!(second.effect, Some(ChatEffect::Quit));
    }

    /// One word, three spellings. Short words that *start* with one of them
    /// are prompts.
    #[test]
    fn only_the_whole_exit_word_leaves() {
        for word in ["exit", "quit", "q"] {
            let mut chat = chat();
            type_text(&mut chat, word);
            assert_eq!(
                chat.on_key(Key::Enter, Instant::now()).effect,
                Some(ChatEffect::Quit),
                "{word:?} leaves an empty session"
            );
        }

        let mut chat = chat();
        type_text(&mut chat, "exit code");
        match chat.on_key(Key::Enter, Instant::now()).effect {
            Some(ChatEffect::Send(EngineCommand::SubmitPrompt { text })) => {
                assert_eq!(text.as_str(), "exit code");
            }
            other => panic!("expected a prompt, got {other:?}"),
        }
    }

    /// `/exit` and `/quit` are the same word with a slash, and both are
    /// listed so they can be discovered.
    #[test]
    fn slash_exit_leaves_like_the_bare_word() {
        for command in ["/exit", "/quit"] {
            let mut chat = chat();
            type_text(&mut chat, command);
            assert_eq!(
                chat.on_key(Key::Enter, Instant::now()).effect,
                Some(ChatEffect::Quit),
                "{command} leaves"
            );
        }
        assert!(COMMANDS.iter().any(|command| command.name == "exit"));
        assert!(COMMANDS.iter().any(|command| command.name == "quit"));
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

    /// Esc on a draft clears it, as it always has — and clearing a draft is
    /// not also the first press of the rewind chord.
    #[test]
    fn one_escape_clears_the_draft_and_arms_nothing() {
        let mut chat = chat();
        type_text(&mut chat, "a draft");
        let at = Instant::now();
        assert!(chat.on_key(Key::Esc, at).effect.is_none());
        assert!(chat.input.is_empty(), "the draft is gone");
        assert!(chat.lines.is_empty(), "nothing was sent or printed");

        // The next press on the now-empty composer only arms the chord: no
        // checkpoint exists, so a chord that fired would be an error line.
        let next = chat.on_key(Key::Esc, at + Duration::from_millis(200));
        assert!(next.effect.is_none());
        assert!(!chat.lines.iter().any(|line| line.kind == LineKind::Error));
    }

    /// A picker keeps its own Esc. See `esc_clears_the_query_then_closes_without_switching`
    /// for the model browser's half of it.
    #[test]
    fn escape_in_the_command_list_only_closes_it() {
        let mut chat = chat();
        type_text(&mut chat, "/mo");
        assert!(chat.picking(), "the command list is up");
        assert!(chat.on_key(Key::Esc, Instant::now()).effect.is_none());
        assert!(!chat.picking(), "esc closed the list");
        assert!(chat.input.is_empty(), "and took the token with it");
        assert!(!chat.lines.iter().any(|line| line.kind == LineKind::Error));
    }

    /// Esc twice on an empty composer is `/rewind` (omp
    /// `doubleEscapeAction`, default `rewind`): the same cut the typed
    /// command makes, because it is the same function.
    #[test]
    fn double_escape_on_an_empty_composer_rewinds() {
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

        let start = Instant::now();
        let first = chat.on_key(Key::Esc, start);
        assert!(first.effect.is_none(), "one Esc is not the chord");
        let second = chat.on_key(Key::Esc, start + Duration::from_millis(200));
        match second.effect {
            Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].content.as_str(), "keep");
            }
            other => panic!("expected restore, got {other:?}"),
        }
        assert!(chat.lines.iter().any(|line| line.text == "keep"));
        assert!(!chat.lines.iter().any(|line| line.text == "drop"));
    }

    /// The chord is a window, not a chain: a second Esc after it has run out
    /// only arms a new one.
    #[test]
    fn a_late_second_escape_does_not_rewind() {
        let mut chat = chat();
        let start = Instant::now();
        chat.on_key(Key::Esc, start);
        let later = chat.on_key(Key::Esc, start + QUIT_WINDOW + Duration::from_millis(1));
        assert!(later.effect.is_none());
        assert!(!chat.lines.iter().any(|line| line.kind == LineKind::Error));
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

    /// A fallback inside a turn says which model gave up, so it cannot be
    /// read as the user's own switch; the masthead follows it either way.
    #[test]
    fn a_fallback_names_the_model_it_left() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.on_event(EngineEvent::ModelSwitched {
            turn_id: Some(TurnId(7)),
            from: "openai/gpt-4.1".into(),
            to: "anthropic/claude-opus-5".into(),
        });
        assert_eq!(
            confirmations(&chat),
            ["model anthropic/claude-opus-5 · fallback from openai/gpt-4.1"]
        );
        assert_eq!(chat.model, "anthropic/claude-opus-5");
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

    /// The crate's table binds `app.session.switch` to ctrl+x and
    /// `app.model.select` to alt+m. A live screen whose mapping drops the
    /// modifier leaves both chords unreachable, so the mapping itself is
    /// asserted here, and not only through the screen.
    #[test]
    fn the_crates_chords_reach_the_live_screen() {
        assert_eq!(
            map_key(KeyCode::Char('x'), KeyModifiers::CONTROL),
            Some(Key::CtrlX),
            "ctrl+x is the session switcher"
        );
        assert_eq!(
            map_key(KeyCode::Char('m'), KeyModifiers::ALT),
            Some(Key::AltM),
            "alt+m opens the model selector"
        );
        assert_eq!(
            map_key(KeyCode::Char('m'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            Some(Key::AltM),
            "a shifted chord is still the chord"
        );
        assert_eq!(
            map_key(KeyCode::Char('m'), KeyModifiers::NONE),
            Some(Key::Char('m')),
            "and a bare m is still a character"
        );
        assert_eq!(
            map_key(KeyCode::Char('x'), KeyModifiers::NONE),
            Some(Key::Char('x'))
        );
    }

    /// Alt+m reaches the model browser from the composer, exactly as bare
    /// `/model` does.
    #[test]
    fn alt_m_opens_the_model_browser() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "half-typed words");
        chat.on_key(Key::AltM, Instant::now());
        assert!(chat.model_picker.is_some(), "alt+m opens the browser");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("▾ openai"), "and it is the browser: {frame}");
        assert!(
            frame.contains("half-typed words"),
            "the composer keeps its text: {frame}"
        );
    }

    /// The file a run appends to follows the screen.
    ///
    /// The log is opened for one session id and stamps every write with it, and
    /// nothing re-opened it: after a switch the new session stayed empty for the
    /// rest of the run while the screen showed its history, and resuming it
    /// later replayed nothing. The write after the switch must land in the new
    /// session's file, and the file the screen left must gain nothing.
    #[test]
    fn the_session_log_follows_the_switch() {
        let dir = tempfile::tempdir().expect("temp");
        let store = titi_core::session::SessionStore::new(dir.path()).expect("session store");
        let create = |title: &str| {
            store
                .create(titi_core::session::SessionMeta {
                    title: Some(title.to_owned()),
                    source: Some("cli".to_owned()),
                    ..Default::default()
                })
                .expect("create")
        };
        // The session the run starts on, with a file of its own, and another to
        // switch to.
        let left_behind = create("left");
        store
            .append(&left_behind, Role::User, "left question")
            .expect("append");
        let other = create("other");
        store
            .append(&other, Role::User, "other question")
            .expect("append");
        store
            .append(&other, Role::Assistant, "other answer")
            .expect("append");
        let mut chat = Chat::new("openai/gpt-4.1", &left_behind, test_theme());
        chat.agent_dir = dir.path().to_path_buf();

        // The run opens its log for the session it started on.
        let log = SessionLog::open(dir.path(), &left_behind).expect("a log");
        let mut log = session_log_for(Some(log), &chat);
        assert!(
            log.is_some(),
            "a run that started with a log keeps one while the screen has not moved"
        );
        record(
            &mut chat,
            &log,
            Some(LogWrite::text(Role::User, "before the switch".to_owned())),
        );
        let before =
            std::fs::read_to_string(dir.path().join(format!("sessions/{left_behind}.jsonl")))
                .expect("the session on screen is written");

        // The screen switches to the other session, exactly as Ctrl+X does.
        chat.session_picker = Some(
            chat.session_choices()
                .iter()
                .position(|id| *id == other)
                .expect("the other session is offered"),
        );
        chat.on_key(Key::Enter, Instant::now());
        assert_eq!(chat.session_id, other, "the screen moved");

        // The next write follows it.
        log = session_log_for(log, &chat);
        assert_eq!(
            log.as_ref().map(SessionLog::session_id),
            Some(other.as_str()),
            "the log is on the session on screen"
        );
        record(
            &mut chat,
            &log,
            Some(LogWrite::text(Role::User, "after the switch".to_owned())),
        );

        let moved = std::fs::read_to_string(dir.path().join(format!("sessions/{other}.jsonl")))
            .expect("the new session is written");
        assert!(
            moved.contains("after the switch"),
            "the write after the switch is in the new session: {moved}"
        );
        assert!(
            !moved.contains("before the switch"),
            "and not the message that came before it: {moved}"
        );
        let left =
            std::fs::read_to_string(dir.path().join(format!("sessions/{left_behind}.jsonl")))
                .expect("the session the screen left is still there");
        assert_eq!(
            left, before,
            "the session the screen left gains nothing after the switch"
        );
    }

    /// Ctrl+X lists the stored sessions, marks the one on screen, and Enter
    /// switches: the session's history replaces the screen and the engine is
    /// told to replay it, as a rewind does. Esc closes without switching.
    #[test]
    fn ctrl_x_switches_sessions() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let store = titi_core::session::SessionStore::new(dir.path()).expect("session store");
        let older = store
            .create(titi_core::session::SessionMeta {
                title: Some("older".to_owned()),
                source: Some("cli".to_owned()),
                ..Default::default()
            })
            .expect("create older");
        store
            .append(&older, Role::User, "older question")
            .expect("append");
        store
            .append(&older, Role::Assistant, "older answer")
            .expect("append");
        let newer = store
            .create(titi_core::session::SessionMeta {
                title: Some("newer".to_owned()),
                source: Some("cli".to_owned()),
                ..Default::default()
            })
            .expect("create newer");
        store
            .append(&newer, Role::User, "newer question")
            .expect("append");

        chat.on_key(Key::CtrlX, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("sessions · 2"),
            "both sessions are listed: {frame}"
        );
        assert!(
            frame.contains(&older),
            "the older session is a row: {frame}"
        );
        assert!(
            frame.contains(&newer),
            "the newer session is a row: {frame}"
        );

        chat.on_key(Key::Esc, Instant::now());
        assert!(
            !frame_text(&mut chat).contains("sessions · 2"),
            "esc closes it"
        );

        // Walk the cursor onto the older session and take it.
        chat.on_key(Key::CtrlX, Instant::now());
        for _ in 0..3 {
            if frame_text(&mut chat).contains(&format!("▶ {older}")) {
                break;
            }
            chat.on_key(Key::Down, Instant::now());
        }
        let applied = chat.on_key(Key::Enter, Instant::now());
        match applied.effect {
            Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
                assert_eq!(
                    messages
                        .iter()
                        .map(|message| message.content.trim().to_owned())
                        .collect::<Vec<_>>(),
                    ["older question", "older answer"],
                    "the engine replays the session that was chosen"
                );
            }
            other => panic!("switching a session restores its history, got {other:?}"),
        }
        assert_eq!(
            chat.session_id, older,
            "the screen is on the chosen session"
        );
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("older question"),
            "the transcript is its history: {frame}"
        );
        assert!(
            !frame.contains("newer question"),
            "and not the other one: {frame}"
        );
    }

    /// The crate's theme is a process-wide global, so the tests that change it
    /// take this lock. Nothing else in this binary touches the global: every
    /// other test hands its chat its own palette.
    static THEME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn theme_lock() -> std::sync::MutexGuard<'static, ()> {
        THEME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The palettes this build carries, in the picker's own shape.
    fn theme_frame(chat: &mut Chat) -> String {
        frame_at(chat, 80, 20)
    }

    /// `/theme` lists every palette the build carries, and typing narrows it.
    #[test]
    fn the_theme_picker_lists_every_palette_and_filters_by_name() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let all = crate::themes::theme_names();
        assert!(all.len() > 50, "the registry is the list: {}", all.len());

        type_text(&mut chat, "/theme");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none(), "opening a picker runs nothing");
        let frame = theme_frame(&mut chat);
        assert!(frame.contains("themes ·"), "{frame}");
        assert!(
            frame.contains("auto  ✓ current"),
            "with nothing chosen, the mark is on the probe's own pick: {frame}"
        );
        // The window shows a slice of a hundred rows; the list behind it is the
        // whole registry, with `auto` in front of it.
        let picker = chat.theme_picker.as_ref().expect("open");
        assert_eq!(
            picker.names.len(),
            all.len() + 1,
            "every palette this build carries is a row"
        );
        for name in ["titanium", "alabaster", "dark-gruvbox"] {
            assert!(
                picker.names.iter().any(|row| row == name),
                "{name} is missing"
            );
        }

        // A query brings one into the window; esc clears it without closing,
        // the way the model browser's does.
        type_text(&mut chat, "titan");
        let frame = theme_frame(&mut chat);
        assert!(
            frame.contains("titanium"),
            "the query brings a preset up: {frame}"
        );
        assert!(
            frame.contains("themes · 1 of 101 · titan"),
            "and the title says what it is showing: {frame}"
        );
        chat.on_key(Key::Esc, Instant::now());
        assert!(
            chat.theme_picker.is_some(),
            "esc clears the query before it closes the picker"
        );

        type_text(&mut chat, "gruv");
        let frame = theme_frame(&mut chat);
        assert!(frame.contains("dark-gruvbox"), "{frame}");
        assert!(frame.contains("light-gruvbox"), "{frame}");
        assert!(
            !frame.contains("alabaster"),
            "a name the query drops is gone: {frame}"
        );
        assert!(
            frame.contains(" of "),
            "the title counts what it shows: {frame}"
        );

        // A query that matches nothing is refused, not applied.
        type_text(&mut chat, "zzzz");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none(), "nothing is applied");
        let last = chat.lines.last().expect("a line");
        assert_eq!(last.kind, LineKind::Error, "{:?}", chat.lines);
        assert!(last.text.contains("no theme matches"), "{}", last.text);
        assert!(
            chat.theme_picker.is_none(),
            "and the picker closed on the answer"
        );
    }

    /// Enter applies a palette to the very next frame, it is remembered, and
    /// Esc leaves the one on screen alone — including a row the cursor was
    /// arrowed past.
    #[test]
    fn enter_applies_a_theme_and_esc_keeps_the_one_on_screen() {
        let _guard = theme_lock();
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let workspace = crate::session_fs::current_workspace();
        chat.theme = crate::themes::theme_for(dir.path(), &workspace, None).expect("a theme");
        let before_frame = theme_frame(&mut chat);
        let before_bg = frame_buffer(&mut chat, 80, 20)[(0, 0)].bg;

        // Open, walk past rows, leave: nothing about the screen changes.
        type_text(&mut chat, "/theme");
        chat.on_key(Key::Enter, Instant::now());
        chat.on_key(Key::Down, Instant::now());
        chat.on_key(Key::Down, Instant::now());
        chat.on_key(Key::Esc, Instant::now());
        assert_eq!(
            theme_frame(&mut chat),
            before_frame,
            "esc leaves the screen as it was"
        );
        assert_eq!(
            frame_buffer(&mut chat, 80, 20)[(0, 0)].bg,
            before_bg,
            "and the palette with it"
        );

        // `/theme <name>` applies one: the next frame is painted in it.
        let applied = chat.slash("/theme alabaster").expect("the command parses");
        assert!(applied.effect.is_none(), "a palette is a local change");
        let after_bg = frame_buffer(&mut chat, 80, 20)[(0, 0)].bg;
        assert_ne!(
            after_bg, before_bg,
            "the frame is painted in the new palette"
        );
        let frame = theme_frame(&mut chat);
        assert!(frame.contains("theme alabaster"), "and it says so: {frame}");
        let expected = titi_tui::theme::loader::load_theme("alabaster", &theme_options())
            .expect("alabaster is a preset of this build");
        assert_eq!(
            after_bg,
            rgb(&expected.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg)),
            "and the palette is that preset's, cell for cell"
        );

        // Remembered for the appearance slot the terminal reports, and read
        // back at startup.
        let settings =
            titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
        let key =
            crate::themes::theme_slot(&titi_tui::theme::appearance::AppearanceInputs::from_env());
        assert_eq!(
            settings
                .get(key)
                .and_then(|value| value.as_str().map(str::to_owned)),
            Some("alabaster".to_owned()),
            "the choice is remembered in {key}"
        );
        let reloaded = crate::themes::theme_for(dir.path(), &workspace, None).expect("a theme");
        assert_eq!(
            reloaded.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            expected.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            "and the next run starts on it"
        );

        // `auto` is the absence of a choice, not a palette.
        chat.slash("/theme auto").expect("the command parses");
        let settings =
            titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
        assert_eq!(settings.get(key), None, "auto clears the slot");
        let auto = theme_frame(&mut chat);
        assert!(auto.contains("(auto)"), "{auto}");
    }

    /// A closing `:` expands a known shortcode and the caret lands on the
    /// glyph's far side; an unknown name stays exactly as it was typed.
    #[test]
    fn a_closing_colon_expands_the_shortcode_and_the_caret_follows_it() {
        {
            let mut chat = chat();
            type_text(&mut chat, ":tada:");
            let frame = frame_text(&mut chat);
            assert!(frame.contains('🎉'), "{frame}");
            assert!(
                !frame.contains(":tada:"),
                "the keystrokes are gone, the glyph is not: {frame}"
            );
            assert!(
                frame.contains("🎉▍"),
                "the caret sits directly after the glyph: {frame}"
            );
            assert!(!chat.emoji_picker.is_visible());
        }

        // Unknown names stay literal, and nothing was expanded for them.
        {
            let mut chat = chat();
            type_text(&mut chat, ":nope:");
            let frame = frame_text(&mut chat);
            assert!(frame.contains(":nope:▍"), "{frame}");
            assert!(
                !frame.contains("emoji ·"),
                "a name with no match opens no picker: {frame}"
            );
        }
    }

    /// A terminating space expands an emoticon, and so does Enter; a fenced
    /// block keeps the keystrokes, and a URL's colon is never a shortcode.
    #[test]
    fn a_terminator_expands_an_emoticon_and_a_fence_or_url_keeps_the_text() {
        {
            let mut chat = chat();
            type_text(&mut chat, ":-)");
            type_text(&mut chat, " ");
            let frame = frame_text(&mut chat);
            assert!(
                frame.contains("🙂 ▍"),
                "space replaced :-) and is kept before the caret: {frame}"
            );
            assert!(!frame.contains(":-)"), "{frame}");
        }

        // Enter is the other terminator: the sent line holds the glyph, not
        // the keystrokes.
        {
            let mut chat = chat();
            type_text(&mut chat, "<3");
            chat.on_key(Key::Enter, Instant::now());
            assert!(
                chat.lines.iter().any(|line| line.text == "❤️"),
                "the sent line holds the glyph: {:?}",
                chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
            );
            let frame = frame_text(&mut chat);
            assert!(frame.contains("❤️"), "{frame}");
        }

        // A fence is code-like: the text is shown, not expanded. A paste is
        // how a fence gets into the composer (Enter would send the line).
        {
            let mut chat = chat();
            chat.paste("```\n:-)");
            type_text(&mut chat, " ");
            let frame = frame_text(&mut chat);
            assert!(frame.contains(":-)"), "a fence keeps it: {frame}");
            assert!(!frame.contains("🙂"), "{frame}");
        }

        // The word-like character before the opening colon keeps a URL whole.
        {
            let mut chat = chat();
            type_text(&mut chat, "http://x:y:");
            let frame = frame_text(&mut chat);
            assert!(frame.contains("http://x:y:"), "{frame}");
        }
    }

    /// `:xx` opens the picker with the matching rows; Tab takes the
    /// highlighted glyph and consumes the query, Esc closes and leaves the
    /// text exactly as it was typed.
    #[test]
    fn a_trailing_query_opens_the_picker_and_tab_takes_a_row() {
        {
            let mut chat = chat();
            type_text(&mut chat, ":sm");
            let frame = frame_text(&mut chat);
            assert!(frame.contains("emoji ·"), "the picker is up: {frame}");
            assert!(frame.contains("smiley"), "{frame}");
            assert!(frame.contains("smirk"), "{frame}");
            assert!(frame.contains("🙂"), "a row carries its glyph: {frame}");

            // Tab takes the highlighted row — `smiley`, the first match — and
            // the `:sm` is gone.
            chat.on_key(Key::Tab, Instant::now());
            let frame = frame_text(&mut chat);
            assert!(
                frame.contains("😊"),
                "tab took the highlighted row: {frame}"
            );
            assert!(!frame.contains(":sm"), "the query is consumed: {frame}");
            assert!(!frame.contains("emoji ·"), "and the picker closed: {frame}");
            assert!(!chat.emoji_picker.is_visible());
        }

        // Backspace takes the query back and the picker follows it, the way
        // the model browser's does.
        {
            let mut chat = chat();
            type_text(&mut chat, ":smi");
            assert!(frame_text(&mut chat).contains("emoji ·"));
            chat.on_key(Key::Backspace, Instant::now());
            let frame = frame_text(&mut chat);
            assert!(frame.contains(":sm▍"), "{frame}");
            assert!(
                frame.contains("emoji ·"),
                "the query still stands, so the picker stays: {frame}"
            );
        }

        // Esc closes and leaves the text alone — unlike the slash list, whose
        // Esc clears the composer.
        {
            let mut chat = chat();
            type_text(&mut chat, ":sm");
            chat.on_key(Key::Esc, Instant::now());
            let frame = frame_text(&mut chat);
            assert!(frame.contains(":sm▍"), "esc leaves the text: {frame}");
            assert!(!frame.contains("emoji ·"), "{frame}");
            assert!(!chat.emoji_picker.is_visible());
        }
    }

    /// While the emoji picker is up Enter belongs to it — the line is not
    /// sent. The slash list's own Enter is untouched, and the two panels
    /// never show at once.
    #[test]
    fn the_emoji_picker_owns_enter_and_the_slash_list_keeps_its_own() {
        {
            let mut chat = chat();
            type_text(&mut chat, ":sm");
            chat.on_key(Key::Enter, Instant::now());
            assert!(
                chat.lines.is_empty() && !chat.turn_active,
                "enter took the row instead of sending: {:?}",
                chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
            );
            let frame = frame_text(&mut chat);
            assert!(frame.contains("😊"), "{frame}");
            assert!(!frame.contains("emoji ·"), "{frame}");
        }

        // A `/` trigger opens the slash list, not the emoji picker, and its
        // Enter is the list's: `/he` completes and `/help` runs.
        {
            let mut chat = chat();
            type_text(&mut chat, "/he");
            assert!(
                !chat.emoji_picker.is_visible(),
                "a slash trigger does not open the emoji picker"
            );
            let frame = frame_text(&mut chat);
            assert!(
                frame.contains("/help"),
                "the slash list owns the panel: {frame}"
            );
            assert!(!frame.contains("emoji ·"), "{frame}");
            chat.on_key(Key::Enter, Instant::now());
            let frame = frame_text(&mut chat);
            assert!(
                frame.contains("ask titi…"),
                "enter sent the completed command, so the composer is empty: {frame}"
            );
        }
    }

    /// An expansion — and the picker showing a glyph — leave every row exactly
    /// the pane's width: a glyph is two cells, measured with the crate's
    /// width model and never with `chars().count()`.
    #[test]
    fn a_frame_after_an_expansion_still_fits_the_pane() {
        {
            let mut chat = chat();
            type_text(&mut chat, "ship it :tada:");
            assert!(frame_text(&mut chat).contains("🎉"));
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
            }
        }

        // The picker's own rows carry glyphs too; they fit just the same.
        {
            let mut chat = chat();
            type_text(&mut chat, ":sm");
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
            }
        }
    }

    /// A theme this build does not carry is refused by name, never silently
    /// replaced by another palette, and the refusal names what there is.
    #[test]
    fn an_unknown_theme_is_refused_by_name() {
        let reason = crate::themes::theme_named("nope").expect_err("nope is not a theme");
        assert!(reason.contains("unknown theme nope"), "{reason}");
        assert!(
            reason.contains(&crate::themes::theme_names().len().to_string()),
            "the refusal counts what exists: {reason}"
        );
        assert!(
            reason.contains("/theme"),
            "and says where the list is: {reason}"
        );

        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.slash("/theme nope").expect("the command parses");
        let last = chat.lines.last().expect("a line");
        assert_eq!(last.kind, LineKind::Error, "{:?}", chat.lines);
        assert!(last.text.contains("unknown theme nope"), "{}", last.text);
    }

    /// With nothing set, the screen resolves exactly as it did before there was
    /// a setting: the crate's own pick for the appearance the terminal reports.
    #[test]
    fn auto_is_the_absence_of_a_choice() {
        let _guard = theme_lock();
        let dir = tempfile::tempdir().expect("temp");
        let workspace = crate::session_fs::current_workspace();
        let inputs = titi_tui::theme::appearance::AppearanceInputs::from_env();
        let picked = crate::themes::theme_for(dir.path(), &workspace, None).expect("auto");
        let expected = titi_tui::theme::loader::load_theme(
            &titi_tui::theme::appearance::resolve_auto_theme(
                titi_tui::theme::appearance::AUTO_DARK_THEME,
                titi_tui::theme::appearance::AUTO_LIGHT_THEME,
                &inputs,
            ),
            &theme_options(),
        )
        .expect("the crate's own pick loads");
        assert_eq!(
            picked.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            expected.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            "auto is the crate's pick, not a new default"
        );

        // Both slots set: whichever appearance the terminal reports, that name
        // is what the screen shows.
        let mut settings =
            titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
        settings
            .set(
                titi_config::settings::THEME_DARK_KEY,
                serde_json::json!("alabaster"),
            )
            .expect("write");
        settings
            .set(
                titi_config::settings::THEME_LIGHT_KEY,
                serde_json::json!("alabaster"),
            )
            .expect("write");
        let chosen = crate::themes::theme_for(dir.path(), &workspace, None).expect("chosen");
        let alabaster =
            titi_tui::theme::loader::load_theme("alabaster", &theme_options()).expect("alabaster");
        assert_eq!(
            chosen.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            alabaster.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            "the setting is read at startup"
        );

        // `--theme` wins over the setting for one run.
        let forced = crate::themes::theme_for(dir.path(), &workspace, Some("titanium"))
            .expect("a named theme");
        let titanium =
            titi_tui::theme::loader::load_theme("titanium", &theme_options()).expect("titanium");
        assert_eq!(
            forced.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            titanium.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
            "the flag overrides the setting"
        );
        assert!(
            crate::themes::theme_for(dir.path(), &workspace, Some("nope")).is_err(),
            "and an unknown name is refused"
        );
    }

    /// The default the rest of the CLI builds its theme with.
    fn theme_options() -> titi_tui::theme::loader::CreateThemeOptions {
        titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        }
    }

    /// A chat on a temp agent directory with one model and a key stored for it:
    /// the facts the welcome reads are the test's own, not this machine's.
    fn welcome_chat() -> (tempfile::TempDir, Chat) {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec!["openai/gpt-4.1".to_owned()]);
        crate::secrets::store_key(&chat.agent_dir, "openai", "sk-test").expect("store a key");
        (dir, chat)
    }

    /// Two stored sessions with a message each, so the welcome has a list of
    /// recent ones to offer.
    fn with_two_sessions(chat: &Chat) {
        let store = titi_core::session::SessionStore::new(&chat.agent_dir).expect("session store");
        for title in ["one", "two"] {
            let id = store
                .create(titi_core::session::SessionMeta {
                    title: Some(title.to_owned()),
                    source: Some("cli".to_owned()),
                    ..Default::default()
                })
                .expect("create");
            store.append(&id, Role::User, title).expect("append");
        }
    }

    /// A frame as one string at `width` x `height`, for a test that is about
    /// which rows are on screen rather than about a row's cells.
    fn frame_at(chat: &mut Chat, width: u16, height: u16) -> String {
        frame_rows(chat, width, height).join("\n")
    }

    /// The rows between the masthead and the composer: where the welcome is
    /// drawn on an idle screen with no panel open.
    fn welcome_area(rows: &[String]) -> &[String] {
        rows.get(1..rows.len().saturating_sub(4))
            .unwrap_or_default()
    }

    /// The welcome's rows of a frame at `width` x `height`, as one string.
    fn welcome_at(chat: &mut Chat, width: u16, height: u16) -> String {
        welcome_area(&frame_rows(chat, width, height)).join("\n")
    }

    /// Whether a fact row carrying `label` is on screen: a label opens its
    /// row, so the masthead's model id or the composer's `/model` does not
    /// count as the welcome's model row.
    fn states_fact(welcome: &str, label: &str) -> bool {
        welcome
            .lines()
            .any(|row| row.trim_start().starts_with(&format!("{label} ")))
    }

    /// Whether there is a blank row between the welcome's first and last rows.
    fn spaced(welcome: &str) -> bool {
        let rows: Vec<&str> = welcome.lines().collect();
        let first = rows.iter().position(|row| !row.trim().is_empty());
        let last = rows.iter().rposition(|row| !row.trim().is_empty());
        match (first, last) {
            (Some(first), Some(last)) => rows[first..=last].iter().any(|row| row.trim().is_empty()),
            _ => false,
        }
    }

    /// What the welcome states for the directory: the path the snapshot gives,
    /// or its leaf behind the ellipsis a row shortens a long value to.
    fn states_path(frame: &str, path: &str) -> bool {
        frame.contains(path)
            || (frame.contains('…')
                && !path.is_empty()
                && frame.contains(path.rsplit('/').next().unwrap_or(path)))
    }

    /// A branch is on screen whole or not at all: four cells of one is not a
    /// branch, whatever the row's width happened to leave.
    fn states_branch_whole(frame: &str, branch: &str) -> bool {
        frame.contains(branch) || !frame.contains(&branch.chars().take(4).collect::<String>())
    }

    /// The text of the fact row carrying `label`.
    fn row_with_label(body: &[Line<'static>], label: &str) -> String {
        body.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .find(|row| row.contains(label))
            .unwrap_or_else(|| panic!("no {label} row in {body:?}"))
    }

    /// The cells the welcome draws something on, as symbol and foreground.
    fn welcome_cells(chat: &mut Chat, width: u16, height: u16) -> Vec<(String, Color)> {
        let buffer = frame_buffer(chat, width, height);
        (1..height.saturating_sub(4))
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| (buffer[(x, y)].symbol().to_owned(), buffer[(x, y)].fg))
            .filter(|(symbol, _)| !symbol.trim().is_empty())
            .collect()
    }

    /// A colour's gray level, or `None` when its channels differ: a colour
    /// with a hue in it is not black and white.
    fn gray_level(color: Color) -> Option<u8> {
        match color {
            Color::Rgb(r, g, b) if r == g && g == b => Some(r),
            _ => None,
        }
    }

    /// BT.601 luma of a colour, 0 to 255.
    fn luma(color: Color) -> f64 {
        match color {
            Color::Rgb(r, g, b) => {
                0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
            }
            other => panic!("not an RGB colour: {other:?}"),
        }
    }

    /// The first screen states the build, the model behind the next turn, what
    /// stands behind that model, and the directory — each read from the source
    /// that owns it — under the TITI mark, the whole lockup centred in the pane
    /// with no box around it.
    #[test]
    fn the_welcome_names_the_build_the_model_and_the_directory() {
        let (_dir, mut chat) = welcome_chat();
        let snapshot = masthead_snapshot(&chat);
        let build = format!("v{}", titi_tui::VERSION);
        for width in [60u16, 80, 120] {
            let rows = frame_rows(&mut chat, width, 20);
            let area = welcome_area(&rows);
            let frame = area.join("\n");
            assert!(
                frame.contains("say what you want done"),
                "{width}: and what to do: {frame}"
            );
            assert!(
                frame.contains(&chat.model),
                "{width}: the model that will answer: {frame}"
            );
            assert!(
                frame.contains("·  key"),
                "{width}: with the credential /keys reads for it: {frame}"
            );
            assert!(
                states_path(&frame, &snapshot.path),
                "{width}: the directory the masthead reads: {frame}"
            );
            if let Some(branch) = &snapshot.git_branch {
                assert!(
                    states_branch_whole(&frame, branch),
                    "{width}: and its git state, whole or not at all: {frame}"
                );
            }
            for expected in ["enter  send", "alt+m  models", "ctrl-c  quit"] {
                assert!(
                    frame.contains(expected),
                    "{width}: the chords that work are advertised: {expected} missing from {frame}"
                );
            }
            // The lockup: the mark with the build beside it, centred as one
            // block, and nothing drawn around it.
            let top = area
                .iter()
                .position(|row| row.contains("██████ ██████"))
                .unwrap_or_else(|| panic!("{width}: no mark: {frame}"));
            let lockup = &area[top..top + 5];
            assert!(
                lockup.iter().any(|row| row.contains(&build)),
                "{width}: the build is named beside the mark: {frame}"
            );
            let margin = |row: &String, trimmed: &str| {
                titi_tui::width::visible_width(row) - titi_tui::width::visible_width(trimmed)
            };
            let left = lockup
                .iter()
                .map(|row| margin(row, row.trim_start()))
                .min()
                .unwrap_or_default();
            let right = lockup
                .iter()
                .map(|row| margin(row, row.trim_end()))
                .min()
                .unwrap_or_default();
            assert!(
                left.abs_diff(right) <= 1,
                "{width}: the lockup is centred ({left} | {right}): {lockup:#?}"
            );
            for corner in ['╭', '╰', '│'] {
                assert!(!frame.contains(corner), "{width}: no box is drawn: {frame}");
            }
            for row in &rows {
                assert_eq!(
                    titi_tui::width::visible_width(row),
                    width as usize,
                    "{width}: {row:?}"
                );
            }
        }
    }

    /// The welcome is black and white on every palette: every cell it draws
    /// is a gray, and the mark stands off the page the way ink does — lighter
    /// than a dark page, darker than a light one — rather than in whatever hue
    /// a theme's accent or its git colours happen to be.
    #[test]
    fn the_welcome_is_black_and_white_on_every_palette() {
        let (mut dark, mut light) = (0, 0);
        for name in titi_tui::theme::builtin::list_builtin_themes() {
            let (_dir, mut chat) = welcome_chat();
            chat.theme = test_theme_named(name);
            let page = luma(bg(&chat.theme, ThemeBg::StatusLineBg));
            let cells = welcome_cells(&mut chat, 80, 24);
            for (symbol, color) in &cells {
                assert!(
                    gray_level(*color).is_some(),
                    "{name}: {symbol:?} is drawn in {color:?}, which is not a gray"
                );
            }
            let mark: Vec<f64> = cells
                .iter()
                .filter(|(symbol, _)| symbol == "█")
                .filter_map(|(_, color)| gray_level(*color).map(f64::from))
                .collect();
            assert!(!mark.is_empty(), "{name}: the mark is drawn");
            if page < 127.5 {
                let brightest = mark.iter().copied().fold(0.0, f64::max);
                assert!(
                    brightest > page,
                    "{name}: the mark ({brightest}) is lighter than a dark page ({page})"
                );
                dark += 1;
            } else {
                let darkest = mark.iter().copied().fold(255.0, f64::min);
                assert!(
                    darkest < page,
                    "{name}: the mark ({darkest}) is darker than a light page ({page})"
                );
                light += 1;
            }
        }
        assert!(
            dark > 0 && light > 0,
            "the presets hold dark pages ({dark}) and light ones ({light})"
        );
        for (name, page_is_dark) in [("titanium", true), ("alabaster", false)] {
            let page = luma(bg(&test_theme_named(name), ThemeBg::StatusLineBg));
            assert_eq!(page < 127.5, page_is_dark, "{name}: {page}");
        }
    }

    /// The facts are one block: left-aligned under one another, and the block
    /// centred in the pane as a whole, so the values read down one column
    /// instead of each row drifting to its own centre.
    #[test]
    fn the_welcome_facts_are_one_left_aligned_block() {
        let (_dir, mut chat) = welcome_chat();
        with_two_sessions(&chat);
        for width in [60u16, 80, 120] {
            let rows = frame_rows(&mut chat, width, 24);
            let area = welcome_area(&rows);
            let model = area
                .iter()
                .position(|row| row.trim_start().starts_with("model "))
                .unwrap_or_else(|| panic!("{width}: no model row: {area:#?}"));
            // The model, the directory, and the two sessions.
            let block = &area[model..model + 4];
            let indent = |row: &String| {
                titi_tui::width::visible_width(row)
                    - titi_tui::width::visible_width(row.trim_start())
            };
            let left = indent(&block[0]);
            assert!(
                block[..3].iter().all(|row| indent(row) == left),
                "{width}: every label starts in one column: {block:#?}"
            );
            assert_eq!(
                indent(&block[3]),
                left + WELCOME_LABEL,
                "{width}: and a second session sits under the first: {block:#?}"
            );
            assert!(
                block[1].trim_start().starts_with("dir ")
                    && block[2].trim_start().starts_with("recent "),
                "{width}: {block:#?}"
            );
            let widest = block
                .iter()
                .map(|row| titi_tui::width::visible_width(row.trim_end()) - left)
                .max()
                .unwrap_or_default();
            let right = width as usize - left - widest;
            assert!(
                left.abs_diff(right) <= 1,
                "{width}: the block is centred ({left} | {right}): {block:#?}"
            );
        }
    }

    /// A fact's tail is stated whole or dropped whole. A detached HEAD's short
    /// sha, cut where the row happens to end, reads as whatever cells fitted —
    /// so the welcome gives the git state up rather than print half of it, the way
    /// the masthead gives it up when the pane is narrow. A value too long for
    /// its row keeps its informative end behind an ellipsis, never a bare cut.
    #[test]
    fn the_welcome_states_a_fact_whole_or_not_at_all() {
        let grays = WelcomeGrays::of(&test_theme());
        let facts = WelcomeFacts {
            version: "0.0.0",
            model: "opencode-go/glm-5.3-flash".to_owned(),
            credential: Some("key".to_owned()),
            path: "/tmp/titi".to_owned(),
            git: Some(WelcomeGit {
                branch: "636c207".to_owned(),
                unstaged: 2,
                staged: 0,
                untracked: 0,
            }),
            sessions: Vec::new(),
        };
        let room = 59;

        let wide = welcome_fact_rows(&facts, 0, room, &grays);
        let row = row_with_label(&wide, "dir");
        assert!(
            row.contains("/tmp/titi") && row.contains("636c207") && row.contains("*2"),
            "a row with room states the fact and its tail: {row}"
        );
        let row = row_with_label(&wide, "model");
        assert!(
            row.contains("opencode-go/glm-5.3-flash") && row.contains("key"),
            "{row}"
        );

        // No room for the branch: it goes, the directory stays.
        let long_path = WelcomeFacts {
            path: format!("/{}", "deep/".repeat(12)),
            ..facts.clone()
        };
        let row = row_with_label(&welcome_fact_rows(&long_path, 0, room, &grays), "dir");
        assert!(
            !row.contains("636c207"),
            "the branch is gone, not cut: {row}"
        );
        assert!(!row.contains("636c"), "and no part of it is left: {row}");
        assert!(
            row.contains('…'),
            "the path itself is shortened honestly: {row}"
        );
        assert!(row.contains("deep"), "to something still readable: {row}");

        // Same for the credential word.
        let long_model = WelcomeFacts {
            model: "x".repeat(room),
            ..facts.clone()
        };
        let row = row_with_label(&welcome_fact_rows(&long_model, 0, room, &grays), "model");
        assert!(
            !row.contains("key"),
            "the credential word is not cut either: {row}"
        );
        assert!(
            row.contains('…'),
            "and the model keeps its informative end: {row}"
        );

        // The boundary is exact: the row is `room` cells, the label 9, and this
        // git tail is 15 (`  ·  ` + `636c207` + ` *2`).
        let fits = WelcomeFacts {
            path: "p".repeat(35),
            ..facts.clone()
        };
        let row = row_with_label(&welcome_fact_rows(&fits, 0, room, &grays), "dir");
        assert!(
            row.contains("636c207") && row.contains("*2"),
            "a tail that fits to the cell is stated: {row}"
        );
        let over = WelcomeFacts {
            path: "p".repeat(36),
            ..facts.clone()
        };
        let row = row_with_label(&welcome_fact_rows(&over, 0, room, &grays), "dir");
        assert!(!row.contains("636c207"), "and one cell over is not: {row}");

        // The session on screen keeps its mark whole; its name gives the room up.
        let long_session = WelcomeFacts {
            sessions: vec![("s".repeat(room), true)],
            ..facts.clone()
        };
        let row = row_with_label(&welcome_fact_rows(&long_session, 0, room, &grays), "recent");
        assert!(
            row.ends_with("✓ current") && row.contains('…'),
            "the mark is whole and the name is shortened: {row}"
        );
        assert_eq!(titi_tui::width::visible_width(&row), room, "{row}");
    }

    /// A session is named on the welcome the way the switcher names it.
    #[test]
    fn the_welcome_names_a_session_as_the_switcher_does() {
        let (_dir, mut chat) = welcome_chat();
        let store = titi_core::session::SessionStore::new(&chat.agent_dir).expect("session store");
        let named = store
            .create(titi_core::session::SessionMeta {
                title: Some("named".to_owned()),
                source: Some("cli".to_owned()),
                ..Default::default()
            })
            .expect("create");
        chat.session_id = named.clone();
        let frame = welcome_at(&mut chat, 80, 20);
        assert!(
            frame.contains(&session_row_text(&named, true)),
            "the welcome marks the session on screen the way the switcher does: {frame}"
        );
    }

    /// The credential word is the model picker's, not a second vocabulary.
    #[test]
    fn the_welcome_states_a_subscription_as_oauth() {
        let (_dir, mut chat) = welcome_chat();
        crate::secrets::remove_key(&chat.agent_dir, "openai").expect("remove the key");
        crate::secrets::store_oauth(
            &chat.agent_dir,
            "openai",
            &titi_providers::oauth::OAuthTokens {
                access: "sk-test".to_owned(),
                refresh: Some("sk-test".to_owned()),
                expires_at: None,
                account_id: None,
                email: None,
                org_id: None,
                org_name: None,
            },
        )
        .expect("store a sign-in");
        let frame = welcome_at(&mut chat, 80, 20);
        assert!(
            frame.contains("·  oauth"),
            "a subscription reads as oauth: {frame}"
        );
        assert!(!frame.contains("·  key"), "and not as a key: {frame}");
    }

    /// The welcome is an empty state: one transcript line takes the screen,
    /// and emptying the transcript brings it back — a rewind to nothing, or a
    /// switch to a session with no history.
    #[test]
    fn the_welcome_yields_the_screen_to_a_line() {
        let (_dir, mut chat) = welcome_chat();
        assert!(frame_at(&mut chat, 80, 20).contains("say what you want done"));

        chat.push(LineKind::User, "hello".to_owned());
        let frame = frame_at(&mut chat, 80, 20);
        assert!(
            !frame.contains("say what you want done") && !frame.contains("██████"),
            "one line is enough to take the screen: {frame}"
        );
        assert!(frame.contains("hello"), "{frame}");

        chat.show_history(&[]);
        let frame = frame_at(&mut chat, 80, 20);
        assert!(
            frame.contains("say what you want done") && frame.contains("██████ ██████"),
            "and an empty transcript brings it back: {frame}"
        );
    }

    /// A session with no history yet — a fresh agent directory — has no list to
    /// offer, so the welcome states the facts it does have and no empty heading.
    #[test]
    fn a_session_with_no_history_shows_no_list() {
        let (_dir, mut chat) = welcome_chat();
        assert!(
            chat.session_choices().is_empty(),
            "a fresh directory has no sessions to list"
        );
        let frame = welcome_at(&mut chat, 80, 20);
        assert!(
            !states_fact(&frame, "recent"),
            "no list is offered: {frame}"
        );
        assert!(states_fact(&frame, "model"), "but the model is: {frame}");
        assert!(states_fact(&frame, "dir"), "and the directory: {frame}");
    }

    /// A short pane gives the welcome down in one order — the tip, the recent
    /// sessions, then the directory, then the tagline, then the blank rows,
    /// then the model — and the lockup with the chords outlasts all of them.
    /// When even those two do not fit, the brand and its build fold into one
    /// line.
    #[test]
    fn a_short_pane_gives_the_welcome_down_in_order() {
        let (_dir, mut chat) = welcome_chat();
        with_two_sessions(&chat);
        let build = format!("v{}", titi_tui::VERSION);
        let chords = |frame: &str| {
            frame.contains("enter  send")
                && frame.contains("alt+m  models")
                && frame.contains("ctrl-c  quit")
        };

        // The pane is the screen less the masthead and the composer's four
        // rows; each height below is the first that drops one more thing.
        let roomy = welcome_at(&mut chat, 80, 21);
        assert!(roomy.contains("Tip: "), "a roomy pane has a tip: {roomy}");

        let no_tip = welcome_at(&mut chat, 80, 19);
        assert!(!no_tip.contains("Tip: "), "the tip goes first: {no_tip}");
        for label in ["recent", "dir", "model"] {
            assert!(
                states_fact(&no_tip, label),
                "the facts are all there: {label} missing from {no_tip}"
            );
        }
        assert!(no_tip.contains("say what you want done"), "{no_tip}");

        let no_sessions = welcome_at(&mut chat, 80, 17);
        assert!(
            !states_fact(&no_sessions, "recent"),
            "then the list: {no_sessions}"
        );
        assert!(
            states_fact(&no_sessions, "dir"),
            "the directory is still there: {no_sessions}"
        );

        let no_directory = welcome_at(&mut chat, 80, 16);
        assert!(
            !states_fact(&no_directory, "dir"),
            "then the directory: {no_directory}"
        );
        assert!(
            no_directory.contains("say what you want done"),
            "the tagline is still there: {no_directory}"
        );

        let no_tagline = welcome_at(&mut chat, 80, 14);
        assert!(
            !no_tagline.contains("say what you want done"),
            "then the tagline: {no_tagline}"
        );
        assert!(
            states_fact(&no_tagline, "model") && spaced(&no_tagline),
            "the model and the blank rows around it are still there: {no_tagline}"
        );

        let tight = welcome_at(&mut chat, 80, 12);
        assert!(!spaced(&tight), "then the blank rows: {tight}");
        assert!(
            states_fact(&tight, "model") && chords(&tight),
            "which keeps the model with the chords: {tight}"
        );

        let bare = welcome_at(&mut chat, 80, 11);
        assert!(!states_fact(&bare, "model"), "then the model: {bare}");
        assert!(
            bare.contains("██████ ██████") && bare.contains(&build) && chords(&bare),
            "the lockup with the build, and the chords, outlast it: {bare}"
        );

        let brand = welcome_at(&mut chat, 80, 10);
        assert!(
            !brand.contains("██████"),
            "a pane with no room for the mark: {brand}"
        );
        assert!(
            brand.contains(&format!("titi {build}")) && chords(&brand),
            "states the brand and its build in one line, over the chords: {brand}"
        );

        // So does a pane too narrow for the mark, however tall it is.
        let narrow = welcome_at(&mut chat, 26, 24);
        assert!(
            !narrow.contains("██████") && narrow.contains(&format!("titi {build}")),
            "{narrow}"
        );
        // Its chords are the ones that fit, each one whole.
        assert!(narrow.contains("enter  send"), "{narrow}");
        for (key, chord) in [("alt+m", "alt+m  models"), ("ctrl-c", "ctrl-c  quit")] {
            assert!(
                !narrow.contains(key) || narrow.contains(chord),
                "{chord} is whole or absent: {narrow}"
            );
        }
    }

    /// The welcome offers one tip, and a true one: every command a tip names
    /// is one this build runs, and every tip fits whole on the narrowest pane
    /// that shows one. The tip is picked from the session, so a redraw keeps it
    /// instead of flickering to another, and a pane too narrow for it shows
    /// none rather than a sentence cut short.
    #[test]
    fn the_welcome_offers_one_true_tip_and_keeps_it() {
        for tip in WELCOME_TIPS {
            for word in tip.split_whitespace().filter(|word| word.starts_with('/')) {
                assert!(
                    COMMANDS
                        .iter()
                        .any(|command| format!("/{}", command.name) == word),
                    "{tip}: {word} is a command"
                );
            }
            assert!(
                titi_tui::width::visible_width(&format!("Tip: {tip}")) + 2 <= WELCOME_TIP_COLUMNS,
                "{tip} fits whole at {WELCOME_TIP_COLUMNS} columns"
            );
        }

        let (_dir, mut chat) = welcome_chat();
        let tip_of = |frame: &str| {
            frame
                .lines()
                .find_map(|row| row.trim().strip_prefix("Tip: ").map(str::to_owned))
        };
        let first = welcome_at(&mut chat, 80, 24);
        let tip = tip_of(&first).unwrap_or_else(|| panic!("a tip is offered: {first}"));
        assert!(
            WELCOME_TIPS.contains(&tip.as_str()),
            "{tip:?} is one of the listed tips"
        );
        assert_eq!(
            tip_of(&welcome_at(&mut chat, 80, 24)),
            Some(tip.clone()),
            "and the next frame offers the same one"
        );
        let rows = frame_rows(&mut chat, 80, 24);
        let (x, y) = cell_of(&rows, "Tip: ").unwrap_or_else(|| panic!("{rows:#?}"));
        assert!(
            frame_buffer(&mut chat, 80, 24)[(x, y)]
                .modifier
                .contains(Modifier::ITALIC),
            "the tip is set in italic"
        );

        let narrow = WELCOME_TIP_COLUMNS as u16;
        assert!(welcome_at(&mut chat, narrow, 24).contains("Tip: "));
        let narrower = welcome_at(&mut chat, narrow - 1, 24);
        assert!(!narrower.contains("Tip:"), "{narrower}");

        // Another session may offer another tip: the pick follows the session.
        let picked: HashSet<&str> = (0..64)
            .filter_map(|n| welcome_tip(&format!("session-{n}")))
            .collect();
        assert!(picked.len() > 1, "{picked:?}");
    }

    /// The welcome holds still until the live screen starts its intro: two
    /// frames of a chat that never started one are the same cells, and an
    /// intro that has run its course leaves exactly that resting frame.
    #[test]
    fn the_welcome_rests_unless_the_screen_starts_its_intro() {
        let (_dir, mut chat) = welcome_chat();
        let rest = frame_buffer(&mut chat, 80, 24);
        assert_eq!(
            frame_buffer(&mut chat, 80, 24),
            rest,
            "a chat that never started the intro draws one frame"
        );
        let long_ago = Instant::now()
            .checked_sub(WELCOME_INTRO * 2)
            .expect("the clock has run longer than two intros");
        chat.start_intro(long_ago);
        assert_eq!(
            frame_buffer(&mut chat, 80, 24),
            rest,
            "and a finished intro settles on it"
        );
    }

    /// The intro sweeps a shine across the mark, from the lit corner to the
    /// far one, quick at first and slowing into the end, and is gone by the
    /// time it has run: the band starts and ends off the mark, so its first
    /// and last frames are the resting one. Every cell it lights stays a gray,
    /// pushed toward the page's far end — whiter on a dark page, blacker on a
    /// light one.
    #[test]
    fn the_intro_sweeps_a_shine_across_the_mark() {
        assert!(
            (Duration::from_millis(1200)..=Duration::from_millis(1500)).contains(&WELCOME_INTRO),
            "{WELCOME_INTRO:?}"
        );
        let at = |millis: u64| welcome_shine(Duration::from_millis(millis));
        assert_eq!(welcome_shine(WELCOME_INTRO), None, "it is over in time");
        assert_eq!(welcome_shine(WELCOME_INTRO * 3), None, "and stays over");
        let steps: Vec<f64> = (0..WELCOME_INTRO.as_millis() as u64)
            .step_by(50)
            .map(|millis| at(millis).expect("still sweeping"))
            .collect();
        assert!(
            steps.windows(2).all(|pair| pair[0] < pair[1]),
            "it moves one way: {steps:?}"
        );
        let half = WELCOME_INTRO.as_millis() as u64 / 2;
        let (start, middle, end) = (
            steps[0],
            at(half).unwrap_or_default(),
            steps[steps.len() - 1],
        );
        assert!(
            middle - start > end - middle,
            "and eases out: {start} → {middle} → {end}"
        );

        for name in ["titanium", "alabaster"] {
            let grays = WelcomeGrays::of(&test_theme_named(name));
            let levels = |shine: Option<f64>| -> Vec<u8> {
                welcome_mark(&grays, shine)
                    .into_iter()
                    .flatten()
                    .filter(|span| span.content.trim() != "")
                    .map(|span| {
                        let color = span.style.fg.unwrap_or(Color::Reset);
                        gray_level(color)
                            .unwrap_or_else(|| panic!("{name}: {color:?} is not a gray"))
                    })
                    .collect()
            };
            let rest = levels(None);
            assert_eq!(
                levels(at(0)),
                rest,
                "{name}: the first frame is the resting one"
            );
            let lit = levels(at(250));
            assert_ne!(lit, rest, "{name}: the shine shows mid-sweep");
            let dark = grays.bright > 0.5;
            for (lit, rest) in lit.iter().zip(&rest) {
                assert!(
                    if dark { lit >= rest } else { lit <= rest },
                    "{name}: the shine moves a cell toward the page's far end ({rest} → {lit})"
                );
            }
        }
    }

    /// The picker above the composer is a box: the title sits inset in the top
    /// rule, and the cursor's row carries the theme's selection band as well as
    /// the marker, so the choice is legible on a terminal whose colours are dim.
    #[test]
    fn the_panel_is_a_titled_box_and_the_selected_row_carries_the_band() {
        for width in [60u16, 80, 120] {
            let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
            chat.catalog = crate::engine::ModelCatalog::fixed(vec![
                "openai/gpt-4.1".to_owned(),
                "anthropic/claude-opus-5".to_owned(),
            ]);
            type_text(&mut chat, "/model");
            chat.on_key(Key::Enter, Instant::now());

            let buffer = frame_buffer(&mut chat, width, 20);
            let theme = Arc::clone(&chat.theme);
            let rows: Vec<String> = (0..20)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            let (x, y) = cell_of(&rows, "╭─ models · 2")
                .unwrap_or_else(|| panic!("{width}: no titled rule: {rows:?}"));
            assert_eq!(x, 0, "{width}: the box starts at the left edge");
            assert!(
                rows[y as usize].trim_end().ends_with('╮'),
                "{width}: the rule closes: {:?}",
                rows[y as usize]
            );

            // The cursor's row: the marker, the theme's band across the row,
            // and the role's own colour on the label.
            let (mx, my) = cell_of(&rows, "▶ openai/gpt-4.1")
                .unwrap_or_else(|| panic!("{width}: the cursor row is unmarked: {rows:?}"));
            let band = bg(&theme, ThemeBg::SelectedBg);
            let page_bg = bg(&theme, ThemeBg::StatusLineBg);
            let rest: Vec<Style> = (2..width - 2).map(|x| buffer[(x, my)].style()).collect();
            for (at, style) in rest.iter().enumerate() {
                assert_eq!(
                    style.bg.unwrap_or(Color::Reset),
                    band,
                    "{width}: column {at} of the cursor row is outside the band"
                );
            }
            assert_eq!(
                buffer[(mx, my)].fg,
                fg(&theme, ThemeColor::CustomMessageLabel)
                    .fg
                    .unwrap_or(Color::Reset),
                "{width}: the cursor row keeps its role's colour"
            );

            // A row the cursor is not on carries neither the marker nor a band,
            // and the box closes under the last row.
            let (x, y) = cell_of(&rows, "anthropic/claude-opus-5")
                .unwrap_or_else(|| panic!("{width}: the other row is missing: {rows:?}"));
            assert_eq!(
                buffer[(x, y)].bg,
                page_bg,
                "{width}: a row the cursor is not on stays unbanded"
            );
            assert!(
                rows[(y + 1) as usize].starts_with('╰'),
                "{width}: the box closes above the composer: {:?}",
                rows[(y + 1) as usize]
            );
        }
    }

    /// The bar beside the panel body is the crate's scrollbar: it takes the
    /// pane's last column only when the list does not fit, and its thumb follows
    /// the window as the cursor walks the list.
    #[test]
    fn the_panel_bar_appears_only_when_the_list_overflows_and_tracks_the_selection() {
        let width = 80u16;
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        let theme = Arc::clone(&chat.theme);
        let border = fg(&theme, ThemeColor::Border).fg.unwrap_or(Color::Reset);

        // Two rows fit: no bar, so the box's border is the pane's last column.
        let buffer = frame_buffer(&mut chat, width, 20);
        let rows: Vec<String> = (0..20)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let (_, y) = cell_of(&rows, "╭─ models · 2").expect("a titled rule");
        assert_eq!(
            buffer[(width - 1, y)].symbol(),
            "╮",
            "the box reaches the pane's edge when nothing is hidden"
        );
        assert_eq!(
            buffer[(width - 1, y + 1)].fg,
            border,
            "and its border there"
        );

        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(
            (0..30)
                .map(|at| format!("openai/gpt-4.{at}"))
                .collect::<Vec<_>>(),
        );
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());
        let theme = Arc::clone(&chat.theme);
        let accent = fg(&theme, ThemeColor::Accent).fg.unwrap_or(Color::Reset);
        let muted = fg(&theme, ThemeColor::Muted).fg.unwrap_or(Color::Reset);

        // Thirty-one lines with nine in the window, plus the two `… N more`
        // rows: the thumb covers three of the eleven body rows at the top, and
        // the track runs on below them.
        let (buffer, body) = panel_frame_and_body(&mut chat, width);
        assert_eq!(
            bar_thumb_rows(&buffer, width, body),
            [0, 1, 2],
            "the thumb starts at the top"
        );
        assert_eq!(
            buffer[(width - 1, body + 5)].fg,
            muted,
            "and the track runs on below it"
        );
        assert_eq!(
            buffer[(width - 1, body)].symbol(),
            "█",
            "the thumb is a solid cell"
        );

        for _ in 0..15 {
            chat.on_key(Key::Down, Instant::now());
        }
        let (buffer, body) = panel_frame_and_body(&mut chat, width);
        assert_eq!(
            bar_thumb_rows(&buffer, width, body),
            [5, 6, 7],
            "the thumb moved down with the window"
        );
        assert_eq!(
            accent,
            fg(&theme, ThemeColor::Accent).fg.unwrap_or(Color::Reset)
        );
    }

    /// A frame and the row just under the panel's top rule.
    fn panel_frame_and_body(chat: &mut Chat, width: u16) -> (ratatui::buffer::Buffer, u16) {
        let buffer = frame_buffer(chat, width, 20);
        let rows: Vec<String> = (0..20)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let (_, y) = cell_of(&rows, "╭─ models · 30").expect("the model panel is open");
        (buffer, y + 1)
    }

    /// The body rows the bar's thumb covers, read from the pane's last column.
    fn bar_thumb_rows(buffer: &ratatui::buffer::Buffer, width: u16, body: u16) -> Vec<u16> {
        let theme = test_theme();
        let accent = fg(&theme, ThemeColor::Accent).fg.unwrap_or(Color::Reset);
        (0..11u16)
            .filter(|at| buffer[(width - 1, body + at)].fg == accent)
            .collect()
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

    /// The last usage footer the transcript holds, as its text.
    fn last_footer(chat: &Chat) -> Option<String> {
        chat.lines
            .iter()
            .rev()
            .find(|line| line.kind == LineKind::Usage)
            .map(|line| line.text.clone())
    }

    /// A finished turn shows its own time, the prompt it paid for, the share
    /// the provider cached and what it answered, on one dim row under the
    /// answer.
    #[test]
    fn a_finished_turn_shows_its_usage_under_the_answer() {
        let mut chat = chat();
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now() - Duration::from_millis(1_400));
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "answer".into(),
        });
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 3_400,
            completion_tokens: 250,
            cached_tokens: 2_900,
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });

        let footer = last_footer(&chat).unwrap_or_default();
        assert!(
            footer.ends_with("3.4k prompt (2.9k cached) · 250 out"),
            "{footer}"
        );
        let seconds = shown_seconds(&footer).unwrap_or_default();
        assert!((1.4..1.5).contains(&seconds), "{footer}");

        // The row is drawn, not only held: the frame is where a user sees it,
        // and it is the last row of the turn's own block.
        let rows = frame_rows(&mut chat, 80, 24);
        let frame = rows.join("");
        assert!(
            frame.contains("3.4k prompt (2.9k cached) · 250 out"),
            "{frame}"
        );

        // …and it is dim, like every other metadata row: the theme's `Dim`
        // token is what every row of its kind carries.
        let at = rows.iter().position(|row| row.contains("3.4k prompt"));
        assert!(at.is_some(), "no footer row: {rows:?}");
        let colors = frame_colors(&mut chat, 80, 24);
        let (footer_fg, _) = colors[at.unwrap_or_default() * 80 + MARK_INDENT];
        assert_eq!(footer_fg, rgb(&chat.theme.get_color_hex(ThemeColor::Dim)));

        // The totals `/usage` reads are untouched by the footer.
        assert_eq!(chat.last_prompt_tokens, 3_400);
        assert_eq!(chat.session_completion_tokens, 250);
    }

    /// A turn whose request carried history and read nothing back from the
    /// cache says so. The first request of a fresh session has no history to
    /// re-read, so a cold cache there is a provider's norm, not a miss.
    #[test]
    fn a_cold_cache_over_history_is_named() {
        let mut chat = chat();
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now());
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 1_000,
            completion_tokens: 40,
            cached_tokens: 0,
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        let first = last_footer(&chat).unwrap_or_default();
        assert!(!first.contains("cache miss"), "no history yet: {first}");
        assert!(first.ends_with("1k prompt · 40 out"), "{first}");
        assert!(!first.contains("(0 cached)"), "no zero as data: {first}");

        // The second turn carries the first: a cold cache is a miss now.
        type_text(&mut chat, "again");
        chat.on_key(Key::Enter, Instant::now());
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(2),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(2),
            prompt_tokens: 2_000,
            completion_tokens: 40,
            cached_tokens: 0,
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(2),
            reason: StopReason::Stop,
        });
        let second = last_footer(&chat).unwrap_or_default();
        assert!(second.ends_with("cache miss"), "{second}");
    }

    /// A priced model states the turn's cost at the end of the footer row, and
    /// the frame draws it: the money is part of the same dim row, not a line
    /// of its own.
    #[test]
    fn a_priced_turn_states_its_cost_in_the_footer() {
        let mut chat = priced_chat();
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now() - Duration::from_millis(1_400));
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "answer".into(),
        });
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 1_000,
            completion_tokens: 250,
            cached_tokens: 800,
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });

        // 200 in x $3/MTok + 800 cached x $0.30/MTok + 250 out x $15/MTok
        // = $0.00459, rounded to four places.
        let footer = last_footer(&chat).unwrap_or_default();
        assert!(
            footer.ends_with("1k prompt (800 cached) · 250 out · $0.0046"),
            "{footer}"
        );

        // The row reaches the screen, money and all.
        let rows = frame_rows(&mut chat, 80, 24);
        assert!(
            rows.join("").contains("· $0.0046"),
            "the money is on the frame: {rows:?}"
        );
    }

    /// An unpriced model's footer has no money part at all: the screen says
    /// nothing rather than `$0.000`, which would read as free.
    #[test]
    fn an_unpriced_turn_states_no_cost() {
        let mut chat = chat();
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now());
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 1_000,
            completion_tokens: 250,
            cached_tokens: 800,
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        let footer = last_footer(&chat).unwrap_or_default();
        assert!(
            footer.ends_with("1k prompt (800 cached) · 250 out"),
            "{footer}"
        );
        assert!(!footer.contains('$'), "no price, no figure: {footer}");
        assert!(!frame_text(&mut chat).contains("$0.000"), "no dollar zero");
    }

    /// A turn that reported no usage has no footer: a cancelled turn before
    /// its first round has nothing to show, and the screen says nothing rather
    /// than a row of zeros.
    #[test]
    fn a_turn_without_usage_shows_no_footer() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "answer".into(),
        });
        chat.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) });
        assert!(last_footer(&chat).is_none());
        let frame = frame_text(&mut chat);
        assert!(!frame.contains("prompt"), "{frame}");
        assert!(!frame.contains("cached"), "{frame}");
        assert!(!frame.contains("cache miss"), "{frame}");
    }

    /// The tab title follows the run state, and the tick writes it exactly
    /// once per state change — never once per tick.
    #[test]
    fn the_title_is_written_once_per_state_change() {
        let mut chat = chat();
        // The first tick claims the tab: it is the user's turn.
        let first = chat.terminal_tick().unwrap_or_default();
        assert!(first.starts_with("\x1b]2;titi "), "{first:?}");
        assert_eq!(first.matches('\x07').count(), 1, "{first:?}");

        // Five ticks in the same state: not one of them writes.
        let idle_writes = (0..5).filter(|_| chat.terminal_tick().is_some()).count();
        assert_eq!(idle_writes, 0, "an unchanged tick writes nothing");

        // The turn starts — before the first token — and the tab says working.
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now());
        chat.phase = WorkPhase::Waiting;
        let writes = (0..5).filter(|_| chat.terminal_tick().is_some()).count();
        assert_eq!(
            writes, 1,
            "one write for the one state change, not per tick"
        );
        let working = chat.last_title.clone().unwrap_or_default();
        assert!(working.contains("titi "), "{working}");

        // Every working phase is the same title: still no second write.
        chat.phase = WorkPhase::Streaming;
        assert!(chat.terminal_tick().is_none(), "{working}");
        chat.phase = WorkPhase::Thinking;
        assert!(chat.terminal_tick().is_none());
        chat.phase = WorkPhase::Tool {
            call_id: "call-1".to_owned(),
            name: "read".to_owned(),
            detail: None,
            since: Instant::now(),
        };
        assert!(
            chat.terminal_tick().is_some(),
            "a running tool is its own state"
        );

        // The turn ends: the tab goes back to the user's turn.
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        let ended = chat.terminal_tick();
        assert!(ended.is_some(), "the turn ended");
        assert!(chat.terminal_tick().is_none());
    }

    /// A failed turn leaves the tab saying so, and the next turn clears it.
    #[test]
    fn a_failed_turn_shows_in_the_title_until_the_next_one() {
        use crate::title::TitleState;
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::Failed {
            turn_id: Some(TurnId(1)),
            reason: titi_providers::ErrorReason::Connection,
            message: "no route to host".into(),
        });
        assert_eq!(chat.title_state(), TitleState::Error);
        let failed = chat.terminal_tick().unwrap_or_default();
        assert!(failed.contains('✘'), "the error mark: {failed:?}");
        assert!(chat.terminal_tick().is_none());

        type_text(&mut chat, "again");
        chat.on_key(Key::Enter, Instant::now());
        assert_eq!(chat.title_state(), TitleState::Waiting);
        assert!(
            chat.terminal_tick().is_some(),
            "the next turn clears the tab"
        );
    }

    /// A finished turn raises exactly one OSC 777 — the brand as its title,
    /// the session's label and one short fact as its body — and not one byte
    /// on the deltas that led there.
    #[test]
    fn a_finished_turn_notifies_once_with_the_label_and_the_fact() {
        use titi_tui::caps::NotifyChannel;
        let mut chat = chat();
        chat.session_label = "blue-otter".to_owned();
        chat.terminal = TerminalFeatures {
            channel: NotifyChannel::Osc777,
            ..TerminalFeatures::default()
        };
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        // Streaming is not an event that notifies: the deltas write the tab
        // and the bar, never a toast.
        for text in ["Hel", "lo, world"] {
            chat.on_event(EngineEvent::StreamDelta {
                turn_id: TurnId(1),
                text: text.into(),
            });
            let tick = chat.terminal_tick().unwrap_or_default();
            assert!(!tick.contains("777"), "a delta notified: {tick:?}");
        }

        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        let tick = chat.terminal_tick().expect("the turn's end writes");
        assert_eq!(tick.matches("\x1b]777;notify;").count(), 1, "{tick:?}");
        assert!(
            tick.contains("\x1b]777;notify;titi;blue-otter · turn finished\x07"),
            "{tick:?}"
        );
        // Once: the next tick has nothing left to say about that turn.
        let again = chat.terminal_tick().unwrap_or_default();
        assert!(!again.contains("777"), "{again:?}");
    }

    /// A turn that ended in a failure has its own fact, and an approval its
    /// own — with the tool's name and nothing of what the tool would read.
    #[test]
    fn a_failed_turn_and_an_approval_each_notify_with_their_own_fact() {
        use titi_tui::caps::NotifyChannel;
        let mut failing = chat();
        failing.session_label = "blue-otter".to_owned();
        failing.terminal = TerminalFeatures {
            channel: NotifyChannel::Osc777,
            ..TerminalFeatures::default()
        };
        failing.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        failing.on_event(EngineEvent::Failed {
            turn_id: Some(TurnId(1)),
            reason: titi_providers::ErrorReason::Connection,
            message: "no route to host".into(),
        });
        let failed = failing.terminal_tick().unwrap_or_default();
        assert!(
            failed.contains("\x1b]777;notify;titi;blue-otter · turn failed\x07"),
            "{failed:?}"
        );
        assert!(!failed.contains("no route"), "the failure is not the toast");

        // An approval of the next turn: the fact names the tool, never the
        // call's arguments.
        let mut asking = chat();
        asking.session_label = "blue-otter".to_owned();
        asking.terminal = TerminalFeatures {
            channel: NotifyChannel::Osc777,
            ..TerminalFeatures::default()
        };
        asking.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        asking.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
            detail: Some("bash rm -rf /tmp/secret".into()),
        });
        asking.on_event(EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
        });
        let asked = asking.terminal_tick().unwrap_or_default();
        assert!(
            asked.contains("\x1b]777;notify;titi;blue-otter · needs approval: bash\x07"),
            "{asked:?}"
        );
        assert!(!asked.contains("secret"), "{asked:?}");
    }

    /// A terminal that does not speak OSC 777 rings instead — and the BEL is
    /// exactly the difference between that terminal and one with no channel
    /// at all, so nothing else creeps into the byte stream.
    #[test]
    fn a_terminal_without_osc_777_gets_the_bell_and_nothing_else() {
        use titi_tui::caps::{BEL, NotifyChannel};
        /// The tick a finished turn writes on a channel, as a string.
        fn finished_tick(channel: NotifyChannel) -> String {
            let mut chat = chat();
            chat.session_label = "blue-otter".to_owned();
            chat.terminal = TerminalFeatures {
                channel,
                ..TerminalFeatures::default()
            };
            chat.on_event(EngineEvent::TurnStarted {
                turn_id: TurnId(1),
                model: "openai/gpt-4.1".into(),
            });
            chat.on_event(EngineEvent::StreamDelta {
                turn_id: TurnId(1),
                text: "done".into(),
            });
            chat.on_event(EngineEvent::TurnFinished {
                turn_id: TurnId(1),
                reason: StopReason::Stop,
            });
            chat.terminal_tick().unwrap_or_default()
        }

        let bell = finished_tick(NotifyChannel::Bell);
        let silent = finished_tick(NotifyChannel::None);
        assert!(!bell.contains("777"), "{bell:?}");
        assert_eq!(bell, format!("{silent}{BEL}"), "one bell, nothing more");
    }

    /// The switch turns one event off and leaves the others alone; a cancel is
    /// nobody's event to notify about.
    #[test]
    fn a_disabled_switch_emits_nothing_and_a_cancel_notifies_nobody() {
        use titi_tui::caps::NotifyChannel;
        /// The tick a finished turn writes, with the completion switch and the
        /// channel the test asks for.
        fn completion_tick(switch: bool, channel: NotifyChannel) -> String {
            let mut chat = chat();
            chat.session_label = "blue-otter".to_owned();
            chat.terminal = TerminalFeatures {
                notify_completion: switch,
                channel,
                ..TerminalFeatures::default()
            };
            chat.on_event(EngineEvent::TurnStarted {
                turn_id: TurnId(1),
                model: "openai/gpt-4.1".into(),
            });
            chat.on_event(EngineEvent::TurnFinished {
                turn_id: TurnId(1),
                reason: StopReason::Stop,
            });
            chat.terminal_tick().unwrap_or_default()
        }

        let off = completion_tick(false, NotifyChannel::Osc777);
        // Nothing of the notification is there — and the tick is byte for byte
        // the one a terminal with no channel at all would get, so the switch
        // removed the notification and nothing else.
        assert!(!off.contains("777"), "{off:?}");
        assert_eq!(off, completion_tick(false, NotifyChannel::None));
        assert_ne!(off, completion_tick(true, NotifyChannel::Osc777));

        // A cancel is the user's own hand on the screen: no notification.
        let mut cancelled = chat();
        cancelled.session_label = "blue-otter".to_owned();
        cancelled.terminal = TerminalFeatures {
            channel: NotifyChannel::Osc777,
            ..TerminalFeatures::default()
        };
        cancelled.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        cancelled.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) });
        let tick = cancelled.terminal_tick().unwrap_or_default();
        assert!(!tick.contains("777"), "{tick:?}");
    }

    /// The bar is raised with the turn and cleared on every way out of it —
    /// finish, failure, cancel — exactly once each, and never raised twice.
    #[test]
    fn the_progress_bar_is_raised_with_the_turn_and_cleared_on_every_exit() {
        use crate::title::{PROGRESS_CLEAR, PROGRESS_SET};
        #[derive(Debug, Clone, Copy)]
        enum Exit {
            Finished,
            Failed,
            Cancelled,
        }
        for exit in [Exit::Finished, Exit::Failed, Exit::Cancelled] {
            let mut chat = chat();
            chat.terminal = TerminalFeatures {
                channel: titi_tui::caps::NotifyChannel::None,
                ..TerminalFeatures::default()
            };
            chat.on_event(EngineEvent::TurnStarted {
                turn_id: TurnId(1),
                model: "openai/gpt-4.1".into(),
            });
            let start = chat.terminal_tick().unwrap_or_default();
            assert_eq!(
                start.matches(PROGRESS_SET).count(),
                1,
                "{exit:?}: {start:?}"
            );
            assert_eq!(start.matches(PROGRESS_CLEAR).count(), 0, "{exit:?}");
            // A second tick in the same state raises no second bar.
            let steady = chat.terminal_tick().unwrap_or_default();
            assert!(!steady.contains(PROGRESS_SET), "{exit:?}: {steady:?}");

            match exit {
                Exit::Finished => chat.on_event(EngineEvent::TurnFinished {
                    turn_id: TurnId(1),
                    reason: StopReason::Stop,
                }),
                Exit::Failed => chat.on_event(EngineEvent::Failed {
                    turn_id: Some(TurnId(1)),
                    reason: titi_providers::ErrorReason::Connection,
                    message: "no route to host".into(),
                }),
                Exit::Cancelled => chat.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) }),
            };
            let end = chat.terminal_tick().unwrap_or_default();
            assert_eq!(end.matches(PROGRESS_CLEAR).count(), 1, "{exit:?}: {end:?}");
            assert_eq!(end.matches(PROGRESS_SET).count(), 0, "{exit:?}: {end:?}");
            // The clear is written once: nothing keeps clearing a bar that is
            // already down.
            let after = chat.terminal_tick().unwrap_or_default();
            assert!(!after.contains(PROGRESS_CLEAR), "{exit:?}: {after:?}");
        }
    }

    /// With the switch off nothing is raised and nothing is cleared: the bar
    /// belongs to the terminal, and a user who turned it off wants no bytes.
    #[test]
    fn the_progress_switch_off_writes_no_bar() {
        let mut chat = chat();
        chat.terminal = TerminalFeatures {
            progress: false,
            ..TerminalFeatures::default()
        };
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        let start = chat.terminal_tick().unwrap_or_default();
        assert!(!start.contains("\x1b]9;4"), "{start:?}");
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        let end = chat.terminal_tick().unwrap_or_default();
        assert!(!end.contains("\x1b]9;4"), "{end:?}");
    }

    /// The switches come from the config and the terminal together: the config
    /// turns one off, and a terminal with no bar is quiet whatever the config
    /// says.
    #[test]
    fn the_resolved_features_read_the_config_and_the_terminal() {
        use titi_tui::caps::{NotifyChannel, TermEnv};
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(
            dir.path().join("config.yml"),
            "notify:\n  ask: off\nterminal:\n  progress: false\n",
        )
        .expect("config");
        let settings =
            titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("load");
        let plain = TermEnv {
            term: Some("xterm-256color".to_owned()),
            ..TermEnv::default()
        };
        let features = TerminalFeatures::resolve(Some(&settings), &plain);
        assert!(!features.notify_ask, "the config turned it off");
        assert!(features.notify_completion, "and left its siblings on");
        assert!(!features.progress, "the config turned the bar off");
        assert!(features.token_rate);
        // An unnamed terminal gets the BEL, which it certainly understands.
        assert_eq!(features.channel, NotifyChannel::Bell);

        // Unset means on, and the terminal then decides what it can take.
        let wezterm = TermEnv {
            term_program: Some("WezTerm".to_owned()),
            term: Some("xterm-256color".to_owned()),
            ..TermEnv::default()
        };
        let features = TerminalFeatures::resolve(None, &wezterm);
        assert!(features.progress, "WezTerm has a bar");
        assert_eq!(features.channel, NotifyChannel::Osc777);
        let no_bar = TerminalFeatures::resolve(None, &plain);
        assert!(!no_bar.progress, "a terminal without a bar stays quiet");
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
            detail: None,
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

    /// The generation rate stands next to the phase word, marked as the
    /// estimate it is: characters the row already counts, over four.
    #[test]
    fn the_working_row_shows_the_rate_the_estimate_earned() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "x".repeat(400).into(),
        });
        // One sample is not a rate: the row shows the count alone.
        let start = Instant::now();
        chat.sample_rate(start);
        let row = above_composer(&mut chat, 80, 20);
        assert!(!row.contains("tok/s"), "{row:?}");
        assert!(row.contains("400 chars"), "{row:?}");

        // A second of streaming later the characters over that second are the
        // reading — 400 more over four characters a token: ~100 tok/s.
        chat.reply.push_str(&"y".repeat(400));
        chat.sample_rate(start + Duration::from_secs(1));
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("~100 tok/s"), "{row:?}");
        assert!(row.contains("800 chars"), "{row:?}");
        assert!(
            row.contains("streaming · ~100 tok/s · 800 chars"),
            "{row:?}"
        );

        // The reading is kept between bursts: a later tick that streams
        // nothing new leaves the number standing.
        chat.sample_rate(start + Duration::from_secs(2));
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("~100 tok/s"), "{row:?}");
    }

    /// A new turn starts with no number — nothing has streamed yet — and the
    /// switch takes the segment off the row entirely.
    #[test]
    fn a_new_turn_and_a_disabled_switch_show_no_rate() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "x".repeat(400).into(),
        });
        let start = Instant::now();
        chat.sample_rate(start);
        chat.reply.push_str(&"y".repeat(400));
        chat.sample_rate(start + Duration::from_secs(1));
        assert!(
            above_composer(&mut chat, 80, 20).contains("tok/s"),
            "the reading the next assertions are about"
        );

        // The next turn clears it: a number from the last turn would be a lie
        // about this one.
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(2),
            model: "openai/gpt-4.1".into(),
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(!row.contains("tok/s"), "{row:?}");
        assert!(row.contains("waiting for the first token"), "{row:?}");

        // With the switch off the segment is not painted at all, even with a
        // reading standing behind it.
        chat.terminal.token_rate = false;
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(2),
            text: "z".repeat(400).into(),
        });
        chat.sample_rate(start);
        chat.reply.push_str(&"w".repeat(400));
        chat.sample_rate(start + Duration::from_secs(1));
        assert!(chat.token_rate.reading().is_some(), "the reading is there");
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("streaming"), "{row:?}");
        assert!(!row.contains("tok/s"), "the switch is off: {row:?}");
    }

    /// A tool call closes the reply line: the text of the round after it is
    /// a new line under the call and its result, not an addition to the text
    /// the model wrote before it asked for the tool.
    #[test]
    fn the_round_after_a_tool_starts_a_new_reply_line() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "Running it.".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "bash".into(),
            detail: Some("bash echo hi".into()),
        });
        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            output: "hi".into(),
            is_error: false,
            detail: None,
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "It printed hi.".into(),
        });
        let shown: Vec<(LineKind, &str)> = chat
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        let first = shown
            .iter()
            .position(|line| *line == (LineKind::Assistant, "Running it."))
            .expect("the first round's line");
        let tool = shown
            .iter()
            .position(|(kind, _)| *kind == LineKind::Tool)
            .expect("the tool chip");
        let second = shown
            .iter()
            .position(|line| *line == (LineKind::Assistant, "It printed hi."))
            .expect("the second round's own line");
        assert!(first < tool && tool < second, "{shown:?}");
    }

    /// An approval names what it approves: the command or the path the tool
    /// described, on the chip, in the status row and on the composer line —
    /// not only the tool's name.
    #[test]
    fn an_approval_shows_what_the_call_will_do() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "bash".into(),
            detail: Some("bash rm -rf build".into()),
        });
        chat.on_event(EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "bash".into(),
        });
        let frame = frame_rows(&mut chat, 80, 20).join("\n");
        assert!(frame.contains("▸ bash rm -rf build"), "{frame}");
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("needs you · bash rm -rf build"), "{row:?}");
        let rows = frame_rows(&mut chat, 80, 20);
        let prompt = &rows[17];
        assert!(prompt.contains("bash rm -rf build"), "{prompt:?}");
        assert!(prompt.contains("y allow"), "{prompt:?}");
    }

    /// A long command is cut, never the keys that answer it.
    #[test]
    fn a_long_approval_keeps_its_keys_on_a_narrow_screen() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        let long = format!("bash {}", "x".repeat(200));
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "bash".into(),
            detail: Some(long.into()),
        });
        chat.on_event(EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "bash".into(),
        });
        let rows = frame_rows(&mut chat, 60, 20);
        let prompt = &rows[17];
        assert!(prompt.contains("y allow"), "{prompt:?}");
        assert!(prompt.contains("n refuse"), "{prompt:?}");
        assert!(prompt.contains("bash xx"), "{prompt:?}");
        assert!(prompt.contains("…"), "{prompt:?}");
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
            detail: None,
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
            detail: None,
        });
        let row = above_composer(&mut chat, 80, 20);
        assert!(row.contains("read ·"), "{row:?}");

        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            output: "fn main() {}".into(),
            is_error: false,
            detail: None,
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
            detail: None,
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
                detail: None,
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
            detail: None,
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
            detail: None,
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

    /// A snapshot a test controls: the layout must not depend on this
    /// machine's working directory or git state.
    fn snapshot_for(chat: &Chat, context_pct: Option<u8>) -> StatusSnapshot {
        // Everything the screen states about the session comes from the live
        // helper, so a preset test measures the real line; only this machine's
        // working directory and git state are pinned. Untracked files are part
        // of that state: they are counted from the checkout the tests run in,
        // so a scratch file anywhere in the repository would otherwise add a
        // `?n` to the line and fail a golden that is about the layout.
        StatusSnapshot {
            path: "~/proj/titi/crates/titi-cli".to_owned(),
            git_branch: Some("master".to_owned()),
            git_unstaged: 2,
            git_untracked: 0,
            context_pct,
            ..masthead_snapshot(chat)
        }
    }

    /// The default status line is today's line, byte for byte.
    ///
    /// The promise the preset table makes is a compatibility one: a user who
    /// sets nothing sees exactly the frame they saw before the table existed.
    /// These strings were captured from the screen before the presets landed
    /// (`masthead_at` over a pinned snapshot, at the three widths the layout is
    /// judged at), and the last case is the context slot with a percentage in
    /// it — the one thing that changes between two frames of this line.
    #[test]
    fn the_default_status_line_is_todays_line() {
        let mut chat = chat();
        chat.model = "glm-5.3-flash".to_owned();
        chat.session_label = "blue-otter".to_owned();
        let golden = [
            (
                60u16,
                None,
                r#" titi  ready             blue-otter > ⬢ glm-5.3-flash >     "#,
            ),
            (
                60,
                Some(42),
                r#" titi  ready             blue-otter > ⬢ glm-5.3-flash >  42%"#,
            ),
            (
                80,
                None,
                r#" titi  ready                                 blue-otter > ⬢ glm-5.3-flash >     "#,
            ),
            (
                80,
                Some(42),
                r#" titi  ready                                 blue-otter > ⬢ glm-5.3-flash >  42%"#,
            ),
            (
                120,
                None,
                r#" titi  ready > 📁 ~/proj/titi/crates/titi-cli > ⑂ master *2                          blue-otter > ⬢ glm-5.3-flash >     "#,
            ),
            (
                120,
                Some(42),
                r#" titi  ready > 📁 ~/proj/titi/crates/titi-cli > ⑂ master *2                          blue-otter > ⬢ glm-5.3-flash >  42%"#,
            ),
        ];
        for (width, percent, expected) in golden {
            let line = masthead_at(&chat, width, &snapshot_for(&chat, percent));
            assert_eq!(line, expected, "{width} at {percent:?}");
        }
    }

    /// A preset is a row of the table, and the row decides the segments: what
    /// the masthead paints is what the row names, and `ascii` paints it in
    /// printable glyphs only.
    #[test]
    fn the_masthead_paints_the_preset_it_is_set_to() {
        let mut chat = chat();
        chat.model = "glm-5.3-flash".to_owned();
        chat.session_label = "blue-otter".to_owned();
        let at = |chat: &mut Chat, preset: StatusLinePreset| {
            chat.status_line.preset = preset;
            let snapshot = snapshot_for(chat, Some(42));
            masthead_at(chat, 120, &snapshot)
        };
        let default = at(&mut chat, StatusLinePreset::Default);
        for expected in ["titi", "ready", "blue-otter", "glm-5.3-flash", "~/proj"] {
            assert!(
                default.contains(expected),
                "{expected:?} missing: {default:?}"
            );
        }
        let minimal = at(&mut chat, StatusLinePreset::Minimal);
        assert!(minimal.contains("glm-5.3-flash"), "{minimal:?}");
        assert!(!minimal.contains("titi"), "{minimal:?}");
        assert!(!minimal.contains("blue-otter"), "{minimal:?}");
        let compact = at(&mut chat, StatusLinePreset::Compact);
        assert!(
            compact.contains("glm-5.3-flash") && compact.contains("~/proj"),
            "{compact:?}"
        );
        assert!(!compact.contains("blue-otter"), "{compact:?}");
        let full = at(&mut chat, StatusLinePreset::Full);
        assert!(full.contains("blue-otter"), "{full:?}");
        let ascii = at(&mut chat, StatusLinePreset::Ascii);
        assert!(ascii.is_ascii(), "{ascii:?}");
        assert!(ascii.contains("[D]") || ascii.contains("[M]"), "{ascii:?}");
        // Every preset fills the pane it is given.
        for preset in [
            StatusLinePreset::Default,
            StatusLinePreset::Minimal,
            StatusLinePreset::Compact,
            StatusLinePreset::Full,
            StatusLinePreset::Ascii,
        ] {
            assert_eq!(
                titi_tui::width::visible_width(&at(&mut chat, preset)),
                120,
                "{preset:?}"
            );
        }
    }

    /// Bare `/statusline` states the preset in force and lists every one, the
    /// way `/help` lists the commands.
    #[test]
    fn bare_statusline_states_the_current_preset_and_lists_them() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.slash("/statusline").expect("the command parses");
        let text: Vec<String> = chat.lines.iter().map(|line| line.text.clone()).collect();
        assert!(
            text.iter()
                .any(|line| line == "status line: default · context off"),
            "{text:?}"
        );
        for id in StatusLinePreset::IDS {
            assert!(
                text.iter()
                    .any(|line| line.starts_with(&format!("/{id}  "))),
                "{id} is not listed: {text:?}"
            );
        }
    }

    /// A name this build does not carry is refused with the list, and nothing
    /// is written: a typo must not become the remembered preset.
    #[test]
    fn an_unknown_preset_is_refused_with_the_list_and_writes_nothing() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let applied = chat.slash("/statusline nope").expect("the command parses");
        assert!(applied.effect.is_none(), "nothing is dispatched");
        let refusal = chat.lines.last().expect("a line").text.clone();
        assert!(refusal.contains("nope"), "{refusal:?}");
        for id in StatusLinePreset::IDS {
            assert!(refusal.contains(id), "{id} is not in {refusal:?}");
        }
        assert_eq!(chat.status_line.preset, StatusLinePreset::Default);
        let settings =
            titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("settings");
        assert_eq!(
            settings.get(titi_config::settings::STATUS_LINE_PRESET_KEY),
            None,
            "a refused name writes no key"
        );
    }

    /// A preset that is set is remembered and painted: the write lands on the
    /// canonical file, and the next frame is the new line.
    #[test]
    fn a_preset_is_remembered_and_the_next_frame_uses_it() {
        let (dir, mut chat) = picker_chat("glm-5.3-flash", "session-123");
        chat.session_label = "blue-otter".to_owned();
        let before = masthead_at(&chat, 120, &snapshot_for(&chat, Some(42)));
        assert!(
            before.contains("titi"),
            "the default opens with the mark: {before:?}"
        );
        chat.slash("/statusline minimal")
            .expect("the command parses");
        assert_eq!(chat.status_line.preset, StatusLinePreset::Minimal);
        let after = masthead_at(&chat, 120, &snapshot_for(&chat, Some(42)));
        assert!(!after.contains("titi"), "the mark is gone: {after:?}");
        assert!(
            !after.contains("blue-otter"),
            "and so is the name: {after:?}"
        );
        assert!(after.contains("glm-5.3-flash"), "{after:?}");
        let settings =
            titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("settings");
        assert_eq!(
            settings
                .get(titi_config::settings::STATUS_LINE_PRESET_KEY)
                .and_then(|value| value.as_str().map(str::to_owned)),
            Some("minimal".to_owned())
        );
        // What the next run resolves from that file is the same preset.
        let stored = settings
            .get(titi_config::settings::STATUS_LINE_PRESET_KEY)
            .and_then(|value| value.as_str().map(str::to_owned));
        assert_eq!(
            StatusLineStyle::resolve(stored.as_deref(), None).preset,
            StatusLinePreset::Minimal
        );
    }

    /// Maths is markdown: an answer whose only markup is a formula takes the
    /// renderer's path, so the `$…$` reaches the screen as the formula rather
    /// than as its own TeX — while a price that only looks like maths keeps the
    /// plain block it has always had.
    #[test]
    fn a_maths_only_answer_takes_the_markdown_path() {
        let rows = reply_rows_of(r"Binary search is $O(\log n)$.", 80);
        let texts = row_texts(&rows);
        assert!(
            texts.iter().any(|row| row.contains("O(log n)")),
            "the formula is rendered: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .all(|row| !row.contains(r"\log") && !row.contains('$')),
            "no TeX and no marker reached the screen: {texts:?}"
        );
        // A price is not maths, so the answer stays the plain speech block.
        let plain = row_texts(&reply_rows_of("It costs $5 and $6.", 80));
        assert!(
            plain.iter().any(|row| row.contains("$5 and $6")),
            "{plain:?}"
        );
    }

    /// The masthead's line, as the text a terminal would show.
    fn masthead_at(chat: &Chat, width: u16, snapshot: &StatusSnapshot) -> String {
        masthead_spans(chat, width, &test_theme(), snapshot)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The column `needle` starts in — a cell count, so a wide glyph before it
    /// does not count as one.
    fn column_of(line: &str, needle: &str) -> usize {
        let at = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not on the masthead: {line:?}"));
        titi_tui::width::visible_width(&line[..at])
    }

    /// The model sits in the same column whether or not a percentage has been
    /// reported: the context slot is drawn either way. The plan's §1.8 frames
    /// are the defect — the whole right block slid three cells left the moment
    /// the first `3%` appeared.
    #[test]
    fn the_context_percent_does_not_move_the_right_block() {
        let mut chat = chat();
        chat.model = "openai-codex/gpt-5.5".to_owned();
        chat.session_label = "blue-otter".to_owned();

        let without = masthead_at(&chat, 80, &snapshot_for(&chat, None));
        let with = masthead_at(&chat, 80, &snapshot_for(&chat, Some(3)));
        assert_eq!(
            column_of(&without, "gpt-5.5"),
            column_of(&with, "gpt-5.5"),
            "the model moved:\n{without}\n{with}"
        );
        assert_eq!(
            column_of(&without, "blue-otter"),
            column_of(&with, "blue-otter"),
            "the name moved:\n{without}\n{with}"
        );
        // The slot is drawn either way: the tail after the last separator has
        // the same width in both lines — that is what the two column
        // assertions above are measuring through — and holds nothing until a
        // report arrives, then the digits in the same place.
        let after_separator = |line: &str| {
            let at = line.rfind('>').expect("the separator before the slot") + 1;
            line[at..].to_owned()
        };
        assert_eq!(
            titi_tui::width::visible_width(&after_separator(&without)),
            titi_tui::width::visible_width(&after_separator(&with)),
            "the context slot moved:\n{without}\n{with}"
        );
        assert!(
            after_separator(&without).trim().is_empty(),
            "the slot is blank before a report: {without:?}"
        );
        assert_eq!(after_separator(&with).trim(), "3%", "{with:?}");
        for line in [&without, &with] {
            assert_eq!(titi_tui::width::visible_width(line), 80, "{line:?}");
        }
    }

    /// What the masthead gives up when the pane narrows, in the order it
    /// promises: the git state, the working directory, the session's name, the
    /// mode, the loop count, and then the model's provider prefix — and the
    /// model is never cut in the middle of a token while its short form fits.
    #[test]
    fn a_narrow_masthead_gives_up_in_the_documented_order() {
        let mut chat = chat();
        chat.model = "openai-codex/gpt-daybreak-blue-latest-wm".to_owned();
        chat.session_label = "a-very-long-session-name".to_owned();
        chat.mode = SessionMode::Plan;
        chat.jobs = vec![JobInfo {
            id: "job-1".into(),
            prompt: "watch CI".into(),
            interval_secs: 60,
            runs: 0,
        }];
        // Wide enough for everything but this machine's git state, since the
        // snapshot is the test's own.
        let wide = masthead_at(&chat, 160, &snapshot_for(&chat, Some(42)));
        for segment in [
            "titi  ready",
            "plan",
            "1 loop(s)",
            "master",
            "a-very-long-sessi",
            "openai-codex",
            "42%",
        ] {
            assert!(wide.contains(segment), "{segment:?} is missing: {wide}");
        }

        // Each step of the order, one at a time.
        let steps = |width: u16| masthead_at(&chat, width, &snapshot_for(&chat, Some(42)));
        let git_gone = steps(145);
        assert!(!git_gone.contains("master"), "{git_gone}");
        assert!(git_gone.contains("titi-cli"), "the path stays: {git_gone}");
        let path_gone = steps(125);
        assert!(!path_gone.contains("titi-cli"), "{path_gone}");
        assert!(
            path_gone.contains("a-very-long-sessi"),
            "the name stays: {path_gone}"
        );
        let name_gone = steps(95);
        assert!(!name_gone.contains("a-very-long-sessi"), "{name_gone}");
        assert!(name_gone.contains("plan"), "the mode stays: {name_gone}");
        let mode_gone = steps(80);
        assert!(!mode_gone.contains("plan"), "{mode_gone}");
        assert!(mode_gone.contains("loop(s)"), "the loops stay: {mode_gone}");
        let loops_gone = steps(70);
        assert!(!loops_gone.contains("loop(s)"), "{loops_gone}");
        assert!(
            loops_gone.contains("openai-codex/gpt-daybreak-blue-latest-wm"),
            "the model is still whole: {loops_gone}"
        );

        // The provider prefix is the next to go, and the short form is whole.
        let short = steps(55);
        assert!(!short.contains("openai-codex"), "{short}");
        assert!(short.contains("gpt-daybreak-blue-latest-wm"), "{short}");
        assert!(!short.contains('…'), "nothing is cut yet: {short}");

        // Last resort: the short form itself is cut, visibly.
        let cut = steps(40);
        assert!(cut.contains('…'), "{cut}");
        assert!(!cut.contains("gpt-daybreak-blue-latest-wm"), "{cut}");
        assert!(cut.contains("titi  ready"), "the state word stays: {cut}");
    }

    /// The masthead never runs past the pane, at any width a terminal can have.
    #[test]
    fn the_masthead_never_exceeds_the_pane() {
        let mut chat = chat();
        chat.model = "openai-codex/gpt-daybreak-blue-latest-wm".to_owned();
        chat.session_label = "a-very-long-session-name".to_owned();
        chat.mode = SessionMode::Plan;
        for width in 16..=200u16 {
            for percent in [None, Some(100)] {
                let line = masthead_at(&chat, width, &snapshot_for(&chat, percent));
                assert!(
                    titi_tui::width::visible_width(&line) <= width as usize,
                    "{width}: {} wide: {line:?}",
                    titi_tui::width::visible_width(&line)
                );
            }
        }
    }

    /// The frame's own masthead: the segments stand in the documented order,
    /// and the model is not cut at a width that has room for it.
    #[test]
    fn the_frame_masthead_stands_in_order() {
        let mut chat = chat();
        chat.model = "openai-codex/gpt-5.5".to_owned();
        chat.session_label = "blue-otter".to_owned();
        chat.mode = SessionMode::Plan;
        chat.context_percent = Some(7);
        let row = frame_rows(&mut chat, 100, 20)[0].clone();

        let mut last = 0usize;
        for segment in ["titi", "ready", "plan", "blue-otter", "gpt-5.5", "7%"] {
            let at = column_of(&row, segment);
            assert!(
                at >= last,
                "{segment:?} is out of order in {row:?} (at {at}, after {last})"
            );
            last = at;
        }
        assert!(!row.contains('…'), "nothing is cut at 100 columns: {row:?}");
    }

    /// A row of the status line, as the text a terminal would show.
    fn work_row_text(glyph: &str, fact: &WorkFact, color: ThemeColor, width: u16) -> String {
        row_texts(&[work_line(glyph, fact, color, width, &test_theme())]).remove(0)
    }

    /// The colour a row paints itself with.
    fn work_row_color(
        glyph: &str,
        fact: &WorkFact,
        color: ThemeColor,
        width: u16,
    ) -> Option<Color> {
        work_line(glyph, fact, color, width, &test_theme())
            .spans
            .first()
            .and_then(|span| span.style.fg)
    }

    /// Each state of the row is told apart by its token, not only by its glyph:
    /// activity in the accent, a running tool in the theme's own tool-title
    /// token, and an approval in the warning one.
    #[test]
    fn each_state_of_the_row_has_its_own_token() {
        let theme = test_theme();
        let fact = |wording: &str| WorkFact {
            wording: wording.to_owned(),
            argument: None,
            compact: wording.to_owned(),
            seconds: Some("0.1s".to_owned()),
        };
        let activity = work_row_color("*", &fact("streaming"), ThemeColor::Accent, 80);
        let tool = work_row_color(
            "\u{2699}",
            &tool_fact("read", Some("read docs/README.md"), "0.1s"),
            ThemeColor::ToolOutput,
            80,
        );
        let approval = work_row_color("\u{26a0}", &fact("needs you"), ThemeColor::Warning, 80);
        assert_eq!(activity, fg(&theme, ThemeColor::Accent).fg);
        assert_eq!(tool, fg(&theme, ThemeColor::ToolOutput).fg);
        assert_eq!(approval, fg(&theme, ThemeColor::Warning).fg);
        // The three are visibly different states, not three words in one
        // colour: this is the defect the plan's §3.5 names.
        for (left, right) in [(activity, tool), (tool, approval), (activity, approval)] {
            assert_ne!(left, right, "two states share a colour");
        }
    }

    /// The frames of the four live states, with the token each is painted with:
    /// this is the whole claim of the step, read off the screen rather than off
    /// a helper.
    ///
    /// The needle is the row's stable half. Two things in a live row move on
    /// their own: the spinner turns every 50 ms and the seconds are the wall
    /// clock, so a needle built from one drawn row (`⠋ … · 0.0s`) misses the
    /// next draw as soon as the two straddle a tick — which is how this test
    /// flaked on a loaded runner. The label and the token are the state; the
    /// clock is checked by shape, through the same [`shown_seconds`] the other
    /// row tests use.
    #[test]
    fn the_frame_paints_each_state_with_its_own_token() {
        let theme = test_theme();
        /// Drives a chat into one state of the row.
        type Drive = fn(&mut Chat);
        // `(label, drive, the row's stable text, its token, whether it runs a clock)`.
        let states: [(&str, Drive, &str, ThemeColor, bool); 5] = [
            (
                "waiting",
                |chat| {
                    chat.on_event(EngineEvent::TurnStarted {
                        turn_id: TurnId(1),
                        model: "openai/gpt-4.1".into(),
                    });
                },
                "waiting for the first token",
                ThemeColor::Accent,
                true,
            ),
            (
                "streaming",
                |chat| {
                    chat.on_event(EngineEvent::TurnStarted {
                        turn_id: TurnId(1),
                        model: "openai/gpt-4.1".into(),
                    });
                    chat.on_event(EngineEvent::StreamDelta {
                        turn_id: TurnId(1),
                        text: "hello".into(),
                    });
                },
                "streaming · 5 chars",
                ThemeColor::Accent,
                true,
            ),
            (
                "thinking",
                |chat| {
                    chat.on_event(EngineEvent::TurnStarted {
                        turn_id: TurnId(1),
                        model: "openai/gpt-4.1".into(),
                    });
                    chat.on_event(EngineEvent::ThinkingDelta {
                        turn_id: TurnId(1),
                        text: "weighing it".into(),
                    });
                },
                "thinking · 11 chars",
                ThemeColor::Accent,
                true,
            ),
            (
                "tool",
                |chat| {
                    chat.on_event(EngineEvent::TurnStarted {
                        turn_id: TurnId(1),
                        model: "openai/gpt-4.1".into(),
                    });
                    chat.on_event(EngineEvent::ToolStarted {
                        turn_id: TurnId(1),
                        call_id: "c1".into(),
                        name: "read".into(),
                        detail: Some("read docs/README.md".into()),
                    });
                },
                "read docs/README.md",
                ThemeColor::ToolOutput,
                true,
            ),
            (
                "needs you",
                |chat| {
                    chat.on_event(EngineEvent::ToolStarted {
                        turn_id: TurnId(1),
                        call_id: "c1".into(),
                        name: "write".into(),
                        detail: Some("write notes/probe.txt".into()),
                    });
                    chat.on_event(EngineEvent::ToolApprovalNeeded {
                        turn_id: TurnId(1),
                        call_id: "c1".into(),
                        name: "write".into(),
                    });
                },
                "needs you · write",
                ThemeColor::Warning,
                false,
            ),
        ];
        for (label, drive, needle, token, clocked) in states {
            let mut chat = chat();
            drive(&mut chat);
            // The work row is the line above the composer box, and only that
            // line is searched: a tool's own chip in the transcript carries the
            // same words (`tool read docs/README.md`), and this test is about
            // the row, not the chip.
            const ROWS: u16 = 20;
            const ROW: usize = (ROWS - 5) as usize;
            let row = above_composer(&mut chat, 80, ROWS);
            let buffer = frame_buffer(&mut chat, 80, ROWS);
            let symbols: Vec<String> = (0..ROWS)
                .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            let (x, _) = cell_of(&symbols[ROW..=ROW], needle)
                .unwrap_or_else(|| panic!("{label}: the row is not on screen: {row:?}"));
            assert_eq!(
                buffer[(x, ROW as u16)].fg,
                fg(&theme, token).fg.unwrap_or(Color::Reset),
                "{label} is not painted in {token:?}: {row:?}"
            );
            // The clock is the row's shape and never its value: the value is
            // the wall clock, and pinning it is what this test used to do.
            if clocked {
                assert!(
                    shown_seconds(&row).is_some(),
                    "{label}: the row carries no running clock: {row:?}"
                );
            } else {
                assert!(
                    shown_seconds(&row).is_none(),
                    "{label}: a row with no clock printed one: {row:?}"
                );
            }
        }
    }

    /// A tool's row carries what the tool said it was doing, and falls back to
    /// its name alone when it had nothing to say.
    #[test]
    fn a_tool_row_carries_the_call_or_the_name_alone() {
        let described = tool_fact("read", Some("read docs/README.md"), "0.4s");
        assert_eq!(
            work_row_text("\u{2699}", &described, ThemeColor::ToolOutput, 80),
            " \u{2699} read docs/README.md · 0.4s"
        );
        let bare = tool_fact("read", None, "0.4s");
        assert_eq!(
            work_row_text("\u{2699}", &bare, ThemeColor::ToolOutput, 80),
            " \u{2699} read · 0.4s"
        );
        // A description that is only a word has no argument to show.
        let wordy = tool_fact("git", Some("git"), "0.2s");
        assert_eq!(
            work_row_text("\u{2699}", &wordy, ThemeColor::ToolOutput, 80),
            " \u{2699} git · 0.2s"
        );
    }

    /// The row shortens the fact instead of cutting it, and the seconds are the
    /// last thing to go — the moving clock is what says the screen is alive.
    #[test]
    fn a_narrow_work_row_shortens_the_fact_and_keeps_the_seconds() {
        let tool = tool_fact("read", Some("read docs/README.md"), "0.4s");
        let row = |width: u16| work_row_text("\u{2699}", &tool, ThemeColor::ToolOutput, width);
        assert_eq!(
            row(80),
            " \u{2699} read docs/README.md · 0.4s",
            "the whole fact"
        );
        assert_eq!(
            row(25),
            " \u{2699} read docs/… · 0.4s",
            "the argument's head"
        );
        assert_eq!(row(19), " \u{2699} read · 0.4s", "the argument goes");

        let waiting = WorkFact {
            wording: "waiting for the first token".to_owned(),
            argument: None,
            compact: "waiting".to_owned(),
            seconds: Some("0.1s".to_owned()),
        };
        let waiting_row = |width: u16| work_row_text("*", &waiting, ThemeColor::Accent, width);
        assert_eq!(
            waiting_row(40),
            " * waiting for the first token · 0.1s",
            "the long wording while it fits"
        );
        assert_eq!(waiting_row(30), " * waiting · 0.1s", "the compact wording");
        let cut = waiting_row(12);
        assert!(cut.ends_with("· 0.1s"), "the seconds stay whole: {cut:?}");
        assert!(cut.contains('…'), "the wording is what was cut: {cut:?}");

        // And never wider than the pane, at any width.
        for width in 8..=120u16 {
            for fact in [&tool, &waiting] {
                let text = work_row_text("\u{2699}", fact, ThemeColor::ToolOutput, width);
                assert!(
                    titi_tui::width::visible_width(&text) <= width as usize,
                    "{width}: {text:?}"
                );
            }
        }
    }

    /// The column a row's body starts in: where its first word begins, with the
    /// gutter measured as the cells before it.
    fn body_column(row: &str, first_word: &str) -> usize {
        let at = row
            .find(first_word)
            .unwrap_or_else(|| panic!("{first_word:?} is not in {row:?}"));
        titi_tui::width::visible_width(&row[..at])
    }

    /// The transcript's geometry is one rule: the gutters measure what the
    /// indents say, and every block kind starts its body in its kind's column —
    /// on its first row and on every row it wraps to, the markdown answer and
    /// the diff included.
    #[test]
    fn the_geometry_is_one_rule() {
        let (air, tag, bar, hang) = message_gutter("you");
        assert_eq!(
            titi_tui::width::visible_width(&format!("{air}{tag}{bar}")),
            MESSAGE_INDENT,
            "the message gutter's pieces do not add up"
        );
        assert_eq!(
            titi_tui::width::visible_width(&hang),
            MESSAGE_INDENT,
            "a wrapped message row does not hang under its body"
        );
        let (air, mark, after, hang) = mark_gutter("\u{25b8}");
        assert_eq!(
            titi_tui::width::visible_width(&format!("{air}{mark}{after}")),
            MARK_INDENT,
            "the mark gutter's pieces do not add up"
        );
        assert_eq!(
            titi_tui::width::visible_width(&hang),
            MARK_INDENT,
            "a wrapped mark row does not hang under its body"
        );

        let theme = test_theme();
        let long = "word ".repeat(30);
        let wrapped = |kind: LineKind, text: String, indent: usize| {
            let rows = row_texts(&message_rows(&TranscriptLine { kind, text }, 60, &theme).0);
            assert!(rows.len() >= 2, "{kind:?} did not wrap: {rows:?}");
            for (at, row) in rows.iter().enumerate() {
                assert_eq!(
                    body_column(row, "word"),
                    indent,
                    "{kind:?} row {at} starts in the wrong column: {row:?}"
                );
            }
        };
        for kind in [
            LineKind::User,
            LineKind::Tool,
            LineKind::Error,
            LineKind::Note,
        ] {
            let text = match kind {
                LineKind::Tool => format!("tool done  {long}"),
                _ => long.clone(),
            };
            let indent = if kind == LineKind::User {
                MESSAGE_INDENT
            } else {
                MARK_INDENT
            };
            wrapped(kind, text, indent);
        }

        // The plain message block and the markdown one are the same block: an
        // answer cannot re-wrap because the renderer took it.
        for rows in [
            speech(
                "titi",
                ThemeColor::Accent,
                ThemeColor::Text,
                Surface::Page,
                &long,
                60,
                &theme,
            ),
            message_rows(
                &TranscriptLine {
                    kind: LineKind::Assistant,
                    text: format!("## {long}"),
                },
                60,
                &theme,
            )
            .0,
        ] {
            let rows = row_texts(&rows);
            assert!(rows.len() >= 2, "{rows:?}");
            for (at, row) in rows.iter().enumerate() {
                assert_eq!(
                    body_column(row, "word"),
                    MESSAGE_INDENT,
                    "an answer row starts in the wrong column ({at}): {row:?}"
                );
            }
        }

        // A diff's rows sit at the mark's inset, and its chip's mark at the
        // column a chip's own mark has.
        let rows = row_texts(
            &message_rows(
                &TranscriptLine {
                    kind: LineKind::Diff,
                    text: edit_result().to_owned(),
                },
                60,
                &theme,
            )
            .0,
        );
        assert!(rows.len() >= 2, "{rows:?}");
        let mark_column = MARK_INDENT - 2;
        assert_eq!(
            body_column(&rows[0], "\u{2713}"),
            mark_column,
            "the diff's chip is not a chip: {:?}",
            rows[0]
        );
        for row in &rows[1..] {
            assert!(
                row.starts_with(&" ".repeat(MARK_INDENT)),
                "a diff row is not at the mark's inset: {row:?}"
            );
        }
    }

    /// A detail that is not a diff — the todo checklist — keeps its rows: a
    /// list flattened onto one line reads as a sentence, not a checklist.
    #[test]
    fn a_checklist_detail_keeps_one_row_per_item() {
        let mut chat = chat();
        chat.push(LineKind::Tool, "todo 1/3 · Fix the parser".to_owned());
        chat.push(
            LineKind::Diff,
            "[x] 1. Read the failing test\n[>] 2. Fix the parser\n[ ] 3. Run the suite".to_owned(),
        );
        let rows = frame_rows(&mut chat, 80, 12);
        for item in [
            "✓ [x] 1. Read the failing test",
            "[>] 2. Fix the parser",
            "[ ] 3. Run the suite",
        ] {
            let row = rows
                .iter()
                .find(|row| row.contains(item))
                .unwrap_or_else(|| panic!("no row for {item:?}: {rows:#?}"));
            assert_eq!(
                row.trim(),
                item,
                "an item shares its row with another: {rows:#?}"
            );
        }
        // The rows after the first hang under its text, not under the mark.
        let column = |item: &str| {
            rows.iter().find_map(|row| {
                row.find(item)
                    .map(|byte| titi_tui::width::visible_width(&row[..byte]))
            })
        };
        assert_eq!(column("[x] 1."), column("[>] 2."), "{rows:#?}");
    }

    /// Air lands where the writer changes, and nowhere else: not between a
    /// turn's own text and its tool chips, not between a chip and the diff under
    /// it, not between a note and the answer it belongs to.
    #[test]
    fn air_lands_on_a_role_change_and_nowhere_else() {
        let mut chat = chat();
        chat.push(LineKind::User, "what is in the repo?".to_owned());
        chat.push(LineKind::Assistant, "## Files".to_owned());
        chat.push(LineKind::Tool, "tool done  read".to_owned());
        chat.push(LineKind::Diff, edit_result().to_owned());
        chat.push(LineKind::Note, "a note".to_owned());
        chat.push(LineKind::User, "and the tests?".to_owned());
        chat.push(LineKind::Assistant, "All green.".to_owned());

        let rows = frame_rows(&mut chat, 80, 30);
        // Every blank row with content above and below it that is not the
        // composer's frame, as what it sits between.
        let gaps: Vec<(String, String)> = rows
            .windows(3)
            .filter(|triple| {
                triple[1].trim().is_empty()
                    && !triple[0].trim().is_empty()
                    && !triple[2].trim().is_empty()
                    && !triple[2].starts_with('╭')
            })
            .map(|triple| (triple[0].trim().to_owned(), triple[2].trim().to_owned()))
            .collect();
        assert_eq!(gaps.len(), 3, "air in the wrong places: {gaps:?}");
        assert!(
            gaps.iter()
                .any(|(above, below)| above.contains("what is in the repo?")
                    && below.contains("Files")),
            "no air between the question and the answer: {gaps:?}"
        );
        assert!(
            gaps.iter()
                .any(|(above, below)| above.contains("a note") && below.contains("and the tests?")),
            "no air between the turn and the next question: {gaps:?}"
        );
        assert!(
            gaps.iter()
                .any(|(above, below)| above.contains("and the tests?")
                    && below.contains("All green")),
            "no air between the question and the answer: {gaps:?}"
        );
        // And none inside the turn: the reply, its chip, its diff and its note
        // are one body.
        for (above, below) in &gaps {
            for joined in [("Files", "read"), ("read", "@@"), ("@@", "a note")] {
                assert!(
                    !(above.contains(joined.0) && below.contains(joined.1)),
                    "air inside one turn's body between {joined:?}: {gaps:?}"
                );
            }
        }
    }

    /// The user's own question is the one block on a surface of its own: the
    /// theme's `userMessageBg` across exactly its rows, `userMessageText` on the
    /// body, and the label's colour unchanged.
    ///
    /// Run against the default preset and against `dark`: the palette titi
    /// starts on by itself has to show the band, which `titanium` did not until
    /// its `userMessageBg` was given a surface of its own.
    #[test]
    fn the_user_block_carries_its_own_surface() {
        for name in ["titanium", "dark"] {
            assert_user_block_surface(name);
        }
    }

    /// The band's extent and colours on one palette.
    fn assert_user_block_surface(name: &str) {
        let theme = test_theme_named(name);
        let band = bg(&theme, ThemeBg::UserMessageBg);
        let page_bg = bg(&theme, ThemeBg::StatusLineBg);
        assert_ne!(
            band, page_bg,
            "{name}: this test needs a palette where the band shows"
        );

        let mut chat = chat_with_theme(theme.clone());
        chat.push(LineKind::User, "word ".repeat(20));
        chat.push(LineKind::Assistant, "All green.".to_owned());

        let buffer = frame_buffer(&mut chat, 80, 30);
        let rows: Vec<String> = (0..30)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let user_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains("word word"))
            .map(|(at, _)| at)
            .collect();
        assert!(user_rows.len() >= 2, "the question wrapped: {rows:?}");

        // The band covers every row of the block, to the layout's width.
        let layout = 78;
        for at in &user_rows {
            for x in [0, 40, layout - 1] {
                assert_eq!(
                    buffer[(x as u16, *at as u16)].bg,
                    band,
                    "row {at} column {x} is not on the band"
                );
            }
            assert_ne!(
                buffer[(79, *at as u16)].bg,
                band,
                "the band ran to the pane's last column"
            );
        }
        // The body and the label keep their own colours, on the band.
        let (x, y) = cell_of(&rows, "word").expect("the question is on screen");
        assert_eq!(
            buffer[(x, y)].fg,
            fg(&theme, ThemeColor::UserMessageText)
                .fg
                .unwrap_or(Color::Reset)
        );
        assert_eq!(buffer[(x, y)].bg, band);
        let (x, y) = cell_of(&rows, "you").expect("the label is on screen");
        assert_eq!(
            buffer[(x, y)].fg,
            fg(&theme, ThemeColor::CustomMessageLabel)
                .fg
                .unwrap_or(Color::Reset),
            "the label's colour changed"
        );
        assert_eq!(buffer[(x, y)].bg, band, "the label is not on the band");

        // The rows around it are not banded: the air, and the answer.
        let after = user_rows.last().expect("a row") + 1;
        assert!(
            rows[after].trim().is_empty(),
            "no air after the block: {rows:?}"
        );
        assert_eq!(
            buffer[(0, after as u16)].bg,
            page_bg,
            "the air is on the band"
        );
        let answer = rows
            .iter()
            .position(|row| row.contains("All green"))
            .expect("the answer is on screen");
        assert_eq!(
            buffer[(0, answer as u16)].bg,
            page_bg,
            "the answer is on the band"
        );
    }

    /// The band is exactly as wide as the block's own column range at any pane
    /// width, and nothing runs past the pane.
    #[test]
    fn the_band_covers_the_blocks_own_width() {
        for name in ["titanium", "dark"] {
            assert_band_width(&test_theme_named(name), name);
        }
    }

    /// The band's width at each pane size, on one palette.
    fn assert_band_width(theme: &Arc<Theme>, name: &str) {
        let band = bg(theme, ThemeBg::UserMessageBg);
        for width in [60u16, 80, 120] {
            let mut chat = chat_with_theme(Arc::clone(theme));
            chat.push(LineKind::User, "word ".repeat(30));
            let buffer = frame_buffer(&mut chat, width, 30);
            let mut banded = 0;
            for y in 0..30 {
                if buffer[(0, y)].bg == band {
                    banded += 1;
                    assert_ne!(
                        buffer[(width - 1, y)].bg,
                        band,
                        "{name} at {width}: the band reached the pane's edge"
                    );
                    assert_eq!(
                        buffer[(width - 3, y)].bg,
                        band,
                        "{name} at {width}: the band stopped short of the layout's width"
                    );
                }
            }
            assert!(
                banded >= 2,
                "{width}: the block did not wrap: {banded} rows"
            );
        }
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
            "exit",
            "quit",
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
                // A picker is an answer too: `/model`, `/login` and `/theme`
                // open one instead of printing.
                || chat.model_picker.is_some()
                || chat.login_picker.is_some()
                || chat.theme_picker.is_some();
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
        // The spend is the provider's count wherever it reports one; calling
        // all of it an estimate undersells the number.
        assert!(
            !chat
                .lines
                .last()
                .is_some_and(|line| line.text.contains("estimated")),
            "{:?}",
            chat.lines.last()
        );
    }

    /// A cap in money cannot be enforced by a token cap — the engine counts
    /// tokens, and those bill at different rates — so it is refused, and the
    /// refusal names what is missing instead of converting at a guessed rate.
    #[test]
    fn budget_refuses_money_and_nonsense() {
        let mut chat = chat();
        type_text(&mut chat, "/budget $5");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines.iter().any(|line| line.kind == LineKind::Error
                && line.text.contains("has no price here")
                && line.text.contains("cap tokens instead")),
            "the refusal says which piece is missing: {:?}",
            chat.lines.last()
        );

        type_text(&mut chat, "/budget plenty");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.kind == LineKind::Error && line.text.contains("plenty"))
        );
    }

    /// When the model does have a price, the refusal states it: the user can
    /// see that the price is known and that the missing piece is the engine's
    /// tally, not a number this screen failed to look up.
    #[test]
    fn budget_refusal_states_a_price_it_does_know() {
        let mut chat = priced_chat();
        type_text(&mut chat, "/budget $2");
        assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
        let said = chat
            .lines
            .last()
            .map(|line| line.text.clone())
            .unwrap_or_default();
        assert!(said.contains("$3.00 in / $15.00 out per MTok"), "{said}");
        assert!(said.contains("cost ledger"), "{said}");
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

    /// A local server that takes no credential is ready as it is: `/keys`
    /// and `/diagnose` must not list it as missing something.
    #[test]
    fn a_provider_without_a_credential_is_not_missing_a_key() {
        let dir = agent_dir_with_extra_provider();
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        type_text(&mut chat, "/keys");
        chat.on_key(Key::Enter, Instant::now());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "ollama  no key needed"),
            "{:?}",
            chat.lines
        );
        // A provider that does need one and has none still says so (the
        // test environment sets no ZAI_API_KEY).
        assert!(
            chat.lines.iter().any(|line| line.text == "zai  no key"),
            "{:?}",
            chat.lines
        );

        type_text(&mut chat, "/diagnose");
        chat.on_key(Key::Enter, Instant::now());
        let summary = chat.lines.last().expect("a transcript line");
        assert!(
            summary.text.contains("ollama (no key needed)"),
            "{summary:?}"
        );
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

    /// A resumed session is replayed into the engine, so the model answers
    /// with that conversation in mind; the screen shows the same conversation
    /// rather than a welcome that reads as a fresh start.
    #[test]
    fn a_resumed_session_shows_its_conversation() {
        let dir = tempfile::tempdir().expect("temp");
        let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
        let id = store
            .create(titi_core::session::SessionMeta::default())
            .expect("session");
        store
            .append(&id, Role::User, "remember the word banana")
            .expect("user");
        store
            .append(&id, Role::Assistant, "noted: banana")
            .expect("assistant");
        let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
        chat.agent_dir = dir.path().to_path_buf();

        chat.show_stored_history();

        let shown: Vec<(LineKind, &str)> = chat
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (LineKind::User, "remember the word banana"),
                (LineKind::Assistant, "noted: banana"),
            ]
        );
        let frame = frame_rows(&mut chat, 80, 20).join("\n");
        assert!(!frame.contains("say what you want done"), "{frame}");

        // A session with nothing in it keeps the welcome.
        let fresh = store
            .create(titi_core::session::SessionMeta::default())
            .expect("fresh");
        let mut chat = Chat::new("openai/gpt-4.1", &fresh, test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        chat.show_stored_history();
        assert!(chat.lines.is_empty(), "{:?}", chat.lines);
    }

    /// A paste is usually code or a log, and its line breaks are part of it:
    /// they reach the model as written, whatever the terminal's line ending,
    /// and a tab stays a tab.
    #[test]
    fn a_pasted_block_keeps_its_lines() {
        let mut chat = chat();
        chat.paste("fn main() {\r\n\tprintln!(\"hi\");\r}\n");
        assert_eq!(chat.input, "fn main() {\n\tprintln!(\"hi\");\n}\n");

        // One row in the composer, each break shown, nothing cut mid-word.
        let rows = frame_rows(&mut chat, 80, 20);
        assert!(
            rows[17].contains("fn main() {↵    println!(\"hi\");↵}"),
            "{:?}",
            rows[17]
        );

        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SubmitPrompt {
                text: "fn main() {\n\tprintln!(\"hi\");\n}".into()
            }))
        );
        // The transcript shows it as the lines it is.
        let frame = frame_rows(&mut chat, 80, 20);
        let first = frame
            .iter()
            .position(|row| row.contains("fn main() {"))
            .expect("the first line");
        assert!(frame[first + 1].contains("│     println!"), "{frame:?}");
        assert!(!frame[first].contains("println!"), "{frame:?}");
    }

    /// The text a send carries, whichever command it is: a prompt opens a
    /// turn and a later one steers it, and a paste test is about the text.
    fn sent_text(applied: &Applied) -> Option<String> {
        match applied.effect.as_ref()? {
            ChatEffect::Send(EngineCommand::SubmitPrompt { text })
            | ChatEffect::Send(EngineCommand::Steer { text }) => Some(text.to_string()),
            _ => None,
        }
    }

    /// A body taller than the composer's threshold: eight lines of a stack
    /// trace, which is the shape the collapse exists for.
    fn stack_trace() -> String {
        (1..=8)
            .map(|n| format!("  at frame {n} (module.rs:{n})"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A paste above the threshold leaves one marker in the draft, not the
    /// wall: the draft is what the user can still edit and send.
    #[test]
    fn a_long_paste_collapses_to_a_marker() {
        let mut chat = chat();
        chat.paste(&stack_trace());
        assert_eq!(
            chat.input, "[Paste #1 · 8 lines]",
            "the draft is the marker, not the body"
        );
        // The frame draws the marker whole — and none of the wall.
        let frame = frame_text(&mut chat);
        assert!(frame.contains("[Paste #1 · 8 lines]"), "{frame}");
        assert!(
            !frame.contains("frame 5"),
            "the body is not on screen: {frame}"
        );
    }

    /// The boundary the collapse turns on: the threshold sits between the two.
    #[test]
    fn a_paste_at_the_threshold_stays_inline() {
        let six = (1..=6)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chat = chat();
        chat.paste(&six);
        assert_eq!(chat.input, six, "six lines are still a draft");

        let mut chat = chat_with_theme(test_theme());
        let seven = (1..=7)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        chat.paste(&seven);
        assert_eq!(chat.input, "[Paste #1 · 7 lines]");
    }

    /// Sending expands the marker: the model reads the whole paste, the
    /// transcript echoes the marker, and the session file records what was
    /// sent — so the wall is never lost and the screen is never the wall.
    #[test]
    fn a_collapsed_paste_expands_when_it_is_sent() {
        let mut chat = chat();
        chat.input.push_str("what is this trace? ");
        chat.paste(&stack_trace());
        let applied = chat.on_key(Key::Enter, Instant::now());

        let text = sent_text(&applied).expect("the prompt goes out");
        assert_eq!(
            text,
            "what is this trace?   at frame 1 (module.rs:1)\n  at frame 2 (module.rs:2)\n  at frame 3 (module.rs:3)\n  at frame 4 (module.rs:4)\n  at frame 5 (module.rs:5)\n  at frame 6 (module.rs:6)\n  at frame 7 (module.rs:7)\n  at frame 8 (module.rs:8)",
            "the whole paste is sent"
        );
        let log = applied.log.expect("the prompt is recorded");
        assert_eq!(log.text, text, "the session file holds what was sent");
        assert!(
            !log.text.contains("[Paste #"),
            "the marker is never text the model reads"
        );

        // The transcript echo is the marker, and no line of the wall follows.
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("what is this trace? [Paste #1 · 8 lines]"),
            "{frame}"
        );
        assert!(!frame.contains("frame 4"), "{frame}");
    }

    /// The guard: only a marker the open draft registered expands. Once the
    /// draft is gone the same characters are text, so a marker can never ship
    /// a body pasted into some earlier draft.
    #[test]
    fn a_marker_stops_expanding_once_its_draft_is_gone() {
        let mut chat = chat();
        chat.paste(&stack_trace());
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.log.is_some(), "the first send expands the marker");

        for ch in "[Paste #1 · 8 lines]".chars() {
            chat.on_key(Key::Char(ch), Instant::now());
        }
        assert_eq!(chat.input, "[Paste #1 · 8 lines]");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            sent_text(&applied).as_deref(),
            Some("[Paste #1 · 8 lines]"),
            "an unregistered marker is literal text"
        );
    }

    /// A pasted body is sent exactly as it was pasted: a break and a tab are
    /// part of the paste, so the wall that reaches the model is byte for byte
    /// what the clipboard held (bar `\r\n`).
    #[test]
    fn a_collapsed_paste_keeps_its_tabs_and_lines() {
        let body = "fn main() {\n\tprintln!(\"a\");\n\tprintln!(\"b\");\n\tprintln!(\"c\");\n\tprintln!(\"d\");\n\tprintln!(\"e\");\r\n\tprintln!(\"f\");\r}\n";
        let mut chat = chat();
        chat.paste(body);
        assert_eq!(chat.input, "[Paste #1 · 8 lines]");
        let applied = chat.on_key(Key::Enter, Instant::now());
        let text = sent_text(&applied).expect("the prompt goes out");
        // Every `\r\n` and lone `\r` is one `\n`; the tabs are the paste's own.
        assert_eq!(text, body.replace("\r\n", "\n").replace('\r', "\n"));
    }

    /// The boundary the marker draws around `/`: a marker is one line with no
    /// slash in it, so it is never a command token itself, and the completion
    /// still works on the draft the composer returns to.
    #[test]
    fn the_slash_list_is_unaffected_by_a_marker() {
        let mut chat = chat();
        chat.paste(&stack_trace());
        assert!(
            !chat.input.contains('\n'),
            "a marker is one line: {:?}",
            chat.input
        );
        assert!(
            picker_rows(&chat).is_empty(),
            "a marker alone is not a slash token"
        );

        // Esc takes the draft — and the body behind the marker — away, and the
        // command list is what it always was.
        chat.on_key(Key::Esc, Instant::now());
        assert!(chat.input.is_empty(), "esc leaves an empty draft");
        for ch in "/comp".chars() {
            chat.on_key(Key::Char(ch), Instant::now());
        }
        let rows = picker_rows(&chat);
        assert!(
            rows.iter()
                .any(|row| matches!(row, PickRow::Command(command) if command.name == "compact")),
            "the command list still completes"
        );
        let frame = frame_text(&mut chat);
        assert!(frame.contains("/compact"), "{frame}");
        // Tab completes the token into the draft.
        chat.on_key(Key::Tab, Instant::now());
        assert_eq!(chat.input, "/compact ");
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
        let flush = chat.take_output_flush();
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
        assert!(chat.take_output_flush().is_empty());
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

    /// Cached input is cheaper input; /usage says how much of the prompt
    /// the provider served from its cache, and says nothing when none was.
    #[test]
    fn usage_names_the_cached_share_of_the_prompt() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 1_000,
            completion_tokens: 50,
            cached_tokens: 800,
        });
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(2),
            prompt_tokens: 1_200,
            completion_tokens: 40,
            cached_tokens: 1_000,
        });
        type_text(&mut chat, "/usage");
        chat.on_key(Key::Enter, Instant::now());
        let note = chat.lines.last().map(|line| line.text.clone());
        assert_eq!(
            note.as_deref(),
            Some(
                "Turn: 1200 prompt (1000 cached) + 40 completion. Session: 2200 (1800 cached) / 90."
            )
        );
    }

    #[test]
    fn usage_command_prints_tokens() {
        let mut chat = chat();
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 0,
        });
        type_text(&mut chat, "/usage");
        chat.on_key(Key::Enter, Instant::now());
        let view = frame_text(&mut chat);
        assert!(
            view.contains("Turn: 100 prompt + 50 completion. Session: 100 / 50."),
            "View: {view}"
        );
        // An unpriced model has no total to state, and `$0.00` is not it.
        assert!(!view.contains("session total"), "View: {view}");
    }

    /// A priced model puts the session's cost next to its token totals,
    /// rounded to cents.
    #[test]
    fn usage_states_the_session_cost_when_the_model_is_priced() {
        let mut chat = priced_chat();
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 100_000,
            completion_tokens: 5_000,
            cached_tokens: 0,
        });
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(2),
            prompt_tokens: 1_200,
            completion_tokens: 40,
            cached_tokens: 1_000,
        });
        type_text(&mut chat, "/usage");
        chat.on_key(Key::Enter, Instant::now());
        // $0.375 for the first turn, $0.0015 for the second: $0.37650.
        assert_eq!(
            chat.lines.last().map(|line| line.text.clone()).as_deref(),
            Some(
                "Turn: 1200 prompt (1000 cached) + 40 completion. \
                 Session: 101200 (1000 cached) / 5040 · session total $0.38."
            )
        );
    }

    /// A session that switched from a priced model to an unpriced one states
    /// its total as a floor: the turns it could not price are named, not
    /// silently dropped.
    #[test]
    fn usage_marks_a_total_that_leaves_turns_out() {
        let mut chat = priced_chat();
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 100_000,
            completion_tokens: 5_000,
            cached_tokens: 0,
        });
        // The switch a `/model ollama/qwen3` makes: the next turn has no
        // price to read.
        chat.model = "ollama/qwen3".to_owned();
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(2),
            prompt_tokens: 500,
            completion_tokens: 20,
            cached_tokens: 0,
        });
        type_text(&mut chat, "/usage");
        chat.on_key(Key::Enter, Instant::now());
        let said = chat
            .lines
            .last()
            .map(|line| line.text.clone())
            .unwrap_or_default();
        assert!(
            said.ends_with("· session total $0.38+ (unpriced turns excluded)."),
            "{said}"
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

    /// `/genome off` writes a false `genome.enabled` on the agent's own
    /// config, and a following `/genome` note reads its own file back: the
    /// note is the file's word, not the input echoed.
    #[test]
    fn genome_off_persists_and_the_next_note_reads_it_back() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        type_text(&mut chat, "/genome off");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(chat.lines.iter().any(|line| line.text == "genome: off"));
        let config = std::fs::read_to_string(chat.agent_dir.join("config.yml")).unwrap();
        assert_eq!(config, "genome:\n  enabled: false\n");

        chat.lines.clear();
        type_text(&mut chat, "/genome");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        let note = chat
            .lines
            .iter()
            .find(|line| line.text.contains("genome: off"))
            .expect("the status note names the off state");
        assert!(note.text.contains("reason: setting"), "{}", note.text);
        assert!(note.text.contains("limit: 24"), "{}", note.text);
    }

    /// An in-range `/genome limit` persists on its own key and the note
    /// reports it; the limit is not coupled to the enabled state.
    #[test]
    fn genome_limit_persists_and_is_reported() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        type_text(&mut chat, "/genome limit 4");
        chat.on_key(Key::Enter, Instant::now());
        let config = std::fs::read_to_string(chat.agent_dir.join("config.yml")).unwrap();
        assert!(config.contains("limit: 4"), "{config}");
        assert!(!config.contains("enabled"), "{config}");

        chat.lines.clear();
        type_text(&mut chat, "/genome");
        chat.on_key(Key::Enter, Instant::now());
        let note = chat
            .lines
            .iter()
            .find(|line| line.text.contains("limit: 4"))
            .expect("the note states the saved cap");
        assert!(note.text.contains("genome: on"), "{}", note.text);
    }

    /// An out-of-range `/genome limit` writes nothing and says the range.
    #[test]
    fn genome_limit_out_of_range_is_refused_without_a_write() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        type_text(&mut chat, "/genome limit 0");
        chat.on_key(Key::Enter, Instant::now());
        assert!(
            !chat.agent_dir.join("config.yml").exists(),
            "nothing was written"
        );
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "genome limit: expected an integer from 1 to 64")
        );
    }

    /// `/genome check` runs the real index over the workspace and pushes the
    /// diagnostic lines as a note; a broken import names itself with its code.
    #[test]
    fn genome_check_reports_diagnostics_from_the_workspace() {
        let dir = tempfile::tempdir().expect("temp");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("lib.rs"),
            "use crate::missing::Thing;\npub fn present() {}\n",
        )
        .unwrap();
        let agent = tempfile::tempdir().expect("temp agent");
        let mut chat = chat();
        chat.agent_dir = agent.path().to_path_buf();
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        // The workspace comes in as a value: the test hands it the temp tree
        // directly instead of moving the process `current_dir`, which every
        // parallel test reads.
        local_genome_note(&mut chat, "check", dir.path());
        let names: Vec<&str> = chat.lines.iter().map(|line| line.text.as_str()).collect();
        let hit = names
            .iter()
            .find(|text| text.contains("unresolved-import"))
            .expect("the broken import names itself");
        assert!(hit.contains("missing"), "{names:?}");
        assert!(hit.contains("src/lib.rs:1:"), "{names:?}");
        assert!(
            hit.starts_with("src/lib.rs:1: unresolved-import: "),
            "{names:?}"
        );
    }

    /// `/genome lsp` never starts a stdio server inside the chat: the note
    /// names the terminal command, exactly, instead of pretending.
    #[test]
    fn genome_lsp_names_the_terminal_command() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        type_text(&mut chat, "/genome lsp");
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert!(applied.effect.is_none());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "genome: lsp is 'titi genome lsp', not a chat command"),
            "{:?}",
            chat.lines
        );
    }

    /// An unknown `/genome` word names itself and shows the usage line.
    #[test]
    fn genome_unknown_subcommand_shows_usage() {
        let dir = tempfile::tempdir().expect("temp");
        let mut chat = chat();
        chat.agent_dir = dir.path().to_path_buf();
        std::fs::create_dir_all(&chat.agent_dir).unwrap();

        type_text(&mut chat, "/genome wat");
        chat.on_key(Key::Enter, Instant::now());
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "genome: unknown command wat")
        );
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text == "usage: titi genome [on|off|limit <n>|check|lsp]")
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

    /// Types a line and presses Enter: the shape every command test uses.
    fn command(chat: &mut Chat, line: &str) -> Applied {
        type_text(chat, line);
        chat.on_key(Key::Enter, Instant::now())
    }

    /// `/details` reaches the four named sections the deleted `transcript.rs`
    /// carried: expanded draws a section's lines, collapsed stands them up as
    /// one counted row, hidden draws neither.
    #[test]
    fn details_sets_a_sections_visibility() {
        let mut chat = chat();
        chat.push(LineKind::Tool, "tool done  read src/main.rs".to_owned());
        chat.push(LineKind::Tool, "tool done  write src/lib.rs".to_owned());
        assert!(
            frame_text(&mut chat).contains("read src/main.rs"),
            "tools start expanded, as the old module's DoD had them"
        );

        command(&mut chat, "/details tools collapsed");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("▸ tools (2)"), "one counted row: {frame}");
        assert!(
            !frame.contains("read src/main.rs"),
            "and the lines are behind it: {frame}"
        );
        assert!(
            frame.contains("details: tools collapsed"),
            "the command answers with the mode the next frame draws: {frame}"
        );

        command(&mut chat, "/details tools hidden");
        let frame = frame_text(&mut chat);
        assert!(
            !frame.contains("tools (2)") && !frame.contains("read src/main.rs"),
            "neither the header nor the lines: {frame}"
        );

        command(&mut chat, "/details tools expanded");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("read src/main.rs"), "{frame}");
        assert!(
            !frame.contains("tools (2)"),
            "no header when expanded: {frame}"
        );
    }

    /// The conversation is not a section: no `/details` word takes the user's
    /// own question or the answer off the screen.
    #[test]
    fn details_never_hides_the_conversation() {
        let mut chat = chat();
        chat.push(LineKind::User, "a question that stays".to_owned());
        chat.push(LineKind::Assistant, "an answer that stays".to_owned());
        for word in ["hidden", "collapsed", "cycle"] {
            command(&mut chat, &format!("/details {word}"));
            let frame = frame_text(&mut chat);
            assert!(frame.contains("a question that stays"), "{word}: {frame}");
            assert!(frame.contains("an answer that stays"), "{word}: {frame}");
        }
    }

    /// Bare `/details` is the only way to read the state back, so it lists
    /// every section and the mode it is on.
    #[test]
    fn details_lists_every_section_when_bare() {
        let mut chat = chat();
        command(&mut chat, "/details");
        let frame = frame_text(&mut chat);
        for needle in [
            "thinking expanded",
            "tools expanded",
            "subagents collapsed",
            "activity expanded",
            "folded collapsed",
        ] {
            assert!(frame.contains(needle), "{needle} is missing: {frame}");
        }
    }

    /// The defaults are the old module's (`SectionVisibility::default`) with
    /// one departure — activity — because on this surface those lines are the
    /// answers `/usage`, `/context` and `/jobs` give.
    #[test]
    fn the_default_visibility_follows_the_old_module() {
        let chat = chat();
        assert_eq!(chat.details.mode("thinking"), Some(SectionMode::Expanded));
        assert_eq!(chat.details.mode("tools"), Some(SectionMode::Expanded));
        assert_eq!(chat.details.mode("subagents"), Some(SectionMode::Collapsed));
        assert_eq!(chat.details.mode("activity"), Some(SectionMode::Expanded));
        assert_eq!(chat.details.mode("folded"), Some(SectionMode::Collapsed));
        assert_eq!(chat.details.mode("bogus"), None);
    }

    /// `cycle` walks the three modes the old `SectionVisibility::apply` walked
    /// (hidden → collapsed → expanded), and a word that is not a mode leaves
    /// the state where it was.
    #[test]
    fn details_cycles_and_refuses_what_it_cannot_read() {
        let mut chat = chat();
        command(&mut chat, "/details tools cycle");
        assert_eq!(chat.details.mode("tools"), Some(SectionMode::Hidden));
        command(&mut chat, "/details tools cycle");
        assert_eq!(chat.details.mode("tools"), Some(SectionMode::Collapsed));
        command(&mut chat, "/details tools cycle");
        assert_eq!(chat.details.mode("tools"), Some(SectionMode::Expanded));

        command(&mut chat, "/details bogus expanded");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("no such section or mode"), "{frame}");
        assert_eq!(chat.details.mode("tools"), Some(SectionMode::Expanded));
    }

    /// Subagent chatter is the section the default folds, and the header says
    /// how much is behind it.
    #[test]
    fn subagents_collapse_behind_their_header_by_default() {
        let mut chat = chat();
        chat.on_event(EngineEvent::AgentStarted {
            agent_id: "agent-1".into(),
            name: "worker".into(),
            parent_id: None,
            kind: titi_engine::protocol::AgentKind::Subagent,
        });
        let frame = frame_text(&mut chat);
        assert!(frame.contains("▸ subagents (1)"), "{frame}");
        assert!(!frame.contains("agent worker: started"), "{frame}");

        command(&mut chat, "/details subagents expanded");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("agent worker: started"), "{frame}");
    }

    /// Reasoning is a section like any other: the live row is drawn while
    /// thinking is expanded and gone when it is hidden.
    #[test]
    fn the_reasoning_row_is_the_thinking_section() {
        let mut chat = chat();
        chat.on_event(EngineEvent::ThinkingDelta {
            turn_id: titi_engine::TurnId(1),
            text: "weighing the options".into(),
        });
        assert!(
            frame_text(&mut chat).contains("weighing the options"),
            "reasoning is on screen while it is the newest thing"
        );
        command(&mut chat, "/details thinking hidden");
        let frame = frame_text(&mut chat);
        assert!(!frame.contains("weighing the options"), "{frame}");
    }

    /// A compaction leaves a divider and takes the history it folded off the
    /// screen: the point of the fold is a short transcript, not a note.
    #[test]
    fn a_compaction_folds_the_history_behind_a_divider() {
        let mut chat = chat();
        chat.push(LineKind::User, "the question that was folded".to_owned());
        chat.push(LineKind::Assistant, "the answer that was folded".to_owned());
        chat.on_event(EngineEvent::Compacted {
            turn_id: titi_engine::TurnId(1),
            folded: 14,
            tokens_before: 22_000,
            strategy: "digest".into(),
        });

        let frame = frame_text(&mut chat);
        assert!(frame.contains("▸ folded 14 turns · 22k tokens"), "{frame}");
        assert!(
            !frame.contains("the question that was folded"),
            "the folded history is not on screen: {frame}"
        );

        command(&mut chat, "/details folded expanded");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("▾ folded 14 turns · 22k tokens"), "{frame}");
        assert!(
            frame.contains("the question that was folded"),
            "expanding the fold brings the history back: {frame}"
        );

        command(&mut chat, "/details folded hidden");
        let frame = frame_text(&mut chat);
        assert!(!frame.contains("folded 14 turns"), "{frame}");
        assert!(!frame.contains("the question that was folded"), "{frame}");
    }

    /// The divider is furniture: the dim chip the theme gives a note's mark,
    /// with the chevron the fold's mode decides.
    #[test]
    fn the_fold_divider_is_drawn_from_the_payload() {
        let mut chat = chat();
        chat.on_event(EngineEvent::Compacted {
            turn_id: titi_engine::TurnId(3),
            folded: 2,
            tokens_before: 900,
            strategy: "digest".into(),
        });
        assert_eq!(fold_divider_label(2, 900), "folded 2 turns · 900 tokens");

        let rows = frame_rows(&mut chat, 80, 24);
        let (x, y) = cell_of(&rows, "▸ folded 2 turns").expect("the divider is on screen");
        let buffer = frame_buffer(&mut chat, 80, 24);
        assert_eq!(
            buffer[(x, y)].fg,
            fg(&test_theme(), ThemeColor::Dim)
                .fg
                .unwrap_or(Color::Reset),
            "the divider is drawn as the furniture it is"
        );

        command(&mut chat, "/details folded expanded");
        let rows = frame_rows(&mut chat, 80, 24);
        assert!(
            rows.iter().any(|row| row.contains("▾ folded 2 turns")),
            "an expanded fold opens its chevron: {rows:?}"
        );
    }

    /// Two compactions keep their own numbers: each divider is the payload of
    /// its own event, and only the newest one stands for history that is still
    /// on the transcript.
    #[test]
    fn each_fold_divider_carries_its_own_event() {
        let mut chat = chat();
        chat.push(LineKind::User, "the oldest question".to_owned());
        chat.on_event(EngineEvent::Compacted {
            turn_id: titi_engine::TurnId(1),
            folded: 3,
            tokens_before: 1_500,
            strategy: "digest".into(),
        });
        chat.push(LineKind::Assistant, "an answer between folds".to_owned());
        chat.on_event(EngineEvent::Compacted {
            turn_id: titi_engine::TurnId(2),
            folded: 40,
            tokens_before: 120_000,
            strategy: "digest".into(),
        });

        let frame = frame_text(&mut chat);
        assert!(frame.contains("▸ folded 40 turns · 120k tokens"), "{frame}");
        assert!(!frame.contains("folded 3 turns"), "{frame}");

        command(&mut chat, "/details folded expanded");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("folded 3 turns · 1.5k tokens"), "{frame}");
        assert!(frame.contains("folded 40 turns · 120k tokens"), "{frame}");
        assert!(frame.contains("the oldest question"), "{frame}");
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
        // Its own kind, not `Tool`: subagent chatter is a section `/details`
        // can fold away, and a section needs lines it can tell apart.
        assert_eq!(chat.lines.last().unwrap().kind, LineKind::Agent);
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

    fn assistant_line(text: &str) -> TranscriptLine {
        TranscriptLine {
            kind: LineKind::Assistant,
            text: text.to_owned(),
        }
    }

    /// A frame's buffer, for a test that has to read a cell's own style.
    fn frame_buffer(chat: &mut Chat, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
        terminal.backend().buffer().clone()
    }

    /// Where the first character of `needle` sits in a frame given as rows of
    /// symbols: the column is the characters before it, since a frame row
    /// carries no escapes.
    fn cell_of(rows: &[String], needle: &str) -> Option<(u16, u16)> {
        rows.iter().enumerate().find_map(|(y, row)| {
            row.find(needle)
                .map(|at| (row[..at].chars().count() as u16, y as u16))
        })
    }

    /// The span that holds `needle`, so a test can read a row's styling.
    fn span_with<'a>(rows: &'a [Line<'static>], needle: &str) -> &'a Span<'static> {
        rows.iter()
            .flat_map(|row| row.spans.iter())
            .find(|span| span.content.contains(needle))
            .unwrap_or_else(|| panic!("no span holds {needle:?}"))
    }

    fn reply_rows_of(text: &str, width: usize) -> Vec<Line<'static>> {
        message_rows(&assistant_line(text), width, &test_theme()).0
    }

    /// A heading is rendered as the theme's heading, and its `#` run is syntax:
    /// no hash reaches the screen. Level 2 carries bold, which the renderer
    /// emits as one combined escape and the frame has to read back as a style.
    #[test]
    fn a_heading_in_a_reply_is_styled_without_its_hashes() {
        let theme = test_theme();
        let rows = reply_rows_of("## What is here", 80);
        let texts = row_texts(&rows);
        assert!(
            texts.iter().all(|row| !row.contains('#')),
            "a hash reached the screen: {texts:?}"
        );
        let heading = span_with(&rows, "What is here");
        assert_eq!(heading.style.fg, fg(&theme, ThemeColor::MdHeading).fg);
        assert!(
            heading.style.has_modifier(Modifier::BOLD),
            "level 2 has no bold: {:?}",
            heading.style
        );
        // The label and the gutter are the plain block's, unchanged.
        assert!(texts[0].starts_with("  titi │ "), "{texts:?}");
        assert_eq!(
            span_with(&rows, "titi").style,
            fg(&theme, ThemeColor::Accent).add_modifier(Modifier::BOLD)
        );
    }

    /// A fenced block is drawn as the renderer's box, with the language named
    /// once on the top rule; the fence itself never reaches the screen, and the
    /// box is narrower than the pane the reply is drawn in.
    #[test]
    fn a_fenced_block_in_a_reply_is_a_box() {
        let theme = test_theme();
        let reply = "before\n\n```rust\nlet plan = 1;\n```\n\nafter";
        let rows = reply_rows_of(reply, 80);
        let texts = row_texts(&rows);
        assert!(
            texts.iter().all(|row| !row.contains("```")),
            "a fence reached the screen: {texts:?}"
        );
        let top = texts
            .iter()
            .find(|row| row.contains("╭"))
            .unwrap_or_else(|| panic!("no box in {texts:?}"));
        assert!(top.contains(" rust "), "the language is not named: {top:?}");
        assert_eq!(
            texts.iter().filter(|row| row.contains(" rust ")).count(),
            1,
            "the language is named more than once: {texts:?}"
        );
        assert_eq!(
            texts.iter().filter(|row| row.contains('╰')).count(),
            1,
            "the box is not closed: {texts:?}"
        );
        assert!(texts.iter().any(|row| row.contains("let plan = 1;")));
        assert_eq!(
            span_with(&rows, "let plan = 1;").style.fg,
            fg(&theme, ThemeColor::MdCodeBlock).fg
        );
        assert_eq!(
            span_with(&rows, "╭").style.fg,
            fg(&theme, ThemeColor::MdCodeBlockBorder).fg
        );
    }

    /// A reply that is not markdown is the plain block, byte for byte: the same
    /// spans, in the same styles, as the rendering the screen had before the
    /// renderer existed.
    #[test]
    fn a_plain_reply_is_the_block_it_always_was() {
        let theme = test_theme();
        let rows = reply_rows_of("done", 80);
        assert_eq!(
            rows,
            vec![Line::from(vec![
                Span::styled("  ", page(&theme)),
                Span::styled(
                    "titi",
                    fg(&theme, ThemeColor::Accent).add_modifier(Modifier::BOLD)
                ),
                Span::styled(" │ ", fg(&theme, ThemeColor::Accent)),
                Span::styled("done", fg(&theme, ThemeColor::Text)),
            ])]
        );
        // A long plain answer wraps exactly as `speech` wraps it, row for row.
        let long = "word ".repeat(40);
        assert_eq!(
            reply_rows_of(&long, 60),
            speech(
                "titi",
                ThemeColor::Accent,
                ThemeColor::Text,
                Surface::Page,
                &long,
                60,
                &theme
            )
        );
        // Arithmetic and identifiers are not emphasis.
        for plain in ["2 * 3 * 4", "the snake_case_name field", "a_trailing_"] {
            assert_eq!(
                reply_rows_of(plain, 80),
                speech(
                    "titi",
                    ThemeColor::Accent,
                    ThemeColor::Text,
                    Surface::Page,
                    plain,
                    80,
                    &theme
                ),
                "{plain:?}"
            );
        }
    }

    /// Inside a markdown reply, a styled run ends where its marker does: the
    /// text after `**bold**` carries the pane's own colour, not the bold run's.
    #[test]
    fn a_bold_run_ends_where_its_marker_does() {
        let rows = reply_rows_of("**bold** and plain", 80);
        assert_eq!(row_texts(&rows)[0], "  titi │ bold and plain");
        let bold = span_with(&rows, "bold");
        assert!(bold.style.has_modifier(Modifier::BOLD), "{:?}", bold.style);
        assert_eq!(bold.style.fg, None, "bold carries no colour of its own");
        let plain = span_with(&rows, "and plain");
        assert!(
            !plain.style.has_modifier(Modifier::BOLD),
            "the reset was lost: {:?}",
            plain.style
        );
    }

    /// A markdown reply is drawn inside the pane at every width, and its body
    /// starts in the same column a plain reply's body does.
    #[test]
    fn a_markdown_reply_stays_inside_the_pane() {
        let reply = "## Title\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\n> quoted\n\nA paragraph that is long enough to wrap more than once at a narrow width, so the wrap has to be measured against the pane the row is drawn in.";
        for width in [60usize, 80, 120] {
            let rows = reply_rows_of(reply, width);
            let texts = row_texts(&rows);
            assert!(texts.len() > 4, "{width}: {texts:?}");
            for (at, row) in texts.iter().enumerate() {
                assert!(
                    titi_tui::width::visible_width(row) <= width,
                    "{width}: row {at} is {} wide: {row:?}",
                    titi_tui::width::visible_width(row)
                );
            }
            assert!(texts[0].starts_with("  titi │ Title"), "{width}: {texts:?}");
            for row in &texts[1..] {
                assert!(
                    row.starts_with("       │ ") || row.trim().is_empty(),
                    "{width}: a continuation row lost the gutter: {row:?}"
                );
            }
        }
    }

    /// The frame itself carries the reply's styling: the cells under a heading,
    /// a code body and a list bullet hold the tokens the renderer assigned them,
    /// and no syntax character reaches the screen.
    #[test]
    fn a_frame_draws_the_reply_with_the_renderers_tokens() {
        let theme = test_theme();
        let mut chat = chat();
        let reply = "## Title\n\n- one\n\n```rust\nlet plan = 1;\n```";
        chat.push(LineKind::Assistant, reply.to_owned());
        let width = 80u16;
        let height = 24u16;
        let buffer = frame_buffer(&mut chat, width, height);
        let symbols: Vec<String> = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let text = symbols.join("\n");
        for syntax in ["##", "```", "- one"] {
            assert!(
                !text.contains(syntax),
                "{syntax:?} reached the screen:\n{text}"
            );
        }
        assert!(text.contains("  titi │ Title"), "{text}");
        assert!(text.contains("let plan = 1;"), "{text}");

        // Where a cell of a named run sits, it carries that run's token.
        for (needle, token) in [
            ("Title", ThemeColor::MdHeading),
            ("let plan = 1;", ThemeColor::MdCodeBlock),
            ("•", ThemeColor::MdListBullet),
        ] {
            let (x, y) = cell_of(&symbols, needle)
                .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{text}"));
            assert_eq!(
                buffer[(x, y)].fg,
                fg(&theme, token).fg.unwrap_or(Color::Reset),
                "{needle:?} is not drawn in {token:?}"
            );
        }
    }

    /// The rows kept for the newest reply are only used for the reply they were
    /// rendered from, at the width they were rendered for: an answer of the
    /// same length, and the same answer in a wider pane, are both re-rendered.
    #[test]
    fn a_kept_reply_is_not_re_used_for_another_answer_or_width() {
        let mut chat = chat();
        let theme = test_theme();
        let first = chat.assistant_rows(true, "# alpha", 40, &theme);
        let same = chat.assistant_rows(true, "# alpha", 40, &theme);
        assert_eq!(first, same, "the reply was rendered differently");
        let other = chat.assistant_rows(true, "# bravo", 40, &theme);
        assert_ne!(first, other, "another reply re-used the kept rows");
        assert!(row_texts(&other).iter().any(|row| row.contains("bravo")));

        // A wider pane wraps the same answer into fewer rows, so a pane that
        // changed width cannot have been served from the kept rows.
        let long = "word ".repeat(40);
        let long = long.trim();
        let narrow = chat.assistant_rows(true, long, 40, &theme);
        let wide = chat.assistant_rows(true, long, 80, &theme);
        assert_ne!(narrow, wide, "a wider pane re-used the kept rows");
        assert!(
            narrow.len() > wide.len(),
            "the narrower pane wrapped into fewer rows: {} vs {}",
            narrow.len(),
            wide.len()
        );
    }

    /// An `edit` result the tool reports with a diff becomes a diff line: the
    /// chip row names the file, and the rows under it are the renderer's, in
    /// the diff tokens.
    #[test]
    fn an_edit_result_is_drawn_as_its_file_and_its_diff() {
        let theme = test_theme();
        let mut chat = chat();
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "edit".into(),
            detail: None,
        });
        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            output: "edited".into(),
            is_error: false,
            detail: Some(edit_result().into()),
        });
        let line = chat.transcript().last().expect("a line").clone();
        assert_eq!(line.kind, LineKind::Diff, "{:?}", line);
        assert_eq!(
            line.text,
            edit_result(),
            "the diff line carries the detail, and only the detail"
        );

        let rows = message_rows(&line, 80, &theme).0;
        let texts = row_texts(&rows);
        assert!(texts[0].contains("notes/kept.txt"), "{texts:?}");
        assert_eq!(texts[0], "   ✓ notes/kept.txt", "{texts:?}");
        assert!(
            texts.iter().any(|row| row.contains("removed_line")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|row| row.contains("added_line")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|row| row.contains("@@ -1,3 +1,3 @@")),
            "the hunk header is drawn: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|row| row.contains("+++ ") || row.contains("--- ")),
            "the file headers are the chip's job: {texts:?}"
        );

        // The frame's own cells carry the diff tokens.
        let buffer = frame_buffer(&mut chat, 80, 24);
        let symbols: Vec<String> = (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        for (needle, token) in [
            ("removed_line", ThemeColor::ToolDiffRemoved),
            ("added_line", ThemeColor::ToolDiffAdded),
            ("line one", ThemeColor::ToolDiffContext),
        ] {
            let (x, y) =
                cell_of(&symbols, needle).unwrap_or_else(|| panic!("{needle:?} is not on screen"));
            assert_eq!(
                buffer[(x, y)].fg,
                fg(&theme, token).fg.unwrap_or(Color::Reset),
                "{needle:?} is not drawn in {token:?}"
            );
        }
    }

    /// The diff an edit reports as its detail, and the tests draw.
    fn edit_result() -> &'static str {
        "--- a/notes/kept.txt\n\
         +++ b/notes/kept.txt\n\
         @@ -1,3 +1,3 @@\n line one\n-removed_line\n+added_line\n line three\n"
    }

    /// The detail decides, not the tool's name or the shape of the answer: a
    /// result that carries one is a diff line even from a tool nobody expects a
    /// diff from, and an `edit` that reports none — or an answer that merely
    /// looks like a diff — stays the chip it always was.
    #[test]
    fn only_a_result_with_a_detail_becomes_a_diff_line() {
        let cases: [(&str, &str, Option<&str>, bool); 4] = [
            (
                "read",
                "the file\n-removed_line\n+added_line\n",
                None,
                false,
            ),
            ("edit", "edited", None, false),
            ("edit", "edited", Some(edit_result()), true),
            (
                "write",
                "wrote out.txt",
                Some("--- /dev/null\n+++ b/out.txt\n@@ -0,0 +1,1 @@\n+one\n"),
                true,
            ),
        ];
        for (tool, output, detail, expected) in cases {
            let mut chat = chat();
            chat.on_event(EngineEvent::ToolStarted {
                turn_id: TurnId(1),
                call_id: "c1".into(),
                name: tool.into(),
                detail: None,
            });
            chat.on_event(EngineEvent::ToolFinished {
                turn_id: TurnId(1),
                call_id: "c1".into(),
                output: output.into(),
                is_error: false,
                detail: detail.map(Into::into),
            });
            let line = chat.transcript().last().expect("a line");
            assert_eq!(
                line.kind == LineKind::Diff,
                expected,
                "{tool}: {:?}",
                line.kind
            );
            if let Some(detail) = detail
                && expected
            {
                assert_eq!(line.text, detail, "{tool}: the detail is the line");
            }
            // The chip the screen draws when there is no detail keeps saying
            // what it always said.
            if !expected {
                let rows = message_rows(line, 80, &test_theme()).0;
                assert!(
                    row_texts(&rows)[0].starts_with("   ✓ "),
                    "{tool}: {:?}",
                    row_texts(&rows)
                );
            }
        }
    }

    /// A tool result that is not a diff is the chip it has always been, byte
    /// for byte: the same spans, in the same styles.
    #[test]
    fn a_result_without_a_diff_is_the_chip_it_always_was() {
        let theme = test_theme();
        let rows = message_rows(
            &TranscriptLine {
                kind: LineKind::Tool,
                text: "tool done  read".to_owned(),
            },
            80,
            &theme,
        )
        .0;
        assert_eq!(
            rows,
            vec![Line::from(vec![
                Span::styled("   ", page(&theme)),
                Span::styled("✓", fg(&theme, ThemeColor::Success)),
                Span::styled(" ", page(&theme)),
                Span::styled("read", fg(&theme, ThemeColor::Muted)),
            ])]
        );
    }

    /// A diff line the renderer cannot read as a diff falls back to the plain
    /// chip: the result is still on screen, line by line, and nothing is
    /// invented.
    #[test]
    fn a_diff_line_that_is_not_a_diff_is_the_plain_chip() {
        let theme = test_theme();
        let line = TranscriptLine {
            kind: LineKind::Diff,
            text: "not a diff at all\nsecond line".to_owned(),
        };
        let rows = message_rows(&line, 80, &theme).0;
        assert_eq!(
            row_texts(&rows)[..2],
            ["   ✓ not a diff at all", "     second line"],
            "each line of the detail keeps its row under the chip"
        );
    }

    /// A diff block keeps every row inside the pane at 60, 80 and 120 columns:
    /// the renderer is handed the pane minus the block's inset.
    #[test]
    fn a_diff_block_stays_inside_the_pane() {
        let line = TranscriptLine {
            kind: LineKind::Diff,
            text: edit_result().to_owned(),
        };
        for width in [60usize, 80, 120] {
            let rows = message_rows(&line, width, &test_theme()).0;
            let texts = row_texts(&rows);
            assert!(texts.len() > 4, "{width}: {texts:?}");
            for (at, row) in texts.iter().enumerate() {
                assert!(
                    titi_tui::width::visible_width(row) <= width,
                    "{width}: row {at} is {} wide: {row:?}",
                    titi_tui::width::visible_width(row)
                );
            }
        }
    }

    /// A diff longer than the cap is cut where the cap says, and the row that
    /// says how much is left counts exactly what the screen did not draw.
    #[test]
    fn a_very_long_diff_is_capped_and_says_so() {
        let mut text = String::from("edited\n--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,300 @@\n");
        for line in 0..300 {
            text.push_str(&format!("+line {line}\n"));
        }
        let line = TranscriptLine {
            kind: LineKind::Diff,
            text,
        };
        let rows = row_texts(&message_rows(&line, 80, &test_theme()).0);
        // The chip row, `DIFF_MAX_ROWS` renderer rows, then the note.
        assert_eq!(rows.len(), DIFF_MAX_ROWS + 2, "{:?}", rows.len());
        let last = rows.last().expect("a note");
        assert_eq!(last.trim(), "… 101 more diff lines", "{last:?}");
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
    ///
    /// A cell a wide glyph draws across is left out: ratatui writes a blank
    /// there and the glyph itself covers both columns, so counting that cell
    /// would make a row measure one column wider than a terminal renders it.
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
                let mut row = String::new();
                let mut hidden = 0usize;
                for x in 0..width {
                    let symbol = buffer[(x, y)].symbol();
                    if hidden > 0 {
                        hidden -= 1;
                        continue;
                    }
                    hidden = titi_tui::width::visible_width(symbol).saturating_sub(1);
                    row.push_str(symbol);
                }
                row
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

    // ---- Mouse selection and copy ---------------------------------------

    /// Draw one frame and hand the terminal back, so a test can read the
    /// buffer (what a terminal would receive) and the chat's own rows (what a
    /// copy would carry).
    fn drawn(
        chat: &mut Chat,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = match ratatui::Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => panic!("test backend: {error}"),
        };
        assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
        terminal
    }

    /// A chat whose reply wraps over several rows at 40 columns, drawn once.
    ///
    /// The reply is markdown-less, so it goes through the plain speech path:
    /// the rows are the text, wrapping, and nothing else.
    fn wrapped_reply_chat() -> (Chat, ratatui::Terminal<ratatui::backend::TestBackend>) {
        let mut chat = chat();
        chat.push(LineKind::User, "wrap it".to_owned());
        chat.push(
            LineKind::Assistant,
            "the quick brown fox jumps over the lazy dog and keeps going".to_owned(),
        );
        let terminal = drawn(&mut chat, 40, 20);
        (chat, terminal)
    }
    /// The transcript rows holding `needle`, and where the transcript starts.
    fn selected_rows(chat: &Chat, needle: &str) -> usize {
        chat.last_rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is on screen: {:?}", chat.last_rows))
    }

    /// A drag over a wrapped reply copies its text: the wrapping is the
    /// screen's, so it is not in the copy; the gutter and the theme's colours
    /// are the screen's too, so they are not either.
    #[test]
    fn a_drag_over_a_wrapped_reply_copies_the_text() {
        let (mut chat, _terminal) = wrapped_reply_chat();
        let first = selected_rows(&chat, "the quick brown fox");
        let last = selected_rows(&chat, "going");
        assert!(last > first, "the reply wrapped: {:?}", chat.last_rows);
        let top = chat.transcript_top;

        // The body column: the nine cells of `  titi │ ` are the gutter.
        chat.mouse_press(9, top + first as u16);
        chat.mouse_drag(39, top + last as u16);
        let copied = chat.mouse_release().expect("a drag copies");

        assert!(!copied.contains('\u{1b}'), "no styling: {copied:?}");
        assert_eq!(copied.lines().count(), last - first + 1, "{copied:?}");
        assert!(copied.starts_with("the quick brown fox"), "{copied:?}");
        assert!(copied.ends_with("going"), "{copied:?}");
        for line in copied.lines() {
            assert_eq!(line, line.trim(), "no padding: {line:?}");
        }
        // The selection stands after the release, the way a terminal's does.
        assert!(chat.selection().is_some_and(|sel| !sel.active));
    }

    /// The frame paints the theme's `selectedBg` behind the selected cells and
    /// leaves the guttter and the rows outside the selection alone.
    #[test]
    fn the_selection_paints_the_selected_cells_with_selected_bg() {
        let (mut chat, mut terminal) = wrapped_reply_chat();
        let first = selected_rows(&chat, "the quick brown fox");
        let last = selected_rows(&chat, "going");
        let top = chat.transcript_top;

        chat.mouse_press(9, top + first as u16);
        chat.mouse_drag(39, top + last as u16);
        assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());

        let theme = Arc::clone(&chat.theme);
        let selected = bg(&theme, ThemeBg::SelectedBg);
        let page_bg = bg(&theme, ThemeBg::StatusLineBg);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(20u16, top + first as u16)].bg, selected);
        assert_eq!(buffer[(20u16, top + last as u16)].bg, selected);
        // The gutter is left of the selection's first column.
        assert_eq!(buffer[(4u16, top + first as u16)].bg, page_bg);
        // And the row below the selection is untouched.
        assert_eq!(buffer[(20u16, top + last as u16 + 1)].bg, page_bg);
    }

    /// A click selects nothing, so nothing is copied; a drag never scrolls.
    #[test]
    fn a_click_copies_nothing_and_a_drag_does_not_scroll() {
        let (mut chat, _terminal) = wrapped_reply_chat();
        let first = selected_rows(&chat, "the quick brown fox");
        let top = chat.transcript_top;
        let before = chat.scroll_offset;

        chat.mouse_press(12, top + first as u16);
        assert!(chat.mouse_release().is_none(), "a click copies nothing");
        assert_eq!(chat.selection_text(), "");

        chat.mouse_press(12, top + first as u16);
        chat.mouse_drag(20, top + first as u16 + 1);
        assert!(chat.mouse_release().is_some());
        assert_eq!(chat.scroll_offset, before, "a drag does not scroll");
    }

    /// A key takes a standing selection away, and the wheel scrolls the
    /// transcript — but moves a panel's cursor while one is open.
    #[test]
    fn a_key_clears_the_selection_and_the_wheel_scrolls() {
        let mut chat = chat();
        for i in 0..50 {
            chat.push(LineKind::Note, format!("line {i}"));
        }
        drawn(&mut chat, 80, 20);
        chat.mouse_press(2, 3);
        chat.mouse_drag(8, 5);
        assert!(chat.selection().is_some());
        chat.on_key(Key::Char('x'), Instant::now());
        assert!(chat.selection().is_none(), "a key clears the selection");

        let before = chat.scroll_offset;
        chat.mouse_wheel(1, Instant::now());
        assert_eq!(chat.scroll_offset, before + 1);
        chat.mouse_wheel(-1, Instant::now());
        assert_eq!(chat.scroll_offset, before);

        // An open slash list takes the wheel as its cursor, not the transcript.
        chat.input.clear();
        chat.on_key(Key::Char('/'), Instant::now());
        assert!(chat.picking(), "input {:?}", chat.input);
        let scroll = chat.scroll_offset;
        chat.mouse_wheel(1, Instant::now());
        assert_eq!(chat.scroll_offset, scroll, "the wheel moved the list");
    }

    /// `/mouse off` persists the preset and queues the sequence that turns
    /// reporting off; a preset is the one the next run reads back.
    #[test]
    fn slash_mouse_persists_the_preset_and_switches_reporting_over() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        type_text(&mut chat, "/mouse off");
        chat.on_key(Key::Enter, Instant::now());

        assert_eq!(chat.mouse_preset(), MousePreset::Off);
        assert_eq!(
            crate::session_fs::load_mouse_preset_from(dir.path()),
            Some(MousePreset::Off)
        );
        let flush = chat.take_output_flush();
        assert!(flush.contains("\x1b[?1002l"), "drags off: {flush:?}");
        assert!(flush.contains("\x1b[?1003l"), "motion off: {flush:?}");
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("mouse: off")),
            "the screen says so: {:?}",
            chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
        );

        // A preset that is not one says what there is and changes nothing.
        let before = chat.mouse_preset();
        type_text(&mut chat, "/mouse sideways");
        chat.on_key(Key::Enter, Instant::now());
        assert_eq!(chat.mouse_preset(), before);
        assert!(chat.take_output_flush().is_empty());
    }

    /// The clipboard writer is found on the search path it is given, and the
    /// copy it makes is the one a paste would find.
    #[test]
    fn a_copy_with_an_os_writer_goes_to_it() {
        let dir = tempfile::tempdir().expect("temp");
        let out = dir.path().join("copied.txt");
        let bin = dir.path().join("pbcopy");
        std::fs::write(&bin, format!("#!/bin/sh\ncat > {}\n", out.display())).expect("write");
        let mut perms = std::fs::metadata(&bin).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod");

        let mut chat = chat();
        copy_selection(
            &mut chat,
            "hello clipboard",
            &dir.path().display().to_string(),
        );
        assert!(
            chat.take_output_flush().is_empty(),
            "a tool took it, so nothing goes out as OSC 52"
        );
        assert_eq!(
            std::fs::read_to_string(&out).expect("the copy landed"),
            "hello clipboard"
        );
        assert!(chat.hint.contains("pbcopy"), "{:?}", chat.hint);
    }

    /// With no writer on the path, the copy goes out as OSC 52 — the route a
    /// terminal over SSH can still reach.
    #[test]
    fn a_copy_with_no_os_writer_goes_out_as_osc52() {
        let mut chat = chat();
        copy_selection(&mut chat, "over ssh", "/nonexistent");
        assert_eq!(
            chat.take_output_flush(),
            titi_tui::caps::osc52_copy("over ssh")
        );
        assert!(chat.hint.contains("OSC 52"), "{:?}", chat.hint);
    }

    /// The capability check is a property of the path it is handed.
    #[test]
    fn the_clipboard_writer_is_the_first_one_on_the_path() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().display().to_string();
        assert_eq!(clipboard_writer(&path), None);
        std::fs::write(dir.path().join("wl-copy"), b"").expect("write");
        assert_eq!(
            executable_path("wl-copy", &path),
            Some(dir.path().join("wl-copy"))
        );
        assert_eq!(executable_path("pbcopy", &path), None);
        assert_eq!(
            clipboard_writer(&path).map(|(bin, _, program)| (bin, program)),
            Some(("wl-copy", dir.path().join("wl-copy")))
        );
    }

    // ---- Terminal appearance --------------------------------------------

    /// A white reply (light) and a black one (dark), as the wire carries them.
    const LIGHT_REPLY: &[u8] = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
    const DARK_REPLY: &[u8] = b"\x1b]11;rgb:0000/0000/0000\x07";

    fn surface_hex(theme: &Theme) -> String {
        theme.get_bg_hex(ThemeBg::StatusLineBg)
    }

    /// An OSC 11 reply moves the screen to the palette that appearance's slot
    /// holds; the same appearance twice is not a second repaint, and a payload
    /// that is not a reply changes nothing.
    #[test]
    fn an_osc11_reply_moves_the_palette_to_that_slot() {
        let _guard = theme_lock();
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.theme = test_theme();
        chat.set_starting_appearance(Appearance::Dark);
        let light = crate::themes::theme_named("light").expect("the crate carries light");
        let dark = test_theme();
        assert_ne!(surface_hex(&light), surface_hex(&dark));

        assert_eq!(
            chat.ingest_probe_reply(LIGHT_REPLY),
            ProbeOutcome::ThemeChanged
        );
        assert_eq!(surface_hex(&chat.theme), surface_hex(&light));
        assert_eq!(
            chat.ingest_probe_reply(LIGHT_REPLY),
            ProbeOutcome::Unchanged,
            "the same appearance is already on screen"
        );
        assert_eq!(
            chat.ingest_probe_reply(DARK_REPLY),
            ProbeOutcome::ThemeChanged
        );
        assert_eq!(surface_hex(&chat.theme), surface_hex(&dark));
        assert_eq!(
            chat.ingest_probe_reply(b"11;rgb:nonsense"),
            ProbeOutcome::Unchanged
        );
    }

    /// The slot's own choice wins over the crate's pick for that appearance,
    /// and an explicit `--theme` is not the probe's to move.
    #[test]
    fn the_slot_choice_wins_and_a_theme_flag_keeps_the_palette() {
        let _guard = theme_lock();
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.theme = test_theme();
        chat.set_starting_appearance(Appearance::Dark);
        let workspace = crate::session_fs::current_workspace();
        let mut settings =
            titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
        settings
            .set(
                titi_config::settings::THEME_LIGHT_KEY,
                serde_json::json!("amethyst"),
            )
            .expect("the light slot is written");
        let chosen = crate::themes::theme_named("amethyst").expect("the crate carries amethyst");

        assert_eq!(
            chat.ingest_probe_reply(LIGHT_REPLY),
            ProbeOutcome::ThemeChanged
        );
        assert_eq!(surface_hex(&chat.theme), surface_hex(&chosen));

        // And with the loop off — `--theme` — nothing moves at all.
        chat.set_appearance_auto(false);
        assert_eq!(
            chat.ingest_probe_reply(DARK_REPLY),
            ProbeOutcome::Unchanged,
            "the palette the run was started with stands"
        );
        assert_eq!(surface_hex(&chat.theme), surface_hex(&chosen));
    }

    /// The reply arrives on the keyboard's own stream: it is reassembled and
    /// swallowed, and a key typed in the same window still reaches the
    /// composer.
    #[test]
    fn a_probe_reply_arrives_as_keys_and_never_reaches_the_composer() {
        let _guard = theme_lock();
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.theme = test_theme();
        chat.set_starting_appearance(Appearance::Dark);
        let light = crate::themes::theme_named("light").expect("the crate carries light");

        let now = Instant::now();
        chat.on_focus_gained(now);
        assert!(
            chat.take_output_flush()
                .contains(titi_tui::caps::OSC11_QUERY),
            "the focus gain asks the terminal"
        );

        // A key typed in the window is a key, not a reply.
        let typed = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!chat.absorb_probe_key(&typed, now), "a plain key is a key");

        // What the terminal answers with, as the event layer reads it: the
        // ESC of `ESC ]` comes back as an alt-modified `]`.
        // The BEL of `\x1b]11;rgb:…\x07` is a C0 byte the event layer reads as
        // the control chord it is a key code for: ctrl-`g`.
        let mut reply: Vec<(char, KeyModifiers)> = "]11;rgb:ffff/ffff/ffff"
            .chars()
            .enumerate()
            .map(|(at, ch)| {
                let modifiers = if at == 0 {
                    KeyModifiers::ALT
                } else {
                    KeyModifiers::NONE
                };
                (ch, modifiers)
            })
            .collect();
        reply.push(('g', KeyModifiers::CONTROL));
        for (ch, modifiers) in reply {
            let key = KeyEvent::new(KeyCode::Char(ch), modifiers);
            assert!(
                chat.absorb_probe_key(&key, now),
                "the reply's {ch:?} is swallowed"
            );
        }
        assert_eq!(chat.input, "", "no character of the reply was typed");
        assert_eq!(surface_hex(&chat.theme), surface_hex(&light));

        // The window closed with the reply: a later alt-`]` is not a reply.
        let late = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        assert!(!chat.absorb_probe_key(&late, now));
        // And neither is one after the window has run out.
        chat.on_focus_gained(now);
        let later = now + PROBE_REPLY_WINDOW + Duration::from_millis(1);
        let late = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
        assert!(!chat.absorb_probe_key(&late, later), "the window expired");
    }

    /// A mode-2031 notification is a re-query trigger: the reply to the fresh
    /// query is what decides the palette.
    #[test]
    fn a_mode_2031_report_asks_for_a_fresh_query() {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.set_starting_appearance(Appearance::Dark);
        assert_eq!(
            chat.ingest_probe_reply(b"\x1b[?997;1n"),
            ProbeOutcome::NeedOsc11Query
        );
        assert_eq!(
            chat.ingest_probe_reply(b"\x1b[?997;2n"),
            ProbeOutcome::NeedOsc11Query
        );
    }

    // ---- Prompt history -------------------------------------------------

    /// A chat whose session exists in its own store, so the prompts it is asked
    /// land where the history reads them from (the live run's own path: the
    /// session is made before the screen opens).
    fn history_chat() -> (tempfile::TempDir, Chat) {
        let dir = tempfile::tempdir().expect("temp");
        let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
        let session_id = store
            .create(titi_core::session::SessionMeta::default())
            .expect("a session");
        let mut chat = Chat::new("openai/gpt-4.1", &session_id, test_theme());
        chat.agent_dir = dir.path().to_path_buf();
        (dir, chat)
    }

    /// Ask `chat` something the way the live run does: the key, then the store
    /// write the run records (`record`), so the session's own history has the
    /// prompt and nothing else does.
    fn ask(chat: &mut Chat, log: &Option<SessionLog>, prompt: &str) {
        type_text(chat, prompt);
        let applied = chat.on_key(Key::Enter, Instant::now());
        record(chat, log, applied.log);
    }

    /// ↑ at an empty composer opens the browser over the session's own prompts,
    /// newest first; a query narrows it; Enter puts the chosen prompt in the
    /// composer and sends nothing; Esc leaves the text alone.
    #[test]
    fn the_history_browser_puts_a_past_prompt_in_the_composer_unsent() {
        let (_dir, mut chat) = history_chat();
        let log = SessionLog::open(&chat.agent_dir, &chat.session_id);
        ask(&mut chat, &log, "first prompt");
        ask(&mut chat, &log, "second prompt");
        assert!(chat.input.is_empty(), "a submit clears the composer");

        // ↑ at the empty composer: the browser, not the transcript's scroll.
        let opened = chat.on_key(Key::Up, Instant::now());
        assert!(opened.effect.is_none(), "opening sends nothing");
        assert!(chat.history_picker.is_some(), "the browser is up");
        let frame = frame_rows(&mut chat, 80, 20).join("\n");
        assert!(frame.contains("history · 2"), "{frame}");
        assert!(frame.contains("second prompt"), "{frame}");
        assert!(frame.contains("first prompt"), "{frame}");
        // Newest first *in the panel*: the transcript above it holds the same
        // two strings in the order they were asked.
        let panel = &frame[frame.find("history · 2").expect("the browser")..];
        let newest = panel.find("second prompt").expect("newest listed");
        let oldest = panel.find("first prompt").expect("oldest listed");
        assert!(newest < oldest, "newest first: {panel}");

        // Typing narrows it, the way the model browser does.
        type_text(&mut chat, "first");
        let narrowed = frame_rows(&mut chat, 80, 20).join("\n");
        let panel = &narrowed[narrowed.find("history · ").expect("the browser")..];
        assert!(panel.contains("history · 1 of 2 · first"), "{panel}");
        assert!(!panel.contains("second prompt"), "{panel}");
        assert!(panel.contains("first prompt"), "{panel}");

        // Enter takes the row into the composer, and the turn is not started.
        let taken = chat.on_key(Key::Enter, Instant::now());
        assert!(taken.effect.is_none(), "a pick never sends");
        assert!(taken.log.is_none(), "and never logs");
        assert!(chat.history_picker.is_none(), "the browser closed");
        assert_eq!(chat.input, "first prompt");

        // Ctrl+R opens it again, and Esc closes without touching the draft.
        chat.on_key(Key::CtrlR, Instant::now());
        assert!(chat.history_picker.is_some());
        type_text(&mut chat, "second");
        chat.on_key(Key::Esc, Instant::now());
        assert!(chat.history_picker.is_none());
        assert_eq!(chat.input, "first prompt", "Esc left the draft alone");
    }

    /// The browser lists the session's prompts, not the screen's lines: a note
    /// or an assistant reply is not a prompt, and a session that was never
    /// asked anything says so rather than opening an empty panel.
    #[test]
    fn the_history_is_the_sessions_prompts_and_an_empty_one_says_so() {
        let (_dir, mut chat) = history_chat();
        chat.push(LineKind::Note, "/help".to_owned());
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "not a prompt".into(),
        });
        chat.on_key(Key::CtrlR, Instant::now());
        assert!(chat.history_picker.is_none(), "nothing to browse");
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.contains("history: this session has no prompts")),
            "{:?}",
            chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
        );

        // A prompt the session did carry is listed once, whole.
        let log = SessionLog::open(&chat.agent_dir, &chat.session_id);
        ask(&mut chat, &log, "what did I ask earlier");
        chat.input.clear();
        chat.on_key(Key::CtrlR, Instant::now());
        let frame = frame_rows(&mut chat, 80, 20).join("\n");
        let panel = &frame[frame.find("history · 1").expect("the browser")..];
        assert!(panel.contains("what did I ask earlier"), "{panel}");
    }
    /// Alt+Backspace and Ctrl+W delete a word at a time; an empty composer (or
    /// one holding only spaces) costs nothing.
    #[test]
    fn delete_word_takes_the_word_before_the_caret() {
        let mut chat = chat();
        type_text(&mut chat, "fix the parser now");
        chat.on_key(Key::DeleteWord, Instant::now());
        assert_eq!(chat.input, "fix the parser ");
        chat.on_key(Key::DeleteWord, Instant::now());
        assert_eq!(chat.input, "fix the ");
        type_text(&mut chat, "now   ");
        chat.on_key(Key::DeleteWord, Instant::now());
        assert_eq!(chat.input, "fix the ", "the spaces go with the word");

        // Nothing to delete is not an error and not a panic.
        chat.input.clear();
        chat.on_key(Key::DeleteWord, Instant::now());
        assert_eq!(chat.input, "");
        chat.input = "   ".to_owned();
        chat.on_key(Key::DeleteWord, Instant::now());
        assert_eq!(chat.input, "");

        // A wide character is one character, not two cells' worth of bytes.
        chat.input = "日本 語".to_owned();
        chat.on_key(Key::DeleteWord, Instant::now());
        assert_eq!(chat.input, "日本 ");
    }

    /// A stored session with one question in it, named the way the index
    /// remembers a name a person or the namer gave it — a title at creation is
    /// the placeholder a session is made with, not a name (`needs_auto_title`).
    fn seed_session(agent_dir: &Path, title: &str, question: &str) -> String {
        let store = titi_core::session::SessionStore::new(agent_dir).expect("session store");

        let id = store
            .create(titi_core::session::SessionMeta {
                title: Some(title.to_owned()),
                ..Default::default()
            })
            .expect("create");
        store.append(&id, Role::User, question).expect("append");
        titi_core::session::SessionIndex::open(&agent_dir.join("state.db"))
            .expect("index")
            .set_title(&id, title)
            .expect("title");
        id
    }

    /// `/sessions` bare is the list Ctrl+X opens: the same rows, the same
    /// switch, the same title — one list, two ways in.
    #[test]
    fn sessions_bare_opens_the_ctrl_x_list() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let older = seed_session(dir.path(), "older", "older question");
        let newer = seed_session(dir.path(), "newer", "newer question");

        command(&mut chat, "/sessions");
        assert!(chat.session_picker.is_some(), "the list is up, as Ctrl+X");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("sessions · 2"), "{frame}");
        assert!(frame.contains(&older) && frame.contains(&newer), "{frame}");
        assert!(chat.session_search.is_none(), "the list is not the search");

        // And Enter takes the row the cursor is on through the same switch.
        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_ne!(chat.session_id, "session-123", "the screen moved");
        assert!(
            matches!(
                applied.effect,
                Some(ChatEffect::Send(EngineCommand::RestoreHistory { .. }))
            ),
            "and the engine was told to replay it"
        );
    }

    /// `/sessions <query>` searches the entries the FTS index holds — the
    /// capability that had no caller — and a hit row names the session, when it
    /// was written, and the line that matched.
    #[test]
    fn sessions_query_finds_the_matching_line_and_enter_switches() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let kafka = seed_session(
            dir.path(),
            "kafka-talk",
            "how do we size the kafka consumers",
        );
        seed_session(
            dir.path(),
            "postgres-talk",
            "which postgres index does the planner pick",
        );

        command(&mut chat, "/sessions kafka");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("kafka-talk"), "{frame}");
        assert!(
            frame.contains("how do we size the kafka consumers"),
            "the matching line is the row: {frame}"
        );
        assert!(
            !frame.contains("postgres-talk") && !frame.contains("the planner pick"),
            "the session that did not match is not offered: {frame}"
        );
        assert!(
            frame.contains("ago") || frame.contains("just now"),
            "the row says when the session was written: {frame}"
        );

        let applied = chat.on_key(Key::Enter, Instant::now());
        assert_eq!(
            chat.session_id, kafka,
            "Enter switches to the hit's session"
        );
        assert!(
            matches!(
                applied.effect,
                Some(ChatEffect::Send(EngineCommand::RestoreHistory { .. }))
            ),
            "through the switch the list uses"
        );
        assert!(chat.session_search.is_none(), "and the picker closes");
    }

    /// No hits says so, and a row says the id when the index holds no real
    /// title for that session.
    #[test]
    fn sessions_query_reports_no_hits_and_falls_back_to_the_id() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        let store = titi_core::session::SessionStore::new(dir.path()).expect("session store");
        let unnamed = store
            .create(titi_core::session::SessionMeta {
                title: Some("titi".to_owned()),
                ..Default::default()
            })
            .expect("create");
        store
            .append(&unnamed, Role::User, "a shared word about sockets")
            .expect("append");

        command(&mut chat, "/sessions zzzz-nothing-matches");
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("no sessions match \"zzzz-nothing-matches\""),
            "{frame}"
        );
        // Enter with nothing to take says so instead of closing in silence.
        chat.on_key(Key::Enter, Instant::now());
        assert!(chat.session_search.is_none(), "the panel closes");
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("sessions: no match for \"zzzz-nothing-matches\""),
            "{frame}"
        );

        // The session nobody named is offered by its id, not by the product
        // name it was created with.
        command(&mut chat, "/sessions sockets");
        let frame = frame_text(&mut chat);
        assert!(frame.contains(&unnamed), "the id is the row: {frame}");
        assert!(
            !frame.contains("titi ·") && !frame.contains("titi  ✓"),
            "the placeholder title is not a name: {frame}"
        );
    }

    /// The query narrows as it is typed, the way the model, theme and history
    /// browsers narrow theirs: a character re-runs it, a backspace takes one
    /// back, the first Esc clears it, the second closes.
    #[test]
    fn the_session_query_narrows_as_it_is_typed() {
        let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        seed_session(dir.path(), "kafka-talk", "kafka consumer groups");
        command(&mut chat, "/sessions kafka");
        assert!(frame_text(&mut chat).contains("kafka-talk"));

        chat.on_key(Key::Char('x'), Instant::now());
        let frame = frame_text(&mut chat);
        assert!(frame.contains("no sessions match \"kafkax\""), "{frame}");
        assert!(!frame.contains("kafka-talk"), "{frame}");

        chat.on_key(Key::Backspace, Instant::now());
        assert!(
            frame_text(&mut chat).contains("kafka-talk"),
            "backspace brings the hit back"
        );

        chat.on_key(Key::Esc, Instant::now());
        assert!(
            chat.session_search.is_some(),
            "the first Esc clears the query, it does not close"
        );
        assert!(
            frame_text(&mut chat).contains("sessions · 1"),
            "and the cleared query is the whole list"
        );
        chat.on_key(Key::Esc, Instant::now());
        assert!(chat.session_search.is_none(), "the second Esc closes");
    }

    /// The age a row carries is computed from the two clocks, so it is testable
    /// without a wall clock of its own.
    #[test]
    fn the_age_label_is_a_shape_not_a_clock() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(age_label(now, 1_000_000), "just now");
        assert_eq!(age_label(now, 999_941), "just now", "under a minute");
        assert_eq!(age_label(now, 999_940), "1m ago");
        assert_eq!(age_label(now, 1_000_000 - 3_600), "1h ago");
        assert_eq!(age_label(now, 1_000_000 - 86_400), "1d ago");
        assert_eq!(age_label(now, 1_000_000 - 604_800), "1w ago");
        // A file stamped ahead of this machine's clock is not a negative age.
        assert_eq!(age_label(now, 1_000_001), "just now");
    }
}
