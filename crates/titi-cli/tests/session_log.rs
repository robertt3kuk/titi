//! The transcript must actually reach the session store, or a resume replays
//! nothing and a checkpoint marks an empty tree.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::{App, default_theme};
use titi_cli::engine::MAX_RESTORED_MESSAGES;
use titi_cli::session_log::SessionLog;
use titi_core::session::{Role, SessionMeta, SessionStore};
use titi_engine::{AgentKind, AgentStatus, EngineEvent, TurnId};
use titi_providers::StopReason;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

#[test]
fn a_turn_queues_the_user_message_then_the_reply() {
    let mut app = app();
    let mut input = "what is 2+2?".to_owned();
    app.handle_canonical("enter", &mut input);

    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    app.ingest_engine_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "four".into(),
    });
    app.ingest_engine_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });

    let writes = app.drain_session_writes();
    assert_eq!(writes.len(), 2, "user then assistant: {writes:?}");
    assert_eq!(writes[0], (Role::User, "what is 2+2?".to_owned()));
    assert_eq!(writes[1], (Role::Assistant, "four".to_owned()));
    // Draining empties the queue.
    assert!(app.drain_session_writes().is_empty());
}

#[test]
fn a_turn_that_streams_nothing_queues_only_the_prompt() {
    let mut app = app();
    let mut input = "hi".to_owned();
    app.handle_canonical("enter", &mut input);
    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    app.ingest_engine_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });

    let writes = app.drain_session_writes();
    assert_eq!(writes.len(), 1, "no empty reply is recorded: {writes:?}");
    assert_eq!(writes[0].0, Role::User);
}

#[test]
fn a_steered_message_is_still_part_of_the_transcript() {
    let mut app = app();
    app.ingest_engine_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "test/model".into(),
    });
    let mut input = "also check the tests".to_owned();
    app.handle_canonical("enter", &mut input);

    let writes = app.drain_session_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0], (Role::User, "also check the tests".to_owned()));
}

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
fn agent_events_do_not_enter_the_transcript() {
    let mut app = app();
    app.ingest_engine_event(EngineEvent::AgentStarted {
        agent_id: "agent-1".into(),
        name: "Worker".into(),
        parent_id: None,
        kind: AgentKind::Subagent,
    });
    app.ingest_engine_event(EngineEvent::AgentStatusChanged {
        agent_id: "agent-1".into(),
        status: AgentStatus::Completed,
    });
    app.ingest_engine_event(EngineEvent::AgentFinished {
        agent_id: "agent-1".into(),
        summary: "did a thing".into(),
        success: true,
    });
    assert!(
        app.drain_session_writes().is_empty(),
        "subagent chatter is not the conversation"
    );
}

#[test]
fn the_restored_history_is_capped() {
    assert_eq!(MAX_RESTORED_MESSAGES, 40);
}

#[test]
fn a_new_session_starts_empty_and_becomes_the_latest() {
    use titi_cli::app::new_session;

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
    use titi_cli::app::session_history;

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
    use titi_cli::app::session_history;

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
    use titi_cli::app::session_history;

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
