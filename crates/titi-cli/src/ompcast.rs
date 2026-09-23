//! `.ompcast`: a session recorded as it was seen, and played back offline.
//!
//! One line of JSON per record, in the order the surface received it:
//! `{"t":<ms since the recording started>,"kind":"event"|"input","payload":…}`.
//! An `event` payload is an [`EngineEvent`] exactly as the engine serialises
//! it — the type already derives `Serialize`/`Deserialize`, so the cast is the
//! engine's own vocabulary rather than a projection of it that would drift the
//! first time a variant changes.
//!
//! The recorder sits where the chat loop receives events, which is downstream
//! of the engine's masking: a tool output reaches this module already
//! redacted, so a key cannot enter the file here.
//!
//! Replay drives the same [`Chat`] the live screen drives and prints the
//! transcript lines it produces. Nothing is sent anywhere: a cast is a
//! recording, so no model is called and no tool runs.

use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use titi_engine::EngineEvent;

use crate::chat::Chat;

/// The extension a cast file is expected to carry.
pub const EXTENSION: &str = "ompcast";

/// Longest wait replay honours between two records.
///
/// A recording holds whatever gap the user left — a lunch break is a
/// legitimate part of the timeline and nobody wants to sit through it.
pub const MAX_GAP: Duration = Duration::from_secs(2);

/// Why a cast could not be written or read.
#[derive(Debug)]
pub enum CastError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CastError::Io(error) => write!(f, "cast io: {error}"),
            CastError::Json(error) => write!(f, "cast json: {error}"),
        }
    }
}

impl std::error::Error for CastError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CastError::Io(error) => Some(error),
            CastError::Json(error) => Some(error),
        }
    }
}

/// One line of a cast: when it happened, and what it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastRecord {
    /// Milliseconds since the recording started. Relative on purpose: a cast
    /// is played back, not correlated with a wall clock.
    pub t: u64,
    #[serde(flatten)]
    pub body: CastBody,
}

/// What one record carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum CastBody {
    /// An engine event, verbatim.
    Event(EngineEvent),
    /// What the user sent, as the transcript recorded it.
    Input(String),
}

/// A line that was not a record, and why. Replay skips it and says so; a
/// crash mid-write tears the last line, and losing the whole recording over
/// that would be the larger failure.
#[derive(Debug, Clone, PartialEq)]
pub struct CastWarning {
    /// 1-based line number in the file.
    pub line: usize,
    pub reason: String,
}

impl fmt::Display for CastWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}

/// A cast as read from disk.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CastFile {
    pub records: Vec<CastRecord>,
    pub warnings: Vec<CastWarning>,
}

/// Appends records to one cast file.
///
/// Writes are buffered and flushed at turn boundaries and on drop, the same
/// bargain the trajectory recorder makes: a torn final line costs one record
/// and [`read`] skips it.
pub struct CastWriter {
    file: BufWriter<File>,
    start: Instant,
}

impl CastWriter {
    /// Creates (or truncates) the cast at `path`. A cast is one session, so
    /// it is never appended to: two sessions in one file would share a clock
    /// that only one of them started.
    pub fn create(path: &Path) -> Result<Self, CastError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(CastError::Io)?;
        }
        let file = File::create(path).map_err(CastError::Io)?;
        Ok(Self {
            file: BufWriter::new(file),
            start: Instant::now(),
        })
    }

    /// Records one engine event, as the surface received it.
    pub fn event(&mut self, event: &EngineEvent) -> Result<(), CastError> {
        let closes_turn = matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        );
        self.write(CastBody::Event(event.clone()))?;
        if closes_turn {
            self.flush()?;
        }
        Ok(())
    }

    /// Records what the user sent.
    pub fn input(&mut self, text: &str) -> Result<(), CastError> {
        self.write(CastBody::Input(text.to_owned()))
    }

    pub fn flush(&mut self) -> Result<(), CastError> {
        self.file.flush().map_err(CastError::Io)
    }

    fn write(&mut self, body: CastBody) -> Result<(), CastError> {
        let record = CastRecord {
            t: self.start.elapsed().as_millis().min(u64::MAX as u128) as u64,
            body,
        };
        let line = serde_json::to_string(&record).map_err(CastError::Json)?;
        writeln!(self.file, "{line}").map_err(CastError::Io)
    }
}

impl Drop for CastWriter {
    fn drop(&mut self) {
        // Best-effort: the tail of a session must not vanish with the buffer.
        let _ = self.file.flush();
    }
}

/// Reads a cast, keeping every line that parses and reporting the rest.
pub fn read(path: &Path) -> Result<CastFile, CastError> {
    let file = File::open(path).map_err(CastError::Io)?;
    let mut cast = CastFile::default();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(CastError::Io)?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<CastRecord>(text) {
            Ok(record) => cast.records.push(record),
            Err(error) => cast.warnings.push(CastWarning {
                line: index + 1,
                reason: error.to_string(),
            }),
        }
    }
    Ok(cast)
}

/// How fast a cast is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Honour the recorded gaps, capped at [`MAX_GAP`].
    Realtime,
    /// As fast as the records can be rendered.
    Fast,
}

/// Reads `path` and plays it, reporting skipped lines before the transcript.
pub fn replay<W: Write>(path: &Path, pace: Pace, out: &mut W) -> Result<(), CastError> {
    let cast = read(path)?;
    for warning in &cast.warnings {
        writeln!(out, "skipped {warning}").map_err(CastError::Io)?;
    }
    play(&cast.records, pace, out)
}

/// Plays records through the live transcript and writes what appears.
///
/// The records drive [`Chat`], so the text, the tool chips and the notices are
/// the ones the session showed; only the terminal is missing.
pub fn play<W: Write>(records: &[CastRecord], pace: Pace, out: &mut W) -> Result<(), CastError> {
    let mut chat = Chat::new("replay", "replay");
    let mut shown = 0;
    let mut last = 0;
    for record in records {
        if pace == Pace::Realtime {
            let gap = Duration::from_millis(record.t.saturating_sub(last));
            std::thread::sleep(gap.min(MAX_GAP));
        }
        last = record.t;
        match &record.body {
            CastBody::Event(event) => {
                chat.on_event(event.clone());
            }
            CastBody::Input(text) => chat.push_user(text),
        }
        let transcript = chat.transcript();
        for line in &transcript[shown.min(transcript.len())..] {
            writeln!(out, "{:<6} {}", line.kind.as_str(), line.text).map_err(CastError::Io)?;
        }
        shown = transcript.len();
        out.flush().map_err(CastError::Io)?;
    }
    Ok(())
}
