#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A response's read calls run together; everything else keeps the model's
//! order, and a write is a barrier between the reads around it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, ToolSpec, Transport,
};
use titi_tools::{
    ApprovalMode, ApprovalTier, ToolDefinition, ToolHandler, ToolRegistry, ToolResult,
};

struct MapResolver(Arc<dyn Transport>);

impl TransportResolver for MapResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        Ok(ResolvedModel::without_credential(
            model,
            Arc::clone(&self.0),
        ))
    }
}

fn resolver(transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(transport))
}

/// Two tool calls in one response, ids `call-1` and `call-2`.
fn two_calls(first: (&str, &str), second: (&str, &str)) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for (index, (name, args)) in [first, second].into_iter().enumerate() {
        let id = format!("call-{}", index + 1);
        events.push(StreamEvent::ToolcallStart {
            id: BlockId::new(id.clone()),
            call: ToolCallRef {
                call_id: id.clone().into(),
                name: name.into(),
                ..Default::default()
            },
        });
        events.push(StreamEvent::ToolcallDelta {
            id: BlockId::new(id.clone()),
            json: args.into(),
        });
        events.push(StreamEvent::ToolcallEnd {
            id: BlockId::new(id),
        });
    }
    events.push(StreamEvent::Done {
        reason: StopReason::ToolUse,
    });
    events
}

fn text(body: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: body.into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ]
}

async fn collect_until_terminal(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let terminal = matches!(
            event,
            EngineEvent::TurnFinished { .. }
                | EngineEvent::Failed { .. }
                | EngineEvent::Cancelled { .. }
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

/// A tool that records when it starts and ends, and can be made to wait for
/// its sibling to arrive before it finishes.
struct SlowTool {
    name: &'static str,
    tier: ApprovalTier,
    /// When set, `invoke` does not return until both calls of a group are in
    /// it — which is what makes a sequential group deadlock.
    gate: Option<Arc<tokio::sync::Barrier>>,
    delay: Duration,
    timeline: Arc<Mutex<Vec<String>>>,
}

impl SlowTool {
    fn read(name: &'static str, timeline: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            tier: ApprovalTier::Read,
            gate: None,
            delay: Duration::ZERO,
            timeline: Arc::clone(timeline),
        }
    }

    fn write(name: &'static str, timeline: &Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            tier: ApprovalTier::Write,
            gate: None,
            delay: Duration::ZERO,
            timeline: Arc::clone(timeline),
        }
    }

    fn waiting(mut self, gate: &Arc<tokio::sync::Barrier>) -> Self {
        self.gate = Some(Arc::clone(gate));
        self
    }

    fn taking(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait]
impl ToolHandler for SlowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: self.name.into(),
                description: "a slow call".into(),
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": true}),
            },
            approval: self.tier,
        }
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        self.timeline
            .lock()
            .unwrap()
            .push(format!("{} start", self.name));
        if let Some(gate) = &self.gate {
            gate.wait().await;
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.timeline
            .lock()
            .unwrap()
            .push(format!("{} end", self.name));
        ToolResult {
            output: format!("{} done", self.name).into(),
            is_error: false,
            detail: None,
        }
    }
}

fn registry(tools: Vec<SlowTool>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(Arc::new(tool));
    }
    registry
}

/// Two read calls of one response meet at a barrier of two. Sequential calls
/// would deadlock there, so the timeout is the proof.
#[tokio::test]
async fn two_reads_of_one_response_run_at_once() {
    let timeline = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(two_calls(("slow_a", "{}"), ("slow_b", "{}"))),
        MockBody::Events(text("both done")),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        registry(vec![
            SlowTool::read("slow_a", &timeline).waiting(&gate),
            SlowTool::read("slow_b", &timeline).waiting(&gate),
        ]),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), collect_until_terminal(&mut engine))
        .await
        .expect("both reads finished; one at a time they would still be at the barrier");

    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolFinished {
                is_error: false,
                ..
            }
        )),
        "the calls answered: {events:?}"
    );
    // Both starts precede both ends; which end logs first is the barrier's
    // business, not the scheduler's, so it is not pinned.
    let timeline = timeline.lock().unwrap().clone();
    assert_eq!(
        timeline.get(..2),
        Some(["slow_a start".to_owned(), "slow_b start".to_owned()].as_slice()),
        "both calls started before either finished: {timeline:?}"
    );
    for name in ["slow_a end", "slow_b end"] {
        assert!(
            timeline.iter().skip(2).any(|line| line == name),
            "{name} is missing from {timeline:?}"
        );
    }
}

/// The model's order is what comes back, whatever order the calls finish in:
/// the first call is the slow one and still answers first.
#[tokio::test]
async fn results_come_back_in_the_calls_own_order() {
    let timeline = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(two_calls(("slow_a", "{}"), ("slow_b", "{}"))),
        MockBody::Events(text("done")),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        registry(vec![
            SlowTool::read("slow_a", &timeline).taking(Duration::from_millis(80)),
            SlowTool::read("slow_b", &timeline).taking(Duration::from_millis(5)),
        ]),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let finished: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ToolFinished {
                call_id, output, ..
            } => Some(format!("{call_id}:{output}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![
            "call-1:slow_a done".to_owned(),
            "call-2:slow_b done".to_owned()
        ],
        "the answers are handed back in the order the calls were made"
    );
    let timeline = timeline.lock().unwrap().clone();
    assert!(
        timeline.iter().position(|entry| entry == "slow_b end")
            < timeline.iter().position(|entry| entry == "slow_a end"),
        "the second call really did finish first: {timeline:?}"
    );
}

/// A write-tier call is a barrier: the reads before it finish before it starts,
/// and the reads after it wait for it.
#[tokio::test]
async fn a_write_between_reads_is_a_barrier() {
    let timeline = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(two_calls(("read_a", "{}"), ("write_w", "{}"))),
        MockBody::Events(two_calls(("read_b", "{}"), ("read_c", "{}"))),
        MockBody::Events(text("done")),
    ]));
    let mut config = EngineConfig::new("primary");
    // The barrier this test is about is the tier, not the approval prompt.
    config.approval_mode = ApprovalMode::Yolo;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(Arc::clone(&transport) as _),
        registry(vec![
            // The read before the write takes long enough that a write which
            // ran beside it would log its start inside that window.
            SlowTool::read("read_a", &timeline).taking(Duration::from_millis(80)),
            SlowTool::write("write_w", &timeline),
            SlowTool::read("read_b", &timeline),
            SlowTool::read("read_c", &timeline),
        ]),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnFinished { .. })),
        "the turn finished: {events:?}"
    );

    let timeline = timeline.lock().unwrap().clone();
    let at = |entry: &str| {
        timeline
            .iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry:?} not in {timeline:?}"))
    };
    assert!(at("read_a end") < at("write_w start"), "{timeline:?}");
    assert!(at("write_w end") < at("read_b start"), "{timeline:?}");
    assert!(at("write_w end") < at("read_c start"), "{timeline:?}");
}

/// A cancel while a group is in flight settles the turn: the reads are allowed
/// to finish, but nothing waits on them.
#[tokio::test]
async fn a_cancel_mid_group_settles_the_turn() {
    struct HangingRead;

    #[async_trait]
    impl ToolHandler for HangingRead {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                spec: ToolSpec {
                    name: "hang".into(),
                    description: "a read that never returns".into(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                approval: ApprovalTier::Read,
            }
        }

        async fn invoke(&self, _args: Value) -> ToolResult {
            futures::future::pending::<()>().await;
            ToolResult {
                output: "unreachable".into(),
                is_error: false,
                detail: None,
            }
        }
    }

    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(two_calls(
        ("hang", "{}"),
        ("hang", "{}"),
    ))]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingRead));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        tools,
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();

    // Let both calls start, then cancel.
    let mut started = 0;
    while started < 2 {
        if let EngineEvent::ToolStarted { .. } = engine.recv().await.expect("an event") {
            started += 1;
        }
    }
    engine.send(EngineCommand::Cancel).await.unwrap();

    // `Cancelled` comes from the command, before the turn has done anything
    // about it, so keep reading for the turn's own answers: a model that asked
    // for two reads must not be left with a dangling tool call.
    let mut answered: Vec<String> = Vec::new();
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = engine.recv().await {
            if let EngineEvent::ToolFinished {
                output, is_error, ..
            } = &event
            {
                assert!(
                    *is_error,
                    "an unfinished call answers as an error: {output}"
                );
                answered.push(output.to_string());
                if answered.len() == 2 {
                    break;
                }
            }
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the turn settled and answered both calls instead of waiting on the reads: {answered:?}"
    );
    assert!(
        answered
            .iter()
            .all(|output| output.contains("cancelled before this call ran")),
        "both answered as cancelled: {answered:?}"
    );
}

/// Every call of a group still gets its own start and finish, in the order the
/// model asked for them.
#[tokio::test]
async fn events_stay_paired_per_call() {
    let timeline = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(two_calls(("slow_a", "{}"), ("slow_b", "{}"))),
        MockBody::Events(text("done")),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(Arc::clone(&transport) as _),
        registry(vec![
            SlowTool::read("slow_a", &timeline).taking(Duration::from_millis(20)),
            SlowTool::read("slow_b", &timeline),
        ]),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;

    let ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ToolStarted { call_id, .. } => Some(format!("start:{call_id}")),
            EngineEvent::ToolFinished { call_id, .. } => Some(format!("finish:{call_id}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            "start:call-1".to_owned(),
            "start:call-2".to_owned(),
            "finish:call-1".to_owned(),
            "finish:call-2".to_owned(),
        ],
        "a surface sees both starts in order, then both finishes in order"
    );
}
