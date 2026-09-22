use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_providers::{ChatMessage, ErrorReason, StopReason};

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

/// Commands accepted by every engine surface.
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
    Shutdown,
}

/// UI-independent events rendered by TUI, GPUI, or serialized by headless RPC.
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
    },
    AgentStarted {
        agent_id: SmolStr,
        name: SmolStr,
        parent_id: Option<SmolStr>,
        kind: AgentKind,
    },
    AgentProgress {
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
    ModelSwitched {
        turn_id: TurnId,
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
    /// `/goal` finished. `report` is the line the surface shows.
    GoalFinished {
        report: SmolStr,
    },
}
