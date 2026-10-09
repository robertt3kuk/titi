//! Agent traces: nested spans per session, one JSONL file per turn at
//! `<agent_dir>/traces/<session_id>/<turn>.jsonl`.
//!
//! A span is written **when it ends**, so a file is a list of finished spans
//! rather than open/close pairs: a crash can tear at most the last line, there
//! is no pairing state to corrupt, and a reader never waits on a `start` whose
//! `end` never came. Ordering is carried by `start_ms`/`end_ms`, not by file
//! position, so a child finished before its parent still nests.
//!
//! The flat, digest-oriented event log stays where it is
//! ([`crate::trajectory`]): GEPA and the stuck-loop detector read a sequence
//! of tool calls, and this module keeps the shape a trace viewer needs — ids,
//! parent links, start/end, tokens, cost, and the thinking a round produced.
//!
//! **The caller masks.** `titi-core` sits below `titi-memory` and cannot reach
//! the redactor (the same layering that makes
//! [`crate::trajectory::TrajectoryRecorder::record`] document the rule), so a
//! span's attribute values and its `thinking` text must already be masked by
//! whoever writes them. The engine does, at the site that records a tool call
//! and at the one that buffers reasoning.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Attribute key holding the size of a round's thinking when the text itself
/// is not recorded (the `trace.thinking` setting is off).
///
/// Written by the engine and read by the viewer, so the name is a contract of
/// this module rather than a string two crates agree on by luck.
pub const THINKING_CHARS_ATTR: &str = "titi.thinking_chars";

/// The most trace files kept when [`Retention::default`] prunes.
pub const MAX_TRACE_FILES: usize = 500;
/// The oldest a trace file may be when [`Retention::default`] prunes, in days.
pub const MAX_TRACE_AGE_DAYS: u64 = 30;

/// Errors surfaced by trace capture and reading.
#[derive(Debug)]
pub enum TraceError {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// A line that has valid lines after it does not parse: only the last one
    /// can be torn by a crash, so an earlier one is damage and is reported
    /// rather than quietly dropped from the turn.
    Corrupt {
        /// The file's path.
        path: PathBuf,
        /// 1-based line number in that file.
        line: usize,
        source: serde_json::Error,
    },
    /// No trace file exists for the asked-for turn.
    NotFound {
        /// The session whose trace was asked for.
        session: String,
        /// The turn whose file is missing.
        turn: u64,
    },
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceError::Io(e) => write!(f, "trace io: {e}"),
            TraceError::Json(e) => write!(f, "trace json: {e}"),
            TraceError::Corrupt { path, line, source } => {
                write!(
                    f,
                    "trace {} line {line} is corrupt: {source}",
                    path.display()
                )
            }
            TraceError::NotFound { session, turn } => {
                write!(f, "no trace for session {session} turn {turn}")
            }
        }
    }
}

impl std::error::Error for TraceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TraceError::Io(e) => Some(e),
            TraceError::Json(e) => Some(e),
            TraceError::Corrupt { source, .. } => Some(source),
            TraceError::NotFound { .. } => None,
        }
    }
}

/// What a span represents.
///
/// The names line up with the two vocabularies a viewer or an exporter reads:
/// Laminar's span types (`Turn`/`Agent` → `DEFAULT`, `Llm` → `LLM`, `Tool` →
/// `TOOL`, `Event` → `EVENT`) and the OTel GenAI operation names
/// (`invoke_agent`, `chat`, `execute_tool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// One turn: the root of a trace. A session groups its turns.
    Turn,
    /// One model call: the request, the streamed answer, and any retries
    /// inside it (the OTel rule — a retry does not get its own span).
    Llm,
    /// One tool call.
    Tool,
    /// A spawned agent's run, with its own model and tool calls beneath it.
    Agent,
    /// Something with no duration of its own that is still worth a row:
    /// a compaction, a retry that was absorbed, a note.
    Event,
}

impl SpanKind {
    /// The kind as the trace tree labels it — one short word, lowercase.
    pub fn label(self) -> &'static str {
        match self {
            SpanKind::Turn => "turn",
            SpanKind::Llm => "llm",
            SpanKind::Tool => "tool",
            SpanKind::Agent => "agent",
            SpanKind::Event => "event",
        }
    }
}

/// How a span ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    Ok,
    Error,
    Cancelled,
}

impl SpanStatus {
    /// The status as the trace tree labels it.
    pub fn label(self) -> &'static str {
        match self {
            SpanStatus::Ok => "ok",
            SpanStatus::Error => "error",
            SpanStatus::Cancelled => "cancelled",
        }
    }
}

/// One finished span.
///
/// `trace_id` is the session id, so one session is one trace group and each of
/// its turns is a root; `span_id`/`parent_span_id` nest within a turn's file.
/// Token and cost figures are typed (cheap to sum and to map onto
/// `gen_ai.usage.*` on export); everything else — provider, finish reason,
/// tool arguments and result — lives in `attributes` under its `gen_ai.*`
/// name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// The session this span belongs to.
    pub trace_id: String,
    /// Unique within the session's traces.
    pub span_id: String,
    /// The enclosing span, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub kind: SpanKind,
    /// What the span is, e.g. `chat openai/gpt-4o` or `execute_tool read`.
    pub name: String,
    /// Milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// Milliseconds since the Unix epoch.
    pub end_ms: u64,
    pub status: SpanStatus,
    /// Why a span failed, when the reason is not in `attributes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Extra data, named the way the OTel GenAI conventions name it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output_tokens: u64,
    /// The part of `input_tokens` the provider served from its prompt cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_tokens: u64,
    /// Output tokens spent on reasoning (chain-of-thought, extended thinking).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
    /// What the span cost, in micro-dollars, when it ran on a priced model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micro_usd: Option<u64>,
    /// The round's reasoning text, when `trace.thinking` is on. Already masked
    /// by the caller and capped before it reaches here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Span {
    /// A span that starts now and has not ended — the shape a caller fills in
    /// as the work runs, then hands to [`TraceWriter::append`] once `end_ms`
    /// is set.
    pub fn new(
        trace_id: impl Into<String>,
        span_id: impl Into<String>,
        kind: SpanKind,
        name: impl Into<String>,
        start_ms: u64,
    ) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            kind,
            name: name.into(),
            start_ms,
            end_ms: start_ms,
            status: SpanStatus::Ok,
            error: None,
            attributes: BTreeMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            reasoning_tokens: 0,
            cost_micro_usd: None,
            thinking: None,
        }
    }

    /// How long the span took.
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    pub fn with_parent(mut self, parent_span_id: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent_span_id.into());
        self
    }

    pub fn with_end_ms(mut self, end_ms: u64) -> Self {
        self.end_ms = end_ms;
        self
    }

    pub fn with_status(mut self, status: SpanStatus) -> Self {
        self.status = status;
        self
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.status = SpanStatus::Error;
        self.error = Some(error.into());
        self
    }

    pub fn with_attr(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    pub fn with_tokens(mut self, input: u64, output: u64, cached: u64, reasoning: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self.cached_tokens = cached;
        self.reasoning_tokens = reasoning;
        self
    }

    pub fn with_cost_micro_usd(mut self, micro_usd: u64) -> Self {
        self.cost_micro_usd = Some(micro_usd);
        self
    }

    pub fn with_thinking(mut self, text: impl Into<String>) -> Self {
        self.thinking = Some(text.into());
        self
    }
}

/// The directory a session's traces live in: `<agent_dir>/traces/<session_id>`.
///
/// One directory per session, one file per turn inside it, so pruning a
/// session's traces is one `remove_dir_all` and listing them is one `read_dir`.
pub fn session_dir(agent_dir: &Path, session_id: &str) -> PathBuf {
    agent_dir.join("traces").join(session_id)
}

/// The file one turn's spans are appended to.
pub fn turn_path(agent_dir: &Path, session_id: &str, turn: u64) -> PathBuf {
    session_dir(agent_dir, session_id).join(format!("{turn}.jsonl"))
}

/// Append-only writer for one turn's spans.
///
/// Opens (or creates) the turn's file, repairs a torn tail, and appends one
/// JSON line per span. The file is private from the moment it exists — 0600 on
/// create and re-tightened on every open, the same posture as the trajectory
/// and stricter than the session transcript. Writes are buffered; call
/// [`TraceWriter::flush`] to force them out (the recorder flushes on drop, so
/// nothing is lost in the ordinary case).
pub struct TraceWriter {
    path: PathBuf,
    file: BufWriter<File>,
}

impl TraceWriter {
    /// Opens (or creates) `<agent_dir>/traces/<session_id>/<turn>.jsonl`.
    pub fn open(agent_dir: &Path, session_id: &str, turn: u64) -> Result<Self, TraceError> {
        let dir = session_dir(agent_dir, session_id);
        fs::create_dir_all(&dir).map_err(TraceError::Io)?;
        let path = turn_path(agent_dir, session_id, turn);
        let mut options = OpenOptions::new();
        // `read` is for the torn-tail scan below; appends still land at the
        // end regardless of the read position.
        options.read(true).create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(TraceError::Io)?;
        // A torn final line would swallow the next append and then be dropped
        // with it, leaving a corrupt line behind for the one after that — the
        // same repair the session and trajectory files get.
        crate::session::store::truncate_torn_tail(&mut file).map_err(TraceError::Io)?;
        // `mode` is only consulted at creation: a file an earlier build made
        // 0644 keeps those bits until someone says otherwise.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(TraceError::Io)?;
        }
        Ok(Self {
            path,
            file: BufWriter::new(file),
        })
    }

    /// Appends one span.
    pub fn append(&mut self, span: &Span) -> Result<(), TraceError> {
        let line = serde_json::to_string(span).map_err(TraceError::Json)?;
        writeln!(self.file, "{line}").map_err(TraceError::Io)
    }

    /// Flushes buffered spans to disk.
    pub fn flush(&mut self) -> Result<(), TraceError> {
        self.file.flush().map_err(TraceError::Io)
    }

    /// Path of the backing JSONL file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TraceWriter {
    fn drop(&mut self) {
        // Best-effort: buffered spans must not vanish on drop.
        let _ = self.file.flush();
    }
}

/// Replays one turn's file, oldest first.
///
/// A line that does not parse is tolerated only when it is the *last* non-empty
/// line, which is all a crash mid-append can tear. Anything earlier is
/// [`TraceError::Corrupt`] naming the line, because a replayed turn that
/// silently lost a middle span would misreport what the agent did.
fn load(path: &Path) -> Result<Vec<Span>, TraceError> {
    let file = File::open(path).map_err(TraceError::Io)?;
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .collect::<std::result::Result<_, _>>()
        .map_err(TraceError::Io)?;
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Span>(line) {
            Ok(span) => out.push(span),
            Err(_) if last == Some(index) => break,
            Err(source) => {
                return Err(TraceError::Corrupt {
                    path: path.to_path_buf(),
                    line: index + 1,
                    source,
                });
            }
        }
    }
    Ok(out)
}

/// Reads one turn's spans, in the order they were written.
pub fn read_turn(agent_dir: &Path, session_id: &str, turn: u64) -> Result<Vec<Span>, TraceError> {
    let path = turn_path(agent_dir, session_id, turn);
    if !path.exists() {
        return Err(TraceError::NotFound {
            session: session_id.into(),
            turn,
        });
    }
    load(&path)
}

/// The turn numbers that have a trace file, ascending.
pub fn turns(agent_dir: &Path, session_id: &str) -> Result<Vec<u64>, TraceError> {
    let dir = session_dir(agent_dir, session_id);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut turns: Vec<u64> = entries
        .flatten()
        .filter_map(|entry| entry.path().file_stem()?.to_str()?.parse::<u64>().ok())
        .collect();
    turns.sort_unstable();
    Ok(turns)
}

/// One turn's spans, as [`read_session`] returns them.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnTrace {
    pub turn: u64,
    pub spans: Vec<Span>,
}

impl TurnTrace {
    /// The spans as a parent/child tree.
    pub fn tree(&self) -> Vec<TraceNode> {
        build_tree(self.spans.clone())
    }

    /// Sum of the LLM spans' input tokens — the turn's prompt cost.
    ///
    /// Gated on [`SpanKind::Llm`] the way Laminar gates its own totals: a tool
    /// or agent span carrying token attributes would otherwise inflate the
    /// figure.
    pub fn input_tokens(&self) -> u64 {
        self.spans
            .iter()
            .filter(|s| s.kind == SpanKind::Llm)
            .map(|s| s.input_tokens)
            .sum()
    }

    /// Sum of the LLM spans' output tokens.
    pub fn output_tokens(&self) -> u64 {
        self.spans
            .iter()
            .filter(|s| s.kind == SpanKind::Llm)
            .map(|s| s.output_tokens)
            .sum()
    }

    /// Sum of the LLM spans' cost, or `None` when none of them was priced.
    pub fn cost_micro_usd(&self) -> Option<u64> {
        let mut total = None;
        for span in self.spans.iter().filter(|s| s.kind == SpanKind::Llm) {
            if let Some(cost) = span.cost_micro_usd {
                *total.get_or_insert(0) += cost;
            }
        }
        total
    }

    /// How many spans of the turn ended in an error.
    pub fn errors(&self) -> usize {
        self.spans
            .iter()
            .filter(|s| s.status == SpanStatus::Error)
            .count()
    }

    /// Wall-clock width of the turn: earliest start to latest end.
    pub fn duration_ms(&self) -> u64 {
        let start = self.spans.iter().map(|s| s.start_ms).min();
        let end = self.spans.iter().map(|s| s.end_ms).max();
        match (start, end) {
            (Some(start), Some(end)) => end.saturating_sub(start),
            _ => 0,
        }
    }
}

/// Every turn of a session that has a trace, ascending by turn.
pub fn read_session(agent_dir: &Path, session_id: &str) -> Result<Vec<TurnTrace>, TraceError> {
    let mut out = Vec::new();
    for turn in turns(agent_dir, session_id)? {
        out.push(TurnTrace {
            turn,
            spans: read_turn(agent_dir, session_id, turn)?,
        });
    }
    Ok(out)
}

/// One node of a span tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceNode {
    pub span: Span,
    pub children: Vec<TraceNode>,
}

/// Builds a parent/child forest from a flat span list.
///
/// A span whose `parent_span_id` names no span in the list is a root — a
/// partially flushed file, or a parent that has not been written yet, still
/// renders instead of disappearing. Siblings are ordered by `start_ms` and
/// then by `span_id`, so the tree is the same however the file was appended.
/// A cycle in a damaged file cannot hang the walk: a span is emitted once, and
/// any span no root reaches is appended as a root of its own.
pub fn build_tree(spans: Vec<Span>) -> Vec<TraceNode> {
    let ids: HashSet<String> = spans.iter().map(|s| s.span_id.clone()).collect();
    let mut children_of: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        match &span.parent_span_id {
            Some(parent) if ids.contains(parent) && parent != &span.span_id => {
                children_of.entry(parent.clone()).or_default().push(index);
            }
            _ => roots.push(index),
        }
    }
    let order = |a: &usize, b: &usize| {
        let (a, b) = (&spans[*a], &spans[*b]);
        (a.start_ms, &a.span_id).cmp(&(b.start_ms, &b.span_id))
    };
    roots.sort_by(order);
    for list in children_of.values_mut() {
        list.sort_by(order);
    }

    let mut visited: HashSet<usize> = HashSet::new();
    let mut forest = Vec::new();
    for root in roots {
        forest.push(build_node(&root, &spans, &children_of, &mut visited));
    }
    // Spans stranded in a cycle (or parented only to each other) are emitted
    // last rather than dropped.
    for index in 0..spans.len() {
        if !visited.contains(&index) {
            forest.push(build_node(&index, &spans, &children_of, &mut visited));
        }
    }
    forest
}

fn build_node(
    index: &usize,
    spans: &[Span],
    children_of: &HashMap<String, Vec<usize>>,
    visited: &mut HashSet<usize>,
) -> TraceNode {
    visited.insert(*index);
    let mut children = Vec::new();
    if let Some(list) = children_of.get(&spans[*index].span_id) {
        for child in list {
            if !visited.contains(child) {
                children.push(build_node(child, spans, children_of, visited));
            }
        }
    }
    TraceNode {
        span: spans[*index].clone(),
        children,
    }
}

/// How much trace history to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// The most trace files kept overall.
    pub max_files: usize,
    /// The oldest a trace file may be, in days.
    pub max_age_days: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_files: MAX_TRACE_FILES,
            max_age_days: MAX_TRACE_AGE_DAYS,
        }
    }
}

/// Deletes trace files a retention rule does not keep, then the session
/// directories they left empty. Returns how many files were removed.
///
/// Two rules, both applied: nothing older than `max_age_days`, and no more
/// than `max_files` overall (the newest win). A file that is both old and
/// surplus counts once.
pub fn prune(agent_dir: &Path, retention: Retention) -> Result<usize, TraceError> {
    prune_at(agent_dir, retention, SystemTime::now())
}

/// [`prune`] against a supplied clock, so a test can age a file without
/// touching its mtime.
pub fn prune_at(
    agent_dir: &Path,
    retention: Retention,
    now: SystemTime,
) -> Result<usize, TraceError> {
    let root = agent_dir.join("traces");
    let Ok(sessions) = fs::read_dir(&root) else {
        return Ok(0);
    };
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    for session in sessions.flatten() {
        let dir = session.path();
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            files.push((path, modified));
        }
    }

    let doomed = select_for_removal(&files, retention, now);
    for path in &doomed {
        fs::remove_file(path).map_err(TraceError::Io)?;
    }
    // A session whose traces are all gone should not leave an empty directory
    // behind; a directory that still holds something is kept.
    if let Ok(sessions) = fs::read_dir(&root) {
        for session in sessions.flatten() {
            let dir = session.path();
            if dir.is_dir() && fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_none()) {
                let _ = fs::remove_dir(&dir);
            }
        }
    }
    Ok(doomed.len())
}

/// Which files a retention rule removes — the pure part of [`prune`].
fn select_for_removal(
    files: &[(PathBuf, SystemTime)],
    retention: Retention,
    now: SystemTime,
) -> Vec<PathBuf> {
    let cutoff = Duration::from_secs(retention.max_age_days.saturating_mul(24 * 60 * 60));
    let mut keep: Vec<&(PathBuf, SystemTime)> = Vec::new();
    let mut doomed: Vec<PathBuf> = Vec::new();
    for entry in files {
        let age = now.duration_since(entry.1).unwrap_or_default();
        if age > cutoff {
            doomed.push(entry.0.clone());
        } else {
            keep.push(entry);
        }
    }
    if retention.max_files < keep.len() {
        // Newest first; ties broken by path so the choice is deterministic.
        keep.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for entry in keep.into_iter().skip(retention.max_files) {
            doomed.push(entry.0.clone());
        }
    }
    // The order files are removed in does not matter; a stable one does, so a
    // caller's log and a test's assertion read the same twice.
    doomed.sort();
    doomed
}

/// A `SystemTime` for a millisecond stamp, for tests that age a file.
#[cfg(test)]
fn time_at(ms: u64) -> SystemTime {
    std::time::UNIX_EPOCH + Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span(id: &str, kind: SpanKind, start: u64, end: u64) -> Span {
        Span::new("s1", id, kind, format!("{id}-name"), start).with_end_ms(end)
    }

    fn writer(dir: &Path, turn: u64) -> TraceWriter {
        TraceWriter::open(dir, "s1", turn).unwrap_or_else(|e| panic!("open: {e}"))
    }

    #[test]
    fn writer_appends_and_reader_replays_in_order() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut w = writer(dir.path(), 1);
        let spans = vec![
            span("a", SpanKind::Turn, 1_000, 1_500),
            span("b", SpanKind::Llm, 1_010, 1_200)
                .with_parent("a")
                .with_tokens(1200, 180, 1000, 40)
                .with_cost_micro_usd(2_100)
                .with_attr("gen_ai.request.model", json!("openai/gpt-4o")),
            span("c", SpanKind::Tool, 1_210, 1_240).with_parent("b"),
        ];
        for s in &spans {
            w.append(s).unwrap_or_else(|e| panic!("append: {e}"));
        }
        w.flush().unwrap_or_else(|e| panic!("flush: {e}"));
        assert!(w.path().ends_with("traces/s1/1.jsonl"));

        let read = read_turn(dir.path(), "s1", 1).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(read, spans);
    }

    #[test]
    fn open_repairs_a_torn_tail_before_appending() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut w = writer(dir.path(), 1);
        w.append(&span("a", SpanKind::Turn, 1, 2))
            .unwrap_or_else(|e| panic!("append: {e}"));
        w.flush().unwrap_or_else(|e| panic!("flush: {e}"));
        // A crash tears the last line: a partial JSON object with no newline.
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(w.path())
                .unwrap_or_else(|e| panic!("open: {e}"));
            write!(file, "{{\"trace_id\":\"s1\",\"span_id\":\"b\"")
                .unwrap_or_else(|e| panic!("{e}"));
        }
        drop(w);

        // Reopening drops the fragment, and the next append lands cleanly.
        let mut w = writer(dir.path(), 1);
        w.append(&span("c", SpanKind::Llm, 3, 4))
            .unwrap_or_else(|e| panic!("append: {e}"));
        w.flush().unwrap_or_else(|e| panic!("flush: {e}"));
        let read = read_turn(dir.path(), "s1", 1).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].span_id, "a");
        assert_eq!(read[1].span_id, "c");
    }

    #[test]
    fn a_damaged_middle_line_is_an_error_not_a_silent_gap() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut w = writer(dir.path(), 1);
        w.append(&span("a", SpanKind::Turn, 1, 2))
            .unwrap_or_else(|e| panic!("append: {e}"));
        w.flush().unwrap_or_else(|e| panic!("flush: {e}"));
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(w.path())
                .unwrap_or_else(|e| panic!("open: {e}"));
            writeln!(file, "not json").unwrap_or_else(|e| panic!("{e}"));
        }
        drop(w);
        let mut w = writer(dir.path(), 1);
        w.append(&span("c", SpanKind::Llm, 3, 4))
            .unwrap_or_else(|e| panic!("append: {e}"));
        w.flush().unwrap_or_else(|e| panic!("flush: {e}"));
        drop(w);

        match read_turn(dir.path(), "s1", 1) {
            Err(TraceError::Corrupt { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn trace_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let w = writer(dir.path(), 1);
        let mode = fs::metadata(w.path())
            .unwrap_or_else(|e| panic!("stat: {e}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn build_tree_nests_children_and_promotes_an_absent_parent() {
        let spans = vec![
            span("root", SpanKind::Turn, 10, 100),
            span("llm", SpanKind::Llm, 20, 60).with_parent("root"),
            span("tool", SpanKind::Tool, 70, 80).with_parent("llm"),
            span("orphan", SpanKind::Agent, 30, 40).with_parent("gone"),
        ];
        let forest = build_tree(spans);
        assert_eq!(forest.len(), 2);
        assert_eq!(forest[0].span.span_id, "root");
        assert_eq!(forest[0].children.len(), 1);
        assert_eq!(forest[0].children[0].span.span_id, "llm");
        assert_eq!(forest[0].children[0].children[0].span.span_id, "tool");
        // The span whose parent is not in the list is a root, not a loss.
        assert_eq!(forest[1].span.span_id, "orphan");
    }

    #[test]
    fn build_tree_orders_siblings_and_survives_a_cycle() {
        let spans = vec![
            span("b", SpanKind::Llm, 20, 30).with_parent("a"),
            span("a", SpanKind::Turn, 10, 40),
            span("c", SpanKind::Llm, 15, 25).with_parent("a"),
            span("x", SpanKind::Event, 50, 51).with_parent("y"),
            span("y", SpanKind::Event, 52, 53).with_parent("x"),
        ];
        let forest = build_tree(spans);
        // `a` and the two-member cycle are both reachable; the cycle is one
        // root, not a hang, and every span appears exactly once.
        assert_eq!(forest.len(), 2);
        assert_eq!(forest[0].span.span_id, "a");
        let siblings: Vec<&str> = forest[0]
            .children
            .iter()
            .map(|n| n.span.span_id.as_str())
            .collect();
        assert_eq!(siblings, vec!["c", "b"]);

        fn collect(nodes: &[TraceNode], ids: &mut Vec<String>) {
            for node in nodes {
                ids.push(node.span.span_id.clone());
                collect(&node.children, ids);
            }
        }
        let mut ids = Vec::new();
        collect(&forest, &mut ids);
        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "b", "c", "x", "y"]);
    }

    #[test]
    fn read_turn_missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        match read_turn(dir.path(), "s1", 9) {
            Err(TraceError::NotFound { turn, .. }) => assert_eq!(turn, 9),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn read_session_groups_turns_ascending() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut w = writer(dir.path(), 2);
        w.append(&span("a", SpanKind::Turn, 100, 200))
            .unwrap_or_else(|e| panic!("append: {e}"));
        w.flush().unwrap_or_else(|e| panic!("{e}"));
        let mut w = writer(dir.path(), 1);
        w.append(&span("b", SpanKind::Llm, 10, 20))
            .unwrap_or_else(|e| panic!("append: {e}"));
        w.flush().unwrap_or_else(|e| panic!("{e}"));
        drop(w);

        let session = read_session(dir.path(), "s1").unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(
            session.iter().map(|t| t.turn).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(turns(dir.path(), "s1").unwrap_or_default(), vec![1, 2]);
    }

    #[test]
    fn turn_totals_gate_on_llm_spans() {
        let mut tool = span("tool", SpanKind::Tool, 10, 20);
        tool.input_tokens = 999;
        let trace = TurnTrace {
            turn: 1,
            spans: vec![
                span("root", SpanKind::Turn, 0, 100),
                span("a", SpanKind::Llm, 10, 30)
                    .with_tokens(1000, 200, 900, 50)
                    .with_cost_micro_usd(2_000),
                span("b", SpanKind::Llm, 40, 60)
                    .with_tokens(500, 100, 0, 0)
                    .with_cost_micro_usd(1_000),
                tool,
            ],
        };
        assert_eq!(trace.input_tokens(), 1500);
        assert_eq!(trace.output_tokens(), 300);
        assert_eq!(trace.cost_micro_usd(), Some(3_000));
    }

    #[test]
    fn an_unpriced_turn_sums_no_cost() {
        let trace = TurnTrace {
            turn: 1,
            spans: vec![span("a", SpanKind::Llm, 1, 2)],
        };
        assert_eq!(trace.cost_micro_usd(), None);
    }

    #[test]
    fn prune_drops_files_past_the_age_cutoff() {
        let now = time_at(1_000 * 24 * 60 * 60 * 1000);
        let old = (PathBuf::from("/t/old.jsonl"), time_at(0));
        let fresh = (PathBuf::from("/t/fresh.jsonl"), now);
        let retention = Retention {
            max_files: 10,
            max_age_days: 30,
        };
        let doomed = select_for_removal(&[old, fresh.clone()], retention, now);
        assert_eq!(doomed, vec![PathBuf::from("/t/old.jsonl")]);
    }

    #[test]
    fn prune_keeps_the_newest_files_up_to_the_cap() {
        let now = time_at(10_000);
        let files: Vec<(PathBuf, SystemTime)> = (0..5)
            .map(|i| (PathBuf::from(format!("/t/{i}.jsonl")), time_at(i)))
            .collect();
        let retention = Retention {
            max_files: 2,
            max_age_days: 365,
        };
        let doomed = select_for_removal(&files, retention, now);
        // The newest two are 3 and 4; 0, 1 and 2 go.
        assert_eq!(
            doomed,
            vec![
                PathBuf::from("/t/0.jsonl"),
                PathBuf::from("/t/1.jsonl"),
                PathBuf::from("/t/2.jsonl"),
            ]
        );
    }

    #[test]
    fn prune_walks_and_removes_on_disk() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        for turn in 1..=3 {
            let mut w = writer(dir.path(), turn);
            w.append(&span("a", SpanKind::Turn, 1, 2))
                .unwrap_or_else(|e| panic!("append: {e}"));
            w.flush().unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(
            prune(dir.path(), Retention::default()).unwrap_or_else(|e| panic!("prune: {e}")),
            0
        );
        let all = prune(
            dir.path(),
            Retention {
                max_files: 1,
                max_age_days: 365,
            },
        )
        .unwrap_or_else(|e| panic!("prune: {e}"));
        assert_eq!(all, 2);
        assert_eq!(turns(dir.path(), "s1").unwrap_or_default().len(), 1);
    }
}
