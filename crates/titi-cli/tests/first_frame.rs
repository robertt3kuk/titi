//! Integration test for the first-frame contract
//! (`docs/research/agent-ux/README.md`, DoD first item):
//!
//! * `titi` paints a banner + status line before the provider is ready;
//! * `time-to-first-frame` < 150 ms;
//! * input submitted during startup is queued and delivered after ready;
//! * integration test drives a mock provider with a 2 s init delay.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use titi_cli::first_frame::{FirstFrame, SubmitOutcome};
use titi_tui::status::AgentState;

/// A provider whose initialization takes `delay` to flip `ready`.
struct SlowProvider {
    ready: Arc<AtomicBool>,
}

impl SlowProvider {
    fn new(delay: Duration) -> Self {
        let ready = Arc::new(AtomicBool::new(false));
        {
            let ready = Arc::clone(&ready);
            thread::spawn(move || {
                thread::sleep(delay);
                ready.store(true, Ordering::SeqCst);
            });
        }
        SlowProvider { ready }
    }
}

#[test]
fn first_frame_paints_banner_and_starting_status_before_ready() {
    let provider = SlowProvider::new(Duration::from_secs(2));
    let mut app = FirstFrame::new(vec![
        "titi v0.1.0 — omp port in Rust".to_owned(),
        String::new(),
    ]);

    let (rows, ttff) = app.first_frame(80);

    // Banner is the first rows; status line is the last, still "starting".
    assert!(rows[0].contains("titi v0.1.0"));
    assert_eq!(app.state(), AgentState::Starting);
    assert!(app.status_line(80).contains("starting"));
    assert!(!provider.ready.load(Ordering::SeqCst));

    // A prompt submitted now is queued, not delivered.
    assert_eq!(app.submit("first prompt".to_owned()), SubmitOutcome::Queued);
    assert_eq!(app.queue_len(), 1);
    assert_eq!(app.delivered(), 0);

    // Time-to-first-frame is well under the 150 ms budget.
    //
    // The assertion is the *best* of a few paints, because one wall-clock
    // sample on a shared runner measures the runner too: a descheduled thread
    // makes any single sample slow without the code being slow. A frame that
    // waited for something — the provider, a config read, a git call — is slow
    // in every sample, which is what this catches; a busy machine only makes
    // one of them slow. Both numbers ride in the message so a real regression
    // is readable rather than a mystery.
    let mut best = ttff;
    for _ in 0..4 {
        best = best.min(app.first_frame(80).1);
    }
    assert!(
        best < Duration::from_millis(150),
        "best ttff = {best:?}, first = {ttff:?}"
    );
}

#[test]
fn prompt_queued_during_init_is_delivered_after_ready() {
    let provider = SlowProvider::new(Duration::from_secs(2));
    let mut app = FirstFrame::new(vec![String::new()]);

    let t0 = Instant::now();
    let (_rows, _ttff) = app.first_frame(80);

    // Queue two prompts while the provider is still starting.
    assert_eq!(app.submit("ping".to_owned()), SubmitOutcome::Queued);
    assert_eq!(app.submit("pong".to_owned()), SubmitOutcome::Queued);
    assert_eq!(app.queue_len(), 2);

    // Wait for the provider to become ready (mock init ~2 s).
    while !provider.ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    let init_elapsed = t0.elapsed();
    assert!(
        init_elapsed >= Duration::from_millis(1500),
        "init too fast: {init_elapsed:?}"
    );

    // Flushing delivers the queued prompts in order and flips to Ready.
    let flushed = app.provider_ready();
    assert_eq!(flushed, vec!["ping".to_owned(), "pong".to_owned()]);
    assert_eq!(app.queue_len(), 0);
    assert_eq!(app.delivered(), 2);
    assert_eq!(app.state(), AgentState::Ready);
    assert!(app.is_ready());

    // Post-ready submits are delivered immediately, not queued.
    assert_eq!(app.submit("after".to_owned()), SubmitOutcome::Delivered);
    assert_eq!(app.delivered(), 3);
    assert_eq!(app.queue_len(), 0);
}
