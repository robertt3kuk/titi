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
