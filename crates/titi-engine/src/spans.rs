//! The engine's half of a trace: writing the spans a turn produces.
//!
//! `titi-core` owns the file format ([`titi_core::trace`]); this is what fills
//! it. One [`SpanRecorder`] per session is handed in by the surface and opens
//! a file per turn; every span names the parent it belongs to explicitly.
//! There is deliberately no ambient "current span": a tool round runs its
//! read-tier calls concurrently and a subagent runs in its own task, so a
//! shared stack would nest one call's span under another's.
//!
//! Nothing here is on the critical path of a stream. A write is one buffered
//! line under a short-lived lock, and every failure is dropped: a trace that
//! cannot be written must never fail the turn it describes. The lock is a
//! std one, never held across an await, so the turn's future stays `Send`.
//!
//! **The caller masks.** A tool's arguments and result reach a span already
//! masked by the same code that masks them for the model (`tool_loop`), and a
//! round's thinking is masked and capped by [`OpenSpan::thinking`]'s caller —
//! the engine's own redactor sits above `titi-core` and below nothing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use titi_core::trace::{Span, SpanKind, SpanStatus, TraceWriter};

/// The span sink the surface hands the engine and the engine hands down.
///
/// `None` inside means "no traces here" — a test, or a surface that could not
/// open the file — and every call site no-ops rather than branching.
pub type SpanSink = Arc<Mutex<Option<SpanRecorder>>>;

/// Milliseconds since the Unix epoch, the unit a span's stamps are in.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes one session's spans, a turn file at a time.
pub struct SpanRecorder {
    agent_dir: PathBuf,
    session_id: String,
    writer: Option<TraceWriter>,
    /// The turn currently open, and the id of the span that frames it. A span
    /// with no parent of its own — an agent, a fallback — hangs from it.
    turn: u64,
    turn_span: String,
    next_id: u64,
}

impl SpanRecorder {
    /// Binds a recorder to a session. No file is opened until
    /// [`begin_turn`](Self::begin_turn) names the turn.
    pub fn new(agent_dir: PathBuf, session_id: String) -> Self {
        Self {
            agent_dir,
            session_id,
            writer: None,
            turn: 0,
            turn_span: String::new(),
            next_id: 0,
        }
    }

    /// The session these spans belong to, which is their `trace_id`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Opens this turn's file, and answers the number it got.
    ///
    /// The engine numbers a turn from its own runtime, which starts at 1 in
    /// every process — so a resumed session's first turn asks to be "turn 1"
    /// again. The file is named for the *session's* own turn ordinal instead:
    /// the next number this session has not used, which is the same number
    /// `titi trace --turn N` counts and the same one the turn's span is named
    /// after. Why it matters: without it a second run of a session appends to
    /// (or reopens) the first turn's file, and two turns' spans land in one
    /// tree. Ids start over per turn: they are unique within a turn's file,
    /// which is the only place they are compared.
    fn begin_turn(&mut self, turn: u64) -> u64 {
        self.next_id = 0;
        self.turn_span.clear();
        let mut number = turn;
        while titi_core::trace::turn_path(&self.agent_dir, &self.session_id, number).exists() {
            number += 1;
        }
        self.turn = number;
        self.writer = TraceWriter::open(&self.agent_dir, &self.session_id, number).ok();
        number
    }

    /// The turn on screen, for a span that wants to say which one it is.
    pub fn turn(&self) -> u64 {
        self.turn
    }

    /// The id of the span framing the open turn, once one has been opened.
    pub fn turn_span(&self) -> &str {
        &self.turn_span
    }

    /// Mints the next span id.
    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("s{}", self.next_id)
    }

    /// Appends one finished span and flushes it.
    ///
    /// Flushed per span rather than buffered until the turn ends: a trace is
    /// read while the session is still running (`/trace`, a cancelled turn),
    /// and one line per span is cheap.
    fn write(&mut self, span: &Span) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.append(span);
            let _ = writer.flush();
        }
    }
}

/// Opens a turn on the sink, if there is one, and answers the turn number the
/// session gave it (`None` when nothing is traced).
pub fn begin_turn(sink: &SpanSink, turn: u64) -> Option<u64> {
    let Ok(mut guard) = sink.lock() else {
        return None;
    };
    let recorder = guard.as_mut()?;
    Some(recorder.begin_turn(turn))
}

/// The id of the span framing the open turn, if there is one.
pub fn turn_span(sink: &SpanSink) -> Option<String> {
    let guard = sink.lock().ok()?;
    let recorder = guard.as_ref()?;
    let id = recorder.turn_span();
    (!id.is_empty()).then(|| id.to_owned())
}

/// One span in progress.
///
/// The id is minted when it opens (so a caller can name it as a parent before
/// the span is finished), the parent is the caller's, and the span is written
/// when it finishes. A sink with no recorder yields a dead handle: every
/// method is a no-op and nothing is written.
pub struct OpenSpan {
    inner: Option<(SpanSink, Span)>,
    /// The turn's frame is remembered by the recorder, so a later span with no
    /// parent of its own — an agent's, a fallback's — can hang from it.
    frame: bool,
}

impl OpenSpan {
    /// Opens a span under `parent` (a turn-level span passes `None`).
    pub fn open(
        sink: &SpanSink,
        parent: Option<String>,
        kind: SpanKind,
        name: impl Into<String>,
    ) -> Self {
        let Ok(mut guard) = sink.lock() else {
            return Self::dead();
        };
        let Some(recorder) = guard.as_mut() else {
            return Self::dead();
        };
        let trace_id = recorder.session_id().to_owned();
        let span_id = recorder.id();
        let span = Span::new(trace_id, span_id, kind, name, now_ms());
        let span = match parent {
            Some(parent) => span.with_parent(parent),
            None => span,
        };
        let frame = kind == SpanKind::Turn;
        if frame {
            recorder.turn_span = span.span_id.clone();
        }
        Self {
            inner: Some((Arc::clone(sink), span)),
            frame,
        }
    }

    /// Opens a span that starts earlier than now — a tool call whose start was
    /// taken before it was allowed to run.
    pub fn open_at(
        sink: &SpanSink,
        parent: Option<String>,
        kind: SpanKind,
        name: impl Into<String>,
        start_ms: u64,
    ) -> Self {
        let mut span = Self::open(sink, parent, kind, name);
        if let Some((_, held)) = span.inner.as_mut() {
            held.start_ms = start_ms;
        }
        span
    }

    fn dead() -> Self {
        Self {
            inner: None,
            frame: false,
        }
    }

    /// The span's id, for a caller that wants to be its children's parent.
    pub fn id(&self) -> Option<&str> {
        self.inner.as_ref().map(|(_, span)| span.span_id.as_str())
    }

    /// Whether this is the turn's framing span (kept out of the tree view, as
    /// the turn is the view's own header).
    pub fn is_frame(&self) -> bool {
        self.frame
    }

    pub fn attr(mut self, key: impl Into<String>, value: Value) -> Self {
        if let Some((_, span)) = self.inner.as_mut() {
            span.attributes.insert(key.into(), value);
        }
        self
    }

    pub fn tokens(mut self, input: u64, output: u64, cached: u64, reasoning: u64) -> Self {
        if let Some((_, span)) = self.inner.as_mut() {
            span.input_tokens = input;
            span.output_tokens = output;
            span.cached_tokens = cached;
            span.reasoning_tokens = reasoning;
        }
        self
    }

    pub fn cost(mut self, micro_usd: Option<u64>) -> Self {
        if let Some((_, span)) = self.inner.as_mut() {
            span.cost_micro_usd = micro_usd;
        }
        self
    }

    /// Records the round's reasoning. Already masked by the caller; trimmed by
    /// the model itself (`THINKING_CAP_CHARS`).
    pub fn thinking(mut self, text: Option<String>) -> Self {
        if let Some((_, span)) = self.inner.as_mut()
            && let Some(text) = text
        {
            span.thinking = Some(titi_core::trace::cap_thinking(&text));
        }
        self
    }

    pub fn status(mut self, status: SpanStatus) -> Self {
        if let Some((_, span)) = self.inner.as_mut() {
            span.status = status;
        }
        self
    }

    /// Marks the span failed in place, for a handle that is held across a
    /// return: the drop is what writes it, so there is nothing to reassign.
    pub fn mark_failed(&mut self, message: impl Into<String>) {
        if let Some((_, span)) = self.inner.as_mut() {
            span.status = SpanStatus::Error;
            span.error = Some(message.into());
        }
    }

    /// Sets the status in place, as [`mark_failed`](Self::mark_failed).
    pub fn set_status(&mut self, status: SpanStatus) {
        if let Some((_, span)) = self.inner.as_mut() {
            span.status = status;
        }
    }

    /// Marks the span failed, with the words the failure came in.
    pub fn failed(mut self, message: impl Into<String>) -> Self {
        if let Some((_, span)) = self.inner.as_mut() {
            span.status = SpanStatus::Error;
            span.error = Some(message.into());
        }
        self
    }

    /// Closes the span at `end_ms` and writes it. A span dropped without a
    /// `finish` — an early return on a failure path — is written here too, at
    /// the moment it is dropped, so a trace never loses the span that explains
    /// why the turn stopped.
    pub fn finish(mut self, end_ms: u64) {
        if let Some((sink, mut span)) = self.inner.take() {
            span.end_ms = end_ms.max(span.start_ms);
            write(&sink, span);
        }
    }
}

impl Drop for OpenSpan {
    fn drop(&mut self) {
        if let Some((sink, mut span)) = self.inner.take() {
            span.end_ms = now_ms().max(span.start_ms);
            write(&sink, span);
        }
    }
}

/// Writes one span through the sink, if it still has a recorder.
fn write(sink: &SpanSink, span: Span) {
    if let Ok(mut guard) = sink.lock()
        && let Some(recorder) = guard.as_mut()
    {
        recorder.write(&span);
    }
}
