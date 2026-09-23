//! Coder ↔ reviewer goal loop.
//!
//! One goal is one bounded milestone: the coder produces a patch, a fresh
//! reviewer judges only that patch, and a failure comes back as the next
//! round's feedback. The reviewer never sees the coder's conversation.
//!
//! Spec: `docs/research/reference-product-port/README.md` (autonomous loop; goal
//! iterations: 8). Oscillation is a repeated patch, not a repeated review
//! sentence — the same FAIL prose with a new diff is still progress.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::review::{Review, ReviewRequest, Reviewer, Verdict};
use async_trait::async_trait;
use smol_str::SmolStr;
use tokio::sync::Notify;

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
/// review and `gate` the previous check failure; one round carries at most
/// one of them, and the first round has neither. The reviewer is not given
/// this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRequest {
    pub goal: SmolStr,
    pub round: u32,
    pub feedback: Option<Review>,
    /// Output of the checks that stopped the previous patch before it reached
    /// a reviewer, trimmed to [`GATE_OUTPUT_CAP`].
    pub gate: Option<SmolStr>,
}

/// Produces the next patch. Implementations own models and tools; the loop
/// only sees the patch.
#[async_trait]
pub trait Coder: Send + Sync + 'static {
    async fn code(&self, request: CodeRequest) -> Result<Patch, SmolStr>;
}

/// One configured check: a program and its arguments, never a shell line, so
/// nothing is word-split or expanded behind the user's back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateCommand {
    pub program: SmolStr,
    pub args: Vec<SmolStr>,
}

impl GateCommand {
    pub fn new(
        program: impl Into<SmolStr>,
        args: impl IntoIterator<Item = impl Into<SmolStr>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The command as one line, for feedback and transcripts.
    pub fn label(&self) -> String {
        let mut line = self.program.to_string();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

/// What the checks concluded about one patch.
///
/// [`Self::Red`] and [`Self::Unavailable`] are deliberately not the same
/// outcome: red means the patch is bad and the coder can fix it, unavailable
/// means there was nothing to check it with. Feeding the second one back as
/// feedback would spin the coder through every remaining round over a machine
/// it cannot change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Every configured check exited zero, or nothing was configured.
    Green,
    /// A check ran and failed. `report` is what the coder is told.
    Red { report: SmolStr },
    /// A check could not be run at all: missing binary, no permission.
    Unavailable { error: SmolStr },
}

/// Runs the configured checks on a patch before a reviewer is paid for.
#[async_trait]
pub trait Gates: Send + Sync + 'static {
    async fn check(&self, patch: &Patch) -> GateVerdict;
}

/// How much of a failing check's output goes back to the coder. A full test
/// log on a large workspace is tens of thousands of tokens.
pub const GATE_OUTPUT_CAP: usize = 4_000;

/// The checks as commands — `cargo check`, `pytest -q`, `npm test`, whatever
/// the project is; the loop itself knows no language. An empty list is the
/// default and means "no gates": the coder's patch goes straight to the
/// reviewer.
#[derive(Debug, Clone, Default)]
pub struct CommandGates {
    commands: Vec<GateCommand>,
    dir: Option<PathBuf>,
}

impl CommandGates {
    pub fn new(commands: Vec<GateCommand>) -> Self {
        Self {
            commands,
            dir: None,
        }
    }

    /// Directory the checks run in. Default is the process's own.
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

#[async_trait]
impl Gates for CommandGates {
    /// The checks read the workspace the coder just edited; the patch text is
    /// evidence for the reviewer, not an input to a build.
    async fn check(&self, _patch: &Patch) -> GateVerdict {
        if self.commands.is_empty() {
            return GateVerdict::Green;
        }
        let commands = self.commands.clone();
        let dir = self.dir.clone();
        // A check is a whole build: never on a runtime thread.
        match tokio::task::spawn_blocking(move || run_gates(&commands, dir.as_deref())).await {
            Ok(verdict) => verdict,
            Err(error) => GateVerdict::Unavailable {
                error: format!("gate runner stopped: {error}").into(),
            },
        }
    }
}

/// First failure wins: a later check would only report fallout from this one.
fn run_gates(commands: &[GateCommand], dir: Option<&Path>) -> GateVerdict {
    for command in commands {
        let mut process = Command::new(command.program.as_str());
        process.args(command.args.iter().map(SmolStr::as_str));
        if let Some(dir) = dir {
            process.current_dir(dir);
        }
        let output = match process.output() {
            Ok(output) => output,
            Err(error) => {
                return GateVerdict::Unavailable {
                    error: format!("gate `{}` did not run: {error}", command.label()).into(),
                };
            }
        };
        if !output.status.success() {
            return GateVerdict::Red {
                report: gate_failure(command, &output),
            };
        }
    }
    GateVerdict::Green
}

fn gate_failure(command: &GateCommand, output: &std::process::Output) -> SmolStr {
    let status = match output.status.code() {
        Some(code) => format!("exit {code}"),
        None => "killed by signal".to_owned(),
    };
    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let mut report = format!("`{}` failed ({status}).", command.label());
    let excerpt = gate_excerpt(body.trim());
    if !excerpt.is_empty() {
        report.push('\n');
        report.push_str(&excerpt);
    }
    report.into()
}

/// The part of a check's output worth spending prompt on: the tail, where
/// runners print their summary, plus the lines that name a failure, which the
/// summary does not. Dropped stretches are marked with an ellipsis line.
fn gate_excerpt(text: &str) -> String {
    if text.len() <= GATE_OUTPUT_CAP {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = vec![false; lines.len()];
    let mut spent = 0usize;
    for (index, line) in lines.iter().enumerate().rev() {
        let cost = line.len() + 1;
        if spent + cost > GATE_OUTPUT_CAP / 2 {
            break;
        }
        spent += cost;
        kept[index] = true;
    }
    for (index, line) in lines.iter().enumerate() {
        if kept[index] || !names_a_failure(line) {
            continue;
        }
        let cost = line.len() + 1;
        if spent + cost > GATE_OUTPUT_CAP {
            break;
        }
        spent += cost;
        kept[index] = true;
    }
    if !kept.iter().any(|keep| *keep) {
        // One enormous line: no whole line fits, so cut bytes rather than
        // hand the coder nothing.
        return format!("…\n{}", tail_bytes(text, GATE_OUTPUT_CAP));
    }
    let mut out = String::with_capacity(spent + lines.len());
    let mut gap = false;
    for (index, line) in lines.iter().enumerate() {
        if !kept[index] {
            gap = true;
            continue;
        }
        if gap {
            out.push_str("…\n");
            gap = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Markers the runners a project is likely to gate on print on failure:
/// rustc and cargo, pytest, npm, go test.
fn names_a_failure(line: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "error",
        "err!",
        "panicked",
        "failed",
        "failure",
        "assertion",
    ];
    let bytes = line.as_bytes();
    MARKERS.iter().any(|marker| {
        let marker = marker.as_bytes();
        bytes
            .windows(marker.len())
            .any(|window| window.eq_ignore_ascii_case(marker))
    })
}

/// The last `cap` bytes, cut on a character boundary.
fn tail_bytes(text: &str, cap: usize) -> &str {
    let mut start = text.len().saturating_sub(cap);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
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
    /// A configured check could not be run at all. The patch was never
    /// judged, so the coder is not asked again: nothing it writes would fix
    /// a missing binary.
    GateUnavailable,
}

/// A shared cancel flag. Setting it wakes a parked waiter; the loop does not
/// poll the flag while a coder or reviewer call is in flight.
#[derive(Clone, Debug, Default)]
pub struct GoalCancel {
    aborted: Arc<AtomicBool>,
    woken: Arc<Notify>,
}

impl GoalCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // `notify_one` stores a permit when nobody is waiting yet, so a cancel
        // that lands between the flag check and the park is not lost.
        self.woken.notify_one();
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
    /// The last failing check's report, when a gate turned a round back.
    pub gate: Option<SmolStr>,
    pub error: Option<SmolStr>,
}

impl GoalOutcome {
    fn cancelled(
        rounds: u32,
        patch: Option<Patch>,
        review: Option<Review>,
        gate: Option<SmolStr>,
    ) -> Self {
        Self {
            stop: GoalStop::Cancelled,
            verdict: None,
            rounds,
            patch,
            review,
            gate,
            error: None,
        }
    }

    fn failed_call(
        rounds: u32,
        error: SmolStr,
        patch: Option<Patch>,
        review: Option<Review>,
        gate: Option<SmolStr>,
    ) -> Self {
        Self {
            stop: GoalStop::Error,
            verdict: None,
            rounds,
            patch,
            review,
            gate,
            error: Some(error),
        }
    }

    fn gate_unavailable(
        rounds: u32,
        error: SmolStr,
        patch: Option<Patch>,
        review: Option<Review>,
        gate: Option<SmolStr>,
    ) -> Self {
        Self {
            stop: GoalStop::GateUnavailable,
            verdict: None,
            rounds,
            patch,
            review,
            gate,
            error: Some(error),
        }
    }
}

/// Runs coder, then the configured checks, then reviewer until pass, cap,
/// oscillation, cancel, or error.
pub struct GoalLoop {
    coder: Arc<dyn Coder>,
    reviewer: Arc<dyn Reviewer>,
    gates: Arc<dyn Gates>,
    max_rounds: u32,
    cancel: GoalCancel,
}

impl GoalLoop {
    pub fn new(coder: Arc<dyn Coder>, reviewer: Arc<dyn Reviewer>) -> Self {
        Self {
            coder,
            reviewer,
            gates: Arc::new(CommandGates::default()),
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

    /// Checks that must pass before a reviewer is called. The default is
    /// none, which is the loop's original shape: coder, then reviewer.
    pub fn with_gates(mut self, gates: Arc<dyn Gates>) -> Self {
        self.gates = gates;
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
        let mut gate = None;
        let mut last_patch = None;
        let mut last_review = None;
        let mut last_gate = None;
        let mut completed = 0u32;

        for round in 1..=self.max_rounds {
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, last_patch, last_review, last_gate);
            }
            let request = CodeRequest {
                goal: goal.clone(),
                round,
                feedback,
                gate,
            };
            let coded = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, last_patch, last_review, last_gate);
                }
                result = self.coder.code(request) => result,
            };
            let patch = match coded {
                Ok(patch) => patch,
                Err(error) => {
                    return GoalOutcome::failed_call(
                        completed,
                        error,
                        last_patch,
                        last_review,
                        last_gate,
                    );
                }
            };
            completed = round;
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, Some(patch), last_review, last_gate);
            }
            let normalized = patch.normalized();
            if seen.iter().any(|previous| previous == &normalized) {
                return GoalOutcome {
                    stop: GoalStop::Oscillation,
                    verdict: Some(Verdict::Fail),
                    rounds: completed,
                    patch: Some(patch),
                    review: last_review,
                    gate: last_gate,
                    error: None,
                };
            }
            seen.push(normalized);
            last_patch = Some(patch.clone());

            // The checks come before the reviewer: a review is a model call,
            // and a patch that does not build has nothing worth reviewing.
            let gated = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, last_patch, last_review, last_gate);
                }
                verdict = self.gates.check(&patch) => verdict,
            };
            match gated {
                GateVerdict::Green => {}
                GateVerdict::Red { report } => {
                    last_gate = Some(report.clone());
                    gate = Some(report);
                    feedback = None;
                    if self.cancel.is_cancelled() {
                        return GoalOutcome::cancelled(
                            completed,
                            last_patch,
                            last_review,
                            last_gate,
                        );
                    }
                    continue;
                }
                GateVerdict::Unavailable { error } => {
                    return GoalOutcome::gate_unavailable(
                        completed,
                        error,
                        last_patch,
                        last_review,
                        last_gate,
                    );
                }
            }

            let review_request = ReviewRequest::new(goal.clone(), patch.text);
            let reviewed = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, last_patch, last_review, last_gate);
                }
                result = self.reviewer.review(review_request) => result,
            };
            let review = match reviewed {
                Ok(review) => review,
                Err(error) => {
                    return GoalOutcome::failed_call(
                        completed,
                        error,
                        last_patch,
                        last_review,
                        last_gate,
                    );
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
                    gate: last_gate,
                    error: None,
                };
            }
            // FAIL and PARTIAL both go back to the coder. A repeated sentence
            // is not oscillation; only a repeated patch is.
            feedback = Some(review);
            // The next round answers this review; an older gate report is
            // about a patch that is already gone.
            gate = None;
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, last_patch, last_review, last_gate);
            }
        }

        GoalOutcome {
            stop: GoalStop::RoundCap,
            verdict: Some(Verdict::Fail),
            rounds: completed,
            patch: last_patch,
            review: last_review,
            gate: last_gate,
            error: None,
        }
    }
}

/// The entry `/goal` uses. Surfaces and tests call this; it does not replace
/// `SubmitPrompt`.
pub async fn run_goal(
    coder: Arc<dyn Coder>,
    reviewer: Arc<dyn Reviewer>,
    goal: impl Into<SmolStr>,
) -> GoalOutcome {
    GoalLoop::new(coder, reviewer).run(goal).await
}

/// One transcript line: stop, rounds, and verdict when there is one.
pub fn goal_report(outcome: &GoalOutcome) -> String {
    let stop = match outcome.stop {
        GoalStop::Passed => "passed",
        GoalStop::RoundCap => "round cap",
        GoalStop::Oscillation => "oscillation",
        GoalStop::Cancelled => "cancelled",
        GoalStop::Error => "error",
        GoalStop::GateUnavailable => "gate unavailable",
    };
    let rounds = if outcome.rounds == 1 {
        "1 round".to_owned()
    } else {
        format!("{} rounds", outcome.rounds)
    };
    let mut line = format!("goal: {stop} · {rounds}");
    if let Some(verdict) = outcome.verdict {
        line.push_str(" · verdict ");
        line.push_str(&verdict.as_str().to_ascii_lowercase());
    }
    if let Some(gate) = &outcome.gate {
        line.push_str(" · gate red: ");
        line.push_str(gate.lines().next().unwrap_or_default());
    }
    if let Some(error) = &outcome.error {
        line.push_str(" · ");
        line.push_str(error);
    }
    line
}

/// A coder that asks the session's [`crate::AgentRunner`] for the next patch.
///
/// The reviewer is a separate runner, so this conversation is not the review.
pub struct RunnerCoder {
    runner: Arc<dyn crate::AgentRunner>,
}

impl RunnerCoder {
    pub fn new(runner: Arc<dyn crate::AgentRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl Coder for RunnerCoder {
    async fn code(&self, request: CodeRequest) -> Result<Patch, SmolStr> {
        let mut task = format!("Goal:\n{}\n\nRound {}.\n", request.goal, request.round);
        if let Some(gate) = &request.gate {
            task.push_str("The project checks failed on your last patch, before any review:\n");
            task.push_str(gate);
            task.push_str("\nMake the checks pass, then produce the patch.\n");
        } else if let Some(review) = &request.feedback {
            task.push_str("Previous review (");
            task.push_str(review.verdict.as_str());
            task.push_str("):\n");
            task.push_str(&review.notes);
            task.push_str("\nRevise the patch.\n");
        } else {
            task.push_str("Produce a patch that meets the goal.\n");
        }
        let text = self
            .runner
            .run(
                crate::AgentRequest {
                    id: "coder".into(),
                    name: "coder".into(),
                    task: task.into(),
                    kind: crate::AgentKind::Subagent,
                    parent_id: None,
                },
                crate::AgentContext::detached(),
            )
            .await?;
        Ok(Patch::new(text))
    }
}

async fn wait_cancelled(cancel: &GoalCancel) {
    loop {
        // Subscribe before the flag check. A permit from `cancel` is then
        // either already stored or delivered to this waiter; nothing spins.
        let notified = cancel.woken.notified();
        if cancel.is_cancelled() {
            return;
        }
        notified.await;
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

    #[test]
    fn a_short_output_is_handed_over_whole() {
        let log = "error[E0308]: mismatched types\n  --> src/main.rs:3:5";
        assert_eq!(gate_excerpt(log), log);
    }

    #[test]
    fn a_huge_log_keeps_the_failure_lines_and_the_tail() {
        let mut log = String::from("error[E0433]: cannot find `frobnicate`\n");
        for index in 0..4_000 {
            log.push_str(&format!("   Compiling crate-{index} v0.1.0\n"));
        }
        log.push_str("error: could not compile `titi-engine`\n");

        let excerpt = gate_excerpt(&log);
        assert!(excerpt.len() < log.len() / 4);
        assert!(excerpt.len() <= GATE_OUTPUT_CAP + 8);
        // The first error is what to fix; the last line is the summary.
        assert!(excerpt.contains("error[E0433]: cannot find `frobnicate`"));
        assert!(excerpt.contains("error: could not compile `titi-engine`"));
        assert!(excerpt.contains('…'));
    }

    #[test]
    fn one_enormous_line_is_cut_on_a_character_boundary() {
        let log = "ошибка ".repeat(GATE_OUTPUT_CAP);
        let excerpt = gate_excerpt(&log);
        assert!(excerpt.len() <= GATE_OUTPUT_CAP + 8);
        assert!(excerpt.ends_with("ошибка "));
    }
}
