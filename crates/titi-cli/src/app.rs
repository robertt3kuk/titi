//! TUI app composition for titi-cli: first frame + transcript accordion.
//!
//! Terminal-independent core — tests drive it with an in-memory render; the
//! binary wraps it with crossterm raw mode + alternate screen.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use titi_engine::protocol::SessionMode;
use titi_engine::{AgentKind as EngineAgentKind, AgentStatus as EngineAgentStatus, EngineEvent};
use titi_tui::caps::{MousePreset, Rgb};
use titi_tui::component::Component as _;
use titi_tui::composer::{
    Composer, PASTE_INLINE_MAX_LINES, PasteResult, QueueMode, Queued, render_box_composer,
};
use titi_tui::history::{BatchKind, HistoryBatch};
use titi_tui::hub::{AgentKind, AgentStatus, HubCommand, HubPeer, HubRoster};
use titi_tui::keybindings::{KeybindingsManager, default_manager};
use titi_tui::markdown::{Section, SectionMode, render_markdown};
use titi_tui::overlay::{Anchor, composite_rows_inset};
use titi_tui::panels::{
    ApprovalPanel, CompletionPanel, SelectionPanel, SessionAction, SessionSwitcher,
};
use titi_tui::recap::{Recap, RecapSection};
use titi_tui::renderer::FramePlan;
use titi_tui::selection::Selection;
use titi_tui::slash::{Route, SlashRegistry};
use titi_tui::space_hold::{SpaceHold, SpaceHoldOutcome, delete_before_cursor};
use titi_tui::status::AgentState;
use titi_tui::status_bar::{live_snapshot, render_status_bar};
use titi_tui::theme::{
    Appearance, AppearanceEvent, AppearanceInputs, ColorMode, SymbolPreset, Theme,
    appearance_from_rgb, classify_appearance_bytes, global,
};
use titi_tui::transcript::{Alert, Entry, Transcript};

use crate::first_frame::{FirstFrame, SubmitOutcome};

/// Result of feeding a terminal appearance probe into the auto-theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceIngest {
    /// No OSC 11 / Mode 2031 payload, or a duplicate OSC 11 report.
    Unchanged,
    /// Auto-theme swapped (or first OSC 11 report while auto is on).
    ThemeChanged,
    /// Mode 2031 DSR — re-query OSC 11; do not treat 997 as luminance.
    NeedOsc11Query,
}

/// Speech-to-text capture state (omp `SttState`). Mic/ASR worker is stubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttState {
    Idle,
    Recording,
    Transcribing,
}

/// Composite app: startup state machine + transcript.
pub struct App {
    first_frame: FirstFrame,
    transcript: Transcript,
    theme: Arc<Theme>,
    width: u16,
    /// Viewport rows used to budget compact overlays (`plan_frame` / PTY size).
    height: u16,
    selection: Option<Selection>,
    /// The modal overlay panel currently shown, if any.
    overlay: Option<ActiveOverlay>,
    /// Paste collapse + attachment numbering state.
    composer: Composer,
    /// Session id awaiting close approval (`SessionAction::Close`).
    pending_close: Option<String>,
    /// Exec-tier tool call waiting on the approval overlay (`call_id`, `name`).
    pending_tool_approval: Option<(String, String)>,
    /// Slash-command registry (builtin names reserved, then file
    /// expansion, then passthrough to the LLM).
    slash: SlashRegistry,
    /// Slash autocomplete rows (painted inside the box composer).
    completion: CompletionPanel,
    /// A queued message pulled back into the editor via Alt+Up; shown
    /// highlighted until Esc clears the highlight (does not re-queue).
    highlighted: Option<Queued>,
    /// OMP `app.*` + TUI editor bindings (user YAML overrides applied).
    keys: KeybindingsManager,
    /// Index into [`model_choices`] for cycleForward/cycleBackward.
    current_model: usize,
    /// Model ids as of the last refresh. Kept as a snapshot because the
    /// status line reads it every frame; the refresh itself happens when the
    /// picker opens.
    available_models: Vec<String>,
    /// Where a refresh reads from, when a registry is behind the surface.
    model_catalog: Option<crate::engine::ModelCatalog>,
    /// Status-line `mode` segment. Set from the engine's `ModeChanged`, so
    /// it can only ever show a mode the turns are really running in.
    mode: SessionMode,
    /// Status-line collab/`live` badge (`app.live.toggle`).
    live_mode: bool,
    /// omp `stt.enabled` — gates hold-Space. Default false.
    stt_enabled: bool,
    stt_state: SttState,
    space_hold: SpaceHold,
    /// In-memory Agent Hub roster (Main is filtered at paint time).
    hub_peers: Vec<HubPeer>,
    /// Membership in the local hub broker, when `/join` connected.
    hub: crate::hub::HubSession,
    /// Submitted prompts for `app.history.search` / `app.retry`.
    prompt_history: Vec<String>,
    last_prompt: Option<String>,
    /// History-batch handshake (next id, acked prefix, in-flight batch).
    history_next_id: u64,
    history_acked: usize,
    history_pending: Option<HistoryBatch>,
    history_replay: bool,
    /// Last OSC 11 classification seen by this app instance.
    last_terminal_appearance: Option<Appearance>,
    /// Completed assistant messages rendered above diagnostic sections.
    assistant_messages: Vec<String>,
    /// Assistant text currently arriving from the engine stream.
    streaming_response: String,
    /// Live session id, so `/checkpoint` and `/rewind` can address it.
    session_id: Option<String>,
    /// A turn is in flight: submitting now steers it instead of queueing a
    /// whole new turn.
    turn_active: bool,
    /// When the first exit request arrived, so a second one can confirm it.
    exit_armed: Option<Instant>,
    /// Context window fill, 0–100, from the last `ContextUsage` event.
    context_pct: Option<u8>,
    /// The agent the view is on. `None` is the main turn.
    focused_agent: Option<String>,
    /// Conversation entries the surface must persist, in order. The App never
    /// touches the session store; the binary drains this after each event
    /// batch and appends it.
    session_writes: Vec<(titi_core::session::Role, String)>,
}

impl App {
    /// Create the app.  `ready` is flipped by the provider-init thread.
    pub fn new(_ready: Arc<AtomicBool>, banner: Vec<String>, theme: Arc<Theme>) -> Self {
        App {
            first_frame: FirstFrame::new(banner),
            transcript: Transcript::new(),
            theme,
            width: 80,
            height: 20,
            selection: None,
            overlay: None,
            composer: Composer::new(),
            pending_close: None,
            pending_tool_approval: None,
            slash: Self::default_slash_registry(),
            completion: CompletionPanel::new(),
            highlighted: None,
            keys: load_keybindings_manager(),
            current_model: 0,
            available_models: Vec::new(),
            model_catalog: None,
            mode: SessionMode::Agent,
            live_mode: false,
            stt_enabled: false,
            stt_state: SttState::Idle,
            space_hold: SpaceHold::new(),
            hub_peers: Vec::new(),
            hub: crate::hub::HubSession::default(),
            prompt_history: Vec::new(),
            last_prompt: None,
            history_next_id: 1,
            history_acked: 0,
            history_pending: None,
            history_replay: false,
            last_terminal_appearance: None,
            assistant_messages: Vec::new(),
            streaming_response: String::new(),
            session_id: None,
            turn_active: false,
            exit_armed: None,
            context_pct: None,
            focused_agent: None,
            session_writes: Vec::new(),
        }
    }

    /// What Herdr should be told.
    ///
    /// A turn in flight is `working`. Waiting on the user — a tool approval or
    /// the exit confirmation — is `blocked`, because that is when another agent
    /// should stop and look. Everything else is `idle`.
    pub fn herdr_state(&self) -> (crate::herdr::AgentState, Option<String>) {
        if self.overlay_open() && self.pending_tool_approval.is_some() {
            return (
                crate::herdr::AgentState::Blocked,
                Some("waiting for approval".to_owned()),
            );
        }
        if self.exit_armed() {
            return (
                crate::herdr::AgentState::Blocked,
                Some("confirm exit".to_owned()),
            );
        }
        if self.turn_active {
            return (crate::herdr::AgentState::Working, None);
        }
        (crate::herdr::AgentState::Idle, None)
    }

    /// The agent the view is on, when it is not the main turn.
    pub fn focused_agent(&self) -> Option<&str> {
        self.focused_agent.as_deref()
    }

    /// Whether the first exit request is still waiting for a second one.
    pub fn exit_armed(&self) -> bool {
        self.exit_armed.is_some()
    }

    /// Takes the conversation entries awaiting persistence, oldest first.
    pub fn drain_session_writes(&mut self) -> Vec<(titi_core::session::Role, String)> {
        std::mem::take(&mut self.session_writes)
    }

    /// Whether a turn is currently in flight.
    pub fn turn_active(&self) -> bool {
        self.turn_active
    }

    /// The built-in slash commands (names reserved — see the Slash DoD).
    fn default_slash_registry() -> SlashRegistry {
        let mut registry = SlashRegistry::new();
        registry.register_builtin("help", "Show available commands");
        registry.register_builtin("model", "Switch the active model");
        registry.register_builtin("sessions", "Open the session switcher");
        registry.register_builtin("agents", "Open the agent hub");
        registry.register_builtin(
            "mouse",
            "Set mouse tracking: off|on|wheel|buttons|all|toggle",
        );
        registry.register_builtin("details", "Toggle transcript section visibility");
        registry.register_builtin("pause", "Stop the agent and hold input until you resume");
        registry.register_builtin("hotkeys", "Show active keybinding chords");
        registry.register_builtin("switch", "Open the session switcher");
        registry.register_builtin("checkpoint", "Record a rewind point for this session");
        registry.register_builtin("checkpoints", "List this session's rewind points");
        registry.register_builtin(
            "rewind",
            "Rewind the session to a checkpoint (newest by default)",
        );
        registry.register_builtin("recap", "Session recap: turns, tools, files, problems");
        registry.register_builtin("goal", "Run the coder/reviewer goal loop");
        registry.register_builtin("hub", "Show or hide the hub roster");
        registry.register_builtin("join", "Join the local hub (usage: /join [name])");
        registry.register_builtin("leave", "Leave the local hub");
        registry
    }

    /// Bind the live session id (set once the engine has resumed or created it).
    pub fn set_session_id(&mut self, id: impl Into<String>) {
        self.session_id = Some(id.into());
    }

    /// The live session id, if the engine has reported one.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Moves the surface to another session: the rendered conversation belongs
    /// to the old one and is dropped. The caller owns the engine and the
    /// session store, so it must replace the replayed history and re-point the
    /// log itself.
    pub fn switch_to_session(&mut self, id: &str) {
        self.session_id = Some(id.to_owned());
        self.transcript.clear();
        self.assistant_messages.clear();
        self.streaming_response.clear();
        self.turn_active = false;
        self.set_alert(format!("session: {id}"));
    }

    /// Whether a modal overlay panel is currently shown.
    pub fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    /// OMP pause overlay (`/pause`).
    pub fn is_paused(&self) -> bool {
        matches!(self.overlay, Some(ActiveOverlay::Pause { closed: false }))
    }

    /// Drop the current overlay without extracting an outcome.
    pub fn close_overlay(&mut self) {
        self.overlay = None;
    }

    /// Effective keybindings (tests assert default chords).
    pub fn keys(&self) -> &KeybindingsManager {
        &self.keys
    }

    /// Whether the engine put this session in plan mode.
    pub fn plan_mode(&self) -> bool {
        self.mode == SessionMode::Plan
    }

    pub fn live_mode(&self) -> bool {
        self.live_mode
    }

    pub fn stt_state(&self) -> SttState {
        self.stt_state
    }

    pub fn set_stt_enabled(&mut self, enabled: bool) {
        self.stt_enabled = enabled;
    }

    /// Route a `/`-prefixed input line: builtin (reserved name), expanded
    /// file command, or passthrough to the LLM.  Never matches a plain
    /// prompt (no leading `/`).
    pub fn route_slash(&self, input: &str) -> Route {
        self.slash.route(input)
    }

    /// Refresh the floating completion panel for the current input:
    /// suggestions only while typing a bare `/name` (no arguments yet);
    /// anything else hides it.
    pub fn slash_completions(&mut self, input: &str) {
        if !input.starts_with('/') || input.contains(' ') {
            self.completion.hide();
            return;
        }
        self.completion.refresh(self.slash.complete(input));
    }

    /// Whether the completion panel is currently shown.
    pub fn completion_visible(&self) -> bool {
        self.completion.is_visible()
    }

    /// Move the completion highlight (Up/Down; wraps).
    pub fn completion_move(&mut self, up: bool) {
        if up {
            self.completion.move_up();
        } else {
            self.completion.move_down();
        }
    }

    /// Accept the highlighted completion: return its command name and hide
    /// the panel.  `None` when the panel is hidden/empty.
    pub fn completion_accept(&mut self) -> Option<String> {
        let name = self.completion.selected_name()?.to_owned();
        self.completion.hide();
        Some(name)
    }

    /// Hide the completion panel without touching the input buffer.
    pub fn completion_hide(&mut self) {
        self.completion.hide();
    }

    /// Show the model picker over the ids the catalog holds right now.
    ///
    /// The list is read here rather than kept from startup: a local server
    /// answers long after the first frame, and a picker that opens without
    /// its models is the whole bug. It is read once per opening, never per
    /// frame, because reading takes the registry's lock.
    pub fn open_model_picker(&mut self) {
        self.refresh_models();
        let models = self.model_choices();
        let labels = models.to_vec();
        self.overlay = Some(ActiveOverlay::ModelPicker(SelectionPanel::new(
            "Model", models, labels,
        )));
    }

    /// Show the session switcher: the live session first, then the ids
    /// stored under `<agent_dir>/sessions`.
    pub fn open_session_switcher(&mut self) {
        let mut titles = vec!["current".to_owned()];
        titles.extend(list_sessions());
        self.open_session_switcher_over(titles);
    }

    /// Show the session switcher over explicit titles (tests, embedded
    /// session sources).
    pub fn open_session_switcher_over(&mut self, titles: Vec<String>) {
        self.overlay = Some(ActiveOverlay::SessionSwitcher(SessionSwitcher::new(titles)));
    }

    /// Show an approval prompt; `approved` decides the pending action.
    pub fn open_approval(&mut self, prompt: &str) {
        self.overlay = Some(ActiveOverlay::Approval(ApprovalPanel::prompt(prompt)));
    }

    /// Queue a session close behind an approval prompt.  Esc / No / Cancel
    /// never deletes ([`App::confirm_pending_close`] runs only on Yes).
    pub fn request_session_close(&mut self, id: &str) {
        self.pending_close = Some(id.to_owned());
        self.open_approval(&format!("Close session {id}?"));
    }

    /// The session id awaiting close approval, if any.
    pub fn take_pending_close(&mut self) -> Option<String> {
        self.pending_close.take()
    }

    /// Open (or queue) an approval overlay for an exec-tier tool call.
    pub fn request_tool_approval(&mut self, call_id: &str, name: &str) {
        self.pending_tool_approval = Some((call_id.to_owned(), name.to_owned()));
        self.open_pending_tool_approval();
    }

    fn open_pending_tool_approval(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        if let Some((_, name)) = &self.pending_tool_approval {
            let prompt = format!("Run tool {name}?");
            self.open_approval(&prompt);
        }
    }

    /// Show the session recap over explicit sections (tests, embedded
    /// callers). `Ctrl+O` inside the panel expands or collapses every block.
    pub fn open_recap(&mut self, sections: Vec<RecapSection>) {
        self.overlay = Some(ActiveOverlay::Recap(Recap::new(sections)));
    }

    /// Show the recap of the live session, reading the store and trajectory.
    pub fn open_session_recap(&mut self) -> Result<(), String> {
        let Some(id) = self.session_id.clone() else {
            return Err("no live session".into());
        };
        let sections = crate::recap::build(&titi_config::agent_dir(), &id)?;
        self.open_recap(sections);
        Ok(())
    }

    /// `Ctrl+O`: open every transcript section, or close them all when they
    /// already are. Returns whether anything changed.
    pub fn toggle_all_details(&mut self) -> bool {
        let sections = [
            Section::Thinking,
            Section::Tools,
            Section::Subagents,
            Section::Activity,
        ];
        let all_expanded = sections
            .iter()
            .all(|section| self.transcript.mode(*section) == SectionMode::Expanded);
        self.details(if all_expanded {
            "collapsed"
        } else {
            "expanded"
        })
    }

    /// Agent Hub overlay (`app.agents.hub` / `app.session.observe`).
    pub fn open_agents_hub(&mut self) {
        self.overlay = Some(ActiveOverlay::Hub(HubRoster::with_theme(
            self.hub_peers.clone(),
            Arc::clone(&self.theme),
        )));
    }

    /// `/hub` — open the roster overlay, or close it if it is already up.
    pub fn toggle_agents_hub(&mut self) {
        if matches!(self.overlay, Some(ActiveOverlay::Hub(_))) {
            self.overlay = None;
            return;
        }
        self.open_agents_hub();
    }

    /// `/join [name]` — put this surface on the local hub roster.
    ///
    /// A missing broker is reported as a note, not an error: running
    /// without one is the ordinary case.
    pub fn join_hub(&mut self, name: &str) {
        self.join_hub_in(&titi_config::agent_dir(), name);
    }

    /// `/join` against a given agent directory. Tests point this at a
    /// temporary broker instead of the user's own.
    pub fn join_hub_in(&mut self, agent_dir: &std::path::Path, name: &str) {
        let name = match (name.trim(), self.session_id.as_deref()) {
            ("", Some(session)) => session.to_owned(),
            ("", None) => "titi".to_owned(),
            (given, _) => given.to_owned(),
        };
        match self.hub.join(agent_dir, &name) {
            Ok(()) => {
                self.sync_hub_roster();
                self.set_alert(format!("hub: joined as {name}"));
            }
            Err(reason) => self.set_alert(format!("hub: {reason}")),
        }
    }

    /// `/leave` — drop the hub connection, which unregisters this peer.
    pub fn leave_hub(&mut self) {
        match self.hub.leave() {
            Some(id) => {
                self.sync_hub_roster();
                self.set_alert(format!("hub: left as {id}"));
            }
            None => self.set_alert("hub: not joined"),
        }
    }

    /// Drains the broker's pushed events. Never blocks: the caller runs
    /// this on the same tick as everything else.
    pub fn poll_hub(&mut self) {
        for update in self.hub.poll() {
            match update {
                crate::hub::HubUpdate::Roster => self.sync_hub_roster(),
                crate::hub::HubUpdate::Message { from, to, message } => {
                    let scope = if to.is_some() { "" } else { " (all)" };
                    self.push_transcript(
                        Section::Activity,
                        format!("hub {from}{scope}: {message}"),
                    );
                    self.set_alert(format!("hub {from}: {message}"));
                }
                crate::hub::HubUpdate::Refused(reason) => {
                    self.set_alert(format!("hub: {reason}"));
                }
                crate::hub::HubUpdate::Disconnected => {
                    self.sync_hub_roster();
                    self.set_alert("hub: the broker went away");
                }
            }
        }
    }

    /// Mirrors the broker's roster into the overlay's rows.
    ///
    /// Broker peers are separate processes, so their rows are replaced
    /// wholesale rather than merged — a peer that left has to leave the
    /// roster with it. This session's own subagents are not the broker's
    /// to remove: they carry a `parent_id` and are kept as they were.
    fn sync_hub_roster(&mut self) {
        let mine = self.hub.agent_id().unwrap_or_default().to_owned();
        let mut rows: Vec<HubPeer> = self
            .hub_peers
            .iter()
            .filter(|peer| peer.parent_id.is_some())
            .cloned()
            .collect();
        for id in self.hub.peers() {
            if rows.iter().any(|row| &row.id == id) {
                continue;
            }
            rows.push(HubPeer {
                id: id.clone(),
                display_name: id.clone(),
                kind: if id == &mine {
                    AgentKind::Main
                } else {
                    AgentKind::Sub
                },
                parent_id: None,
                status: AgentStatus::Idle,
            });
        }
        self.set_hub_peers(rows);
        if let Some(ActiveOverlay::Hub(roster)) = &mut self.overlay {
            *roster = HubRoster::with_theme(self.hub_peers.clone(), Arc::clone(&self.theme));
        }
    }

    /// Replace the in-memory hub roster (tests / future broker ingest).
    pub fn set_hub_peers(&mut self, peers: Vec<HubPeer>) {
        self.hub_peers = peers;
    }

    /// Current hub roster, including `Main` if the caller injected it.
    pub fn hub_peers(&self) -> &[HubPeer] {
        &self.hub_peers
    }

    fn merge_hub_peers(&mut self, visible: &[HubPeer]) {
        for peer in visible {
            if let Some(slot) = self.hub_peers.iter_mut().find(|p| p.id == peer.id) {
                *slot = peer.clone();
            } else {
                self.hub_peers.push(peer.clone());
            }
        }
    }

    fn open_help(&mut self) {
        let items: Vec<(String, String)> = self
            .slash
            .catalog()
            .into_iter()
            .filter(|c| !c.shadowed)
            .map(|c| (c.name.clone(), format!("/{}  {}", c.name, c.description)))
            .collect();
        let (ids, labels): (Vec<String>, Vec<String>) = items.into_iter().unzip();
        self.overlay = Some(ActiveOverlay::Help(SelectionPanel::new(
            "Help", ids, labels,
        )));
    }

    fn open_hotkeys(&mut self) {
        let ids = self.keys.actions();
        let labels: Vec<String> = ids
            .iter()
            .map(|id| {
                let keys = self.keys.get_keys(id).join(" ");
                format!("{id}  {keys}")
            })
            .collect();
        self.overlay = Some(ActiveOverlay::Hotkeys(SelectionPanel::new(
            "Hotkeys", ids, labels,
        )));
    }

    fn open_history_search(&mut self) {
        if self.prompt_history.is_empty() {
            self.set_alert("history: empty");
            return;
        }
        let items = self.prompt_history.clone();
        let labels = items.clone();
        self.overlay = Some(ActiveOverlay::HistorySearch(SelectionPanel::new(
            "History", items, labels,
        )));
    }

    /// Route a decoded key to the open overlay.  Returns the panel's
    /// outcome once it closes; `None` while it stays open or no overlay is
    /// shown.  A switcher `Close` never returns directly — it opens the
    /// approval prompt ([`App::request_session_close`]), and only an
    /// explicit Yes reaches the deletion.
    pub fn overlay_input(&mut self, data: &str) -> Option<OverlayOutcome> {
        let mut active = self.overlay.take()?;
        active.handle_input(data);
        if let ActiveOverlay::Hub(h) = &mut active {
            self.merge_hub_peers(h.peers());
            if let Some(command) = h.take_pending_command() {
                self.overlay = Some(active);
                return Some(match command {
                    HubCommand::Revive(id) => OverlayOutcome::HubRevive(id),
                    HubCommand::Stop(id) => OverlayOutcome::HubStop(id),
                });
            }
        }
        if !active.is_closed() {
            self.overlay = Some(active);
            return None;
        }
        // Intercept the switcher's Close: resolve the session id and gate
        // the deletion behind the approval panel.
        if let ActiveOverlay::SessionSwitcher(s) = &active
            && let Some(SessionAction::Close(i)) = s.action()
            && let Some(id) = s.titles().get(*i).cloned()
        {
            self.request_session_close(&id);
            return None;
        }
        let outcome = match active.outcome() {
            Some(OverlayOutcome::Approval(approved)) if self.pending_close.is_some() => {
                if !approved {
                    self.pending_close = None;
                }
                Some(OverlayOutcome::Approval(approved))
            }
            Some(OverlayOutcome::Approval(approved)) => {
                if let Some((call_id, _)) = self.pending_tool_approval.take() {
                    Some(OverlayOutcome::ToolApproval { call_id, approved })
                } else {
                    Some(OverlayOutcome::Approval(approved))
                }
            }
            other => other,
        };
        self.open_pending_tool_approval();
        outcome
    }

    /// Append a bracketed paste to the input buffer.  Multi-line pastes are
    /// inserted as one block (never executed line-by-line); pastes longer
    /// than [`PASTE_INLINE_MAX_LINES`] collapse to an inline preview; a
    /// single image path becomes an `[Image #N]` attachment marker.
    pub fn paste(&mut self, text: &str) -> String {
        match self.composer.ingest_paste(text, PASTE_INLINE_MAX_LINES) {
            PasteResult::Text(text) => text,
            PasteResult::Collapsed {
                preview,
                omitted_lines,
            } => format!("{preview}\n… (+{omitted_lines} lines)"),
            PasteResult::Attachment { marker, .. } => marker,
        }
    }

    /// Apply an OSC 11 / Mode 2031 probe reply (omp live appearance ingest).
    ///
    /// Mode 2031 is a re-query trigger, not a luminance source. Zellij-on-macOS
    /// still ignores OSC 11 inside [`detect_terminal_background`].
    pub fn ingest_probe_reply(&mut self, bytes: &[u8]) -> AppearanceIngest {
        match classify_appearance_bytes(bytes) {
            Some(AppearanceEvent::Osc11(mode)) => self.apply_terminal_appearance(mode),
            Some(AppearanceEvent::Mode2031Requery) => AppearanceIngest::NeedOsc11Query,
            None => AppearanceIngest::Unchanged,
        }
    }

    /// Feed a probed OSC 11 RGB triple into auto-theme (Capabilities::bg).
    pub fn apply_bg_rgb(&mut self, rgb: Rgb) -> AppearanceIngest {
        self.apply_terminal_appearance(appearance_from_rgb(rgb.r, rgb.g, rgb.b))
    }

    fn apply_terminal_appearance(&mut self, mode: Appearance) -> AppearanceIngest {
        if self.last_terminal_appearance == Some(mode) {
            return AppearanceIngest::Unchanged;
        }
        self.last_terminal_appearance = Some(mode);

        let inputs = AppearanceInputs::from_env();
        if !global().on_terminal_appearance_change(mode, &inputs) {
            return AppearanceIngest::Unchanged;
        }
        if let Some(theme) = global().current() {
            self.theme = theme;
        }
        AppearanceIngest::ThemeChanged
    }

    /// Set the terminal width (resize).
    pub fn resize(&mut self, width: u16) {
        self.width = width;
    }

    /// Current width.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Viewport size used by compact overlays.
    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height.max(1);
    }

    /// Mouse press: anchor a drag-select at (x, y).
    pub fn mouse_press(&mut self, x: u16, y: u16) {
        self.selection = Some(Selection::anchor(x, y));
    }

    /// Mouse drag: move the selection cursor.
    pub fn mouse_drag(&mut self, x: u16, y: u16) {
        if let Some(sel) = &mut self.selection {
            sel.drag(x, y);
        }
    }

    /// Mouse release: commit the selection.
    pub fn mouse_release(&mut self) {
        if let Some(sel) = &mut self.selection {
            sel.release();
        }
    }

    /// Clear the active selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The active selection, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Agent state.
    pub fn state(&self) -> AgentState {
        self.first_frame.state()
    }

    /// Provider readiness.
    pub fn is_ready(&self) -> bool {
        self.first_frame.is_ready()
    }

    /// Queued prompts count.
    pub fn queue_len(&self) -> usize {
        self.first_frame.queue_len()
    }

    /// Apply a `/details` directive to the transcript.
    pub fn details(&mut self, directive: &str) -> bool {
        let changed = self.transcript.details(directive);
        if changed && self.transcript.all_hidden() {
            self.transcript.set_alert(Alert {
                text: "all sections hidden — use /details to show a section".into(),
            });
        } else if changed {
            self.transcript.clear_alert();
        }
        changed
    }

    /// All transcript sections hidden.
    pub fn all_hidden(&self) -> bool {
        self.transcript.all_hidden()
    }

    /// Append a transcript entry (thinking/tools/subagents/activity).
    pub fn push_transcript(&mut self, section: Section, text: impl Into<String>) {
        self.transcript.push(Entry::new(section, text));
    }

    /// Apply one engine event to the terminal presentation model.
    pub fn ingest_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::TurnStarted { model, .. } => {
                self.streaming_response.clear();
                self.turn_active = true;
                self.set_alert(format!("{model} · running"));
            }
            EngineEvent::StreamDelta { text, .. } => self.streaming_response.push_str(&text),
            EngineEvent::ThinkingDelta { text, .. } => {
                self.push_transcript(Section::Thinking, text.to_string())
            }
            EngineEvent::ToolStarted { name, call_id, .. } => {
                self.push_transcript(Section::Tools, format!("{name} · {call_id} · running"))
            }
            EngineEvent::ToolApprovalNeeded { name, call_id, .. } => {
                self.push_transcript(
                    Section::Tools,
                    format!("{name} · {call_id} · waiting for approval"),
                );
                self.request_tool_approval(&call_id, &name);
            }
            EngineEvent::ToolFinished {
                call_id,
                output,
                is_error,
                ..
            } => {
                let status = if is_error { "failed" } else { "done" };
                self.push_transcript(Section::Tools, format!("{call_id} · {status}\n{output}"));
            }
            EngineEvent::AgentStarted {
                agent_id,
                name,
                parent_id,
                kind,
            } => {
                let kind = match kind {
                    EngineAgentKind::Subagent => AgentKind::Sub,
                    EngineAgentKind::Advisor => AgentKind::Advisor,
                };
                let peer = HubPeer {
                    id: agent_id.to_string(),
                    display_name: name.to_string(),
                    kind,
                    parent_id: parent_id
                        .map(|id| id.to_string())
                        .or_else(|| Some("Main".to_owned())),
                    status: AgentStatus::Running,
                };
                self.merge_hub_peers(&[peer]);
                self.push_transcript(Section::Subagents, format!("{name} · started"));
            }
            EngineEvent::AgentProgress { agent_id, text } => {
                self.push_transcript(Section::Subagents, format!("{agent_id} · {text}"))
            }
            EngineEvent::AgentStatusChanged { agent_id, status } => {
                let status = match status {
                    EngineAgentStatus::Running => AgentStatus::Running,
                    EngineAgentStatus::Idle | EngineAgentStatus::Completed => AgentStatus::Idle,
                    EngineAgentStatus::Parked => AgentStatus::Parked,
                    EngineAgentStatus::Aborted | EngineAgentStatus::Failed => AgentStatus::Aborted,
                };
                if let Some(peer) = self.hub_peers.iter_mut().find(|peer| peer.id == agent_id) {
                    peer.status = status;
                }
            }
            EngineEvent::AgentFocused { agent_id } => {
                self.focused_agent = agent_id.map(|id| id.to_string());
                let label = self
                    .focused_agent
                    .clone()
                    .unwrap_or_else(|| "main".to_owned());
                self.set_alert(format!("focused: {label}"));
            }
            EngineEvent::AgentFinished {
                agent_id,
                summary,
                success,
            } => {
                if let Some(peer) = self.hub_peers.iter_mut().find(|peer| peer.id == agent_id) {
                    peer.status = if success {
                        AgentStatus::Idle
                    } else {
                        AgentStatus::Aborted
                    };
                }
                self.push_transcript(Section::Subagents, format!("{agent_id} · {summary}"));
            }
            EngineEvent::ModelSwitched { from, to, .. } => {
                self.push_transcript(Section::Activity, format!("model fallback: {from} → {to}"));
                self.set_alert(format!("model: {to}"));
            }
            EngineEvent::ContextUsage { tokens, window, .. } => {
                self.context_pct = Some(if window == 0 {
                    0
                } else {
                    ((tokens.saturating_mul(100)) / window).min(100) as u8
                });
            }
            EngineEvent::Compacted {
                folded,
                tokens_before,
                strategy,
                ..
            } => {
                // Into the tools section, which is visible by default: the user
                // should know the agent just lost the start of the session.
                self.push_transcript(
                    Section::Tools,
                    format!(
                        "compaction · {strategy} · folded {folded} message(s) at ~{tokens_before} tokens"
                    ),
                );
                self.set_alert(format!("context compacted ({strategy})"));
            }
            EngineEvent::TurnFinished { .. } => {
                if !self.streaming_response.is_empty() {
                    let reply = std::mem::take(&mut self.streaming_response);
                    self.session_writes
                        .push((titi_core::session::Role::Assistant, reply.clone()));
                    self.assistant_messages.push(reply);
                }
                self.turn_active = false;
                self.transcript.clear_alert();
            }
            EngineEvent::Failed { message, .. } => {
                self.turn_active = false;
                self.set_alert(format!("error: {message}"));
            }
            EngineEvent::Cancelled { .. } => {
                self.turn_active = false;
                self.set_alert("cancelled");
            }
            EngineEvent::PromptReturned { text } => {
                // The cancelled turn's queue is not replayed, so the text has
                // to come back where the user can see and resend it.
                self.push_transcript(Section::Activity, format!("not sent: {text}"));
                self.set_alert(format!("cancelled · queued prompt returned: {text}"));
            }
            EngineEvent::GoalFinished { report } => {
                self.set_alert(report.to_string());
            }
            EngineEvent::CouncilFinished { report } => {
                // The council's report is a block — header, the fold, then one
                // line per member — so it goes where it can be read, not into
                // a one-line alert.
                self.push_transcript(Section::Activity, report.to_string());
                self.set_alert("council answered");
            }
            EngineEvent::Notice { message } => {
                self.push_transcript(Section::Activity, message.to_string());
                self.set_alert(message.to_string());
            }
            EngineEvent::ContextBreakdown { parts, window } => {
                let total: u64 = parts.iter().map(|part| part.tokens).sum();
                let mut body = String::from("context · token estimates, not provider counts");
                for part in &parts {
                    body.push_str(&format!("\n{} · ~{} tokens", part.label, part.tokens));
                }
                body.push_str(&format!("\ntotal · ~{total} of {window} tokens"));
                self.push_transcript(Section::Activity, body);
            }
            EngineEvent::SessionNamed { title, .. } => {
                self.set_alert(format!("session: {title}"));
            }
            EngineEvent::TurnUsage { .. } => {}
            EngineEvent::MemoryResult { output } => {
                self.push_transcript(Section::Activity, output.to_string());
            }
            EngineEvent::JobStarted { job } => {
                self.push_transcript(
                    Section::Activity,
                    format!(
                        "job {} · every {}s · {}",
                        job.id, job.interval_secs, job.prompt
                    ),
                );
                self.set_alert(format!("{} started", job.id));
            }
            EngineEvent::JobList { jobs } => {
                if jobs.is_empty() {
                    self.push_transcript(Section::Activity, "no background jobs".to_owned());
                }
                for job in jobs {
                    self.push_transcript(
                        Section::Activity,
                        format!(
                            "job {} · every {}s · ran {} · {}",
                            job.id, job.interval_secs, job.runs, job.prompt
                        ),
                    );
                }
            }
            EngineEvent::JobFinished { job_id } => {
                self.push_transcript(Section::Activity, format!("job {job_id} stopped"));
                self.set_alert(format!("{job_id} stopped"));
            }
            EngineEvent::AdvisorAnswer { text } if text.trim().is_empty() => {
                self.set_alert("failed consult: the advisor answered with nothing");
            }
            EngineEvent::AdvisorAnswer { text } => {
                self.push_transcript(Section::Activity, format!("advisor · {}", text.trim()));
                self.set_alert("advisor answered");
            }
            EngineEvent::AdvisorFailed { reason } => {
                self.push_transcript(Section::Activity, format!("failed consult: {reason}"));
                self.set_alert(format!("failed consult: {reason}"));
            }
            EngineEvent::BudgetUpdated { .. } => {}
            EngineEvent::BudgetExceeded { spent, limit } => {
                self.turn_active = false;
                self.push_transcript(
                    Section::Activity,
                    format!("budget reached: {spent} of {limit} tokens"),
                );
                self.set_alert(format!("budget reached: {spent} of {limit} tokens"));
            }
            EngineEvent::ModeChanged { mode } => {
                self.mode = mode;
                self.set_alert(format!("mode: {}", mode.label()));
            }
        }
    }

    /// Set the floating alert directly.
    pub fn set_alert(&mut self, text: impl Into<String>) {
        self.transcript.set_alert(Alert { text: text.into() });
    }

    /// Submit a prompt; queued while starting, delivered when ready.
    pub fn submit(&mut self, prompt: String) -> SubmitOutcome {
        self.first_frame.submit(prompt)
    }

    /// Flush queued prompts after provider readiness; returns them.
    pub fn flush_queued(&mut self, ready: &AtomicBool) -> Vec<String> {
        if ready.load(Ordering::SeqCst) && !self.is_ready() {
            self.first_frame.provider_ready()
        } else {
            Vec::new()
        }
    }

    /// Queue a message for the stream (Steer / FollowUp).
    pub fn push_queued(&mut self, text: impl Into<String>, mode: QueueMode) {
        self.composer.push_queue(text.into(), mode);
    }

    /// Number of messages waiting in the stream queue.
    pub fn stream_queue_len(&self) -> usize {
        self.composer.queue_len()
    }

    /// Pull the last queued message back into the editor (Alt+Up).  The
    /// returned text is marked highlighted — Esc clears the highlight
    /// without re-queueing.  `None` when the queue is empty.
    pub fn pull_last_queued(&mut self) -> Option<String> {
        let queued = self.composer.dequeue_last()?;
        self.highlighted = Some(queued.clone());
        Some(queued.text)
    }

    /// Whether a queued message is currently highlighted in the editor.
    pub fn queue_highlighted(&self) -> bool {
        self.highlighted.is_some()
    }

    /// Clear the queued-message highlight without deleting the text (Esc).
    pub fn clear_highlight(&mut self) {
        self.highlighted = None;
    }

    /// Active model id shown in the status bar.
    pub fn model(&self) -> String {
        self.model_choices()
            .get(self.current_model)
            .cloned()
            .unwrap_or_else(|| "no-model".to_owned())
    }

    /// Apply a picker selection to the cycle index.
    pub fn apply_model(&mut self, model: &str) {
        if let Some(i) = self.model_choices().iter().position(|m| m == model) {
            self.current_model = i;
        }
    }

    /// Replace the runtime model catalog used by picker and cycle actions.
    pub fn set_available_models(&mut self, models: Vec<String>) {
        self.available_models = models;
        if self.current_model >= self.model_choices().len() {
            self.current_model = 0;
        }
    }

    /// Point the surface at a live catalog; `open_model_picker` reads it.
    pub fn set_model_catalog(&mut self, catalog: crate::engine::ModelCatalog) {
        self.model_catalog = Some(catalog);
        self.refresh_models();
    }

    fn refresh_models(&mut self) {
        let Some(catalog) = &self.model_catalog else {
            return;
        };
        let ids = catalog.ids();
        if ids.is_empty() {
            return;
        }
        self.set_available_models(ids);
    }

    fn model_choices(&self) -> Vec<String> {
        if self.available_models.is_empty() {
            model_choices()
        } else {
            self.available_models.clone()
        }
    }

    fn cycle_model(&mut self, forward: bool) {
        let n = self.model_choices().len();
        if n == 0 {
            return;
        }
        self.current_model = if forward {
            (self.current_model + 1) % n
        } else {
            (self.current_model + n - 1) % n
        };
    }

    fn status_fill(&self, composer_width: u16) -> u16 {
        composer_width.saturating_sub(6)
    }

    fn render_layers(&mut self, input: &str) -> FrameLayers {
        let banner = self.first_frame.banner().to_vec();
        let mut transcript = Vec::new();
        for message in &self.assistant_messages {
            transcript.extend(render_markdown(message, &self.theme, self.width));
        }
        if !self.streaming_response.is_empty() {
            transcript.extend(render_markdown(
                &self.streaming_response,
                &self.theme,
                self.width,
            ));
        }
        transcript.extend(self.transcript.render(self.width, &self.theme));
        let session_label = self
            .focused_agent
            .clone()
            .unwrap_or_else(|| "session".to_owned());
        let mut snap = live_snapshot(&self.model(), &session_label);
        snap.context_pct = self.context_pct;
        if self.mode != SessionMode::Agent {
            snap.mode = Some(self.mode.label().to_owned());
        }
        if self.live_mode {
            snap.collab = Some("live".to_owned());
        }
        let fill = self.status_fill(self.width);
        let status = render_status_bar(&self.theme, fill, &snap);
        let inner = self.completion.item_rows();
        let show_cursor = self.overlay.is_none();
        let composer = render_box_composer(
            &self.theme,
            self.width,
            &status,
            input,
            self.queue_highlighted(),
            show_cursor,
            &inner,
        );
        FrameLayers {
            banner,
            transcript,
            composer,
        }
    }

    fn apply_chrome(&mut self, mut rows: Vec<String>, margin_bottom: usize) -> Vec<String> {
        if let Some(sel) = &self.selection {
            rows = sel.apply_background(&rows, &self.theme);
        }
        let budget = compact_item_budget(self.height as usize, margin_bottom);
        if let Some(active) = &mut self.overlay {
            active.set_max_visible(budget);
        }
        let w = self.width;
        let overlay_rows = match &mut self.overlay {
            Some(active) => active.render(w),
            None => Vec::new(),
        };
        if !overlay_rows.is_empty() {
            rows =
                composite_rows_inset(&rows, &overlay_rows, w, Anchor::BottomCenter, margin_bottom);
        }
        rows
    }

    /// Render the full frame: banner, transcript accordion, box composer.
    /// Does **not** pad to terminal height (that broke transcript tests).
    pub fn render(&mut self) -> Vec<String> {
        let layers = self.render_layers("");
        let margin = layers.composer.len();
        let mut rows = layers.banner;
        if !rows.is_empty() {
            rows.push(String::new());
        }
        rows.extend(layers.transcript);
        rows.extend(layers.composer);
        self.apply_chrome(rows, margin)
    }

    /// Viewport-diff plan: overflow transcript becomes a history batch;
    /// the viewport is padded to `height` so compact overlays sit above
    /// the composer.
    pub fn plan_frame(&mut self, input: &str, height: u16) -> FramePlan {
        self.height = height.max(1);
        let layers = self.render_layers(input);
        let composer = layers.composer;
        let composer_h = composer.len();
        let mut above = Vec::new();
        above.extend(layers.banner);
        if !above.is_empty() {
            above.push(String::new());
        }
        above.extend(layers.transcript);

        let view_above = (self.height as usize).saturating_sub(composer_h);
        let mut history = None;

        if self.history_replay {
            let id = self.history_next_id;
            self.history_next_id = self.history_next_id.saturating_add(1);
            let batch = HistoryBatch {
                id,
                rows: above.clone(),
                kind: BatchKind::Replay,
            };
            self.history_pending = Some(batch.clone());
            history = Some(batch);
            self.history_acked = above.len();
            self.history_replay = false;
        } else if above.len() > view_above {
            let overflow = above.len() - view_above;
            if overflow > self.history_acked {
                let new_rows = above[self.history_acked..overflow].to_vec();
                if !new_rows.is_empty() {
                    let id = self.history_next_id;
                    self.history_next_id = self.history_next_id.saturating_add(1);
                    let batch = HistoryBatch {
                        id,
                        rows: new_rows,
                        kind: BatchKind::Append,
                    };
                    self.history_pending = Some(batch.clone());
                    history = Some(batch);
                }
            }
            above = above[overflow..].to_vec();
        }

        while above.len() < view_above {
            above.insert(0, String::new());
        }
        if above.len() > view_above {
            let skip = above.len() - view_above;
            above = above[skip..].to_vec();
        }
        above.extend(composer);
        let mut viewport = self.apply_chrome(above, composer_h);
        let target = self.height as usize;
        if viewport.len() > target {
            viewport.truncate(target);
        }
        while viewport.len() < target {
            viewport.push(String::new());
        }
        FramePlan { history, viewport }
    }

    /// Confirm that history batch `id` was written.
    pub fn acknowledge_history(&mut self, id: u64) {
        if let Some(pending) = self.history_pending.take() {
            if pending.id == id {
                self.history_acked = self.history_acked.saturating_add(pending.rows.len());
            } else {
                self.history_pending = Some(pending);
            }
        }
    }

    /// Re-offer acked history under a new id (`app.display.reset` / Ctrl+L).
    pub fn request_history_replay(&mut self) {
        self.history_replay = true;
        self.history_acked = 0;
        self.history_pending = None;
    }

    /// Time-to-first-frame (from construction to first render).
    pub fn time_to_first_frame(&self) -> Duration {
        self.first_frame.frame_elapsed()
    }

    /// Dispatch a canonical key id (`ctrl+q`, `alt+m`, …) against the
    /// OMP action table.  Overlay keys are handled by the caller first.
    pub fn handle_canonical(&mut self, canonical: &str, input: &mut String) -> Dispatch {
        self.handle_canonical_at(canonical, input, Instant::now())
    }

    /// Dispatch with an injectable clock (tests drive space-hold cadence).
    pub fn handle_canonical_at(
        &mut self,
        canonical: &str,
        input: &mut String,
        now: Instant,
    ) -> Dispatch {
        if self
            .keys
            .matches_canonical(canonical, "app.details.toggleAll")
        {
            let _ = self.toggle_all_details();
            return Dispatch::Handled(None);
        }

        let interrupt = self.keys.matches_canonical(canonical, "app.interrupt");
        if !interrupt {
            // Any other key means the user is still working, so an armed exit
            // must not survive it.
            self.exit_armed = None;
        }
        if interrupt {
            // The action is documented as "Interrupt / exit", and that is what
            // it has to be: while a turn runs there was no way at all to stop
            // it from the terminal, only to quit the app.
            if self.turn_active {
                return Dispatch::Cancel;
            }
            // Leaving on one stray Ctrl+C throws the session away; ask twice.
            let confirmed = self
                .exit_armed
                .is_some_and(|armed| now.saturating_duration_since(armed) <= EXIT_CONFIRM_WINDOW);
            if confirmed {
                return Dispatch::Exit;
            }
            self.exit_armed = Some(now);
            self.set_alert(EXIT_HINT);
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "app.message.followUp")
        {
            if !input.is_empty() {
                let text = std::mem::take(input);
                self.completion_hide();
                self.push_queued(text, QueueMode::FollowUp);
            }
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "app.message.dequeue")
        {
            if let Some(text) = self.pull_last_queued() {
                *input = text;
            }
            return Dispatch::Handled(None);
        }

        if self.keys.matches_canonical(canonical, "app.session.switch") {
            self.open_session_switcher();
            return Dispatch::Handled(None);
        }

        if self.keys.matches_canonical(canonical, "app.model.select")
            || self
                .keys
                .matches_canonical(canonical, "app.model.selectTemporary")
        {
            self.open_model_picker();
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "app.model.cycleForward")
        {
            self.cycle_model(true);
            return Dispatch::Handled(None);
        }
        if self
            .keys
            .matches_canonical(canonical, "app.model.cycleBackward")
        {
            self.cycle_model(false);
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "app.thinking.toggle")
            || self.keys.matches_canonical(canonical, "app.thinking.cycle")
        {
            self.details("thinking cycle");
            return Dispatch::Handled(None);
        }
        if self.keys.matches_canonical(canonical, "app.tools.expand") {
            self.details("tools cycle");
            return Dispatch::Handled(None);
        }
        if self
            .keys
            .matches_canonical(canonical, "app.tools.toggleVisibility")
        {
            if self.transcript.mode(Section::Tools) == SectionMode::Hidden {
                self.details("tools expanded");
            } else {
                self.details("tools hidden");
            }
            return Dispatch::Handled(None);
        }

        if self.keys.matches_canonical(canonical, "app.display.reset") {
            self.request_history_replay();
            return Dispatch::Handled(Some(SubmitEffect::DisplayReset));
        }

        if self.keys.matches_canonical(canonical, "app.plan.toggle") {
            // The badge is not flipped here: it follows the engine's
            // ModeChanged, so it can never claim a mode the turns are not
            // actually running in.
            let mode = if self.mode == SessionMode::Plan {
                SessionMode::Agent
            } else {
                SessionMode::Plan
            };
            return Dispatch::Handled(Some(SubmitEffect::SetMode(mode)));
        }

        if self.keys.matches_canonical(canonical, "app.live.toggle") {
            self.live_mode = !self.live_mode;
            let label = if self.live_mode { "live" } else { "live off" };
            self.set_alert(label.to_owned());
            return Dispatch::Handled(None);
        }

        if self.keys.matches_canonical(canonical, "app.stt.toggle") {
            self.toggle_stt();
            return Dispatch::Handled(None);
        }

        if self.keys.matches_canonical(canonical, "app.agents.hub")
            || self
                .keys
                .matches_canonical(canonical, "app.session.observe")
        {
            self.open_agents_hub();
            return Dispatch::Handled(None);
        }

        if self.keys.matches_canonical(canonical, "app.history.search") {
            self.open_history_search();
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "app.editor.external")
        {
            return Dispatch::Handled(Some(SubmitEffect::ExternalEditor));
        }

        if self.keys.matches_canonical(canonical, "app.retry") {
            if let Some(text) = self.last_prompt.clone() {
                *input = text;
                return Dispatch::Handled(self.submit_line(input));
            }
            self.set_alert("retry: no last prompt");
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "app.clipboard.copyLine")
        {
            return Dispatch::Handled(Some(SubmitEffect::Copy(input.clone())));
        }
        if self
            .keys
            .matches_canonical(canonical, "app.clipboard.copyPrompt")
        {
            let text = if input.is_empty() {
                self.last_prompt.clone().unwrap_or_default()
            } else {
                input.clone()
            };
            return Dispatch::Handled(Some(SubmitEffect::Copy(text)));
        }
        if self
            .keys
            .matches_canonical(canonical, "app.clipboard.pasteTextRaw")
        {
            if let Some(text) = read_system_clipboard() {
                input.push_str(&text);
                self.slash_completions(input);
            }
            return Dispatch::Handled(None);
        }
        if self
            .keys
            .matches_canonical(canonical, "app.clipboard.pasteImage")
        {
            if let Some(text) = read_system_clipboard() {
                let appended = self.paste(&text);
                input.push_str(&appended);
                self.slash_completions(input);
            }
            return Dispatch::Handled(None);
        }

        if canonical == "escape" {
            if self.completion_visible() {
                self.completion_hide();
                return Dispatch::Handled(None);
            }
            if self.queue_highlighted() {
                self.clear_highlight();
                return Dispatch::Handled(None);
            }
            return Dispatch::Handled(None);
        }

        if canonical == "tab" || (canonical == "enter" && self.completion_visible()) {
            if let Some(name) = self.completion_accept() {
                *input = name;
                return Dispatch::Handled(None);
            }
            if canonical == "tab" {
                return Dispatch::Handled(None);
            }
        }

        if canonical == "enter" {
            return Dispatch::Handled(self.submit_line(input));
        }

        if self
            .keys
            .matches_canonical(canonical, "tui.editor.cursorUp")
            || canonical == "up"
        {
            if self.completion_visible() {
                self.completion_move(true);
            }
            return Dispatch::Handled(None);
        }
        if self
            .keys
            .matches_canonical(canonical, "tui.editor.cursorDown")
            || canonical == "down"
        {
            if self.completion_visible() {
                self.completion_move(false);
            }
            return Dispatch::Handled(None);
        }

        if self
            .keys
            .matches_canonical(canonical, "tui.editor.deleteCharBackward")
            || canonical == "backspace"
        {
            let _ = input.pop();
            self.slash_completions(input);
            return Dispatch::Handled(None);
        }

        match self
            .space_hold
            .handle(canonical, now, self.stt_enabled, self.completion_visible())
        {
            SpaceHoldOutcome::Continue => {}
            SpaceHoldOutcome::InsertSpace => {
                input.push(' ');
                self.slash_completions(input);
                return Dispatch::Handled(None);
            }
            SpaceHoldOutcome::Swallow => return Dispatch::Handled(None),
            SpaceHoldOutcome::Start { retract } => {
                delete_before_cursor(input, retract);
                self.slash_completions(input);
                self.toggle_stt();
                return Dispatch::Handled(None);
            }
            SpaceHoldOutcome::EndThenContinue => {
                self.toggle_stt();
            }
        }

        if let Some(ch) = printable_char(canonical) {
            input.push(ch);
            self.slash_completions(input);
            return Dispatch::Handled(None);
        }

        Dispatch::Unhandled
    }

    /// Poll the 250ms space-hold release. Returns true when recording ended.
    pub fn poll_space_hold(&mut self, now: Instant) -> bool {
        if self.space_hold.poll(now) {
            self.toggle_stt();
            true
        } else {
            false
        }
    }

    fn toggle_stt(&mut self) {
        if self.live_mode {
            self.set_alert("End live mode before using push-to-talk speech input.");
            return;
        }
        if !self.stt_enabled {
            self.set_alert("Speech-to-text is disabled. Enable it in settings: stt.enabled");
            return;
        }
        self.stt_state = match self.stt_state {
            SttState::Idle => {
                self.set_alert("stt: recording");
                SttState::Recording
            }
            SttState::Recording => {
                self.set_alert("stt: idle");
                SttState::Idle
            }
            SttState::Transcribing => {
                self.set_alert("Transcription in progress...");
                SttState::Transcribing
            }
        };
    }

    fn submit_line(&mut self, input: &mut String) -> Option<SubmitEffect> {
        let line = std::mem::take(input);
        self.completion_hide();
        if line.is_empty() {
            return None;
        }
        match self.slash.route(&line) {
            Route::Builtin(name) => self.dispatch_builtin(&name, &line),
            Route::Expanded(text) => self.deliver_prompt(text),
            Route::Passthrough => self.deliver_prompt(line),
        }
    }

    fn dispatch_builtin(&mut self, name: &str, line: &str) -> Option<SubmitEffect> {
        let args = line
            .split_once(' ')
            .map(|(_, rest)| rest.trim())
            .unwrap_or("");
        match name {
            "help" => {
                self.open_help();
                None
            }
            "model" => {
                self.open_model_picker();
                None
            }
            "sessions" | "switch" => {
                self.open_session_switcher();
                None
            }
            "agents" => {
                self.open_agents_hub();
                None
            }
            "hotkeys" => {
                self.open_hotkeys();
                None
            }
            "pause" => {
                self.overlay = Some(ActiveOverlay::Pause { closed: false });
                // The overlay only holds input. Stopping the agent is the
                // other half, and without it the label was a promise the UI
                // did not keep: the turn kept streaming behind the modal.
                Some(SubmitEffect::Pause)
            }
            "details" => {
                let _ = self.details(args);
                None
            }
            "recap" => {
                if let Err(reason) = self.open_session_recap() {
                    self.set_alert(format!("recap: {reason}"));
                }
                None
            }
            "goal" => {
                let text = args.trim();
                if text.is_empty() {
                    self.set_alert("usage: /goal <text>");
                } else {
                    self.set_alert(format!("goal: {text}"));
                }
                None
            }
            "checkpoint" => {
                match self.session_id.as_deref() {
                    Some(id) => match checkpoint_session(
                        &titi_config::agent_dir(),
                        &current_workspace(),
                        id,
                    ) {
                        Ok(summary) => self.set_alert(summary),
                        Err(reason) => self.set_alert(format!("checkpoint: {reason}")),
                    },
                    None => self.set_alert("checkpoint: no live session"),
                }
                None
            }
            "checkpoints" => {
                match self.session_id.as_deref() {
                    Some(id) => match list_checkpoints(&titi_config::agent_dir(), id) {
                        Ok(summary) => self.set_alert(summary),
                        Err(reason) => self.set_alert(format!("checkpoints: {reason}")),
                    },
                    None => self.set_alert("checkpoints: no live session"),
                }
                None
            }
            "rewind" => {
                let index = match args {
                    "" => Ok(None),
                    other => other
                        .parse::<usize>()
                        .map(Some)
                        .map_err(|_| format!("usage: /rewind [n] (got {other})")),
                };
                match (self.session_id.as_deref(), index) {
                    (None, _) => self.set_alert("rewind: no live session"),
                    (Some(_), Err(reason)) => self.set_alert(format!("rewind: {reason}")),
                    (Some(id), Ok(index)) => {
                        match rewind_session(
                            &titi_config::agent_dir(),
                            &current_workspace(),
                            id,
                            index,
                        ) {
                            Ok(summary) => {
                                self.set_alert(summary);
                                // The engine still holds the pre-rewind history;
                                // the surface must replace it, or the model
                                // keeps reading what the user just cut away.
                                return Some(SubmitEffect::Rewind);
                            }
                            Err(reason) => self.set_alert(format!("rewind: {reason}")),
                        }
                    }
                }
                None
            }
            "hub" => {
                self.toggle_agents_hub();
                None
            }
            "join" => {
                self.join_hub(args);
                None
            }
            "leave" => {
                self.leave_hub();
                None
            }
            "mouse" => {
                if args == "toggle" || args.is_empty() {
                    Some(SubmitEffect::MouseToggle)
                } else if let Some(preset) = MousePreset::parse(args) {
                    Some(SubmitEffect::Mouse(preset))
                } else {
                    self.set_alert("usage: /mouse off|on|wheel|buttons|all|toggle");
                    None
                }
            }
            _ => None,
        }
    }

    fn deliver_prompt(&mut self, text: String) -> Option<SubmitEffect> {
        self.prompt_history.push(text.clone());
        self.last_prompt = Some(text.clone());
        // Whatever the outcome, the user's message is part of the transcript.
        self.session_writes
            .push((titi_core::session::Role::User, text.clone()));
        // A turn in flight is steered, not restarted: the message is injected at
        // its next step boundary.
        if self.turn_active {
            return Some(SubmitEffect::Steer(text));
        }
        match self.submit(text.clone()) {
            SubmitOutcome::Queued => Some(SubmitEffect::Queued(text)),
            SubmitOutcome::Delivered => Some(SubmitEffect::Delivered(text)),
        }
    }
}

/// The config key that stores the mouse-tracking preset.
pub const MOUSE_TRACKING_KEY: &str = "display.mouse_tracking";

/// How long the first exit request stays armed.
pub const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

/// What the first Ctrl+C says.
pub const EXIT_HINT: &str = "press Ctrl+C again to exit";

/// Load the persisted mouse preset from the titi config.
///
/// `agent_dir` is the settings root (see [`titi_config::agent_dir`]).
/// Returns `None` when the key is absent or unparsable (caller falls back to
/// its own default).
pub fn load_mouse_preset_from(agent_dir: &std::path::Path) -> Option<MousePreset> {
    use titi_config::settings::Settings;
    let settings = Settings::load(
        agent_dir,
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        &[],
    )
    .ok()?;
    let value = settings.get(MOUSE_TRACKING_KEY)?;
    let name = match value {
        serde_json::Value::String(s) => s,
        _ => return None,
    };
    MousePreset::parse(&name)
}

/// Load the persisted mouse preset using the real agent directory.
pub fn load_mouse_preset() -> Option<MousePreset> {
    load_mouse_preset_from(&titi_config::agent_dir())
}

/// Persist the mouse preset to the titi config (`display.mouse_tracking`).
pub fn save_mouse_preset_to(
    agent_dir: &std::path::Path,
    preset: MousePreset,
) -> Result<(), String> {
    use titi_config::settings::Settings;
    let mut settings = Settings::load(
        agent_dir,
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        &[],
    )
    .map_err(|e| format!("{e}"))?;
    settings
        .set(MOUSE_TRACKING_KEY, serde_json::json!(preset.name()))
        .map_err(|e| format!("{e}"))
}

/// Persist the mouse preset using the real agent directory.
pub fn save_mouse_preset(preset: MousePreset) -> Result<(), String> {
    save_mouse_preset_to(&titi_config::agent_dir(), preset)
}

/// Load the process-wide default theme via auto appearance (COLORFGBG first).
pub fn default_theme() -> Result<Arc<Theme>, String> {
    let inputs = AppearanceInputs::from_env();
    let name = global().init_auto(&inputs);
    match global().current() {
        Some(theme) => Ok(theme),
        None => Theme::new(
            name,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            ColorMode::Truecolor,
            SymbolPreset::Unicode,
            std::collections::HashMap::new(),
            None,
            None,
        )
        .map(Arc::new),
    }
}

/// The modal overlay panel currently shown, kept typed so the application
/// can extract its result when it closes (contract:
/// `docs/research/agent-ux/README.md` — panels implement Overlay; Esc is
/// always cancel-without-delete).
pub enum ActiveOverlay {
    ModelPicker(SelectionPanel<String>),
    Recap(Recap),
    SessionSwitcher(SessionSwitcher),
    Approval(ApprovalPanel),
    Help(SelectionPanel<String>),
    Hotkeys(SelectionPanel<String>),
    HistorySearch(SelectionPanel<String>),
    Hub(HubRoster),
    Pause { closed: bool },
}

impl ActiveOverlay {
    fn set_max_visible(&mut self, rows: usize) {
        match self {
            ActiveOverlay::ModelPicker(p)
            | ActiveOverlay::Help(p)
            | ActiveOverlay::Hotkeys(p)
            | ActiveOverlay::HistorySearch(p) => p.set_max_visible(rows),
            ActiveOverlay::Hub(h) => h.set_max_visible(rows),
            ActiveOverlay::Recap(r) => r.set_max_visible(rows),
            ActiveOverlay::Approval(p) => p.set_max_visible(rows),
            ActiveOverlay::SessionSwitcher(s) => s.set_max_visible(rows),
            ActiveOverlay::Pause { .. } => {}
        }
    }

    fn render(&mut self, width: u16) -> Vec<String> {
        match self {
            ActiveOverlay::ModelPicker(p) => p.render(width),
            ActiveOverlay::Recap(r) => r.render(width),
            ActiveOverlay::SessionSwitcher(s) => s.render(width),
            ActiveOverlay::Approval(a) => a.render(width),
            ActiveOverlay::Help(p)
            | ActiveOverlay::Hotkeys(p)
            | ActiveOverlay::HistorySearch(p) => p.render(width),
            ActiveOverlay::Hub(h) => h.render(width),
            ActiveOverlay::Pause { closed } => {
                if *closed {
                    Vec::new()
                } else {
                    pause_rows(width)
                }
            }
        }
    }

    fn handle_input(&mut self, data: &str) {
        match self {
            ActiveOverlay::ModelPicker(p) => p.handle_input(data),
            ActiveOverlay::Recap(r) => r.handle_input(data),
            ActiveOverlay::SessionSwitcher(s) => s.handle_input(data),
            ActiveOverlay::Approval(a) => a.handle_input(data),
            ActiveOverlay::Help(p)
            | ActiveOverlay::Hotkeys(p)
            | ActiveOverlay::HistorySearch(p) => p.handle_input(data),
            ActiveOverlay::Hub(h) => h.handle_input(data),
            ActiveOverlay::Pause { closed } => {
                if matches!(data, "\x1b" | "\r" | " " | "\x03") {
                    *closed = true;
                }
            }
        }
    }

    fn is_closed(&self) -> bool {
        match self {
            ActiveOverlay::ModelPicker(p) => p.is_closed(),
            ActiveOverlay::Recap(r) => r.is_closed(),
            ActiveOverlay::SessionSwitcher(s) => s.is_closed(),
            ActiveOverlay::Approval(a) => a.is_closed(),
            ActiveOverlay::Help(p)
            | ActiveOverlay::Hotkeys(p)
            | ActiveOverlay::HistorySearch(p) => p.is_closed(),
            ActiveOverlay::Hub(h) => h.is_closed(),
            ActiveOverlay::Pause { closed } => *closed,
        }
    }

    /// Extract the outcome of a closed panel.
    fn outcome(self) -> Option<OverlayOutcome> {
        match self {
            ActiveOverlay::ModelPicker(p) => p
                .into_result()
                .and_then(|r| r.selected)
                .map(OverlayOutcome::ModelSelected),
            ActiveOverlay::SessionSwitcher(s) => {
                let titles = s.titles().to_vec();
                match s.into_action() {
                    Some(SessionAction::Switch(i)) => {
                        titles.get(i).cloned().map(OverlayOutcome::SessionSwitched)
                    }
                    Some(SessionAction::New) => Some(OverlayOutcome::SessionNew),
                    Some(SessionAction::Cancel) => Some(OverlayOutcome::SessionCancelled),
                    Some(SessionAction::Refresh) | Some(SessionAction::Close(_)) => None,
                    None => None,
                }
            }
            ActiveOverlay::Approval(a) => a
                .into_result()
                .map(|r| OverlayOutcome::Approval(!r.cancelled && r.selected == Some("Yes"))),
            ActiveOverlay::HistorySearch(p) => p
                .into_result()
                .and_then(|r| r.selected)
                .map(OverlayOutcome::HistoryPicked),
            ActiveOverlay::Hub(h) => {
                if h.cancelled() {
                    Some(OverlayOutcome::Dismissed)
                } else {
                    h.into_selected().map(OverlayOutcome::HubSelected)
                }
            }
            ActiveOverlay::Help(_)
            | ActiveOverlay::Hotkeys(_)
            | ActiveOverlay::Recap(_)
            | ActiveOverlay::Pause { .. } => Some(OverlayOutcome::Dismissed),
        }
    }
}

/// What a closed overlay panel decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    /// Model picker: the chosen model id.
    ModelSelected(String),
    /// Session switcher, Enter: the session to switch to.
    SessionSwitched(String),
    /// Session switcher, Ctrl+N: create a new session.
    SessionNew,
    /// Session switcher, Esc: cancelled — nothing deleted.
    SessionCancelled,
    /// Approval panel: `true` only for an explicit Yes; Esc/No/Cancel are
    /// all cancel-without-delete.
    Approval(bool),
    /// Exec-tier tool gate: Yes runs the call, Esc/No/Cancel deny it.
    ToolApproval { call_id: String, approved: bool },
    /// Help / hotkeys / pause / hub Esc — closed without a side effect.
    Dismissed,
    /// Agent Hub Enter: focus the selected peer (no live session switch yet).
    HubSelected(String),
    /// Agent Hub `r`: revive a parked peer.
    HubRevive(String),
    /// Agent Hub `x`: stop a running peer.
    HubStop(String),
    /// History search: insert the chosen prompt into the composer.
    HistoryPicked(String),
}

/// Models offered by the picker — the fallback chains from
/// `docs/research/STATE.md`.
pub fn model_choices() -> Vec<String> {
    [
        "opencode-go/glm-5.3-flash",
        "clinepass/glm-5.3",
        "opencode-go/deepseek-v4-flash",
        "clinepass/deepseek-v4-flash",
        "bai/glm-5.3-flash",
        "bai/qwen3.8-flash",
        "clinepass/deepseek-v4-pro",
        "qwen3.8-max",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// List session ids stored under `<agent_dir>/sessions` (newest first).
pub fn list_sessions_from(agent_dir: &std::path::Path) -> Vec<String> {
    let dir = agent_dir.join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids: Vec<(std::time::SystemTime, String)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((
                modified,
                e.path().file_stem()?.to_string_lossy().into_owned(),
            ))
        })
        .collect();
    ids.sort_by_key(|a| std::cmp::Reverse(a.0));
    ids.into_iter().map(|(_, id)| id).collect()
}

/// [`list_sessions_from`] against the real agent directory.
pub fn list_sessions() -> Vec<String> {
    list_sessions_from(&titi_config::agent_dir())
}

/// Delete a session's JSONL file.  Callers must gate this behind an
/// approval prompt — Esc never reaches here.
pub fn delete_session_from(agent_dir: &std::path::Path, id: &str) -> Result<(), String> {
    let path = agent_dir.join("sessions").join(format!("{id}.jsonl"));
    std::fs::remove_file(&path).map_err(|e| format!("{e}"))
}

/// [`delete_session_from`] against the real agent directory.
pub fn delete_session(id: &str) -> Result<(), String> {
    delete_session_from(&titi_config::agent_dir(), id)
}

/// Creates an empty session and returns its id.
pub fn new_session(agent_dir: &std::path::Path) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    store
        .create(titi_core::session::SessionMeta {
            title: Some("titi".into()),
            source: Some("cli".into()),
            ..Default::default()
        })
        .map_err(|e| e.to_string())
}

pub fn fork_session(agent_dir: &std::path::Path, session_id: &str) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    store
        .fork_session(session_id, titi_core::session::SessionMeta::default())
        .map_err(|e| e.to_string())
}

pub fn export_session(
    agent_dir: &std::path::Path,
    session_id: &str,
    path: &str,
) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;

    // Default to markdown if not specified in path
    let format = if path.ends_with(".jsonl") {
        titi_core::session::export::ExportFormat::Jsonl
    } else {
        titi_core::session::export::ExportFormat::Markdown
    };

    let path_val = std::path::PathBuf::from(if path.is_empty() {
        format!("{session_id}.md")
    } else {
        path.to_owned()
    });

    store
        .export_to_file(session_id, format, &path_val)
        .map_err(|e| e.to_string())?;

    Ok(format!("exported to {}", path_val.display()))
}

/// The conversation a resumed session replays: the path to its current leaf,
/// capped at a boundary that keeps every tool round whole, so an old
/// transcript cannot crowd out the workspace map or replay an orphan call.
pub fn session_history(
    agent_dir: &std::path::Path,
    session_id: &str,
) -> Result<Vec<titi_providers::ChatMessage>, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let entries = store.walk(session_id, None).map_err(|e| e.to_string())?;
    Ok(crate::engine::restore_window(
        titi_core::session::entries_to_messages(&entries),
        crate::engine::MAX_RESTORED_MESSAGES,
    ))
}

/// The directory a checkpoint pins and a rewind restores: where titi runs.
pub fn current_workspace() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| ".".into())
}

/// Record a rewind point on a session; returns a human summary.
///
/// `workspace` is explicit: taking the process cwd here made the tests
/// commit into whatever checkout ran them.
pub fn checkpoint_session(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
    session_id: &str,
) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let mut checkpoint = store.checkpoint(session_id).map_err(|e| e.to_string())?;
    // Also pin the workspace, so a later rewind can undo code and not only
    // the transcript. A directory that is not a repo stays session-only.
    let git = crate::git_checkpoint::snapshot(
        workspace,
        &format!("{session_id} · {} entries", checkpoint.entries),
    );
    if let Ok(commit) = &git {
        checkpoint.git_commit = Some(commit.clone());
        let _ = store.record_git_commit(session_id, commit);
    }
    let suffix = match &git {
        Ok(commit) => format!(" · git {}", &commit[..7.min(commit.len())]),
        Err(_) => String::new(),
    };
    Ok(format!(
        "checkpoint: {} entries{suffix}",
        checkpoint.entries
    ))
}

/// List a session's rewind points, oldest first.
pub fn list_checkpoints(agent_dir: &std::path::Path, session_id: &str) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let all = store.checkpoints(session_id).map_err(|e| e.to_string())?;
    if all.is_empty() {
        return Ok("checkpoints: none".into());
    }
    let rows: Vec<String> = all
        .iter()
        .enumerate()
        .map(|(i, cp)| format!("#{} · {} entries", i + 1, cp.entries))
        .collect();
    Ok(format!("checkpoints: {}", rows.join(" | ")))
}

/// Rewind a session to checkpoint `index` (1-based); the newest when `None`.
pub fn rewind_session(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
    session_id: &str,
    index: Option<usize>,
) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let all = store.checkpoints(session_id).map_err(|e| e.to_string())?;
    if all.is_empty() {
        return Err("no checkpoints recorded".into());
    }
    let position = match index {
        None => all.len() - 1,
        Some(0) => return Err("checkpoints are numbered from 1".into()),
        Some(n) if n <= all.len() => n - 1,
        Some(n) => return Err(format!("no checkpoint #{n} (have {})", all.len())),
    };
    let target = all[position].clone();
    store
        .rewind(session_id, &target)
        .map_err(|e| e.to_string())?;
    // Put the files back too, when the checkpoint pinned a commit and the
    // tree is clean. A dirty tree is reported rather than overwritten.
    let git = match &target.git_commit {
        Some(commit) => match crate::git_checkpoint::restore(workspace, commit) {
            Ok(()) => format!(" · git {}", &commit[..7.min(commit.len())]),
            Err(reason) => format!(" · git not restored: {reason}"),
        },
        None => String::new(),
    };
    Ok(format!(
        "rewound to checkpoint #{} ({} entries){git}",
        position + 1,
        target.entries
    ))
}

/// Result of dispatching a canonical key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// Action consumed; caller should redraw. Optional slash/prompt side effect.
    Handled(Option<SubmitEffect>),
    /// Ctrl+C / app.interrupt — leave the TUI.
    Exit,
    /// Ctrl+C while a turn runs: stop the turn, stay in the TUI.
    Cancel,
    /// Key not bound and not printable.
    Unhandled,
}

/// Side effect of submitting a composer line (slash or prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitEffect {
    None,
    Mouse(MousePreset),
    /// Cycle off → wheel → buttons → all → off.
    MouseToggle,
    Queued(String),
    Delivered(String),
    /// A turn is running: redirect it instead of starting a new one.
    Steer(String),
    /// `/pause`: stop the running turn and hold input behind the modal.
    Pause,
    /// The session was rewound: replace the engine's replayed history.
    Rewind,
    /// OSC 52 copy of the given text.
    Copy(String),
    /// ED3 + re-offer history (`app.display.reset`).
    DisplayReset,
    /// Open `$VISUAL` / `$EDITOR` on the draft.
    ExternalEditor,
    /// `app.plan.toggle`: ask the engine for a mode; the badge follows its
    /// answer.
    SetMode(SessionMode),
}

fn load_keybindings_manager() -> KeybindingsManager {
    let path = titi_config::agent_dir().join("keybindings.yml");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut user = titi_tui::keybindings::parse_keybindings_config(&raw);
    let _ = titi_tui::keybindings::migrate_keybinding_names(&mut user);
    default_manager(user)
}

fn printable_char(canonical: &str) -> Option<char> {
    match canonical {
        "space" => Some(' '),
        s if s.len() == 1 => s.chars().next(),
        s if s.starts_with("shift+") && s.len() == 7 => {
            s.chars().last().map(|c| c.to_ascii_uppercase())
        }
        _ => None,
    }
}

/// OMP compact picker: ~40% of the terminal, minus chrome, sitting above the composer.
fn compact_item_budget(term_rows: usize, margin_bottom: usize) -> usize {
    const HEIGHT_FRACTION: f64 = 0.4;
    const CHROME_ROWS: usize = 2;
    const MIN_VISIBLE: usize = 3;
    let term_rows = term_rows.max(16);
    let from_fraction = ((term_rows as f64) * HEIGHT_FRACTION).floor() as usize;
    let from_fraction = from_fraction.saturating_sub(CHROME_ROWS);
    let from_space = term_rows
        .saturating_sub(margin_bottom)
        .saturating_sub(CHROME_ROWS);
    from_fraction.max(MIN_VISIBLE).min(from_space.max(1))
}

fn pause_rows(width: u16) -> Vec<String> {
    let labels = vec![
        "agent stopped, input held".to_owned(),
        "press Esc / Enter / Space / Ctrl+C to resume".to_owned(),
    ];
    let mut panel = SelectionPanel::new("paused", vec!["resume".to_owned()], labels);
    panel.render(width)
}

struct FrameLayers {
    banner: Vec<String>,
    transcript: Vec<String>,
    composer: Vec<String>,
}

fn read_system_clipboard() -> Option<String> {
    let candidates: &[(&str, &[&str])] = &[
        ("pbpaste", &[]),
        ("wl-paste", &["-n"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
    ];
    for (bin, args) in candidates {
        if let Ok(out) = std::process::Command::new(bin).args(*args).output()
            && out.status.success()
        {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    None
}
