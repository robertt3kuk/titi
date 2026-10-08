//! Wire-format round trips for the headless JSONL protocol.
//!
//! Every `EngineCommand` and `EngineEvent` variant is built once, serialized
//! and compared to a hand-written JSON fixture, then decoded back from that
//! same fixture and compared to the value. The fixtures are literals, not
//! produced from the type: a renamed field, a dropped optional field or a
//! reordered one is a failure here rather than a silent pass, because the
//! encoded string is compared byte for byte.
//!
//! Both enums are `#[non_exhaustive]`, so a client that matches exhaustively
//! keeps compiling when a variant is added instead of breaking; this file is
//! what makes adding one deliberate.

use titi_engine::protocol::{JobInfo, SessionMode};
use titi_engine::{AgentKind, AgentStatus, ContextPart, EngineCommand, EngineEvent, TurnId};
use titi_providers::{ChatMessage, ErrorReason, Role, StopReason, ToolCallRef};

/// The variant encodes to exactly `json`, and `json` decodes to exactly it.
fn command(value: EngineCommand, json: &str) {
    let encoded = serde_json::to_string(&value).expect("serialize");
    assert_eq!(encoded, json, "the serialized EngineCommand changed");
    let decoded: EngineCommand = serde_json::from_str(json).expect("deserialize");
    assert_eq!(decoded, value, "the decoded EngineCommand differs");
}

fn event(value: EngineEvent, json: &str) {
    let encoded = serde_json::to_string(&value).expect("serialize");
    assert_eq!(encoded, json, "the serialized EngineEvent changed");
    let decoded: EngineEvent = serde_json::from_str(json).expect("deserialize");
    assert_eq!(decoded, value, "the decoded EngineEvent differs");
}

fn job() -> JobInfo {
    JobInfo {
        id: "job-1".into(),
        prompt: "check the build".into(),
        interval_secs: 300,
        runs: 2,
    }
}

#[test]
fn every_engine_command_round_trips_through_its_fixture() {
    command(
        EngineCommand::SubmitPrompt { text: "hi".into() },
        r#"{"SubmitPrompt":{"text":"hi"}}"#,
    );
    command(
        EngineCommand::FollowUp { text: "more".into() },
        r#"{"FollowUp":{"text":"more"}}"#,
    );
    command(
        EngineCommand::Steer { text: "left".into() },
        r#"{"Steer":{"text":"left"}}"#,
    );
    command(
        EngineCommand::RestoreHistory {
            messages: vec![ChatMessage {
                role: Role::User,
                content: "earlier".into(),
                tool_calls: vec![ToolCallRef {
                    call_id: "call-1".into(),
                    name: "read".into(),
                }],
            }],
        },
        r#"{"RestoreHistory":{"messages":[{"role":"user","content":"earlier","tool_calls":[{"call_id":"call-1","name":"read"}]}]}}"#,
    );
    command(EngineCommand::Cancel, r#""Cancel""#);
    command(
        EngineCommand::SwitchModel { model: "m/one".into() },
        r#"{"SwitchModel":{"model":"m/one"}}"#,
    );
    command(
        EngineCommand::ApproveTool {
            call_id: "call-1".into(),
            approved: true,
        },
        r#"{"ApproveTool":{"call_id":"call-1","approved":true}}"#,
    );
    command(
        EngineCommand::SpawnAgent {
            name: "scout".into(),
            task: "map the tree".into(),
            kind: AgentKind::Subagent,
        },
        r#"{"SpawnAgent":{"name":"scout","task":"map the tree","kind":"subagent"}}"#,
    );
    command(
        EngineCommand::FocusAgent { agent_id: "a1".into() },
        r#"{"FocusAgent":{"agent_id":"a1"}}"#,
    );
    command(
        EngineCommand::ReviveAgent { agent_id: "a1".into() },
        r#"{"ReviveAgent":{"agent_id":"a1"}}"#,
    );
    command(
        EngineCommand::StopAgent { agent_id: "a1".into() },
        r#"{"StopAgent":{"agent_id":"a1"}}"#,
    );
    command(
        EngineCommand::RunGoal { text: "fix it".into() },
        r#"{"RunGoal":{"text":"fix it"}}"#,
    );
    command(
        EngineCommand::RunCouncil { question: "why?".into() },
        r#"{"RunCouncil":{"question":"why?"}}"#,
    );
    command(
        EngineCommand::RunGraph { task: "ship it".into() },
        r#"{"RunGraph":{"task":"ship it"}}"#,
    );
    command(EngineCommand::DescribeContext, r#""DescribeContext""#);
    command(
        EngineCommand::Compact {
            focus: Some("keep tests".into()),
        },
        r#"{"Compact":{"focus":"keep tests"}}"#,
    );
    command(EngineCommand::MemoryList, r#""MemoryList""#);
    command(
        EngineCommand::MemorySearch { query: "tabs".into() },
        r#"{"MemorySearch":{"query":"tabs"}}"#,
    );
    command(
        EngineCommand::MemoryForget { id: -3 },
        r#"{"MemoryForget":{"id":-3}}"#,
    );
    command(
        EngineCommand::StartLoop {
            interval_secs: 300,
            prompt: "check".into(),
        },
        r#"{"StartLoop":{"interval_secs":300,"prompt":"check"}}"#,
    );
    command(EngineCommand::ListJobs, r#""ListJobs""#);
    command(
        EngineCommand::CancelJob { job_id: "job-1".into() },
        r#"{"CancelJob":{"job_id":"job-1"}}"#,
    );
    command(
        EngineCommand::Consult {
            question: Some("second opinion".into()),
        },
        r#"{"Consult":{"question":"second opinion"}}"#,
    );
    command(
        EngineCommand::Consult { question: None },
        r#"{"Consult":{"question":null}}"#,
    );
    command(
        EngineCommand::SetBudget { tokens: Some(10) },
        r#"{"SetBudget":{"tokens":10}}"#,
    );
    command(
        EngineCommand::SetMode { mode: SessionMode::Plan },
        r#"{"SetMode":{"mode":"plan"}}"#,
    );
    command(EngineCommand::Shutdown, r#""Shutdown""#);
}

#[test]
fn every_engine_event_round_trips_through_its_fixture() {
    event(
        EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "m/one".into(),
        },
        r#"{"TurnStarted":{"turn_id":1,"model":"m/one"}}"#,
    );
    event(
        EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "answer".into(),
        },
        r#"{"StreamDelta":{"turn_id":1,"text":"answer"}}"#,
    );
    event(
        EngineEvent::ThinkingDelta {
            turn_id: TurnId(1),
            text: "weighing".into(),
        },
        r#"{"ThinkingDelta":{"turn_id":1,"text":"weighing"}}"#,
    );
    // `detail` is optional: its absence must stay absent, not encode as null.
    event(
        EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "read".into(),
            detail: None,
        },
        r#"{"ToolStarted":{"turn_id":1,"call_id":"call-1","name":"read"}}"#,
    );
    event(
        EngineEvent::ToolApprovalNeeded {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            name: "write".into(),
        },
        r#"{"ToolApprovalNeeded":{"turn_id":1,"call_id":"call-1","name":"write"}}"#,
    );
    event(
        EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "call-1".into(),
            output: "ok".into(),
            is_error: true,
            detail: Some("the diff".into()),
        },
        r#"{"ToolFinished":{"turn_id":1,"call_id":"call-1","output":"ok","is_error":true,"detail":"the diff"}}"#,
    );
    event(
        EngineEvent::AgentStarted {
            agent_id: "a1".into(),
            name: "scout".into(),
            parent_id: Some("boss".into()),
            kind: AgentKind::Advisor,
        },
        r#"{"AgentStarted":{"agent_id":"a1","name":"scout","parent_id":"boss","kind":"advisor"}}"#,
    );
    event(
        EngineEvent::AgentProgress {
            agent_id: "a1".into(),
            text: "reading".into(),
        },
        r#"{"AgentProgress":{"agent_id":"a1","text":"reading"}}"#,
    );
    event(
        EngineEvent::AgentStatusChanged {
            agent_id: "a1".into(),
            status: AgentStatus::Parked,
        },
        r#"{"AgentStatusChanged":{"agent_id":"a1","status":"parked"}}"#,
    );
    event(
        EngineEvent::AgentFinished {
            agent_id: "a1".into(),
            summary: "done".into(),
            success: false,
        },
        r#"{"AgentFinished":{"agent_id":"a1","summary":"done","success":false}}"#,
    );
    event(
        EngineEvent::AgentFocused { agent_id: None },
        r#"{"AgentFocused":{"agent_id":null}}"#,
    );
    event(
        EngineEvent::ModelSwitched {
            turn_id: Some(TurnId(2)),
            from: "a".into(),
            to: "b".into(),
        },
        r#"{"ModelSwitched":{"turn_id":2,"from":"a","to":"b"}}"#,
    );
    event(
        EngineEvent::ContextUsage {
            turn_id: TurnId(1),
            tokens: 10,
            window: 1000,
        },
        r#"{"ContextUsage":{"turn_id":1,"tokens":10,"window":1000}}"#,
    );
    event(
        EngineEvent::Compacted {
            turn_id: TurnId(1),
            folded: 2,
            tokens_before: 30,
            strategy: "digest".into(),
        },
        r#"{"Compacted":{"turn_id":1,"folded":2,"tokens_before":30,"strategy":"digest"}}"#,
    );
    event(
        EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 11,
            completion_tokens: 22,
            cached_tokens: 3,
        },
        r#"{"TurnUsage":{"turn_id":1,"prompt_tokens":11,"completion_tokens":22,"cached_tokens":3}}"#,
    );
    event(
        EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::ToolUse,
        },
        r#"{"TurnFinished":{"turn_id":1,"reason":"tool_use"}}"#,
    );
    event(
        EngineEvent::Failed {
            turn_id: Some(TurnId(1)),
            reason: ErrorReason::Connection,
            message: "boom".into(),
        },
        r#"{"Failed":{"turn_id":1,"reason":"connection","message":"boom"}}"#,
    );
    event(
        EngineEvent::Cancelled { turn_id: TurnId(1) },
        r#"{"Cancelled":{"turn_id":1}}"#,
    );
    event(
        EngineEvent::PromptReturned { text: "queued".into() },
        r#"{"PromptReturned":{"text":"queued"}}"#,
    );
    event(
        EngineEvent::GoalFinished { report: "pass".into() },
        r#"{"GoalFinished":{"report":"pass"}}"#,
    );
    event(
        EngineEvent::CouncilFinished { report: "verdict".into() },
        r#"{"CouncilFinished":{"report":"verdict"}}"#,
    );
    event(
        EngineEvent::GraphFinished { report: "shipped".into() },
        r#"{"GraphFinished":{"report":"shipped"}}"#,
    );
    event(
        EngineEvent::Notice { message: "refused".into() },
        r#"{"Notice":{"message":"refused"}}"#,
    );
    event(
        EngineEvent::ContextBreakdown {
            parts: vec![ContextPart {
                label: "tools".into(),
                tokens: 12,
            }],
            window: 1000,
        },
        r#"{"ContextBreakdown":{"parts":[{"label":"tools","tokens":12}],"window":1000}}"#,
    );
    event(
        EngineEvent::SessionNamed {
            session_id: "s1".into(),
            title: "the fix".into(),
        },
        r#"{"SessionNamed":{"session_id":"s1","title":"the fix"}}"#,
    );
    event(
        EngineEvent::MemoryResult { output: "found".into() },
        r#"{"MemoryResult":{"output":"found"}}"#,
    );
    event(
        EngineEvent::JobStarted { job: job() },
        r#"{"JobStarted":{"job":{"id":"job-1","prompt":"check the build","interval_secs":300,"runs":2}}}"#,
    );
    event(
        EngineEvent::JobList { jobs: vec![job()] },
        r#"{"JobList":{"jobs":[{"id":"job-1","prompt":"check the build","interval_secs":300,"runs":2}]}}"#,
    );
    event(
        EngineEvent::JobFinished { job_id: "job-1".into() },
        r#"{"JobFinished":{"job_id":"job-1"}}"#,
    );
    event(
        EngineEvent::AdvisorAnswer { text: "advice".into() },
        r#"{"AdvisorAnswer":{"text":"advice"}}"#,
    );
    event(
        EngineEvent::AdvisorFailed { reason: "no model".into() },
        r#"{"AdvisorFailed":{"reason":"no model"}}"#,
    );
    event(
        EngineEvent::BudgetUpdated {
            spent: 5,
            limit: Some(10),
        },
        r#"{"BudgetUpdated":{"spent":5,"limit":10}}"#,
    );
    event(
        EngineEvent::BudgetExceeded { spent: 10, limit: 10 },
        r#"{"BudgetExceeded":{"spent":10,"limit":10}}"#,
    );
    event(
        EngineEvent::ModeChanged {
            mode: SessionMode::Duck,
        },
        r#"{"ModeChanged":{"mode":"duck"}}"#,
    );
}
