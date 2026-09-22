//! Goal loop: coder then fresh reviewer, bounded, with patch oscillation.
//!
//! Spec: `docs/research/reference-product-port/README.md` (autonomous loop).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use async_trait::async_trait;
use smol_str::SmolStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use titi_engine::{
    CodeRequest, Coder, DEFAULT_GOAL_ROUNDS, GoalCancel, GoalLoop, GoalStop, Patch, Review,
    ReviewRequest, Reviewer, Verdict,
};
use tokio::sync::Notify;

struct ScriptedCoder {
    patches: Vec<&'static str>,
    calls: Sender<CodeRequest>,
    next: AtomicUsize,
}

#[async_trait]
impl Coder for ScriptedCoder {
    async fn code(&self, request: CodeRequest) -> Result<Patch, SmolStr> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        let _ = self.calls.send(request);
        let text = self
            .patches
            .get(index)
            .copied()
            .unwrap_or("no further patch");
        Ok(Patch::new(text))
    }
}

struct ScriptedReviewer {
    verdicts: Vec<Verdict>,
    notes: &'static str,
    calls: Sender<ReviewRequest>,
    next: AtomicUsize,
}

#[async_trait]
impl Reviewer for ScriptedReviewer {
    async fn review(&self, request: ReviewRequest) -> Result<Review, SmolStr> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        let _ = self.calls.send(request);
        let verdict = self.verdicts.get(index).copied().unwrap_or(Verdict::Fail);
        Ok(Review {
            verdict,
            notes: format!("{}\n{}", verdict.as_str(), self.notes).into(),
        })
    }
}

fn coder(patches: Vec<&'static str>) -> (Arc<ScriptedCoder>, Receiver<CodeRequest>) {
    let (tx, rx) = mpsc::channel();
    let coder = Arc::new(ScriptedCoder {
        patches,
        calls: tx,
        next: AtomicUsize::new(0),
    });
    (coder, rx)
}

fn reviewer(
    verdicts: Vec<Verdict>,
    notes: &'static str,
) -> (Arc<ScriptedReviewer>, Receiver<ReviewRequest>) {
    let (tx, rx) = mpsc::channel();
    let reviewer = Arc::new(ScriptedReviewer {
        verdicts,
        notes,
        calls: tx,
        next: AtomicUsize::new(0),
    });
    (reviewer, rx)
}

fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
    rx.try_iter().collect()
}

#[tokio::test]
async fn a_pass_ends_the_loop_without_spending_the_remaining_rounds() {
    let (coder, coder_calls) = coder(vec!["+fn done() {}\n", "+fn ignored() {}\n"]);
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Pass], "the tests cover it");
    let outcome = GoalLoop::new(coder, reviewer)
        .with_max_rounds(DEFAULT_GOAL_ROUNDS)
        .run("make the build green")
        .await;

    assert_eq!(outcome.stop, GoalStop::Passed);
    assert_eq!(outcome.verdict, Some(Verdict::Pass));
    assert_eq!(outcome.verdict.unwrap().exit_code(), 0);
    assert_eq!(outcome.rounds, 1);
    assert_eq!(drain(&coder_calls).len(), 1);
    let reviews = drain(&reviewer_calls);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].evidence.as_str(), "+fn done() {}\n");
}

#[tokio::test]
async fn the_round_cap_fails_after_every_round_is_a_new_patch() {
    let (coder, coder_calls) = coder(vec!["+fn a() {}\n", "+fn b() {}\n", "+fn c() {}\n"]);
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Fail; 3], "still red");
    let outcome = GoalLoop::new(coder, reviewer)
        .with_max_rounds(3)
        .run("make the build green")
        .await;

    assert_eq!(outcome.stop, GoalStop::RoundCap);
    assert_eq!(outcome.verdict, Some(Verdict::Fail));
    assert_eq!(outcome.verdict.unwrap().exit_code(), 3);
    assert_eq!(outcome.rounds, 3);
    assert_eq!(drain(&coder_calls).len(), 3);
    assert_eq!(drain(&reviewer_calls).len(), 3);
}

#[tokio::test]
async fn the_default_cap_is_eight_rounds() {
    let distinct = [
        "+a\n", "+b\n", "+c\n", "+d\n", "+e\n", "+f\n", "+g\n", "+h\n",
    ];
    let (coder, coder_calls) = coder(distinct.to_vec());
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Fail; 8], "still red");
    let loop_ = GoalLoop::new(coder, reviewer);
    assert_eq!(loop_.max_rounds(), DEFAULT_GOAL_ROUNDS);

    let outcome = loop_.run("goal").await;

    assert_eq!(outcome.stop, GoalStop::RoundCap);
    assert_eq!(drain(&coder_calls).len(), 8);
    assert_eq!(drain(&reviewer_calls).len(), 8);
}

#[tokio::test]
async fn an_identical_patch_is_oscillation_and_does_not_burn_the_cap() {
    let (coder, coder_calls) = coder(vec![
        "diff --git a/a.rs b/a.rs\n+fn a() {}\n",
        "diff --git a/a.rs b/a.rs\n+fn a() {}\r\n",
        "+fn should-not-run() {}\n",
    ]);
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Fail; 8], "the tests are red");
    let outcome = GoalLoop::new(coder, reviewer)
        .with_max_rounds(8)
        .run("fix a")
        .await;

    assert_eq!(outcome.stop, GoalStop::Oscillation);
    assert_eq!(outcome.verdict, Some(Verdict::Fail));
    assert_eq!(outcome.verdict.unwrap().exit_code(), 3);
    assert_eq!(drain(&coder_calls).len(), 2);
    // The repeated patch is not reviewed again, and rounds 3..=8 do not run.
    assert_eq!(drain(&reviewer_calls).len(), 1);
    assert_eq!(outcome.rounds, 2);
}

#[tokio::test]
async fn a_changed_patch_is_not_oscillation_even_when_the_review_repeats() {
    let (coder, coder_calls) = coder(vec!["+fn a() {}\n", "+fn b() {}\n"]);
    let (reviewer, reviewer_calls) =
        reviewer(vec![Verdict::Fail, Verdict::Fail], "the tests are red");
    let outcome = GoalLoop::new(coder, reviewer)
        .with_max_rounds(2)
        .run("fix a")
        .await;

    assert_eq!(outcome.stop, GoalStop::RoundCap);
    let coder_log = drain(&coder_calls);
    let reviews = drain(&reviewer_calls);
    assert_eq!(coder_log.len(), 2);
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[0].evidence.as_str(), "+fn a() {}\n");
    assert_eq!(reviews[1].evidence.as_str(), "+fn b() {}\n");
    // Same review sentence both times; the loop did not treat that as stuck.
    assert!(reviews.iter().all(|review| review.goal == "fix a"));
}

#[tokio::test]
async fn a_patch_that_matches_an_earlier_round_still_oscillates() {
    let (coder, coder_calls) = coder(vec!["+fn a() {}\n", "+fn b() {}\n", "+fn a() {}\n"]);
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Fail, Verdict::Fail], "no");
    let outcome = GoalLoop::new(coder, reviewer)
        .with_max_rounds(8)
        .run("fix a")
        .await;

    assert_eq!(outcome.stop, GoalStop::Oscillation);
    assert_eq!(drain(&coder_calls).len(), 3);
    assert_eq!(drain(&reviewer_calls).len(), 2);
}

#[tokio::test]
async fn partial_returns_to_the_coder_and_pass_stops() {
    let (coder, coder_calls) = coder(vec!["+fn a() {}\n", "+fn a() { tested }\n"]);
    let (reviewer, reviewer_calls) = reviewer(
        vec![Verdict::Partial, Verdict::Pass],
        "no test run to judge",
    );
    let outcome = GoalLoop::new(coder, reviewer)
        .with_max_rounds(8)
        .run("cover the change")
        .await;

    assert_eq!(outcome.stop, GoalStop::Passed);
    assert_eq!(outcome.rounds, 2);
    let coder_log = drain(&coder_calls);
    let reviews = drain(&reviewer_calls);
    assert_eq!(coder_log.len(), 2);
    assert_eq!(reviews.len(), 2);
    let feedback = coder_log[1].feedback.clone().unwrap();
    assert_eq!(feedback.verdict, Verdict::Partial);
    assert!(feedback.notes.contains("no test run to judge"));
    // Round 2's reviewer sees the new patch, not the previous review prose.
    assert_eq!(reviews[1].evidence.as_str(), "+fn a() { tested }\n");
    assert!(!reviews[1].evidence.contains("no test run to judge"));
}

#[tokio::test]
async fn cancel_before_the_loop_runs_nothing() {
    let (coder, coder_calls) = coder(vec!["+fn a() {}\n"]);
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Pass], "ok");
    let cancel = GoalCancel::new();
    cancel.cancel();
    let outcome = GoalLoop::new(coder, reviewer)
        .with_cancel(cancel)
        .run("goal")
        .await;

    assert_eq!(outcome.stop, GoalStop::Cancelled);
    assert_eq!(outcome.verdict, None);
    assert_eq!(outcome.rounds, 0);
    assert!(drain(&coder_calls).is_empty());
    assert!(drain(&reviewer_calls).is_empty());
}

struct WaitingCoder {
    started: Arc<Notify>,
    release: Arc<Notify>,
    calls: Sender<CodeRequest>,
}

#[async_trait]
impl Coder for WaitingCoder {
    async fn code(&self, request: CodeRequest) -> Result<Patch, SmolStr> {
        let _ = self.calls.send(request);
        self.started.notify_one();
        self.release.notified().await;
        Ok(Patch::new("+fn late() {}\n"))
    }
}

#[tokio::test]
async fn cancel_aborts_an_in_flight_coder_and_skips_the_reviewer() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (tx, coder_calls) = mpsc::channel();
    let coder = Arc::new(WaitingCoder {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
        calls: tx,
    });
    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Pass], "ok");
    let cancel = GoalCancel::new();
    let loop_ = GoalLoop::new(coder, reviewer).with_cancel(cancel.clone());
    let run = tokio::spawn(async move { loop_.run("goal").await });

    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("coder was not started");
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("loop did not stop after cancel")
        .unwrap();
    release.notify_one();

    assert_eq!(outcome.stop, GoalStop::Cancelled);
    assert_eq!(outcome.verdict, None);
    assert_eq!(outcome.rounds, 0);
    assert_eq!(drain(&coder_calls).len(), 1);
    assert!(drain(&reviewer_calls).is_empty());
}

struct CancellingReviewer {
    flag: GoalCancel,
    calls: Sender<ReviewRequest>,
}

#[async_trait]
impl Reviewer for CancellingReviewer {
    async fn review(&self, request: ReviewRequest) -> Result<Review, SmolStr> {
        let _ = self.calls.send(request);
        self.flag.cancel();
        Ok(Review {
            verdict: Verdict::Fail,
            notes: "FAIL\nnot yet".into(),
        })
    }
}

#[tokio::test]
async fn cancel_between_rounds_does_not_start_another_coder_turn() {
    let (coder, coder_calls) = coder(vec!["+fn a() {}\n", "+fn b() {}\n"]);
    let cancel = GoalCancel::new();
    let (tx, reviewer_calls) = mpsc::channel();
    let reviewer = Arc::new(CancellingReviewer {
        flag: cancel.clone(),
        calls: tx,
    });
    let outcome = GoalLoop::new(coder, reviewer)
        .with_cancel(cancel)
        .with_max_rounds(8)
        .run("goal")
        .await;

    assert_eq!(outcome.stop, GoalStop::Cancelled);
    assert_eq!(drain(&coder_calls).len(), 1);
    assert_eq!(drain(&reviewer_calls).len(), 1);
    assert_eq!(outcome.rounds, 1);
}

#[tokio::test]
async fn a_coder_error_stops_the_loop_instead_of_panicking() {
    struct BrokenCoder;

    #[async_trait]
    impl Coder for BrokenCoder {
        async fn code(&self, _request: CodeRequest) -> Result<Patch, SmolStr> {
            Err("provider down".into())
        }
    }

    let (reviewer, reviewer_calls) = reviewer(vec![Verdict::Pass], "ok");
    let outcome = GoalLoop::new(Arc::new(BrokenCoder), reviewer)
        .run("goal")
        .await;

    assert_eq!(outcome.stop, GoalStop::Error);
    assert_eq!(outcome.error.as_deref(), Some("provider down"));
    assert_eq!(outcome.verdict, None);
    assert!(drain(&reviewer_calls).is_empty());
}
