//! JSONL persistence for append-only session trees.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::checkpoint::Checkpoint;
use super::entry::{self, Entry, Role};
use super::export::{self, ExportFormat};
use super::index::{SearchHit, SessionIndex};
use super::{SessionError, SessionMeta};

/// Replays persisted entries as provider messages, so a restored session
/// continues the conversation instead of starting blank — tool rounds
/// included, because a restart that drops them makes the model re-run work
/// it already did.
pub fn entries_to_messages(entries: &[Entry]) -> Vec<titi_providers::ChatMessage> {
    entries
        .iter()
        .map(|entry| titi_providers::ChatMessage {
            role: match entry.role {
                Role::User => titi_providers::Role::User,
                Role::Assistant => titi_providers::Role::Assistant,
                Role::System => titi_providers::Role::System,
                Role::Tool => titi_providers::Role::Tool,
            },
            content: entry.content.clone().into(),
            tool_calls: entry.tool_calls.clone(),
        })
        .collect()
}

/// Filesystem store: one JSONL file per session under `<agent_dir>/sessions`,
/// a per-session leaf pointer, and the SQLite/FTS5 index at
/// `<agent_dir>/state.db`. The agent directory is injected — no env lookups —
/// so tests point it at a `tempfile::TempDir`. The one thing read from the
/// environment is the working directory recorded as a new session's
/// workspace; a test that cares about it says so in `SessionMeta::cwd`, which
/// wins.
pub struct SessionStore {
    dir: PathBuf,
    index: SessionIndex,
}

impl SessionStore {
    /// Opens the store rooted at `<agent_dir>/sessions`.
    pub fn new(agent_dir: &Path) -> Result<Self, SessionError> {
        let dir = agent_dir.join("sessions");
        fs::create_dir_all(&dir).map_err(SessionError::Io)?;
        let index = SessionIndex::open(&agent_dir.join("state.db"))?;
        Ok(Self { dir, index })
    }

    /// Creates an empty session and returns its id.
    ///
    /// `meta.cwd` is the workspace the session is started in. A caller that
    /// does not name one still gets one recorded — the process working
    /// directory, which is the directory the CLI calls the workspace
    /// (`session_fs::current_workspace`) — because a session that recorded
    /// nothing is unreachable by a workspace-scoped resume for no reason, and
    /// the live creation path passes a default `SessionMeta`. Nothing is
    /// recorded only when the working directory cannot be read or is not
    /// valid UTF-8, which is also what a session written before the field
    /// existed reports: `None` means "unknown workspace", never "not in
    /// yours".
    pub fn create(&self, meta: SessionMeta) -> Result<String, SessionError> {
        let meta = SessionMeta {
            cwd: meta.cwd.or_else(working_directory),
            ..meta
        };
        let id = entry::new_id();
        File::create_new(self.session_file(&id)).map_err(SessionError::Io)?;
        self.index.insert_session(&id, entry::now_ms(), &meta)?;
        Ok(id)
    }

    /// Loads every parseable entry of a session, oldest first.
    ///
    /// A torn final line left by a crash is skipped; a line that does not
    /// parse and is *not* last is [`SessionError::Corrupt`]. See `load`.
    pub fn open(&self, session_id: &str) -> Result<Vec<Entry>, SessionError> {
        self.load(session_id)
    }

    /// Appends an entry as a child of the current leaf; the leaf moves to it.
    pub fn append(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
    ) -> Result<Entry, SessionError> {
        self.append_entry(session_id, Entry::new(None, role, content))
    }

    /// Appends an assistant entry that issued `tool_calls`.
    ///
    /// Without the calls the following tool results are orphans, and a
    /// request carrying an orphan result is rejected by the provider.
    pub fn append_with_tool_calls(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
        tool_calls: Vec<titi_providers::ToolCallRef>,
    ) -> Result<Entry, SessionError> {
        self.append_entry(
            session_id,
            Entry::new(None, role, content).with_tool_calls(tool_calls),
        )
    }

    fn append_entry(&self, session_id: &str, entry: Entry) -> Result<Entry, SessionError> {
        if !self.session_file(session_id).exists() {
            return Err(SessionError::NotFound(session_id.into()));
        }
        let e = Entry {
            parent_id: self.current_leaf(session_id)?,
            ..entry
        };
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.session_file(session_id))
            .map_err(SessionError::Io)?;
        // A torn line from the previous crash goes before the new one, so the
        // fragment cannot swallow this append.
        truncate_torn_tail(&mut file).map_err(SessionError::Io)?;
        let line = serde_json::to_string(&e).map_err(SessionError::Json)?;
        writeln!(file, "{line}").map_err(SessionError::Io)?;
        // The entry has to be on the platter before the leaf moves to it, or
        // a crash between the two leaves the leaf naming an entry the file
        // never got, and the next append chains onto a missing parent.
        file.sync_all().map_err(SessionError::Io)?;
        self.set_leaf(session_id, &e.id)?;
        self.index.index_entry(session_id, &e)?;
        Ok(e)
    }

    /// Returns one entry by id.
    pub fn entry(&self, session_id: &str, entry_id: &str) -> Result<Option<Entry>, SessionError> {
        Ok(self
            .load(session_id)?
            .into_iter()
            .find(|e| e.id == entry_id))
    }

    /// Moves the leaf pointer to `from_entry_id` without touching history.
    /// The next [`append`](Self::append) becomes a child of that entry.
    pub fn fork(&self, session_id: &str, from_entry_id: &str) -> Result<(), SessionError> {
        if self.entry(session_id, from_entry_id)?.is_none() {
            return Err(SessionError::NotFound(format!(
                "{session_id}/{from_entry_id}"
            )));
        }
        self.set_leaf(session_id, from_entry_id)
    }

    /// Path from the root to `leaf` (or the current leaf when `None`),
    /// oldest first. Cycle-safe: each id is visited at most once.
    pub fn walk(&self, session_id: &str, leaf: Option<&str>) -> Result<Vec<Entry>, SessionError> {
        let entries = self.load(session_id)?;
        let by_id: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
        let mut cur = match leaf {
            Some(id) => Some(id.to_string()),
            None => self.current_leaf(session_id)?,
        };
        let mut chain = Vec::new();
        let mut visited = HashSet::new();
        while let Some(id) = cur {
            if !visited.insert(id.clone()) {
                break;
            }
            match by_id.get(id.as_str()) {
                Some(e) => {
                    chain.push((*e).clone());
                    cur = e.parent_id.clone();
                }
                None => break,
            }
        }
        chain.reverse();
        Ok(chain)
    }

    /// Id of the most recently created session, if any.
    pub fn resume_latest(&self) -> Result<Option<String>, SessionError> {
        self.index.resume_latest()
    }

    /// Resumes the most recent session: its id plus the conversation along the
    /// path to the current leaf. Abandoned fork branches are left out.
    pub fn restore_latest(&self) -> Result<Option<(String, Vec<Entry>)>, SessionError> {
        let Some(id) = self.resume_latest()? else {
            return Ok(None);
        };
        let conversation = self.walk(&id, None)?;
        Ok(Some((id, conversation)))
    }

    /// Records the current position as a rewind point and returns it.
    /// History is untouched; the checkpoint goes to a sidecar file.
    pub fn checkpoint(&self, session_id: &str) -> Result<Checkpoint, SessionError> {
        let entries = self.load(session_id)?.len();
        let checkpoint = Checkpoint {
            entry_id: self.current_leaf(session_id)?,
            entries,
            ts: entry::now_ms(),
            git_commit: None,
        };
        let mut file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(self.checkpoint_file(session_id))
            .map_err(SessionError::Io)?;
        truncate_torn_tail(&mut file).map_err(SessionError::Io)?;
        let line = serde_json::to_string(&checkpoint).map_err(SessionError::Json)?;
        writeln!(file, "{line}").map_err(SessionError::Io)?;
        // A checkpoint is the promise that a rewind point exists, and the git
        // commit is recorded against it right after: it cannot still be in the
        // page cache when the caller is told it was taken.
        file.sync_all().map_err(SessionError::Io)?;
        Ok(checkpoint)
    }

    /// Attach a git commit to the newest checkpoint.
    ///
    /// The commit is made after the checkpoint is written, because making it
    /// can fail (not a repo, nothing to commit). Rewriting the last line
    /// keeps the sidecar append-only apart from this one correction.
    pub fn record_git_commit(&self, session_id: &str, commit: &str) -> Result<(), SessionError> {
        let path = self.checkpoint_file(session_id);
        let text = std::fs::read_to_string(&path).map_err(SessionError::Io)?;
        let mut lines: Vec<&str> = text.lines().collect();
        let Some(last) = lines.pop() else {
            return Ok(());
        };
        let mut checkpoint: Checkpoint = serde_json::from_str(last).map_err(SessionError::Json)?;
        checkpoint.git_commit = Some(commit.to_owned());
        let mut rewritten = lines.join("\n");
        if !rewritten.is_empty() {
            rewritten.push('\n');
        }
        let line = serde_json::to_string(&checkpoint).map_err(SessionError::Json)?;
        rewritten.push_str(&line);
        rewritten.push('\n');
        write_atomic(&path, rewritten.as_bytes())
    }

    /// Checkpoints recorded for a session, oldest first.
    ///
    /// Same contract as the session file: a torn final line (a crash between
    /// the append and its sync) is skipped, damage in the middle is named.
    pub fn checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, SessionError> {
        let file = self.checkpoint_file(session_id);
        if !file.exists() {
            return Ok(Vec::new());
        }
        parse_lines(&read_lines(&file)?)
    }

    /// Rewinds a session to `checkpoint`: the session file keeps its first
    /// `entries` lines, the leaf moves back, and that checkpoint plus every
    /// later one is dropped. The FTS index is rebuilt for the shortened tree.
    pub fn rewind(&self, session_id: &str, checkpoint: &Checkpoint) -> Result<(), SessionError> {
        let entries = self.load(session_id)?;
        if checkpoint.entries > entries.len() {
            return Err(SessionError::NotFound(format!(
                "{session_id}@{} entries",
                checkpoint.entries
            )));
        }
        if let Some(id) = &checkpoint.entry_id
            && !entries.iter().any(|entry| &entry.id == id)
        {
            return Err(SessionError::NotFound(format!("{session_id}/{id}")));
        }

        let kept = &entries[..checkpoint.entries];
        let mut body = String::new();
        for entry in kept {
            body.push_str(&serde_json::to_string(entry).map_err(SessionError::Json)?);
            body.push('\n');
        }
        write_atomic(&self.session_file(session_id), body.as_bytes())?;

        match &checkpoint.entry_id {
            Some(id) => self.set_leaf(session_id, id)?,
            None => {
                let _ = fs::remove_file(self.leaf_file(session_id));
            }
        }

        let all = self.checkpoints(session_id)?;
        let keep = all
            .iter()
            .position(|candidate| candidate == checkpoint)
            .unwrap_or(all.len());
        let mut body = String::new();
        for candidate in all.iter().take(keep) {
            body.push_str(&serde_json::to_string(candidate).map_err(SessionError::Json)?);
            body.push('\n');
        }
        write_atomic(&self.checkpoint_file(session_id), body.as_bytes())?;

        self.index.reindex_session(session_id, kept)?;
        Ok(())
    }

    /// Session ids, newest first, optionally only those started in
    /// `workspace`; see [`SessionIndex::sessions_in`] for what the filter
    /// keeps and what it deliberately leaves to the unfiltered listing.
    pub fn sessions_in(&self, workspace: Option<&str>) -> Result<Vec<String>, SessionError> {
        self.index.sessions_in(workspace)
    }

    /// Full-text search over indexed entries, optionally scoped to one bot
    /// and to one workspace. `None` leaves that dimension unfiltered, which
    /// is the only way an entry of a session with no recorded workspace can
    /// be found.
    pub fn search(
        &self,
        query: &str,
        bot_id: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Vec<SearchHit>, SessionError> {
        self.index.search(query, bot_id, workspace)
    }

    /// Catalog metadata recorded for a session.
    pub fn session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, SessionError> {
        self.index.session_meta(session_id)
    }

    /// Copies a session's whole conversation into a fresh session and returns
    /// its id.
    ///
    /// The copy is independent: appending to either side leaves the other
    /// exactly as it was. That is the difference from [`fork`](Self::fork),
    /// which only moves one session's leaf pointer.
    pub fn fork_session(&self, source_id: &str, meta: SessionMeta) -> Result<String, SessionError> {
        let entries = self.walk(source_id, None)?;
        self.seed_session(source_id, &entries, meta, "fork")
    }

    /// Same as [`fork_session`](Self::fork_session), but seeded only with the
    /// conversation up to `checkpoint` — everything the session said after
    /// that rewind point is left behind.
    pub fn branch_session(
        &self,
        source_id: &str,
        checkpoint: &Checkpoint,
        meta: SessionMeta,
    ) -> Result<String, SessionError> {
        let entries = match &checkpoint.entry_id {
            Some(entry_id) => {
                if self.entry(source_id, entry_id)?.is_none() {
                    return Err(SessionError::NotFound(format!("{source_id}/{entry_id}")));
                }
                self.walk(source_id, Some(entry_id))?
            }
            // A checkpoint taken on an empty session branches to an empty
            // session, but the source still has to exist.
            None => {
                self.load(source_id)?;
                Vec::new()
            }
        };
        self.seed_session(source_id, &entries, meta, "branch")
    }

    /// Creates a session carrying `entries` as its own history.
    ///
    /// Ids are fresh so the two sessions never name the same entry, while
    /// timestamps are kept: a copy should read like the conversation it came
    /// from, not like it all happened at the moment of copying.
    fn seed_session(
        &self,
        source_id: &str,
        entries: &[Entry],
        meta: SessionMeta,
        kind: &str,
    ) -> Result<String, SessionError> {
        let inherited = self.session_meta(source_id)?.unwrap_or_default();
        let meta = SessionMeta {
            title: meta.title.or(inherited.title),
            bot_id: meta.bot_id.or(inherited.bot_id),
            source: Some(meta.source.unwrap_or_else(|| format!("{kind}:{source_id}"))),
            // A copy is still about the workspace the conversation happened
            // in, so a fork taken from another directory keeps searching and
            // resuming there. A source that never recorded one leaves this
            // unset, and `create` records the directory the copy is made in.
            cwd: meta.cwd.or(inherited.cwd),
        };
        let new_id = self.create(meta)?;
        for entry in entries {
            self.append_entry(
                &new_id,
                Entry {
                    id: entry::new_id(),
                    parent_id: None,
                    role: entry.role,
                    content: entry.content.clone(),
                    ts: entry.ts,
                    tool_calls: entry.tool_calls.clone(),
                },
            )?;
        }
        Ok(new_id)
    }

    /// Renders the session's current conversation in `format`, titled with
    /// whatever the catalog knows the session as.
    pub fn export(&self, session_id: &str, format: ExportFormat) -> Result<String, SessionError> {
        let entries = self.walk(session_id, None)?;
        let title = self.index.title(session_id)?;
        export::render(format, title.as_deref(), &entries)
    }

    /// Writes [`export`](Self::export) to `path`, creating its directory.
    ///
    /// The write is [`write_atomic`], so an export that lands on a file
    /// somebody already has open leaves them the *old* file, complete, rather
    /// than a truncated one — and a crash mid-export leaves the old file
    /// rather than half of the new one.
    pub fn export_to_file(
        &self,
        session_id: &str,
        format: ExportFormat,
        path: &Path,
    ) -> Result<(), SessionError> {
        let rendered = self.export(session_id, format)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(SessionError::Io)?;
        }
        write_atomic(
            &export_target(path).map_err(SessionError::Io)?,
            rendered.as_bytes(),
        )
    }

    fn session_file(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.jsonl"))
    }

    fn leaf_file(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.leaf"))
    }

    fn checkpoint_file(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.checkpoints.jsonl"))
    }

    fn current_leaf(&self, session_id: &str) -> Result<Option<String>, SessionError> {
        match fs::read_to_string(self.leaf_file(session_id)) {
            Ok(s) => {
                let t = s.trim();
                Ok((!t.is_empty()).then(|| t.to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SessionError::Io(e)),
        }
    }

    fn set_leaf(&self, session_id: &str, entry_id: &str) -> Result<(), SessionError> {
        write_atomic(&self.leaf_file(session_id), entry_id.as_bytes())
    }

    /// Reads a session's entries, oldest first, under [`parse_lines`]'
    /// contract: a torn final line is skipped, corruption elsewhere is named.
    fn load(&self, session_id: &str) -> Result<Vec<Entry>, SessionError> {
        let file = self.session_file(session_id);
        if !file.exists() {
            return Err(SessionError::NotFound(session_id.into()));
        }
        parse_lines(&read_lines(&file)?)
    }
}

/// The process working directory, as a workspace key, when it can be read.
///
/// Recorded for a session whose creator named no workspace. It is the same
/// directory the CLI pins a checkpoint to and rewinds into, so it is what
/// "this project" means to the resume paths; but it is read here rather than
/// passed in because the live creation path (`engine::launch_session`) builds
/// its metadata from `Default` and a session that records nothing would be
/// unreachable by a workspace-scoped resume.
fn working_directory() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|dir| dir.to_string_lossy().into_owned())
}

/// Every line of a JSONL file, in order.
fn read_lines(path: &Path) -> Result<Vec<String>, SessionError> {
    let file = File::open(path).map_err(SessionError::Io)?;
    BufReader::new(file)
        .lines()
        .collect::<std::result::Result<_, _>>()
        .map_err(SessionError::Io)
}

/// Parses JSONL lines under the store's reader contract.
///
/// A line that does not parse is tolerated only when it is the *last*
/// non-empty line, which is all a crash mid-append can tear; anything earlier
/// comes back as [`SessionError::Corrupt`] naming it. Reading on would turn a
/// damaged session into a silently shorter one — the entries after the damage
/// are indistinguishable from entries that were never written — and a caller
/// cannot offer to recover from a gap it was never told about.
fn parse_lines<T: serde::de::DeserializeOwned>(lines: &[String]) -> Result<Vec<T>, SessionError> {
    // 1-based index of the last line that holds anything, `None` for none.
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(value) => out.push(value),
            Err(_) if last == Some(index) => break,
            Err(source) => {
                return Err(SessionError::Corrupt {
                    line: index + 1,
                    source,
                });
            }
        }
    }
    Ok(out)
}

/// Drops a trailing fragment that was never closed with a newline.
///
/// Skipping a torn final line is only safe while nothing is written after it:
/// an append lands on the same physical line as the fragment, so the reader
/// sees one unparsable line where two entries should be and drops the second
/// with it — silently, until a further append turns the fragment into a
/// corrupt *middle* line the reader must refuse. The session files, the
/// checkpoint sidecar and the trajectory run this before every append, so what
/// is on disk goes back to being a sequence of whole lines. The check is one
/// read of the last byte; only a real tear pays for the walk back.
pub(crate) fn truncate_torn_tail(file: &mut File) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut byte)?;
    if byte[0] == b'\n' {
        return Ok(());
    }
    // Rare: only a crash between a write and its sync gets here. Walk back a
    // block at a time for the boundary the fragment hangs off, so the cost
    // stays proportional to the fragment.
    const BLOCK: u64 = 4096;
    let mut block = vec![0u8; BLOCK as usize];
    let mut end = len;
    loop {
        let start = end.saturating_sub(BLOCK);
        let want = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block[..want])?;
        match block[..want].iter().rposition(|byte| *byte == b'\n') {
            Some(at) => return file.set_len(start + at as u64 + 1),
            // The whole file is one unterminated line; nothing in it is a
            // complete entry.
            None if start == 0 => return file.set_len(0),
            None => end = start,
        }
    }
}

/// Replaces `path` with `bytes` in one step.
///
/// The bytes go to a sibling temp file that reaches the platter before it is
/// renamed over the target, so a reader sees the whole old file or the whole
/// new one. `fs::write` truncates first, and a crash — or a full disk — in
/// that window leaves a truncated session behind.
///
/// The temp file must be a *sibling*: `rename(2)` replaces atomically only
/// within one filesystem, and a temp under `std::env::temp_dir()` can be a
/// different mount, where the rename degrades into a copy that a crash can
/// catch halfway and leaves the target truncated after all.
/// Most symbolic links one path may be followed through before it is a loop.
///
/// The kernel's own limit; a chain that does not settle inside it is an error
/// rather than a write to whichever link happened to be eighth.
const MAX_SYMLINK_HOPS: u32 = 40;

/// Where an atomic write to `path` actually lands.
///
/// A symlink is followed by hand rather than with `canonicalize`, which needs
/// its target to exist — a link to a file that is not there yet is exactly the
/// case a first export hits. Following it is what keeps the write going where
/// the user pointed it (the link survives, the file behind it changes), and
/// resolving *before* the temp is made is what keeps the rename on the same
/// filesystem as the file it replaces.
fn export_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(meta) = fs::symlink_metadata(&target) else {
            // Nothing there: the write goes where it says, and the temp beside
            // it, which is what a new file needs.
            return Ok(target);
        };
        if !meta.file_type().is_symlink() {
            return Ok(target);
        }
        let link = fs::read_link(&target)?;
        target = if link.is_absolute() {
            link
        } else {
            target.parent().unwrap_or_else(|| Path::new(".")).join(link)
        };
    }
    Err(std::io::Error::new(
        // `ErrorKind::FilesystemLoop` is still unstable, so the kind is the
        // one an ordinary write would report and the message says which.
        std::io::ErrorKind::InvalidInput,
        format!("{}: too many levels of symbolic links", path.display()),
    ))
}

/// Writes `bytes` to `path` by writing a sibling and renaming it over the
/// target, so a reader never sees a half-written file: it reads the old one or
/// the new one, whichever the rename left it.
///
/// The temp is `sync_all`ed before the rename, because a rename is atomic but
/// not durable on its own — without the sync a crash can leave the *old* file
/// in place after this returned, which for an append is a message the user
/// watched being written and lost. The temp takes the target's permissions
/// when the target has any, so replacing a file the user restricted to `0600`
/// does not widen it back to the default the temp was created with.
///
/// Deliberately not done: `fsync`ing the parent directory after the rename.
/// That is what makes the *rename* durable, and it is also the call that some
/// network filesystems refuse with `EINVAL` — a write that fails on a working
/// filesystem is worse than one that a power cut can roll back.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    let tmp = tmp_path(path);
    let mut file = File::create(&tmp).map_err(SessionError::Io)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| keep_permissions(path, &tmp));
    drop(file);
    if let Err(e) = written {
        // The target is still the old file, so the half-written temp is only
        // litter and goes now rather than being renamed later.
        let _ = fs::remove_file(&tmp);
        return Err(SessionError::Io(e));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        // A rename either happened or did not, so the target is untouched and
        // the temp is litter: removed here rather than left beside a file the
        // caller believes was written.
        let _ = fs::remove_file(&tmp);
        return Err(SessionError::Io(e));
    }
    Ok(())
}

/// Gives `tmp` the permissions `path` already had, when it has any.
///
/// A file created for the temp carries the process default (`0644` under the
/// usual umask), so a replace would relax a target the user had tightened. A
/// target that does not exist yet keeps the default, which is what a new file
/// gets anyway.
fn keep_permissions(path: &Path, tmp: &Path) -> std::io::Result<()> {
    let Ok(existing) = fs::metadata(path) else {
        return Ok(());
    };
    fs::set_permissions(tmp, existing.permissions())
}

/// `<file>.tmp` beside `file`, so the rename stays inside one directory.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let store = SessionStore::new(dir.path()).unwrap_or_else(|e| panic!("new: {e}"));
        (dir, store)
    }

    fn meta(bot_id: &str) -> SessionMeta {
        SessionMeta {
            title: Some(format!("bot-{bot_id}")),
            bot_id: Some(bot_id.into()),
            source: Some("cli".into()),
            cwd: None,
        }
    }

    #[test]
    fn jsonl_roundtrip_preserves_entries() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "hello")
            .unwrap_or_else(|e| panic!("{e}"));
        let b = s
            .append(&sid, Role::Assistant, "world")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.parent_id.as_deref(), Some(a.id.as_str()));

        // Raw file: one JSON object per line, in append order.
        let raw = fs::read_to_string(s.session_file(&sid)).unwrap_or_else(|e| panic!("{e}"));
        let parsed: Vec<Entry> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("line: {e}")))
            .collect();
        assert_eq!(parsed, vec![a.clone(), b.clone()]);

        // Reopening the store (fresh index connection) replays the same tree.
        let reopened = SessionStore::new(_dir.path()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            reopened.open(&sid).unwrap_or_else(|e| panic!("{e}")),
            vec![a, b]
        );
    }

    #[test]
    fn fork_moves_leaf_without_rewriting_history() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "a")
            .unwrap_or_else(|e| panic!("{e}"));
        let b = s
            .append(&sid, Role::Assistant, "b")
            .unwrap_or_else(|e| panic!("{e}"));

        s.fork(&sid, &a.id).unwrap_or_else(|e| panic!("{e}"));
        let c = s
            .append(&sid, Role::User, "c")
            .unwrap_or_else(|e| panic!("{e}"));

        // New append is a child of the fork target, not of the old leaf.
        assert_eq!(c.parent_id.as_deref(), Some(a.id.as_str()));
        // Parent entries are untouched on disk.
        let all = s.open(&sid).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(all, vec![a.clone(), b.clone(), c]);
        assert_eq!(all[1].parent_id.as_deref(), Some(a.id.as_str()));
    }

    #[test]
    fn walk_after_fork_follows_new_leaf() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "a")
            .unwrap_or_else(|e| panic!("{e}"));
        let b = s
            .append(&sid, Role::Assistant, "b")
            .unwrap_or_else(|e| panic!("{e}"));

        // Before fork: full path to current leaf.
        assert_eq!(
            s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}")),
            vec![a.clone(), b.clone()]
        );

        s.fork(&sid, &a.id).unwrap_or_else(|e| panic!("{e}"));
        let c = s
            .append(&sid, Role::User, "c")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}")),
            vec![a.clone(), c.clone()]
        );
        // Explicit leaf still works, and b remains reachable.
        assert_eq!(
            s.walk(&sid, Some(&b.id)).unwrap_or_else(|e| panic!("{e}")),
            vec![a.clone(), b]
        );
        assert_eq!(
            s.walk(&sid, Some(&c.id)).unwrap_or_else(|e| panic!("{e}")),
            vec![a, c]
        );
    }

    #[test]
    fn fork_to_missing_entry_is_not_found() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let err = s.fork(&sid, "nope").unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[test]
    fn fts_finds_appended_content() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let e = s
            .append(&sid, Role::User, "deploy kafka cluster")
            .unwrap_or_else(|e| panic!("{e}"));
        let hits = s
            .search("kafka", None, None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry_id, e.id);
        assert_eq!(hits[0].session_id, sid);
    }

    #[test]
    fn bot_id_isolation_a_cannot_see_b() {
        let (_dir, s) = store();
        let sa = s.create(meta("bot-a")).unwrap_or_else(|e| panic!("{e}"));
        let sb = s.create(meta("bot-b")).unwrap_or_else(|e| panic!("{e}"));
        let ea = s
            .append(&sa, Role::User, "alpha secret plan")
            .unwrap_or_else(|e| panic!("{e}"));
        let eb = s
            .append(&sb, Role::User, "beta secret plan")
            .unwrap_or_else(|e| panic!("{e}"));

        let for_a = s
            .search("secret", Some("bot-a"), None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].entry_id, ea.id);
        assert_ne!(for_a[0].session_id, sb);

        let for_b = s
            .search("secret", Some("bot-b"), None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].entry_id, eb.id);

        assert_eq!(
            s.search("secret", None, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len(),
            2
        );
    }

    #[test]
    fn resume_latest_returns_last_created_session() {
        let (_dir, s) = store();
        assert_eq!(s.resume_latest().unwrap_or_else(|e| panic!("{e}")), None);
        let first = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let second = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.resume_latest().unwrap_or_else(|e| panic!("{e}")),
            Some(second.clone())
        );
        assert_ne!(first, second);
    }

    /// A session says where it was started. A named workspace is kept as it
    /// is; an unnamed one falls back to the process working directory, which
    /// is what makes `--continue` project-scoped without every creation site
    /// having to say so.
    #[test]
    fn create_records_the_workspace_it_is_given_or_runs_in() {
        let (_dir, s) = store();
        let named = s
            .create(SessionMeta {
                cwd: Some("/work/named".into()),
                ..meta("a")
            })
            .unwrap_or_else(|e| panic!("{e}"));
        let named = s
            .session_meta(&named)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("named was created"));
        assert_eq!(named.cwd.as_deref(), Some("/work/named"));

        let unnamed = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let unnamed = s
            .session_meta(&unnamed)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("unnamed was created"));
        let here = std::env::current_dir()
            .unwrap_or_else(|e| panic!("{e}"))
            .to_string_lossy()
            .into_owned();
        assert_eq!(unnamed.cwd.as_deref(), Some(here.as_str()));
    }

    /// A fork or branch of a session stays in that session's workspace even
    /// when it is taken from somewhere else: the copy is about the same tree.
    #[test]
    fn a_fork_keeps_the_workspace_of_the_session_it_copied() {
        let (_dir, s) = store();
        let source = s
            .create(SessionMeta {
                cwd: Some("/work/source".into()),
                ..meta("a")
            })
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&source, Role::User, "hello")
            .unwrap_or_else(|e| panic!("{e}"));

        let fork = s
            .fork_session(&source, meta("a"))
            .unwrap_or_else(|e| panic!("{e}"));
        let fork = s
            .session_meta(&fork)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("the fork was created"));
        assert_eq!(fork.cwd.as_deref(), Some("/work/source"));
    }

    #[test]
    fn torn_final_line_is_skipped_leniently() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "intact")
            .unwrap_or_else(|e| panic!("{e}"));
        // Simulate kill -9 mid-write: append a torn line without newline.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(s.session_file(&sid))
                .unwrap_or_else(|e| panic!("{e}"));
            write!(f, "{{\"id\":\"torn").unwrap_or_else(|e| panic!("{e}"));
        }
        let all = s.open(&sid).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(all, vec![a.clone()]);
        // The tree stays usable: append after recovery still works.
        let b = s
            .append(&sid, Role::Assistant, "after crash")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.parent_id.as_deref(), Some(a.id.as_str()));
    }

    /// Tolerating a torn tail is only safe while nothing lands after it: the
    /// fragment has no newline, so an append would join it and the reader would
    /// see one unparsable line where two entries should be — losing the new
    /// entry, then turning the fragment into a corrupt middle line.
    #[test]
    fn appending_after_a_torn_line_keeps_the_old_and_the_new_entry() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "intact")
            .unwrap_or_else(|e| panic!("{e}"));
        tear(&s.session_file(&sid));
        // Last line, so still tolerated.
        assert_eq!(
            s.open(&sid).unwrap_or_else(|e| panic!("{e}")),
            vec![a.clone()]
        );

        let b = s
            .append(&sid, Role::Assistant, "after the crash")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(b.parent_id.as_deref(), Some(a.id.as_str()));
        // Both entries survive a fresh read, and no error is raised: the
        // fragment was dropped rather than left to swallow the append.
        assert_eq!(s.open(&sid).unwrap_or_else(|e| panic!("{e}")), vec![a, b]);
    }

    /// The checkpoint sidecar is appended to the same way, so it gets the same
    /// repair, and a fragment that is *not* last is damage like any other.
    #[test]
    fn a_torn_checkpoint_line_is_dropped_before_the_next_append() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "one")
            .unwrap_or_else(|e| panic!("{e}"));
        let first = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        tear(&s.checkpoint_file(&sid));
        assert_eq!(
            s.checkpoints(&sid).unwrap_or_else(|e| panic!("{e}")),
            vec![first.clone()]
        );

        s.append(&sid, Role::Assistant, "two")
            .unwrap_or_else(|e| panic!("{e}"));
        let second = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.checkpoints(&sid).unwrap_or_else(|e| panic!("{e}")),
            vec![first, second]
        );
    }

    #[test]
    fn a_corrupt_checkpoint_line_before_the_end_is_an_error_naming_it() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "one")
            .unwrap_or_else(|e| panic!("{e}"));
        s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));

        let path = s.checkpoint_file(&sid);
        let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
        let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
        lines[0] = "{\"entries\":".to_owned();
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap_or_else(|e| panic!("{e}"));

        assert!(matches!(
            s.checkpoints(&sid),
            Err(SessionError::Corrupt { line: 1, .. })
        ));
    }

    /// Simulates a crash mid-append: a partial JSON line with no newline.
    fn tear(path: &std::path::Path) {
        let mut f = OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap_or_else(|e| panic!("{e}"));
        write!(f, "{{\"id\":\"torn").unwrap_or_else(|e| panic!("{e}"));
    }

    /// Distinguishable from the torn tail above: that one is not an error at
    /// all, this one names the damaged line instead of reading past it.
    #[test]
    fn a_corrupt_line_before_the_end_is_an_error_naming_it() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "intact")
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "second")
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "third")
            .unwrap_or_else(|e| panic!("{e}"));

        // Garbled second line, with a valid third line after it.
        let path = s.session_file(&sid);
        let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
        let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
        lines[1] = "{\"id\":\"garbled\"".to_owned();
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap_or_else(|e| panic!("{e}"));

        match s.open(&sid) {
            Err(SessionError::Corrupt { line, .. }) => assert_eq!(line, 2, "1-based line number"),
            other => panic!("expected a corrupt-line error, got {other:?}"),
        }
        // Every reader that replays the tree hits it, not just `open`.
        assert!(matches!(
            s.walk(&sid, None),
            Err(SessionError::Corrupt { line: 2, .. })
        ));
        assert!(matches!(
            s.entry(&sid, "anything"),
            Err(SessionError::Corrupt { line: 2, .. })
        ));
    }

    /// The write must not touch the target it is replacing until the
    /// replacement exists. An in-place `fs::write` would truncate the session
    /// first and then succeed despite the occupied staging name; this fails
    /// with the session still holding its previous bytes.
    #[test]
    fn a_rewrite_that_cannot_be_staged_leaves_the_session_untouched() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "keep me")
            .unwrap_or_else(|e| panic!("{e}"));
        let checkpoint = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "drop me")
            .unwrap_or_else(|e| panic!("{e}"));

        let path = s.session_file(&sid);
        let before = fs::read(&path).unwrap_or_else(|e| panic!("{e}"));
        // Litter a crash could leave: a directory where the staging file goes,
        // so even a fresh `File::create` cannot open it.
        let staging = tmp_path(&path);
        fs::create_dir(&staging).unwrap_or_else(|e| panic!("{e}"));

        let err = s.rewind(&sid, &checkpoint).unwrap_err();
        assert!(matches!(err, SessionError::Io(_)), "{err}");
        assert_eq!(
            fs::read(&path).unwrap_or_else(|e| panic!("{e}")),
            before,
            "a failed rewrite must leave the previous bytes in place"
        );

        // With the stage free the same rewind lands, so the failure was the
        // staging file and not the rewind itself.
        fs::remove_dir(&staging).unwrap_or_else(|e| panic!("{e}"));
        s.rewind(&sid, &checkpoint)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.open(&sid).unwrap_or_else(|e| panic!("{e}")), vec![a]);
    }

    /// Same invariant from the other side: a landing rewrite got there by
    /// replacing the file, not by overwriting it. An in-place truncate keeps
    /// the inode; the rename that follows a successful `sync_all` does not.
    #[cfg(unix)]
    #[test]
    fn a_rewrite_replaces_the_file_instead_of_truncating_it() {
        use std::os::unix::fs::MetadataExt;

        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "keep me")
            .unwrap_or_else(|e| panic!("{e}"));
        let checkpoint = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "drop me")
            .unwrap_or_else(|e| panic!("{e}"));

        let path = s.session_file(&sid);
        let ino = |p: &std::path::Path| fs::metadata(p).unwrap_or_else(|e| panic!("{e}")).ino();
        let before = ino(&path);

        s.rewind(&sid, &checkpoint)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(ino(&path), before, "the file was replaced, not overwritten");
        assert_eq!(s.open(&sid).unwrap_or_else(|e| panic!("{e}")), vec![a]);
        assert!(
            !tmp_path(&path).exists(),
            "the staging file is gone once its rename landed"
        );
    }

    #[test]
    fn rewind_keeps_entries_up_to_the_checkpoint() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "keep me")
            .unwrap_or_else(|e| panic!("{e}"));
        let checkpoint = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(checkpoint.entries, 1);
        assert_eq!(checkpoint.entry_id.as_deref(), Some(a.id.as_str()));

        s.append(&sid, Role::Assistant, "drop me")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.open(&sid).unwrap_or_else(|e| panic!("{e}")).len(), 2);

        s.rewind(&sid, &checkpoint)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.open(&sid).unwrap_or_else(|e| panic!("{e}")),
            vec![a.clone()]
        );
        // The leaf moved back, so the next append branches off the checkpoint.
        let c = s
            .append(&sid, Role::User, "after rewind")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.parent_id.as_deref(), Some(a.id.as_str()));
    }

    #[test]
    fn rewind_drops_that_checkpoint_and_later_ones() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "one")
            .unwrap_or_else(|e| panic!("{e}"));
        let first = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "two")
            .unwrap_or_else(|e| panic!("{e}"));
        let second = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.checkpoints(&sid).unwrap_or_else(|e| panic!("{e}")).len(),
            2
        );

        s.rewind(&sid, &first).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            s.checkpoints(&sid)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty()
        );

        // A checkpoint taken after the rewind is recorded again.
        let third = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(third, second);
        assert_eq!(
            s.checkpoints(&sid).unwrap_or_else(|e| panic!("{e}")).len(),
            1
        );
    }

    #[test]
    fn rewind_removes_entries_from_search() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "durable fact")
            .unwrap_or_else(|e| panic!("{e}"));
        let checkpoint = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "retracted fact")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.search("retracted", None, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len(),
            1
        );

        s.rewind(&sid, &checkpoint)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            s.search("retracted", None, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty(),
            "a rewound entry must not be searchable"
        );
        assert_eq!(
            s.search("durable", None, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len(),
            1
        );
    }

    #[test]
    fn rewind_to_an_unknown_point_is_not_found() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "only")
            .unwrap_or_else(|e| panic!("{e}"));

        let too_far = Checkpoint {
            entry_id: None,
            entries: 9,
            ts: 0,
            git_commit: None,
        };
        assert!(matches!(
            s.rewind(&sid, &too_far),
            Err(SessionError::NotFound(_))
        ));

        let wrong_leaf = Checkpoint {
            entry_id: Some("ghost".into()),
            entries: 1,
            ts: 0,
            git_commit: None,
        };
        assert!(matches!(
            s.rewind(&sid, &wrong_leaf),
            Err(SessionError::NotFound(_))
        ));
    }

    #[test]
    fn restore_latest_replays_the_active_conversation() {
        let (_dir, s) = store();
        assert_eq!(s.restore_latest().unwrap_or_else(|e| panic!("{e}")), None);

        let older = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&older, Role::User, "old question")
            .unwrap_or_else(|e| panic!("{e}"));

        let newest = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let q = s
            .append(&newest, Role::User, "new question")
            .unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&newest, Role::Assistant, "new answer")
            .unwrap_or_else(|e| panic!("{e}"));

        let (id, entries) = s
            .restore_latest()
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("a session to restore"));
        assert_eq!(id, newest);
        assert_eq!(entries, vec![q, a]);

        let messages = entries_to_messages(&entries);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, titi_providers::Role::User);
        assert_eq!(messages[0].content, "new question");
        assert_eq!(messages[1].role, titi_providers::Role::Assistant);
    }

    #[test]
    fn restore_latest_follows_the_fork_not_the_abandoned_branch() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let a = s
            .append(&sid, Role::User, "root")
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "abandoned")
            .unwrap_or_else(|e| panic!("{e}"));

        s.fork(&sid, &a.id).unwrap_or_else(|e| panic!("{e}"));
        let kept = s
            .append(&sid, Role::Assistant, "kept")
            .unwrap_or_else(|e| panic!("{e}"));

        let (_id, entries) = s
            .restore_latest()
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("a session to restore"));
        assert_eq!(entries, vec![a, kept]);
    }

    #[test]
    fn unknown_session_errors_on_append_and_open() {
        let (_dir, s) = store();
        assert!(matches!(
            s.append("ghost", Role::User, "x"),
            Err(SessionError::NotFound(_))
        ));
        assert!(matches!(s.open("ghost"), Err(SessionError::NotFound(_))));
    }

    #[test]
    fn tool_traffic_survives_the_file_and_old_lines_still_load() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        // A line written before tool traffic was persisted has no field.
        let legacy = "{\"id\":\"0\",\"parent_id\":null,\"role\":\"user\",\
                      \"content\":\"read Cargo.toml\",\"ts\":1}\n";
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(s.session_file(&sid))
                .unwrap_or_else(|e| panic!("{e}"));
            write!(file, "{legacy}").unwrap_or_else(|e| panic!("{e}"));
        }
        s.set_leaf(&sid, "0").unwrap_or_else(|e| panic!("{e}"));

        let call = titi_providers::ToolCallRef {
            call_id: "call-1".into(),
            name: "read".into(),
        };
        s.append_with_tool_calls(&sid, Role::Assistant, "", vec![call.clone()])
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Tool, "[package]")
            .unwrap_or_else(|e| panic!("{e}"));

        let entries = s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}"));
        let messages = entries_to_messages(&entries);
        assert_eq!(
            messages,
            vec![
                titi_providers::ChatMessage {
                    role: titi_providers::Role::User,
                    content: "read Cargo.toml".into(),
                    tool_calls: Vec::new(),
                },
                titi_providers::ChatMessage {
                    role: titi_providers::Role::Assistant,
                    content: "".into(),
                    tool_calls: vec![call],
                },
                titi_providers::ChatMessage {
                    role: titi_providers::Role::Tool,
                    content: "[package]".into(),
                    tool_calls: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn fork_session_copies_history_and_stays_independent() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let call = titi_providers::ToolCallRef {
            call_id: "call-1".into(),
            name: "read".into(),
        };
        s.append(&sid, Role::User, "read Cargo.toml")
            .unwrap_or_else(|e| panic!("{e}"));
        s.append_with_tool_calls(&sid, Role::Assistant, "", vec![call.clone()])
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Tool, "[package]")
            .unwrap_or_else(|e| panic!("{e}"));
        let before = s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}"));

        let forked = s
            .fork_session(&sid, SessionMeta::default())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(forked, sid);

        // Same conversation, including the tool round.
        let copy = s.walk(&forked, None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            copy.iter()
                .map(|e| (e.role, e.content.clone(), e.tool_calls.clone()))
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|e| (e.role, e.content.clone(), e.tool_calls.clone()))
                .collect::<Vec<_>>()
        );

        // Writing to the fork leaves the source exactly as it was.
        s.append(&forked, Role::User, "now read the lockfile")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}")), before);
        assert_eq!(s.open(&sid).unwrap_or_else(|e| panic!("{e}")), before);
        assert_eq!(
            s.walk(&forked, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len(),
            before.len() + 1
        );

        // And the source is unaffected by a write of its own afterwards.
        s.append(&sid, Role::User, "stay here")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            s.walk(&forked, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len(),
            before.len() + 1
        );

        // Metadata is inherited, provenance recorded.
        let inherited = s
            .session_meta(&forked)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("metadata for the fork"));
        assert_eq!(inherited.bot_id.as_deref(), Some("a"));
        assert_eq!(inherited.source, Some(format!("fork:{sid}")));
    }

    #[test]
    fn branch_session_keeps_only_what_came_before_the_checkpoint() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "one")
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "two")
            .unwrap_or_else(|e| panic!("{e}"));
        let checkpoint = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "three")
            .unwrap_or_else(|e| panic!("{e}"));

        let branched = s
            .branch_session(&sid, &checkpoint, SessionMeta::default())
            .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(
            s.walk(&branched, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .iter()
                .map(|e| e.content.clone())
                .collect::<Vec<_>>(),
            vec!["one".to_owned(), "two".to_owned()]
        );
        // The source still has everything it had.
        assert_eq!(
            s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}")).len(),
            3
        );
    }

    #[test]
    fn branch_from_an_empty_checkpoint_starts_blank() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let checkpoint = s.checkpoint(&sid).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "later")
            .unwrap_or_else(|e| panic!("{e}"));

        let branched = s
            .branch_session(&sid, &checkpoint, SessionMeta::default())
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            s.walk(&branched, None)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty()
        );
    }

    #[test]
    fn seeding_from_an_unknown_session_errors() {
        let (_dir, s) = store();
        assert!(matches!(
            s.fork_session("ghost", SessionMeta::default()),
            Err(SessionError::NotFound(_))
        ));
        let checkpoint = Checkpoint {
            entry_id: Some("nope".into()),
            entries: 1,
            ts: 0,
            git_commit: None,
        };
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            s.branch_session(&sid, &checkpoint, SessionMeta::default()),
            Err(SessionError::NotFound(_))
        ));
    }

    #[test]
    fn export_renders_the_live_conversation_in_both_formats() {
        let (_dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        let asked = s
            .append(&sid, Role::User, "hello")
            .unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "wrong turn")
            .unwrap_or_else(|e| panic!("{e}"));
        s.fork(&sid, &asked.id).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::Assistant, "hi there")
            .unwrap_or_else(|e| panic!("{e}"));

        let md = s
            .export(&sid, ExportFormat::Markdown)
            .unwrap_or_else(|e| panic!("{e}"));
        let user = md.find("hello").unwrap_or_else(|| panic!("{md}"));
        let answer = md.find("hi there").unwrap_or_else(|| panic!("{md}"));
        assert!(user < answer, "{md}");
        assert!(md.starts_with("# bot-a\n"), "{md}");
        assert!(!md.contains("wrong turn"), "{md}");

        let jsonl = s
            .export(&sid, ExportFormat::Jsonl)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(jsonl.lines().count(), 2);
        assert_eq!(
            export::from_jsonl(&jsonl).unwrap_or_else(|e| panic!("{e}")),
            s.walk(&sid, None).unwrap_or_else(|e| panic!("{e}"))
        );

        let path = _dir.path().join("out").join("chat.jsonl");
        s.export_to_file(&sid, ExportFormat::Jsonl, &path)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}")),
            jsonl
        );
    }

    /// An export **replaces** the file rather than truncating it in place: a
    /// reader that already has the export open keeps reading the file it
    /// opened, whole, instead of watching it emptied and refilled.
    ///
    /// The open handle is the observable form of that contract — a truncating
    /// write shows through it, a rename does not — and it is the same handle a
    /// `tail -f`, a pager or an editor would be holding.
    #[test]
    fn an_export_replaces_the_file_rather_than_truncating_it() {
        let (dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "the first question")
            .unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("chat.md");
        s.export_to_file(&sid, ExportFormat::Markdown, &path)
            .unwrap_or_else(|e| panic!("{e}"));

        let mut open = File::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let mut first = String::new();
        open.read_to_string(&mut first)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(first.contains("the first question"), "{first}");

        s.append(&sid, Role::Assistant, "the second answer")
            .unwrap_or_else(|e| panic!("{e}"));
        s.export_to_file(&sid, ExportFormat::Markdown, &path)
            .unwrap_or_else(|e| panic!("{e}"));

        // The handle still reads what it read: the file it opened was not the
        // one the new export went into.
        open.seek(SeekFrom::Start(0))
            .unwrap_or_else(|e| panic!("{e}"));
        let mut again = String::new();
        open.read_to_string(&mut again)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(again, first, "the reader's file was rewritten under it");
        // And the path itself holds the new export.
        let at_path = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(at_path.contains("the second answer"), "{at_path}");
    }

    /// An export to a path that is a symlink writes through the link, the way
    /// an ordinary write does, and leaves the link in place.
    #[test]
    fn an_export_writes_through_a_symlink() {
        let (dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "through the link")
            .unwrap_or_else(|e| panic!("{e}"));
        let real = dir.path().join("real.md");
        let link = dir.path().join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap_or_else(|e| panic!("{e}"));

        s.export_to_file(&sid, ExportFormat::Markdown, &link)
            .unwrap_or_else(|e| panic!("{e}"));

        assert!(
            fs::symlink_metadata(&link)
                .unwrap_or_else(|e| panic!("{e}"))
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );
        let written = fs::read_to_string(&real).unwrap_or_else(|e| panic!("{e}"));
        assert!(written.contains("through the link"), "{written}");
    }

    /// A replace keeps the permissions the file had: an export the user
    /// restricted to `0600` must not be widened back by the next one.
    #[test]
    fn an_export_keeps_the_permissions_the_file_had() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "private")
            .unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("chat.md");
        s.export_to_file(&sid, ExportFormat::Markdown, &path)
            .unwrap_or_else(|e| panic!("{e}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("{e}"));

        s.export_to_file(&sid, ExportFormat::Markdown, &path)
            .unwrap_or_else(|e| panic!("{e}"));

        let mode = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{e}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the replace widened the file's permissions");
    }

    /// A write that cannot be renamed into place leaves the target as it was
    /// and no temporary beside it.
    #[test]
    fn a_failed_export_leaves_no_temporary_behind() {
        let (dir, s) = store();
        let sid = s.create(meta("a")).unwrap_or_else(|e| panic!("{e}"));
        s.append(&sid, Role::User, "hello")
            .unwrap_or_else(|e| panic!("{e}"));
        // A directory where the export wants a file: the temp is written and
        // the rename cannot land.
        let path = dir.path().join("out.md");
        fs::create_dir(&path).unwrap_or_else(|e| panic!("{e}"));

        let refused = s.export_to_file(&sid, ExportFormat::Markdown, &path);
        assert!(refused.is_err(), "a directory is not a file to write");
        assert!(
            !dir.path().join("out.md.tmp").exists(),
            "the temp was left beside the target"
        );
        assert!(path.is_dir(), "the target was not touched");
    }

    /// The store's listing takes the workspace filter through, and the
    /// unfiltered one still holds every session — including the two written
    /// before workspaces were recorded.
    #[test]
    fn the_listing_scopes_to_a_workspace_and_keeps_old_sessions() {
        let (_dir, s) = store();
        let here = s
            .create(SessionMeta {
                cwd: Some("/work/here".into()),
                ..meta("a")
            })
            .unwrap_or_else(|e| panic!("{e}"));
        let there = s
            .create(SessionMeta {
                cwd: Some("/work/there".into()),
                ..meta("a")
            })
            .unwrap_or_else(|e| panic!("{e}"));
        // Rows the way the release before this field wrote them.
        for (id, ts) in [("legacy-1", 1), ("legacy-2", 2)] {
            s.index
                .insert_session(id, ts, &SessionMeta::default())
                .unwrap_or_else(|e| panic!("{e}"));
        }

        assert_eq!(
            s.sessions_in(Some("/work/here"))
                .unwrap_or_else(|e| panic!("{e}")),
            vec![here.clone()]
        );
        assert_eq!(
            s.sessions_in(Some("/work/nowhere"))
                .unwrap_or_else(|e| panic!("{e}")),
            Vec::<String>::new()
        );
        let all = s.sessions_in(None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(all.len(), 4);
        for id in [&here, &there] {
            assert!(all.contains(id));
        }
        assert!(all.contains(&"legacy-1".to_owned()));
        assert!(all.contains(&"legacy-2".to_owned()));
    }
}
