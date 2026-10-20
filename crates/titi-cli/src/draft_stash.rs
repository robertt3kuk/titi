//! Stashed drafts: the prompts put aside with alt+s, or saved when a long draft
//! was cleared, kept per project so alt+p brings them back — in this session or
//! the next.
//!
//! One small JSON file per workspace under `<agent dir>/draft-stash/`, newest
//! first, at most [`MAX_DRAFTS`] entries, written 0600 through a temp file and
//! a rename so a crash never leaves half a stash behind. Drafts are prompts and
//! can hold anything the user pasted, hence the mode.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The drafts one project keeps; the oldest fall off.
pub(crate) const MAX_DRAFTS: usize = 20;
/// A draft is worth saving when clearing it would cost real typing: two lines
/// or eighty characters.
const WORTH_LINES: usize = 2;
const WORTH_CHARS: usize = 80;

#[derive(Debug)]
pub(crate) enum StashError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Corrupt {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl fmt::Display for StashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StashError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            StashError::Corrupt { path, source } => {
                write!(f, "{} is not a readable stash: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for StashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StashError::Io { source, .. } => Some(source),
            StashError::Corrupt { source, .. } => Some(source),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StashFile {
    /// Newest first.
    drafts: Vec<String>,
}

/// Whether clearing `text` should save it first.
pub(crate) fn worth_stashing(text: &str) -> bool {
    text.lines().count() >= WORTH_LINES || text.chars().count() >= WORTH_CHARS
}

/// One project's stash.
pub(crate) struct DraftStash {
    path: PathBuf,
}

impl DraftStash {
    /// The stash of `workspace`, under `agent_dir`. Nothing is read or created
    /// until a method needs it.
    pub(crate) fn new(agent_dir: &Path, workspace: &Path) -> Self {
        Self {
            path: agent_dir
                .join("draft-stash")
                .join(format!("{}.json", project_key(workspace))),
        }
    }

    fn read(&self) -> Result<StashFile, StashError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| StashError::Corrupt {
                path: self.path.clone(),
                source,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StashFile::default()),
            Err(source) => Err(StashError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn write(&self, mut file: StashFile) -> Result<(), StashError> {
        file.drafts.truncate(MAX_DRAFTS);
        let io_error = |source| StashError::Io {
            path: self.path.clone(),
            source,
        };
        let bytes = serde_json::to_vec(&file).map_err(|source| StashError::Corrupt {
            path: self.path.clone(),
            source,
        })?;
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir).map_err(io_error)?;
        }
        let temp = self.path.with_extension("json.tmp");
        write_private(&temp, &bytes).map_err(io_error)?;
        std::fs::rename(&temp, &self.path).map_err(io_error)
    }

    /// The drafts, newest first.
    #[cfg(test)]
    pub(crate) fn list(&self) -> Result<Vec<String>, StashError> {
        Ok(self.read()?.drafts)
    }

    /// Put `text` on top. An empty draft is not worth a slot.
    pub(crate) fn push(&self, text: &str) -> Result<(), StashError> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let mut file = self.read()?;
        file.drafts.insert(0, text.to_owned());
        self.write(file)
    }

    /// Take the newest draft out, and — in the same write — put `current`
    /// where it was, so restoring over a draft never loses that one. `None`
    /// when the stash is empty (and `current` is then not stored: nothing was
    /// restored, so nothing is swapped).
    pub(crate) fn take_newest(&self, current: Option<&str>) -> Result<Option<String>, StashError> {
        let mut file = self.read()?;
        if file.drafts.is_empty() {
            return Ok(None);
        }
        let newest = file.drafts.remove(0);
        if let Some(current) = current.filter(|text| !text.trim().is_empty()) {
            file.drafts.insert(0, current.to_owned());
        }
        self.write(file)?;
        Ok(Some(newest))
    }
}

/// A directory name for a workspace: its path made file-name safe, plus a hash
/// of the exact path so two spellings that sanitize alike stay apart. FNV-1a
/// by hand: `DefaultHasher` is not stable between releases, and this name must
/// outlive an upgrade.
fn project_key(workspace: &Path) -> String {
    let text = workspace.to_string_lossy();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let slug: String = text
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let tail: String = slug
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{tail}-{hash:016x}")
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies to a file this call creates.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}
