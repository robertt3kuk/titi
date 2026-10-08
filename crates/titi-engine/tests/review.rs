#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! E4 integration: the fresh-context reviewer.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E4).

use std::sync::Arc;

use async_trait::async_trait;
use smol_str::SmolStr;
use titi_engine::{
    AgentContext, AgentRequest, AgentReviewer, AgentRunner, ReviewRequest, Reviewer,
};

/// A runner that echoes the prompt it was handed, so the test can prove what
/// context the reviewer actually saw.
struct EchoRunner;

#[async_trait]
impl AgentRunner for EchoRunner {
    async fn run(&self, request: AgentRequest, _context: AgentContext) -> Result<SmolStr, SmolStr> {
        Ok(request.task.clone())
    }
}

/// A runner that answers with a fixed verdict, ignoring the prompt.
struct FixedRunner(&'static str);

#[async_trait]
impl AgentRunner for FixedRunner {
    async fn run(
        &self,
        _request: AgentRequest,
        _context: AgentContext,
    ) -> Result<SmolStr, SmolStr> {
        Ok(self.0.into())
    }
}

#[tokio::test]
async fn the_reviewer_sees_only_the_goal_and_the_evidence() {
    let reviewer = AgentReviewer::new(Arc::new(EchoRunner), "reviewer");
    let request = ReviewRequest::new("make the build green", "cargo test: 3 failed in titi-core");
    let review = reviewer.review(request).await.unwrap();

    assert!(review.notes.contains("make the build green"));
    assert!(review.notes.contains("cargo test: 3 failed"));
    // An echo names no verdict on line one, so it is not approval.
    assert_eq!(review.verdict, titi_engine::Verdict::Partial);
}

#[tokio::test]
async fn the_verdict_and_exit_code_follow_the_reply() {
    for (reply, verdict, code) in [
        ("PASS\nthe goal is met", titi_engine::Verdict::Pass, 0),
        ("FAIL — the edit broke auth", titi_engine::Verdict::Fail, 3),
        (
            "PARTIAL: no test run to judge",
            titi_engine::Verdict::Partial,
            1,
        ),
    ] {
        let reviewer = AgentReviewer::new(Arc::new(FixedRunner(reply)), "reviewer");
        let review = reviewer
            .review(ReviewRequest::new("goal", "evidence"))
            .await
            .unwrap();
        assert_eq!(review.verdict, verdict, "reply: {reply}");
        assert_eq!(review.verdict.exit_code(), code);
        assert_eq!(review.notes, reply);
    }
}

#[tokio::test]
async fn a_reviewer_that_fails_reports_the_error() {
    struct FailingRunner;

    #[async_trait]
    impl AgentRunner for FailingRunner {
        async fn run(
            &self,
            _request: AgentRequest,
            _context: AgentContext,
        ) -> Result<SmolStr, SmolStr> {
            Err("model unavailable".into())
        }
    }

    let reviewer = AgentReviewer::new(Arc::new(FailingRunner), "reviewer");
    let error = reviewer
        .review(ReviewRequest::new("goal", "evidence"))
        .await
        .unwrap_err();
    assert_eq!(error, "model unavailable");
}
