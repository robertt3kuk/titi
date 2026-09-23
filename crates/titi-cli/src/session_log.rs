//! Persisting the conversation.
//!
//! The engine emits events; the session store holds the transcript. Nothing
//! joined them, so every session file stayed at 0 bytes, `restore_latest`
//! replayed nothing, and a checkpoint marked an empty tree.
//!
//! This is the join: the surface hands each finished turn to [`SessionLog`],
//! which appends it to the same session the trajectory recorder is bound to.

use std::path::Path;

use titi_core::session::{Role, SessionStore};

/// Appends conversation entries to one session, one JSONL line per entry.
pub struct SessionLog {
    store: SessionStore,
    session_id: String,
}

impl SessionLog {
    /// Opens the store rooted at `<agent_dir>/sessions`.
    ///
    /// Returns `None` when the store cannot be opened at all; a write failure
    /// later is reported per call so one bad append does not stop the CLI.
    pub fn open(agent_dir: &Path, session_id: &str) -> Option<Self> {
        let store = SessionStore::new(agent_dir).ok()?;
        Some(Self {
            store,
            session_id: session_id.to_owned(),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Records a message the user sent.
    pub fn user(&self, text: &str) -> Result<(), String> {
        self.append(Role::User, text)
    }

    /// Records the assistant's reply.
    pub fn assistant(&self, text: &str) -> Result<(), String> {
        self.append(Role::Assistant, text)
    }

    /// Records the assistant message that issued `tool_calls`.
    ///
    /// Unlike a plain reply this is written even with empty text: a model
    /// that only calls a tool says nothing, and dropping the message would
    /// leave the tool results that follow without the call they answer.
    pub fn assistant_tool_calls(
        &self,
        text: &str,
        tool_calls: Vec<titi_providers::ToolCallRef>,
    ) -> Result<(), String> {
        self.store
            .append_with_tool_calls(&self.session_id, Role::Assistant, text, tool_calls)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Records one tool result, already masked by the engine.
    ///
    /// Written even when the tool printed nothing, for the same reason: the
    /// call above it must keep its answer.
    pub fn tool_result(&self, text: &str) -> Result<(), String> {
        self.store
            .append(&self.session_id, Role::Tool, text)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Records a system note (compaction, a resumed-session marker).
    pub fn system(&self, text: &str) -> Result<(), String> {
        self.append(Role::System, text)
    }

    fn append(&self, role: Role, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.store
            .append(&self.session_id, role, text)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}
