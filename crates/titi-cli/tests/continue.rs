//! `--continue` and `session.autoResume`: which session a launch runs on.
//!
//! Both ask for the same thing, so both go through `launch_session`; these
//! tests pin that the answer is the newest stored session, that nothing being
//! stored starts a fresh one rather than failing, and that without either
//! switch a launch ignores the stored sessions entirely.

#![allow(clippy::unwrap_used)]

use titi_cli::engine::{launch_session, session_auto_resume_from};
use titi_cli::session_fs::newest_session;
use titi_config::settings::{SESSION_AUTO_RESUME_KEY, Settings};
use titi_core::session::{Role, SessionMeta, SessionStore};

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Two sessions, the second written last. The listing is by modification
/// time, so the two are separated on purpose: created in the same nanosecond
/// they would not order.
fn two_sessions(agent_dir: &std::path::Path) -> (String, String) {
    let store = SessionStore::new(agent_dir).unwrap();
    let older = store.create(SessionMeta::default()).unwrap();
    store.append(&older, Role::User, "old work").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let newer = store.create(SessionMeta::default()).unwrap();
    store.append(&newer, Role::User, "last word").unwrap();
    (older, newer)
}

#[test]
fn continue_resumes_the_newest_session() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let (older, newer) = two_sessions(agent_dir);

    assert_eq!(newest_session(agent_dir), Some(newer.clone()));

    let (id, history) = launch_session(agent_dir, true);
    assert_eq!(id, newer, "the newest, not {older}");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content.as_str(), "last word");
}

/// Without the flag and without the key, a launch starts blank even though
/// the agent directory is full of sessions: continuing is opt-in.
#[test]
fn without_continue_a_launch_starts_blank() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let (older, newer) = two_sessions(agent_dir);

    let (id, history) = launch_session(agent_dir, false);
    assert_ne!(id, newer);
    assert_ne!(id, older);
    assert!(history.is_empty(), "nothing was replayed");

    // A real, empty session: the run has one to write its turns into, and
    // the stored ones are untouched.
    let store = SessionStore::new(agent_dir).unwrap();
    assert!(store.open(&id).unwrap().is_empty());
    assert_eq!(store.open(&newer).unwrap().len(), 1);
}

/// Nothing stored is a fresh start, not a failure — and there is nothing to
/// name, which is what makes the run say so (`main.rs`).
#[test]
fn continue_with_nothing_to_resume_starts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();

    assert_eq!(newest_session(agent_dir), None);

    let (id, history) = launch_session(agent_dir, true);
    assert!(history.is_empty(), "nothing was resumed");
    let store = SessionStore::new(agent_dir).unwrap();
    assert!(
        store.open(&id).unwrap().is_empty(),
        "a usable fresh session"
    );
}

/// The key is off unless it says otherwise; a value that cannot be read is
/// off too, so a typo never turns a blank launch into a resumed one.
#[test]
fn auto_resume_is_off_unless_the_key_says_on() {
    let cases: [(&str, bool); 7] = [
        ("", false),
        ("session:\n  autoResume: true\n", true),
        ("session:\n  autoResume: false\n", false),
        ("session:\n  autoResume: \"on\"\n", true),
        ("session:\n  autoResume: \"no\"\n", false),
        ("session:\n  autoResume: 7\n", false),
        ("session:\n  autoResume: \"maybe\"\n", false),
    ];
    for (config, expected) in cases {
        let agent = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        if !config.is_empty() {
            write(&agent.path().join("config.yml"), config);
        }
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert_eq!(
            session_auto_resume_from(&settings),
            expected,
            "for {config:?}"
        );
    }
}

/// The key is read under its documented name, and the project layer may set
/// it: a repository's own `.titi/config.yml` is the user's consent for that
/// repository.
#[test]
fn the_key_is_session_auto_resume_and_the_project_may_set_it() {
    let agent = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join(".titi/config.yml"),
        "session:\n  autoResume: true\n",
    );
    let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
    assert_eq!(
        settings
            .get(SESSION_AUTO_RESUME_KEY)
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(session_auto_resume_from(&settings));
}
