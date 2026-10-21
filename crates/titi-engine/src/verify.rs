//! The turn-end verifier: a command the user trusts, run when the model says it
//! is done.
//!
//! A model that edited files and stops is claiming the work holds. This runs
//! the user's own check (`verify.command`) before the turn is allowed to end
//! and hands a failure back to the model once, so the next thing the user
//! reads is a result that was checked rather than one that was hoped for.
//!
//! The engine never decides *which* command: the surface resolves
//! `verify.command` from the user's layers or, for a repository's own, from a
//! command the user approved (`EngineCommand::SetVerify`). A repository's
//! command is code, and it never reaches this module unapproved.
//!
//! The run is bounded as `bash` is: 300 s, then the whole process group is
//! killed; output is cut at both ends, stripped of terminal escapes, masked,
//! and only its tail goes to the model. A cancel stops it.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use smol_str::SmolStr;
use titi_providers::{ChatMessage, Role};
use titi_tools::{Interrupt, PipeError};
use tokio::sync::mpsc;

use crate::protocol::{EngineEvent, TurnId, VerifyOutcome};
use crate::tool_loop::mask;

/// How long the command may run: the same bound a `bash` call gets.
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// Lines of a failure's output the model is shown, from the end: where a
/// build or a test runner prints what broke.
const TAIL_LINES: usize = 60;

/// Characters of a failure's output the model is shown.
const TAIL_CHARS: usize = 6_000;

#[derive(Clone)]
struct Spec {
    command: String,
    on_edit: bool,
}

struct Inner {
    spec: RwLock<Option<Spec>>,
    cwd: PathBuf,
    interrupt: Interrupt,
    mask_ips: bool,
}

/// The session's verify command. Cheap to clone; every clone sees `set`.
#[derive(Clone)]
pub struct Verifier {
    inner: Arc<Inner>,
}

impl Verifier {
    pub fn new(cwd: PathBuf, interrupt: Interrupt, mask_ips: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                spec: RwLock::new(None),
                cwd,
                interrupt,
                mask_ips,
            }),
        }
    }

    /// Replaces the command. A blank one clears it: there is nothing to run,
    /// and nothing is guessed in its place.
    pub fn set(&self, command: Option<String>, on_edit: bool) {
        let spec = command
            .map(|command| command.trim().to_owned())
            .filter(|command| !command.is_empty())
            .map(|command| Spec { command, on_edit });
        if let Ok(mut slot) = self.inner.spec.write() {
            *slot = spec;
        }
    }

    fn spec(&self) -> Option<Spec> {
        self.inner.spec.read().ok().and_then(|slot| slot.clone())
    }

    /// Starts the verification of one turn. `edits` is the session's count of
    /// successful file changes as the turn begins.
    pub fn for_turn(&self, edits: u64) -> TurnVerify {
        TurnVerify {
            verifier: self.clone(),
            baseline: edits,
            ran: false,
            retried: false,
            recheck: false,
        }
    }
}

/// One turn's verification state: whether it has run, and whether the model
/// has already been sent a failure.
pub struct TurnVerify {
    verifier: Verifier,
    /// The edit count the next run measures new edits against: the count at
    /// the turn's start, then the count when the last run began.
    baseline: u64,
    ran: bool,
    /// A failure went back to the model. Only one does, per turn.
    retried: bool,
    /// The model was just told of a failure, so the next end of turn runs the
    /// command again whether or not it edited anything.
    recheck: bool,
}

impl TurnVerify {
    /// Whether the end of the turn must run the command. `edits` is the
    /// session's count of successful file changes now.
    pub fn due(&self, edits: u64) -> bool {
        let Some(spec) = self.verifier.spec() else {
            return false;
        };
        self.recheck || edits > self.baseline || (!spec.on_edit && !self.ran)
    }

    /// Runs the command and reports it. `Some(message)` is a failure to hand
    /// the model, which continues; `None` means the turn may end, whether
    /// because it passed, because the model has had its one retry, or because
    /// the run was cancelled. `can_retry` is false once the turn has no tool
    /// round left: a model that cannot edit cannot repair anything.
    pub async fn finish(
        &mut self,
        turn_id: TurnId,
        edits: u64,
        can_retry: bool,
        events: &mpsc::Sender<EngineEvent>,
    ) -> Option<ChatMessage> {
        let spec = self.verifier.spec()?;
        self.baseline = edits;
        self.ran = true;
        self.recheck = false;
        let inner = &self.verifier.inner;
        let shown: SmolStr = mask(&spec.command, inner.mask_ips).into();
        let _ = events
            .send(EngineEvent::VerifyStarted {
                turn_id,
                command: shown.clone(),
            })
            .await;
        let started = Instant::now();
        let command = spec.command.clone();
        let cwd = inner.cwd.clone();
        let interrupt = inner.interrupt.clone();
        let raw = tokio::task::spawn_blocking(move || {
            titi_tools::pipe::run(&command, &cwd, TIMEOUT, &interrupt)
        })
        .await
        .unwrap_or_else(|error| {
            Err(PipeError::Spawn {
                reason: error.to_string(),
            })
        });
        let ms = started.elapsed().as_millis() as u64;
        let report = match raw {
            Ok(run) => Report {
                outcome: if run.success {
                    VerifyOutcome::Passed
                } else {
                    VerifyOutcome::Failed
                },
                exit_code: run.exit_code,
                headline: match run.exit_code {
                    Some(code) => format!("exit {code}"),
                    None => "killed by a signal".to_owned(),
                },
                output: run.output,
                started: true,
            },
            Err(PipeError::TimedOut { after, output }) => Report {
                outcome: VerifyOutcome::TimedOut,
                exit_code: None,
                headline: format!("timed out after {}s", after.as_secs()),
                output,
                started: true,
            },
            // A cancel ends the turn; there is no result to report.
            Err(PipeError::Interrupted { .. }) => return None,
            Err(error) => Report {
                outcome: VerifyOutcome::Failed,
                exit_code: None,
                headline: "did not run".to_owned(),
                output: error.to_string(),
                started: false,
            },
        };
        let failed = report.outcome != VerifyOutcome::Passed;
        let retrying = failed && can_retry && !self.retried && report.started;
        let _ = events
            .send(EngineEvent::VerifyFinished {
                turn_id,
                command: shown.clone(),
                outcome: report.outcome,
                exit_code: report.exit_code,
                ms,
                retrying,
            })
            .await;
        if !retrying {
            return None;
        }
        self.retried = true;
        self.recheck = true;
        Some(ChatMessage {
            role: Role::User,
            content: format!(
                "verification failed: {shown} {}\n\n{}",
                report.headline,
                tail(&mask(
                    &titi_tools::ansi::plain(&report.output),
                    inner.mask_ips
                ))
            )
            .into(),
            ..Default::default()
        })
    }
}

struct Report {
    outcome: VerifyOutcome,
    exit_code: Option<i32>,
    /// `exit 1`, `timed out after 300s`: what follows the command in the
    /// message the model reads.
    headline: String,
    output: String,
    /// Whether the command ran at all. One that would not start is the user's
    /// configuration to fix, not something the model can repair.
    started: bool,
}

/// The end of `output`: at most [`TAIL_LINES`] lines and [`TAIL_CHARS`]
/// characters, with a note when the front was left out.
fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let from = lines.len().saturating_sub(TAIL_LINES);
    let mut kept = lines[from..].join("\n");
    let mut cut = from > 0;
    let chars = kept.chars().count();
    if chars > TAIL_CHARS {
        kept = kept.chars().skip(chars - TAIL_CHARS).collect();
        cut = true;
    }
    if cut {
        format!("[output cut to its end]\n{kept}")
    } else {
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_output_is_kept_whole() {
        assert_eq!(tail("a\nb\n"), "a\nb");
    }

    #[test]
    fn a_long_output_keeps_its_end_and_says_so() {
        let output: String = (0..200).map(|n| format!("line {n}\n")).collect();
        let kept = tail(&output);
        assert!(kept.starts_with("[output cut to its end]\n"));
        assert!(kept.ends_with("line 199"), "{kept}");
        assert!(!kept.contains("line 100\n"), "{kept}");
        assert_eq!(kept.lines().count(), TAIL_LINES + 1);
    }

    #[test]
    fn one_enormous_line_is_cut_by_characters() {
        let kept = tail(&"é".repeat(TAIL_CHARS * 3));
        assert!(kept.chars().count() <= TAIL_CHARS + 30, "{}", kept.len());
    }

    #[test]
    fn a_blank_command_clears_the_verifier() {
        let verifier = Verifier::new(PathBuf::from("."), Interrupt::new(), false);
        verifier.set(Some("  ".into()), true);
        assert!(!verifier.for_turn(0).due(5));
        verifier.set(Some("true".into()), true);
        assert!(verifier.for_turn(0).due(5));
        verifier.set(None, true);
        assert!(!verifier.for_turn(0).due(5));
    }

    #[test]
    fn on_edit_waits_for_an_edit_and_off_runs_once_regardless() {
        let verifier = Verifier::new(PathBuf::from("."), Interrupt::new(), false);
        verifier.set(Some("true".into()), true);
        assert!(!verifier.for_turn(3).due(3), "no edit this turn");
        assert!(verifier.for_turn(3).due(4));
        verifier.set(Some("true".into()), false);
        assert!(verifier.for_turn(3).due(3), "on_edit off: every turn");
    }
}
