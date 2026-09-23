//! JSONL persistence for append-only session trees.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
/// so tests point it at a `tempfile::TempDir`.
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
    pub fn create(&self, meta: SessionMeta) -> Result<String, SessionError> {
        let id = entry::new_id();
        File::create_new(self.session_file(&id)).map_err(|e| SessionError::Io(e))?;
        self.index.insert_session(&id, entry::now_ms(), &meta)?;
        Ok(id)
    }

    /// Loads every parseable entry of a session, oldest first.
    ///
    /// Lenient by design: a torn final line left by a crash is skipped.
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
            .append(true)
            .open(self.session_file(session_id))
            .map_err(SessionError::Io)?;
        let line = serde_json::to_string(&e).map_err(SessionError::Json)?;
        writeln!(file, "{line}").map_err(SessionError::Io)?;
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
            .create(true)
            .append(true)
            .open(self.checkpoint_file(session_id))
            .map_err(SessionError::Io)?;
        let line = serde_json::to_string(&checkpoint).map_err(SessionError::Json)?;
        writeln!(file, "{line}").map_err(SessionError::Io)?;
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
        std::fs::write(&path, rewritten).map_err(SessionError::Io)
    }

    /// Checkpoints recorded for a session, oldest first.
    pub fn checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, SessionError> {
        let file = self.checkpoint_file(session_id);
        if !file.exists() {
            return Ok(Vec::new());
        }
        let f = File::open(&file).map_err(SessionError::Io)?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line.map_err(SessionError::Io)?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(checkpoint) = serde_json::from_str::<Checkpoint>(line) {
                out.push(checkpoint);
            }
        }
        Ok(out)
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
        fs::write(self.session_file(session_id), body).map_err(SessionError::Io)?;

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
        fs::write(self.checkpoint_file(session_id), body).map_err(SessionError::Io)?;

        self.index.reindex_session(session_id, kept)?;
        Ok(())
    }

    /// Full-text search over indexed entries, optionally scoped to one bot.
    pub fn search(
        &self,
        query: &str,
        bot_id: Option<&str>,
    ) -> Result<Vec<SearchHit>, SessionError> {
        self.index.search(query, bot_id)
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
        fs::write(path, rendered).map_err(SessionError::Io)
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
        fs::write(self.leaf_file(session_id), entry_id).map_err(SessionError::Io)
    }

    fn load(&self, session_id: &str) -> Result<Vec<Entry>, SessionError> {
        let file = self.session_file(session_id);
        if !file.exists() {
            return Err(SessionError::NotFound(session_id.into()));
        }
        let f = File::open(&file).map_err(SessionError::Io)?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line.map_err(SessionError::Io)?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Lenient: a torn final line after a crash is skipped, not fatal.
            if let Ok(e) = serde_json::from_str::<Entry>(line) {
                out.push(e);
            }
        }
        Ok(out)
    }
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
        let hits = s.search("kafka", None).unwrap_or_else(|e| panic!("{e}"));
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
            .search("secret", Some("bot-a"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].entry_id, ea.id);
        assert_ne!(for_a[0].session_id, sb);

        let for_b = s
            .search("secret", Some("bot-b"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].entry_id, eb.id);

        assert_eq!(
            s.search("secret", None)
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
            s.search("retracted", None)
                .unwrap_or_else(|e| panic!("{e}"))
                .len(),
            1
        );

        s.rewind(&sid, &checkpoint)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            s.search("retracted", None)
                .unwrap_or_else(|e| panic!("{e}"))
                .is_empty(),
            "a rewound entry must not be searchable"
        );
        assert_eq!(
            s.search("durable", None)
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
}
