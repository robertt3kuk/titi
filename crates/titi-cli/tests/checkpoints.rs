//! Integration tests for the session checkpoint surface: `/checkpoint`,
//! `/checkpoints`, `/rewind`.
//!
//! Contract: `docs/research/reference-product-port/README.md` (E2 — checkpoints).

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::{App, checkpoint_session, default_theme, list_checkpoints, rewind_session};
use titi_core::session::{Role, SessionMeta, SessionStore};
use titi_tui::slash::Route;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

#[test]
fn checkpoint_builtins_are_reserved() {
    let app = app();
    for name in ["checkpoint", "checkpoints", "rewind"] {
        assert_eq!(
            app.route_slash(&format!("/{name}")),
            Route::Builtin(name.to_owned())
        );
    }
    // Arguments do not change the route.
    assert_eq!(
        app.route_slash("/rewind 2"),
        Route::Builtin("rewind".to_owned())
    );
}

#[test]
fn checkpoint_rewind_roundtrip_through_the_helpers() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let sid = store.create(SessionMeta::default()).unwrap();
    store.append(&sid, Role::User, "first").unwrap();

    // A workspace that is not a repo keeps the checkpoint session-only.
    let workspace = tempfile::tempdir().unwrap();
    let summary = checkpoint_session(agent_dir, workspace.path(), &sid).unwrap();
    assert!(!summary.contains("git"), "{summary}");
    assert!(summary.starts_with("checkpoint: 1 entries"), "{summary}");
    store.append(&sid, Role::Assistant, "second").unwrap();
    assert_eq!(store.open(&sid).unwrap().len(), 2);

    assert!(
        list_checkpoints(agent_dir, &sid)
            .unwrap()
            .contains("#1 · 1 entries")
    );
    assert!(
        rewind_session(agent_dir, workspace.path(), &sid, None)
            .unwrap()
            .contains("rewound to checkpoint #1")
    );
    assert_eq!(store.open(&sid).unwrap().len(), 1);
}

#[test]
fn rewind_reports_missing_and_out_of_range_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let sid = store.create(SessionMeta::default()).unwrap();
    store.append(&sid, Role::User, "only").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();

    assert_eq!(
        list_checkpoints(agent_dir, &sid).unwrap(),
        "checkpoints: none"
    );
    assert!(rewind_session(agent_dir, ws, &sid, None).is_err());
    assert!(rewind_session(agent_dir, ws, &sid, Some(1)).is_err());

    checkpoint_session(agent_dir, ws, &sid).unwrap();
    // Checkpoints are 1-based, so 0 and 2 are both rejected.
    assert!(rewind_session(agent_dir, ws, &sid, Some(0)).is_err());
    assert!(rewind_session(agent_dir, ws, &sid, Some(2)).is_err());
    assert!(rewind_session(agent_dir, ws, &sid, Some(1)).is_ok());
}

#[test]
fn an_app_without_a_session_reports_it_instead_of_panicking() {
    let mut app = app();
    assert_eq!(app.session_id(), None);
    app.set_session_id("live");
    assert_eq!(app.session_id(), Some("live"));
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Running the tests from a checkout with staged work used to commit that
/// work into the checkout itself, because the workspace was the process cwd.
#[test]
fn checkpoint_and_rewind_touch_only_the_given_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let agent_dir = tmp.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let sid = store.create(SessionMeta::default()).unwrap();
    store.append(&sid, Role::User, "first").unwrap();

    let repo = tempfile::tempdir().unwrap();
    let ws = repo.path();
    git(ws, &["init", "-q"]);
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    git(ws, &["add", "a.txt"]);
    git(ws, &["commit", "-q", "-m", "base"]);
    std::fs::write(ws.join("a.txt"), "two\n").unwrap();
    git(ws, &["add", "a.txt"]);

    let summary = checkpoint_session(agent_dir, ws, &sid).unwrap();
    let pinned = git(ws, &["rev-parse", "HEAD"]);
    assert!(
        summary.contains(&format!("git {}", &pinned[..7])),
        "{summary}"
    );
    assert!(git(ws, &["log", "-1", "--format=%s"]).starts_with("titi checkpoint:"));

    std::fs::write(ws.join("a.txt"), "three\n").unwrap();
    git(ws, &["commit", "-qam", "later"]);
    rewind_session(agent_dir, ws, &sid, None).unwrap();
    assert_eq!(git(ws, &["rev-parse", "HEAD"]), pinned);
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "two\n");
}
