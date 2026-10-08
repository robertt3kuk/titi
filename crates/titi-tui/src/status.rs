//! Status line — agent state machine, turn timers, badges, and busy indicator.
//!
//! Contract: `docs/research/agent-ux/README.md`.

use crate::width::truncate_to_width;
use crate::width::visible_width;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Agent state machine
// ---------------------------------------------------------------------------

/// Agent lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Starting,
    Ready,
    Thinking,
    Running,
    Interrupted,
}

/// Events that drive the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusEvent {
    ProviderReady,
    PromptSent,
    FirstToken,
    TurnEnd,
    Interrupt,
    Resume,
}

impl AgentState {
    /// Transition to the next state given an event.
    ///
    /// Returns the new state, or `None` if the event is invalid for the
    /// current state.
    pub fn transition(self, event: StatusEvent) -> Option<AgentState> {
        use AgentState::*;
        use StatusEvent::*;
        match (self, event) {
            (Starting, ProviderReady) => Some(Ready),
            (Ready, PromptSent) => Some(Thinking),
            (Thinking, FirstToken) => Some(Running),
            (Running, TurnEnd) => Some(Ready),
            (Running, Interrupt) => Some(Interrupted),
            (Interrupted, Resume) => Some(Ready),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Timer formatting
// ---------------------------------------------------------------------------

/// Format a duration as a compact timer string.
///
/// - < 1 minute → `"12s"` (seconds)
/// - < 1 hour → `"3m 45s"` (minutes + seconds)
/// - ≥ 1 hour → `"1h 23m"` (hours + minutes)
/// - ≥ 24 hours → `"24h+"` (hours +)
pub fn format_timer(dur: Duration) -> String {
    let total_secs = dur.as_secs();
    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{}m {:02}s", mins, secs)
    } else if total_secs < 86400 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        format!("{}h {:02}m", hours, mins)
    } else {
        let hours = total_secs / 3600;
        format!("{}h+", hours)
    }
}

// ---------------------------------------------------------------------------
// Turn footer
// ---------------------------------------------------------------------------

/// One finished turn's accounting, as the dim row under the answer shows it.
///
/// omp prints the same row under the answer of every turn
/// (`display.showTokenUsage`, `display.showTurnTime`,
/// `pi-tui/src/overlays/usage-row.ts:117`): the turn's wall time, the prompt it
/// paid for, the share of it the provider read from its cache, and what it
/// answered. The row is only built for a turn that reported usage — a turn
/// cancelled before its first round has nothing to show, and a row of zeros
/// would be a fact dressed up as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnFooter {
    /// The turn's wall time, from the prompt to the turn's last event.
    pub elapsed: Duration,
    /// Prompt tokens the turn's requests carried.
    pub prompt_tokens: u32,
    /// The part of `prompt_tokens` the provider served from its cache.
    pub cached_tokens: u32,
    /// Tokens the turn's answer cost.
    pub completion_tokens: u32,
    /// The turn's request carried history and the provider served none of it
    /// from cache, so the prefix was paid for again.
    pub cache_miss: bool,
}

impl TurnFooter {
    /// The row: `1.4s · 3.4k prompt (2.9k cached) · 250 out`, with a trailing
    /// `· cache miss` when the request re-paid for its own history.
    ///
    /// The cached share is named only when there is one: `(0 cached)` would be
    /// a zero dressed as data, and the miss marker already says the honest
    /// thing about a cold request.
    pub fn row(&self) -> String {
        let prompt = if self.cached_tokens > 0 {
            format!(
                "{} prompt ({} cached)",
                compact_tokens(self.prompt_tokens),
                compact_tokens(self.cached_tokens)
            )
        } else {
            format!("{} prompt", compact_tokens(self.prompt_tokens))
        };
        let mut parts = vec![
            format_turn_time(self.elapsed),
            prompt,
            format!("{} out", compact_tokens(self.completion_tokens)),
        ];
        if self.cache_miss {
            parts.push("cache miss".to_owned());
        }
        parts.join(" · ")
    }
}

/// A turn's wall time: one decimal deep below a minute (`1.4s`), the status
/// line's own timer from there (`3m 05s`, `1h 23m`).
///
/// The tenths are worth their cells here, unlike on the live status row: a turn
/// is short, and a footer that reads `1s` for a turn that took 1.4 of them says
/// less than the row it replaces.
fn format_turn_time(elapsed: Duration) -> String {
    if elapsed.as_secs() < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format_timer(elapsed)
    }
}

/// A token count as the screen prints it: exact below a thousand, one decimal
/// up to ten thousand, whole thousands and millions from there — `250`, `3.4k`,
/// `128k`, `1.2M`.
///
/// omp's `formatNumber` (`pi-utils/src/format.ts:34`), in the lowercase `k`/`M`
/// this crate's context labels already use.
pub fn compact_tokens(tokens: u32) -> String {
    let n = u64::from(tokens);
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        one_decimal(n as f64 / 1_000.0, "k")
    } else if n < 1_000_000 {
        format!("{}k", (n + 500) / 1_000)
    } else if n < 10_000_000 {
        one_decimal(n as f64 / 1_000_000.0, "M")
    } else {
        format!("{}M", (n + 500_000) / 1_000_000)
    }
}

/// `value` to one decimal, a trailing `.0` dropped, with `unit` appended.
fn one_decimal(value: f64, unit: &str) -> String {
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text);
    format!("{text}{unit}")
}

// ---------------------------------------------------------------------------
// Busy indicator
// ---------------------------------------------------------------------------

/// Available busy-indicator presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPreset {
    /// ASCII spinner: `|/-\` (width 1).
    Ascii,
    /// Braille dots: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` (width 1 each).
    Braille,
    /// Kaomoji — each frame padded to the same display width.
    Kaomoji,
}

/// A fixed-width busy indicator that cycles through animation frames.
///
/// All frames have the same visual width, so the status line does not
/// jitter when the indicator advances.
#[derive(Debug, Clone)]
pub struct BusyIndicator {
    frames: &'static [&'static str],
    index: usize,
    frame_width: usize,
}

impl BusyIndicator {
    /// Create a new indicator with the given preset.
    pub fn new(preset: BusyPreset) -> Self {
        let (frames_str, frame_width) = match preset {
            BusyPreset::Ascii => (&["|", "/", "-", "\\"][..], 1),
            BusyPreset::Braille => {
                // OMP status spinner (`omp://theme.md`): ⣾⣽⣻⢿⡿⣟⣯⣷
                (&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"][..], 1)
            }
            BusyPreset::Kaomoji => {
                // Each frame padded to the max display width of the set.
                let raw: &[&str] = &["(◕‿◕)", "(◕‿◕)", "(◡‿◡)", "(◠‿◠)", "(◕‿◕)", "(◕‿◕)"];
                let max_w = raw.iter().map(|f| visible_width(f)).max().unwrap_or(1);
                // We store the raw frames and pad at render time.
                return BusyIndicator {
                    frames: raw,
                    index: 0,
                    frame_width: max_w,
                };
            }
        };
        BusyIndicator {
            frames: frames_str,
            index: 0,
            frame_width,
        }
    }

    /// Advance to the next frame and return the current frame string,
    /// padded to the fixed width.
    pub fn frame(&mut self) -> String {
        let f = self.frames[self.index];
        self.index = (self.index + 1) % self.frames.len();
        pad_to_width(f, self.frame_width)
    }

    /// The constant display width of every frame.
    pub fn width(&self) -> usize {
        self.frame_width
    }
}

/// Pad a string to a given display width with spaces.
fn pad_to_width(s: &str, target: usize) -> String {
    let w = visible_width(s);
    if w >= target {
        s.to_owned()
    } else {
        let mut out = s.to_owned();
        out.push_str(&" ".repeat(target - w));
        out
    }
}

// ---------------------------------------------------------------------------
// Status line model
// ---------------------------------------------------------------------------

/// Badges shown on the status line.
#[derive(Debug, Clone, Default)]
pub struct Badges {
    pub compressions: u32,
    pub background_tasks: u32,
    pub yolo: bool,
}

/// The status line model — state machine, timers, badges, and busy indicator.
#[derive(Debug, Clone)]
pub struct StatusLine {
    /// Current agent state.
    state: AgentState,
    /// When the current turn started (for timer display).
    turn_start: Option<Instant>,
    /// Badges.
    pub badges: Badges,
    /// Busy indicator (active during Running/Thinking states).
    busy: BusyIndicator,
}

impl StatusLine {
    /// Create a new status line in the `Starting` state.
    pub fn new() -> Self {
        StatusLine {
            state: AgentState::Starting,
            turn_start: None,
            badges: Badges::default(),
            busy: BusyIndicator::new(BusyPreset::Braille),
        }
    }
}

impl Default for StatusLine {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusLine {
    /// Apply a state transition event.
    ///
    /// Returns the new state, or `None` if the event was invalid for the
    /// current state (no-op).
    pub fn transition(&mut self, event: StatusEvent) -> Option<AgentState> {
        let new = self.state.transition(event)?;
        // Side effects on specific transitions.
        match event {
            StatusEvent::PromptSent => {
                self.turn_start = Some(Instant::now());
            }
            StatusEvent::TurnEnd | StatusEvent::Interrupt => {
                self.turn_start = None;
            }
            _ => {}
        }
        self.state = new;
        Some(new)
    }

    /// Current agent state.
    pub fn state(&self) -> AgentState {
        self.state
    }

    /// Set the busy indicator preset.
    pub fn set_busy_preset(&mut self, preset: BusyPreset) {
        self.busy = BusyIndicator::new(preset);
    }

    /// Busy indicator reference.
    pub fn busy(&self) -> &BusyIndicator {
        &self.busy
    }

    /// Busy indicator mutable reference.
    pub fn busy_mut(&mut self) -> &mut BusyIndicator {
        &mut self.busy
    }

    /// Render the status line to a single string at the given width.
    ///
    /// The format is:
    /// `state-label  ⏱/⏲ timer  ┃N  ⟳N  ⚠ YOLO  busy`
    ///
    /// The line is truncated to `width` columns if it exceeds the terminal
    /// width, then padded with trailing spaces to exactly `width` columns so
    /// the status bar always spans the terminal.
    pub fn render(&mut self, width: u16) -> String {
        let w = width as usize;

        // State label.
        let state_label = match self.state {
            AgentState::Starting => "starting",
            AgentState::Ready => "ready",
            AgentState::Thinking => "thinking",
            AgentState::Running => "running",
            AgentState::Interrupted => "interrupted",
        };

        // Timer.
        let timer_str = if let Some(start) = self.turn_start {
            let elapsed = start.elapsed();
            match self.state {
                AgentState::Running => format!("⏱ {}", format_timer(elapsed)),
                AgentState::Thinking => format!("⏱ {}", format_timer(elapsed)),
                _ => format!("⏲ {}", format_timer(elapsed)),
            }
        } else {
            String::new()
        };

        // Badges.
        let mut badge_parts = Vec::new();
        if self.badges.compressions > 0 {
            badge_parts.push(format!("┃{}", self.badges.compressions));
        }
        if self.badges.background_tasks > 0 {
            badge_parts.push(format!("⟳{}", self.badges.background_tasks));
        }
        if self.badges.yolo {
            badge_parts.push("⚠ YOLO".to_owned());
        }
        let badges_str = if badge_parts.is_empty() {
            String::new()
        } else {
            badge_parts.join(" ")
        };

        // Busy indicator — only during active states.
        let busy_str = match self.state {
            AgentState::Running | AgentState::Thinking => {
                format!(" {}", self.busy.frame())
            }
            _ => String::new(),
        };

        // Assemble.
        let line = if timer_str.is_empty() && badges_str.is_empty() {
            format!("{}{}", state_label, busy_str)
        } else if badges_str.is_empty() {
            format!("{}  {}{}", state_label, timer_str, busy_str)
        } else {
            format!("{}  {}  {}{}", state_label, timer_str, badges_str, busy_str)
        };

        let line = truncate_to_width(&line, w);
        pad_to_width(&line, w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- State machine ----------------------------------------------------

    #[test]
    fn state_transitions_starting_to_ready() {
        let s = AgentState::Starting;
        assert_eq!(
            s.transition(StatusEvent::ProviderReady),
            Some(AgentState::Ready)
        );
    }

    #[test]
    fn state_transitions_ready_to_thinking() {
        let s = AgentState::Ready;
        assert_eq!(
            s.transition(StatusEvent::PromptSent),
            Some(AgentState::Thinking)
        );
    }

    #[test]
    fn state_transitions_thinking_to_running() {
        let s = AgentState::Thinking;
        assert_eq!(
            s.transition(StatusEvent::FirstToken),
            Some(AgentState::Running)
        );
    }

    #[test]
    fn state_transitions_running_to_ready() {
        let s = AgentState::Running;
        assert_eq!(s.transition(StatusEvent::TurnEnd), Some(AgentState::Ready));
    }

    #[test]
    fn state_transitions_running_to_interrupted() {
        let s = AgentState::Running;
        assert_eq!(
            s.transition(StatusEvent::Interrupt),
            Some(AgentState::Interrupted)
        );
    }

    #[test]
    fn state_transitions_interrupted_to_ready() {
        let s = AgentState::Interrupted;
        assert_eq!(s.transition(StatusEvent::Resume), Some(AgentState::Ready));
    }

    #[test]
    fn invalid_transition_returns_none() {
        let s = AgentState::Starting;
        assert_eq!(s.transition(StatusEvent::TurnEnd), None);
        assert_eq!(s.transition(StatusEvent::Interrupt), None);
    }

    #[test]
    fn status_line_initial_state() {
        let sl = StatusLine::new();
        assert_eq!(sl.state(), AgentState::Starting);
    }

    #[test]
    fn status_line_transition_side_effects() {
        let mut sl = StatusLine::new();
        assert!(sl.transition(StatusEvent::ProviderReady).is_some());
        assert_eq!(sl.state(), AgentState::Ready);
        assert!(sl.turn_start.is_none());
    }

    // ---- Timer formatting -------------------------------------------------

    #[test]
    fn format_seconds() {
        assert_eq!(format_timer(Duration::from_secs(5)), "5s");
        assert_eq!(format_timer(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_minutes() {
        assert_eq!(format_timer(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_timer(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn format_hours() {
        assert_eq!(format_timer(Duration::from_secs(3600)), "1h 00m");
        assert_eq!(format_timer(Duration::from_secs(7380)), "2h 03m");
    }

    #[test]
    fn format_days() {
        assert_eq!(format_timer(Duration::from_secs(86400)), "24h+");
        assert_eq!(format_timer(Duration::from_secs(90000)), "25h+");
    }

    // ---- Busy indicator ---------------------------------------------------

    #[test]
    fn busy_indicator_ascii_cycles() {
        let mut ind = BusyIndicator::new(BusyPreset::Ascii);
        let f0 = ind.frame();
        assert_eq!(visible_width(&f0), 1, "ascii width 1");
        let f1 = ind.frame();
        let f2 = ind.frame();
        let f3 = ind.frame();
        // Should have cycled: | / - \.
        let seen = [f0, f1, f2, f3];
        let all_frames = ["|", "/", "-", "\\"];
        for f in &seen {
            assert!(all_frames.contains(&f.as_str()), "unexpected frame {f}");
        }
        // Wrap around.
        let f4 = ind.frame();
        assert_eq!(f4, "|", "wraps after 4 frames");
    }

    #[test]
    fn busy_indicator_braille_width_constant() {
        let mut ind = BusyIndicator::new(BusyPreset::Braille);
        let n = 10;
        for _ in 0..n {
            let f = ind.frame();
            assert_eq!(visible_width(&f), 1, "braille frame width 1");
        }
    }

    #[test]
    fn busy_indicator_kaomoji_width_constant() {
        let mut ind = BusyIndicator::new(BusyPreset::Kaomoji);
        let w = ind.width();
        let n = 6;
        for _ in 0..n {
            let f = ind.frame();
            assert_eq!(visible_width(&f), w, "kaomoji frame width = {w}");
        }
    }

    #[test]
    fn busy_indicator_pad_to_width() {
        let s = pad_to_width("x", 3);
        assert_eq!(s, "x  ");
        assert_eq!(visible_width(&s), 3);
    }

    // ---- Turn footer ------------------------------------------------------

    #[test]
    fn turn_footer_names_every_part_it_has() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_400),
            prompt_tokens: 3_400,
            cached_tokens: 2_900,
            completion_tokens: 250,
            cache_miss: false,
        };
        assert_eq!(footer.row(), "1.4s · 3.4k prompt (2.9k cached) · 250 out");
    }

    #[test]
    fn turn_footer_drops_the_cached_share_when_there_is_none() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(900),
            prompt_tokens: 900,
            cached_tokens: 0,
            completion_tokens: 40,
            cache_miss: false,
        };
        assert_eq!(footer.row(), "0.9s · 900 prompt · 40 out");
    }

    #[test]
    fn turn_footer_keeps_the_small_numbers_exact() {
        let footer = TurnFooter {
            elapsed: Duration::from_secs(75),
            prompt_tokens: 999,
            cached_tokens: 12,
            completion_tokens: 7,
            cache_miss: false,
        };
        assert_eq!(footer.row(), "1m 15s · 999 prompt (12 cached) · 7 out");
    }

    #[test]
    fn a_cold_cache_over_history_is_marked() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_200),
            prompt_tokens: 12_000,
            cached_tokens: 0,
            completion_tokens: 80,
            cache_miss: true,
        };
        assert_eq!(
            footer.row(),
            "1.2s · 12k prompt · 80 out · cache miss",
            "the miss is named, and no zero is dressed as data"
        );
    }

    #[test]
    fn compact_tokens_shortens_like_the_rest_of_the_ui() {
        assert_eq!(compact_tokens(0), "0");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(3_400), "3.4k");
        assert_eq!(compact_tokens(9_400), "9.4k");
        assert_eq!(compact_tokens(10_000), "10k");
        assert_eq!(compact_tokens(128_000), "128k");
        assert_eq!(compact_tokens(1_200_000), "1.2M");
        assert_eq!(compact_tokens(128_000_000), "128M");
    }

    // ---- Render -----------------------------------------------------------

    #[test]
    fn render_pads_to_full_width() {
        let mut sl = StatusLine::new();
        for w in [40u16, 80, 120] {
            let line = sl.render(w);
            assert_eq!(line.len(), w as usize, "status bar spans width {w}");
            assert!(line.starts_with("starting"), "line: {line}");
        }
    }

    #[test]
    fn render_initial_state() {
        let mut sl = StatusLine::new();
        let line = sl.render(80);
        assert!(line.contains("starting"), "line: {line}");
    }

    #[test]
    fn render_ready_state() {
        let mut sl = StatusLine::new();
        sl.transition(StatusEvent::ProviderReady);
        let line = sl.render(80);
        assert!(line.contains("ready"), "line: {line}");
    }

    #[test]
    fn render_truncated() {
        let mut sl = StatusLine::new();
        sl.transition(StatusEvent::ProviderReady);
        // Very narrow width — should truncate.
        let line = sl.render(4);
        assert!(line.len() <= 4, "truncated line: {line}");
    }

    #[test]
    fn render_badges_appear() {
        let mut sl = StatusLine::new();
        sl.transition(StatusEvent::ProviderReady);
        sl.badges.yolo = true;
        let line = sl.render(80);
        assert!(line.contains("YOLO"), "line: {line}");
    }

    #[test]
    fn render_with_busy_indicator() {
        let mut sl = StatusLine::new();
        sl.transition(StatusEvent::ProviderReady);
        sl.transition(StatusEvent::PromptSent);
        sl.transition(StatusEvent::FirstToken);
        // Running state → busy indicator shown.
        let line = sl.render(80);
        assert!(line.contains("run"), "line: {line}");
        // Busy indicator is at least 1 char wide.
        assert!(line.len() > 10, "line: {line}");
    }

    #[test]
    fn render_no_timer_before_prompt() {
        let mut sl = StatusLine::new();
        sl.transition(StatusEvent::ProviderReady);
        // Ready state, no timer.
        let line = sl.render(80);
        assert!(!line.contains("⏱"), "no timer before prompt: {line}");
        assert!(!line.contains("⏲"), "no frozen timer: {line}");
    }
}
