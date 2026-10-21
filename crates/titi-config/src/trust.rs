//! The workspace trust store: which code-bearing project definitions the user
//! has approved.
//!
//! A repository can ship things that run on the user's machine — hooks today,
//! MCP servers later — and a clone must not get to run them just by existing.
//! The consent lives in `<agent_dir>/trust.json`, outside every repository, as
//! one entry per *definition*: `{workspaces: {<canonical path>: {trusted_at,
//! items: {<kind>: {<sha256 of the canonical definition>: {trusted_at,
//! label}}}}}}`. Approving a workspace therefore approves nothing by itself,
//! and a definition that is edited — by the author, by a pull, by a model
//! that planted a change — hashes differently and is untrusted again until
//! someone reviews it.
//!
//! Only surfaces write this file, in answer to a person. Nothing reads a trust
//! decision out of a project file, and the `settings` tool refuses the keys
//! that would stand in for one. The file is created `0600`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Name of the store inside the agent directory.
pub const TRUST_FILE: &str = "trust.json";

/// `kind` of a command hook, the one code-bearing definition there is today.
pub const KIND_HOOK: &str = "hook";

/// `kind` of a repository's `verify.command`: a shell command the engine runs
/// when a turn that edited files ends.
pub const KIND_VERIFY: &str = "verify";

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("could not read {path}: {reason}")]
    Read { path: PathBuf, reason: String },
    /// A file that is not the store's shape is never guessed at: nothing in
    /// it counts as trusted, and the owner decides what to do with it.
    #[error("{path} is not a valid trust store: {reason}")]
    Corrupt { path: PathBuf, reason: String },
    #[error("could not write {path}: {reason}")]
    Write { path: PathBuf, reason: String },
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    workspaces: BTreeMap<String, WorkspaceTrust>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct WorkspaceTrust {
    #[serde(default)]
    trusted_at: u64,
    /// kind → definition hash → the record of the approval.
    #[serde(default)]
    items: BTreeMap<String, BTreeMap<String, ItemTrust>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ItemTrust {
    #[serde(default)]
    trusted_at: u64,
    /// What the person saw when approving, so the file is readable by hand.
    #[serde(default)]
    label: String,
}

/// The store, as it was when [`TrustStore::load`] read it.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    data: TrustFile,
}

impl TrustStore {
    /// Reads `<agent_dir>/trust.json`. A missing file is an empty store.
    pub fn load(agent_dir: &Path) -> Result<Self, TrustError> {
        let path = agent_dir.join(TRUST_FILE);
        let data = read_file(&path)?;
        Ok(Self { path, data })
    }

    /// A store with nothing in it, writing to the same place. What a surface
    /// falls back to after [`TrustError`]: nothing is trusted, and a decision
    /// the person makes next replaces the unreadable file.
    pub fn empty(agent_dir: &Path) -> Self {
        Self {
            path: agent_dir.join(TRUST_FILE),
            data: TrustFile::default(),
        }
    }

    /// Whether this exact definition was approved for this workspace.
    pub fn is_trusted(&self, workspace: &Path, kind: &str, hash: &str) -> bool {
        self.data
            .workspaces
            .get(&workspace_key(workspace))
            .and_then(|entry| entry.items.get(kind))
            .is_some_and(|items| items.contains_key(hash))
    }

    /// Records an approval and writes it through at once. The file is
    /// re-read under a lock first, so a second titi that approved something
    /// meanwhile is not overwritten.
    pub fn trust(
        &mut self,
        workspace: &Path,
        kind: &str,
        hash: &str,
        label: &str,
    ) -> Result<(), TrustError> {
        let key = workspace_key(workspace);
        let (kind, hash, label) = (kind.to_owned(), hash.to_owned(), label.to_owned());
        self.update(move |data| {
            let now = now_secs();
            let entry = data.workspaces.entry(key).or_default();
            if entry.trusted_at == 0 {
                entry.trusted_at = now;
            }
            entry.items.entry(kind).or_default().insert(
                hash,
                ItemTrust {
                    trusted_at: now,
                    label,
                },
            );
            true
        })
        .map(|_| ())
    }

    /// Withdraws an approval. `Ok(false)` when there was none.
    pub fn revoke(&mut self, workspace: &Path, kind: &str, hash: &str) -> Result<bool, TrustError> {
        let key = workspace_key(workspace);
        let (kind, hash) = (kind.to_owned(), hash.to_owned());
        self.update(move |data| {
            let Some(entry) = data.workspaces.get_mut(&key) else {
                return false;
            };
            let Some(items) = entry.items.get_mut(&kind) else {
                return false;
            };
            let removed = items.remove(&hash).is_some();
            if items.is_empty() {
                entry.items.remove(&kind);
            }
            if entry.items.is_empty() {
                data.workspaces.remove(&key);
            }
            removed
        })
    }

    fn update(&mut self, change: impl FnOnce(&mut TrustFile) -> bool) -> Result<bool, TrustError> {
        let path = self.path.clone();
        let write_error = |reason: String| TrustError::Write {
            path: path.clone(),
            reason,
        };
        let changed = crate::config_file::with_file_lock(&path, || {
            // A corrupt file is replaced by the decision: the person is
            // choosing now, and refusing would leave them unable to choose.
            let mut latest = read_file(&path).unwrap_or_default();
            let changed = change(&mut latest);
            if changed {
                write_private(&path, &serde_json::to_vec_pretty(&latest)?)?;
            }
            self.data = latest;
            Ok::<bool, LockedError>(changed)
        })
        .map_err(|error| write_error(error.to_string()))?;
        Ok(changed)
    }
}

/// What can go wrong inside the locked section; flattened into
/// [`TrustError::Write`] by the caller.
#[derive(Debug, thiserror::Error)]
enum LockedError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

fn read_file(path: &Path) -> Result<TrustFile, TrustError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustFile::default());
        }
        Err(error) => {
            return Err(TrustError::Read {
                path: path.to_owned(),
                reason: error.to_string(),
            });
        }
    };
    serde_json::from_str(&text).map_err(|error| TrustError::Corrupt {
        path: path.to_owned(),
        reason: error.to_string(),
    })
}

/// The key a workspace is stored under: its canonical path, so `./repo`,
/// `/tmp/../tmp/repo` and a symlink to it are one workspace.
fn workspace_key(workspace: &Path) -> String {
    fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_owned())
        .to_string_lossy()
        .into_owned()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Writes through a temporary file in the same directory and renames it, so a
/// reader never sees half a store; `0600` from the first byte.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// The sha-256 of `bytes`, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The sha-256 of a definition's canonical JSON, as lowercase hex.
///
/// Canonical means object keys sorted at every depth and no whitespace, so the
/// hash follows what the definition *says* and not how the file spelled it.
pub fn hash_definition(definition: &Value) -> String {
    let mut text = String::new();
    canonical(definition, &mut text);
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                if let Some(inner) = map.get(key) {
                    canonical(inner, out);
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, TrustStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::load(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn an_approved_definition_is_trusted_and_survives_a_reload() {
        let (agent, mut store) = store();
        let repo = tempfile::tempdir().unwrap();
        let hash = hash_definition(&json!({"command": "lint.sh"}));
        assert!(!store.is_trusted(repo.path(), KIND_HOOK, &hash));

        store
            .trust(repo.path(), KIND_HOOK, &hash, "lint.sh")
            .unwrap();
        assert!(store.is_trusted(repo.path(), KIND_HOOK, &hash));

        let again = TrustStore::load(agent.path()).unwrap();
        assert!(again.is_trusted(repo.path(), KIND_HOOK, &hash));
    }

    #[test]
    fn a_changed_definition_loses_trust() {
        let (_agent, mut store) = store();
        let repo = tempfile::tempdir().unwrap();
        let approved = hash_definition(&json!({"command": "lint.sh", "timeout": 30}));
        store
            .trust(repo.path(), KIND_HOOK, &approved, "lint.sh")
            .unwrap();

        let edited = hash_definition(&json!({"command": "lint.sh && curl evil", "timeout": 30}));
        assert_ne!(approved, edited);
        assert!(store.is_trusted(repo.path(), KIND_HOOK, &approved));
        assert!(!store.is_trusted(repo.path(), KIND_HOOK, &edited));
    }

    #[test]
    fn trust_is_per_workspace_and_per_kind() {
        let (_agent, mut store) = store();
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let hash = hash_definition(&json!({"command": "x"}));
        store.trust(one.path(), KIND_HOOK, &hash, "x").unwrap();
        assert!(!store.is_trusted(other.path(), KIND_HOOK, &hash));
        assert!(!store.is_trusted(one.path(), "mcp", &hash));
    }

    #[test]
    fn a_workspace_is_known_by_its_canonical_path() {
        let (_agent, mut store) = store();
        let repo = tempfile::tempdir().unwrap();
        let hash = hash_definition(&json!({"command": "x"}));
        store.trust(repo.path(), KIND_HOOK, &hash, "x").unwrap();
        let roundabout = repo
            .path()
            .join("..")
            .join(repo.path().file_name().unwrap());
        assert!(store.is_trusted(&roundabout, KIND_HOOK, &hash));
    }

    #[test]
    fn revoking_withdraws_the_approval() {
        let (agent, mut store) = store();
        let repo = tempfile::tempdir().unwrap();
        let hash = hash_definition(&json!({"command": "x"}));
        store.trust(repo.path(), KIND_HOOK, &hash, "x").unwrap();
        assert!(store.revoke(repo.path(), KIND_HOOK, &hash).unwrap());
        assert!(!store.revoke(repo.path(), KIND_HOOK, &hash).unwrap());
        assert!(!store.is_trusted(repo.path(), KIND_HOOK, &hash));
        let again = TrustStore::load(agent.path()).unwrap();
        assert!(!again.is_trusted(repo.path(), KIND_HOOK, &hash));
    }

    #[test]
    fn the_hash_ignores_key_order_and_spacing() {
        let one = hash_definition(&json!({"a": 1, "b": {"y": [1, 2], "x": null}}));
        let other: Value =
            serde_json::from_str(r#"{ "b": {"x": null, "y": [1,2]}, "a": 1 }"#).unwrap();
        assert_eq!(one, hash_definition(&other));
        assert_eq!(one.len(), 64);
        // Order inside an array is meaning.
        assert_ne!(
            hash_definition(&json!({"a": [1, 2]})),
            hash_definition(&json!({"a": [2, 1]}))
        );
    }

    #[test]
    fn a_corrupt_store_trusts_nothing_and_is_replaced_by_a_decision() {
        let agent = tempfile::tempdir().unwrap();
        fs::write(agent.path().join(TRUST_FILE), "{not json").unwrap();
        assert!(matches!(
            TrustStore::load(agent.path()),
            Err(TrustError::Corrupt { .. })
        ));
        let repo = tempfile::tempdir().unwrap();
        let hash = hash_definition(&json!({"command": "x"}));
        let mut store = TrustStore::empty(agent.path());
        assert!(!store.is_trusted(repo.path(), KIND_HOOK, &hash));
        store.trust(repo.path(), KIND_HOOK, &hash, "x").unwrap();
        assert!(
            TrustStore::load(agent.path())
                .unwrap()
                .is_trusted(repo.path(), KIND_HOOK, &hash)
        );
    }

    #[test]
    fn a_second_writer_is_not_overwritten() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let mut first = TrustStore::load(agent.path()).unwrap();
        let mut second = TrustStore::load(agent.path()).unwrap();
        first.trust(repo.path(), KIND_HOOK, "aaa", "a").unwrap();
        second.trust(repo.path(), KIND_HOOK, "bbb", "b").unwrap();
        let both = TrustStore::load(agent.path()).unwrap();
        assert!(both.is_trusted(repo.path(), KIND_HOOK, "aaa"));
        assert!(both.is_trusted(repo.path(), KIND_HOOK, "bbb"));
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (agent, mut store) = store();
        let repo = tempfile::tempdir().unwrap();
        store.trust(repo.path(), KIND_HOOK, "aaa", "a").unwrap();
        let mode = fs::metadata(agent.path().join(TRUST_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
