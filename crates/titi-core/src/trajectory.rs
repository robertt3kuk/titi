//! Trajectory capture: append-only JSONL event log per session at
//! `<agent_dir>/trajectories/<session_id>.jsonl`, monotonic sequence
//! numbers, tail reads for GEPA review, and stable digests for the
//! stuck-loop detector (`docs/research/trajectory-gepa`).

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Errors surfaced by trajectory capture.
#[derive(Debug)]
pub enum TrajectoryError {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// A line that has valid lines after it does not parse: only the last one
    /// can be torn by a crash, so an earlier one is damage and is reported
    /// rather than quietly dropped from the replayed tail.
    Corrupt {
        /// 1-based line number in the file at [`TrajectoryRecorder::path`].
        line: usize,
        source: serde_json::Error,
    },
}

impl fmt::Display for TrajectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrajectoryError::Io(e) => write!(f, "trajectory io: {e}"),
            TrajectoryError::Json(e) => write!(f, "trajectory json: {e}"),
            TrajectoryError::Corrupt { line, source } => {
                write!(f, "trajectory line {line} is corrupt: {source}")
            }
        }
    }
}

impl std::error::Error for TrajectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TrajectoryError::Io(e) => Some(e),
            TrajectoryError::Json(e) => Some(e),
            TrajectoryError::Corrupt { source, .. } => Some(source),
        }
    }
}

/// One recorded trajectory event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryEvent {
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    /// Monotonic, gap-free, 1-based position within the session file.
    pub seq: u64,
    pub kind: EventKind,
}

/// Payload of a trajectory event. `ToolCall::id` and `ToolResult::id`
/// pair a call with its outcome; the id is excluded from digests so a
/// repeated identical call still looks repeated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        id: String,
        duration_ms: u64,
        ok: bool,
    },
    /// Turn boundary; the recorder flushes the file here.
    TurnEnd,
    /// The request crossed the context threshold and the oldest messages were
    /// folded into one digest by `strategy`.
    Compaction {
        folded: u64,
        strategy: String,
    },
    /// A GEPA review window was closed over the recent trajectory.
    GepaReview,
}

/// Append-only recorder for one session's trajectory.
///
/// Writes are buffered and flushed on `turn_end` (and on drop); a crash
/// can at worst tear the final line, which re-open skips leniently. The
/// agent directory is injected — no env lookups — so tests point it at a
/// `tempfile::TempDir`.
pub struct TrajectoryRecorder {
    path: PathBuf,
    file: BufWriter<File>,
    events: Vec<TrajectoryEvent>,
    next_seq: u64,
}

impl TrajectoryRecorder {
    /// Opens (or creates) `<agent_dir>/trajectories/<session_id>.jsonl`,
    /// replaying existing events and continuing their sequence.
    pub fn open(agent_dir: &Path, session_id: &str) -> Result<Self, TrajectoryError> {
        let dir = agent_dir.join("trajectories");
        fs::create_dir_all(&dir).map_err(TrajectoryError::Io)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let events = if path.exists() {
            Self::load(&path)?
        } else {
            Vec::new()
        };
        let next_seq = events.last().map_or(1, |e| e.seq + 1);
        let mut options = OpenOptions::new();
        // `read` is for the torn-tail scan below; appends still land at the
        // end regardless of the read position.
        options.read(true).create(true).append(true);
        // A trajectory repeats the session's tool calls and arguments, so it
        // is as private as the session file. The umask can only clear bits a
        // `0o600` request does not carry, so this is 0600 on create.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(TrajectoryError::Io)?;
        let mut file = file;
        // The same repair the session files get: replay tolerates a torn final
        // line, but only while nothing is written after it — the next event
        // would join the fragment, be dropped with it on the next replay, and
        // leave a corrupt line behind for the one after that.
        crate::session::store::truncate_torn_tail(&mut file).map_err(TrajectoryError::Io)?;
        // `mode` is only consulted at creation: a file an earlier build made
        // 0644 keeps those bits until someone says otherwise.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(TrajectoryError::Io)?;
        }
        Ok(Self {
            path,
            file: BufWriter::new(file),
            events,
            next_seq,
        })
    }

    /// Appends an event with the current timestamp and the next sequence
    /// number, flushing to disk when the event closes a turn.
    ///
    /// The payload is written to the file exactly as it is passed, so a caller
    /// recording a [`ToolCall`](EventKind::ToolCall) **must** mask its
    /// arguments first — its `args` are a tool's own input, and a tool is
    /// routinely handed a credential. The engine does, at its single
    /// `ToolCall` recording site, and the event it hands back is the masked
    /// one. Masking here would be better and is not possible: the workspace's
    /// secret pattern set lives in `titi-memory`, which is layered above this
    /// crate, so a dependency on it would point the graph backwards.
    pub fn record(&mut self, kind: EventKind) -> Result<TrajectoryEvent, TrajectoryError> {
        let e = TrajectoryEvent {
            ts: crate::session::entry::now_ms(),
            seq: self.next_seq,
            kind,
        };
        let line = serde_json::to_string(&e).map_err(TrajectoryError::Json)?;
        writeln!(self.file, "{line}").map_err(TrajectoryError::Io)?;
        if matches!(e.kind, EventKind::TurnEnd) {
            self.file.flush().map_err(TrajectoryError::Io)?;
        }
        self.next_seq += 1;
        self.events.push(e.clone());
        Ok(e)
    }

    /// Flushes buffered events to disk.
    pub fn flush(&mut self) -> Result<(), TrajectoryError> {
        self.file.flush().map_err(TrajectoryError::Io)
    }

    /// The last `n` events, oldest first — the GEPA review window.
    pub fn tail(&self, n: usize) -> Vec<TrajectoryEvent> {
        let start = self.events.len().saturating_sub(n);
        self.events[start..].to_vec()
    }

    /// Number of events held (replayed plus recorded).
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether any event has been recorded.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Path of the backing JSONL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Digests of the last `k` tool calls, oldest first. Identical
    /// consecutive digests mean the agent is repeating itself.
    pub fn recent_tool_digests(&self, k: usize) -> Vec<String> {
        let mut calls: Vec<(&str, &Value)> = self
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolCall { name, args, .. } => Some((name.as_str(), args)),
                _ => None,
            })
            .collect();
        let start = calls.len().saturating_sub(k);
        calls
            .drain(start..)
            .map(|(n, a)| tool_call_digest(n, a))
            .collect()
    }

    /// Digest over the last `k` tool calls as one window (`None` when
    /// fewer than `k` calls exist) — the identity GEPA review dedupes on.
    pub fn window_digest(&self, k: usize) -> Option<String> {
        let digests = self.recent_tool_digests(k);
        (digests.len() == k).then(|| {
            let joined = digests.concat();
            format!("{:016x}", fnv1a(joined.as_bytes()))
        })
    }

    /// Stuck-loop flag: the last `repeats` tool calls share one digest.
    pub fn stuck_loop(&self, repeats: usize) -> bool {
        if repeats == 0 {
            return false;
        }
        let digests = self.recent_tool_digests(repeats);
        digests.len() == repeats && digests.windows(2).all(|w| w[0] == w[1])
    }

    /// Replays the file, oldest first.
    ///
    /// Contract, matching the session reader: a line that does not parse is
    /// tolerated only when it is the *last* non-empty line, which is all a
    /// crash mid-append can tear. Anything earlier is [`TrajectoryError::Corrupt`]
    /// naming the line, because a replayed tail that silently lost a middle
    /// event would misreport what the session did — the stuck-loop detector
    /// counts calls, and a dropped one hides a repeat.
    fn load(path: &Path) -> Result<Vec<TrajectoryEvent>, TrajectoryError> {
        let file = File::open(path).map_err(TrajectoryError::Io)?;
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<std::result::Result<_, _>>()
            .map_err(TrajectoryError::Io)?;
        let last = lines.iter().rposition(|line| !line.trim().is_empty());
        let mut out = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TrajectoryEvent>(line) {
                Ok(event) => out.push(event),
                Err(_) if last == Some(index) => break,
                Err(source) => {
                    return Err(TrajectoryError::Corrupt {
                        line: index + 1,
                        source,
                    });
                }
            }
        }
        Ok(out)
    }
}

impl Drop for TrajectoryRecorder {
    fn drop(&mut self) {
        // Best-effort: buffered events must not vanish on drop.
        let _ = self.file.flush();
    }
}

/// Stable 64-bit FNV-1a digest of one tool call: name plus canonical
/// args JSON. Stable across processes, so persisted trajectories digest
/// identically after a restart.
pub fn tool_call_digest(name: &str, args: &Value) -> String {
    let mut bytes = Vec::with_capacity(name.len() + 32);
    bytes.extend_from_slice(name.as_bytes());
    match serde_json::to_string(args) {
        Ok(json) => bytes.extend_from_slice(json.as_bytes()),
        Err(_) => bytes.extend_from_slice(b"\0unserializable"),
    }
    format!("{:016x}", fnv1a(&bytes))
}

/// FNV-1a 64-bit — dependency-free and deterministic.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open(dir: &Path) -> TrajectoryRecorder {
        TrajectoryRecorder::open(dir, "s1").unwrap_or_else(|e| panic!("open: {e}"))
    }

    fn call(id: &str, name: &str, args: Value) -> EventKind {
        EventKind::ToolCall {
            id: id.into(),
            name: name.into(),
            args,
        }
    }

    #[test]
    fn append_assigns_monotonic_seq_and_tail_reads_back() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut r = open(dir.path());
        assert!(r.is_empty());

        let events = vec![
            EventKind::UserMessage { text: "go".into() },
            call("t1", "bash", json!({"cmd": "ls"})),
            EventKind::ToolResult {
                id: "t1".into(),
                duration_ms: 12,
                ok: true,
            },
            EventKind::AssistantMessage {
                text: "done".into(),
            },
            EventKind::TurnEnd,
        ];
        let recorded: Vec<TrajectoryEvent> = events
            .into_iter()
            .map(|k| r.record(k).unwrap_or_else(|e| panic!("record: {e}")))
            .collect();
        for (i, e) in recorded.iter().enumerate() {
            assert_eq!(e.seq, i as u64 + 1);
        }

        // Tail returns the newest events, oldest first.
        let tail = r.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(
            tail[0].kind,
            EventKind::AssistantMessage {
                text: "done".into()
            }
        );
        assert_eq!(tail[1].kind, EventKind::TurnEnd);
        assert_eq!(r.tail(usize::MAX).len(), 5);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn turn_end_flushes_buffered_events_to_disk() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut r = open(dir.path());
        r.record(EventKind::UserMessage { text: "hi".into() })
            .unwrap_or_else(|e| panic!("record: {e}"));
        r.record(call("t1", "bash", json!({"cmd": "ls"})))
            .unwrap_or_else(|e| panic!("record: {e}"));

        // Buffered, not yet visible on disk.
        let raw = fs::read_to_string(r.path()).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(raw.lines().count(), 0);

        r.record(EventKind::TurnEnd)
            .unwrap_or_else(|e| panic!("record: {e}"));
        let raw = fs::read_to_string(r.path()).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(raw.lines().count(), 3);
        let kinds: Vec<EventKind> = raw
            .lines()
            .map(|l| {
                serde_json::from_str::<TrajectoryEvent>(l)
                    .unwrap_or_else(|e| panic!("{e}"))
                    .kind
            })
            .collect();
        assert!(matches!(kinds[2], EventKind::TurnEnd));
    }

    #[test]
    fn reopen_continues_seq_and_tail() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        {
            let mut r = open(dir.path());
            r.record(EventKind::UserMessage { text: "one".into() })
                .unwrap_or_else(|e| panic!("record: {e}"));
            r.record(EventKind::TurnEnd)
                .unwrap_or_else(|e| panic!("record: {e}"));
        }
        let mut r = open(dir.path());
        let e = r
            .record(EventKind::UserMessage { text: "two".into() })
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert_eq!(e.seq, 3);
        assert_eq!(r.len(), 3);
        assert_eq!(r.tail(10)[2].seq, 3);
    }

    #[test]
    fn torn_final_line_is_skipped_leniently() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut r = open(dir.path());
        r.record(EventKind::UserMessage { text: "one".into() })
            .unwrap_or_else(|e| panic!("record: {e}"));
        r.record(EventKind::TurnEnd)
            .unwrap_or_else(|e| panic!("record: {e}"));

        // Simulate a crash mid-write: a partial JSON line without a newline.
        let mut f = OpenOptions::new()
            .append(true)
            .open(r.path())
            .unwrap_or_else(|e| panic!("append: {e}"));
        write!(f, r#"{{"ts":123,"seq""#).unwrap_or_else(|e| panic!("write: {e}"));
        drop(f);
        drop(r);

        let r = open(dir.path());
        assert_eq!(r.len(), 2);
        // Sequence continues after the last valid event.
        let mut r = r;
        let e = r
            .record(EventKind::UserMessage { text: "two".into() })
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert_eq!(e.seq, 3);
    }

    #[test]
    fn stuck_loop_flags_repeated_identical_tool_calls() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut r = open(dir.path());

        // Too few repeats: no flag.
        r.record(call("t1", "bash", json!({"cmd": "ls"})))
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(!r.stuck_loop(3));

        for i in 2..=4 {
            r.record(call(&format!("t{i}"), "bash", json!({"cmd": "ls"})))
                .unwrap_or_else(|e| panic!("record: {e}"));
        }
        assert!(r.stuck_loop(3));
        assert!(r.stuck_loop(4));

        // A differing call breaks the streak.
        r.record(call("t5", "bash", json!({"cmd": "pwd"})))
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(!r.stuck_loop(2));

        // Different id, same name+args: still a repeat for the detector.
        r.record(call("t6", "bash", json!({"cmd": "pwd"})))
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(r.stuck_loop(2));
    }

    #[test]
    fn window_digest_requires_k_calls_and_is_stable() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut r = open(dir.path());
        assert_eq!(r.window_digest(2), None);

        r.record(call("t1", "bash", json!({"cmd": "ls"})))
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert_eq!(r.window_digest(2), None);

        r.record(call("t2", "grep", json!({"q": "x"})))
            .unwrap_or_else(|e| panic!("record: {e}"));
        let d = r.window_digest(2).unwrap_or_else(|| panic!("digest"));
        assert_eq!(d.len(), 16);

        // Key order in args does not change the digest.
        assert_eq!(
            tool_call_digest("bash", &json!({"a": 1, "b": 2})),
            tool_call_digest("bash", &json!({"b": 2, "a": 1}))
        );
        assert_ne!(
            tool_call_digest("bash", &json!({"a": 1})),
            tool_call_digest("bash", &json!({"a": 2}))
        );
    }

    /// A torn final line is tolerated on replay, but the next event must not
    /// be written after it: it would join the fragment and be lost with it on
    /// the following replay, leaving a corrupt line for the event after that.
    #[test]
    fn a_torn_line_is_dropped_before_the_next_event_lands() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        {
            let mut r = open(dir.path());
            r.record(EventKind::UserMessage { text: "one".into() })
                .unwrap_or_else(|e| panic!("record: {e}"));
            r.record(EventKind::TurnEnd)
                .unwrap_or_else(|e| panic!("record: {e}"));
            r.flush().unwrap_or_else(|e| panic!("flush: {e}"));

            // Simulate a crash mid-write: a partial JSON line, no newline.
            let mut f = OpenOptions::new()
                .append(true)
                .open(r.path())
                .unwrap_or_else(|e| panic!("append: {e}"));
            write!(f, r#"{{"ts":123,"seq""#).unwrap_or_else(|e| panic!("write: {e}"));
        }

        // Replay tolerates it and hands the sequence on.
        let mut r = open(dir.path());
        assert_eq!(r.len(), 2);
        r.record(EventKind::UserMessage { text: "two".into() })
            .unwrap_or_else(|e| panic!("record: {e}"));
        r.record(EventKind::TurnEnd)
            .unwrap_or_else(|e| panic!("record: {e}"));
        drop(r);

        // Both events are there and the file reads cleanly: the fragment did
        // not swallow the new one and did not become a corrupt middle line.
        let replay = open(dir.path());
        assert_eq!(replay.len(), 4);
        assert!(matches!(
            &replay.tail(2)[0].kind,
            EventKind::UserMessage { text } if text == "two"
        ));
    }

    /// A trajectory repeats the session's tool calls, so it is as private as
    /// the session file — on creation and, for a file an earlier build left
    /// group- or world-readable, on the next open.
    #[cfg(unix)]
    #[test]
    fn the_trajectory_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mode = |path: &Path| {
            fs::metadata(path)
                .unwrap_or_else(|e| panic!("metadata: {e}"))
                .permissions()
                .mode()
                & 0o777
        };

        let mut r = open(dir.path());
        r.record(EventKind::TurnEnd)
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert_eq!(mode(r.path()), 0o600);
        drop(r);

        // A file an earlier build created with the default mode.
        let loose = dir.path().join("trajectories").join("s2.jsonl");
        fs::write(&loose, "").unwrap_or_else(|e| panic!("write: {e}"));
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|e| panic!("chmod: {e}"));
        assert_eq!(mode(&loose), 0o644);

        let r = TrajectoryRecorder::open(dir.path(), "s2").unwrap_or_else(|e| panic!("open: {e}"));
        assert_eq!(mode(r.path()), 0o600, "a loose trajectory is tightened");
    }
}
