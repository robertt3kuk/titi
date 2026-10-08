//! Emoji shortcodes and emoticons — hand-written table, composer expansion
//! (`:name:` → glyph on the closing colon; emoticon → glyph on the
//! terminating space/newline), and a separate emoji-suggestion picker.
//!
//! Table policy: the arrays below are **our own hand-written data** — the
//! same idea as omp's `emojis.json`, but never a vendored copy of that file.
//! It is a deliberately small "codes people actually type" subset.
//!
//! Adding entries:
//! - append to `EMOJI_SHORTCODES` (keep the array alphabetical; matching
//!   uses a length-descending index built once from it, so no manual
//!   re-sorting is ever needed);
//! - append to `EMOTICONS` (keep that one hand-sorted longest first —
//!   see `try_expand_emoticon`).

use std::sync::LazyLock;

use crate::theme::{Theme, ThemeColor};

/// Max number of rows the emoji suggestion picker shows at once.
pub const EMOJI_PICKER_MAX_ROWS: usize = 8;

/// Hand-written shortcode table. **Add new entries here**, alphabetically.
#[allow(clippy::type_complexity)]
const EMOJI_SHORTCODES: &[(&str, &str)] = &[
    ("airplane", "✈️"),
    ("alien", "👽"),
    ("angry", "😠"),
    ("apple", "🍎"),
    ("art", "🎨"),
    ("banana", "🍌"),
    ("bar_chart", "📊"),
    ("baseball", "⚾"),
    ("basketball", "🏀"),
    ("balloon", "🎈"),
    ("bear", "🐻"),
    ("beer", "🍺"),
    ("beers", "🍻"),
    ("bee", "🐝"),
    ("bike", "🚲"),
    ("birthday", "🎂"),
    ("blush", "😊"),
    ("boat", "⛵"),
    ("bomb", "💣"),
    ("book", "📖"),
    ("books", "📚"),
    ("broken_heart", "💔"),
    ("bulb", "💡"),
    ("bus", "🚌"),
    ("butterfly", "🦋"),
    ("cake", "🍰"),
    ("calendar", "📅"),
    ("camera", "📷"),
    ("castle", "🏰"),
    ("cat", "🐱"),
    ("chart_with_downwards_trend", "📉"),
    ("chart_with_upwards_trend", "📈"),
    ("cherry", "🍒"),
    ("chicken", "🐔"),
    ("church", "⛪"),
    ("clap", "👏"),
    ("clown", "🤡"),
    ("coffee", "☕"),
    ("computer", "💻"),
    ("confetti_ball", "🎊"),
    ("cow", "🐮"),
    ("crown", "👑"),
    ("crying", "😢"),
    ("dart", "🎯"),
    ("desktop", "🖥️"),
    ("diamond", "💎"),
    ("dizzy", "💫"),
    ("dolphin", "🐬"),
    ("dog", "🐶"),
    ("door", "🚪"),
    ("dragon", "🐉"),
    ("droplet", "💧"),
    ("drum", "🥁"),
    ("eightball", "🎱"),
    ("email", "📧"),
    ("envelope", "✉️"),
    ("exclamation", "❗"),
    ("expressionless", "😑"),
    ("eyes", "👀"),
    ("fish", "🐟"),
    ("fire", "🔥"),
    ("fries", "🍟"),
    ("frog", "🐸"),
    ("game_die", "🎲"),
    ("gem", "💎"),
    ("ghost", "👻"),
    ("gift", "🎁"),
    ("grin", "😁"),
    ("grinning", "😀"),
    ("guitar", "🎸"),
    ("hammer", "🔨"),
    ("hamster", "🐹"),
    ("heart", "❤️"),
    ("heart_eyes", "😍"),
    ("headphones", "🎧"),
    ("hospital", "🏥"),
    ("hourglass", "⏳"),
    ("house", "🏠"),
    ("idea", "💡"),
    ("iphone", "📱"),
    ("joy", "😂"),
    ("key", "🔑"),
    ("kiss", "😘"),
    ("koala", "🐨"),
    ("keyboard", "⌨️"),
    ("laughing", "😆"),
    ("leaf", "🍃"),
    ("lion", "🦁"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("lollipop", "🍭"),
    ("love_letter", "💌"),
    ("mag", "🔍"),
    ("mailbox", "📫"),
    ("memo", "📝"),
    ("microphone", "🎤"),
    ("money", "💰"),
    ("monkey", "🐵"),
    ("moon", "🌙"),
    ("mountain", "⛰️"),
    ("mouse", "🐭"),
    ("muscle", "💪"),
    ("musical_keyboard", "🎹"),
    ("musical_note", "🎵"),
    ("neutral_face", "😐"),
    ("night_with_stars", "🌃"),
    ("no_entry", "⛔"),
    ("notes", "🎶"),
    ("ocean", "🌊"),
    ("octopus", "🐙"),
    ("office", "🏢"),
    ("ok_hand", "👌"),
    ("orange", "🍊"),
    ("owl", "🦉"),
    ("package", "📦"),
    ("paperclip", "📎"),
    ("partly_sunny", "⛅"),
    ("party", "🥳"),
    ("pencil", "✏️"),
    ("penguin", "🐧"),
    ("phone", "📱"),
    ("pig", "🐷"),
    ("pizza", "🍕"),
    ("point_down", "👇"),
    ("point_left", "👈"),
    ("point_right", "👉"),
    ("point_up", "☝️"),
    ("poop", "💩"),
    ("pushpin", "📌"),
    ("rabbit", "🐰"),
    ("rage", "😡"),
    ("rainbow", "🌈"),
    ("raised_hands", "🙌"),
    ("robot", "🤖"),
    ("rocket", "🚀"),
    ("rofl", "🤣"),
    ("rolled_eyes", "🙄"),
    ("rose", "🌹"),
    ("sailboat", "⛵"),
    ("santa", "🎅"),
    ("school", "🏫"),
    ("scissors", "✂️"),
    ("scream", "😱"),
    ("seedling", "🌱"),
    ("shark", "🦈"),
    ("sheep", "🐑"),
    ("ship", "🚢"),
    ("skull", "💀"),
    ("sleepy", "😪"),
    ("smile", "🙂"),
    ("smiley", "😊"),
    ("smirk", "😏"),
    ("snowflake", "❄️"),
    ("soccer", "⚽"),
    ("sob", "😭"),
    ("space_invader", "👾"),
    ("spaghetti", "🍝"),
    ("sparkles", "✨"),
    ("star", "⭐"),
    ("star2", "🌟"),
    ("star_struck", "🤩"),
    ("stuck_out_tongue", "😛"),
    ("sun", "☀️"),
    ("sunflower", "🌻"),
    ("sun_with_face", "🌞"),
    ("tada", "🎉"),
    ("tea", "🍵"),
    ("telephone", "☎️"),
    ("tennis", "🎾"),
    ("thought_balloon", "💭"),
    ("thumbsdown", "👎"),
    ("thumbsup", "👍"),
    ("ticket", "🎫"),
    ("tiger", "🐯"),
    ("trophy", "🏆"),
    ("truck", "🚚"),
    ("umbrella", "☔"),
    ("warning", "⚠️"),
    ("watermelon", "🍉"),
    ("wave", "👋"),
    ("waving_hand", "👋"),
    ("white_check_mark", "✅"),
    ("wind_chime", "🎐"),
    ("wine_glass", "🍷"),
    ("wink", "😉"),
    ("wolf", "🐺"),
    ("world_map", "🗺️"),
    ("wrench", "🔧"),
    ("writing_hand", "✍️"),
    ("x", "❌"),
    ("zany_face", "🤪"),
    ("zzz", "💤"),
];

/// Hand-written emoticon table, **sorted longest first** (matching relies on
/// that order). Add new entries in decreasing length (`":-)"` before `":)"`).
#[allow(clippy::type_complexity)]
const EMOTICONS: &[(&str, &str)] = &[
    ("</3", "💔"),
    (":-)", "🙂"),
    (":-(", "🙁"),
    (":-D", "😃"),
    (":-P", "😛"),
    (":-p", "😛"),
    (":-O", "😮"),
    (":-*", "😘"),
    (":-|", "😐"),
    (":-/", "😕"),
    (":-\\", "😕"),
    (";-)", "😉"),
    ("<3", "❤️"),
    (":'(", "😢"),
    (":)", "🙂"),
    (":(", "🙁"),
    (":D", "😃"),
    (":P", "😛"),
    (":p", "😛"),
    (":O", "😮"),
    (":*", "😘"),
    (":|", "😐"),
    (":/", "😕"),
];

/// Shortcode name → glyph, or `None` when unknown.
#[must_use]
pub fn shortcode_lookup(name: &str) -> Option<&'static str> {
    EMOJI_SHORTCODES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, g)| *g)
}

/// All shortcodes sorted longest-first (then alphabetically). Built once.
static SORTED_INDEX: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| {
    let mut rows: Vec<(String, &'static str)> = EMOJI_SHORTCODES
        .iter()
        .map(|(name, glyph)| ((*name).to_string(), *glyph))
        .collect();
    rows.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    rows
});

/// All shortcodes sorted longest-first (then alphabetically).
#[must_use]
pub fn sorted_shortcodes() -> &'static [(String, &'static str)] {
    &SORTED_INDEX
}

/// Is `text` (the buffer up to the caret) code-like? True inside a fenced
/// code block (odd ``` count) or an unclosed inline code span (odd number of
/// backticks after the last complete fence).
#[must_use]
pub fn in_code_like(text: &str) -> bool {
    let fences = text.matches("```").count();
    if !fences.is_multiple_of(2) {
        return true;
    }
    let after_last_fence = match text.rfind("```") {
        Some(idx) => &text[idx + 3..],
        None => text,
    };
    !after_last_fence.matches('`').count().is_multiple_of(2)
}

/// Try expanding a shortnocode ending at `text` (the buffer up to and
/// excluding the closing colon just typed). Returns the byte offset of the
/// opening `:` and the replacement glyph.
///
/// Guards, in order — no expansion unless all hold:
///   1. the closing `:` has just been typed and a non-empty shortcode name
///      sits between the two colons (ASCII letters/digits, `_`, `-`, `+`,
///      at most 32 chars);
///   2. the character immediately before the opening `:` is not word-like
///      (alnum, `_`, `.`, `-`) — that is what keeps `http://x:y:` intact;
///   3. the text is not code-like (see `in_code_like`);
///   4. the name is in the table — unknown names stay literal.
#[must_use]
pub fn try_expand_shortcode(text: &str) -> Option<(usize, &'static str)> {
    let colon = text.rfind(':')?;
    let name = &text[colon + 1..];
    if name.is_empty() || name.len() > 32 {
        return None;
    }
    let name_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'));
    if !name_ok {
        return None;
    }
    if text[..colon]
        .chars()
        .next_back()
        .is_some_and(|before| before.is_alphanumeric() || matches!(before, '_' | '.' | '-'))
    {
        return None;
    }
    if in_code_like(text) {
        return None;
    }
    let glyph = shortcode_lookup(name)?;
    Some((colon, glyph))
}

/// Try expanding an emoticon that ends at `text` (the terminator — the
/// space/newline just typed — is *not* part of it). Returns the byte offset
/// where the emoticon starts and the glyph.
///
/// Guards: the emoticon must start at a word boundary (start of text or
/// preceded by whitespace — `a:-)` stays literal), and the text must not be
/// code-like. Longer emoticons match first (`EMOTICONS` order).
#[must_use]
pub fn try_expand_emoticon(text: &str) -> Option<(usize, &'static str)> {
    for (emoticon, glyph) in EMOTICONS {
        if !text.ends_with(emoticon) {
            continue;
        }
        let start = text.len() - emoticon.len();
        let boundary_ok = text[..start]
            .chars()
            .next_back()
            .map(|c| c.is_whitespace())
            .unwrap_or(true);
        if !boundary_ok {
            continue;
        }
        if in_code_like(text) {
            return None;
        }
        return Some((start, glyph));
    }
    None
}

/// The picker's open query for `text` (buffer up to the caret): a trailing
/// `:word` (`:word[:…]` where nothing after) whose characters are legal
/// shortcode-name characters, opened at a word boundary, not code-like, and
/// shorter than the longest plausible shortcode.
#[must_use]
pub fn trailing_query(text: &str) -> Option<&str> {
    let colon = text.rfind(':')?;
    let word = &text[colon + 1..];
    if word.is_empty() || word.len() > 32 {
        return None;
    }
    if !word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
    {
        return None;
    }
    if text[..colon]
        .chars()
        .next_back()
        .is_some_and(|before| before.is_alphanumeric() || matches!(before, '_' | '.' | '-'))
    {
        return None;
    }
    if in_code_like(text) {
        return None;
    }
    Some(word)
}

/// Separately-owned emoji suggestion picker (a second picker state next to
/// the slash `CompletionPanel`; the app's key loop owns routing and calls
/// these). It lists matching shortcodes with their glyphs while the user
/// types `:pre…`, and Tab/Enter accepts the highlighted one.
#[derive(Debug, Clone, Default)]
pub struct EmojiPicker {
    visible: bool,
    query: String,
    selected: usize,
}

impl EmojiPicker {
    /// Show the picker for `query` (the text after the trailing `:`).
    pub fn open(&mut self, query: &str) {
        self.visible = true;
        self.query = query.to_owned();
        self.selected = 0;
    }

    /// Hide without touching the composer text (Esc semantics).
    pub fn hide(&mut self) {
        self.visible = false;
        self.query.clear();
        self.selected = 0;
    }

    /// Whether the picker is showing rows.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Matching shortcodes (longest first), glyph last.
    pub fn matches(&self) -> impl Iterator<Item = (&str, &str)> {
        let query = self.query.clone();
        sorted_shortcodes()
            .iter()
            .filter(move |(name, _)| name.starts_with(query.as_str()))
            .take(EMOJI_PICKER_MAX_ROWS)
            .map(|(name, glyph)| (name.as_str(), *glyph))
    }

    /// Move the selection; wraps (returns `false` when there is nothing).
    pub fn move_selection(&mut self, up: bool) -> bool {
        let count = self.matches().count();
        if count == 0 {
            return false;
        }
        self.selected = if up {
            (self.selected + count - 1) % count
        } else {
            (self.selected + 1) % count
        };
        true
    }

    /// Accept the highlighted suggestion: the glyph to insert, or `None`
    /// when hidden or there are no matches.
    #[must_use]
    pub fn accept(&self) -> Option<&'static str> {
        if !self.visible {
            return None;
        }
        self.matches()
            .nth(self.selected)
            .and_then(|(name, _)| shortcode_lookup(name))
    }

    /// Rendered picker rows (no chrome) for embedding above the composer
    /// prompt. Selected row carries an accent-coloured marker; the rest are
    /// muted. All styling comes from `theme` — never a hardcoded colour.
    #[must_use]
    pub fn item_rows(&self, theme: &Theme) -> Vec<String> {
        if !self.visible {
            return Vec::new();
        }
        self.matches()
            .enumerate()
            .map(|(i, (name, glyph))| {
                let label = format!("{name}:{glyph}");
                if i == self.selected {
                    theme.fg(ThemeColor::Accent, format!("▶ {label}").as_str())
                } else {
                    theme.fg(ThemeColor::Muted, format!("  {label}").as_str())
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_and_unknown() {
        assert_eq!(shortcode_lookup("tada"), Some("🎉"));
        assert_eq!(shortcode_lookup("nope"), None);
    }

    #[test]
    fn sorted_index_is_longest_first() {
        let rows = sorted_shortcodes();
        assert!(rows.windows(2).all(|w| w[0].0.len() >= w[1].0.len()));
    }

    #[test]
    fn shortcode_expansion_finds_trailing_candidate() {
        assert_eq!(try_expand_shortcode("hi :tada"), Some((3, "🎉")));
        assert_eq!(try_expand_shortcode("plain"), None);
        assert_eq!(try_expand_shortcode("hi :nope"), None);
    }

    #[test]
    fn shortcode_guard_rejects_url_context() {
        assert_eq!(try_expand_shortcode("http://x:y"), None);
        assert_eq!(try_expand_shortcode("word:stringname"), None);
    }

    #[test]
    fn shortcode_guard_rejects_code_like() {
        assert_eq!(try_expand_shortcode("```\n:x\n```"), None);
        assert_eq!(try_expand_shortcode("`nope:"), None);
        assert_eq!(try_expand_shortcode("`tada"), None);
        assert_eq!(try_expand_shortcode("ok :tada"), Some((3, "🎉")));
    }

    #[test]
    fn emoticon_expansion_matches_longest_first() {
        assert_eq!(try_expand_emoticon(":-)"), Some((0, "🙂")));
        assert_eq!(try_expand_emoticon("no:"), None);
        assert_eq!(try_expand_emoticon("a<3"), None);
        assert_eq!(try_expand_emoticon("<3"), Some((0, "❤️")));
    }

    #[test]
    fn query_is_exposed_for_picker() {
        assert_eq!(trailing_query("hello :sm"), Some("sm"));
        assert_eq!(trailing_query("hello smile"), None);
        assert_eq!(trailing_query("x:y"), None);
    }

    #[test]
    fn picker_rows_use_theme_colours() {
        use crate::theme::global;
        global().init("titanium");
        let theme = global().current().expect("theme");
        let mut picker = EmojiPicker::default();
        picker.open("sm");
        assert!(picker.is_visible());
        let rows = picker.item_rows(&theme);
        assert!(!rows.is_empty(), "no rows from `sm`");
        assert!(rows[0].contains("smiley:😊"), "rows[i] = {}", rows[0]);
        assert!(rows.iter().any(|row| row.contains("smile:🙂")));
        assert!(
            rows[0].contains('\u{1b}'),
            "selected row styled: {}",
            rows[0]
        );
        picker.move_selection(false);
        let rows = picker.item_rows(&theme);
        assert!(
            rows[0].starts_with('\u{1b}') && rows[0].contains("smiley:"),
            "selection moved: {}",
            rows[0]
        );
    }

    #[test]
    fn picker_accept_gives_glyph() {
        let mut picker = EmojiPicker::default();
        picker.open("ta");
        assert_eq!(picker.accept(), Some("🎉"));
        picker.hide();
        assert_eq!(picker.accept(), None);
        assert!(!picker.is_visible());
    }
}
