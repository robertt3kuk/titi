//! Auth store: the single issuer of API keys and OAuth tokens.
//!
//! SQLite database at `<agent_dir>/auth.db`; the file is restricted to
//! `0600` (owner read/write only) so credentials never leak to other users.
//!
//! Spec: docs/research/secrets-env/README.md (omp AuthStorage analogue).

use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Store error: filesystem failure, database failure, or a rejected account
/// label. No variant ever carries a token.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Db(rusqlite::Error),
    /// Account label outside `[A-Za-z0-9._-]{1,64}`.
    InvalidLabel(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "auth store io error: {e}"),
            Error::Db(e) => write!(f, "auth store db error: {e}"),
            Error::InvalidLabel(label) => write!(
                f,
                "invalid account label {label:?}: expected 1-64 characters of [A-Za-z0-9._-]"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Db(e) => Some(e),
            Error::InvalidLabel(_) => None,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

/// Label of the implicit account every single-account provider uses.
pub const DEFAULT_LABEL: &str = "default";

/// Longest accepted label; keeps `provider/label` ids short enough to show.
const MAX_LABEL_LEN: usize = 64;

/// Table shape v2: one row per `provider/label` account.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS credentials (
                provider   TEXT NOT NULL,
                label      TEXT NOT NULL DEFAULT 'default',
                kind       TEXT NOT NULL,
                token      TEXT NOT NULL,
                expires_at INTEGER,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (provider, label)
            );";

/// One stored credential, identified by `provider/label`.
///
/// `Debug` masks the token on purpose: only the label and the last four
/// characters may ever reach a log, an error or a screen.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub provider: String,
    /// Account name within the provider (`work`, `personal`);
    /// [`DEFAULT_LABEL`] for a provider with a single account.
    pub label: String,
    /// e.g. `oauth` or `api_key`.
    pub kind: String,
    pub token: String,
    /// Unix seconds; `None` = never expires.
    pub expires_at: Option<i64>,
    pub updated_at: i64,
}

impl Credential {
    /// `provider/label` — the identifier callers may print.
    pub fn account_id(&self) -> String {
        format!("{}/{}", self.provider, self.label)
    }

    /// The only renderable form of the secret: an ellipsis plus its last four
    /// characters, or a bare ellipsis when the token is too short to mask.
    pub fn masked_token(&self) -> String {
        mask_secret(&self.token)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("provider", &self.provider)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("token", &self.masked_token())
            .field("expires_at", &self.expires_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Reduce a secret to its last four characters. Anything shorter than five
/// characters is hidden entirely.
pub fn mask_secret(secret: &str) -> String {
    let len = secret.chars().count();
    if len <= 4 {
        return "…".to_owned();
    }
    let tail: String = secret.chars().skip(len - 4).collect();
    format!("…{tail}")
}

fn validate_label(label: &str) -> Result<()> {
    let ok = !label.is_empty()
        && label.chars().count() <= MAX_LABEL_LEN
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidLabel(label.to_owned()))
    }
}

/// SQLite-backed credential store (`credentials` table, upsert semantics,
/// several accounts per provider).
pub struct AuthStore {
    conn: rusqlite::Connection,
}

impl AuthStore {
    /// Open (creating if needed) the database at `path` with `0600` permissions.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Create the file ourselves with mode 0600 so SQLite never races a
        // world-readable default.
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        opts.open(path)?;

        let conn = rusqlite::Connection::open(path)?;
        migrate(&conn)?;
        #[cfg(unix)]
        restrict_permissions(path)?;
        Ok(Self { conn })
    }

    /// Insert or update the provider's [`DEFAULT_LABEL`] account.
    pub fn store(
        &self,
        provider: &str,
        kind: &str,
        token: &str,
        expires_at: Option<i64>,
    ) -> Result<()> {
        self.store_account(provider, DEFAULT_LABEL, kind, token, expires_at)
    }

    /// Insert or update one `provider/label` account.
    pub fn store_account(
        &self,
        provider: &str,
        label: &str,
        kind: &str,
        token: &str,
        expires_at: Option<i64>,
    ) -> Result<()> {
        validate_label(label)?;
        self.conn.execute(
            "INSERT INTO credentials (provider, label, kind, token, expires_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(provider, label) DO UPDATE SET
                kind = excluded.kind,
                token = excluded.token,
                expires_at = excluded.expires_at,
                updated_at = excluded.updated_at",
            rusqlite::params![provider, label, kind, token, expires_at, now()],
        )?;
        Ok(())
    }

    /// The provider's primary account: [`DEFAULT_LABEL`] when it exists, else
    /// the first label in rotation order.
    pub fn get(&self, provider: &str) -> Result<Option<Credential>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, label, kind, token, expires_at, updated_at
             FROM credentials WHERE provider = ?1
             ORDER BY label = 'default' DESC, label ASC
             LIMIT 1",
        )?;
        let mut rows = stmt.query([provider])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_credential(row)?)),
            None => Ok(None),
        }
    }

    /// One exact `provider/label` account.
    pub fn get_account(&self, provider: &str, label: &str) -> Result<Option<Credential>> {
        validate_label(label)?;
        let mut stmt = self.conn.prepare(
            "SELECT provider, label, kind, token, expires_at, updated_at
             FROM credentials WHERE provider = ?1 AND label = ?2",
        )?;
        let mut rows = stmt.query([provider, label])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_credential(row)?)),
            None => Ok(None),
        }
    }

    /// Every account of `provider` in rotation order ([`DEFAULT_LABEL`]
    /// first, then alphabetical) — the input of a rotation.
    pub fn accounts(&self, provider: &str) -> Result<Vec<Credential>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, label, kind, token, expires_at, updated_at
             FROM credentials WHERE provider = ?1
             ORDER BY label = 'default' DESC, label ASC",
        )?;
        let rows = stmt.query_map([provider], row_to_credential)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Account labels of `provider`, in rotation order. Display path: no
    /// token ever leaves the database.
    pub fn account_labels(&self, provider: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT label FROM credentials WHERE provider = ?1
             ORDER BY label = 'default' DESC, label ASC",
        )?;
        let rows = stmt.query_map([provider], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Remove every account of `provider`; returns whether a row existed.
    pub fn remove(&self, provider: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM credentials WHERE provider = ?1", [provider])?;
        Ok(n > 0)
    }

    /// Remove one `provider/label` account; returns whether it existed.
    pub fn remove_account(&self, provider: &str, label: &str) -> Result<bool> {
        validate_label(label)?;
        let n = self.conn.execute(
            "DELETE FROM credentials WHERE provider = ?1 AND label = ?2",
            [provider, label],
        )?;
        Ok(n > 0)
    }

    /// All stored credentials, ordered by provider then rotation order.
    pub fn list(&self) -> Result<Vec<Credential>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, label, kind, token, expires_at, updated_at
             FROM credentials ORDER BY provider, label = 'default' DESC, label ASC",
        )?;
        let rows = stmt.query_map([], row_to_credential)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Quarantine dead credentials: drop every row whose `expires_at` has
    /// passed (e.g. refresh tokens that can no longer be renewed).
    /// Returns the number of quarantined rows.
    pub fn quarantine_expired(&self) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM credentials WHERE expires_at IS NOT NULL AND expires_at < ?1",
            [now()],
        )?;
        Ok(n)
    }
}

/// Additive migration to the v2 shape: a v1 table (`provider` as primary key,
/// no `label`) is rebuilt with `(provider, label)` and every existing row
/// becomes its provider's [`DEFAULT_LABEL`] account. Nothing is dropped.
fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    if !table_exists(conn, "credentials")? {
        conn.execute_batch(SCHEMA)?;
        return Ok(());
    }
    if column_exists(conn, "credentials", "label")? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         ALTER TABLE credentials RENAME TO credentials_v1;
         {SCHEMA}
         INSERT INTO credentials (provider, label, kind, token, expires_at, updated_at)
            SELECT provider, '{DEFAULT_LABEL}', kind, token, expires_at, updated_at
            FROM credentials_v1;
         DROP TABLE credentials_v1;
         COMMIT;"
    ))?;
    Ok(())
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> Result<bool> {
    // `table` is a crate-internal literal, never caller input.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn row_to_credential(row: &rusqlite::Row<'_>) -> rusqlite::Result<Credential> {
    Ok(Credential {
        provider: row.get(0)?,
        label: row.get(1)?,
        kind: row.get(2)?,
        token: row.get(3)?,
        expires_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp(tag: &str) -> (tempfile::TempDir, AuthStore) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir failed: {e}"));
        let store =
            AuthStore::open(&dir.path().join(tag)).unwrap_or_else(|e| panic!("open failed: {e}"));
        (dir, store)
    }

    #[test]
    fn two_labels_live_side_by_side_under_one_provider() {
        let (_dir, store) = open_tmp("auth.db");
        store
            .store_account("anthropic", "work", "api_key", "sk-test-work", None)
            .unwrap_or_else(|e| panic!("store work: {e}"));
        store
            .store_account("anthropic", "personal", "api_key", "sk-test-personal", None)
            .unwrap_or_else(|e| panic!("store personal: {e}"));

        let accounts = store
            .accounts("anthropic")
            .unwrap_or_else(|e| panic!("accounts: {e}"));
        assert_eq!(
            accounts.iter().map(|c| c.account_id()).collect::<Vec<_>>(),
            vec!["anthropic/personal".to_owned(), "anthropic/work".to_owned()]
        );
        assert_eq!(
            store
                .get_account("anthropic", "work")
                .unwrap_or_else(|e| panic!("get_account: {e}"))
                .map(|c| c.token),
            Some("sk-test-work".to_owned())
        );

        // Upserting one account leaves the other untouched.
        store
            .store_account("anthropic", "work", "api_key", "sk-test-work-2", None)
            .unwrap_or_else(|e| panic!("re-store work: {e}"));
        assert_eq!(
            store
                .get_account("anthropic", "personal")
                .unwrap_or_else(|e| panic!("get_account: {e}"))
                .map(|c| c.token),
            Some("sk-test-personal".to_owned())
        );
        assert_eq!(
            store
                .accounts("anthropic")
                .unwrap_or_else(|e| panic!("accounts: {e}"))
                .len(),
            2
        );

        // Removing one account keeps the other; removing the provider clears both.
        assert!(
            store
                .remove_account("anthropic", "work")
                .unwrap_or_else(|e| panic!("remove_account: {e}"))
        );
        assert!(
            !store
                .remove_account("anthropic", "work")
                .unwrap_or_else(|e| panic!("remove_account: {e}"))
        );
        assert_eq!(
            store
                .account_labels("anthropic")
                .unwrap_or_else(|e| panic!("labels: {e}")),
            vec!["personal".to_owned()]
        );
        assert!(
            store
                .remove("anthropic")
                .unwrap_or_else(|e| panic!("remove: {e}"))
        );
        assert!(
            store
                .accounts("anthropic")
                .unwrap_or_else(|e| panic!("accounts: {e}"))
                .is_empty()
        );
    }

    #[test]
    fn single_account_provider_keeps_the_default_label() {
        let (_dir, store) = open_tmp("auth.db");
        store
            .store("openai", "api_key", "sk-test-only", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        let accounts = store
            .accounts("openai")
            .unwrap_or_else(|e| panic!("accounts: {e}"));
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, DEFAULT_LABEL);
        assert_eq!(
            store
                .get("openai")
                .unwrap_or_else(|e| panic!("get: {e}"))
                .map(|c| c.token),
            Some("sk-test-only".to_owned())
        );
    }

    #[test]
    fn get_prefers_the_default_account() {
        let (_dir, store) = open_tmp("auth.db");
        store
            .store_account("anthropic", "work", "api_key", "sk-test-work", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        store
            .store("anthropic", "api_key", "sk-test-default", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        let primary = store
            .get("anthropic")
            .unwrap_or_else(|e| panic!("get: {e}"))
            .unwrap_or_else(|| panic!("expected a primary account"));
        assert_eq!(primary.label, DEFAULT_LABEL);
        assert_eq!(primary.token, "sk-test-default");
    }

    #[test]
    fn legacy_v1_database_migrates_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir failed: {e}"));
        let path = dir.path().join("auth.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap_or_else(|e| panic!("open: {e}"));
            conn.execute_batch(
                "CREATE TABLE credentials (
                    provider   TEXT PRIMARY KEY,
                    kind       TEXT NOT NULL,
                    token      TEXT NOT NULL,
                    expires_at INTEGER,
                    updated_at INTEGER NOT NULL
                );
                 INSERT INTO credentials VALUES ('anthropic', 'api_key', 'sk-test-old', NULL, 42);
                 INSERT INTO credentials VALUES ('openai', 'oauth', 'sk-test-oauth', 99, 43);",
            )
            .unwrap_or_else(|e| panic!("seed v1: {e}"));
        }

        let store = AuthStore::open(&path).unwrap_or_else(|e| panic!("migrate: {e}"));
        let migrated = store
            .get("anthropic")
            .unwrap_or_else(|e| panic!("get: {e}"))
            .unwrap_or_else(|| panic!("row lost in migration"));
        assert_eq!(migrated.label, DEFAULT_LABEL);
        assert_eq!(migrated.token, "sk-test-old");
        assert_eq!(migrated.updated_at, 42);
        let other = store
            .get("openai")
            .unwrap_or_else(|e| panic!("get: {e}"))
            .unwrap_or_else(|| panic!("row lost in migration"));
        assert_eq!(other.kind, "oauth");
        assert_eq!(other.expires_at, Some(99));

        // The migrated database now takes a second account.
        store
            .store_account("anthropic", "work", "api_key", "sk-test-work", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        assert_eq!(
            store
                .account_labels("anthropic")
                .unwrap_or_else(|e| panic!("labels: {e}")),
            vec!["default".to_owned(), "work".to_owned()]
        );

        // Re-opening an already migrated database is a no-op.
        drop(store);
        let store = AuthStore::open(&path).unwrap_or_else(|e| panic!("reopen: {e}"));
        assert_eq!(
            store
                .accounts("anthropic")
                .unwrap_or_else(|e| panic!("accounts: {e}"))
                .len(),
            2
        );
    }

    #[test]
    fn illegal_labels_are_rejected_and_write_nothing() {
        let (_dir, store) = open_tmp("auth.db");
        for label in ["", "with/slash", "with space", "tab\t"] {
            let err = store
                .store_account("anthropic", label, "api_key", "sk-test", None)
                .expect_err("label must be rejected");
            assert!(matches!(err, Error::InvalidLabel(_)), "got {err:?}");
            assert!(!err.to_string().contains("sk-test"), "error leaked a token");
        }
        assert!(
            store
                .accounts("anthropic")
                .unwrap_or_else(|e| panic!("accounts: {e}"))
                .is_empty()
        );
        let long = "x".repeat(65);
        assert!(
            store
                .store_account("anthropic", &long, "api_key", "sk-test", None)
                .is_err()
        );
    }

    #[test]
    fn no_display_path_renders_a_full_token() {
        let (_dir, store) = open_tmp("auth.db");
        store
            .store_account("anthropic", "work", "api_key", "sk-test-secret-1234", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        let cred = store
            .get_account("anthropic", "work")
            .unwrap_or_else(|e| panic!("get: {e}"))
            .unwrap_or_else(|| panic!("expected an account"));

        let debug = format!("{cred:?}");
        assert!(
            !debug.contains("sk-test-secret-1234"),
            "Debug leaked the token: {debug}"
        );
        assert!(
            debug.contains("anthropic"),
            "label context missing: {debug}"
        );
        assert!(debug.contains("work"), "label context missing: {debug}");
        assert!(debug.contains("1234"), "last-4 missing: {debug}");
        assert_eq!(cred.masked_token(), "…1234");
        assert_eq!(cred.account_id(), "anthropic/work");

        // Short secrets are hidden entirely, never partially.
        assert_eq!(mask_secret("abcd"), "…");
        assert_eq!(mask_secret(""), "…");
        assert_eq!(mask_secret("abcde"), "…bcde");

        // The label listing never loads a token at all.
        let labels = store
            .account_labels("anthropic")
            .unwrap_or_else(|e| panic!("labels: {e}"));
        assert!(!format!("{labels:?}").contains("sk-test"));
    }

    #[test]
    fn store_get_remove_roundtrip() {
        let (_dir, store) = open_tmp("auth.db");
        assert_eq!(
            store
                .get("anthropic")
                .unwrap_or_else(|e| panic!("get: {e}")),
            None
        );

        store
            .store("anthropic", "oauth", "tok-1", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        let cred = store
            .get("anthropic")
            .unwrap_or_else(|e| panic!("get: {e}"));
        assert_eq!(cred.as_ref().map(|c| c.token.as_str()), Some("tok-1"));
        assert_eq!(cred.as_ref().map(|c| c.kind.as_str()), Some("oauth"));
        assert_eq!(cred.as_ref().and_then(|c| c.expires_at), None);
        assert!(cred.is_some_and(|c| c.updated_at > 0));

        // Upsert replaces token and expiry.
        store
            .store("anthropic", "api_key", "tok-2", Some(1_700_000_000))
            .unwrap_or_else(|e| panic!("store: {e}"));
        let cred = store
            .get("anthropic")
            .unwrap_or_else(|e| panic!("get: {e}"));
        assert_eq!(cred.as_ref().map(|c| c.token.as_str()), Some("tok-2"));
        assert_eq!(
            cred.as_ref().and_then(|c| c.expires_at),
            Some(1_700_000_000)
        );

        assert!(store.list().unwrap_or_else(|e| panic!("list: {e}")).len() == 1);
        assert!(
            store
                .remove("anthropic")
                .unwrap_or_else(|e| panic!("remove: {e}"))
        );
        assert!(
            !store
                .remove("anthropic")
                .unwrap_or_else(|e| panic!("remove: {e}"))
        );
        assert_eq!(
            store
                .get("anthropic")
                .unwrap_or_else(|e| panic!("get: {e}")),
            None
        );
    }

    #[test]
    fn list_and_remove_are_provider_scoped() {
        let (_dir, store) = open_tmp("auth.db");
        store
            .store("a", "api_key", "ta", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        store
            .store("b", "oauth", "tb", None)
            .unwrap_or_else(|e| panic!("store: {e}"));
        assert!(store.remove("a").unwrap_or_else(|e| panic!("remove: {e}")));
        let listed = store.list().unwrap_or_else(|e| panic!("list: {e}"));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].provider, "b");
    }

    #[test]
    fn db_file_is_owner_only_0600() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir failed: {e}"));
        let path = dir.path().join("auth.db");
        AuthStore::open(&path).unwrap_or_else(|e| panic!("open failed: {e}"));
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("stat: {e}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "db file must be 0600, got {mode:o}");
    }

    #[test]
    fn reopening_existing_db_preserves_rows_and_fixes_mode() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir failed: {e}"));
        let path = dir.path().join("auth.db");
        {
            let store = AuthStore::open(&path).unwrap_or_else(|e| panic!("open: {e}"));
            store
                .store("openai", "api_key", "k", None)
                .unwrap_or_else(|e| panic!("store: {e}"));
        }
        // Simulate a wrong-mode pre-existing file, then reopen: mode is repaired.
        let mut perms = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("stat: {e}"))
            .permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap_or_else(|e| panic!("chmod: {e}"));
        let store = AuthStore::open(&path).unwrap_or_else(|e| panic!("reopen: {e}"));
        assert_eq!(
            store
                .get("openai")
                .unwrap_or_else(|e| panic!("get: {e}"))
                .map(|c| c.token),
            Some("k".to_string())
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("stat: {e}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn quarantine_expired_drops_only_dead_rows() {
        let (_dir, store) = open_tmp("auth.db");
        let now = now();
        store
            .store("dead-refresh", "oauth", "r1", Some(now - 100))
            .unwrap_or_else(|e| panic!("store: {e}"));
        store
            .store("live-refresh", "oauth", "r2", Some(now + 3_600))
            .unwrap_or_else(|e| panic!("store: {e}"));
        store
            .store("never-expires", "api_key", "k", None)
            .unwrap_or_else(|e| panic!("store: {e}"));

        assert_eq!(
            store
                .quarantine_expired()
                .unwrap_or_else(|e| panic!("quarantine: {e}")),
            1
        );
        assert!(
            !store
                .remove("dead-refresh")
                .unwrap_or_else(|e| panic!("remove: {e}"))
        );
        assert!(
            store
                .get("live-refresh")
                .unwrap_or_else(|e| panic!("get: {e}"))
                .is_some()
        );
        assert!(
            store
                .get("never-expires")
                .unwrap_or_else(|e| panic!("get: {e}"))
                .is_some()
        );
        // Idempotent: nothing left to quarantine.
        assert_eq!(
            store
                .quarantine_expired()
                .unwrap_or_else(|e| panic!("quarantine: {e}")),
            0
        );
    }
}
