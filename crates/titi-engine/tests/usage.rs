//! A turn's usage is the provider's own count when it reports one, and the
//! project's estimate only for a round it reported nothing for.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, ErrorReason, MockBody, MockTransport, StopReason, StreamEvent, TokenUsage,
    ToolCallRef, Transport, TransportError,
};
use titi_tools::{EchoTool, ToolRegistry};

struct MapResolver(HashMap<String, Arc<dyn Transport>>);

impl TransportResolver for MapResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self.0
            .get(model)
            .cloned()
            .map(|transport| ResolvedModel::without_credential(model, transport))
            .ok_or_else(|| RegistryError::UnknownModel(model.into()))
    }
}

fn resolver(models: Vec<(&str, Arc<dyn Transport>)>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        models
            .into_iter()
            .map(|(name, transport)| (name.to_owned(), transport))
            .collect(),
    ))
}

fn echo_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools
}

fn reported(prompt: u64, completion: u64) -> StreamEvent {
    StreamEvent::Usage(TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_tokens: 0,
    })
}

fn text(text: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        id: BlockId::new("text"),
        text: text.into(),
    }
}

fn done(reason: StopReason) -> StreamEvent {
    StreamEvent::Done { reason }
}

/// A round that calls `echo`, without its terminal event.
fn echo_call() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: "echo".into(),
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: r#"{"text":"pong"}"#.into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
    ]
}

/// Everything the engine says up to the spend report that follows a turn.
async fn run_turn(engine: &mut titi_engine::Engine) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .expect("the engine takes the prompt");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = engine.recv().await {
            let last = matches!(
                event,
                EngineEvent::BudgetUpdated { .. } | EngineEvent::Failed { .. }
            );
            events.push(event);
            if last {
                return events;
            }
        }
        panic!("the engine stopped before the turn was accounted: {events:?}");
    })
    .await
    .expect("the turn never finished")
}

fn turn_usage(events: &[EngineEvent]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TurnUsage {
                prompt_tokens,
                completion_tokens,
                ..
            } => Some((*prompt_tokens, *completion_tokens)),
            _ => None,
        })
        .collect()
}

fn context_usage(events: &[EngineEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ContextUsage { tokens, .. } => Some(*tokens),
            _ => None,
        })
        .collect()
}

fn spent(events: &[EngineEvent]) -> u64 {
    events
        .iter()
        .find_map(|event| match event {
            EngineEvent::BudgetUpdated { spent, .. } => Some(*spent),
            _ => None,
        })
        .expect("the turn's spend is reported")
}

/// The provider's figures are what the turn reports and what the session's
/// budget is charged, not the estimate of the same request.
#[tokio::test]
async fn a_reported_count_is_the_turns_usage_and_its_spend() {
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        text("hi"),
        reported(5000, 42),
        done(StopReason::Stop),
    ])]));
    let mut engine = EngineRuntime::start(
        EngineConfig::new("primary"),
        resolver(vec![("primary", transport)]),
    );
    let events = run_turn(&mut engine).await;

    assert_eq!(turn_usage(&events), vec![(5000, 42)], "{events:?}");
    assert_eq!(spent(&events), 5042);
}

/// Counts are cumulative: a second report in one round replaces the first.
#[tokio::test]
async fn the_last_report_of_a_round_is_the_one_charged() {
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        reported(800, 1),
        text("hi"),
        reported(800, 30),
        done(StopReason::Stop),
    ])]));
    let mut engine = EngineRuntime::start(
        EngineConfig::new("primary"),
        resolver(vec![("primary", transport)]),
    );
    let events = run_turn(&mut engine).await;

    assert_eq!(turn_usage(&events), vec![(800, 30)], "{events:?}");
    assert_eq!(spent(&events), 830);
}

/// A tool round the provider counted and a final round it did not: each is
/// charged its own figure, and the turn reports the sum.
#[tokio::test]
async fn rounds_with_and_without_a_report_each_add_their_own_figure() {
    let mut tool_round = echo_call();
    tool_round.push(reported(1000, 20));
    tool_round.push(done(StopReason::ToolUse));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_round),
        MockBody::Events(vec![text("done"), done(StopReason::Stop)]),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(vec![("primary", transport)]),
        echo_registry(),
    );
    let events = run_turn(&mut engine).await;

    let rounds = context_usage(&events);
    assert_eq!(rounds.len(), 2, "{events:?}");
    let estimated_prompt = rounds[1];
    let estimated_completion = titi_core::compaction::estimate_tokens("done");
    let usage = turn_usage(&events);
    assert_eq!(
        usage,
        vec![(
            u32::try_from(1000 + estimated_prompt).expect("small"),
            u32::try_from(20 + estimated_completion).expect("small"),
        )],
        "{events:?}"
    );
    assert_eq!(
        spent(&events),
        1000 + 20 + estimated_prompt + estimated_completion
    );
}

/// An attempt that fails is retried and charges nothing, even when the
/// provider counted it before the connection broke.
#[tokio::test]
async fn a_failed_attempt_charges_nothing_even_with_a_report() {
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(vec![
            reported(777, 7),
            StreamEvent::Error {
                reason: ErrorReason::Connection,
                message: "connection reset".into(),
            },
        ]),
        MockBody::Events(vec![text("ok"), reported(300, 3), done(StopReason::Stop)]),
    ]));
    let captured = Arc::clone(&transport);
    let mut config = EngineConfig::new("primary");
    config.max_transient_retries = 1;
    config.retry_backoff = Duration::from_millis(1);
    let mut engine = EngineRuntime::start(config, resolver(vec![("primary", transport)]));
    let events = run_turn(&mut engine).await;

    assert_eq!(captured.call_count(), 2, "the failed attempt is retried");
    assert_eq!(turn_usage(&events), vec![(300, 3)], "{events:?}");
    assert_eq!(spent(&events), 303);
}

/// The meter is the turn's, not the model's: a round the primary answered
/// stays paid for when a later round falls back to another model.
#[tokio::test]
async fn reported_rounds_on_both_sides_of_a_fallback_are_summed() {
    let mut tool_round = echo_call();
    tool_round.push(reported(1000, 10));
    tool_round.push(done(StopReason::ToolUse));
    let primary = Arc::new(MockTransport::new(vec![
        MockBody::Events(tool_round),
        MockBody::Err(TransportError::Retryable {
            status: Some(503),
            message: "overloaded".into(),
        }),
    ]));
    let backup = Arc::new(MockTransport::new(vec![MockBody::Events(vec![
        text("ok"),
        reported(1200, 5),
        done(StopReason::Stop),
    ])]));
    let mut config = EngineConfig::new("primary");
    config.fallback_models = vec!["backup".into()];
    config.max_transient_retries = 0;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(vec![("primary", primary), ("backup", backup)]),
        echo_registry(),
    );
    let events = run_turn(&mut engine).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::ModelSwitched { .. })),
        "{events:?}"
    );
    assert_eq!(turn_usage(&events), vec![(2200, 15)], "{events:?}");
    assert_eq!(spent(&events), 2215);
}

/// Rounds a turn paid for stay paid for when it ends without finishing: a
/// turn stopped by the tool-round cap reports what its rounds cost before
/// the failure that ends it.
#[tokio::test]
async fn a_turn_that_fails_still_reports_the_rounds_it_paid_for() {
    let round = |prompt, completion| {
        let mut events = echo_call();
        events.push(reported(prompt, completion));
        events.push(done(StopReason::ToolUse));
        MockBody::Events(events)
    };
    let transport = Arc::new(MockTransport::new(vec![round(100, 10), round(200, 20)]));
    let mut config = EngineConfig::new("primary");
    config.max_tool_rounds = 1;
    let mut engine = EngineRuntime::start_with_tools(
        config,
        resolver(vec![("primary", transport)]),
        echo_registry(),
    );
    let events = run_turn(&mut engine).await;
    assert!(
        matches!(events.last(), Some(EngineEvent::Failed { message, .. }) if message.contains("cap")),
        "{events:?}"
    );
    assert_eq!(turn_usage(&events), [(300, 30)], "{events:?}");
}

/// A cancelled turn reports the rounds it finished, so the session's total
/// is what was actually spent.
#[tokio::test]
async fn a_cancelled_turn_still_reports_the_rounds_it_paid_for() {
    let mut events = vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: "call-1".into(),
                name: "shell_probe".into(),
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: "{}".into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
    ];
    events.push(reported(100, 10));
    events.push(done(StopReason::ToolUse));
    let transport = Arc::new(MockTransport::new(vec![MockBody::Events(events)]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(titi_tools::ShellProbeTool));
    // The exec-tier call waits on an approval nobody gives, so the turn is
    // parked mid-way when the cancel lands.
    let mut config = EngineConfig::new("primary");
    config.approval_mode = titi_tools::ApprovalMode::Write;
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(vec![("primary", transport)]), tools);
    engine
        .send(EngineCommand::SubmitPrompt { text: "hi".into() })
        .await
        .expect("the engine takes the prompt");
    let mut seen = Vec::new();
    let usage = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = engine.recv().await {
            if matches!(event, EngineEvent::ToolApprovalNeeded { .. }) {
                engine
                    .send(EngineCommand::Cancel)
                    .await
                    .expect("the engine takes the cancel");
            }
            let usage = turn_usage(std::slice::from_ref(&event));
            seen.push(event);
            if let Some(usage) = usage.first() {
                return *usage;
            }
        }
        panic!("the engine stopped: {seen:?}");
    })
    .await
    .expect("a cancelled turn never reported its spend");
    assert_eq!(usage, (100, 10));
}

/// The share of the input the provider read from its prompt cache reaches
/// the turn's usage, summed over the rounds like the rest.
#[tokio::test]
async fn the_cached_share_of_each_round_is_reported() {
    let cached = |prompt, cached| {
        StreamEvent::Usage(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: 5,
            cached_tokens: cached,
        })
    };
    let mut first = echo_call();
    first.push(cached(1_000, 0));
    first.push(done(StopReason::ToolUse));
    let transport = Arc::new(MockTransport::new(vec![
        MockBody::Events(first),
        MockBody::Events(vec![
            text("done"),
            cached(1_200, 900),
            done(StopReason::Stop),
        ]),
    ]));
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        resolver(vec![("primary", transport)]),
        echo_registry(),
    );
    let events = run_turn(&mut engine).await;
    let reported: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TurnUsage { cached_tokens, .. } => Some(*cached_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(reported, [900], "{events:?}");
}
