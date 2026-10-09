#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use titi_engine::{EngineCommand, EngineEvent, TurnId};
use titi_providers::StopReason;

#[test]
fn engine_events_round_trip_as_jsonl() {
    let event = EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    };
    let line = serde_json::to_string(&event).unwrap();
    let parsed: EngineEvent = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed, event);

    let command = EngineCommand::SubmitPrompt { text: "hi".into() };
    let frame = serde_json::json!({ "command": command });
    let decoded: EngineCommand = serde_json::from_value(frame["command"].clone()).unwrap();
    assert_eq!(decoded, command);

    // A question's answer is a frame like any other command: the surface the
    // model asks is whichever one reads the event stream, JSONL included.
    let answer = EngineCommand::AnswerAsk {
        request_id: "ask-1".into(),
        answer: titi_tools::AskAnswer::Chosen(vec!["blue".into()]),
    };
    let frame = serde_json::json!({ "command": answer });
    let decoded: EngineCommand = serde_json::from_value(frame["command"].clone()).unwrap();
    assert_eq!(decoded, answer);
}

#[test]
fn frame_version_is_optional_and_enforced() {
    use titi_cli::headless::{FrameError, RPC_PROTOCOL, decode};

    // Omitted version means "current".
    let bare = decode(r#"{"command":"Cancel"}"#).unwrap();
    assert_eq!(bare.v, None);
    assert!(matches!(bare.command, EngineCommand::Cancel));

    // The current version is accepted explicitly.
    let pinned = decode(&format!(r#"{{"v":{RPC_PROTOCOL},"command":"Cancel"}}"#)).unwrap();
    assert_eq!(pinned.v, Some(RPC_PROTOCOL));

    // A future version is refused with a message naming both sides.
    let err = decode(r#"{"v":99,"command":"Cancel"}"#).unwrap_err();
    assert_eq!(
        err,
        FrameError::UnsupportedVersion {
            client: 99,
            runner: RPC_PROTOCOL
        }
    );
    assert!(err.message().contains("99"));
    assert!(err.message().contains(&RPC_PROTOCOL.to_string()));

    // A line that is not a frame is malformed, not a version error.
    assert!(matches!(decode("not json"), Err(FrameError::Malformed(_))));
}

/// The defect this guards: a lone `SwitchModel` on a closed pipe used to
/// hang. The engine emitted nothing for it and `headless::run` parked on the
/// events channel, so `printf … | titi --headless` sat there until the
/// client's own timeout and never printed the switch. Now the command
/// answers and a closed stdin ends the run.
/// A headless run has no screen, so a pinned model that could not be honoured
/// is said on stderr — the same line the screen would carry as a note — and the
/// run starts on the model it would have used anyway.
#[test]
fn a_pinned_model_that_is_not_available_is_named_on_stderr() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(
        dir.path().join("config.yml"),
        "providers:\n  - id: fake\n    api: openai-completions\n    base_url: http://127.0.0.1:9/v1\n    credential_required: false\nmodels:\n  - id: fake/scripted\n    provider: fake\n    wire_model: fake\n    context_window: 32000\nmodelRoles:\n  default: nope/nope\n",
    )
    .expect("write the config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--headless")
        .env("TITI_AGENT_DIR", dir.path())
        .env("TITI_NO_GENOME", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    drop(child.stdin.take());
    let mut stderr = child.stderr.take().expect("piped stderr");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let _ = child.wait();
    let said = reader.join().expect("the reader thread");
    assert!(
        said.contains("modelRoles.default: nope/nope is not available"),
        "the pin is named on stderr: {said:?}"
    );
    assert!(
        said.contains("starting on fake/scripted"),
        "and so is what the run started on: {said:?}"
    );
}

#[test]
fn a_lone_switch_model_answers_and_the_runner_exits() {
    let dir = tempfile::tempdir().expect("temp");
    let mut child = Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--headless")
        .env("TITI_AGENT_DIR", dir.path())
        .env("TITI_NO_GENOME", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary runs");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(br#"{"v":1,"command":{"SwitchModel":{"model":"openai-codex/gpt-5.5"}}}"#)
            .expect("write the frame");
        // Dropping stdin closes the pipe: exactly the `printf |` invocation.
    }
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the runner did not exit after stdin closed: it is parked");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = reader.join().expect("the reader thread");

    assert!(status.success(), "{status:?} — stdout: {out}");
    assert!(out.contains(r#""ready":true"#), "{out}");
    let line = out
        .lines()
        .find(|line| line.contains("ModelSwitched"))
        .unwrap_or_else(|| panic!("no switch event on stdout: {out}"));
    assert!(line.contains(r#""turn_id":null"#), "{line}");
    assert!(line.contains(r#""from":"#), "{line}");
    assert!(line.contains(r#""to":"openai-codex/gpt-5.5""#), "{line}");
}
