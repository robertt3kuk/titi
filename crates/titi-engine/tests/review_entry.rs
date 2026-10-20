#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! `RunReview` through the session runtime: the scope the surface names, a
//! reviewer that can only read, and no model call when nothing changed.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, REVIEW_MARKER, ReviewReport, Verdict,
};
use titi_providers::{BlockId, MockBody, MockTransport, StopReason, StreamEvent};

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn reply(text: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("r"),
            text: text.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

fn start(root: &Path, transport: &Arc<MockTransport>) -> titi_engine::Engine {
    let captured = Arc::clone(transport);
    let resolver = Arc::new(move |model: &str| {
        Ok(titi_engine::ResolvedModel::without_credential(
            model,
            Arc::clone(&captured) as Arc<dyn titi_providers::Transport>,
        ))
    });
    let mut config = EngineConfig::new("primary");
    config.workspace_root = Some(root.to_path_buf());
    EngineRuntime::start(config, resolver)
}

async fn report(engine: &mut titi_engine::Engine) -> ReviewReport {
    match engine.recv().await.expect("a review report") {
        EngineEvent::ReviewFinished { report } => report,
        other => panic!("expected a review report, got {other:?}"),
    }
}

#[tokio::test]
async fn a_review_reads_the_edit_set_with_read_only_tools() {
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.path().join("other.txt"), "not edited by the agent\n").unwrap();
    let transport = Arc::new(MockTransport::new(vec![reply(
        "VERDICT: FAIL — missing test\n- a.txt:2 — nothing covers the new line",
    )]));
    let mut engine = start(dir.path(), &transport);
    engine
        .send(EngineCommand::RunReview {
            edited: vec!["a.txt".into()],
        })
        .await
        .unwrap();
    let report = report(&mut engine).await;
    assert_eq!(report.verdict, Some(Verdict::Fail));
    assert_eq!(report.reason, "missing test");
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.files, ["a.txt"]);

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let prompt = &requests[0].messages[0].content;
    assert!(prompt.contains(REVIEW_MARKER), "{prompt}");
    assert!(prompt.contains("- a.txt +1/\u{2212}0"), "{prompt}");
    assert!(!prompt.contains("other.txt"), "{prompt}");
    let tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tools.contains(&"read"), "{tools:?}");
    for writer in ["write", "edit", "bash"] {
        assert!(!tools.contains(&writer), "{writer} offered: {tools:?}");
    }
}

#[tokio::test]
async fn without_an_edit_set_the_uncommitted_diff_is_reviewed() {
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    let transport = Arc::new(MockTransport::new(vec![reply("VERDICT: PASS — fine")]));
    let mut engine = start(dir.path(), &transport);
    engine
        .send(EngineCommand::RunReview { edited: Vec::new() })
        .await
        .unwrap();
    let report = report(&mut engine).await;
    assert_eq!(report.verdict, Some(Verdict::Pass));
    assert_eq!(report.files, ["a.txt"]);
}

#[tokio::test]
async fn nothing_changed_calls_no_model_and_names_no_verdict() {
    let dir = repo();
    let transport = Arc::new(MockTransport::new(vec![reply("VERDICT: PASS — fine")]));
    let mut engine = start(dir.path(), &transport);
    engine
        .send(EngineCommand::RunReview { edited: Vec::new() })
        .await
        .unwrap();
    let report = report(&mut engine).await;
    assert_eq!(report.verdict, None, "nothing to review is never a pass");
    assert_eq!(report.reason, "nothing to review");
    assert_eq!(transport.call_count(), 0);
}
