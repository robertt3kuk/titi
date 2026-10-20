//! The action block: one assistant round's tool calls as a work log.
//!
//! A call is a [`TranscriptLine`] of kind `Tool` that carries an
//! [`ActionCall`]. It is pushed when the engine says the call started and
//! settled in place when the engine says it finished, so one call is one line
//! for its whole life — what it was, whether it is still running, how it ended
//! and how long it took. The diff an edit reports stays its own `Diff` line,
//! right under the call.
//!
//! [`group_rows`] turns a run of those lines into the block the screen draws:
//!
//! ```text
//!     3 actions ▾
//!       ✓ Read     README.md              12 lines   0.0s
//!       ✓ Edited   src/main.rs            +4 −1      0.1s
//!       ✓ Ran      cargo test             exit 0     4.2s
//! ```
//!
//! Three or more finished calls of one verb in a row fold into a single
//! `Read ×5` row. A call that failed, is still running or has a diff under it
//! never folds: the point of the fold is to say "and more of the same", and
//! those are the rows that are not.

use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use titi_tui::markdown::SectionMode;
use titi_tui::theme::{Theme, ThemeColor};
use titi_tui::width::{truncate_to_width, visible_width};

use crate::chat::{Chat, TOOL_PREVIEW, fg, page, spinner_frame};
use crate::composer::one_line;
use crate::transcript::{
    ACTION_INDENT, BODY_MARGIN, DIFF_MAX_ROWS, LineKind, MESSAGE_INDENT, TranscriptLine, sgr_row,
};

/// Calls of one verb in a row that fold into a single row.
pub(crate) const FOLD_AT: usize = 3;

/// The narrowest verb column: short verbs keep the target clear of the glyph.
const VERB_MIN: usize = 6;

/// The widest verb column a block spends on its rows (`Searched web ×12`).
const VERB_MAX: usize = 16;

/// Columns the longest result may keep when the target wants the room.
const RESULT_FLOOR: usize = 8;

/// A row narrower than this has no room for a result next to its target.
const RESULT_MIN_ROOM: usize = 24;

/// One tool call as the screen has watched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionCall {
    pub(crate) call_id: String,
    pub(crate) tool: String,
    /// What the call acts on — the path, command or pattern its description
    /// names, without the tool's own name in front.
    pub(crate) target: String,
    started: Instant,
    /// Set while the engine waits for a person to approve the call: that wait
    /// is theirs, not the tool's, so it stays out of the duration.
    waiting_since: Option<Instant>,
    waited: Duration,
    outcome: Option<Outcome>,
}

/// How a call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub(crate) ok: bool,
    /// The one-line summary of what came back.
    pub(crate) result: String,
    pub(crate) elapsed: Duration,
}

impl ActionCall {
    pub(crate) fn start(call_id: &str, tool: &str, detail: Option<&str>, now: Instant) -> Self {
        Self {
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            target: target_of(tool, detail),
            started: now,
            waiting_since: None,
            waited: Duration::ZERO,
            outcome: None,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    pub(crate) fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    /// The call's run time at `now`, or its final one once it has finished.
    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        if let Some(outcome) = &self.outcome {
            return outcome.elapsed;
        }
        let waiting = self
            .waiting_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        now.saturating_duration_since(self.started)
            .saturating_sub(self.waited)
            .saturating_sub(waiting)
    }

    pub(crate) fn wait_for_approval(&mut self, now: Instant) {
        if self.outcome.is_none() {
            self.waiting_since.get_or_insert(now);
        }
    }

    pub(crate) fn settle_approval(&mut self, now: Instant) {
        if let Some(since) = self.waiting_since.take() {
            self.waited += now.saturating_duration_since(since);
        }
    }

    pub(crate) fn finish(&mut self, now: Instant, ok: bool, output: &str, diff: Option<&str>) {
        self.settle_approval(now);
        let elapsed = self.elapsed(now);
        self.outcome = Some(Outcome {
            ok,
            result: result_summary(&self.tool, ok, output, diff),
            elapsed,
        });
    }

    /// The verb the call's row leads with.
    pub(crate) fn verb(&self) -> &str {
        verb(&self.tool)
    }
}

/// The verb a tool's row leads with. A tool the table does not know keeps its
/// own name, which is never wrong.
pub(crate) fn verb(tool: &str) -> &str {
    match tool {
        "read" => "Read",
        "glob" => "Found",
        "grep" => "Searched",
        "bash" => "Ran",
        "edit" | "hashline_edit" => "Edited",
        "write" => "Wrote",
        "fetch" => "Fetched",
        "web_search" => "Searched web",
        "todo" => "Planned",
        "agent" => "Delegated",
        "ask" => "Asked",
        other => other,
    }
}

/// The target of a call: the tool's description of it, less the tool's own name
/// that `describe()` puts in front (`read src/main.rs` → `src/main.rs`).
fn target_of(tool: &str, detail: Option<&str>) -> String {
    let Some(detail) = detail.map(str::trim).filter(|detail| !detail.is_empty()) else {
        return String::new();
    };
    // `hashline_edit` describes itself as an `edit`.
    let lead = if tool == "hashline_edit" {
        "edit"
    } else {
        tool
    };
    let rest = detail
        .strip_prefix(lead)
        .and_then(|rest| rest.strip_prefix(' '))
        .unwrap_or(detail);
    one_line(rest.trim(), TOOL_PREVIEW)
}

/// One dim line about what a finished call brought back: a count for the tools
/// whose answer is a listing, `exit N` for a command, the diff's size for a
/// change, and the answer's own first line for anything else.
pub(crate) fn result_summary(tool: &str, ok: bool, output: &str, diff: Option<&str>) -> String {
    let first = || {
        output
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| one_line(line, TOOL_PREVIEW))
            .unwrap_or_default()
    };
    if !ok {
        return first();
    }
    match tool {
        "read" => {
            let body = match output.strip_prefix("[lines ") {
                Some(rest) => rest.split_once('\n').map_or("", |(_, body)| body),
                None => output,
            };
            counted(body.lines().count(), "line", "lines")
        }
        "glob" => counted(listed(output, "more files"), "file", "files"),
        "grep" => match listed(output, "more matches") {
            0 => "no matches".to_owned(),
            count => counted(count, "match", "matches"),
        },
        "bash" => "exit 0".to_owned(),
        "edit" | "hashline_edit" | "write" => match diff.map(diff_size) {
            Some((added, removed)) if added + removed > 0 => format!("+{added} \u{2212}{removed}"),
            _ => first(),
        },
        _ => first(),
    }
}

fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Entries a capped listing holds: its lines, plus the `… N more files` the
/// tool appends for the ones past its cap.
fn listed(output: &str, more: &str) -> usize {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let hidden = line
                .strip_prefix("… ")
                .and_then(|rest| rest.strip_suffix(more))
                .and_then(|count| count.trim().parse::<usize>().ok());
            hidden.unwrap_or(1)
        })
        .sum()
}

/// Lines a unified diff adds and removes, headers left out.
fn diff_size(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

/// A duration as a row ends with it: `0.3s`, `12s`, `1m05s`.
pub(crate) fn duration_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

/// `text` cut to `room` cells, with an ellipsis where it was cut.
pub(crate) fn fit(text: &str, room: usize) -> String {
    if visible_width(text) <= room {
        return text.to_owned();
    }
    if room == 0 {
        return String::new();
    }
    format!("{}…", truncate_to_width(text, room - 1))
}

/// A call and the diff lines under it.
struct Entry<'a> {
    call: &'a ActionCall,
    diffs: Vec<&'a str>,
}

/// A row of the block: one call, or a fold of several.
enum Item<'a> {
    One(Entry<'a>),
    Folded(Vec<&'a ActionCall>),
}

/// A run of `Tool` call lines and the `Diff` lines under them, as entries. A
/// diff with no call before it has nothing to hang under and is dropped; the
/// caller only hands over runs that start with a call.
fn entries(lines: &[TranscriptLine]) -> Vec<Entry<'_>> {
    let mut entries: Vec<Entry<'_>> = Vec::new();
    for line in lines {
        match (&line.call, line.kind) {
            (Some(call), _) => entries.push(Entry {
                call,
                diffs: Vec::new(),
            }),
            (None, LineKind::Diff) => {
                if let Some(last) = entries.last_mut() {
                    last.diffs.push(&line.text);
                }
            }
            (None, _) => {}
        }
    }
    entries
}

/// Whether a call may disappear into a fold: it finished well and has no diff
/// to keep visible.
fn foldable(entry: &Entry<'_>) -> bool {
    entry.diffs.is_empty() && entry.call.outcome().is_some_and(|outcome| outcome.ok)
}

/// Folds runs of [`FOLD_AT`] or more foldable calls of one verb.
fn fold(entries: Vec<Entry<'_>>) -> Vec<Item<'_>> {
    let mut runs = Vec::new();
    let mut at = 0;
    while at < entries.len() {
        let mut end = at + 1;
        if foldable(&entries[at]) {
            while end < entries.len()
                && foldable(&entries[end])
                && entries[end].call.verb() == entries[at].call.verb()
            {
                end += 1;
            }
        }
        runs.push(end - at);
        at = end;
    }
    let mut entries = entries.into_iter();
    let mut items = Vec::new();
    for run in runs {
        if run >= FOLD_AT {
            items.push(Item::Folded(
                entries.by_ref().take(run).map(|entry| entry.call).collect(),
            ));
        } else {
            items.extend(entries.by_ref().take(run).map(Item::One));
        }
    }
    items
}

/// What one row says, before it is laid out.
struct RowParts {
    glyph: (String, ThemeColor),
    verb: String,
    target: String,
    result: String,
    result_color: ThemeColor,
    duration: Option<String>,
}

fn parts_of(item: &Item<'_>, now: Instant) -> RowParts {
    match item {
        Item::One(entry) => {
            let call = entry.call;
            let (glyph, result, result_color) = match call.outcome() {
                None if call.waiting_since.is_some() => (
                    (
                        spinner_frame(now.saturating_duration_since(call.started)).to_owned(),
                        ThemeColor::Warning,
                    ),
                    "awaiting approval".to_owned(),
                    ThemeColor::Warning,
                ),
                None => (
                    (
                        spinner_frame(now.saturating_duration_since(call.started)).to_owned(),
                        ThemeColor::Warning,
                    ),
                    String::new(),
                    ThemeColor::Dim,
                ),
                Some(outcome) if outcome.ok => (
                    ("✓".to_owned(), ThemeColor::Success),
                    outcome.result.clone(),
                    ThemeColor::Dim,
                ),
                Some(outcome) => (
                    ("✕".to_owned(), ThemeColor::Error),
                    outcome.result.clone(),
                    ThemeColor::Error,
                ),
            };
            RowParts {
                glyph,
                verb: call.verb().to_owned(),
                target: call.target.clone(),
                result,
                result_color,
                duration: Some(duration_label(call.elapsed(now))),
            }
        }
        Item::Folded(calls) => {
            let mut targets: Vec<&str> = Vec::new();
            for call in calls.iter().rev() {
                if !call.target.is_empty() && !targets.contains(&call.target.as_str()) {
                    targets.push(&call.target);
                }
            }
            let total: Duration = calls.iter().map(|call| call.elapsed(now)).sum();
            RowParts {
                glyph: ("✓".to_owned(), ThemeColor::Success),
                verb: format!("{} ×{}", calls[0].verb(), calls.len()),
                target: targets.join(", "),
                result: String::new(),
                result_color: ThemeColor::Dim,
                duration: Some(duration_label(total)),
            }
        }
    }
}

/// One row, laid out for `width`: the glyph, the verb in a column of its own,
/// the target and its dim result, and the duration against the right edge.
///
/// A row never wraps. When it is too wide the result gives way first — down to
/// a third of the room — and then the target, each cut with an ellipsis.
fn row(parts: &RowParts, verb_width: usize, theme: &Theme, width: usize) -> Line<'static> {
    let total = width.saturating_sub(BODY_MARGIN);
    let fixed = ACTION_INDENT + 2 + verb_width + 1;
    let duration = parts.duration.as_deref().unwrap_or("");
    let reserved = if duration.is_empty() {
        0
    } else {
        visible_width(duration) + 2
    };
    let room = total.saturating_sub(fixed + reserved);
    let target = one_line(&parts.target, TOOL_PREVIEW);
    let result = one_line(&parts.result, TOOL_PREVIEW);
    let (target_width, result_width) = (visible_width(&target), visible_width(&result));

    let mut result_room = if result.is_empty() || room < RESULT_MIN_ROOM {
        0
    } else {
        result_width.min((room / 3).max(RESULT_FLOOR))
    };
    let target_room = room.saturating_sub(if result_room > 0 { result_room + 2 } else { 0 });
    if result_room > 0 && target_width <= target_room {
        result_room = result_width.min(room - target_width - 2);
    }
    let target = fit(&target, if result_room > 0 { target_room } else { room });
    let result = fit(&result, result_room);

    let mut spans = vec![
        Span::styled(" ".repeat(ACTION_INDENT), page(theme)),
        Span::styled(parts.glyph.0.clone(), fg(theme, parts.glyph.1)),
        Span::styled(" ", page(theme)),
        Span::styled(
            format!("{:<verb_width$}", parts.verb),
            fg(theme, ThemeColor::Text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", page(theme)),
    ];
    let mut used = visible_width(&target);
    spans.push(Span::styled(target, fg(theme, ThemeColor::Muted)));
    if !result.is_empty() {
        used += 2 + visible_width(&result);
        spans.push(Span::styled("  ", page(theme)));
        spans.push(Span::styled(result, fg(theme, parts.result_color)));
    }
    if !duration.is_empty() {
        let gap = room.saturating_sub(used) + 2;
        spans.push(Span::styled(" ".repeat(gap), page(theme)));
        spans.push(Span::styled(
            duration.to_owned(),
            fg(theme, ThemeColor::Dim),
        ));
    }
    Line::from(spans)
}

/// The block's first row: how many calls it holds and whether they are drawn.
fn summary_row(count: usize, mode: SectionMode, theme: &Theme) -> Line<'static> {
    let chevron = if mode == SectionMode::Expanded {
        "▾"
    } else {
        "▸"
    };
    let noun = if count == 1 { "action" } else { "actions" };
    Line::from(vec![
        Span::styled(" ".repeat(MESSAGE_INDENT), page(theme)),
        Span::styled(
            format!("{count} {noun} {chevron}"),
            fg(theme, ThemeColor::Dim),
        ),
    ])
}

/// The rows under a call's own: its diff, or the checklist a tool reported
/// that is not one.
fn detail_rows(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let inset = ACTION_INDENT + 2;
    let pad = || Span::styled(" ".repeat(inset), page(theme));
    let note =
        |text: String| Line::from(vec![pad(), Span::styled(text, fg(theme, ThemeColor::Dim))]);
    let room = width.saturating_sub(inset).max(8);
    let Some(diff) = titi_tui::diff::render_diff(text, theme, room as u16) else {
        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len().min(DIFF_MAX_ROWS);
        let mut rows: Vec<Line<'static>> = lines[..shown]
            .iter()
            .map(|line| {
                Line::from(vec![
                    pad(),
                    Span::styled(one_line(line, TOOL_PREVIEW), fg(theme, ThemeColor::Muted)),
                ])
            })
            .collect();
        if lines.len() > shown {
            rows.push(note(format!("… {} more lines", lines.len() - shown)));
        }
        return rows;
    };
    let shown = diff.rows.len().min(DIFF_MAX_ROWS);
    let mut rows: Vec<Line<'static>> = diff.rows[..shown]
        .iter()
        .map(|row| {
            let mut spans = vec![pad()];
            spans.extend(sgr_row(row));
            Line::from(spans)
        })
        .collect();
    if diff.rows.len() > shown {
        rows.push(note(format!(
            "… {} more diff lines",
            diff.rows.len() - shown
        )));
    }
    rows
}

/// A run of call lines (and their diffs) as the block the screen draws.
///
/// `Expanded` draws the summary and every row; `Collapsed` draws the summary
/// alone, which is how `/details tools collapsed` reaches the block. `Hidden` is
/// the caller's: it draws nothing at all.
pub(crate) fn group_rows(
    lines: &[TranscriptLine],
    mode: SectionMode,
    now: Instant,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let entries = entries(lines);
    let mut rows = vec![summary_row(entries.len(), mode, theme)];
    if mode != SectionMode::Expanded {
        return rows;
    }
    let items = fold(entries);
    let parts: Vec<RowParts> = items.iter().map(|item| parts_of(item, now)).collect();
    let verb_width = parts
        .iter()
        .map(|parts| visible_width(&parts.verb))
        .max()
        .unwrap_or(0)
        .clamp(VERB_MIN, VERB_MAX);
    for (item, parts) in items.iter().zip(&parts) {
        rows.push(row(parts, verb_width, theme, width));
        if let Item::One(entry) = item {
            for diff in &entry.diffs {
                rows.extend(detail_rows(diff, theme, width));
            }
        }
    }
    rows
}

/// One call line on its own — the row without the block around it, for a
/// caller that renders a line at a time.
pub(crate) fn call_row(
    line: &TranscriptLine,
    now: Instant,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut rows = group_rows(
        std::slice::from_ref(line),
        SectionMode::Expanded,
        now,
        theme,
        width,
    );
    rows.remove(0);
    rows
}

/// The style of the `↯` row a steering message is shown as.
pub(crate) fn steer_style(theme: &Theme) -> Style {
    fg(theme, ThemeColor::Accent).add_modifier(Modifier::DIM)
}

impl Chat {
    /// A call starts: it joins the transcript as a running row.
    pub(crate) fn begin_action(&mut self, call_id: &str, tool: &str, detail: Option<&str>) {
        let shown = detail.filter(|detail| !detail.is_empty()).unwrap_or(tool);
        let mut line =
            TranscriptLine::new(LineKind::Tool, format!("tool {shown}")).stamped(&self.model);
        line.call = Some(ActionCall::start(call_id, tool, detail, Instant::now()));
        self.push_line(line);
    }

    /// The engine is asking a person about a call: its clock stops.
    pub(crate) fn await_approval(&mut self, call_id: &str) {
        if let Some(call) = self.running_call(call_id) {
            call.wait_for_approval(Instant::now());
        }
    }

    /// A call ends: its row settles in place, and the diff it reported goes
    /// right under it. `false` when no running call has that id — a result the
    /// screen never saw start — and the caller shows it the plain way.
    pub(crate) fn finish_action(
        &mut self,
        call_id: &str,
        output: &str,
        is_error: bool,
        diff: Option<&str>,
    ) -> bool {
        let Some(at) = self.lines.iter().rposition(|line| {
            line.call
                .as_ref()
                .is_some_and(|call| call.call_id == call_id && call.is_running())
        }) else {
            return false;
        };
        let line = &mut self.lines[at];
        let Some(call) = line.call.as_mut() else {
            return false;
        };
        call.finish(Instant::now(), !is_error, output, diff);
        let Some(outcome) = call.outcome() else {
            return false;
        };
        // The text is what a surface with no rows reads (the cast replay), so
        // it says the same thing the row does.
        let status = if outcome.ok { "done" } else { "error" };
        let mut text = format!("tool {status}  {}", call.tool);
        for part in [call.target.as_str(), outcome.result.as_str()] {
            if !part.is_empty() {
                text.push_str("  ");
                text.push_str(part);
            }
        }
        line.text = text;
        if let Some(diff) = diff {
            let under = TranscriptLine::new(LineKind::Diff, diff.to_owned());
            self.insert_line(at + 1, under);
        }
        self.scroll_offset = 0;
        true
    }

    fn running_call(&mut self, call_id: &str) -> Option<&mut ActionCall> {
        self.lines
            .iter_mut()
            .rev()
            .filter_map(|line| line.call.as_mut())
            .find(|call| call.call_id == call_id && call.is_running())
    }

    /// Ends the wait of every call whose approval is no longer pending, so the
    /// time a person took to say yes is not the tool's. Frame-accurate: the
    /// transcript asks once per frame.
    pub(crate) fn settle_approvals(&mut self, now: Instant) {
        let pending = self
            .approval
            .as_ref()
            .map(|approval| approval.call_id.as_str());
        for line in &mut self.lines {
            if let Some(call) = line.call.as_mut()
                && call.waiting_since.is_some()
                && Some(call.call_id.as_str()) != pending
            {
                call.settle_approval(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        };
        titi_tui::theme::loader::load_theme("titanium", &options).expect("built-in theme")
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn texts(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(text).collect()
    }

    /// A finished call that took `millis` ms.
    fn call(tool: &str, detail: &str, ok: bool, output: &str, millis: u64) -> TranscriptLine {
        let start = Instant::now();
        let mut call = ActionCall::start("c", tool, Some(detail), start);
        call.finish(start + Duration::from_millis(millis), ok, output, None);
        let mut line = TranscriptLine::new(LineKind::Tool, format!("tool {detail}"));
        line.call = Some(call);
        line
    }

    fn running(tool: &str, detail: &str) -> (TranscriptLine, Instant) {
        let start = Instant::now();
        let mut line = TranscriptLine::new(LineKind::Tool, format!("tool {detail}"));
        line.call = Some(ActionCall::start("c", tool, Some(detail), start));
        (line, start)
    }

    fn block(lines: &[TranscriptLine], mode: SectionMode, width: usize) -> Vec<String> {
        texts(&group_rows(lines, mode, Instant::now(), &theme(), width))
    }

    #[test]
    fn every_tool_has_the_verb_its_row_leads_with() {
        for (tool, expected) in [
            ("read", "Read"),
            ("glob", "Found"),
            ("grep", "Searched"),
            ("bash", "Ran"),
            ("edit", "Edited"),
            ("hashline_edit", "Edited"),
            ("write", "Wrote"),
            ("fetch", "Fetched"),
            ("web_search", "Searched web"),
            ("todo", "Planned"),
            ("agent", "Delegated"),
            ("ask", "Asked"),
            ("settings", "settings"),
            ("git_commit", "git_commit"),
        ] {
            assert_eq!(verb(tool), expected, "{tool}");
        }
    }

    #[test]
    fn the_target_is_the_description_less_the_tool_word() {
        let target = |tool: &str, detail: &str| target_of(tool, Some(detail));
        assert_eq!(target("read", "read src/main.rs:1-20"), "src/main.rs:1-20");
        assert_eq!(
            target("bash", "bash cargo test -p titi-cli"),
            "cargo test -p titi-cli"
        );
        assert_eq!(target("hashline_edit", "edit a.rs"), "a.rs");
        assert_eq!(target("web_search", "web_search \"rust\""), "\"rust\"");
        assert_eq!(target("ask", "Which database?"), "Which database?");
        assert_eq!(
            target("agent", "Worker: do the thing"),
            "Worker: do the thing"
        );
        // A tool that is a prefix of another word keeps its description whole.
        assert_eq!(target("read", "reader x"), "reader x");
        assert_eq!(target_of("read", None), "");
        assert_eq!(target_of("read", Some("  ")), "");
    }

    #[test]
    fn the_result_summary_counts_what_came_back() {
        let sum = |tool: &str, ok: bool, out: &str, diff: Option<&str>| {
            result_summary(tool, ok, out, diff)
        };
        assert_eq!(sum("read", true, "a\nb\nc\n", None), "3 lines");
        assert_eq!(sum("read", true, "a\n", None), "1 line");
        assert_eq!(
            sum("read", true, "[lines 10-12 of 99]\nx\ny\nz\n", None),
            "3 lines",
            "the window's own header is not a line of the file"
        );
        assert_eq!(sum("glob", true, "a.rs\nb.rs\nc.rs", None), "3 files");
        assert_eq!(
            sum("glob", true, "a.rs\n… 7 more files", None),
            "8 files",
            "a capped listing counts the files it left out"
        );
        assert_eq!(sum("glob", true, "", None), "0 files");
        assert_eq!(sum("grep", true, "a:1:x\nb:2:y", None), "2 matches");
        assert_eq!(sum("grep", true, "", None), "no matches");
        assert_eq!(sum("bash", true, "lots\nof\noutput", None), "exit 0");
        assert_eq!(sum("bash", false, "exit 2\nboom", None), "exit 2");
        assert_eq!(
            sum(
                "edit",
                true,
                "edited",
                Some("--- a/f\n+++ b/f\n@@ -1,2 +1,5 @@\n-old\n+a\n+b\n+c\n+d\n ctx\n")
            ),
            "+4 \u{2212}1"
        );
        assert_eq!(sum("write", true, "wrote out.txt", None), "wrote out.txt");
        assert_eq!(
            sum("fetch", true, "\n\nfirst line\nsecond", None),
            "first line"
        );
        assert_eq!(
            sum("read", false, "src/x.rs is outside the workspace", None),
            "src/x.rs is outside the workspace"
        );
    }

    #[test]
    fn durations_read_as_seconds_up_to_a_minute() {
        let label = |millis: u64| duration_label(Duration::from_millis(millis));
        assert_eq!(label(0), "0.0s");
        assert_eq!(label(340), "0.3s");
        assert_eq!(label(9_940), "9.9s");
        assert_eq!(label(12_400), "12s");
        assert_eq!(label(65_000), "1m05s");
    }

    #[test]
    fn a_block_opens_with_how_many_actions_it_holds() {
        let lines = vec![
            call("read", "read README.md", true, "a\nb\n", 300),
            call("bash", "bash echo hi", true, "hi", 100),
        ];
        let rows = block(&lines, SectionMode::Expanded, 100);
        assert_eq!(rows[0], "    2 actions ▾");
        assert_eq!(rows.len(), 3, "{rows:#?}");
        assert!(rows[1].starts_with("      ✓ Read "), "{rows:#?}");
        assert!(rows[1].contains("README.md"), "{rows:#?}");
        assert!(rows[1].contains("2 lines"), "{rows:#?}");
        assert!(rows[1].ends_with("0.3s"), "{rows:#?}");
        assert!(rows[2].starts_with("      ✓ Ran  "), "{rows:#?}");
        assert!(rows[2].contains("exit 0"), "{rows:#?}");

        let one = block(&lines[..1], SectionMode::Expanded, 100);
        assert_eq!(one[0], "    1 action ▾");
    }

    #[test]
    fn a_collapsed_block_is_its_summary() {
        let lines = vec![
            call("read", "read a", true, "x", 10),
            call("read", "read b", true, "x", 10),
        ];
        assert_eq!(
            block(&lines, SectionMode::Collapsed, 100),
            vec!["    2 actions ▸".to_owned()]
        );
    }

    /// The duration is flush with the right edge on every row of the block,
    /// at a phone-width pane and at a wide one, whatever the row held.
    #[test]
    fn durations_align_at_narrow_and_wide_widths() {
        let lines = vec![
            call(
                "read",
                "read src/a/very/long/path/to/some/module/file.rs",
                true,
                "a\n",
                300,
            ),
            call(
                "bash",
                "bash cargo test --workspace --locked -- --nocapture",
                true,
                "ok",
                12_400,
            ),
            call(
                "grep",
                "grep needle",
                false,
                "pattern is not a valid regex: unclosed group",
                70,
            ),
            call("glob", "glob **/*.rs", true, "a\nb", 2_000),
        ];
        for width in [60usize, 80, 120, 200] {
            let rows = block(&lines, SectionMode::Expanded, width);
            for row in &rows[1..] {
                assert_eq!(
                    visible_width(row),
                    width - BODY_MARGIN,
                    "{width}: the row does not end at the right edge: {row:?}"
                );
            }
            assert!(rows[1].ends_with("0.3s"), "{width}: {rows:#?}");
            assert!(rows[2].ends_with("12s"), "{width}: {rows:#?}");
            assert!(rows[3].ends_with("0.1s"), "{width}: {rows:#?}");
            assert!(
                rows[3].contains('✕'),
                "{width}: a failure has its own glyph: {rows:#?}"
            );
        }
        // Narrow, the target is cut with an ellipsis rather than pushing the
        // duration off the row.
        let narrow = block(&lines, SectionMode::Expanded, 60);
        assert!(narrow[1].contains('…'), "{narrow:#?}");
        // Wide, nothing is cut.
        let wide = block(&lines, SectionMode::Expanded, 200);
        assert!(
            !wide[1].contains('…') && !wide[2].contains('…'),
            "{wide:#?}"
        );
        assert!(wide[2].contains("cargo test --workspace --locked -- --nocapture"));
    }

    #[test]
    fn a_running_call_shows_a_spinner_and_its_clock() {
        let (line, start) = running("bash", "bash sleep 3");
        let rows = texts(&group_rows(
            std::slice::from_ref(&line),
            SectionMode::Expanded,
            start + Duration::from_millis(1_500),
            &theme(),
            80,
        ));
        assert!(rows[1].contains("Ran"), "{rows:#?}");
        assert!(rows[1].contains("sleep 3"), "{rows:#?}");
        assert!(rows[1].ends_with("1.5s"), "{rows:#?}");
        assert!(
            crate::chat::SPINNER
                .iter()
                .any(|frame| rows[1].contains(frame)),
            "{rows:#?}"
        );
    }

    /// The time a person took to answer an approval is not the tool's.
    #[test]
    fn the_wait_for_an_approval_is_not_the_calls_duration() {
        let start = Instant::now();
        let mut call = ActionCall::start("c", "bash", Some("bash echo hi"), start);
        call.wait_for_approval(start + Duration::from_millis(100));
        assert_eq!(
            call.elapsed(start + Duration::from_secs(30)),
            Duration::from_millis(100),
            "the clock stands still while the engine waits for a person"
        );
        call.settle_approval(start + Duration::from_secs(30));
        call.finish(start + Duration::from_millis(30_250), true, "hi", None);
        assert_eq!(
            call.outcome().map(|outcome| outcome.elapsed),
            Some(Duration::from_millis(350))
        );
    }

    #[test]
    fn three_finished_calls_of_one_verb_fold_into_one_row() {
        let reads: Vec<TranscriptLine> = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]
            .iter()
            .map(|file| call("read", &format!("read {file}"), true, "x\n", 100))
            .collect();
        let rows = block(&reads, SectionMode::Expanded, 100);
        assert_eq!(
            rows[0], "    5 actions ▾",
            "the summary still counts every call"
        );
        assert_eq!(rows.len(), 2, "{rows:#?}");
        assert!(rows[1].contains("Read ×5"), "{rows:#?}");
        let (newest, oldest) = (rows[1].find("e.rs"), rows[1].find("a.rs"));
        assert!(
            newest.is_some() && newest < oldest,
            "the newest target comes first: {rows:#?}"
        );
        assert!(
            rows[1].ends_with("0.5s"),
            "the row carries the calls' total: {rows:#?}"
        );

        // The target list is cut with an ellipsis, newest kept.
        let narrow = block(&reads, SectionMode::Expanded, 40);
        assert!(narrow[1].contains("e.rs"), "{narrow:#?}");
        assert!(narrow[1].contains('…'), "{narrow:#?}");
        assert!(!narrow[1].contains("a.rs"), "{narrow:#?}");
    }

    #[test]
    fn two_calls_do_not_fold_and_a_different_verb_breaks_a_run() {
        let two = vec![
            call("read", "read a", true, "x", 1),
            call("read", "read b", true, "x", 1),
        ];
        assert_eq!(block(&two, SectionMode::Expanded, 80).len(), 3);

        let broken = vec![
            call("read", "read a", true, "x", 1),
            call("read", "read b", true, "x", 1),
            call("bash", "bash ls", true, "x", 1),
            call("read", "read c", true, "x", 1),
            call("read", "read d", true, "x", 1),
        ];
        let rows = block(&broken, SectionMode::Expanded, 80);
        assert_eq!(rows.len(), 6, "no verb has three in a row: {rows:#?}");
        assert!(!rows.iter().any(|row| row.contains('×')), "{rows:#?}");
    }

    /// A failure is the row a reader must not miss, so it never folds: it
    /// stands on its own and ends the run it interrupts.
    #[test]
    fn a_failure_never_folds() {
        let calls = vec![
            call("read", "read a", true, "x", 1),
            call("read", "read b", true, "x", 1),
            call("read", "read missing", false, "missing is not there", 1),
            call("read", "read c", true, "x", 1),
            call("read", "read d", true, "x", 1),
        ];
        let rows = block(&calls, SectionMode::Expanded, 80);
        assert_eq!(rows.len(), 6, "{rows:#?}");
        assert!(
            rows[3].contains('✕') && rows[3].contains("missing is not there"),
            "{rows:#?}"
        );
        assert!(!rows.iter().any(|row| row.contains('×')), "{rows:#?}");

        // Three good calls and a failure: the three fold, the failure stays.
        let calls = vec![
            call("read", "read a", true, "x", 1),
            call("read", "read b", true, "x", 1),
            call("read", "read c", true, "x", 1),
            call("read", "read missing", false, "missing is not there", 1),
        ];
        let rows = block(&calls, SectionMode::Expanded, 80);
        assert_eq!(rows.len(), 3, "{rows:#?}");
        assert!(rows[1].contains("Read ×3"), "{rows:#?}");
        assert!(rows[2].contains('✕'), "{rows:#?}");
    }

    #[test]
    fn a_running_call_never_folds() {
        let mut calls: Vec<TranscriptLine> = (0..3)
            .map(|n| call("read", &format!("read f{n}"), true, "x", 1))
            .collect();
        calls.push(running("read", "read live").0);
        let rows = block(&calls, SectionMode::Expanded, 80);
        assert!(rows[1].contains("Read ×3"), "{rows:#?}");
        assert!(
            rows[2].contains("read live") || rows[2].contains("live"),
            "{rows:#?}"
        );
    }

    /// An edit's diff stays visible under its own row, so an edit is never
    /// folded away from it.
    #[test]
    fn a_diff_stays_under_its_row_and_keeps_it_unfolded() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old_line\n+new_line\n ctx\n";
        let mut lines = Vec::new();
        for name in ["a.rs", "b.rs", "c.rs"] {
            let start = Instant::now();
            let mut edit = ActionCall::start("e", "edit", Some(&format!("edit {name}")), start);
            edit.finish(
                start + Duration::from_millis(50),
                true,
                "edited",
                Some(diff),
            );
            let mut line = TranscriptLine::new(LineKind::Tool, String::new());
            line.call = Some(edit);
            lines.push(line);
            lines.push(TranscriptLine::new(LineKind::Diff, diff.to_owned()));
        }
        let rows = block(&lines, SectionMode::Expanded, 100);
        assert_eq!(rows[0], "    3 actions ▾");
        assert!(!rows.iter().any(|row| row.contains('×')), "{rows:#?}");
        assert!(
            rows[1].contains("Edited") && rows[1].contains("+1 \u{2212}1"),
            "{rows:#?}"
        );
        assert!(
            rows[2].starts_with(&format!("{:1$}@@", "", ACTION_INDENT + 2)),
            "{rows:#?}"
        );
        assert!(rows[3].contains("old_line"), "{rows:#?}");
        assert!(rows[4].contains("new_line"), "{rows:#?}");
        assert!(rows[5].contains("ctx"), "{rows:#?}");
        assert!(
            rows[6].contains("Edited"),
            "the next call's row follows its diff: {rows:#?}"
        );
    }
}
