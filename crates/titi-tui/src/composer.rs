//! Input composer — bracketed paste, OSC 5522 enhanced paste, paste
//! collapse, and non-blocking message queue (Steer / FollowUp).
//!
//! Contract: `docs/research/agent-ux/README.md`.

use std::collections::VecDeque;

use crate::cursor::CURSOR_MARKER;
use crate::theme::{Theme, ThemeColor};
use crate::width::{truncate_to_width, visible_width};

/// Mode for a queued message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Steer,
    FollowUp,
}

/// A message queued during streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub text: String,
    pub mode: QueueMode,
}

/// Result of collapsing a long paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteResult {
    /// Short enough to paste inline.
    Text(String),
    /// Long paste collapsed: preview + number of hidden lines.
    Collapsed {
        preview: String,
        omitted_lines: usize,
    },
    /// Single file path → attachment marker.
    Attachment { name: String, marker: String },
}

/// Parsed OSC 5522 enhanced paste payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Osc5522 {
    pub mime: String,
    pub data: Vec<u8>,
}

/// A paste-state spanning (simplified: engine tracks bracketed vs enhanced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteState {
    None,
    Bracketed,
    // Enhanced paste (OSC 5522) is parsed at reception; state is transient.
}

/// The composer — input buffer + streaming message queue.
#[derive(Debug, Clone)]
pub struct Composer {
    /// Current editor buffer text (single-line prompt).
    pub buffer: String,
    /// Queue of messages placed during streaming.
    queue: VecDeque<Queued>,
    /// Paste state.
    paste: PasteState,
    /// Attachment counter — `[Image #N]` markers increment it.
    attach_seq: usize,
    /// Snapshot of the buffer before the last expansion (single-step undo).
    undo_prev: Option<String>,
}

/// Pastes longer than this many lines collapse to an inline preview.
pub const PASTE_INLINE_MAX_LINES: usize = 6;

impl Composer {
    /// Create a new, empty composer.
    pub fn new() -> Self {
        Composer {
            buffer: String::new(),
            queue: VecDeque::new(),
            paste: PasteState::None,
            attach_seq: 0,
            undo_prev: None,
        }
    }

    /// Insert a character into the buffer.
    ///
    /// Expansion hooks (OMP `emojiAutocomplete`):
    /// - a typed `:` that closes a known shortcode (`:name:`) replaces the
    ///   whole `:name:` with the glyph — unless the guards say otherwise
    ///   (word-adjacent colon like in `http://x:y:`, or code-like context);
    /// - a typed space/newline terminates a known emoticon at a word
    ///   boundary (`:-)` + space → glyph + the terminator kept);
    /// - either expansion takes a snapshot for `undo_last`.
    ///
    /// Caret note: the composer caret is at end-of-buffer, so after an
    /// expansion it sits directly after the inserted glyph — nothing else
    /// has to move.
    pub fn insert(&mut self, ch: char) {
        let prev = self.buffer.clone();
        let terminator = matches!(ch, ' ' | '\n' | '\r');
        let emoticon = if terminator {
            crate::emoji::try_expand_emoticon(&prev)
        } else {
            None
        };
        let shortcode = if ch == ':' {
            crate::emoji::try_expand_shortcode(&prev)
        } else {
            None
        };
        self.buffer.push(ch);
        if let Some((start, glyph)) = shortcode.or(emoticon) {
            let mut next = String::with_capacity(prev.len() + glyph.len() + 1);
            next.push_str(&prev[..start]);
            next.push_str(glyph);
            // Shortcode: the closing colon *is* the trigger — it is consumed
            // and replaced by the glyph. Emoticon: the terminator (space or
            // newline) is kept after the glyph.
            if shortcode.is_none() {
                next.push(ch);
            }
            self.buffer = next;
            self.undo_prev = Some(prev);
        }
    }

    /// Single-step undo: restore the exact pre-expansion buffer, or `false`
    /// when there is nothing to undo (the snapshot is consumed either way).
    pub fn undo_last(&mut self) -> bool {
        match self.undo_prev.take() {
            Some(prev) => {
                self.buffer = prev;
                true
            }
            None => false,
        }
    }

    /// Accept an emoji suggestion's glyph at the caret (end of buffer); the
    /// pre-insertion buffer is snapshotted for `undo_last`.
    pub fn accept_emoji(&mut self, glyph: &str) {
        self.undo_prev = Some(self.buffer.clone());
        self.buffer.push_str(glyph);
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.undo_prev = None;
    }

    /// Set the buffer content.
    pub fn set_buffer(&mut self, text: &str) {
        self.buffer = text.to_owned();
        self.undo_prev = None;
    }

    /// Queue a message during streaming (Steer or FollowUp).
    pub fn push_queue(&mut self, text: String, mode: QueueMode) {
        self.queue.push_back(Queued { text, mode });
    }

    /// Dequeue the last queued message and return it, or `None` if empty.
    pub fn dequeue_last(&mut self) -> Option<Queued> {
        self.queue.pop_back()
    }

    /// Number of queued messages.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    pub fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Peek the last queued message without removing it.
    pub fn queue_peek_last(&self) -> Option<&Queued> {
        self.queue.back()
    }

    /// Set paste state.
    pub fn set_paste(&mut self, state: PasteState) {
        self.paste = state;
    }

    /// Current paste state.
    pub fn paste(&self) -> PasteState {
        self.paste
    }

    /// Collapse a long multiline paste.
    ///
    /// - If the text is a single image path (`.png`, `.jpg`, `.jpeg`, `.gif`,
    ///   `.bmp`, `.ico`) → `Attachment` with an `[Image #N]` marker (the
    ///   attachment counter increments per image pasted).
    /// - If the text has ≤ `max_lines` lines → `Text` verbatim.
    /// - Otherwise → `Collapsed` with the first line and omitted count.
    /// Bracketed paste or OSC 5522 enhanced paste (`mime;base64` or full OSC).
    pub fn ingest_paste(&mut self, text: &str, max_lines: usize) -> PasteResult {
        if let Some(osc) = extract_osc5522(text).or_else(|| parse_osc5522(text.trim())) {
            return self.apply_osc5522(osc, max_lines);
        }
        self.collapse_paste(text, max_lines)
    }

    fn apply_osc5522(&mut self, osc: Osc5522, max_lines: usize) -> PasteResult {
        if osc.mime.starts_with("image/") {
            self.attach_seq += 1;
            return PasteResult::Attachment {
                name: osc.mime,
                marker: format!("[Image #{}]", self.attach_seq),
            };
        }
        let decoded = String::from_utf8_lossy(&osc.data).into_owned();
        self.collapse_paste(&decoded, max_lines)
    }

    pub fn collapse_paste(&mut self, text: &str, max_lines: usize) -> PasteResult {
        // Check for a single image path.
        let trimmed = text.trim();
        if is_image_path(trimmed) {
            self.attach_seq += 1;
            return PasteResult::Attachment {
                name: trimmed.to_owned(),
                marker: format!("[Image #{}]", self.attach_seq),
            };
        }

        let lines: Vec<&str> = text.lines().collect();
        if lines.len() <= max_lines {
            return PasteResult::Text(text.to_owned());
        }

        PasteResult::Collapsed {
            preview: lines[0].to_owned(),
            omitted_lines: lines.len() - max_lines,
        }
    }
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// OSC 5522 parser
// ---------------------------------------------------------------------------

/// Parse an OSC 5522 enhanced paste payload.
///
/// The payload is the segment between `\x1b]5522;` and the string terminator
/// (ST `\x1b\\` or BEL `\x07`).  Format: `<mime>;<base64-data>`.
///
/// Returns `None` when the payload is malformed or base64 cannot be decoded.
pub fn parse_osc5522(payload: &str) -> Option<Osc5522> {
    let (mime, b64) = payload.split_once(';')?;
    if mime.is_empty() || b64.is_empty() {
        return None;
    }
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    Some(Osc5522 {
        mime: mime.to_owned(),
        data,
    })
}

/// Pull an OSC 5522 payload out of a raw terminal sequence.
///
/// Looks for `ESC ] 5522 ; <mime>;<base64> ST/BEL`.
pub fn extract_osc5522(raw: &str) -> Option<Osc5522> {
    const PREFIX: &str = "\x1b]5522;";
    let start = raw.find(PREFIX)?;
    let rest = &raw[start + PREFIX.len()..];
    let end = rest.find("\x1b\\").or_else(|| rest.find('\u{07}'))?;
    parse_osc5522(&rest[..end])
}

/// Check whether `s` looks like a single image file path (no newlines, known
/// extension).
fn is_image_path(s: &str) -> bool {
    if s.contains('\n') {
        return false;
    }
    let ext = match s.rsplit_once('.') {
        Some((_, e)) => e.to_lowercase(),
        None => return false,
    };
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico")
}

/// Default box-composer padding (OMP `boxComposerStyle.defaultPaddingX`).
const BOX_PADDING_X: usize = 2;

/// Render the OMP **box** composer: status in the top `boxRound` border,
/// optional inner rows (slash complete), prompt merged into the bottom
/// border. Embed `CURSOR_MARKER` at the caret in `input`.
pub fn render_box_composer(
    theme: &Theme,
    width: u16,
    status: &str,
    input: &str,
    highlighted: bool,
    show_cursor: bool,
    inner_rows: &[String],
) -> Vec<String> {
    let w = width as usize;
    if w < 8 {
        let mut rows = inner_rows.to_vec();
        rows.push(prompt_text(input, highlighted, show_cursor));
        return rows;
    }
    let tl = glyph(theme, "boxRound.topLeft", "╭");
    let tr = glyph(theme, "boxRound.topRight", "╮");
    let bl = glyph(theme, "boxRound.bottomLeft", "╰");
    let br = glyph(theme, "boxRound.bottomRight", "╯");
    let h = glyph(theme, "boxRound.horizontal", "─");
    let v = glyph(theme, "boxRound.vertical", "│");
    let border = |s: &str| theme.fg(ThemeColor::Border, s);

    let pad_h = h.repeat(BOX_PADDING_X);
    let top_left = border(&format!("{tl}{pad_h}"));
    let top_right = border(&format!("{pad_h}{tr}"));
    let side = BOX_PADDING_X + 1;
    let fill_w = w.saturating_sub(side * 2);
    let status_trim = truncate_to_width(status, fill_w);
    let fill = fill_w.saturating_sub(visible_width(&status_trim));
    let top = format!(
        "{top_left}{status_trim}{}{top_right}",
        border(&h.repeat(fill))
    );

    let mut rows = vec![top];
    let inner_w = w.saturating_sub(2);
    for row in inner_rows {
        let body = truncate_to_width(row, inner_w.saturating_sub(BOX_PADDING_X));
        let pad = inner_w
            .saturating_sub(BOX_PADDING_X)
            .saturating_sub(visible_width(&body));
        let left = border(&format!("{v}{}", " ".repeat(BOX_PADDING_X)));
        let right = border(v);
        rows.push(format!("{left}{body}{}{right}", " ".repeat(pad)));
    }

    let prompt = prompt_text(input, highlighted, show_cursor);
    let left_pad = " ".repeat(BOX_PADDING_X.saturating_sub(1));
    let bottom_left = border(&format!("{bl}{h}{left_pad}"));
    let bottom_right = border(&format!("{h}{br}"));
    let used = visible_width(&bottom_left) + visible_width(&prompt) + visible_width(&bottom_right);
    let mid = w.saturating_sub(used);
    rows.push(format!(
        "{bottom_left}{prompt}{}{bottom_right}",
        border(&h.repeat(mid))
    ));
    rows
}

fn glyph<'a>(theme: &'a Theme, key: &str, fallback: &'a str) -> &'a str {
    let s = theme.symbol(key);
    if s.is_empty() {
        fallback
    } else {
        s
    }
}

fn prompt_text(input: &str, highlighted: bool, show_cursor: bool) -> String {
    let marker = if show_cursor {
        CURSOR_MARKER.to_string()
    } else {
        String::new()
    };
    let body = if highlighted {
        format!("\x1b[7m{input}\x1b[27m")
    } else {
        input.to_owned()
    };
    format!("{body}{marker}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Queue ------------------------------------------------------------

    #[test]
    fn queue_push_and_dequeue() {
        let mut c = Composer::new();
        c.push_queue("msg1".into(), QueueMode::Steer);
        c.push_queue("msg2".into(), QueueMode::FollowUp);
        assert_eq!(c.queue_len(), 2);

        let last = c.dequeue_last().unwrap();
        assert_eq!(last.text, "msg2");
        assert_eq!(last.mode, QueueMode::FollowUp);
        assert_eq!(c.queue_len(), 1);

        let first = c.dequeue_last().unwrap();
        assert_eq!(first.text, "msg1");
        assert_eq!(c.queue_len(), 0);
        assert!(c.dequeue_last().is_none());
    }

    #[test]
    fn dequeue_empty_returns_none() {
        let mut c = Composer::new();
        assert!(c.dequeue_last().is_none());
        assert!(c.queue_is_empty());
    }

    #[test]
    fn queue_peek_does_not_remove() {
        let mut c = Composer::new();
        c.push_queue("hello".into(), QueueMode::Steer);
        assert_eq!(c.queue_peek_last().unwrap().text, "hello");
        assert_eq!(c.queue_len(), 1, "peek does not remove");
    }

    // ---- Paste collapse ---------------------------------------------------

    #[test]
    fn short_paste_is_text() {
        let mut c = Composer::new();
        let result = c.collapse_paste("hello world", 5);
        assert_eq!(result, PasteResult::Text("hello world".into()));
    }

    #[test]
    fn long_paste_is_collapsed() {
        let long = "first line\nsecond\nthird\nfourth\nfifth\nsixth";
        let mut c = Composer::new();
        let result = c.collapse_paste(long, 3);
        match result {
            PasteResult::Collapsed {
                preview,
                omitted_lines,
            } => {
                assert_eq!(preview, "first line");
                assert_eq!(omitted_lines, 3, "6 lines - 3 max = 3 omitted");
            }
            other => panic!("expected Collapsed, got {other:?}"),
        }
    }

    #[test]
    fn image_path_returns_attachment() {
        let mut c = Composer::new();
        let result = c.collapse_paste("/path/to/photo.png", 5);
        match result {
            PasteResult::Attachment { name, marker } => {
                assert_eq!(name, "/path/to/photo.png");
                assert_eq!(marker, "[Image #1]");
            }
            other => panic!("expected Attachment, got {other:?}"),
        }
    }

    #[test]
    fn attachment_counter_increments_per_image() {
        let mut c = Composer::new();
        assert!(matches!(
            c.collapse_paste("a.png", 5),
            PasteResult::Attachment { ref marker, .. } if marker == "[Image #1]"
        ));
        assert!(matches!(
            c.collapse_paste("b.jpg", 5),
            PasteResult::Attachment { ref marker, .. } if marker == "[Image #2]"
        ));
        // Non-image pastes do not consume a number.
        assert!(matches!(c.collapse_paste("note", 5), PasteResult::Text(_)));
        assert!(matches!(
            c.collapse_paste("c.png", 5),
            PasteResult::Attachment { ref marker, .. } if marker == "[Image #3]"
        ));
    }

    #[test]
    fn jpeg_path_returns_attachment() {
        let mut c = Composer::new();
        let result = c.collapse_paste("image.JPEG", 5);
        assert!(matches!(result, PasteResult::Attachment { .. }));
    }

    #[test]
    fn non_image_path_is_not_attachment() {
        let mut c = Composer::new();
        let result = c.collapse_paste("/path/to/file.txt", 5);
        assert!(matches!(result, PasteResult::Text(_)));
    }

    #[test]
    fn multiline_image_path_is_not_attachment() {
        let mut c = Composer::new();
        let result = c.collapse_paste("photo.png\nmore", 5);
        assert!(matches!(result, PasteResult::Text(_)), "has newline");
    }

    // ---- OSC 5522 ---------------------------------------------------------

    #[test]
    fn parse_osc5522_valid() {
        let payload = "image/png;aGVsbG8="; // base64("hello")
        let parsed = parse_osc5522(payload).unwrap();
        assert_eq!(parsed.mime, "image/png");
        assert_eq!(parsed.data, b"hello");
    }

    #[test]
    fn extract_osc5522_from_sequence() {
        let raw = "\x1b]5522;image/png;aGVsbG8=\x07trailing";
        let parsed = extract_osc5522(raw).unwrap();
        assert_eq!(parsed.mime, "image/png");
        assert_eq!(parsed.data, b"hello");
    }

    #[test]
    fn ingest_osc5522_image_is_attachment() {
        let mut c = Composer::new();
        let raw = "\x1b]5522;image/png;aGVsbG8=\x1b\\";
        match c.ingest_paste(raw, 6) {
            PasteResult::Attachment { marker, .. } => assert_eq!(marker, "[Image #1]"),
            other => panic!("expected attachment, got {other:?}"),
        }
    }

    #[test]
    fn parse_osc5522_empty_mime() {
        assert!(parse_osc5522(";aGVsbG8=").is_none());
    }

    #[test]
    fn parse_osc5522_missing_semicolon() {
        assert!(parse_osc5522("justtext").is_none());
    }

    #[test]
    fn parse_osc5522_invalid_base64() {
        assert!(parse_osc5522("text/plain;!!!invalid!!!").is_none());
    }

    // ---- Buffer -----------------------------------------------------------

    #[test]
    fn composer_buffer_insert_clear() {
        let mut c = Composer::new();
        assert!(c.buffer.is_empty());
        c.insert('h');
        c.insert('i');
        assert_eq!(c.buffer, "hi");
        c.clear();
        assert!(c.buffer.is_empty());
    }

    #[test]
    fn composer_set_buffer() {
        let mut c = Composer::new();
        c.set_buffer("hello");
        assert_eq!(c.buffer, "hello");
    }

    #[test]
    fn box_composer_round_corners_and_cursor_marker() {
        use crate::theme::global;
        global().init("titanium");
        let theme = global().current().expect("theme");
        let rows = render_box_composer(&theme, 40, "π model", "hi", false, true, &[]);
        assert!(
            rows[0].contains('╭') && rows[0].contains('╮'),
            "top: {}",
            rows[0]
        );
        let last = rows.last().expect("bottom");
        assert!(last.contains('╰') && last.contains('╯'), "bottom: {last}");
        assert!(
            last.contains(CURSOR_MARKER),
            "cursor marker in prompt: {last}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("π model") || r.contains("model")),
            "{rows:?}"
        );
    }

    // ---- Emoji expansion ---------------------------------------------------

    use crate::cursor::extract_cursor;

    fn typed(s: &str) -> Composer {
        let mut c = Composer::new();
        for ch in s.chars() {
            c.insert(ch);
        }
        c
    }

    #[test]
    fn closing_colon_expands_known_shortcode() {
        assert_eq!(typed(":tada:").buffer, "🎉");
        assert_eq!(typed("hi :tada:").buffer, "hi 🎉");
    }

    #[test]
    fn unknown_shortcode_stays_literal() {
        let mut c = typed("hi :nope:");
        assert_eq!(c.buffer, "hi :nope:");
        // And it never panics or deletes text — a later colon does not eat it.
        c.insert(' ');
        assert_eq!(c.buffer, "hi :nope: ");
    }

    #[test]
    fn emoticon_expands_on_terminating_space() {
        assert_eq!(typed(":-) ").buffer, "🙂 ");
        assert_eq!(typed("<3 ").buffer, "❤️ ");
    }

    #[test]
    fn emoticon_never_expands_inside_code_fence() {
        let mut c = typed("``` :-)");
        c.insert(' ');
        assert_eq!(c.buffer, "``` :-) ");
    }

    #[test]
    fn shortcode_never_expands_inside_code_fence() {
        let mut c = typed("```\n:tada");
        c.insert(':');
        assert_eq!(c.buffer, "```\n:tada:");
    }

    #[test]
    fn url_colons_are_never_mangled() {
        let url = "http://x:y";
        let mut c = typed(url);
        c.insert(':');
        assert_eq!(c.buffer, "http://x:y:");
    }

    #[test]
    fn caret_sits_after_glyph_and_frame_widths_stay_exact() {
        use crate::theme::global;
        global().init("titanium");
        let theme = global().current().expect("theme");
        let c = typed("hi :tada:");
        assert_eq!(c.buffer, "hi 🎉");
        let rows = render_box_composer(&theme, 40, "", &c.buffer, false, true, &[]);
        let last = rows.last().expect("prompt row");
        // Caret: the glyph, then the cursor marker (caret is end-of-buffer).
        assert!(
            last.contains(&format!("hi 🎉{CURSOR_MARKER}")),
            "prompt: {last}"
        );
        // "hi 🎉" measures 3 + 2 (emoji = 2 cells), not 5 chars.
        let prompt = format!("hi 🎉");
        assert_eq!(crate::width::visible_width(&prompt), 5);
        for row in &rows {
            assert_eq!(crate::width::visible_width(row), 40, "row: {row}");
        }
    }

    #[test]
    fn undo_after_expansion_restores_text_exactly() {
        let mut c = typed("hi :tada:");
        assert_eq!(c.buffer, "hi 🎉");
        assert!(c.undo_last());
        assert_eq!(c.buffer, "hi :tada", "exact pre-expansion state");
        assert!(!c.undo_last(), "single-step only");
        assert_eq!(c.buffer, "hi :tada");
    }

    #[test]
    fn known_shortcode_expands_without_emoji_wiring() {
        // A plain set_buffer + insert path: expansion keys off the buffer
        // state, so midwords like `word:tada` stay literal.
        let mut c = typed("word:tada");
        assert_eq!(c.buffer, "word:tada");
        let _ = c;
    }

    #[test]
    fn picker_accept_inserts_glyph_at_caret() {
        use crate::emoji::EmojiPicker;
        let mut picker = EmojiPicker::default();
        picker.open("ta");
        let mut c = typed("ship it ");
        let glyph = picker.accept().expect("a `ta` match");
        c.accept_emoji(glyph);
        assert_eq!(c.buffer, "ship it 🎉");
        // Esc-equivalent: hiding must not touch the text.
        picker.hide();
        picker.hide();
        assert_eq!(c.buffer, "ship it 🎉");
    }
}
