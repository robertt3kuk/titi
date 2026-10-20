//! What the `copy · fork` on a turn's footer does, and `/copy`.
//!
//! The footer row draws the two words ([`crate::transcript`]) and remembers
//! where it drew them; a click on one lands here. Copy puts the turn's answer
//! on the clipboard the way a selection does (the OS writer, else OSC 52);
//! fork starts a new session from the conversation as it stood at the end of
//! that turn. `/copy [n]` is the same copy for a keyboard, and works with the
//! mouse reporting off.

use std::path::Path;

use titi_core::session::{Checkpoint, Entry, Role, SessionMeta, SessionStore};
use titi_tui::status::FooterAction;

use crate::chat::{Applied, Chat};
use crate::keys::copy_selection;
use crate::transcript::{LineKind, TranscriptLine};

/// The answers in `lines`, oldest first: one per turn that said anything,
/// every round of the turn's text joined by a blank line. A turn is what
/// follows a user message up to the next one.
pub(crate) fn answers(lines: &[TranscriptLine]) -> Vec<String> {
    let mut found = Vec::new();
    let mut turn: Vec<&str> = Vec::new();
    for line in lines {
        match line.kind {
            LineKind::User => {
                if !turn.is_empty() {
                    found.push(turn.join("\n\n"));
                    turn.clear();
                }
            }
            LineKind::Assistant if !line.text.trim().is_empty() => turn.push(line.text.trim_end()),
            _ => {}
        }
    }
    if !turn.is_empty() {
        found.push(turn.join("\n\n"));
    }
    found
}

/// The answer of the turn the line at `at` ends: what the assistant said
/// since the last user message before it.
fn answer_before(lines: &[TranscriptLine], at: usize) -> String {
    let upto = at.min(lines.len());
    let start = lines[..upto]
        .iter()
        .rposition(|line| line.kind == LineKind::User)
        .map_or(0, |user| user + 1);
    answers(&lines[start..upto]).join("\n\n")
}

/// Which prompt of the session a turn belongs to, in what the transcript
/// holds, so the same prompt can be found in the session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptAnchor {
    /// The turn's own prompt, counting from 1 over every prompt the
    /// transcript holds (a message sent mid-turn is a prompt too: the session
    /// file records it as one).
    pub(crate) ordinal: usize,
    /// Prompts the transcript holds in all.
    pub(crate) total: usize,
    /// What the prompt said, to check the session file's entry against.
    pub(crate) text: String,
}

fn is_prompt(kind: LineKind) -> bool {
    matches!(kind, LineKind::User | LineKind::Steer)
}

/// The prompt of the turn the footer at `at` closes; `None` when no prompt
/// precedes it (a transcript that starts mid-conversation).
pub(crate) fn anchor_at(lines: &[TranscriptLine], at: usize) -> Option<PromptAnchor> {
    let upto = (at + 1).min(lines.len());
    let ordinal = lines[..upto]
        .iter()
        .filter(|line| is_prompt(line.kind))
        .count();
    let prompt = lines[..upto]
        .iter()
        .rev()
        .find(|line| is_prompt(line.kind))?;
    Some(PromptAnchor {
        ordinal,
        total: lines.iter().filter(|line| is_prompt(line.kind)).count(),
        text: prompt.text.clone(),
    })
}

/// Where a turn ends in the session file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    /// After the entry at this position: the next prompt starts right behind.
    After(usize),
    /// The turn is the session's newest, so its end is the session's.
    Latest,
}

/// Find the end of the anchored turn among the session's `entries`.
///
/// The transcript may start part way through a resumed session, so its prompts
/// are matched to the *last* prompts of the file; the one the anchor names
/// must also say what the transcript says, or the match is not trusted. `None`
/// when it cannot be matched — a prompt whose pasted text the file holds
/// expanded, say — and the caller says so rather than guessing a place.
pub(crate) fn locate_turn_end(entries: &[Entry], anchor: &PromptAnchor) -> Option<TurnEnd> {
    let prompts: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| (entry.role == Role::User).then_some(at))
        .collect();
    let first = prompts.len().checked_sub(anchor.total)?;
    let mine = first + anchor.ordinal.checked_sub(1)?;
    let entry = entries.get(*prompts.get(mine)?)?;
    if !entry.content.starts_with(anchor.text.trim()) {
        return None;
    }
    match prompts.get(mine + 1) {
        Some(next) => Some(TurnEnd::After(next - 1)),
        None => Some(TurnEnd::Latest),
    }
}

/// Fork the session at the end of the anchored turn. A turn that cannot be
/// placed forks the whole session, and the answer says so.
pub(crate) fn fork_at_turn(
    agent_dir: &Path,
    session_id: &str,
    anchor: Option<&PromptAnchor>,
) -> Result<String, String> {
    let store = SessionStore::new(agent_dir).map_err(|error| error.to_string())?;
    let entries = store
        .walk(session_id, None)
        .map_err(|error| error.to_string())?;
    let end = anchor.and_then(|anchor| locate_turn_end(&entries, anchor));
    let whole = |store: &SessionStore| {
        store
            .fork_session(session_id, SessionMeta::default())
            .map_err(|error| error.to_string())
    };
    match end {
        Some(TurnEnd::After(at)) => {
            let checkpoint = Checkpoint {
                entry_id: entries.get(at).map(|entry| entry.id.clone()),
                entries: at + 1,
                ts: 0,
                git_commit: None,
            };
            let new_id = store
                .branch_session(session_id, &checkpoint, SessionMeta::default())
                .map_err(|error| error.to_string())?;
            Ok(format!(
                "forked to {new_id} at the end of this turn · restart to resume it"
            ))
        }
        Some(TurnEnd::Latest) => Ok(format!(
            "forked to {} · this is the latest turn, so the whole session · restart to resume it",
            whole(&store)?
        )),
        None => Ok(format!(
            "forked the whole session to {} · this turn could not be placed in the session file · restart to resume it",
            whole(&store)?
        )),
    }
}

impl Chat {
    /// `/copy [n]`: the nth-last answer (the newest by default) to the
    /// clipboard.
    pub(crate) fn copy_command(&mut self, args: &str) -> Applied {
        let path = std::env::var("PATH").unwrap_or_default();
        self.copy_answer(args, &path)
    }

    /// [`Chat::copy_command`] with the search path for the OS clipboard writer
    /// given, so a test never reaches the machine's own clipboard.
    pub(crate) fn copy_answer(&mut self, args: &str, path: &str) -> Applied {
        let nth = if args.is_empty() {
            1
        } else {
            match args.parse::<usize>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    self.push(
                        LineKind::Error,
                        format!(
                            "usage: /copy [n] — n counts back from the newest answer (got {args})"
                        ),
                    );
                    return Applied::none();
                }
            }
        };
        let found = answers(&self.lines);
        if found.is_empty() {
            self.push(LineKind::Note, "copy: no answer yet".to_owned());
            return Applied::none();
        }
        match found.iter().rev().nth(nth - 1) {
            Some(text) => {
                let text = text.clone();
                copy_selection(self, &text, path);
                // The submit that ran this command clears the hint, so the
                // route the copy took is said where it stays.
                let said = std::mem::take(&mut self.hint);
                self.push(LineKind::Note, said);
            }
            None => {
                let have = found.len();
                self.push(
                    LineKind::Note,
                    format!(
                        "copy: only {have} answer{} so far",
                        if have == 1 { "" } else { "s" }
                    ),
                );
            }
        }
        Applied::none()
    }

    /// The footer action under a screen cell, and the usage line it belongs
    /// to; `None` for a click anywhere else.
    pub(crate) fn footer_action_at(&self, x: u16, y: u16) -> Option<(usize, FooterAction)> {
        let row = y.checked_sub(self.transcript_top)?;
        let hit = self.footer_hits.iter().find(|hit| hit.row == row)?;
        let action = hit.actions.action_at(usize::from(x))?;
        (self.lines.get(hit.line)?.kind == LineKind::Usage).then_some((hit.line, action))
    }

    /// Do what a click on a footer's action asked for. `path` is where the OS
    /// clipboard writer is looked for.
    pub(crate) fn run_footer_action(&mut self, line: usize, action: FooterAction, path: &str) {
        match action {
            FooterAction::Copy => {
                let text = answer_before(&self.lines, line);
                if text.is_empty() {
                    self.push(LineKind::Note, "copy: this turn said nothing".to_owned());
                } else {
                    copy_selection(self, &text, path);
                }
            }
            FooterAction::Fork => {
                let anchor = anchor_at(&self.lines, line);
                match fork_at_turn(&self.agent_dir, &self.session_id, anchor.as_ref()) {
                    Ok(summary) => self.push(LineKind::Note, summary),
                    Err(reason) => self.push(LineKind::Error, format!("fork: {reason}")),
                }
            }
        }
    }
}
