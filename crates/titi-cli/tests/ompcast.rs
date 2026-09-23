//! `.ompcast`: recording a session's event stream and playing it back.
//!
//! The cast is written where the surface receives engine events, which is
//! after the engine has masked them; the key test below pins that a secret in
//! tool output never reaches the file.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use titi_cli::ompcast::{CastBody, CastRecord, CastWriter, Pace, play, read, replay};
use titi_engine::{EngineEvent, TurnId};
use titi_providers::StopReason;

fn stream() -> Vec<EngineEvent> {
    vec![
        EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        },
        EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "looking at the parser".into(),
        },
        EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "read".into(),
        },
        EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            output: "[package]".into(),
            is_error: false,
        },
        EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        },
    ]
}

fn record_stream(path: &Path) {
    let mut writer = CastWriter::create(path).expect("the cast file opens");
    writer
        .input("what is in Cargo.toml?")
        .expect("input records");
    for event in stream() {
        writer.event(&event).expect("event records");
    }
    writer.flush().expect("the cast flushes");
}

#[test]
fn a_recorded_stream_round_trips_through_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.ompcast");
    record_stream(&path);

    assert!(path.exists(), "recording creates the file");
    let cast = read(&path).expect("the cast reads back");
    assert!(cast.warnings.is_empty(), "{:?}", cast.warnings);

    let mut expected = vec![CastBody::Input("what is in Cargo.toml?".to_owned())];
    expected.extend(stream().into_iter().map(CastBody::Event));
    let bodies: Vec<CastBody> = cast
        .records
        .iter()
        .map(|record| record.body.clone())
        .collect();
    assert_eq!(bodies, expected, "same records, same order");

    // Time only moves forward, and the first record starts the clock.
    assert_eq!(cast.records[0].t, 0);
    assert!(
        cast.records.windows(2).all(|pair| pair[0].t <= pair[1].t),
        "{:?}",
        cast.records.iter().map(|r| r.t).collect::<Vec<_>>()
    );
}

#[test]
fn a_torn_line_is_skipped_with_a_warning_and_the_rest_plays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.ompcast");
    record_stream(&path);

    // A crash mid-write leaves a half line, and a stray blank line is not a
    // record either.
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"t\":99,\"kind\":\"event\",\"payload\":{\"TurnFinis");
    text.push('\n');
    text.push('\n');
    text.push_str("{\"t\":100,\"kind\":\"input\",\"payload\":\"after the tear\"}\n");
    std::fs::write(&path, text).unwrap();

    let cast = read(&path).expect("a torn cast still reads");
    assert_eq!(cast.warnings.len(), 1, "{:?}", cast.warnings);
    assert_eq!(cast.warnings[0].line, 7, "{:?}", cast.warnings[0]);
    assert_eq!(
        cast.records.last().map(|record| record.body.clone()),
        Some(CastBody::Input("after the tear".to_owned())),
        "the records after the tear still play"
    );

    let mut out = Vec::new();
    play(&cast.records, Pace::Fast, &mut out).expect("a torn cast plays");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("after the tear"), "{text}");
    assert!(text.contains("looking at the parser"), "{text}");
}

#[test]
fn a_masked_tool_output_keeps_the_key_out_of_the_cast() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.ompcast");

    // The engine masks tool output before it emits `ToolFinished`; the
    // surface records what it received. Same masking call the engine's tool
    // loop makes, so the cast sees exactly what the screen sees.
    let raw = "OPENAI_API_KEY=sk-test-0000000000000000";
    let masked = titi_memory::redact::redact(raw).text;
    let mut writer = CastWriter::create(&path).expect("the cast file opens");
    writer
        .event(&EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            output: masked.into(),
            is_error: false,
        })
        .expect("event records");
    writer.flush().expect("the cast flushes");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("sk-test-0000000000000000"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");

    let mut out = Vec::new();
    replay(&path, Pace::Fast, &mut out).expect("the cast replays");
    let played = String::from_utf8(out).unwrap();
    assert!(!played.contains("sk-test-0000000000000000"), "{played}");
}

#[test]
fn replaying_renders_the_transcript_of_the_recorded_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.ompcast");
    record_stream(&path);

    let mut out = Vec::new();
    replay(&path, Pace::Fast, &mut out).expect("the cast replays");
    let text = String::from_utf8(out).unwrap();

    assert!(text.contains("what is in Cargo.toml?"), "{text}");
    assert!(text.contains("looking at the parser"), "{text}");
    assert!(text.contains("read"), "{text}");
    // The order on screen is the order it happened in.
    let input_at = text.find("what is in Cargo.toml?").unwrap();
    let reply_at = text.find("looking at the parser").unwrap();
    assert!(input_at < reply_at, "{text}");
}

#[test]
fn the_replay_flag_plays_a_cast_without_starting_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.ompcast");
    record_stream(&path);

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--replay")
        .arg(&path)
        .arg("--replay-fast")
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{:?}", output.status);
    assert!(stdout.contains("what is in Cargo.toml?"), "{stdout}");
    assert!(stdout.contains("looking at the parser"), "{stdout}");
}

#[test]
fn replaying_a_missing_cast_fails_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nothing.ompcast");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--replay")
        .arg(&missing)
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("replay"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");

    let mut out = Vec::new();
    let error = replay(&missing, Pace::Fast, &mut out).expect_err("a missing cast is an error");
    assert!(error.to_string().contains("cast"), "{error}");
}

#[test]
fn a_record_is_one_json_line_with_a_relative_timestamp() {
    let record = CastRecord {
        t: 42,
        body: CastBody::Input("hello".to_owned()),
    };
    let line = serde_json::to_string(&record).expect("a record serialises");
    assert_eq!(line, r#"{"t":42,"kind":"input","payload":"hello"}"#);
    assert!(!line.contains('\n'), "one record is one line");
    let back: CastRecord = serde_json::from_str(&line).expect("a record parses");
    assert_eq!(back, record);
}
