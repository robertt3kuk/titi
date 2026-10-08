//! Integration tests for the stream queue (Steer/FollowUp) in titi-cli:
//! Alt+Up pulls the last queued message back into the editor, Esc clears
//! the highlight without deleting, and the queue is LIFO.
//!
//! Contract: `docs/research/agent-ux/README.md` (Queue DoD).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use titi_cli::app::App;
use titi_cli::themes::default_theme;
use titi_tui::composer::QueueMode;

fn app() -> App {
    App::new(
        Arc::new(AtomicBool::new(false)),
        vec!["titi".to_owned()],
        default_theme().unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Queue push, pull, LIFO order, highlight, Esc clear
// ---------------------------------------------------------------------------

#[test]
fn push_and_pull_last_returns_text_and_highlights() {
    let mut app = app();
    app.push_queued("hello world", QueueMode::Steer);
    assert_eq!(app.stream_queue_len(), 1);
    assert!(!app.queue_highlighted());

    let text = app.pull_last_queued().expect("should have queued text");
    assert_eq!(text, "hello world");
    assert!(app.queue_highlighted());
    assert_eq!(app.stream_queue_len(), 0);
}

#[test]
fn pull_empty_returns_none() {
    let mut app = app();
    assert!(app.pull_last_queued().is_none());
    assert!(!app.queue_highlighted());
}

#[test]
fn queue_is_lifo() {
    let mut app = app();
    app.push_queued("first", QueueMode::Steer);
    app.push_queued("second", QueueMode::FollowUp);

    // LIFO: last in, first out
    let text = app.pull_last_queued().expect("second");
    assert_eq!(text, "second");
    assert!(app.queue_highlighted());
    assert_eq!(app.stream_queue_len(), 1);

    // Clear highlight and pull again
    app.clear_highlight();
    assert!(!app.queue_highlighted());

    let text = app.pull_last_queued().expect("first");
    assert_eq!(text, "first");
    assert!(app.queue_highlighted());
    assert_eq!(app.stream_queue_len(), 0);
}

#[test]
fn clear_highlight_removes_flag_but_leaves_queue() {
    let mut app = app();
    app.push_queued("keep me", QueueMode::FollowUp);
    let _text = app.pull_last_queued().expect("pulled");
    assert!(app.queue_highlighted());

    // Esc clears highlight without re-queueing
    app.clear_highlight();
    assert!(!app.queue_highlighted());

    // The queue is still empty (clear_highlight does not re-queue)
    assert_eq!(app.stream_queue_len(), 0);
}

#[test]
fn stream_queue_len_reflects_count() {
    let mut app = app();
    assert_eq!(app.stream_queue_len(), 0);
    app.push_queued("a", QueueMode::Steer);
    assert_eq!(app.stream_queue_len(), 1);
    app.push_queued("b", QueueMode::Steer);
    assert_eq!(app.stream_queue_len(), 2);
    app.push_queued("c", QueueMode::FollowUp);
    assert_eq!(app.stream_queue_len(), 3);
    app.pull_last_queued();
    assert_eq!(app.stream_queue_len(), 2);
    app.pull_last_queued();
    assert_eq!(app.stream_queue_len(), 1);
    app.pull_last_queued();
    assert_eq!(app.stream_queue_len(), 0);
}
