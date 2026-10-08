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
/// `cwd` is the workspace the session was started in; `NULL` means it was
/// never recorded (see [`SessionMeta::cwd`]).
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT PRIMARY KEY,
    title        TEXT,
    created_at   INTEGER NOT NULL,
    bot_id       TEXT,
    source       TEXT,
    title_source TEXT,
    cwd          TEXT
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

/// Schema version recorded in `PRAGMA user_version`.
///
/// `1` is the catalog, entries and FTS tables with the title's provenance;
/// `2` adds `sessions.cwd`, the workspace a session was started in.
///
/// Read before anything else is done to the file: a database stamped with a
/// number this build does not know cannot be interpreted with the column
/// meanings it was written under, and guessing is worse than refusing.
const SCHEMA_VERSION: i64 = 2;

impl SessionIndex {
    /// Opens (or creates) the index database, running the schema migration.
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(SessionError::Io)?;
        }
        let conn = Connection::open(path).map_err(SessionError::Db)?;
        Self::check_version(&conn)?;
        // Two handles now write here: the store on the surface's thread and
        // the session namer from its own task. A busy timeout makes the
        // loser of that race wait instead of failing the write.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(SessionError::Db)?;
        conn.execute_batch(SCHEMA).map_err(SessionError::Db)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Accepts this build's schema and anything older, and refuses anything
    /// newer.
    ///
    /// A stamp below the constant is a file from an earlier release — the
    /// normal case now that version 2 exists — and `0` is what SQLite reports
    /// for a file that never stamped one at all (every `state.db` written
    /// before the constant did). Both are stamped current here; the columns
    /// they are missing are added by [`Self::migrate`], which runs on every
    /// open, so a stamp this file already carries never has to be trusted for
    /// the shape of the table. Refusing a newer file is what keeps this from
    /// needing a migration in the other direction.
    fn check_version(conn: &Connection) -> Result<(), SessionError> {
        let found: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SessionError::Db)?;
        if found > SCHEMA_VERSION {
            return Err(SessionError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        if found != SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(SessionError::Db)?;
        }
        Ok(())
    }

    /// Adds what a database from an earlier release is missing.
    ///
    /// `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it
    /// was, so without this every write against an already-created
    /// `state.db` would fail on an unknown column: the title's provenance and,
    /// since version 2, the workspace a session was started in.
    fn migrate(conn: &Connection) -> Result<(), SessionError> {
        Self::add_column_if_missing(
            conn,
            "title_source",
            "ALTER TABLE sessions ADD COLUMN title_source TEXT;",
        )?;
        Self::add_column_if_missing(conn, "cwd", "ALTER TABLE sessions ADD COLUMN cwd TEXT;")
    }

    /// Adds a column an older `sessions` table does not have yet.
    ///
    /// The table's columns are read back rather than the stamped version
    /// trusted, so this is safe to run on every open and heals a file whose
    /// stamp was written before its columns were (the two are not one
    /// transaction).
    fn add_column_if_missing(
        conn: &Connection,
        column: &str,
        alter: &str,
    ) -> Result<(), SessionError> {
        let mut columns = conn
            .prepare("PRAGMA table_info(sessions)")
            .map_err(SessionError::Db)?;
        let present = columns
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(SessionError::Db)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(SessionError::Db)?
            .iter()
            .any(|name| name == column);
        if !present {
            conn.execute_batch(alter).map_err(SessionError::Db)?;
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
                "INSERT INTO sessions (id, title, created_at, bot_id, source, cwd)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    meta.title,
                    created_at as i64,
                    meta.bot_id,
                    meta.source,
                    meta.cwd
                ],
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

    /// Full-text search over indexed entries, optionally isolated to one bot
    /// and to one workspace.
    ///
    /// `None` for either leaves that dimension unfiltered — including
    /// sessions that have no bot, and sessions whose workspace was never
    /// recorded, so an old session is searchable rather than invisible.
    ///
    /// The raw query is wrapped as an FTS5 phrase, so user input can never
    /// alter the query grammar.
    pub fn search(
        &self,
        query: &str,
        bot_id: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Vec<SearchHit>, SessionError> {
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT entries_fts.entry_id, entries_fts.session_id, entries_fts.text
                 FROM entries_fts JOIN sessions ON sessions.id = entries_fts.session_id
                 WHERE entries_fts MATCH ?1
                   AND (?2 IS NULL OR sessions.bot_id = ?2)
                   AND (?3 IS NULL OR sessions.cwd = ?3)
                 ORDER BY rank",
            )
            .map_err(SessionError::Db)?;
        let hits = stmt
            .query_map(params![phrase, bot_id, workspace], |row| {
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

    /// Session ids, newest first, optionally only those started in
    /// `workspace`.
    ///
    /// `Some(root)` keeps the sessions whose recorded workspace is exactly
    /// that root. A session that never recorded one is *not* one of them —
    /// there is nothing to match — so it is absent from the scoped list and
    /// present in the unfiltered one, which is the fallback its caller takes
    /// when the scoped list comes back empty. That is what keeps an old
    /// session reachable rather than hidden behind a filter it predates.
    pub fn sessions_in(&self, workspace: Option<&str>) -> Result<Vec<String>, SessionError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id FROM sessions
                 WHERE ?1 IS NULL OR cwd = ?1
                 ORDER BY created_at DESC, rowid DESC",
            )
            .map_err(SessionError::Db)?;
        let ids = stmt
            .query_map(params![workspace], |row| row.get(0))
            .map_err(SessionError::Db)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(SessionError::Db)?;
        Ok(ids)
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

    /// Catalog metadata recorded for a session, if it is known.
    ///
    /// A session seeded from another one inherits what its caller left
    /// unset, so a fork keeps searching under the same bot.
    pub fn session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, SessionError> {
        self.conn
            .query_row(
                "SELECT title, bot_id, source, cwd FROM sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionMeta {
                        title: row.get(0)?,
                        bot_id: row.get(1)?,
                        source: row.get(2)?,
                        cwd: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(SessionError::Db)
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
            cwd: None,
        }
    }

    /// A session started in `workspace`.
    fn meta_in(bot_id: Option<&str>, workspace: &str) -> SessionMeta {
        SessionMeta {
            cwd: Some(workspace.to_owned()),
            ..meta(bot_id)
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

    /// A session keeps the workspace it was recorded in, and an absent one
    /// reads back as absent rather than as an empty string.
    #[test]
    fn a_session_keeps_the_workspace_it_was_recorded_in() {
        let (_dir, index) = tmp_index();
        index
            .insert_session("here", 100, &meta_in(None, "/work/here"))
            .unwrap_or_else(|e| panic!("{e}"));
        index
            .insert_session("unknown", 200, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));

        let here = index
            .session_meta("here")
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("here was inserted"));
        assert_eq!(here.cwd.as_deref(), Some("/work/here"));
        let unknown = index
            .session_meta("unknown")
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("unknown was inserted"));
        assert_eq!(unknown.cwd, None);
    }

    /// A `state.db` stamped 1 — the release before sessions carried a
    /// workspace — opens, keeps the sessions it has, and gains the column:
    /// its rows read back as "no workspace recorded", never as missing.
    #[test]
    fn a_version_1_index_gains_the_workspace_column() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("state.db");
        let old = Connection::open(&path).unwrap_or_else(|e| panic!("{e}"));
        old.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, title TEXT, created_at INTEGER NOT NULL,
                 bot_id TEXT, source TEXT, title_source TEXT
             );
             INSERT INTO sessions (id, title, created_at, source)
                 VALUES ('s1', 'titi', 1, 'cli');
             PRAGMA user_version = 1;",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        drop(old);

        let index = SessionIndex::open(&path).unwrap_or_else(|e| panic!("open: {e}"));
        let legacy = index
            .session_meta("s1")
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("the old session survived the migration"));
        assert_eq!(legacy.cwd, None, "no workspace was ever recorded for it");

        index
            .insert_session("s2", 2, &meta_in(None, "/work/a"))
            .unwrap_or_else(|e| panic!("{e}"));
        let fresh = index
            .session_meta("s2")
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("the new session was written"));
        assert_eq!(fresh.cwd.as_deref(), Some("/work/a"));

        let stamped: i64 = index
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or_else(|e| panic!("user_version: {e}"));
        assert_eq!(stamped, SCHEMA_VERSION);
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
            .search("kafka", None, None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry_id, e.id);

        // Phrase matches only the exact token sequence, not arbitrary text.
        assert!(
            index
                .search("kafka deploy", None, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty()
        );
        assert!(
            index
                .search("deploy kafka", None, None)
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
                .search("safe\" OR (1=1) AND \"", None, None)
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

    /// A fresh index is stamped, and the stamp survives the open.
    #[test]
    fn a_new_index_records_the_schema_version() {
        let (_dir, index) = tmp_index();
        drop(index);
        let reopened = SessionIndex::open(&_dir.path().join("state.db"))
            .unwrap_or_else(|e| panic!("open: {e}"));
        let found: i64 = reopened
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or_else(|e| panic!("user_version: {e}"));
        assert_eq!(found, SCHEMA_VERSION);
    }

    /// An index from before the stamp existed is adopted, not refused: it has
    /// the current schema, it just never said so.
    #[test]
    fn an_unstamped_index_is_adopted_and_stamped() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("state.db");
        Connection::open(&path)
            .unwrap_or_else(|e| panic!("{e}"))
            .execute_batch(
                "CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, title TEXT, created_at INTEGER NOT NULL,
                     bot_id TEXT, source TEXT
                 );",
            )
            .unwrap_or_else(|e| panic!("{e}"));

        let index = SessionIndex::open(&path).unwrap_or_else(|e| panic!("open: {e}"));
        index
            .insert_session("s1", 1, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));
        let found: i64 = index
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or_else(|e| panic!("user_version: {e}"));
        assert_eq!(found, SCHEMA_VERSION);
    }

    /// A file from a later release is refused with a typed error instead of
    /// being read with this build's column meanings.
    #[test]
    fn an_index_from_a_newer_release_is_refused() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("state.db");
        Connection::open(&path)
            .unwrap_or_else(|e| panic!("{e}"))
            .pragma_update(None, "user_version", SCHEMA_VERSION + 7)
            .unwrap_or_else(|e| panic!("{e}"));

        match SessionIndex::open(&path) {
            Err(SessionError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, SCHEMA_VERSION + 7);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            Err(other) => panic!("expected a newer-schema refusal, got {other}"),
            Ok(_) => panic!("a file from a newer release was opened instead of refused"),
        }
    }

    /// The scoped listing is exactly the sessions of one workspace, while the
    /// unscoped one still holds every session — the old one included, which
    /// is what keeps it reachable.
    #[test]
    fn listing_scopes_to_a_workspace_without_hiding_old_sessions() {
        let (_dir, index) = tmp_index();
        index
            .insert_session("a", 100, &meta_in(None, "/work/a"))
            .unwrap_or_else(|e| panic!("{e}"));
        index
            .insert_session("b", 200, &meta_in(None, "/work/b"))
            .unwrap_or_else(|e| panic!("{e}"));
        // A session from before workspaces were recorded.
        index
            .insert_session("legacy", 300, &meta(None))
            .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(
            index
                .sessions_in(Some("/work/a"))
                .unwrap_or_else(|e| panic!("{e}")),
            vec!["a".to_owned()]
        );
        assert_eq!(
            index
                .sessions_in(Some("/work/c"))
                .unwrap_or_else(|e| panic!("{e}")),
            Vec::<String>::new()
        );
        // Newest first, and the session with no workspace is among them.
        assert_eq!(
            index.sessions_in(None).unwrap_or_else(|e| panic!("{e}")),
            vec!["legacy".to_owned(), "b".to_owned(), "a".to_owned()]
        );
    }

    /// Search takes the same filter: one workspace is one project's
    /// transcripts, and leaving it off searches everything there is.
    #[test]
    fn search_scopes_to_a_workspace_without_hiding_old_sessions() {
        let (_dir, index) = tmp_index();
        let here = Entry::new(None, super::super::Role::User, "shared kafka note");
        let there = Entry::new(None, super::super::Role::User, "shared kafka note");
        let legacy = Entry::new(None, super::super::Role::User, "shared kafka note");
        for (id, ts, meta, entry) in [
            ("here", 100, meta_in(None, "/work/a"), &here),
            ("there", 200, meta_in(None, "/work/b"), &there),
            ("legacy", 300, meta(None), &legacy),
        ] {
            index
                .insert_session(id, ts, &meta)
                .unwrap_or_else(|e| panic!("{e}"));
            index
                .index_entry(id, entry)
                .unwrap_or_else(|e| panic!("{e}"));
        }

        let scoped = index
            .search("kafka", None, Some("/work/a"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].session_id, "here");

        let all = index
            .search("kafka", None, None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(all.len(), 3, "every session is still searchable");
    }
}
