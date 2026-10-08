//! `--continue` and `session.autoResume`: which session a launch runs on.
//!
//! Both ask for the same thing, so both go through `launch_session`; these
//! tests pin that the answer is the newest stored session *of the workspace
//! the run started in* — and the newest stored session anywhere when that
//! workspace has none, which is what keeps a session from before workspaces
//! were recorded resumable. They also pin that nothing being stored starts a
//! fresh one rather than failing, and that without either switch a launch
//! ignores the stored sessions entirely.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::Path;

use titi_cli::engine::{launch_session, session_auto_resume_from};
use titi_cli::session_fs::{list_sessions_from, newest_session, newest_session_in};
use titi_config::settings::{SESSION_AUTO_RESUME_KEY, Settings};
use titi_core::session::{Role, SessionIndex, SessionMeta, SessionStore};

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A session created in `workspace`, the way a run in that directory creates
/// one.
fn session_in(store: &SessionStore, workspace: &str) -> String {
    store
        .create(SessionMeta {
            cwd: Some(workspace.to_owned()),
            ..SessionMeta::default()
        })
        .unwrap()
}

/// Two sessions, the second written last. The listing is by modification
/// time, so the two are separated on purpose: created in the same nanosecond
/// they would not order. Both are created the way a run creates one, so both
/// land in the workspace the test runs in, and it is recency — not the
/// workspace — that has to order them.
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

/// Two projects, one agent directory: `--continue` resumes the session of the
/// workspace the run started in, even when a session from another project was
/// written more recently.
#[test]
fn a_launch_prefers_the_session_of_this_workspace_over_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let here = session_in(&store, "/work/here");
    store.append(&here, Role::User, "this project").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let there = session_in(&store, "/work/there");
    store.append(&there, Role::User, "another project").unwrap();

    // The scoped listing is exactly one project's sessions...
    assert_eq!(
        store.sessions_in(Some("/work/here")).unwrap(),
        vec![here.clone()]
    );
    assert_eq!(
        store.sessions_in(Some("/work/there")).unwrap(),
        vec![there.clone()]
    );
    // ...and this workspace's session wins over the newer one from the other,
    // whichever order their files were last touched in.
    assert_eq!(
        newest_session_in(agent_dir, Path::new("/work/here")),
        Some(here.clone())
    );

    // A workspace with nothing recorded is not left without an answer: the
    // newest session overall is what gets resumed.
    assert_eq!(
        newest_session_in(agent_dir, Path::new("/work/elsewhere")),
        Some(there)
    );
}

/// A session written before sessions carried a workspace is still resumed: it
/// is absent from the workspace-scoped list (there is nothing to match) but
/// present in the flat one, and the fallback answers with it.
#[test]
fn a_session_with_no_recorded_workspace_is_still_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    // The store records a workspace on every session it creates, so the row
    // is written the way the release before the field wrote it: straight into
    // the index, with no workspace.
    let legacy = "00000000000000000000000000000001";
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{legacy}.jsonl")), "").unwrap();
    {
        let index = SessionIndex::open(&agent_dir.join("state.db")).unwrap();
        index
            .insert_session(legacy, 1, &SessionMeta::default())
            .unwrap();
    }

    let store = SessionStore::new(agent_dir).unwrap();
    assert_eq!(
        store.sessions_in(Some("/work/here")).unwrap(),
        Vec::<String>::new(),
        "nothing is recorded for the workspace asked for"
    );
    assert!(
        store
            .sessions_in(None)
            .unwrap()
            .contains(&legacy.to_owned()),
        "an old session is not hidden from the flat listing"
    );
    assert!(
        list_sessions_from(agent_dir).contains(&legacy.to_owned()),
        "nor from the listing the switcher reads"
    );

    assert_eq!(
        newest_session_in(agent_dir, Path::new("/work/here")),
        Some(legacy.to_owned())
    );
    let (id, _) = launch_session(agent_dir, true);
    assert_eq!(id, legacy, "and it is what a launch resumes");
}

/// Two sessions of the same project still order by recency: scoping must not
/// turn the pick into "any session of this workspace".
#[test]
fn two_sessions_in_one_workspace_still_order_by_recency() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let older = session_in(&store, "/work/here");
    store.append(&older, Role::User, "older").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let newer = session_in(&store, "/work/here");
    store.append(&newer, Role::User, "newer").unwrap();

    assert_eq!(
        newest_session_in(agent_dir, Path::new("/work/here")),
        Some(newer)
    );
}

/// The live path, and why the store records a workspace at all: a CLI-created
/// session names none (`engine::launch_session` builds its metadata from
/// `Default`), and it still has to be the one this workspace resumes.
#[test]
fn continue_resumes_the_session_recorded_in_the_workspace_it_runs_in() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let mine = store.create(SessionMeta::default()).unwrap();
    store.append(&mine, Role::User, "this project").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let elsewhere = store
        .create(SessionMeta {
            cwd: Some("/work/elsewhere".into()),
            ..SessionMeta::default()
        })
        .unwrap();
    store
        .append(&elsewhere, Role::User, "another project")
        .unwrap();

    let workspace = std::env::current_dir().unwrap();
    assert_eq!(newest_session_in(agent_dir, &workspace), Some(mine.clone()));

    let (id, history) = launch_session(agent_dir, true);
    assert_eq!(id, mine, "not the newer session {elsewhere}");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].content.as_str(), "this project");
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

/// Asking which sessions belong to a project is a read: it answers from the
/// files when there is no index, and it does not create one to answer.
#[test]
fn a_lookup_does_not_create_an_index() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();

    assert_eq!(newest_session_in(agent_dir, Path::new("/work/here")), None);
    assert!(
        !agent_dir.join("state.db").exists(),
        "a lookup must not write an index"
    );
    assert!(
        !agent_dir.join("sessions").exists(),
        "nor the directory the store would create for it"
    );

    // With an index, the answer comes from it.
    let store = SessionStore::new(agent_dir).unwrap();
    let here = session_in(&store, "/work/here");
    assert_eq!(
        newest_session_in(agent_dir, Path::new("/work/here")),
        Some(here)
    );
}
