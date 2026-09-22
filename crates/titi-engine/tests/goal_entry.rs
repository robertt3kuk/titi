//! `/goal` calls [`titi_engine::run_goal`]. These are the two outcomes the
//! slash must be able to report. The rest of the loop is covered in `goal.rs`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use smol_str::SmolStr;
use titi_engine::{
    CodeRequest, Coder, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, GoalStop, Patch,
    Review, ReviewRequest, Reviewer, Verdict, goal_report, run_goal,
};
use titi_providers::{BlockId, MockBody, MockTransport, StopReason, StreamEvent};

struct FixedCoder {
    patches: Vec<&'static str>,
    next: AtomicUsize,
}

#[async_trait]
impl Coder for FixedCoder {
    async fn code(&self, _request: CodeRequest) -> Result<Patch, SmolStr> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        Ok(Patch::new(self.patches[index]))
    }
}

struct FixedReviewer {
    verdict: Verdict,
}

#[async_trait]
impl Reviewer for FixedReviewer {
    async fn review(&self, _request: ReviewRequest) -> Result<Review, SmolStr> {
        Ok(Review {
            verdict: self.verdict,
            notes: "noted".into(),
        })
    }
}

#[tokio::test]
async fn the_slash_entry_passes() {
    let outcome = run_goal(
        Arc::new(FixedCoder {
            patches: vec!["diff a\n"],
            next: AtomicUsize::new(0),
        }),
        Arc::new(FixedReviewer {
            verdict: Verdict::Pass,
        }),
        "fix the parser",
    )
    .await;
    assert_eq!(outcome.stop, GoalStop::Passed);
    assert_eq!(outcome.verdict, Some(Verdict::Pass));
    assert_eq!(outcome.rounds, 1);
    let report = goal_report(&outcome);
    assert!(report.contains("passed"), "{report}");
    assert!(report.contains("verdict pass"), "{report}");
}

#[tokio::test]
async fn the_slash_entry_oscillates() {
    let outcome = run_goal(
        Arc::new(FixedCoder {
            patches: vec!["same\n", "same\n"],
            next: AtomicUsize::new(0),
        }),
        Arc::new(FixedReviewer {
            verdict: Verdict::Fail,
        }),
        "fix the parser",
    )
    .await;
    assert_eq!(outcome.stop, GoalStop::Oscillation);
    assert_eq!(outcome.verdict, Some(Verdict::Fail));
    let report = goal_report(&outcome);
    assert!(report.contains("oscillation"), "{report}");
}

/// The product command uses the same `run_goal` entry, through the session
/// runner, and does not open a chat turn.
#[tokio::test]
async fn run_goal_command_reports_a_pass_without_a_turn() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("c"),
                text: "diff a\n".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("r"),
                text: "PASS\nok\n".into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ]),
    ]));
    let captured = Arc::clone(&transport);
    let resolver = Arc::new(move |model: &str| {
        Ok(titi_engine::ResolvedModel::without_credential(
            model,
            Arc::clone(&captured) as Arc<dyn titi_providers::Transport>,
        ))
    });
    let mut engine = EngineRuntime::start(EngineConfig::new("primary"), resolver);
    engine
        .send(EngineCommand::RunGoal {
            text: "fix the parser".into(),
        })
        .await
        .unwrap();
    let event = engine.recv().await.expect("a goal report");
    match event {
        EngineEvent::GoalFinished { report } => {
            assert!(report.contains("passed"), "{report}");
        }
        other => panic!("expected a goal report, got {other:?}"),
    }
    assert_eq!(transport.call_count(), 2);
}
