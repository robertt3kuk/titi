//! The composer's one row and its caption.
//!
//! The composer is the box under the transcript: a border, a caption on its
//! bottom rule, and the draft — or what the screen is asking for instead of a
//! draft (an approval, a key, a device code). It reads the chat's state and
//! writes nothing, so every frame draws it from the same fields the keys write.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use titi_tui::theme::{Theme, ThemeColor};

use crate::chat::{Chat, LoginMethod, fg, surface};
use crate::pickers::ellipsis_label;

/// The colours of the composer's border and its caption, one pair per state.
///
/// A state that means something keeps its own colour: needs-you is the warning
/// token, a running turn the accent. `statusLine.sessionAccent` colours the
/// *idle* border, which is the one omp's key names (its "editor border"), and
/// it never overrides a state — running and needs-you say something the accent
/// does not.
pub(crate) fn composer_colors(chat: &Chat) -> (ThemeColor, ThemeColor) {
    if chat.approval.is_some() || chat.pending_ask.is_some() || chat.login_for.is_some() {
        (ThemeColor::Warning, ThemeColor::Warning)
    } else if chat.turn_active {
        (ThemeColor::Accent, ThemeColor::Accent)
    } else if chat.status_line.session_accent {
        (ThemeColor::Accent, ThemeColor::Dim)
    } else {
        (ThemeColor::Border, ThemeColor::Dim)
    }
}

pub(crate) fn composer(chat: &Chat, width: u16, theme: &Theme) -> Paragraph<'static> {
    let (border, caption_color) = composer_colors(chat);
    // The mode the draft is in, when the vim keys are on, in the composer's
    // own border. `NORMAL` is the mode that swallows typing, so it is the one
    // the border shouts about; `INSERT` is the composer the screen always had.
    let chip = chat.vim_mode();
    let room = (width as usize)
        .saturating_sub(4)
        .saturating_sub(chip.map_or(0, |mode| mode.label().len() + 3));
    let caption = titi_tui::width::truncate_to_width(&composer_caption(chat), room);
    let title = match chip {
        Some(mode) => {
            let color = if mode == crate::vim::VimMode::Normal {
                ThemeColor::Accent
            } else {
                caption_color
            };
            Line::from(vec![
                Span::styled(format!(" {} ", mode.label()), fg(theme, color)),
                Span::styled(format!("· {caption} "), fg(theme, caption_color)),
            ])
        }
        None => Line::from(Span::styled(
            format!(" {caption} "),
            fg(theme, caption_color),
        )),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(theme, border))
        .title_bottom(title.centered())
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
        let placeholder = if let Some(ask) = &chat.pending_ask {
            // The row below is the answer field while a question waits: it
            // says what to do with it, and with a list up that is picking.
            if ask.answering() {
                "your answer…"
            } else {
                "pick above, or type your own"
            }
        } else if chat.paused {
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
        // The caret's own row: the draft windowed so the caret is visible, and
        // the caret drawn where it is rather than at the end.
        let room = inner.saturating_sub(4).max(1);
        let row = caret_row(&chat.input, chat.caret(), room);
        Line::from(vec![
            Span::styled("› ", fg(theme, ThemeColor::Accent)),
            Span::styled(row.before, fg(theme, ThemeColor::Text)),
            Span::styled("▍", fg(theme, ThemeColor::Accent)),
            Span::styled(row.after, fg(theme, ThemeColor::Text)),
        ])
    };
    Paragraph::new(line).block(block)
}

fn composer_caption(chat: &Chat) -> String {
    let ctx = match chat.context_percent {
        Some(percent) => format!("{percent}%  ·  "),
        None => String::new(),
    };
    let keys = if let Some(ask) = &chat.pending_ask {
        if ask.answering() {
            "enter sends  ·  esc cancels"
        } else if ask.multi {
            "↑↓ move  ·  space ticks  ·  enter sends  ·  esc cancels"
        } else if ask.free_text {
            "↑↓ move  ·  enter picks  ·  esc cancels  ·  type your own"
        } else {
            "↑↓ move  ·  enter picks  ·  esc cancels"
        }
    } else if chat.approval.is_some() {
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

/// The draft's row around the caret: what is drawn before the caret and what
/// is drawn after it, the pair the composer puts its caret glyph between.
pub(crate) struct CaretRow {
    pub(crate) before: String,
    pub(crate) after: String,
}

/// Windows the draft to `room` cells around the caret.
///
/// The caret is what the row is for, so it is what the window keeps in view: a
/// few cells of what follows it are held open when there is any (so the
/// character being typed into is visible too), the window never runs past
/// either end of the draft, and a draft that fits is drawn whole.
fn caret_row(input: &str, caret: usize, room: usize) -> CaretRow {
    let caret = caret.min(input.len());
    let head = composer_view(&input[..caret]);
    let tail = composer_view(&input[caret..]);
    let caret_col = titi_tui::width::visible_width(&head);
    let total = caret_col + titi_tui::width::visible_width(&tail);
    if total <= room {
        return CaretRow {
            before: head,
            after: tail,
        };
    }
    // Keep a few cells of the tail visible, so the window shows where the
    // caret is writing as well as what it is writing into.
    let tail_room = 4.min(room / 2);
    let start = caret_col
        .saturating_sub(room.saturating_sub(tail_room))
        .min(total.saturating_sub(room));
    CaretRow {
        before: titi_tui::width::slice_by_column(&head, start, caret_col),
        after: titi_tui::width::slice_by_column(&tail, 0, start + room - caret_col),
    }
}

/// The composer's one row: a pasted line break is shown as `↵` and a tab as
/// four spaces, so a multi-line paste reads as what it is without the box
/// growing. The input itself keeps both.
fn composer_view(input: &str) -> String {
    input.replace('\n', "↵").replace('\t', "    ")
}

pub(crate) fn fit_tail(text: &str, width: usize) -> String {
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

pub(crate) fn tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_owned()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}
