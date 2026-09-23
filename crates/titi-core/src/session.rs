//! Session domain: append-only entry trees, JSONL persistence, SQLite/FTS5 index.

pub mod checkpoint;
pub mod entry;
pub mod export;
pub mod index;
pub mod namer;
pub mod store;

pub use checkpoint::Checkpoint;
pub use entry::{Entry, Role};
pub use index::{SearchHit, SessionIndex};
pub use store::{SessionStore, entries_to_messages};

use std::fmt;

/// Errors surfaced by the session subsystem.
#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Db(rusqlite::Error),
    /// Session or entry id does not resolve to anything on disk.
    NotFound(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "session io: {e}"),
            SessionError::Json(e) => write!(f, "session json: {e}"),
            SessionError::Db(e) => write!(f, "session index: {e}"),
            SessionError::NotFound(what) => write!(f, "not found: {what}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Io(e) => Some(e),
            SessionError::Json(e) => Some(e),
            SessionError::Db(e) => Some(e),
            SessionError::NotFound(_) => None,
        }
    }
}

/// Descriptive metadata recorded when a session is created.
#[derive(Debug, Default, Clone)]
pub struct SessionMeta {
    pub title: Option<String>,
    pub bot_id: Option<String>,
    pub source: Option<String>,
}
