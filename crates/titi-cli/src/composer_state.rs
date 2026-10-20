//! The composer's own working state, kept apart from the chat's: the `@file`
//! picker, the shell-style prompt walk and the project's earlier prompts.

use crate::history_nav::HistoryNav;
use crate::mention::MentionState;

#[derive(Debug, Default)]
pub(crate) struct ComposerState {
    pub(crate) mention: MentionState,
    pub(crate) history: HistoryNav,
    /// Prompts of this project's earlier sessions, newest first. Read from the
    /// session store on the first recall and kept for the run: the sessions it
    /// covers are over, so it cannot go stale.
    pub(crate) project_prompts: Option<Vec<String>>,
}
