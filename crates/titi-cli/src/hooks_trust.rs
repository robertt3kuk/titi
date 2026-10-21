//! The start-up panel that asks whether to run a repository's hooks.
//!
//! Shown before the first prompt when the project defines hooks the trust
//! store does not hold: `this repo defines 2 hooks · trust · once · no`.
//! `trust` remembers them until their text changes, `once` runs them for this
//! session only, `no` skips them. Nothing but a key press here enables a
//! repository's hook, and any other key is ignored: a prompt that a stray
//! Enter or a typed letter dismisses would not be a consent.

use std::time::Instant;

use titi_engine::EngineCommand;

use crate::chat::{Applied, Chat, Key};
use crate::composer::one_line;
use crate::hooks_setup::{Choice, HookStartup, describe};
use crate::pickers::{PanelLine, PanelView, panel_body, panel_label_room, panel_view};
use crate::transcript::LineKind;

/// The three answers, in the order the panel lists them. The letter is the
/// key that picks the row without moving to it.
const ROWS: [(Choice, char, &str, &str); 3] = [
    (
        Choice::Trust,
        't',
        "trust",
        "run them, and remember until their text changes",
    ),
    (Choice::Once, 'o', "once", "run them this session only"),
    (Choice::No, 'n', "no", "skip them"),
];

/// Most hooks listed above the rows before the rest are counted.
const LISTED: usize = 6;

/// The question, and what it was asked about.
#[derive(Debug)]
pub(crate) struct TrustPanel {
    start: HookStartup,
    selected: usize,
}

impl TrustPanel {
    /// The cursor starts on `no`: Enter pressed without looking enables
    /// nothing.
    pub(crate) fn new(start: HookStartup) -> Self {
        Self {
            start,
            selected: ROWS.len() - 1,
        }
    }
}

impl Chat {
    /// A key while the panel is up. Arrows move, Enter takes the row, `t` `o`
    /// `n` take one outright, Esc is `no`; ctrl+c and ctrl+d decline and then
    /// do what they always do. Every other key is ignored.
    pub(crate) fn hook_trust_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => self.move_hook_trust(-1),
            Key::Down | Key::Tab => self.move_hook_trust(1),
            Key::Enter => {
                let selected = self.hook_trust.as_ref().map_or(0, |panel| panel.selected);
                return self.answer_hook_trust(ROWS[selected % ROWS.len()].0);
            }
            Key::Char(letter) => {
                if let Some((choice, ..)) = ROWS
                    .iter()
                    .find(|(_, hotkey, ..)| hotkey.eq_ignore_ascii_case(&letter))
                {
                    return self.answer_hook_trust(*choice);
                }
            }
            Key::Esc => return self.answer_hook_trust(Choice::No),
            Key::CtrlC | Key::CtrlD => {
                self.answer_hook_trust(Choice::No);
                return self.on_key(key, now);
            }
            _ => {}
        }
        Applied::none()
    }

    fn move_hook_trust(&mut self, delta: isize) {
        if let Some(panel) = self.hook_trust.as_mut() {
            panel.selected =
                (panel.selected as isize + delta).rem_euclid(ROWS.len() as isize) as usize;
        }
    }

    /// Applies the answer: records it, tells the transcript what happened,
    /// and hands the engine the hooks it may now run.
    fn answer_hook_trust(&mut self, choice: Choice) -> Applied {
        let Some(mut panel) = self.hook_trust.take() else {
            return Applied::none();
        };
        let count = panel.start.pending.len();
        let hooks = if count == 1 { "hook" } else { "hooks" };
        match panel.start.decide(choice) {
            Ok(Some(all)) => {
                let what = match choice {
                    Choice::Trust => {
                        format!("trusted {count} project {hooks} (until their text changes)")
                    }
                    _ => format!("running {count} project {hooks} this session"),
                };
                self.push(LineKind::Note, format!("hooks: {what}"));
                Applied::send(EngineCommand::SetHooks { hooks: all }, None)
            }
            Ok(None) => {
                self.push(
                    LineKind::Note,
                    format!("hooks: skipped {count} project {hooks}"),
                );
                Applied::none()
            }
            Err(error) => {
                self.push(
                    LineKind::Error,
                    format!("hooks: not enabled — the trust could not be recorded: {error}"),
                );
                Applied::none()
            }
        }
    }
}

/// The panel: the hooks the repository defines, then the three answers.
pub(crate) fn trust_panel(chat: &Chat, total: u16, width: u16) -> PanelView {
    let Some(panel) = chat.hook_trust.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let pending = &panel.start.pending;
    let room = panel_label_room(width).saturating_sub(3);
    let mut lines: Vec<PanelLine> = pending
        .iter()
        .take(LISTED)
        .map(|def| {
            PanelLine::Heading(one_line(
                &format!("{}  [{}]", describe(def), def.source),
                room,
            ))
        })
        .collect();
    if pending.len() > LISTED {
        lines.push(PanelLine::Heading(format!(
            "… and {} more",
            pending.len() - LISTED
        )));
    }
    let first_row = lines.len();
    lines.extend(ROWS.iter().map(|(_, _, name, about)| PanelLine::Row {
        text: format!("{name}   {about}"),
        accent: false,
    }));
    let title = format!(
        "this repo defines {} {} · trust · once · no",
        pending.len(),
        if pending.len() == 1 { "hook" } else { "hooks" }
    );
    panel_view(
        Some(title),
        lines,
        Some(first_row + panel.selected),
        panel_body(total),
    )
}

/// The dim transcript row for a hook that ran: `hook PreToolUse · lint.sh ·
/// blocked: no rm -rf`. A hook that ran and had nothing to say draws no row;
/// the engine still reports it, for a headless consumer that audits them.
pub(crate) fn hook_row(
    event: &str,
    hook: &str,
    outcome: titi_engine::protocol::HookOutcome,
    detail: &str,
) -> Option<String> {
    use titi_engine::protocol::HookOutcome as Outcome;
    let word = match outcome {
        Outcome::Ok => return None,
        Outcome::Context => "added context",
        Outcome::Blocked => "blocked",
        Outcome::Denied => "denied",
        Outcome::Asked => "asks",
        Outcome::Warning => "warning",
        Outcome::TimedOut => "timed out",
        // A variant a newer engine adds is still a hook that ran.
        #[allow(unreachable_patterns)]
        _ => "ran",
    };
    let detail = detail.trim();
    Some(if detail.is_empty() {
        format!("hook {event} · {hook} · {word}")
    } else {
        format!("hook {event} · {hook} · {word}: {detail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatEffect;
    use crate::pickers::panel_view_for;
    use std::sync::Arc;
    use titi_engine::protocol::HookOutcome;
    use titi_tui::theme::Theme;

    fn theme() -> Arc<Theme> {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        };
        match titi_tui::theme::loader::load_theme("titanium", &options) {
            Ok(theme) => Arc::new(theme),
            Err(reason) => panic!("built-in theme: {reason}"),
        }
    }

    const REPO: &str = r#"{"hooks": {
        "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "./lint.sh"}]}],
        "Stop": [{"hooks": [{"type": "command", "command": "./done.sh"}]}]}}"#;

    struct Screen {
        agent: tempfile::TempDir,
        repo: tempfile::TempDir,
        chat: Chat,
    }

    /// A screen with the panel up over a repo defining two hooks.
    fn screen() -> Screen {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
        std::fs::write(repo.path().join(".claude/settings.json"), REPO).unwrap();
        let start = HookStartup::resolve(agent.path(), repo.path(), None, false);
        assert_eq!(start.pending.len(), 2);
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme());
        chat.set_agent_dir(agent.path());
        chat.hook_trust = Some(TrustPanel::new(start));
        Screen { agent, repo, chat }
    }

    fn press(screen: &mut Screen, key: Key) -> Applied {
        screen.chat.on_key(key, Instant::now())
    }

    fn pending_after(screen: &Screen) -> usize {
        HookStartup::resolve(screen.agent.path(), screen.repo.path(), None, false)
            .pending
            .len()
    }

    #[test]
    fn the_panel_says_how_many_hooks_the_repo_defines() {
        let screen = screen();
        let view = panel_view_for(&screen.chat, 40, 100).expect("the panel is up");
        assert_eq!(
            view.title.as_deref(),
            Some("this repo defines 2 hooks · trust · once · no")
        );
        assert!(screen.chat.panel_open());
    }

    #[test]
    fn a_stray_key_or_enter_enables_nothing() {
        let mut screen = screen();
        press(&mut screen, Key::Char('x'));
        press(&mut screen, Key::Char('?'));
        assert!(screen.chat.hook_trust.is_some(), "other keys leave it up");
        assert!(screen.chat.input.is_empty(), "and never reach the draft");
        // The cursor starts on `no`.
        let applied = press(&mut screen, Key::Enter);
        assert!(applied.effect.is_none());
        assert!(screen.chat.hook_trust.is_none());
        assert_eq!(pending_after(&screen), 2, "nothing was trusted");
    }

    #[test]
    fn t_trusts_and_remembers_and_hands_the_engine_the_hooks() {
        let mut screen = screen();
        let applied = press(&mut screen, Key::Char('t'));
        let Some(ChatEffect::Send(EngineCommand::SetHooks { hooks })) = applied.effect else {
            panic!("expected SetHooks, got {:?}", applied.effect);
        };
        assert_eq!(hooks.len(), 2);
        assert!(screen.chat.hook_trust.is_none());
        assert_eq!(pending_after(&screen), 0, "remembered for the next start");
    }

    #[test]
    fn once_runs_them_without_remembering() {
        let mut screen = screen();
        press(&mut screen, Key::Up);
        press(&mut screen, Key::Enter);
        // Up from `no` lands on `once`.
        assert!(screen.chat.hook_trust.is_none());
        assert_eq!(pending_after(&screen), 2, "once is not recorded");

        let mut screen = self::screen();
        let applied = press(&mut screen, Key::Char('o'));
        assert!(matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::SetHooks { ref hooks })) if hooks.len() == 2
        ));
        assert_eq!(pending_after(&screen), 2);
    }

    #[test]
    fn esc_declines() {
        let mut screen = screen();
        let applied = press(&mut screen, Key::Esc);
        assert!(applied.effect.is_none());
        assert!(screen.chat.hook_trust.is_none());
        assert_eq!(pending_after(&screen), 2);
    }

    #[test]
    fn a_row_names_the_event_the_hook_and_what_it_did() {
        assert_eq!(
            hook_row("PreToolUse", "lint.sh", HookOutcome::Blocked, "no rm -rf").as_deref(),
            Some("hook PreToolUse · lint.sh · blocked: no rm -rf")
        );
        assert_eq!(
            hook_row("Stop", "t.sh", HookOutcome::TimedOut, "").as_deref(),
            Some("hook Stop · t.sh · timed out")
        );
        assert_eq!(hook_row("Stop", "t.sh", HookOutcome::Ok, ""), None);
    }
}
