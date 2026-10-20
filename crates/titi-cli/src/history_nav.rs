//! Shell-style prompt recall: ↑ puts the previous prompt in the composer, ↓
//! walks forward and finally back to the draft that was there before.
//!
//! [`HistoryNav`] is only the walk — which entry is on screen and what to
//! restore at the end of it. Where the entries come from, and when ↑ means
//! "recall" rather than "scroll the transcript", is the chat's call
//! (`Chat::recall_older` in `keys.rs`).

use std::collections::HashMap;
use std::path::Path;

/// The unsent draft a walk started from, markers and all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) text: String,
    pub(crate) pastes: HashMap<String, String>,
}

/// What ↓ puts in the composer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recall {
    /// A newer prompt.
    Entry(String),
    /// The end of the walk: the draft it started from.
    Draft(Held),
}

#[derive(Debug, Default)]
pub(crate) struct HistoryNav {
    /// Newest first, as of the moment the walk started.
    entries: Vec<String>,
    /// The entry on screen; `None` before the first ↑ of a walk.
    at: Option<usize>,
    /// What the composer held right after the last recall: a composer that no
    /// longer says this has been edited, and the walk is over.
    shown: String,
    held: Option<Held>,
}

impl HistoryNav {
    /// Whether `input` is still the recalled entry, untouched.
    pub(crate) fn browsing(&self, input: &str) -> bool {
        self.at.is_some() && self.shown == input
    }

    /// Begin a walk over `entries` (newest first) from the draft `held`.
    pub(crate) fn start(&mut self, entries: Vec<String>, held: Held) {
        self.entries = entries;
        self.at = None;
        self.shown.clear();
        self.held = Some(held);
    }

    /// The next older entry, or `None` at the oldest.
    pub(crate) fn older(&mut self) -> Option<String> {
        let next = self.at.map_or(0, |at| at + 1);
        let entry = self.entries.get(next)?.clone();
        self.at = Some(next);
        self.shown.clone_from(&entry);
        Some(entry)
    }

    /// The next newer entry, or the draft once the newest is passed. `None`
    /// when no walk is on.
    pub(crate) fn newer(&mut self) -> Option<Recall> {
        let at = self.at?;
        if at == 0 {
            let held = self.held.take().unwrap_or_default();
            self.reset();
            return Some(Recall::Draft(held));
        }
        let entry = self.entries.get(at - 1)?.clone();
        self.at = Some(at - 1);
        self.shown.clone_from(&entry);
        Some(Recall::Entry(entry))
    }

    pub(crate) fn reset(&mut self) {
        self.entries.clear();
        self.at = None;
        self.shown.clear();
        self.held = None;
    }
}

/// Most earlier sessions read for the project's history, and most prompts kept
/// from all of them.
const MAX_SESSIONS: usize = 20;
pub(crate) const MAX_ENTRIES: usize = 500;

/// A message's text as the person typed it: the files an `@path` attached are
/// not part of the prompt.
pub(crate) fn typed(content: &str) -> &str {
    crate::mention::without_attachments(content)
}

/// The prompts this project's earlier sessions carried, newest first. Empty
/// when the agent directory has no index to say which sessions are the
/// project's — the history is then this session's own.
pub(crate) fn project_prompts(
    agent_dir: &Path,
    workspace: &Path,
    current_session: &str,
) -> Vec<String> {
    let Some(here) = crate::session_fs::ids_in_workspace(agent_dir, workspace) else {
        return Vec::new();
    };
    let mut prompts = Vec::new();
    let sessions = crate::session_fs::list_sessions_from(agent_dir)
        .into_iter()
        .filter(|id| id != current_session && here.contains(id.as_str()))
        .take(MAX_SESSIONS);
    for id in sessions {
        let Ok(messages) = crate::session_fs::session_history(agent_dir, &id) else {
            continue;
        };
        prompts.extend(
            messages
                .iter()
                .rev()
                .filter(|message| message.role == titi_providers::Role::User)
                .map(|message| typed(&message.content).to_owned())
                .filter(|text| !text.trim().is_empty()),
        );
        if prompts.len() >= MAX_ENTRIES {
            break;
        }
    }
    prompts.truncate(MAX_ENTRIES);
    prompts
}

/// `first` then `second`, each newest first, a prompt that appears twice kept
/// once at its newest place.
pub(crate) fn merged(first: Vec<String>, second: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in first.into_iter().chain(second.iter().cloned()) {
        if seen.insert(entry.clone()) {
            out.push(entry);
        }
        if out.len() >= MAX_ENTRIES {
            break;
        }
    }
    out
}
