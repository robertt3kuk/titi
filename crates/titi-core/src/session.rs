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
    /// The index file was written by a release that knew a newer schema.
    ///
    /// Reading it with this build's column meanings would misinterpret
    /// whatever changed, so the file is refused instead.
    SchemaTooNew {
        found: i64,
        supported: i64,
    },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "session io: {e}"),
            SessionError::Json(e) => write!(f, "session json: {e}"),
            SessionError::Db(e) => write!(f, "session index: {e}"),
            SessionError::NotFound(what) => write!(f, "not found: {what}"),
            SessionError::SchemaTooNew { found, supported } => write!(
                f,
                "session index is schema {found}; this build understands {supported}"
            ),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Io(e) => Some(e),
            SessionError::Json(e) => Some(e),
            SessionError::Db(e) => Some(e),
            SessionError::NotFound(_) | SessionError::SchemaTooNew { .. } => None,
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
