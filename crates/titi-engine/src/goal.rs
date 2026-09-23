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

/// Consecutive failed rounds after which the loop stops asking the same
/// question.
///
/// One failure is ordinary work: a review or a check found something and the
/// next round fixes it. Two in a row mean the same inputs keep producing the
/// same dead end, so the round after them changes a variable instead.
pub const STUCK_AFTER_FAILURES: u32 = 2;

/// The one variable a stuck round changes.
///
/// Exactly one per round, and never the same one two rounds running: a round
/// that changed several things at once would not tell anyone which of them
/// mattered. A [`Coder`] is handed nothing but a [`CodeRequest`], so this is
/// what the loop can turn — the round's context, or the instruction that
/// rides on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyChange {
    /// Solve the goal another way instead of repairing the patch that keeps
    /// failing.
    Strategy,
    /// Drop the accumulated review and check output: the round is asked with
    /// the goal alone, the way the first round was.
    FreshContext,
    /// Spend a larger reasoning budget than the last round did.
    Effort,
}

impl StrategyChange {
    /// Wording for the round log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strategy => "strategy hint",
            Self::FreshContext => "fresh context",
            Self::Effort => "effort",
        }
    }

    /// What the coder is told to do differently this round.
    pub fn instruction(self) -> &'static str {
        match self {
            Self::Strategy => {
                "Changed variable: strategy. The previous approach failed twice in a row; \
                 take a different one instead of patching it again."
            }
            Self::FreshContext => {
                "Changed variable: fresh context. The earlier reviews and check logs are \
                 withheld on purpose; work from the goal itself."
            }
            Self::Effort => {
                "Changed variable: effort. Spend a larger reasoning budget on this round \
                 than on the last one before writing the patch."
            }
        }
    }

    /// Cycles the knobs, so a stuck goal never changes the same one twice in
    /// a row and comes back to the first only after all three were tried.
    fn for_streak(failures: u32) -> Self {
        match failures.saturating_sub(STUCK_AFTER_FAILURES) % 3 {
            0 => Self::Strategy,
            1 => Self::FreshContext,
            _ => Self::Effort,
        }
    }

    /// Turns this one knob on the round that is about to be asked.
    ///
    /// [`Self::FreshContext`] withholds what the last rounds produced; the
    /// other two ride on whichever of the two inputs this round carries, last,
    /// so the coder reads the failure first and the new instruction after it.
    ///
    /// Returns `false` when there was nothing to turn — a round with no
    /// feedback and no check output is already the fresh one. The log then
    /// records no change, because none happened.
    fn apply(self, feedback: &mut Option<Review>, gate: &mut Option<SmolStr>) -> bool {
        let carried = feedback.is_some() || gate.is_some();
        match self {
            Self::FreshContext => {
                *feedback = None;
                *gate = None;
                carried
            }
            Self::Strategy | Self::Effort => {
                let note = self.instruction();
                if let Some(report) = gate.as_mut() {
                    *report = format!("{report}\n{note}").into();
                } else if let Some(review) = feedback.as_mut() {
                    review.notes = format!("{}\n{note}", review.notes).into();
                }
                carried
            }
        }
    }
}

/// One line of the round log: the round, and the single variable it changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundChange {
    pub round: u32,
    pub change: StrategyChange,
}

impl RoundChange {
    pub fn label(&self) -> String {
        format!("round {} {}", self.round, self.change.as_str())
    }
}

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
///
/// Once a goal is stuck (see [`STUCK_AFTER_FAILURES`]) the loop changes one
/// variable per round through these two fields: it either withholds them or
/// appends its instruction to the one this round carries. The round log in
/// [`GoalOutcome::changes`] says which.
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

/// What the loop has accumulated so far: the last patch, the last review, the
/// last red check, and the round log. Every terminal outcome is built from
/// one of these, so the loop carries the state in a single value instead of
/// threading four parallel locals through a dozen early returns.
#[derive(Debug, Default)]
struct Trail {
    patch: Option<Patch>,
    review: Option<Review>,
    gate: Option<SmolStr>,
    changes: Vec<RoundChange>,
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
    /// The round log of a stuck goal: which round changed which variable.
    /// Empty while the goal is still making progress.
    pub changes: Vec<RoundChange>,
    pub error: Option<SmolStr>,
}

impl GoalOutcome {
    fn judged(stop: GoalStop, verdict: Verdict, rounds: u32, trail: Trail) -> Self {
        Self {
            stop,
            verdict: Some(verdict),
            rounds,
            patch: trail.patch,
            review: trail.review,
            gate: trail.gate,
            changes: trail.changes,
            error: None,
        }
    }

    /// Process exit code for a CI run: `PASS` is 0, `PARTIAL` is 1, and
    /// everything else is 3.
    ///
    /// No verdict at all — cancelled, a coder or reviewer error, a check that
    /// could not run — is a plain failure: none of those is a pass, and none
    /// is a half-result a bot could act on.
    pub fn exit_code(&self) -> i32 {
        self.verdict.unwrap_or(Verdict::Fail).exit_code()
    }

    fn cancelled(rounds: u32, trail: Trail) -> Self {
        Self {
            stop: GoalStop::Cancelled,
            verdict: None,
            rounds,
            patch: trail.patch,
            review: trail.review,
            gate: trail.gate,
            changes: trail.changes,
            error: None,
        }
    }

    fn failed_call(rounds: u32, error: SmolStr, trail: Trail) -> Self {
        Self {
            stop: GoalStop::Error,
            verdict: None,
            rounds,
            patch: trail.patch,
            review: trail.review,
            gate: trail.gate,
            changes: trail.changes,
            error: Some(error),
        }
    }

    fn gate_unavailable(rounds: u32, error: SmolStr, trail: Trail) -> Self {
        Self {
            stop: GoalStop::GateUnavailable,
            verdict: None,
            rounds,
            patch: trail.patch,
            review: trail.review,
            gate: trail.gate,
            changes: trail.changes,
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
    ///
    /// A round that ends in a red check or a non-passing review is a failure.
    /// After [`STUCK_AFTER_FAILURES`] of them in a row the loop stops handing
    /// the same failure to the same request: every further round changes
    /// exactly one variable ([`StrategyChange`]) and records it in
    /// [`GoalOutcome::changes`]. A red check turning green is progress, so the
    /// streak starts over there.
    pub async fn run(&self, goal: impl Into<SmolStr>) -> GoalOutcome {
        let goal = goal.into();
        let mut seen = Vec::<String>::new();
        let mut feedback = None;
        let mut gate = None;
        let mut trail = Trail::default();
        let mut completed = 0u32;
        let mut failures = 0u32;
        let mut gate_was_red = false;

        for round in 1..=self.max_rounds {
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, trail);
            }
            if failures >= STUCK_AFTER_FAILURES {
                let change = StrategyChange::for_streak(failures);
                if change.apply(&mut feedback, &mut gate) {
                    trail.changes.push(RoundChange { round, change });
                }
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
                    return GoalOutcome::cancelled(completed, trail);
                }
                result = self.coder.code(request) => result,
            };
            let patch = match coded {
                Ok(patch) => patch,
                Err(error) => {
                    return GoalOutcome::failed_call(completed, error, trail);
                }
            };
            completed = round;
            if self.cancel.is_cancelled() {
                trail.patch = Some(patch);
                return GoalOutcome::cancelled(completed, trail);
            }
            let normalized = patch.normalized();
            if seen.iter().any(|previous| previous == &normalized) {
                trail.patch = Some(patch);
                return GoalOutcome::judged(GoalStop::Oscillation, Verdict::Fail, completed, trail);
            }
            seen.push(normalized);
            trail.patch = Some(patch.clone());

            // The checks come before the reviewer: a review is a model call,
            // and a patch that does not build has nothing worth reviewing.
            let gated = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, trail);
                }
                verdict = self.gates.check(&patch) => verdict,
            };
            match gated {
                GateVerdict::Green => {
                    if gate_was_red {
                        // A patch that now builds is movement, not another
                        // repetition: the streak starts over rather than
                        // spending a variable on a goal that is unsticking.
                        gate_was_red = false;
                        failures = 0;
                    }
                }
                GateVerdict::Red { report } => {
                    trail.gate = Some(report.clone());
                    gate = Some(report);
                    feedback = None;
                    gate_was_red = true;
                    failures += 1;
                    if self.cancel.is_cancelled() {
                        return GoalOutcome::cancelled(completed, trail);
                    }
                    continue;
                }
                GateVerdict::Unavailable { error } => {
                    return GoalOutcome::gate_unavailable(completed, error, trail);
                }
            }

            let review_request = ReviewRequest::new(goal.clone(), patch.text);
            let reviewed = tokio::select! {
                biased;
                () = wait_cancelled(&self.cancel) => {
                    return GoalOutcome::cancelled(completed, trail);
                }
                result = self.reviewer.review(review_request) => result,
            };
            let review = match reviewed {
                Ok(review) => review,
                Err(error) => {
                    return GoalOutcome::failed_call(completed, error, trail);
                }
            };
            trail.review = Some(review.clone());
            if review.verdict == Verdict::Pass {
                return GoalOutcome::judged(GoalStop::Passed, Verdict::Pass, completed, trail);
            }
            // FAIL and PARTIAL both go back to the coder. A repeated sentence
            // is not oscillation; only a repeated patch is.
            feedback = Some(review);
            // The next round answers this review; an older gate report is
            // about a patch that is already gone.
            gate = None;
            failures += 1;
            if self.cancel.is_cancelled() {
                return GoalOutcome::cancelled(completed, trail);
            }
        }

        // The cap is spent. A last review that could not tell is still a
        // PARTIAL result, and CI reads that differently from a rejection;
        // anything else — including no review at all — is a failure.
        let verdict = match trail.review.as_ref().map(|review| review.verdict) {
            Some(Verdict::Partial) => Verdict::Partial,
            _ => Verdict::Fail,
        };
        GoalOutcome::judged(GoalStop::RoundCap, verdict, completed, trail)
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

/// One transcript line: stop, rounds, the verdict when there is one, and the
/// round log of a stuck goal — which round changed which variable.
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
        line.push_str(VERDICT_MARK);
        line.push_str(&verdict.as_str().to_ascii_lowercase());
    }
    if !outcome.changes.is_empty() {
        let log: Vec<String> = outcome.changes.iter().map(RoundChange::label).collect();
        line.push_str(" · changed: ");
        line.push_str(&log.join(", "));
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

/// The mark [`goal_report`] writes before the verdict word, and the one
/// [`goal_exit_code`] looks for.
const VERDICT_MARK: &str = " · verdict ";

/// The exit code behind a [`goal_report`] line.
///
/// A surface that only sees `EngineEvent::GoalFinished` has the line and
/// nothing else, so the code that writes the line reads it back: an unknown
/// or missing verdict is a failure, never a pass.
pub fn goal_exit_code(report: &str) -> i32 {
    let Some(rest) = report.split(VERDICT_MARK).nth(1) else {
        return Verdict::Fail.exit_code();
    };
    let word = rest
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    match word.to_ascii_uppercase().as_str() {
        token if token == Verdict::Pass.as_str() => Verdict::Pass.exit_code(),
        token if token == Verdict::Partial.as_str() => Verdict::Partial.exit_code(),
        _ => Verdict::Fail.exit_code(),
    }
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
    use std::sync::Mutex;

    /// Answers with a new patch every round and keeps the requests it saw.
    struct RecordingCoder {
        seen: Mutex<Vec<CodeRequest>>,
    }

    impl RecordingCoder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<CodeRequest> {
            match self.seen.lock() {
                Ok(seen) => seen.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[async_trait]
    impl Coder for RecordingCoder {
        async fn code(&self, request: CodeRequest) -> Result<Patch, SmolStr> {
            let round = request.round;
            match self.seen.lock() {
                Ok(mut seen) => seen.push(request),
                Err(poisoned) => poisoned.into_inner().push(request),
            }
            Ok(Patch::new(format!("patch for round {round}")))
        }
    }

    /// Answers with one scripted verdict, forever.
    struct FixedReviewer {
        verdict: Verdict,
    }

    #[async_trait]
    impl Reviewer for FixedReviewer {
        async fn review(&self, _request: ReviewRequest) -> Result<Review, SmolStr> {
            Ok(Review {
                verdict: self.verdict,
                notes: "as judged".into(),
            })
        }
    }

    #[tokio::test]
    async fn an_outcome_exits_zero_on_pass_one_on_partial_and_three_on_fail() {
        let mut codes = Vec::new();
        for verdict in [Verdict::Pass, Verdict::Partial, Verdict::Fail] {
            let outcome = GoalLoop::new(
                RecordingCoder::new() as Arc<dyn Coder>,
                Arc::new(FixedReviewer { verdict }),
            )
            .with_max_rounds(2)
            .run("ship it")
            .await;
            codes.push((outcome.stop, outcome.verdict, outcome.exit_code()));
        }

        assert_eq!(
            codes,
            vec![
                (GoalStop::Passed, Some(Verdict::Pass), 0),
                // The cap is spent with a reviewer that never could tell.
                (GoalStop::RoundCap, Some(Verdict::Partial), 1),
                (GoalStop::RoundCap, Some(Verdict::Fail), 3),
            ]
        );
    }

    #[tokio::test]
    async fn a_goal_that_never_reached_a_verdict_exits_three() {
        struct BrokenCoder;

        #[async_trait]
        impl Coder for BrokenCoder {
            async fn code(&self, _request: CodeRequest) -> Result<Patch, SmolStr> {
                Err("provider down".into())
            }
        }

        let outcome = GoalLoop::new(
            Arc::new(BrokenCoder),
            Arc::new(FixedReviewer {
                verdict: Verdict::Pass,
            }),
        )
        .run("ship it")
        .await;

        assert_eq!(outcome.stop, GoalStop::Error);
        assert_eq!(outcome.verdict, None);
        assert_eq!(outcome.exit_code(), 3);
    }

    /// The headless surface sees the report line and nothing else, so the
    /// line has to carry the code back.
    #[test]
    fn the_report_line_carries_the_exit_code_back() {
        let outcome = |stop, verdict| GoalOutcome {
            stop,
            verdict,
            rounds: 2,
            patch: None,
            review: None,
            gate: None,
            changes: vec![RoundChange {
                round: 2,
                change: StrategyChange::Effort,
            }],
            error: None,
        };
        for (stop, verdict) in [
            (GoalStop::Passed, Some(Verdict::Pass)),
            (GoalStop::RoundCap, Some(Verdict::Partial)),
            (GoalStop::RoundCap, Some(Verdict::Fail)),
            (GoalStop::Oscillation, Some(Verdict::Fail)),
            (GoalStop::Cancelled, None),
            (GoalStop::Error, None),
            (GoalStop::GateUnavailable, None),
        ] {
            let outcome = outcome(stop, verdict);
            let report = goal_report(&outcome);
            assert_eq!(goal_exit_code(&report), outcome.exit_code(), "{report}");
        }
    }

    #[test]
    fn a_line_that_names_no_verdict_is_not_a_pass() {
        assert_eq!(goal_exit_code(""), 3);
        assert_eq!(goal_exit_code("goal: cancelled · 2 rounds"), 3);
        // A goal whose text happens to contain the word.
        assert_eq!(goal_exit_code("goal: error · 1 round · make pass work"), 3);
    }

    struct AlwaysFails;

    #[async_trait]
    impl Reviewer for AlwaysFails {
        async fn review(&self, _request: ReviewRequest) -> Result<Review, SmolStr> {
            Ok(Review {
                verdict: Verdict::Fail,
                notes: "still not there".into(),
            })
        }
    }

    /// Hands out scripted verdicts, then stays on the last one.
    struct ScriptedGates {
        verdicts: Mutex<std::collections::VecDeque<GateVerdict>>,
        last: GateVerdict,
    }

    #[async_trait]
    impl Gates for ScriptedGates {
        async fn check(&self, _patch: &Patch) -> GateVerdict {
            let mut verdicts = match self.verdicts.lock() {
                Ok(verdicts) => verdicts,
                Err(poisoned) => poisoned.into_inner(),
            };
            verdicts.pop_front().unwrap_or_else(|| self.last.clone())
        }
    }

    fn notes(requests: &[CodeRequest]) -> Vec<Option<String>> {
        requests
            .iter()
            .map(|request| {
                request
                    .feedback
                    .as_ref()
                    .map(|review| review.notes.to_string())
            })
            .collect()
    }

    #[tokio::test]
    async fn a_stuck_goal_changes_one_variable_per_round_and_a_different_one_each_time() {
        let coder = RecordingCoder::new();
        let outcome = GoalLoop::new(Arc::clone(&coder) as Arc<dyn Coder>, Arc::new(AlwaysFails))
            .with_max_rounds(5)
            .run("ship it")
            .await;

        assert_eq!(outcome.stop, GoalStop::RoundCap);
        assert_eq!(
            outcome.changes,
            vec![
                RoundChange {
                    round: 3,
                    change: StrategyChange::Strategy,
                },
                RoundChange {
                    round: 4,
                    change: StrategyChange::FreshContext,
                },
                RoundChange {
                    round: 5,
                    change: StrategyChange::Effort,
                },
            ]
        );

        let seen = notes(&coder.requests());
        // One failure is ordinary: round 2 answers it with the review alone.
        assert_eq!(seen[0], None);
        assert_eq!(seen[1].as_deref(), Some("still not there"));
        let stuck = seen[2].clone().unwrap_or_default();
        assert!(stuck.contains("still not there"), "{stuck}");
        assert!(
            stuck.contains(StrategyChange::Strategy.instruction()),
            "{stuck}"
        );
        // Fresh context is the absence of the failure text, not a line about it.
        assert_eq!(seen[3], None);
        let effort = seen[4].clone().unwrap_or_default();
        assert!(
            effort.contains(StrategyChange::Effort.instruction()),
            "{effort}"
        );
        // One variable per round: the previous round's hint is not carried on.
        assert!(
            !effort.contains(StrategyChange::Strategy.instruction()),
            "{effort}"
        );
    }

    #[tokio::test]
    async fn the_round_log_names_the_round_and_the_variable_it_changed() {
        let coder = RecordingCoder::new();
        let outcome = GoalLoop::new(Arc::clone(&coder) as Arc<dyn Coder>, Arc::new(AlwaysFails))
            .with_max_rounds(4)
            .run("ship it")
            .await;

        let report = goal_report(&outcome);
        assert!(
            report.contains("changed: round 3 strategy hint, round 4 fresh context"),
            "{report}"
        );
    }

    #[tokio::test]
    async fn a_goal_that_is_not_stuck_changes_nothing_and_logs_nothing() {
        struct PassingReviewer;

        #[async_trait]
        impl Reviewer for PassingReviewer {
            async fn review(&self, _request: ReviewRequest) -> Result<Review, SmolStr> {
                Ok(Review {
                    verdict: Verdict::Pass,
                    notes: "good".into(),
                })
            }
        }

        let coder = RecordingCoder::new();
        let outcome = GoalLoop::new(
            Arc::clone(&coder) as Arc<dyn Coder>,
            Arc::new(PassingReviewer),
        )
        .run("ship it")
        .await;

        assert_eq!(outcome.stop, GoalStop::Passed);
        assert!(outcome.changes.is_empty());
        let report = goal_report(&outcome);
        assert!(!report.contains("changed"), "{report}");
    }

    /// A red check turning green is progress, so the goal is no longer stuck:
    /// the next failure starts a new streak instead of burning a variable.
    #[tokio::test]
    async fn a_check_that_goes_green_again_restarts_the_streak() {
        let coder = RecordingCoder::new();
        let gates = Arc::new(ScriptedGates {
            verdicts: std::collections::VecDeque::from(vec![
                GateVerdict::Red {
                    report: "check failed".into(),
                },
                GateVerdict::Red {
                    report: "check failed".into(),
                },
            ])
            .into(),
            last: GateVerdict::Green,
        });
        let outcome = GoalLoop::new(Arc::clone(&coder) as Arc<dyn Coder>, Arc::new(AlwaysFails))
            .with_gates(gates)
            .with_max_rounds(4)
            .run("ship it")
            .await;

        // Rounds 1 and 2 are red, so round 3 is stuck and changes a variable.
        // Round 3's check is green — progress — so round 4 is back to plain
        // repair of the one review that failed it.
        assert_eq!(
            outcome.changes,
            vec![RoundChange {
                round: 3,
                change: StrategyChange::Strategy,
            }]
        );
        let requests = coder.requests();
        let stuck = requests[2].gate.clone().unwrap_or_default();
        assert!(stuck.contains("check failed"), "{stuck}");
        assert!(
            stuck.contains(StrategyChange::Strategy.instruction()),
            "{stuck}"
        );
        assert_eq!(outcome.stop, GoalStop::RoundCap);
    }

    #[test]
    fn the_instruction_rides_on_whichever_input_the_round_carries() {
        let mut feedback = None;
        let mut gate = Some(SmolStr::new("check failed"));
        assert!(StrategyChange::Effort.apply(&mut feedback, &mut gate));
        let report = gate.clone().unwrap_or_default();
        assert!(report.starts_with("check failed"), "{report}");
        assert!(
            report.ends_with(StrategyChange::Effort.instruction()),
            "{report}"
        );
        assert!(feedback.is_none());

        let mut feedback = Some(Review {
            verdict: Verdict::Fail,
            notes: "wrong layer".into(),
        });
        let mut gate = None;
        assert!(StrategyChange::Strategy.apply(&mut feedback, &mut gate));
        let notes = feedback
            .map(|review| review.notes.to_string())
            .unwrap_or_default();
        assert!(notes.starts_with("wrong layer"), "{notes}");
        assert!(
            notes.ends_with(StrategyChange::Strategy.instruction()),
            "{notes}"
        );
        assert!(gate.is_none());

        // A round that already carries nothing is the fresh one: no change.
        let mut feedback = None;
        let mut gate = None;
        assert!(!StrategyChange::FreshContext.apply(&mut feedback, &mut gate));
    }

    /// The coder is a model: the changed variable has to reach its prompt,
    /// not only the request struct.
    #[tokio::test]
    async fn the_runner_coder_puts_the_changed_variable_in_the_prompt() {
        struct CapturingRunner {
            prompts: Mutex<Vec<SmolStr>>,
        }

        #[async_trait]
        impl crate::AgentRunner for CapturingRunner {
            async fn run(
                &self,
                request: crate::AgentRequest,
                _context: crate::AgentContext,
            ) -> Result<SmolStr, SmolStr> {
                match self.prompts.lock() {
                    Ok(mut prompts) => prompts.push(request.task),
                    Err(poisoned) => poisoned.into_inner().push(request.task),
                }
                Ok("diff".into())
            }
        }

        let mut feedback = Some(Review {
            verdict: Verdict::Fail,
            notes: "still not there".into(),
        });
        let mut gate = None;
        assert!(StrategyChange::Strategy.apply(&mut feedback, &mut gate));

        let runner = Arc::new(CapturingRunner {
            prompts: Mutex::new(Vec::new()),
        });
        let coder = RunnerCoder::new(Arc::clone(&runner) as Arc<dyn crate::AgentRunner>);
        let patch = coder
            .code(CodeRequest {
                goal: "ship it".into(),
                round: 3,
                feedback,
                gate,
            })
            .await;

        assert_eq!(patch, Ok(Patch::new("diff")));
        let prompts = match runner.prompts.lock() {
            Ok(prompts) => prompts.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let prompt = prompts.first().map(SmolStr::to_string).unwrap_or_default();
        assert!(
            prompt.contains(StrategyChange::Strategy.instruction()),
            "{prompt}"
        );
    }

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
