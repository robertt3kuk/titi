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
    /// A line of a JSONL file that has valid lines after it does not parse.
    ///
    /// Only the final line can be torn by a crash, so any earlier one is
    /// corruption. It is reported rather than skipped: a session that lost a
    /// middle entry without a word reads as a complete conversation that
    /// simply never said that.
    Corrupt {
        /// 1-based line number in the file.
        line: usize,
        source: serde_json::Error,
    },
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
            SessionError::Corrupt { line, source } => {
                write!(f, "session line {line} is corrupt: {source}")
            }
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
            SessionError::Corrupt { source, .. } => Some(source),
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
    /// The workspace root the session was started in, as an absolute path
    /// string.
    ///
    /// `None` means no workspace was ever recorded for this session — every
    /// session written before this field existed, and nothing else: the store
    /// records the process working directory when a caller does not name one
    /// ([`SessionStore::create`](store::SessionStore::create)). A reader must
    /// therefore treat `None` as "unknown", never as "not this workspace": a
    /// workspace-scoped lookup that matches nothing falls back to the wider
    /// scope, so an old session stays reachable instead of being hidden by a
    /// filter it cannot satisfy.
    pub cwd: Option<String>,
}
