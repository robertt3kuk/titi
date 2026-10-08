//! The transcript must actually reach the session store, or a resume replays
//! nothing and a checkpoint marks an empty tree.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use titi_cli::engine::MAX_RESTORED_MESSAGES;
use titi_cli::session_fs::new_session;
use titi_cli::session_fs::session_history;
use titi_cli::session_log::SessionLog;
use titi_core::session::{Role, SessionMeta, SessionStore};

#[test]
fn the_log_writes_a_conversation_the_store_can_replay() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();

    let log = SessionLog::open(agent_dir, &session_id).expect("a log over the store");
    assert_eq!(log.session_id(), session_id);
    log.user("first question").unwrap();
    log.assistant("first answer").unwrap();
    log.system("compaction marker").unwrap();
    // Blank text is not a turn.
    log.assistant("   ").unwrap();

    let entries = store.open(&session_id).unwrap();
    let roles: Vec<Role> = entries.iter().map(|entry| entry.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::System]);
    assert_eq!(entries[0].content, "first question");
    assert_eq!(entries[2].content, "compaction marker");

    // The resume path sees the same conversation.
    let (id, restored) = store.restore_latest().unwrap().expect("a session");
    assert_eq!(id, session_id);
    assert_eq!(restored, entries);
}

#[test]
fn writing_to_a_session_that_does_not_exist_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let log = SessionLog::open(dir.path(), "ghost").expect("the store opens");
    assert!(log.user("nobody is listening").is_err());
}

#[test]
fn a_new_session_starts_empty_and_becomes_the_latest() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let first = store.create(SessionMeta::default()).unwrap();
    store.append(&first, Role::User, "old work").unwrap();

    let second = new_session(agent_dir).unwrap();
    assert_ne!(first, second, "a new session, not the old one");

    // Empty, so switching to it really starts blank...
    assert!(store.open(&second).unwrap().is_empty());
    // ...and it is the one a later resume picks up.
    let (latest, entries) = store.restore_latest().unwrap().unwrap();
    assert_eq!(latest, second);
    assert!(entries.is_empty());
    // The old work is still there, untouched.
    assert_eq!(store.open(&first).unwrap().len(), 1);
}

#[test]
fn session_history_matches_what_the_log_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();
    let log = SessionLog::open(agent_dir, &session_id).unwrap();
    log.user("question").unwrap();
    log.assistant("answer").unwrap();

    let history = session_history(agent_dir, &session_id).unwrap();
    let pairs: Vec<(titi_providers::Role, String)> = history
        .into_iter()
        .map(|message| (message.role, message.content.to_string()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            (titi_providers::Role::User, "question".to_owned()),
            (titi_providers::Role::Assistant, "answer".to_owned()),
        ]
    );
}

#[test]
fn session_history_stops_at_the_tail_cap() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();
    for index in 0..MAX_RESTORED_MESSAGES + 10 {
        store
            .append(&session_id, Role::User, &format!("turn {index}"))
            .unwrap();
    }

    let history = session_history(agent_dir, &session_id).unwrap();
    assert_eq!(history.len(), MAX_RESTORED_MESSAGES);
    assert_eq!(
        history.last().unwrap().content,
        format!("turn {}", MAX_RESTORED_MESSAGES + 9)
    );
}

/// A tool round is the part of a turn a restart used to lose: the call the
/// assistant made and the output it read back.
#[test]
fn a_restored_tool_round_is_the_one_the_live_session_had() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();
    let log = SessionLog::open(agent_dir, &session_id).unwrap();

    let call = titi_providers::ToolCallRef {
        call_id: "call-1".into(),
        name: "read".into(),
    };
    // What the live turn sent: prompt, the call, its output, then the answer.
    let live = vec![
        message(titi_providers::Role::User, "what is in Cargo.toml?", &[]),
        message(
            titi_providers::Role::Assistant,
            "let me look",
            std::slice::from_ref(&call),
        ),
        message(
            titi_providers::Role::Tool,
            "[package]\nname = \"titi\"",
            &[],
        ),
        message(titi_providers::Role::Assistant, "it is the workspace", &[]),
    ];

    log.user("what is in Cargo.toml?").unwrap();
    log.assistant_tool_calls("let me look", vec![call.clone()])
        .unwrap();
    log.tool_result("[package]\nname = \"titi\"").unwrap();
    log.assistant("it is the workspace").unwrap();

    assert_eq!(session_history(agent_dir, &session_id).unwrap(), live);
}

/// Cutting the oldest messages must not split a tool round: a result without
/// its call, or a call without its result, is a request both Anthropic and
/// OpenAI reject.
#[test]
fn a_truncated_restore_never_splits_a_tool_round() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();
    let log = SessionLog::open(agent_dir, &session_id).unwrap();

    // Four messages per round, so the cap lands inside a round whatever the
    // cap is; then a call the crash left unanswered.
    for index in 0..MAX_RESTORED_MESSAGES {
        log.user(&format!("question {index}")).unwrap();
        log.assistant_tool_calls(
            "",
            vec![titi_providers::ToolCallRef {
                call_id: format!("call-{index}").into(),
                name: "read".into(),
            }],
        )
        .unwrap();
        log.tool_result(&format!("output {index}")).unwrap();
        log.assistant(&format!("answer {index}")).unwrap();
    }
    log.user("one more").unwrap();
    log.assistant_tool_calls(
        "",
        vec![titi_providers::ToolCallRef {
            call_id: "call-cut-short".into(),
            name: "bash".into(),
        }],
    )
    .unwrap();

    let history = session_history(agent_dir, &session_id).unwrap();
    assert!(history.len() <= MAX_RESTORED_MESSAGES, "{}", history.len());
    assert!(!history.is_empty(), "a cut that keeps nothing is not a cut");

    let mut open_calls = 0usize;
    for message in &history {
        if message.role == titi_providers::Role::Tool {
            assert!(
                open_calls > 0,
                "a tool result without its call: {history:#?}"
            );
            open_calls -= 1;
        } else {
            assert_eq!(
                open_calls, 0,
                "a tool call without its result: {history:#?}"
            );
            open_calls = message.tool_calls.len();
        }
    }
    assert_eq!(open_calls, 0, "the last call has no result: {history:#?}");
}

/// A call is written the moment it starts, so a cancel or a crash mid-round
/// leaves one unanswered wherever it happened to be — including the middle of
/// a long session. Dropping everything after it would lose the conversation
/// the user actually kept having.
#[test]
fn a_broken_round_in_the_middle_does_not_erase_what_came_after() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    let store = SessionStore::new(agent_dir).unwrap();
    let session_id = store.create(SessionMeta::default()).unwrap();
    let log = SessionLog::open(agent_dir, &session_id).unwrap();

    log.user("first question").unwrap();
    // Cancelled while the tool ran: the call was logged, the result never was.
    log.assistant_tool_calls(
        "let me check",
        vec![titi_providers::ToolCallRef {
            call_id: "call-abandoned".into(),
            name: "bash".into(),
        }],
    )
    .unwrap();
    log.user("never mind, different question").unwrap();
    log.assistant("here is the answer").unwrap();

    let history = session_history(agent_dir, &session_id).unwrap();
    let shape: Vec<(titi_providers::Role, String)> = history
        .iter()
        .map(|message| (message.role, message.content.to_string()))
        .collect();
    assert!(
        shape.contains(&(
            titi_providers::Role::User,
            "never mind, different question".to_owned()
        )),
        "the session after the broken round was erased: {shape:#?}"
    );
    assert!(
        shape.contains(&(
            titi_providers::Role::Assistant,
            "here is the answer".to_owned()
        )),
        "the session after the broken round was erased: {shape:#?}"
    );
    assert!(
        shape.contains(&(titi_providers::Role::Assistant, "let me check".to_owned())),
        "what the assistant said is part of the conversation: {shape:#?}"
    );
    assert!(
        history.iter().all(|message| message.tool_calls.is_empty()),
        "the unanswered call must not reach the provider: {history:#?}"
    );
}

fn message(
    role: titi_providers::Role,
    text: &str,
    tool_calls: &[titi_providers::ToolCallRef],
) -> titi_providers::ChatMessage {
    titi_providers::ChatMessage {
        role,
        content: text.into(),
        tool_calls: tool_calls.to_vec(),
    }
}
