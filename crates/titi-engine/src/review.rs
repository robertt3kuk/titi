//! Fresh-context reviewer.
//!
//! The reviewer reads what changed and returns a verdict; it never sees the
//! conversation that produced the change, so it cannot be talked into agreeing
//! with the coder's own summary.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E4, "fresh-context reviewer").

use async_trait::async_trait;
use smol_str::SmolStr;

use crate::agents::{AgentContext, AgentRequest, AgentRunner};
use crate::protocol::AgentKind;

/// What a reviewer concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Pass,
    Fail,
    /// The reviewer could not confirm or deny — also the fallback for a reply
    /// that names no verdict, because silence is not approval.
    Partial,
}

impl Verdict {
    /// The token as written in a reviewer reply.
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Partial => "PARTIAL",
        }
    }

    /// Process exit code for a CI run. `PASS` is 0 and `FAIL` is 3, matching
    /// reference product's goal-loop contract; `PARTIAL` is a plain failure (1).
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Pass => 0,
            Verdict::Fail => 3,
            Verdict::Partial => 1,
        }
    }

    /// Reads the verdict out of a free-form reply.
    ///
    /// Only the first non-empty line is read, because [`REVIEWER_BRIEF`] asks
    /// for the verdict there. That is deliberate: a reply that restates the
    /// instructions, apologises, or hedges names no verdict on line one, and
    /// silence is not approval — it reads as [`Verdict::Partial`].
    pub fn parse(reply: &str) -> Verdict {
        let Some(first) = reply.lines().map(str::trim).find(|line| !line.is_empty()) else {
            return Verdict::Partial;
        };
        for word in first.split(|c: char| !c.is_ascii_alphabetic()) {
            match word.to_ascii_uppercase().as_str() {
                "PASS" => return Verdict::Pass,
                "FAIL" => return Verdict::Fail,
                "PARTIAL" => return Verdict::Partial,
                _ => {}
            }
        }
        Verdict::Partial
    }
}

/// What the reviewer is asked to judge. Deliberately small: the goal, and the
/// evidence (a diff, a test run, file excerpts) — never the conversation.
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub goal: SmolStr,
    pub evidence: SmolStr,
}

impl ReviewRequest {
    pub fn new(goal: impl Into<SmolStr>, evidence: impl Into<SmolStr>) -> Self {
        Self {
            goal: goal.into(),
            evidence: evidence.into(),
        }
    }

    /// The prompt handed to a fresh agent.
    pub fn prompt(&self) -> SmolStr {
        format!(
            "{REVIEWER_BRIEF}\n\nGoal:\n{}\n\nEvidence:\n{}",
            self.goal, self.evidence
        )
        .into()
    }
}

/// The reviewer's standing instructions. Kept verbatim in the module so every
/// reviewer implementation asks for the same output.
pub const REVIEWER_BRIEF: &str = "\
You are reviewing someone else's work. You did not write it and you cannot see \
the conversation that produced it; judge only the evidence below.\n\
Answer with exactly one verdict token on the first line: PASS, FAIL or PARTIAL.\n\
- PASS: the goal is met and the evidence supports it.\n\
- FAIL: the goal is not met, or the evidence contradicts it.\n\
- PARTIAL: you cannot tell from the evidence, or something required is missing.\n\
Then, on the following lines, name the single most important reason.";

/// A review outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub verdict: Verdict,
    /// The reviewer's reply, verbatim.
    pub notes: SmolStr,
}

/// Runs a review. Implementations differ in which model and tools they use.
#[async_trait]
pub trait Reviewer: Send + Sync + 'static {
    async fn review(&self, request: ReviewRequest) -> Result<Review, SmolStr>;
}

/// Reviews by running a fresh agent turn through the engine's [`AgentRunner`].
pub struct AgentReviewer {
    runner: std::sync::Arc<dyn AgentRunner>,
    name: SmolStr,
}

impl AgentReviewer {
    pub fn new(runner: std::sync::Arc<dyn AgentRunner>, name: impl Into<SmolStr>) -> Self {
        Self {
            runner,
            name: name.into(),
        }
    }
}

#[async_trait]
impl Reviewer for AgentReviewer {
    async fn review(&self, request: ReviewRequest) -> Result<Review, SmolStr> {
        // A fresh agent request: the reviewer's context is the evidence alone.
        let agent_request = AgentRequest {
            id: "reviewer".into(),
            name: self.name.clone(),
            task: request.prompt(),
            kind: AgentKind::Advisor,
            parent_id: None,
            profile: None,
        };
        let notes = self
            .runner
            .run(agent_request, AgentContext::detached())
            .await?;
        Ok(Review {
            verdict: Verdict::parse(&notes),
            notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_read_by_its_first_verdict_token() {
        assert_eq!(Verdict::parse("PASS\nthe tests cover it"), Verdict::Pass);
        assert_eq!(Verdict::parse("FAIL — the edit broke auth"), Verdict::Fail);
        assert_eq!(Verdict::parse("partial: no test run"), Verdict::Partial);
    }

    #[test]
    fn a_reply_that_names_no_verdict_is_partial() {
        assert_eq!(Verdict::parse("looks fine to me"), Verdict::Partial);
        assert_eq!(Verdict::parse(""), Verdict::Partial);
        assert_eq!(Verdict::parse("\n\n   \n"), Verdict::Partial);
    }

    #[test]
    fn echoing_the_brief_is_not_approval() {
        // A model that restates its instructions names no verdict on line one.
        assert_eq!(Verdict::parse(REVIEWER_BRIEF), Verdict::Partial);
        // A verdict on a later line is not read either.
        assert_eq!(Verdict::parse("Sure!\nPASS"), Verdict::Partial);
    }

    #[test]
    fn the_first_token_wins_even_when_another_is_quoted_later() {
        assert_eq!(
            Verdict::parse("FAIL because the coder claimed PASS without tests"),
            Verdict::Fail
        );
    }

    #[test]
    fn exit_codes_match_the_goal_loop_contract() {
        assert_eq!(Verdict::Pass.exit_code(), 0);
        assert_eq!(Verdict::Fail.exit_code(), 3);
        assert_eq!(Verdict::Partial.exit_code(), 1);
    }

    #[test]
    fn the_prompt_carries_the_brief_goal_and_evidence() {
        let request = ReviewRequest::new("make the build green", "cargo test: 3 failed");
        let prompt = request.prompt();
        assert!(prompt.contains(REVIEWER_BRIEF));
        assert!(prompt.contains("make the build green"));
        assert!(prompt.contains("cargo test: 3 failed"));
    }
}
