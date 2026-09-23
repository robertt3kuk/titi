//! SQLite + FTS5 index over sessions — a derivable structure built
//! incrementally on every append (`docs/research/sessions-persistence`).

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use super::entry::Entry;
use super::{SessionError, SessionMeta};

/// A full-text search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub text: String,
}

/// SQLite index at `<agent_dir>/state.db` (WAL): session catalog, entries,
/// and an FTS5 table over entry text.
pub struct SessionIndex {
    conn: Connection,
}

/// `title_source` records who named a session: `user` for a title the user
/// chose, `auto` for one the session namer generated, `NULL` for a session
/// nobody has named yet. The placeholder a surface writes at creation time
/// counts as unnamed, so a fresh session can still be given a real title.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT PRIMARY KEY,
    title        TEXT,
    created_at   INTEGER NOT NULL,
    bot_id       TEXT,
    source       TEXT,
    title_source TEXT
);
CREATE TABLE IF NOT EXISTS entries (
    session_id TEXT NOT NULL,
    entry_id   TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    text       TEXT NOT NULL,
    PRIMARY KEY (session_id, entry_id)
);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    text, entry_id UNINDEXED, session_id UNINDEXED
);
";

impl SessionIndex {
    /// Opens (or creates) the index database, running the schema migration.
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(SessionError::Io)?;
        }
        let conn = Connection::open(path).map_err(SessionError::Db)?;
        // Two handles now write here: the store on the surface's thread and
        // the session namer from its own task. A busy timeout makes the
        // loser of that race wait instead of failing the write.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(SessionError::Db)?;
        conn.execute_batch(SCHEMA).map_err(SessionError::Db)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Adds what a database from an earlier release is missing.
    ///
    /// `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it
    /// was, so without this every title write against an already-created
    /// `state.db` would fail on an unknown column.
    fn migrate(conn: &Connection) -> Result<(), SessionError> {
        let mut columns = conn
            .prepare("PRAGMA table_info(sessions)")
            .map_err(SessionError::Db)?;
        let has_title_source = columns
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(SessionError::Db)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(SessionError::Db)?
            .iter()
            .any(|name| name == "title_source");
        if !has_title_source {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN title_source TEXT;")
                .map_err(SessionError::Db)?;
        }
        Ok(())
    }

    /// Records a session in the catalog.
    pub fn insert_session(
        &self,
        id: &str,
        created_at: u64,
        meta: &SessionMeta,
    ) -> Result<(), SessionError> {
        self.conn
            .execute(
                "INSERT INTO sessions (id, title, created_at, bot_id, source)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, meta.title, created_at as i64, meta.bot_id, meta.source],
            )
            .map_err(SessionError::Db)?;
        Ok(())
    }

    /// Indexes one entry: catalog row + FTS row.
    pub fn index_entry(&self, session_id: &str, entry: &Entry) -> Result<(), SessionError> {
        self.conn
            .execute(
                "INSERT INTO entries (session_id, entry_id, ts, text) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, entry.id, entry.ts as i64, entry.content],
            )
            .map_err(SessionError::Db)?;
        self.conn
            .execute(
                "INSERT INTO entries_fts (text, entry_id, session_id) VALUES (?1, ?2, ?3)",
                params![entry.content, entry.id, session_id],
            )
            .map_err(SessionError::Db)?;
        Ok(())
    }

    /// Full-text search over indexed entries, optionally isolated to one bot.
    ///
    /// The raw query is wrapped as an FTS5 phrase, so user input can never
    /// alter the query grammar.
    pub fn search(
        &self,
        query: &str,
        bot_id: Option<&str>,
    ) -> Result<Vec<SearchHit>, SessionError> {
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT entries_fts.entry_id, entries_fts.session_id, entries_fts.text
                 FROM entries_fts JOIN sessions ON sessions.id = entries_fts.session_id
                 WHERE entries_fts MATCH ?1 AND (?2 IS NULL OR sessions.bot_id = ?2)
                 ORDER BY rank",
            )
            .map_err(SessionError::Db)?;
        let hits = stmt
            .query_map(params![phrase, bot_id], |row| {
                Ok(SearchHit {
                    entry_id: row.get(0)?,
                    session_id: row.get(1)?,
                    text: row.get(2)?,
                })
            })
            .map_err(SessionError::Db)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(SessionError::Db)?;
        Ok(hits)
    }

    /// Drops a session's indexed entries and re-indexes `entries`. Used after
    /// a rewind shortens the persisted tree, so search cannot surface entries
    /// that are no longer on disk.
    pub fn reindex_session(&self, session_id: &str, entries: &[Entry]) -> Result<(), SessionError> {
        self.conn
            .execute(
                "DELETE FROM entries WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(SessionError::Db)?;
        self.conn
            .execute(
                "DELETE FROM entries_fts WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(SessionError::Db)?;
        for entry in entries {
            self.index_entry(session_id, entry)?;
        }
        Ok(())
    }

    /// Id of the most recently created session, if any.
    pub fn resume_latest(&self) -> Result<Option<String>, SessionError> {
        self.conn
            .query_row(
                "SELECT id FROM sessions ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(SessionError::Db)
    }

    /// The session's title, if it has one.
    pub fn title(&self, session_id: &str) -> Result<Option<String>, SessionError> {
        self.conn
            .query_row(
                "SELECT title FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(SessionError::Db)
            .map(Option::flatten)
    }

    /// Records a title the user chose. The session namer never replaces it.
    pub fn set_title(&self, session_id: &str, title: &str) -> Result<(), SessionError> {
        self.conn
            .execute(
                "UPDATE sessions SET title = ?2, title_source = 'user' WHERE id = ?1",
                params![session_id, title],
            )
            .map_err(SessionError::Db)?;
        Ok(())
    }

    /// Records a generated title, and reports whether it was taken.
    ///
    /// The condition lives in the statement rather than in a read followed by
    /// a write, so a `/rename` that lands while the namer is talking to the
    /// model cannot be undone by the answer arriving a moment later.
    pub fn set_auto_title(&self, session_id: &str, title: &str) -> Result<bool, SessionError> {
        let updated = self
            .conn
            .execute(
                "UPDATE sessions SET title = ?2, title_source = 'auto'
                 WHERE id = ?1 AND title_source IS NULL",
                params![session_id, title],
            )
            .map_err(SessionError::Db)?;
        Ok(updated > 0)
    }

    /// Whether this session is still waiting for a generated title. Answers
    /// "is it worth calling a model at all" before one is called.
    pub fn needs_auto_title(&self, session_id: &str) -> Result<bool, SessionError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1 AND title_source IS NULL",
                params![session_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(SessionError::Db)?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_index() -> (tempfile::TempDir, SessionIndex) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let index = SessionIndex::open(&dir.path().join("state.db"))
            .unwrap_or_else(|e| panic!("open: {e}"));
        (dir, index)
    }

    fn meta(bot_id: Option<&str>) -> SessionMeta {
        SessionMeta {
            title: None,
            bot_id: bot_id.map(String::from),
            source: Some("cli".into()),
        }
    }

    #[test]
    fn resume_latest_returns_newest_session() {
        let (_dir, index) = tmp_index();
        assert_eq!(
            index.resume_latest().unwrap_or_else(|e| panic!("{e}")),
            None
        );
        index
            .insert_session("s1", 100, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        index
            .insert_session("s2", 200, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            index.resume_latest().unwrap_or_else(|e| panic!("{e}")),
            Some("s2".into())
        );
    }

    #[test]
    fn search_matches_phrase_and_prefix_tokens() {
        let (_dir, index) = tmp_index();
        let e = Entry::new(None, super::super::Role::User, "deploy kafka cluster");
        index
            .insert_session("s1", 1, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        index
            .index_entry("s1", &e)
            .unwrap_or_else(|e| panic!("{e}"));

        let hits = index
            .search("kafka", None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry_id, e.id);

        // Phrase matches only the exact token sequence, not arbitrary text.
        assert!(
            index
                .search("kafka deploy", None)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty()
        );
        assert!(
            index
                .search("deploy kafka", None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len()
                == 1
        );
    }

    #[test]
    fn fts_query_injection_is_neutralized() {
        let (_dir, index) = tmp_index();
        let e = Entry::new(None, super::super::Role::User, "safe text");
        index
            .insert_session("s1", 1, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        index
            .index_entry("s1", &e)
            .unwrap_or_else(|e| panic!("{e}"));
        // Raw FTS5 grammar in user input must not error or escape the phrase.
        assert!(
            index
                .search("safe\" OR (1=1) AND \"", None)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty()
        );
    }

    /// A session nobody named takes a generated title once, and only once.
    #[test]
    fn a_generated_title_lands_on_an_unnamed_session_and_never_twice() {
        let (_dir, index) = tmp_index();
        index
            .insert_session("s1", 1, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            index
                .needs_auto_title("s1")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert!(
            index
                .set_auto_title("s1", "fix the parser")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert_eq!(
            index.title("s1").unwrap_or_else(|e| panic!("{e}")),
            Some("fix the parser".to_owned())
        );
        assert!(
            !index
                .needs_auto_title("s1")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert!(
            !index
                .set_auto_title("s1", "something else")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert_eq!(
            index.title("s1").unwrap_or_else(|e| panic!("{e}")),
            Some("fix the parser".to_owned())
        );
    }

    /// A name the user typed is never replaced by a generated one.
    #[test]
    fn a_user_chosen_title_survives_the_namer() {
        let (_dir, index) = tmp_index();
        index
            .insert_session("s1", 1, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        index
            .set_title("s1", "ship the release")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            !index
                .needs_auto_title("s1")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert!(
            !index
                .set_auto_title("s1", "fix the parser")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert_eq!(
            index.title("s1").unwrap_or_else(|e| panic!("{e}")),
            Some("ship the release".to_owned())
        );
    }

    /// A `state.db` written before titles had provenance keeps working: the
    /// column is added on open instead of every title write failing.
    #[test]
    fn an_index_from_an_earlier_release_gains_the_title_column() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("state.db");
        let old = Connection::open(&path).unwrap_or_else(|e| panic!("{e}"));
        old.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, title TEXT, created_at INTEGER NOT NULL,
                 bot_id TEXT, source TEXT
             );
             INSERT INTO sessions (id, title, created_at) VALUES ('s1', 'titi', 1);",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        drop(old);

        let index = SessionIndex::open(&path).unwrap_or_else(|e| panic!("open: {e}"));
        assert!(
            index
                .set_auto_title("s1", "fix the parser")
                .unwrap_or_else(|e| panic!("{e}"))
        );
        assert_eq!(
            index.title("s1").unwrap_or_else(|e| panic!("{e}")),
            Some("fix the parser".to_owned())
        );
    }
}
