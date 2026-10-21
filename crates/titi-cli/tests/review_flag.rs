//! `titi --review`: a verdict on the uncommitted change, as an exit code.
//!
//! The binary runs against a throwaway git workspace. Where a model is
//! needed, a one-reply HTTP server on loopback stands in for the provider.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A committed repository, with `a.txt` modified when `dirty`.
fn workspace(dirty: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    if dirty {
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("write");
    }
    dir
}

/// An agent directory whose only provider is `base_url`.
fn agent_dir(base_url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(
        dir.path().join("config.yml"),
        format!(
            "providers:\n  - id: fake\n    api: openai-completions\n    base_url: {base_url}\n    credential_required: false\nmodels:\n  - id: fake/scripted\n    provider: fake\n    wire_model: fake\n    context_window: 32000\n"
        ),
    )
    .expect("config");
    dir
}

/// Answers every request with `reply` as one SSE stream.
fn serve(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut seen = Vec::new();
            let mut buf = [0_u8; 4096];
            // Headers, then however many body bytes they promise.
            let (mut head_end, mut want) = (None, 0_usize);
            loop {
                let Ok(read) = stream.read(&mut buf) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..read]);
                if head_end.is_none()
                    && let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    head_end = Some(at + 4);
                    let head = String::from_utf8_lossy(&seen[..at]).to_lowercase();
                    want = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                }
                if let Some(end) = head_end
                    && seen.len() >= end + want
                {
                    break;
                }
            }
            let chunk = |delta: serde_json::Value, finish: Option<&str>| {
                format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "id": "c", "object": "chat.completion.chunk", "model": "fake",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                    })
                )
            };
            let body = format!(
                "{}{}data: [DONE]\n\n",
                chunk(serde_json::json!({"content": reply}), None),
                chunk(serde_json::json!({}), Some("stop")),
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

fn run(args: &[&str], workspace: &Path, agent: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_titi"))
        .args(args)
        .current_dir(workspace)
        .env("TITI_AGENT_DIR", agent)
        .env("TITI_NO_ATLAS", "1")
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs")
}

#[test]
fn a_changed_workspace_exits_by_the_verdict() {
    for (reply, code, word) in [
        ("VERDICT: PASS — fine", 0, "PASS"),
        (
            "VERDICT: FAIL — missing test\n- a.txt:2 — untested",
            3,
            "FAIL",
        ),
        ("VERDICT: PARTIAL — cannot tell", 4, "PARTIAL"),
        ("no verdict here at all", 4, "PARTIAL"),
    ] {
        let ws = workspace(true);
        let agent = agent_dir(&serve(reply));
        let output = run(&["--review"], ws.path(), agent.path());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(code), "{reply}: {stderr}");
        let line: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
        assert_eq!(line["verdict"], word, "{stdout}");
        assert_eq!(line["files"], serde_json::json!(["a.txt"]), "{stdout}");
        assert_eq!(stdout.lines().count(), 1, "stdout is the verdict alone");
    }
}

#[test]
fn nothing_changed_exits_five_without_asking_a_model() {
    let ws = workspace(false);
    // Nothing listens here: a model call would fail, not return a verdict.
    let agent = agent_dir("http://127.0.0.1:9/v1");
    let output = run(&["--review"], ws.path(), agent.path());
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    assert!(line["verdict"].is_null(), "{stdout}");
    assert_eq!(line["reason"], "nothing to review");
}

#[test]
fn review_with_a_prompt_or_a_goal_is_a_usage_error() {
    let ws = workspace(true);
    let agent = agent_dir("http://127.0.0.1:9/v1");
    for args in [
        &["--review", "extra words"][..],
        &["--review", "--goal", "x"],
        &["--goal", "x", "--review"],
        &["--review", "--prompt", "x"],
    ] {
        let output = run(args, ws.path(), agent.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn help_documents_the_flag_and_its_exit_codes() {
    let output = Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--help")
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--review"), "{stdout}");
    assert!(stdout.contains("5 nothing"), "{stdout}");
}
