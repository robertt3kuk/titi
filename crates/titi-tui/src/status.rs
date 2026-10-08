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
    /// What the turn cost, in micro-dollars, when the model has a price.
    ///
    /// `None` for an unpriced model — a local one, a subscription backend, or
    /// a price nobody wrote down. The row then omits the money entirely:
    /// `$0.000` would read as free, and unpriced is not free.
    pub cost_micro_usd: Option<u64>,
}

/// Decimals on one turn's cost: a turn is small, so the figure is a
/// ten-thousandth of a dollar deep. A session's total is rounded to cents
/// ([`SESSION_COST_DECIMALS`]).
pub const TURN_COST_DECIMALS: usize = 4;

/// Decimals on a session's total: a session is a day's work, and cents are
/// what a user reads.
pub const SESSION_COST_DECIMALS: usize = 2;

/// Which parts of a finished turn's footer the screen leaves on.
///
/// The three switches omp keeps for the same row (`display.showTokenUsage`,
/// `display.showTurnTime`, `display.cacheMissMarker` in its settings registry),
/// read here rather than in the settings crate: this is the reader's side of
/// the row, and what an unset key means is the screen's decision. The screen
/// resolves them once and hands them to [`TurnFooter::row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnFooterSwitches {
    /// The turn's wall time (`1.4s`).
    pub time: bool,
    /// The token counts and the money that rides with them.
    pub tokens: bool,
    /// The marker on a request that re-paid for its own history.
    pub cache_miss: bool,
}

impl Default for TurnFooterSwitches {
    /// Everything on: what a screen that read no settings draws, and what
    /// every key leaves unset.
    fn default() -> Self {
        Self {
            time: true,
            tokens: true,
            cache_miss: true,
        }
    }
}

impl TurnFooter {
    /// The row: `1.4s · 3.4k prompt (2.9k cached) · 250 out`, a trailing
    /// `· $0.004` when the model has a price, and a `· cache miss` when the
    /// request re-paid for its own history.
    ///
    /// `switches` is the reader's half of it: a muted part is not built at all,
    /// and a row with no part left is `None` — the screen adds no line rather
    /// than a line that says nothing. The money rides with the token counts
    /// ([`TurnFooterSwitches::tokens`]): a price is what those tokens cost, and
    /// `$0.004` alone is a figure without its subject.
    ///
    /// The cached share is named only when there is one: `(0 cached)` would be
    /// a zero dressed as data, and the miss marker already says the honest
    /// thing about a cold request. The money is the same rule — a descriptor
    /// without a price prints no figure at all.
    pub fn row(&self, switches: TurnFooterSwitches) -> Option<String> {
        let mut parts = Vec::new();
        if switches.time {
            parts.push(format_turn_time(self.elapsed));
        }
        if switches.tokens {
            parts.push(if self.cached_tokens > 0 {
                format!(
                    "{} prompt ({} cached)",
                    compact_tokens(self.prompt_tokens),
                    compact_tokens(self.cached_tokens)
                )
            } else {
                format!("{} prompt", compact_tokens(self.prompt_tokens))
            });
            parts.push(format!("{} out", compact_tokens(self.completion_tokens)));
        }
        if switches.cache_miss && self.cache_miss {
            parts.push("cache miss".to_owned());
        }
        if switches.tokens
            && let Some(cost) = self.cost_micro_usd
        {
            parts.push(format_usd(cost, TURN_COST_DECIMALS));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The most decimals a money figure may grow to before it counts as zero: one
/// micro-dollar, the unit the engine's cost arithmetic keeps.
const MAX_COST_DECIMALS: usize = 6;

/// `micro_usd` rounded to `decimals` places, in units of `10^-decimals`
/// dollars. Integer arithmetic, so the rounding is the decimal one a reader
/// would do — never a float's nearest binary neighbour.
fn scaled_usd(micro_usd: u64, decimals: usize) -> u128 {
    let scale = 10u128.pow(decimals as u32);
    (u128::from(micro_usd) * scale + 500_000) / 1_000_000
}

/// A cost as the screen prints it: `$0.004`, `$0.38`, `$1.20`.
///
/// `decimals` is the figure's own precision — four on one turn's cost
/// ([`TURN_COST_DECIMALS`]), two on a session's total
/// ([`SESSION_COST_DECIMALS`]). Two rules sit on top of it, both about not
/// lying with zeros:
///
/// - trailing zeros are dropped, down to two places at the least, because a
///   fourth decimal that is `0` says nothing — but `$0.50` never reads
///   `$0.5`;
/// - **a fraction of a cent is never printed as `$0.00`.** An amount that
///   rounds to zero at the asked-for precision takes more digits instead,
///   until it shows what it is (at most [`MAX_COST_DECIMALS`], one
///   micro-dollar). Only an exact zero reads `$0.00`.
pub fn format_usd(micro_usd: u64, decimals: usize) -> String {
    let mut digits = decimals.max(2);
    let scaled = loop {
        let scaled = scaled_usd(micro_usd, digits);
        if scaled > 0 || micro_usd == 0 || digits >= MAX_COST_DECIMALS {
            break scaled;
        }
        digits += 1;
    };
    let unit = 10u128.pow(digits as u32);
    let whole = scaled / unit;
    let mut fraction = format!("{:0width$}", scaled % unit, width = digits);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${whole}.{fraction}")
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
// Generation rate
// ---------------------------------------------------------------------------

/// How long the rolling window reaches back.
const RATE_WINDOW: Duration = Duration::from_secs(4);

/// The least time a reading may stand on. A rate computed from one delta and
/// 30 ms is arithmetic, not a measurement.
const RATE_MIN_SAMPLE: f64 = 0.4;

/// Tokens per character, omp's rough four characters to a token. The number is
/// the whole reason the row's reading wears a `~`.
const TOKENS_PER_CHAR: f64 = 0.25;

/// A rolling estimate of the model's generation rate, in tokens per second.
///
/// **An estimate, not a count.** The samples are the characters the working
/// row already counts — the answer's, or the reasoning's while no answer has
/// started — so no second counter exists to drift from the row's own number.
/// That count is divided by four, because the provider's real token counts
/// arrive with the turn's usage report and nothing reports them while the
/// text is still arriving. The row prints the `~` that says so.
///
/// Rolling: samples older than [`RATE_WINDOW`] are dropped, so the figure
/// follows a slow stretch after a fast one. The last reading is kept across a
/// tool call or a round boundary — a row between two bursts keeps its number
/// rather than blinking out — and a new turn clears it, so a turn with nothing
/// streamed yet shows no number at all.
#[derive(Debug, Clone)]
pub struct TokenRate {
    window: Duration,
    /// `(when, cumulative characters)`, oldest first.
    samples: std::collections::VecDeque<(Instant, usize)>,
    reading: Option<f64>,
}

impl Default for TokenRate {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenRate {
    pub fn new() -> Self {
        Self {
            window: RATE_WINDOW,
            samples: std::collections::VecDeque::new(),
            reading: None,
        }
    }

    /// A new turn: no reading until this turn's own deltas arrive.
    pub fn begin(&mut self) {
        self.samples.clear();
        self.reading = None;
    }

    /// Feed the run's cumulative character count at `now`.
    ///
    /// A count below the previous sample is a series that started over — the
    /// row swaps from reasoning to the answer, and the reasoning it counted is
    /// dropped with it — so the samples are thrown away and the count becomes
    /// the new baseline. A count that did not move is not a sample: pushing it
    /// would make the reading decay while nothing happened, and the row would
    /// show a number falling for a stream that had merely paused. The reading
    /// itself is kept across both, because the last honest number beats a
    /// blank row between two bursts.
    pub fn observe(&mut self, chars: usize, now: Instant) {
        match self.samples.back() {
            Some((_, last)) if chars < *last => self.samples.clear(),
            Some((_, last)) if chars == *last => {
                self.evict(now);
                self.recompute();
                return;
            }
            _ => {}
        }
        self.samples.push_back((now, chars));
        self.evict(now);
        self.recompute();
    }

    /// Drop the samples the window no longer reaches.
    ///
    /// Never below two: the window bounds the reading, but the pair that
    /// actually produced text is the least that can measure anything, and
    /// throwing it away would blind the row on a stream that had paused once.
    fn evict(&mut self, now: Instant) {
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > self.window)
        {
            self.samples.pop_front();
        }
    }

    /// The last reading in tokens per second, or `None` before any has been
    /// earned.
    pub fn reading(&self) -> Option<f64> {
        self.reading
    }

    /// Recompute the reading from the window; too little time or no growth
    /// leaves the previous one standing.
    fn recompute(&mut self) {
        let (Some((first_at, first)), Some((last_at, last))) =
            (self.samples.front().copied(), self.samples.back().copied())
        else {
            return;
        };
        let elapsed = last_at.duration_since(first_at).as_secs_f64();
        let chars = last.saturating_sub(first);
        if elapsed < RATE_MIN_SAMPLE || chars == 0 {
            return;
        }
        self.reading = Some(chars as f64 / elapsed * TOKENS_PER_CHAR);
    }
}

/// A reading as the working row prints it: `~42 tok/s`, or `~<1 tok/s` under
/// one — the `~` is the row's own mark for an estimate.
pub fn format_rate(tokens_per_second: f64) -> String {
    if tokens_per_second < 1.0 {
        return "~<1 tok/s".to_owned();
    }
    format!("~{} tok/s", tokens_per_second.round() as u64)
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

    /// The row as a screen that read no settings draws it: every part on, which
    /// is what an unset `display.turnFooter.*` key means.
    fn shown(footer: &TurnFooter) -> String {
        footer
            .row(TurnFooterSwitches::default())
            .unwrap_or_default()
    }

    #[test]
    fn turn_footer_names_every_part_it_has() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_400),
            prompt_tokens: 3_400,
            cached_tokens: 2_900,
            completion_tokens: 250,
            cache_miss: false,
            cost_micro_usd: None,
        };
        assert_eq!(shown(&footer), "1.4s · 3.4k prompt (2.9k cached) · 250 out");
    }

    #[test]
    fn turn_footer_drops_the_cached_share_when_there_is_none() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(900),
            prompt_tokens: 900,
            cached_tokens: 0,
            completion_tokens: 40,
            cache_miss: false,
            cost_micro_usd: None,
        };
        assert_eq!(shown(&footer), "0.9s · 900 prompt · 40 out");
    }

    #[test]
    fn turn_footer_keeps_the_small_numbers_exact() {
        let footer = TurnFooter {
            elapsed: Duration::from_secs(75),
            prompt_tokens: 999,
            cached_tokens: 12,
            completion_tokens: 7,
            cache_miss: false,
            cost_micro_usd: None,
        };
        assert_eq!(shown(&footer), "1m 15s · 999 prompt (12 cached) · 7 out");
    }

    #[test]
    fn a_cold_cache_over_history_is_marked() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_200),
            prompt_tokens: 12_000,
            cached_tokens: 0,
            completion_tokens: 80,
            cache_miss: true,
            cost_micro_usd: None,
        };
        assert_eq!(
            shown(&footer),
            "1.2s · 12k prompt · 80 out · cache miss",
            "the miss is named, and no zero is dressed as data"
        );
    }

    /// A priced model puts the turn's cost at the end of the row, rounded to
    /// four places.
    #[test]
    fn turn_footer_states_the_cost_of_a_priced_turn() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_400),
            prompt_tokens: 3_400,
            cached_tokens: 2_900,
            completion_tokens: 250,
            cache_miss: false,
            cost_micro_usd: Some(4_500),
        };
        assert_eq!(
            shown(&footer),
            "1.4s · 3.4k prompt (2.9k cached) · 250 out · $0.0045"
        );

        // The same turn with a cold cache keeps every part, money last.
        let cold = TurnFooter {
            cache_miss: true,
            ..footer
        };
        assert_eq!(
            shown(&cold),
            "1.4s · 3.4k prompt (2.9k cached) · 250 out · cache miss · $0.0045"
        );
    }

    /// An unpriced model has no figure at all: `$0.000` would read as free,
    /// and a local model was never free — its price is simply unknown.
    #[test]
    fn turn_footer_says_nothing_about_money_without_a_price() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(900),
            prompt_tokens: 900,
            cached_tokens: 0,
            completion_tokens: 40,
            cache_miss: false,
            cost_micro_usd: None,
        };
        let row = shown(&footer);
        assert_eq!(row, "0.9s · 900 prompt · 40 out");
        assert!(!row.contains('$'), "no price, no figure: {row}");
    }

    /// A muted part is not built at all, so the row left behind reads as it
    /// would have without it — no empty `· ·` where something was cut.
    #[test]
    fn turn_footer_leaves_out_what_is_switched_off() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_400),
            prompt_tokens: 3_400,
            cached_tokens: 2_900,
            completion_tokens: 250,
            cache_miss: true,
            cost_micro_usd: Some(4_000),
        };
        let no_time = TurnFooterSwitches {
            time: false,
            ..Default::default()
        };
        assert_eq!(
            footer.row(no_time).unwrap_or_default(),
            "3.4k prompt (2.9k cached) · 250 out · cache miss · $0.004"
        );
        let no_tokens = TurnFooterSwitches {
            tokens: false,
            ..Default::default()
        };
        assert_eq!(
            footer.row(no_tokens).unwrap_or_default(),
            "1.4s · cache miss",
            "the money rides with the token counts"
        );
        let no_marker = TurnFooterSwitches {
            cache_miss: false,
            ..Default::default()
        };
        assert_eq!(
            footer.row(no_marker).unwrap_or_default(),
            "1.4s · 3.4k prompt (2.9k cached) · 250 out · $0.004"
        );
        // A marker switch off on a warm turn changes nothing: there was none.
        let warm = TurnFooter {
            cache_miss: false,
            ..footer
        };
        assert_eq!(
            warm.row(no_marker).unwrap_or_default(),
            "1.4s · 3.4k prompt (2.9k cached) · 250 out · $0.004"
        );
    }

    /// Nothing left is no row: the screen adds no line, so a quiet transcript
    /// has no blank one either.
    #[test]
    fn a_footer_with_every_part_switched_off_is_no_row() {
        let footer = TurnFooter {
            elapsed: Duration::from_millis(1_400),
            prompt_tokens: 3_400,
            cached_tokens: 2_900,
            completion_tokens: 250,
            cache_miss: true,
            cost_micro_usd: Some(4_000),
        };
        let off = TurnFooterSwitches {
            time: false,
            tokens: false,
            cache_miss: false,
        };
        assert_eq!(footer.row(off), None);
        // …and a warm turn is no row with only the marker switch left on.
        let warm = TurnFooter {
            cache_miss: false,
            ..footer
        };
        let only_marker = TurnFooterSwitches {
            time: false,
            tokens: false,
            cache_miss: true,
        };
        assert_eq!(warm.row(only_marker), None);
    }

    /// Money is rounded to the places the figure carries, trailing zeros
    /// dropped but never below cents.
    #[test]
    fn usd_rounds_to_its_own_precision() {
        assert_eq!(format_usd(4_500, TURN_COST_DECIMALS), "$0.0045");
        assert_eq!(format_usd(4_000, TURN_COST_DECIMALS), "$0.004");
        assert_eq!(format_usd(830_000, SESSION_COST_DECIMALS), "$0.83");
        assert_eq!(format_usd(500_000, SESSION_COST_DECIMALS), "$0.50");
        assert_eq!(format_usd(1_200_000, TURN_COST_DECIMALS), "$1.20");
        assert_eq!(format_usd(375_000_000, SESSION_COST_DECIMALS), "$375.00");
    }

    /// A fraction of a cent is never printed as `$0.00`: the figure takes more
    /// digits rather than rounding a real cost away. Only an exact zero — a
    /// model priced at zero — reads `$0.00`.
    #[test]
    fn usd_never_prints_a_real_fraction_as_zero() {
        assert_eq!(format_usd(0, SESSION_COST_DECIMALS), "$0.00");
        assert_eq!(format_usd(0, TURN_COST_DECIMALS), "$0.00");
        // Three tenths of a cent: two places would say `$0.00`.
        assert_eq!(format_usd(3_000, SESSION_COST_DECIMALS), "$0.003");
        assert_eq!(format_usd(30, TURN_COST_DECIMALS), "$0.00003");
        assert_eq!(format_usd(3, TURN_COST_DECIMALS), "$0.000003");
        // Half a cent rounds up at two places, so it is not a zero case.
        assert_eq!(format_usd(5_000, SESSION_COST_DECIMALS), "$0.01");
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

    // ---- Generation rate --------------------------------------------------

    #[test]
    fn a_rate_needs_time_and_text_before_it_says_anything() {
        let mut rate = TokenRate::new();
        let start = Instant::now();
        rate.observe(0, start);
        assert_eq!(rate.reading(), None, "one sample is not a rate");
        // A tenth of a second later, a hundred characters: still too soon.
        rate.observe(100, start + Duration::from_millis(100));
        assert_eq!(rate.reading(), None);
        // A second in, 400 characters: a four-characters-a-token estimate.
        rate.observe(400, start + Duration::from_secs(1));
        let reading = rate.reading().expect("a reading");
        assert!((reading - 100.0).abs() < 0.01, "{reading}");
        assert_eq!(format_rate(reading), "~100 tok/s");
    }

    #[test]
    fn a_series_that_starts_over_is_a_new_baseline_not_a_negative_rate() {
        let mut rate = TokenRate::new();
        let start = Instant::now();
        // Reasoning arrives, then the answer starts and the reasoning counter
        // it was read from is dropped: the count falls back to zero.
        rate.observe(2000, start);
        rate.observe(2400, start + Duration::from_secs(1));
        let reasoning = rate.reading().expect("a reading");
        rate.observe(0, start + Duration::from_secs(2));
        assert_eq!(rate.reading(), Some(reasoning), "the last reading stands");
        rate.observe(800, start + Duration::from_secs(3));
        let answer = rate.reading().expect("a reading");
        assert!(
            answer > 0.0 && answer.is_finite(),
            "a restart is not a negative rate: {answer}"
        );
    }

    #[test]
    fn a_new_turn_clears_the_reading_and_the_window_forgets_old_samples() {
        let mut rate = TokenRate::new();
        let start = Instant::now();
        rate.observe(0, start);
        rate.observe(400, start + Duration::from_secs(1));
        assert!(rate.reading().is_some());
        rate.begin();
        assert_eq!(rate.reading(), None, "a new turn shows no number");

        // Growth that stopped four seconds ago no longer counts: the window
        // reaches back, so a stalled stream reads as slower, not as its old
        // average.
        let mut rolling = TokenRate::new();
        rolling.observe(0, start);
        rolling.observe(400, start + Duration::from_secs(1));
        let fast = rolling.reading().expect("a reading");
        rolling.observe(400, start + Duration::from_secs(5));
        assert_eq!(
            rolling.reading(),
            Some(fast),
            "a stall keeps the last number"
        );
        rolling.observe(440, start + Duration::from_secs(6));
        let slow = rolling.reading().expect("a reading");
        assert!(slow < fast, "the window moved on: {slow} vs {fast}");
    }

    #[test]
    fn a_rate_under_one_reads_as_under_one() {
        assert_eq!(format_rate(0.4), "~<1 tok/s");
        assert_eq!(format_rate(1.4), "~1 tok/s");
        assert_eq!(format_rate(41.6), "~42 tok/s");
    }
}
