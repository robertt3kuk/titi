use std::io::{self, BufRead, Write};

use serde::Deserialize;
use titi_engine::{Engine, EngineCommand, EngineEvent};

/// Wire protocol version for the headless JSONL surface. A client declares it
/// per frame; the runner refuses a version it does not implement instead of
/// misreading fields it does not know.
pub const RPC_PROTOCOL: u32 = 1;

/// One inbound line: `{"v": 1, "command": {...}}`. `v` may be omitted, which
/// means "the runner's current version".
#[derive(Debug, Deserialize)]
pub struct HeadlessFrame {
    #[serde(default)]
    pub v: Option<u32>,
    pub command: EngineCommand,
}

/// Why a line could not become a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The client speaks a version this runner does not implement.
    UnsupportedVersion { client: u32, runner: u32 },
    /// The line is not a frame at all.
    Malformed(String),
}

impl FrameError {
    /// The `error` string written back on stdout.
    pub fn message(&self) -> String {
        match self {
            FrameError::UnsupportedVersion { client, runner } => {
                format!("unsupported protocol version {client}; this runner speaks {runner}")
            }
            FrameError::Malformed(reason) => reason.clone(),
        }
    }
}

/// Decodes one inbound line, enforcing the protocol version.
pub fn decode(line: &str) -> Result<HeadlessFrame, FrameError> {
    let frame: HeadlessFrame =
        serde_json::from_str(line).map_err(|error| FrameError::Malformed(error.to_string()))?;
    if let Some(client) = frame.v
        && client != RPC_PROTOCOL
    {
        return Err(FrameError::UnsupportedVersion {
            client,
            runner: RPC_PROTOCOL,
        });
    }
    Ok(frame)
}

/// Runs the JSONL surface. `log` receives the same transcript the TUI would
/// write, so a headless run is resumable too.
pub async fn run(
    mut engine: Engine,
    log: Option<crate::session_log::SessionLog>,
) -> io::Result<i32> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    // The handshake: a client can pin the version before sending anything.
    writeln!(
        stdout,
        "{}",
        serde_json::json!({"ready": true, "protocol": RPC_PROTOCOL})
    )?;
    stdout.flush()?;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let frame = match decode(&line) {
            Ok(frame) => frame,
            Err(error) => {
                writeln!(
                    stdout,
                    "{}",
                    serde_json::json!({"error": error.message(), "protocol": RPC_PROTOCOL})
                )?;
                stdout.flush()?;
                continue;
            }
        };
        let shutdown = matches!(frame.command, EngineCommand::Shutdown);
        // A submitted prompt is part of the conversation; a bare command is not.
        if let (Some(log), EngineCommand::SubmitPrompt { text } | EngineCommand::Steer { text }) =
            (&log, &frame.command)
        {
            let _ = log.user(text);
        }
        if engine.send(frame.command).await.is_err() {
            return Ok(1);
        }
        let mut reply = String::new();
        while let Some(event) = engine.recv().await {
            writeln!(
                stdout,
                "{}",
                serde_json::to_string(&event).unwrap_or_default()
            )?;
            stdout.flush()?;
            if let EngineEvent::StreamDelta { text, .. } = &event {
                reply.push_str(text);
            }
            if matches!(event, EngineEvent::TurnFinished { .. })
                && let Some(log) = &log
            {
                let _ = log.assistant(&reply);
                reply.clear();
            }
            let terminal = matches!(
                event,
                EngineEvent::TurnFinished { .. }
                    | EngineEvent::Failed { .. }
                    | EngineEvent::Cancelled { .. }
                    | EngineEvent::AgentFinished { .. }
                    | EngineEvent::GoalFinished { .. }
            );
            if terminal {
                break;
            }
        }
        if shutdown {
            break;
        }
    }
    Ok(0)
}

/// One prompt, no JSONL client: submit it, stream the reply to stderr, and
/// stop when the turn ends. Events still go to stdout as JSONL.
pub async fn run_prompt(
    mut engine: Engine,
    log: Option<crate::session_log::SessionLog>,
    prompt: &str,
) -> io::Result<i32> {
    if let Some(log) = &log {
        let _ = log.user(prompt);
    }
    if engine
        .send(EngineCommand::SubmitPrompt {
            text: prompt.into(),
        })
        .await
        .is_err()
    {
        return Ok(1);
    }
    let mut stdout = io::stdout();
    let mut reply = String::new();
    let mut failed = false;
    while let Some(event) = engine.recv().await {
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&event).unwrap_or_default()
        )?;
        stdout.flush()?;
        if let EngineEvent::StreamDelta { text, .. } = &event {
            reply.push_str(text);
            eprint!("{text}");
        }
        if matches!(event, EngineEvent::Failed { .. }) {
            failed = true;
        }
        if matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        ) {
            break;
        }
    }
    if !reply.is_empty() {
        eprintln!();
        if let Some(log) = &log {
            let _ = log.assistant(&reply);
        }
    }
    Ok(if failed { 1 } else { 0 })
}

/// Exit code for a headless goal that never reached a report: the engine
/// stopped talking, or the turn failed outright. Same code a rejected goal
/// gets — a bot must not read silence as success.
const GOAL_UNFINISHED: i32 = 3;

/// One goal, no JSONL client: run the coder/reviewer loop and exit with the
/// code CI reads (`0` pass, `1` partial, `3` anything else).
///
/// Events still go to stdout as JSONL; the report line goes to stderr, where
/// a shell script can read it without parsing the stream.
pub async fn run_goal(
    mut engine: Engine,
    log: Option<crate::session_log::SessionLog>,
    goal: &str,
) -> io::Result<i32> {
    if let Some(log) = &log {
        let _ = log.user(&format!("/goal {goal}"));
    }
    if engine
        .send(EngineCommand::RunGoal { text: goal.into() })
        .await
        .is_err()
    {
        eprintln!("goal: the engine is gone");
        return Ok(GOAL_UNFINISHED);
    }
    let mut stdout = io::stdout();
    while let Some(event) = engine.recv().await {
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&event).unwrap_or_default()
        )?;
        stdout.flush()?;
        match &event {
            EngineEvent::GoalFinished { report } => {
                eprintln!("{report}");
                if let Some(log) = &log {
                    let _ = log.assistant(report);
                }
            }
            EngineEvent::Failed { message, .. } => eprintln!("goal: {message}"),
            _ => {}
        }
        if let Some(code) = goal_exit_code(&event) {
            return Ok(code);
        }
    }
    eprintln!("goal: the engine stopped before the goal finished");
    Ok(GOAL_UNFINISHED)
}

/// The exit code this event ends the run with, or `None` while the goal is
/// still going.
///
/// The engine reports a goal as one line, so the verdict comes back out of
/// it ([`titi_engine::goal::goal_exit_code`]). A failed turn never produces
/// that line, and a missing verdict is not a pass.
fn goal_exit_code(event: &EngineEvent) -> Option<i32> {
    match event {
        EngineEvent::GoalFinished { report } => Some(titi_engine::goal::goal_exit_code(report)),
        EngineEvent::Failed { .. } => Some(GOAL_UNFINISHED),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use titi_engine::{GoalOutcome, GoalStop, Verdict, goal_report};

    /// The event the engine sends for a goal that ended this way, built from
    /// the engine's own report line so a change to it fails here.
    fn finished(stop: GoalStop, verdict: Option<Verdict>) -> EngineEvent {
        let outcome = GoalOutcome {
            stop,
            verdict,
            rounds: 2,
            patch: None,
            review: None,
            gate: None,
            changes: Vec::new(),
            error: None,
        };
        EngineEvent::GoalFinished {
            report: goal_report(&outcome).into(),
        }
    }

    #[test]
    fn a_passed_goal_exits_zero_a_partial_one_and_a_rejected_three() {
        assert_eq!(
            goal_exit_code(&finished(GoalStop::Passed, Some(Verdict::Pass))),
            Some(0)
        );
        assert_eq!(
            goal_exit_code(&finished(GoalStop::RoundCap, Some(Verdict::Partial))),
            Some(1)
        );
        assert_eq!(
            goal_exit_code(&finished(GoalStop::RoundCap, Some(Verdict::Fail))),
            Some(3)
        );
        assert_eq!(
            goal_exit_code(&finished(GoalStop::Oscillation, Some(Verdict::Fail))),
            Some(3)
        );
    }

    /// A goal that never got a verdict — cancelled, a provider error, a check
    /// that could not run — is neither a pass nor a half-result.
    #[test]
    fn a_goal_without_a_verdict_exits_three() {
        for stop in [
            GoalStop::Cancelled,
            GoalStop::Error,
            GoalStop::GateUnavailable,
        ] {
            assert_eq!(goal_exit_code(&finished(stop, None)), Some(3), "{stop:?}");
        }
        assert_eq!(
            goal_exit_code(&EngineEvent::Failed {
                turn_id: None,
                reason: titi_providers::ErrorReason::Connection,
                message: "no key for the primary model".into(),
            }),
            Some(3)
        );
    }

    #[test]
    fn an_event_that_is_not_the_end_does_not_decide_a_code() {
        assert_eq!(
            goal_exit_code(&EngineEvent::Notice {
                message: "goal: round 2".into()
            }),
            None
        );
    }
}
