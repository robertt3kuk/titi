use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_providers::{ChatMessage, ErrorReason, StopReason};
// The answer a surface sends back is the same value the `ask` tool renders,
// so one vocabulary covers both halves: what the tool waits for and what the
// wire carries are not two enums that could drift apart.
use titi_tools::AskAnswer;

/// Stable identifier correlating commands and events for one agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Subagent,
    Advisor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Idle,
    Parked,
    Aborted,
    Completed,
    Failed,
}

/// What a session lets the agent reach for.
///
/// The mode picks the tools a turn is given, so a mode is not advice the
/// model may ignore: in plan mode nothing that writes is registered, and a
/// call it cannot make is a call it cannot make by mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Everything the surface registered: read, write, exec.
    #[default]
    Agent,
    /// Read-only tools. The turn answers with a plan, not with a change.
    Plan,
    /// A repo-blind chat partner: no tool that can reach the filesystem or
    /// a shell is registered at all, and no repository map is sent.
    Duck,
}

impl SessionMode {
    /// The word the status bar shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Plan => "plan",
            Self::Duck => "duck",
        }
    }
}

/// Commands accepted by every engine surface.
///
/// `#[non_exhaustive]`: a surface matching exhaustively must handle an
/// unknown variant instead of failing to compile when one is added. The
/// wire vocabulary is pinned by `tests/protocol.rs`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineCommand {
    SubmitPrompt {
        text: SmolStr,
    },
    FollowUp {
        text: SmolStr,
    },
    /// Redirect the running turn at its next step boundary, without aborting.
    Steer {
        text: SmolStr,
    },
    /// Replace the replayed conversation for every following turn. A surface
    /// sends this after rewinding a session, so the model stops seeing the
    /// history the user just cut away.
    RestoreHistory {
        messages: Vec<ChatMessage>,
    },
    Cancel,
    SwitchModel {
        model: SmolStr,
    },
    ApproveTool {
        call_id: SmolStr,
        approved: bool,
    },
    SpawnAgent {
        name: SmolStr,
        task: SmolStr,
        kind: AgentKind,
    },
    FocusAgent {
        agent_id: SmolStr,
    },
    ReviveAgent {
        agent_id: SmolStr,
    },
    StopAgent {
        agent_id: SmolStr,
    },
    /// Run the coder/reviewer goal loop. This is not a chat turn.
    RunGoal {
        text: SmolStr,
    },
    /// Put a question to a council of briefs. This is not a chat turn.
    RunCouncil {
        question: SmolStr,
    },
    /// Run the built-in orchestrator graph over a task: a council decides the
    /// approach, then the goal loop does the work. This is not a chat turn.
    RunGraph {
        task: SmolStr,
    },
    /// Report what currently fills the context window, part by part.
    DescribeContext,
    /// Fold the history now, whatever the threshold says. `focus` is free
    /// text that biases what the digest keeps.
    Compact {
        focus: Option<SmolStr>,
    },
    MemoryList,
    MemorySearch {
        query: SmolStr,
    },
    MemoryForget {
        id: i64,
    },
    /// Repeat `prompt` as a background turn every `interval_secs`.
    ///
    /// The engine owns the timer: a surface that dies does not take the loop
    /// with it, and a loop turn queues behind the live one like any other
    /// prompt instead of interrupting it.
    StartLoop {
        interval_secs: u64,
        prompt: SmolStr,
    },
    /// Report the background jobs running right now.
    ListJobs,
    /// Stop a background job by name: a loop's timer, or a handed-over
    /// command's process group. Cancelling the *turn* reaches neither — that
    /// is what handing a command over means.
    CancelJob {
        job_id: SmolStr,
    },
    /// Ask a toolless advisor what it thinks of the conversation so far.
    ///
    /// `question` is what the user typed after the command; without one the
    /// advisor is asked about the conversation as a whole.
    Consult {
        question: Option<SmolStr>,
    },
    /// Answer the question the model asked — see [`EngineEvent::AskRequested`].
    ///
    /// An answer to a question nobody is waiting for is dropped rather than
    /// refused: the turn it belonged to was cancelled or interrupted, and
    /// there is no longer anything to unblock. A surface that answers late
    /// loses the answer, not the session.
    AnswerAsk {
        request_id: SmolStr,
        answer: AskAnswer,
    },
    /// Cap the tokens this session may spend. `None` lifts the cap.
    ///
    /// Reaching the cap stops the engine starting turns: a budget that only
    /// warned would be a budget that was already spent.
    SetBudget {
        tokens: Option<u64>,
    },
    /// Cap the money this session may spend, in micro-dollars (millionths of
    /// a US dollar). `None` lifts the cap.
    ///
    /// A sibling of [`Self::SetBudget`] rather than a field beside its
    /// `tokens`, because the two bounds are independent: setting one does not
    /// restate the other, and clearing one leaves the other standing. The
    /// engine refuses a money cap over a model it cannot price — see
    /// [`EngineEvent::MoneyBudgetUnpriced`] — instead of accepting a bound it
    /// would then be unable to enforce.
    SetMoneyBudget {
        micro_usd: Option<u64>,
    },
    /// Switch what the next turns are allowed to do.
    SetMode {
        mode: SessionMode,
    },
    Shutdown,
}

/// UI-independent events rendered by TUI, GPUI, or serialized by headless RPC.
///
/// `#[non_exhaustive]`: a surface matching exhaustively must handle an
/// unknown variant instead of failing to compile when one is added. The
/// wire vocabulary is pinned by `tests/protocol.rs`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineEvent {
    TurnStarted {
        turn_id: TurnId,
        model: SmolStr,
    },
    StreamDelta {
        turn_id: TurnId,
        text: SmolStr,
    },
    ThinkingDelta {
        turn_id: TurnId,
        text: SmolStr,
    },
    ToolStarted {
        turn_id: TurnId,
        call_id: SmolStr,
        name: SmolStr,
        /// What the tool says it is about to do — `read docs/README.md` —
        /// for a surface to show beside the name. Never sent to the provider:
        /// the call's own arguments are already in the assistant message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<SmolStr>,
    },
    ToolApprovalNeeded {
        turn_id: TurnId,
        call_id: SmolStr,
        name: SmolStr,
    },
    ToolFinished {
        turn_id: TurnId,
        call_id: SmolStr,
        output: SmolStr,
        is_error: bool,
        /// What a surface may show besides `output` — a diff of what the tool
        /// changed, today. It never reaches the provider: the tool message
        /// carries `output` alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<SmolStr>,
    },
    AgentStarted {
        agent_id: SmolStr,
        name: SmolStr,
        parent_id: Option<SmolStr>,
        kind: AgentKind,
    },
    /// The text the agent's model is writing, streamed as it arrives.
    ///
    /// This is the answer being written. What the agent is *doing* - which
    /// tools it just called, which file it is reading - is
    /// [`Self::AgentActivity`], because a surface that wants to say "what is
    /// this agent up to" cannot get that out of the model's prose.
    AgentProgress {
        agent_id: SmolStr,
        text: SmolStr,
    },
    /// What the agent is doing rather than saying: `tools: read, grep`.
    ///
    /// The engine's own line, never the model's words, so a surface can show
    /// it as activity without parsing or trusting the stream. Split out of
    /// `AgentProgress`, which used to carry both and left a reader unable to
    /// tell a sentence from a status.
    AgentActivity {
        agent_id: SmolStr,
        text: SmolStr,
    },
    AgentStatusChanged {
        agent_id: SmolStr,
        status: AgentStatus,
    },
    AgentFinished {
        agent_id: SmolStr,
        summary: SmolStr,
        success: bool,
    },
    /// The view moved to this agent. `None` returns it to the main turn.
    ///
    /// Focus used to answer only "does this agent exist" and change nothing,
    /// so selecting one in the Hub had no visible effect.
    AgentFocused {
        agent_id: Option<SmolStr>,
    },
    /// The primary model changed, whether from a mid-turn fallback or a
    /// standalone `SwitchModel`.
    ///
    /// A standalone switch belongs to no turn, so `turn_id` is `None` there;
    /// a fallback happens inside a turn and names it. The field is optional
    /// rather than a separate variant so a surface that only needs the pair
    /// (`from`, `to`) handles both the same way.
    ModelSwitched {
        turn_id: Option<TurnId>,
        from: SmolStr,
        to: SmolStr,
    },
    /// How full the context window is after the request was assembled.
    ///
    /// `tokens` is an estimate of the request about to be sent, not a count
    /// the provider reported — providers do not all return usage.
    ContextUsage {
        turn_id: TurnId,
        tokens: u64,
        window: u64,
    },
    /// The request crossed the context threshold and the oldest messages were
    /// folded into one digest.
    Compacted {
        turn_id: TurnId,
        folded: u32,
        tokens_before: u64,
        strategy: SmolStr,
    },
    TurnUsage {
        turn_id: TurnId,
        prompt_tokens: u32,
        completion_tokens: u32,
        /// The part of `prompt_tokens` the provider served from its prompt
        /// cache. Zero when it reported none, or reported nothing at all.
        #[serde(default)]
        cached_tokens: u32,
        /// What those tokens cost, in micro-dollars, when the model the turn
        /// ran on states a price.
        ///
        /// The engine's own figure for the turn, from the same three counts
        /// above and the same [`crate::ModelPrice::cost_micro_usd`] a surface
        /// would use — so a footer and a budget can never disagree about what
        /// a turn cost. `None` is *unpriced* (not free), and also the answer
        /// for a turn whose rounds did not all have a price: a figure that
        /// covered only part of a turn would read as the whole of it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_micro_usd: Option<u64>,
    },
    TurnFinished {
        turn_id: TurnId,
        reason: StopReason,
    },
    Failed {
        turn_id: Option<TurnId>,
        reason: ErrorReason,
        message: SmolStr,
    },
    Cancelled {
        turn_id: TurnId,
    },
    /// A prompt that was waiting behind the cancelled turn and will not run.
    ///
    /// Cancel means stop, so the queue is not allowed to fire later and
    /// answer a question the user walked away from. The text goes back to
    /// the surface instead of being dropped, one event per queued prompt in
    /// the order they were typed.
    PromptReturned {
        text: SmolStr,
    },
    /// `/goal` finished. `report` is the line the surface shows.
    GoalFinished {
        report: SmolStr,
    },
    /// `/council` finished. `report` is the block the surface shows.
    CouncilFinished {
        report: SmolStr,
    },
    /// An orchestrator graph finished. `report` is the block the surface
    /// shows: one line per node entered, then the final verdict.
    GraphFinished {
        report: SmolStr,
    },
    /// Something the user asked for was not done, without failing the turn.
    ///
    /// A refused skill body is the first use: the turn still runs, but the
    /// reason the body is missing has to reach the surface, or the prompt
    /// quietly means something else than the user read on screen.
    Notice {
        message: SmolStr,
    },
    /// What fills the context window right now, part by part.
    ///
    /// The answer to [`EngineCommand::DescribeContext`]. There is no total
    /// field: the total is the sum of the parts, and two numbers that can
    /// disagree are worse than one the surface adds up itself.
    ContextBreakdown {
        parts: Vec<ContextPart>,
        window: u64,
    },
    /// A session that had no name of its own got one, generated after a
    /// finished turn. A name the user chose is never replaced, so this never
    /// overwrites what the user typed.
    SessionNamed {
        session_id: SmolStr,
        title: SmolStr,
    },
    MemoryResult {
        output: SmolStr,
    },
    /// A background loop started and is now the engine's to run.
    JobStarted {
        job: JobInfo,
    },
    /// Every background job alive when [`EngineCommand::ListJobs`] arrived.
    JobList {
        jobs: Vec<JobInfo>,
    },
    /// A background job stopped and will not fire again.
    JobFinished {
        job_id: SmolStr,
    },
    /// The advisor's second opinion. Never empty: an advisor with nothing
    /// to say is a failed consult, not a silent one.
    AdvisorAnswer {
        text: SmolStr,
    },
    /// The consult produced no opinion, and why.
    AdvisorFailed {
        reason: SmolStr,
    },
    /// What this session has spent, and against what cap.
    ///
    /// `spent` sums every request's input and output as the provider counted
    /// them; a request whose provider reported nothing is charged the
    /// engine's own estimate (~4 characters per token) instead.
    BudgetUpdated {
        spent: u64,
        limit: Option<u64>,
    },
    /// The cap was reached. No further turn starts until it is raised or
    /// lifted, and anything queued behind it was returned.
    BudgetExceeded {
        spent: u64,
        limit: u64,
    },
    /// What this session has spent in money, and against what money cap.
    ///
    /// Micro-dollars (millionths of a US dollar), the unit a price is stated
    /// in, so nothing here is a float that could drift. `spent_micro_usd` is
    /// what the engine could *measure*: a turn that ran on a model with no
    /// price adds nothing to it, and [`Self::MoneyBudgetUnpriced`] is what
    /// says so — the figure is a floor, never a total, once that has been
    /// reported.
    MoneyBudgetUpdated {
        spent_micro_usd: u64,
        limit_micro_usd: Option<u64>,
    },
    /// The money cap was reached. Same stop as [`Self::BudgetExceeded`]: no
    /// further turn starts until it is raised or lifted, and anything queued
    /// behind it was returned.
    MoneyBudgetExceeded {
        spent_micro_usd: u64,
        limit_micro_usd: u64,
    },
    /// A money cap is in force over a model the engine cannot price.
    ///
    /// An unpriced model is not a free one — a local server, a subscription
    /// backend, a price nobody wrote down — so the engine neither treats its
    /// turns as costing nothing nor pretends the cap it was given can be
    /// enforced against them. It says this instead, naming the model, once
    /// per cap: the bound stays in force, and its spend becomes a floor.
    MoneyBudgetUnpriced {
        model: SmolStr,
    },
    /// The model asked the user a question and is waiting for the answer.
    ///
    /// The turn is stopped until [`EngineCommand::AnswerAsk`] arrives or the
    /// turn is cancelled, which answers
    /// [`AskAnswer::Cancelled`] — there is no deadline, because a deadline
    /// would have to answer on the user's behalf.
    AskRequested {
        /// Correlates the answer with this question; a surface echoes it back.
        request_id: SmolStr,
        question: SmolStr,
        /// The choices, in the order to show them. Empty is a question with no
        /// list, which the user answers in their own words.
        options: Vec<SmolStr>,
        /// Whether more than one choice may be taken.
        multi: bool,
        /// Whether the user may answer in their own words besides the list.
        free_text: bool,
    },
    /// The mode the engine is now in. A surface badge follows this, never
    /// its own keypress: the mode that matters is the one the turns run in.
    ModeChanged {
        mode: SessionMode,
    },
}

/// One labelled slice of the request a turn would send, with the estimated
/// tokens it costs. The estimate is the project's own (~4 chars per token),
/// not a count any provider reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPart {
    pub label: SmolStr,
    pub tokens: u64,
}

/// One background job the engine holds: a loop it repeats on its own timer,
/// or a `bash` command a turn handed over and the engine is waiting on. A
/// command's `interval_secs` and `runs` are zero — it runs once, on no timer
/// of ours, and a surface reading them as a schedule would be reading them
/// wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: SmolStr,
    pub prompt: SmolStr,
    pub interval_secs: u64,
    /// Turns this job has already submitted.
    pub runs: u64,
}
