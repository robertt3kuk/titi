//! Coder ↔ reviewer goal loop.
//!
//! One goal is one bounded milestone: the coder produces a patch, a fresh
//! reviewer judges only that patch, and a failure comes back as the next
//! round's feedback. The reviewer never sees the coder's conversation.
//!
//! Spec: `docs/research/reference-product-port/README.md` (autonomous loop; goal
//! iterations: 8). Oscillation is a repeated patch, not a repeated review
//! sentence — the same FAIL prose with a new diff is still progress.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use smol_str::SmolStr;

use crate::review::{Review, ReviewRequest, Reviewer, Verdict};

/// Rounds one goal may spend ("goal iterations" in the spec).
pub const DEFAULT_GOAL_ROUNDS: u32 = 8;

/// A coder's patch. Oscillation compares [`normalize_patch`], not this text
/// and not the review that followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub text: SmolStr,
}

impl Patch {
    pub fn new(text: impl Into<SmolStr>) -> Self {
        Self { text: text.into() }
    }

    pub fn normalized(&self) -> String {
        normalize_patch(&self.text)
    }
}

/// Canonical patch text.
///
/// Line endings become `\n`, trailing whitespace on each line is dropped, and
/// leading or trailing blank lines are dropped. A trailing newline is added
/// when the patch is non-empty, so `"a"` and `"a\n"` are one patch. Leading
/// whitespace is kept: a diff uses it to tell additions from context. Review
/// prose is not an input.
pub fn normalize_patch(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = unified.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// What the coder is asked to do on one round. `feedback` is the previous
/// review; the first round has none. The reviewer is not given this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRequest {
    pub goal: SmolStr,
    pub round: u32,
    pub feedback: Option<Review>,
}

/// Produces the next patch. Implementations own models and tools; the loop
/// only sees the patch.
#[async_trait]
pub trait Coder: Send + Sync + 'static {
    async fn code(&self, request: CodeRequest) -> Result<Patch, SmolStr>;
}

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStop {
    /// The reviewer returned [`Verdict::Pass`].
    Passed,
    /// Every round was spent and the reviewer never passed.
    RoundCap,
    /// The coder repeated a patch already produced in this goal.
    Oscillation,
    /// [`GoalCancel`] fired. Remaining rounds are not run.
    Cancelled,
    /// The coder or the reviewer returned an error. This loop does not pick
    /// another model; the turn loop already owns fallback.
    Error,
}

/// A shared cancel flag. `Cancel` on a running turn uses the same idea: set
/// the flag, and the loop stops at the next step boundary — or sooner, if the
/// in-flight coder or reviewer future is still pending.
#[derive(Clone, Debug, Default)]
pub struct GoalCancel {
    aborted: Arc<AtomicBool>,
}

impl GoalCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.aborted.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }
}

/// How one goal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOutcome {
    pub stop: GoalStop,
    /// [`Verdict::Pass`] on success. [`Verdict::Fail`] when the cap is spent
    /// or the patch oscillates. `None` on cancel or on a coder/reviewer error.
    pub verdict: Option<Verdict>,
    /// Coder invocations that returned a patch, including the one that
    /// oscillated. A cancel during the coder does not increment this.
    pub rounds: u32,
    pub patch: Option<Patch>,
    pub review: Option<Review>,
    pub error: Option<SmolStr>,
}

impl GoalOutcome {
    fn cancelled(rounds: u32, patch: Option<Patch>, review: Option<Review>) -> Self {
        Self {
            stop: GoalStop::Cancelled,
            verdict: None,
            rounds,
            patch,
            review,
            error: None,
        }
    }

    fn failed_call(
        rounds: u32,
        error: SmolStr,
        patch: Option<Patch>,
        review: Option<Review>,
    ) -> Self {
        Self {
            stop: GoalStop::Error,
            verdict: None,
            rounds,
            patch,
            review,
            error: Some(error),
        }
    }
}

/// Runs coder then reviewer until pass, cap, oscillation, cancel, or error.
pub struct GoalLoop {
    coder: Arc<dyn Coder>,
    reviewer: Arc<dyn Reviewer>,
    max_rounds: u32,
    cancel: GoalCancel,
}

impl GoalLoop {
    pub fn new(coder: Arc<dyn Coder>, reviewer: Arc<dyn Reviewer>) -> Self {
        Self {
            coder,
            reviewer,
            max_rounds: DEFAULT_GOAL_ROUNDS,
            cancel: GoalCancel::new(),
        }
    }

    /// Caps the loop. Zero is raised to one so a misconfigured cap cannot
    /// skip the goal or spin.
    pub fn with_max_rounds(mut self, rounds: u32) -> Self {
        self.max_rounds = rounds.max(1);
        self
    }

    pub fn with_cancel(mut self, cancel: GoalCancel) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn max_rounds(&self) -> u32 {
        self.max_rounds
    }

    /// Runs `goal` to a terminal outcome. Always returns; it does not panic
    /// on a coder or reviewer error.
    pub async fn run(&self, goal: impl Into<SmolStr>) -> GoalOutcome {
        let goal = goal.into();
        let mut seen = Vec::<String>::new();
        let mut feedback = None;
        let mut last_patch = None;
        let mut last_review = None;
        let mut completed = 0u32;

        for round in 1..=self.max_rounds {
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, last_patch, last_review);
            }
            let request = CodeRequest {
                goal: goal.clone(),
                round,
                feedback,
            };
            let coded = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, last_patch, last_review);
                }
                result = self.coder.code(request) => result,
            };
            let patch = match coded {
                Ok(patch) => patch,
                Err(error) => {
                    return GoalOutcome::failed_call(completed, error, last_patch, last_review);
                }
            };
            completed = round;
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, Some(patch), last_review);
            }
            let normalized = patch.normalized();
            if seen.iter().any(|previous| previous == &normalized) {
                return GoalOutcome {
                    stop: GoalStop::Oscillation,
                    verdict: Some(Verdict::Fail),
                    rounds: completed,
                    patch: Some(patch),
                    review: last_review,
                    error: None,
                };
            }
            seen.push(normalized);
            last_patch = Some(patch.clone());

            let review_request = ReviewRequest::new(goal.clone(), patch.text);
            let reviewed = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, last_patch, last_review);
                }
                result = self.reviewer.review(review_request) => result,
            };
            let review = match reviewed {
                Ok(review) => review,
                Err(error) => {
                    return GoalOutcome::failed_call(completed, error, last_patch, last_review);
                }
            };
            last_review = Some(review.clone());
            if review.verdict == Verdict::Pass {
                return GoalOutcome {
                    stop: GoalStop::Passed,
                    verdict: Some(Verdict::Pass),
                    rounds: completed,
                    patch: last_patch,
                    review: last_review,
                    error: None,
                };
            }
            // FAIL and PARTIAL both go back to the coder. A repeated sentence
            // is not oscillation; only a repeated patch is.
            feedback = Some(review);
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, last_patch, last_review);
            }
        }

        GoalOutcome {
            stop: GoalStop::RoundCap,
            verdict: Some(Verdict::Fail),
            rounds: completed,
            patch: last_patch,
            review: last_review,
            error: None,
        }
    }
}

async fn wait_cancelled(cancel: &GoalCancel) {
    while !cancel.is_cancelled() {
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_bound_is_the_spec_goal_iterations() {
        assert_eq!(DEFAULT_GOAL_ROUNDS, 8);
    }

    #[test]
    fn normalization_ignores_line_endings_and_trailing_space_only() {
        assert_eq!(normalize_patch("a\r\nb \n"), normalize_patch("a\nb\n"));
        assert_eq!(normalize_patch("\n\na\n\n"), normalize_patch("a"));
        assert_eq!(normalize_patch(""), "");
        // Leading space is a diff marker, not noise.
        assert_ne!(
            normalize_patch("+fn a() {}"),
            normalize_patch(" +fn a() {}")
        );
        // An internal blank line is part of the patch.
        assert_ne!(normalize_patch("a\n\nb\n"), normalize_patch("a\nb\n"));
    }
}
