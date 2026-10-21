#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Cache-break diagnostics through the real engine: the provider's reported
//! cached share, compared with what changed in the request since the one
//! before, becomes a notice naming the first change — or nothing at all when
//! the cache held.

use std::collections::HashMap;
use std::sync::Arc;

use titi_engine::protocol::SessionMode;
use titi_engine::{
    Engine, EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, Effort, MockBody, MockTransport, ModelCompat, StopReason, StreamEvent, TokenUsage,
    Transport,
};
use titi_tools::ToolRegistry;

struct MapResolver(HashMap<String, Arc<dyn Transport>>);

impl TransportResolver for MapResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        let transport = self
            .0
            .get(model)
            .cloned()
            .ok_or_else(|| RegistryError::UnknownModel(model.into()))?;
        let mut resolved = ResolvedModel::without_credential(model, transport);
        resolved.reasoning = Some(ModelCompat {
            efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            ..ModelCompat::default()
        });
        Ok(resolved)
    }
}

/// One answer, with the provider's own count of what it read from its cache.
fn answer(prompt: u64, cached: u64) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: "ok".into(),
        },
        StreamEvent::Usage(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: 5,
            cached_tokens: cached,
        }),
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

fn silent_answer() -> MockBody {
    MockBody::Events(vec![
        StreamEvent::TextDelta {
            id: BlockId::new("text"),
            text: "ok".into(),
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
        },
    ])
}

struct Session {
    engine: Engine,
    agent: tempfile::TempDir,
}

fn start(bodies: Vec<MockBody>) -> Session {
    let agent = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(bodies));
    let mut config = EngineConfig::new("primary");
    config.agent_dir = Some(agent.path().to_path_buf());
    config.workspace_root = Some(workspace.path().to_path_buf());
    let resolver = Arc::new(MapResolver(
        [("primary".to_owned(), transport as Arc<dyn Transport>)]
            .into_iter()
            .collect(),
    ));
    let mut tools = ToolRegistry::new();
    for tool in titi_tools::workspace_tools(workspace.path()) {
        tools.register(Arc::from(tool));
    }
    // The workspace outlives the engine through the tool registry's own copy
    // of its path; the directory itself is kept alive by the leak below.
    std::mem::forget(workspace);
    Session {
        engine: EngineRuntime::start_with_tools(config, resolver, tools),
        agent,
    }
}

/// One turn's events: up to the turn's end, and the few that trail it (the
/// budget, and what the provider's cache did for the last request).
async fn turn(engine: &mut Engine, text: &str) -> Vec<EngineEvent> {
    engine
        .send(EngineCommand::SubmitPrompt { text: text.into() })
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = engine.recv().await {
        let done = matches!(
            event,
            EngineEvent::TurnFinished { .. } | EngineEvent::Failed { .. }
        );
        events.push(event);
        if done {
            break;
        }
    }
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(300), engine.recv()).await
    {
        events.push(event);
    }
    events
}

fn cache_notices(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Notice { message } if message.starts_with("cache miss") => {
                Some(message.to_string())
            }
            _ => None,
        })
        .collect()
}

fn observed(events: &[EngineEvent]) -> Vec<(u32, u32, String, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::CacheObserved {
                prompt_tokens,
                cached_tokens,
                change,
                missed,
                ..
            } => Some((*prompt_tokens, *cached_tokens, change.to_string(), *missed)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_unchanged_second_turn_that_hit_its_cache_says_nothing() {
    let mut s = start(vec![
        answer(5000, 0),
        answer(5100, 4900),
        answer(5200, 5000),
    ]);
    let first = turn(&mut s.engine, "one").await;
    assert_eq!(cache_notices(&first), ["cache miss · first request"]);
    for text in ["two", "three"] {
        let events = turn(&mut s.engine, text).await;
        assert!(cache_notices(&events).is_empty(), "{events:?}");
        let seen = observed(&events);
        assert_eq!(seen.len(), 1, "{events:?}");
        assert_eq!(seen[0].2, "prefix unchanged");
        assert!(!seen[0].3);
    }
}

#[tokio::test]
async fn an_effort_switch_between_turns_is_named_when_the_cache_is_lost() {
    let mut s = start(vec![
        answer(5000, 4000),
        answer(5100, 4900),
        answer(5200, 80),
    ]);
    turn(&mut s.engine, "one").await;
    turn(&mut s.engine, "two").await;
    s.engine
        .send(EngineCommand::SetEffort {
            effort: Some(Effort::High),
        })
        .await
        .unwrap();
    let events = turn(&mut s.engine, "three").await;
    assert_eq!(
        cache_notices(&events),
        ["cache miss · model or effort switched"],
        "{events:?}"
    );
    let seen = observed(&events);
    assert_eq!(seen[0].2, "model or effort switched");
    assert!(seen[0].3);
}

#[tokio::test]
async fn a_tool_list_change_is_named_with_the_tools_that_moved() {
    let mut s = start(vec![
        answer(5000, 4000),
        answer(5100, 4900),
        answer(5200, 60),
    ]);
    turn(&mut s.engine, "one").await;
    turn(&mut s.engine, "two").await;
    // Plan mode keeps only the read tools: everything else leaves the list.
    s.engine
        .send(EngineCommand::SetMode {
            mode: SessionMode::Plan,
        })
        .await
        .unwrap();
    let events = turn(&mut s.engine, "three").await;
    let said = cache_notices(&events);
    assert_eq!(said.len(), 1, "{events:?}");
    assert!(
        said[0].starts_with("cache miss · tool list changed ("),
        "{said:?}"
    );
    assert!(said[0].contains('−'), "{said:?}");
    assert!(!said[0].contains('+'), "nothing was added: {said:?}");
}

#[tokio::test]
async fn a_skill_added_mid_session_is_a_system_prompt_change_by_block_and_size() {
    let mut s = start(vec![
        answer(5000, 4000),
        answer(5100, 4900),
        answer(5200, 50),
    ]);
    turn(&mut s.engine, "one").await;
    turn(&mut s.engine, "two").await;
    let skill = s.agent.path().join("skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: SECRET-SKILL-TEXT\n---\nBody\n",
    )
    .unwrap();
    let events = turn(&mut s.engine, "three").await;
    let said = cache_notices(&events);
    assert_eq!(said.len(), 1, "{events:?}");
    assert!(
        said[0].starts_with("cache miss · system prompt changed (block "),
        "{said:?}"
    );
    assert!(said[0].contains(", +"), "{said:?}");
    assert!(said[0].ends_with(" chars)"), "{said:?}");
    assert!(!said[0].contains("SECRET"), "never content: {said:?}");
}

#[tokio::test]
async fn a_change_the_provider_did_not_lose_to_is_not_a_notice() {
    let mut s = start(vec![
        answer(5000, 4000),
        answer(5100, 4900),
        answer(5200, 5000),
    ]);
    turn(&mut s.engine, "one").await;
    turn(&mut s.engine, "two").await;
    s.engine
        .send(EngineCommand::SetMode {
            mode: SessionMode::Plan,
        })
        .await
        .unwrap();
    // The provider still read nearly all of it.
    let events = turn(&mut s.engine, "three").await;
    assert!(cache_notices(&events).is_empty(), "{events:?}");
    let seen = observed(&events);
    assert!(seen[0].2.starts_with("tool list changed"), "{seen:?}");
    assert!(!seen[0].3);
}

#[tokio::test]
async fn a_provider_that_reports_no_usage_gets_no_report_and_no_notice() {
    let mut s = start(vec![silent_answer(), silent_answer()]);
    for text in ["one", "two"] {
        let events = turn(&mut s.engine, text).await;
        assert!(observed(&events).is_empty(), "{events:?}");
        assert!(cache_notices(&events).is_empty(), "{events:?}");
    }
}
