//! Overlay panels for agent UX — selection lists (model, session) and
//! approval prompts with Esc cancel-without-delete.
//!
//! Each panel implements `Component` and can be shown via
//! `OverlayStack::show`.  The application layer checks `result()` after
//! routing input and calls `hide()` on the overlay handle when the panel
//! is closed.
//!
//! Contract: `docs/research/agent-ux/README.md`.

use crate::component::Component;
use crate::width::{truncate_to_width, visible_width};

/// OMP overlay chrome: `boxRound` corners + `boxSharp` tees (`omp://theme.md`).
fn box_top(inner_w: usize) -> String {
    format!("╭{}╮", "─".repeat(inner_w))
}
#[allow(dead_code)]
fn box_mid(inner_w: usize) -> String {
    format!("├{}┤", "─".repeat(inner_w))
}
/// The bottom rule of a box `inner_w` cells wide.
pub fn box_bot(inner_w: usize) -> String {
    format!("╰{}╯", "─".repeat(inner_w))
}

/// OMP `topBorder`: title inset into the top rule (`╭─ Title ────╮`).
///
/// Public because a host that draws its own panel chrome — the live chat's
/// pickers, whose input loop is its own — uses the same rule rather than
/// spelling the corners itself.
pub fn box_top_title(inner_w: usize, title: &str) -> String {
    if title.is_empty() {
        return box_top(inner_w);
    }
    let shown = truncate_to_width(&format!(" {title} "), inner_w.saturating_sub(1));
    let fill = inner_w
        .saturating_sub(1)
        .saturating_sub(visible_width(&shown));
    format!("╭─{shown}{}╮", "─".repeat(fill))
}

/// Result of a closed panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelResult<T> {
    /// The selected item, or `None` if cancelled.
    pub selected: Option<T>,
    /// Whether the panel was cancelled (Esc).
    pub cancelled: bool,
}

/// A generic selection-list panel.
///
/// Renders a bordered box with a title and scrollable list.  Navigation:
///
/// - `↑`/`↓` — move selection in the filtered list
/// - printable characters — type-to-filter (OMP fuzzy / subsequence)
/// - Backspace — delete the last filter character
/// - `Enter` — confirm the highlighted visible item
/// - `Esc` — cancel (no selection)
pub struct SelectionPanel<T> {
    title: String,
    items: Vec<T>,
    labels: Vec<String>,
    selected: usize,
    filter: String,
    /// When set, only this many filtered rows are painted (windowed around the highlight).
    max_visible: Option<usize>,
    result: Option<PanelResult<T>>,
    closed: bool,
}

impl<T> SelectionPanel<T> {
    /// Create a new selection panel.
    ///
    /// `items` — the data items; `labels` — their display strings.
    /// Each item at index `i` is displayed as `labels[i]`.
    ///
    /// # Panics
    ///
    /// Panics if `items` and `labels` have different lengths.
    pub fn new(title: &str, items: Vec<T>, labels: Vec<String>) -> Self {
        assert_eq!(
            items.len(),
            labels.len(),
            "items and labels must be same length"
        );
        SelectionPanel {
            title: title.to_owned(),
            items,
            labels,
            selected: 0,
            filter: String::new(),
            max_visible: None,
            result: None,
            closed: false,
        }
    }

    /// The result once the panel is closed, or `None` while still open.
    pub fn result(&self) -> Option<&PanelResult<T>> {
        self.result.as_ref()
    }

    /// Whether the panel has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Consume the panel and return its result, or `None` if not yet closed.
    pub fn into_result(self) -> Option<PanelResult<T>> {
        self.result
    }

    /// Selected index (0-based) into the **visible** (filtered) list.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Current type-to-filter query.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Number of labels that match the current filter.
    pub fn visible_count(&self) -> usize {
        self.visible_indices().len()
    }

    /// Cap painted item rows (OMP compact overlay / `SelectList.setMaxVisible`).
    pub fn set_max_visible(&mut self, rows: usize) {
        self.max_visible = Some(rows.max(1));
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.labels
            .iter()
            .enumerate()
            .filter(|(_, label)| fuzzy_match(label, &self.filter))
            .map(|(i, _)| i)
            .collect()
    }

    fn windowed_indices(&self) -> Vec<usize> {
        let vis = self.visible_indices();
        let Some(cap) = self.max_visible else {
            return vis;
        };
        if vis.len() <= cap {
            return vis;
        }
        let cap = cap.max(1);
        let mut start = self.selected.saturating_sub(cap / 2);
        if start + cap > vis.len() {
            start = vis.len() - cap;
        }
        vis[start..start + cap].to_vec()
    }

    fn clamp_selected(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        let n = self.visible_indices().len();
        if n > 0 && self.selected + 1 < n {
            self.selected += 1;
        }
    }

    fn confirm(&mut self) {
        let indices = self.visible_indices();
        let Some(&orig) = indices.get(self.selected) else {
            return;
        };
        if orig >= self.items.len() {
            return;
        }
        let item = self.items.swap_remove(orig);
        self.result = Some(PanelResult {
            selected: Some(item),
            cancelled: false,
        });
        self.closed = true;
    }

    fn cancel(&mut self) {
        self.result = Some(PanelResult {
            selected: None,
            cancelled: true,
        });
        self.closed = true;
    }
}

impl<T> Component for SelectionPanel<T> {
    fn render(&mut self, width: u16) -> Vec<String> {
        if self.closed {
            return Vec::new();
        }

        let w = width as usize;
        let visible = self.visible_indices();
        let window = self.windowed_indices();
        if w < 8 {
            return window
                .iter()
                .filter_map(|&orig| {
                    let vis_i = visible.iter().position(|&i| i == orig)?;
                    let label = self.labels.get(orig)?;
                    Some(if vis_i == self.selected {
                        format!("> {label}")
                    } else {
                        format!("  {label}")
                    })
                })
                .collect();
        }

        let inner_w = w.saturating_sub(4).max(6);
        let title = if self.filter.is_empty() {
            self.title.clone()
        } else {
            format!("{}  {}", self.title, self.filter)
        };

        let mut rows = Vec::new();
        rows.push(box_top_title(inner_w, &title));

        for &orig in &window {
            let Some(label) = self.labels.get(orig) else {
                continue;
            };
            let Some(vis_i) = visible.iter().position(|&i| i == orig) else {
                continue;
            };
            let truncated = truncate_to_width(label, inner_w.saturating_sub(2));
            let pad = inner_w.saturating_sub(2) - visible_width(&truncated);
            let marker = if vis_i == self.selected { "▶" } else { " " };
            rows.push(format!("│ {marker}{truncated}{} │", " ".repeat(pad)));
        }

        rows.push(box_bot(inner_w));
        rows
    }

    fn handle_input(&mut self, data: &str) {
        if self.closed {
            return;
        }
        match data {
            "\x1b" | "\x1b\x1b" => self.cancel(),
            "\x1b[A" => self.move_up(),
            "\x1b[B" => self.move_down(),
            "\r" | "\n" => self.confirm(),
            "\x7f" | "\x08" => {
                self.filter.pop();
                self.selected = 0;
                self.clamp_selected();
            }
            other => {
                let mut chars = other.chars();
                if let Some(ch) = chars.next()
                    && chars.next().is_none()
                    && !ch.is_control()
                {
                    self.filter.push(ch);
                    self.selected = 0;
                    self.clamp_selected();
                }
            }
        }
    }

    fn wants_key_release(&self) -> bool {
        false
    }
}

/// Case-insensitive substring, then in-order subsequence (OMP type-to-filter).
fn fuzzy_match(label: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let label_lc: String = label.to_lowercase();
    let query_lc: String = query.to_lowercase();
    if label_lc.contains(&query_lc) {
        return true;
    }
    let mut chars = label_lc.chars();
    for q in query_lc.chars() {
        loop {
            match chars.next() {
                Some(c) if c == q => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Approval panel  (Yes / No / Cancel)
// ---------------------------------------------------------------------------

/// A three-choice approval panel: Yes, No, Cancel.
pub type ApprovalPanel = SelectionPanel<&'static str>;

impl ApprovalPanel {
    /// Create a new approval panel with the given prompt.
    pub fn prompt(title: &str) -> Self {
        SelectionPanel::new(
            title,
            vec!["Yes", "No", "Cancel"],
            vec!["Yes".into(), "No".into(), "Cancel".into()],
        )
    }
}

// ---------------------------------------------------------------------------
// Session switcher  (Ctrl+X)
// ---------------------------------------------------------------------------

/// Outcome of closing the session switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    /// Switch to the session at the given index (Enter).
    Switch(usize),
    /// Close the session at the given index (Ctrl+D).
    Close(usize),
    /// Create a new session (Ctrl+N).
    New,
    /// Reload the list (Ctrl+R) — the panel stays open.
    Refresh,
    /// Cancelled (Esc) — no action, no deletion.
    Cancel,
}

/// Live session switcher.
///
/// Hermes-style: `↑`/`↓` move, `Enter` switches, `Ctrl+D` closes the
/// selected session, `Ctrl+N` creates a new one, `Ctrl+R` refreshes,
/// `Esc` cancels without deleting.
pub struct SessionSwitcher {
    titles: Vec<String>,
    selected: usize,
    max_visible: Option<usize>,
    action: Option<SessionAction>,
    closed: bool,
}

impl SessionSwitcher {
    /// Create a switcher over the given session titles (one per session).
    pub fn new(titles: Vec<String>) -> Self {
        SessionSwitcher {
            titles,
            selected: 0,
            max_visible: None,
            action: None,
            closed: false,
        }
    }

    /// The chosen action once closed, or `None` while still open.
    pub fn action(&self) -> Option<&SessionAction> {
        self.action.as_ref()
    }

    /// Consume the panel and return its action, or `None` if not yet closed.
    pub fn into_action(self) -> Option<SessionAction> {
        self.action
    }

    /// Whether the switcher has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The session titles the switcher was built over.
    pub fn titles(&self) -> &[String] {
        &self.titles
    }

    /// Cap painted session rows (compact overlay).
    pub fn set_max_visible(&mut self, rows: usize) {
        self.max_visible = Some(rows.max(1));
    }

    fn close_with(&mut self, action: SessionAction) {
        self.action = Some(action);
        self.closed = true;
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if !self.titles.is_empty() && self.selected + 1 < self.titles.len() {
            self.selected += 1;
        }
    }
}

impl Component for SessionSwitcher {
    fn render(&mut self, width: u16) -> Vec<String> {
        if self.closed {
            return Vec::new();
        }
        let w = width as usize;
        if w < 8 {
            return self
                .titles
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    if i == self.selected {
                        format!("> {t}")
                    } else {
                        format!("  {t}")
                    }
                })
                .collect();
        }

        let inner_w = w.saturating_sub(4).max(6);
        let mut rows = Vec::new();
        rows.push(box_top_title(inner_w, "Sessions"));

        let cap = self.max_visible.unwrap_or(self.titles.len()).max(1);
        let n = self.titles.len();
        let (start, end) = if n <= cap {
            (0, n)
        } else {
            let mut start = self.selected.saturating_sub(cap / 2);
            if start + cap > n {
                start = n - cap;
            }
            (start, start + cap)
        };
        for i in start..end {
            let Some(title) = self.titles.get(i) else {
                continue;
            };
            let truncated = truncate_to_width(title, inner_w.saturating_sub(2));
            let pad = inner_w.saturating_sub(2) - visible_width(&truncated);
            let marker = if i == self.selected { "▶" } else { " " };
            rows.push(format!("│ {marker}{truncated}{} │", " ".repeat(pad)));
        }

        rows.push(box_bot(inner_w));
        rows
    }

    fn handle_input(&mut self, data: &str) {
        if self.closed {
            return;
        }
        match data {
            "\x1b" | "\x1b\x1b" => self.close_with(SessionAction::Cancel), // Esc
            "\x1b[A" | "k" => self.move_up(),                              // Up / k
            "\x1b[B" | "j" => self.move_down(),                            // Down / j
            "\r" | "\n" => {
                if !self.titles.is_empty() {
                    self.close_with(SessionAction::Switch(self.selected));
                }
            }
            "\x04" => {
                // Ctrl+D — close selected session
                if !self.titles.is_empty() {
                    self.close_with(SessionAction::Close(self.selected));
                }
            }
            "\x0e" => self.close_with(SessionAction::New), // Ctrl+N
            "\x12" => self.action = Some(SessionAction::Refresh), // Ctrl+R, keep open
            _ => {}
        }
    }

    fn wants_key_release(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SelectionPanel navigation ----------------------------------------

    #[test]
    fn panel_initial_state() {
        let p = SelectionPanel::new(
            "Choose",
            vec!["a", "b", "c"],
            vec!["Item A".into(), "Item B".into(), "Item C".into()],
        );
        assert!(!p.is_closed());
        assert!(p.result().is_none());
        assert_eq!(p.selected_index(), 0);
    }

    #[test]
    fn panel_arrow_navigation() {
        let mut p = SelectionPanel::new(
            "Choose",
            vec!["a", "b", "c"],
            vec!["A".into(), "B".into(), "C".into()],
        );
        assert_eq!(p.selected, 0);
        p.handle_input("\x1b[B"); // Down
        assert_eq!(p.selected, 1);
        p.handle_input("\x1b[B"); // Down
        assert_eq!(p.selected, 2);
        p.handle_input("\x1b[B"); // Down (at bottom, no-op)
        assert_eq!(p.selected, 2);
        p.handle_input("\x1b[A"); // Up
        assert_eq!(p.selected, 1);
        p.handle_input("\x1b[A"); // Up
        assert_eq!(p.selected, 0);
        p.handle_input("\x1b[A"); // Up (at top, no-op)
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn panel_enter_confirms() {
        let mut p = SelectionPanel::new(
            "Choose",
            vec!["apple", "banana", "cherry"],
            vec!["Apple".into(), "Banana".into(), "Cherry".into()],
        );
        p.handle_input("\x1b[B"); // move to banana
        p.handle_input("\x1b[B"); // move to cherry
        p.handle_input("\r"); // Enter
        assert!(p.is_closed());
        let res = p.result().unwrap();
        assert!(!res.cancelled);
        assert_eq!(res.selected, Some("cherry"));
    }

    #[test]
    fn panel_esc_cancels() {
        let mut p = SelectionPanel::new("Choose", vec!["a", "b"], vec!["A".into(), "B".into()]);
        p.handle_input("\x1b"); // Esc
        assert!(p.is_closed());
        let res = p.result().unwrap();
        assert!(res.cancelled);
        assert_eq!(res.selected, None);
    }

    #[test]
    fn panel_type_to_filter_confirms_match() {
        let mut p = SelectionPanel::new(
            "Choose",
            vec!["apple", "banana", "cherry"],
            vec!["Apple".into(), "Banana".into(), "Cherry".into()],
        );
        p.handle_input("b");
        p.handle_input("a");
        assert_eq!(p.filter(), "ba");
        assert_eq!(p.visible_count(), 1);
        p.handle_input("\r");
        assert!(p.is_closed());
        assert_eq!(p.result().unwrap().selected, Some("banana"));
    }

    #[test]
    fn panel_fuzzy_subsequence_matches() {
        let mut p = SelectionPanel::new(
            "Model",
            vec!["opencode-go/glm-5.3-flash", "clinepass/deepseek-v4-flash"],
            vec![
                "opencode-go/glm-5.3-flash".into(),
                "clinepass/deepseek-v4-flash".into(),
            ],
        );
        p.handle_input("g");
        p.handle_input("l");
        p.handle_input("m");
        assert_eq!(p.visible_count(), 1);
        p.handle_input("\r");
        assert_eq!(
            p.result().unwrap().selected,
            Some("opencode-go/glm-5.3-flash")
        );
    }

    #[test]
    fn panel_backspace_edits_filter() {
        let mut p = SelectionPanel::new("X", vec!["aa", "ab"], vec!["aa".into(), "ab".into()]);
        p.handle_input("b");
        assert_eq!(p.visible_count(), 1);
        p.handle_input("\x7f");
        assert_eq!(p.filter(), "");
        assert_eq!(p.visible_count(), 2);
    }

    #[test]
    fn panel_windows_list_keeps_title() {
        let items: Vec<String> = (0..20).map(|i| format!("m{i}")).collect();
        let labels = items.clone();
        let mut p = SelectionPanel::new("Model", items, labels);
        p.set_max_visible(5);
        let rows = p.render(40);
        assert!(
            rows[0].contains("Model"),
            "title stays in top border: {rows:?}"
        );
        assert!(rows.len() <= 7, "chrome + 5 items: {}", rows.len());
        p.handle_input("\x1b[B");
        p.handle_input("\x1b[B");
        p.handle_input("\x1b[B");
        p.handle_input("\x1b[B");
        p.handle_input("\x1b[B");
        let rows = p.render(40);
        assert!(rows[0].contains("Model"), "title after scroll: {rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("m5")),
            "window follows highlight: {rows:?}"
        );
    }

    #[test]
    fn panel_into_result() {
        let mut p = SelectionPanel::new("X", vec!["only"], vec!["Only".into()]);
        p.handle_input("\r");
        let res = p.into_result().unwrap();
        assert_eq!(res.selected, Some("only"));
    }

    // ---- ApprovalPanel ----------------------------------------------------

    #[test]
    fn approval_panel_yes() {
        let mut p = ApprovalPanel::prompt("Proceed?");
        p.handle_input("\r"); // Yes is default (index 0)
        assert!(p.is_closed());
        let res = p.result().unwrap();
        assert_eq!(res.selected, Some("Yes"));
    }

    #[test]
    fn approval_panel_no() {
        let mut p = ApprovalPanel::prompt("Proceed?");
        p.handle_input("\x1b[B"); // No
        p.handle_input("\r");
        let res = p.result().unwrap();
        assert_eq!(res.selected, Some("No"));
    }

    #[test]
    fn approval_panel_cancel() {
        let mut p = ApprovalPanel::prompt("Proceed?");
        p.handle_input("\x1b"); // Esc
        assert!(p.result().unwrap().cancelled);
    }

    // ---- Render -----------------------------------------------------------

    #[test]
    fn render_closed_panel_is_empty() {
        let mut p = SelectionPanel::<&str>::new("X", vec![], vec![]);
        p.handle_input("\x1b");
        assert!(p.render(80).is_empty());
    }

    #[test]
    fn render_narrow_panel_uses_simple_format() {
        let mut p = SelectionPanel::new("X", vec!["a", "b"], vec!["A".into(), "B".into()]);
        let rows = p.render(4);
        assert!(rows[0].contains("> A"), "narrow render: {rows:?}");
        assert!(rows[1].contains("  B"), "narrow render: {rows:?}");
    }

    #[test]
    fn render_wide_panel_has_border() {
        let mut p = SelectionPanel::new("Choose", vec!["x"], vec!["Item".into()]);
        let rows = p.render(40);
        assert!(
            rows[0].starts_with('╭'),
            "should start with top border: {rows:?}"
        );
        assert!(
            rows.last().unwrap().starts_with('╰'),
            "should end with bottom border"
        );
    }

    // ---- SessionSwitcher --------------------------------------------------

    #[test]
    fn switcher_enter_switches() {
        let mut s = SessionSwitcher::new(vec!["A".into(), "B".into()]);
        s.handle_input("\x1b[B"); // move to B
        s.handle_input("\r"); // Enter
        assert_eq!(s.action(), Some(&SessionAction::Switch(1)));
    }

    #[test]
    fn switcher_ctrl_d_closes() {
        let mut s = SessionSwitcher::new(vec!["A".into(), "B".into(), "C".into()]);
        s.handle_input("\x1b[B"); // move to index 1
        s.handle_input("\x04"); // Ctrl+D
        assert_eq!(s.action(), Some(&SessionAction::Close(1)));
    }

    #[test]
    fn switcher_ctrl_n_new() {
        let mut s = SessionSwitcher::new(vec!["A".into()]);
        s.handle_input("\x0e"); // Ctrl+N
        assert_eq!(s.action(), Some(&SessionAction::New));
    }

    #[test]
    fn switcher_ctrl_r_refreshes_keeps_open() {
        let mut s = SessionSwitcher::new(vec!["A".into(), "B".into()]);
        s.handle_input("\x12"); // Ctrl+R
        assert_eq!(s.action(), Some(&SessionAction::Refresh));
        assert!(!s.is_closed(), "refresh must keep the switcher open");
    }

    #[test]
    fn switcher_esc_cancels_without_delete() {
        let mut s = SessionSwitcher::new(vec!["A".into(), "B".into()]);
        s.handle_input("\x1b"); // Esc
        assert_eq!(s.action(), Some(&SessionAction::Cancel));
        assert!(s.is_closed());
    }

    #[test]
    fn switcher_empty_list_enter_is_noop() {
        let mut s = SessionSwitcher::new(vec![]);
        s.handle_input("\r");
        assert!(!s.is_closed(), "Enter on empty list must not close");
    }

    #[test]
    fn switcher_into_action() {
        let mut s = SessionSwitcher::new(vec!["Only".into()]);
        s.handle_input("\x0e"); // Ctrl+N
        assert_eq!(s.into_action(), Some(SessionAction::New));
    }
}
