//! The terminal's own channels: the tab title (OSC 2) and the progress bar
//! (OSC 9;4), for the run state.
//!
//! omp keeps the tab readable without looking at the pane: a spinner while the
//! agent works, `>` when it is the user's turn, `!` when it is blocked on one
//! (`tui.titleState`; `pi-coding-agent/src/utils/title-generator.ts:862`),
//! written as `\x1b]0;…\x07` by `pi-tui/src/terminal.ts:2548`). titi wrote no
//! title at all, so a tab showed the shell's own text for the whole session.
//!
//! The same tab strip carries the second channel: omp's `terminal.showProgress`
//! raises an indeterminate OSC 9;4 bar for the whole turn
//! (`pi-tui/src/terminal.ts:38-39,602`), so a person who tabbed away sees that
//! the agent is working without reading the pane. The bar and the title are
//! one lifecycle — raised with the turn, cleared on every way out of it — so
//! they are composed by one tick and handed back by one
//! [`restore`].
//!
//! The title here is a function of the run state and the label — never of the
//! clock. The run loop composes it on the tick it already has (the one the
//! progress row rides) and writes the escape only when the composed title
//! changed, so a tick in an unchanged state writes nothing.

use titi_tui::theme::Theme;
use titi_tui::width::{truncate_to_width, visible_width};

/// The run state the title shows.
///
/// The four working variants are one title — the tab has room for one glyph,
/// and what a person glancing at it needs is "working", not which half of the
/// turn is working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleState {
    /// A turn is in flight and the model has not answered yet.
    Waiting,
    /// Assistant text is arriving.
    Streaming,
    /// Reasoning is arriving and no answer text has yet.
    Thinking,
    /// A tool call is running.
    Tool,
    /// No turn is running: it is the user's turn.
    Idle,
    /// Waiting on the user: an approval, the exit confirmation, a sign-in.
    Blocked,
    /// The last turn failed.
    Error,
}

impl TitleState {
    /// Whether a turn is in flight in this state.
    pub fn working(self) -> bool {
        matches!(
            self,
            Self::Waiting | Self::Streaming | Self::Thinking | Self::Tool
        )
    }
}

/// The brand every title opens with.
const BRAND: &str = "titi";

/// Most cells a title may take.
///
/// A tab strip cuts a title that overruns it, and it cuts it silently; the
/// ellipsis here is what says the label was longer than the tab.
const TITLE_MAX: usize = 48;

/// The mark each run state draws, resolved from the theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleGlyphs {
    /// The working mark: the status spinner's first frame.
    pub working: String,
    /// A running tool's mark — the status row's own tool glyph.
    pub tool: String,
    /// The user's turn.
    pub idle: String,
    /// Blocked on the user.
    pub blocked: String,
    /// The last turn failed.
    pub error: String,
}

impl TitleGlyphs {
    /// The marks the active theme resolves.
    ///
    /// Every one is a theme symbol, so the `ascii` preset — the fallback the
    /// rest of the screen honours — gives ascii here too (`|`, `bg`, `>`,
    /// `[!]`, `[!!]`) rather than braille and nerd glyphs.
    pub fn for_theme(theme: &Theme) -> Self {
        Self {
            working: theme
                .spinner_frames()
                .first()
                .cloned()
                .unwrap_or_else(|| "-".to_owned()),
            tool: mark(theme, "icon.job", "-"),
            idle: mark(theme, "nav.cursor", ">"),
            blocked: mark(theme, "icon.warning", "!"),
            error: mark(theme, "status.error", "x"),
        }
    }

    /// The mark this state draws.
    fn for_state(&self, state: TitleState) -> &str {
        match state {
            TitleState::Waiting | TitleState::Streaming | TitleState::Thinking => &self.working,
            TitleState::Tool => &self.tool,
            TitleState::Idle => &self.idle,
            TitleState::Blocked => &self.blocked,
            TitleState::Error => &self.error,
        }
    }
}

/// A theme symbol, or `fallback` when the theme's preset has none.
fn mark(theme: &Theme, key: &str, fallback: &str) -> String {
    let value = theme.symbol(key);
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// The tab title: `titi <mark> <label>`, cut to [`TITLE_MAX`] cells.
///
/// Without a label the mark still trails the brand (`titi >`), so the state is
/// readable before the session has a name.
pub fn title(state: TitleState, label: &str, glyphs: &TitleGlyphs) -> String {
    let text = format!("{BRAND} {} {}", glyphs.for_state(state), sanitize(label));
    let trimmed = text.trim_end();
    if visible_width(trimmed) <= TITLE_MAX {
        return trimmed.to_owned();
    }
    format!("{}…", truncate_to_width(trimmed, TITLE_MAX - 1))
}

/// The OSC 2 sequence that sets the title: `ESC ] 2 ; <text> BEL`.
pub fn set_title(text: &str) -> String {
    format!("\x1b]2;{}\x07", sanitize(text))
}

/// The sequence that hands the tab back to the shell, for the restore path.
pub fn reset_title() -> String {
    "\x1b]2;\x07".to_owned()
}

/// OSC 9;4 state 3: an indeterminate progress bar — the terminal's own
/// spinner for the whole turn, with no fraction to report.
pub const PROGRESS_SET: &str = "\x1b]9;4;3\x07";

/// OSC 9;4 state 0: no bar.
///
/// Written on every way out of a turn — it finished, it failed, it was
/// cancelled, or the screen itself is going away — because a bar left running
/// says the agent is still working when nobody is.
pub const PROGRESS_CLEAR: &str = "\x1b]9;4;0\x07";

/// The bytes that hand the terminal's own channels back: the empty OSC 2 that
/// gives the tab back, and — when the run raised a progress bar — the OSC 9;4
/// clear, so the bar cannot outlive the screen that raised it.
pub fn restore(progress: bool) -> String {
    let title = reset_title();
    if progress {
        format!("{title}{PROGRESS_CLEAR}")
    } else {
        title
    }
}

/// Drops the characters that would end the title's escape early or write to
/// the terminal through it: a control character in a session name must never
/// reach the OSC payload (`\x07` would close it, `\x1b` would start a new one).
fn sanitize(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marks a plain theme resolves, built by hand so a test is about the
    /// builder and not about a palette.
    fn glyphs() -> TitleGlyphs {
        TitleGlyphs {
            working: "|".to_owned(),
            tool: "bg".to_owned(),
            idle: ">".to_owned(),
            blocked: "[!]".to_owned(),
            error: "[!!]".to_owned(),
        }
    }

    fn ascii_theme() -> Theme {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            symbol_preset_override: Some(titi_tui::theme::symbols::SymbolPreset::Ascii),
            ..Default::default()
        };
        titi_tui::theme::loader::load_theme("dark", &options).expect("built-in ascii theme")
    }

    #[test]
    fn every_state_has_a_title() {
        let cases = [
            (TitleState::Waiting, "titi | session"),
            (TitleState::Streaming, "titi | session"),
            (TitleState::Thinking, "titi | session"),
            (TitleState::Tool, "titi bg session"),
            (TitleState::Idle, "titi > session"),
            (TitleState::Blocked, "titi [!] session"),
            (TitleState::Error, "titi [!!] session"),
        ];
        for (state, expected) in cases {
            assert_eq!(title(state, "session", &glyphs()), expected, "{state:?}");
        }
        // The working phases are one title: the tab has one glyph to give.
        assert!(TitleState::Waiting.working());
        assert!(TitleState::Tool.working());
        assert!(!TitleState::Idle.working());
        assert!(!TitleState::Blocked.working());
        assert!(!TitleState::Error.working());
    }

    #[test]
    fn a_title_without_a_label_keeps_its_state() {
        assert_eq!(title(TitleState::Idle, "", &glyphs()), "titi >");
        assert_eq!(title(TitleState::Waiting, "  ", &glyphs()), "titi |");
    }

    #[test]
    fn a_long_label_is_cut_with_an_ellipsis() {
        let label = "a".repeat(200);
        let cut = title(TitleState::Idle, &label, &glyphs());
        assert!(
            visible_width(&cut) <= TITLE_MAX,
            "{} cells: {cut}",
            visible_width(&cut)
        );
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.starts_with("titi > aaa"), "{cut}");
    }

    #[test]
    fn a_label_that_just_fits_is_not_cut() {
        let label = "x".repeat(TITLE_MAX - "titi > ".len());
        let exact = title(TitleState::Idle, &label, &glyphs());
        assert!(!exact.contains('…'), "{exact}");
        assert_eq!(visible_width(&exact), TITLE_MAX);
    }

    #[test]
    fn a_control_character_in_the_label_never_reaches_the_escape() {
        // BEL would end the OSC and ESC would open another one; a session name
        // is not allowed to write to the terminal through the title.
        let label = "evil\x07\x1b]2;hijacked\nname\ttab";
        let rendered = title(TitleState::Idle, label, &glyphs());
        assert_eq!(rendered, "titi > evil]2;hijackednametab", "{rendered}");

        let escape = set_title(&rendered);
        assert!(escape.starts_with("\x1b]2;titi > evil"), "{escape:?}");
        assert_eq!(escape.matches('\x07').count(), 1, "{escape:?}");
        assert_eq!(escape.matches('\x1b').count(), 1, "{escape:?}");
        assert!(!escape.contains('\n'), "{escape:?}");
    }

    #[test]
    fn the_escape_is_osc_two_and_the_reset_hands_the_tab_back() {
        assert_eq!(set_title("titi > s"), "\x1b]2;titi > s\x07");
        assert_eq!(reset_title(), "\x1b]2;\x07");
    }

    #[test]
    fn the_progress_escapes_are_the_indeterminate_set_and_the_clear() {
        assert_eq!(PROGRESS_SET, "\x1b]9;4;3\x07");
        assert_eq!(PROGRESS_CLEAR, "\x1b]9;4;0\x07");
    }

    #[test]
    fn the_restore_hands_both_channels_back() {
        // Without a bar the restore is the title's own reset, byte for byte.
        assert_eq!(restore(false), reset_title());
        // With one, the clear follows it — the tab back first, then the bar.
        assert_eq!(
            restore(true),
            format!("{}{}", reset_title(), PROGRESS_CLEAR)
        );
    }

    #[test]
    fn the_ascii_preset_falls_back_to_ascii_marks() {
        let glyphs = TitleGlyphs::for_theme(&ascii_theme());
        assert_eq!(glyphs.working, "|");
        assert_eq!(glyphs.tool, "bg");
        assert_eq!(glyphs.idle, ">");
        assert_eq!(glyphs.blocked, "[!]");
        assert_eq!(glyphs.error, "[!!]");
        for glyph in [
            &glyphs.working,
            &glyphs.tool,
            &glyphs.idle,
            &glyphs.blocked,
            &glyphs.error,
        ] {
            assert!(glyph.is_ascii(), "{glyph:?} is not ascii");
        }
    }

    #[test]
    fn the_default_preset_uses_the_status_spinner_and_symbols() {
        let options = titi_tui::theme::loader::CreateThemeOptions::default();
        let theme = titi_tui::theme::loader::load_theme("titanium", &options).expect("built-in");
        let glyphs = TitleGlyphs::for_theme(&theme);
        assert_eq!(glyphs.working, "⣾", "the status line's own spinner frame");
        assert_eq!(glyphs.tool, "⚙");
        assert_eq!(glyphs.idle, "❯");
        assert_eq!(glyphs.blocked, "⚠");
        assert_eq!(glyphs.error, "✘");
    }
}
