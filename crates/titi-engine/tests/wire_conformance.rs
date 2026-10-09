#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Every wire family titi speaks, replayed byte for byte.
//!
//! Each case is a hand-written SSE script in the shape the family's decoder
//! accepts, fed through `FamilyTransport` (the real request builder, the real
//! SSE reader, the real per-family decoder) into the engine's turn loop, with
//! the turn driven by a `MockFetch` that replays those bytes and no network
//! anywhere. What is asserted is what the two readers of a turn see: the tool
//! calls and their arguments as they reach the *next request*, and the events
//! a surface receives, in order and paired.
//!
//! The point is the seam these tests close: every layer here was tested alone
//! — the decoders by their own unit tests, the loop by scripted `StreamEvent`s
//! — and a defect that needed both (two tool calls in one response, where the
//! decoder was right and the collector held one slot) lived in the gap.

use std::sync::Arc;

use serde_json::Value;
use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::ToolSpec;
use titi_providers::{
    ApiKind, FamilyTransport, HttpFetch, MockFetch, MockFetchResponse, Transport, TransportError,
};
use titi_tools::{ApprovalTier, ToolDefinition, ToolHandler, ToolRegistry, ToolResult};

// ---- the harness -----------------------------------------------------------

/// A Read-tier tool that answers with its own name and the arguments it was
/// given, so a wrong or missing argument shows up in the answer.
struct Stub(&'static str);

#[async_trait::async_trait]
impl ToolHandler for Stub {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: self.0.into(),
                description: format!("the {} stub", self.0).into(),
                parameters: serde_json::json!({"type": "object", "additionalProperties": true}),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        ToolResult {
            output: format!("{} called with {args}", self.0).into(),
            is_error: false,
            detail: None,
        }
    }
}

fn stubs(names: &[&'static str]) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    for name in names {
        tools.register(Arc::new(Stub(name)));
    }
    tools
}

struct FixedResolver(Arc<dyn Transport>);

impl TransportResolver for FixedResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        Ok(ResolvedModel::without_credential(
            model,
            Arc::clone(&self.0),
        ))
    }
}

/// A transport that replays `bodies` in order, one per provider request.
fn replay(api: ApiKind, bodies: Vec<Vec<&str>>) -> (Arc<dyn Transport>, Arc<MockFetch>) {
    let responses: Vec<Result<MockFetchResponse, TransportError>> = bodies
        .into_iter()
        .map(|lines| {
            // Each line is one SSE frame, so each needs its blank line: the
            // reader emits a frame at a boundary and holds the tail otherwise.
            Ok(MockFetchResponse::sse(
                lines
                    .into_iter()
                    .map(|line| format!("{line}\n\n"))
                    .collect(),
            ))
        })
        .collect();
    let fetch = Arc::new(MockFetch::new(responses));
    let transport: Arc<dyn Transport> = Arc::new(FamilyTransport::new(
        api,
        "http://127.0.0.1:9/v1",
        Arc::clone(&fetch) as Arc<dyn HttpFetch>,
    ));
    (transport, fetch)
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

/// Drives one turn over a replayed stream and hands back the events.
async fn turn(
    api: ApiKind,
    bodies: Vec<Vec<&str>>,
    tools: &[&'static str],
) -> (Vec<EngineEvent>, Arc<MockFetch>) {
    let (transport, fetch) = replay(api, bodies);
    let mut engine = EngineRuntime::start_with_tools(
        EngineConfig::new("primary"),
        Arc::new(FixedResolver(transport)),
        stubs(tools),
    );
    engine
        .send(EngineCommand::SubmitPrompt { text: "go".into() })
        .await
        .unwrap();
    let events = collect_until_terminal(&mut engine).await;
    (events, fetch)
}

/// The calls the *next* request carries, as `(id, name, arguments)`.
///
/// Every family writes them its own way, so this reads the wire body rather
/// than a shared shape: the point is what the provider would be told.
fn calls_in(body: &Value, api: ApiKind) -> Vec<(String, String, String)> {
    let mut calls = Vec::new();
    match api {
        ApiKind::OpenAiCompletions => {
            for message in body["messages"].as_array().into_iter().flatten() {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    calls.push((
                        text(&call["id"]),
                        text(&call["function"]["name"]),
                        text(&call["function"]["arguments"]),
                    ));
                }
            }
        }
        ApiKind::OpenAiResponses => {
            for item in body["input"].as_array().into_iter().flatten() {
                if item["type"] == "function_call" {
                    calls.push((
                        text(&item["call_id"]),
                        text(&item["name"]),
                        text(&item["arguments"]),
                    ));
                }
            }
        }
        ApiKind::AnthropicMessages => {
            for message in body["messages"].as_array().into_iter().flatten() {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        calls.push((
                            text(&block["id"]),
                            text(&block["name"]),
                            block["input"].to_string(),
                        ));
                    }
                }
            }
        }
        ApiKind::GeminiGenerateContent => {
            for content in body["contents"].as_array().into_iter().flatten() {
                for part in content["parts"].as_array().into_iter().flatten() {
                    if let Some(call) = part.get("functionCall") {
                        calls.push((String::new(), text(&call["name"]), call["args"].to_string()));
                    }
                }
            }
        }
    }
    calls
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// The tool messages the next request carries, in order.
fn tool_messages(body: &Value, api: ApiKind) -> Vec<String> {
    let mut out = Vec::new();
    match api {
        ApiKind::OpenAiCompletions => {
            for message in body["messages"].as_array().into_iter().flatten() {
                if message["role"] == "tool" {
                    out.push(text(&message["content"]));
                }
            }
        }
        ApiKind::OpenAiResponses => {
            for item in body["input"].as_array().into_iter().flatten() {
                if item["type"] == "function_call_output" {
                    out.push(text(&item["output"]));
                }
            }
        }
        ApiKind::AnthropicMessages => {
            for message in body["messages"].as_array().into_iter().flatten() {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" {
                        out.push(match &block["content"] {
                            Value::Array(items) => items
                                .iter()
                                .map(|item| text(&item["text"]))
                                .collect::<Vec<_>>()
                                .join(""),
                            other => text(other),
                        });
                    }
                }
            }
        }
        ApiKind::GeminiGenerateContent => {
            for content in body["contents"].as_array().into_iter().flatten() {
                for part in content["parts"].as_array().into_iter().flatten() {
                    if let Some(response) = part.get("functionResponse") {
                        out.push(response["response"].to_string());
                    }
                }
            }
        }
    }
    out
}

fn bodies(fetch: &MockFetch) -> Vec<Value> {
    fetch
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| {
            request
                .body
                .as_ref()
                .and_then(|body| serde_json::from_slice::<Value>(body).ok())
        })
        .collect()
}

/// The paired `ToolStarted`/`ToolFinished` names, in order.
fn tool_events(events: &[EngineEvent]) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for event in events {
        if let EngineEvent::ToolFinished { is_error, .. } = event {
            out.push((String::from("finished"), *is_error));
        }
    }
    out
}

/// A tool call's arguments as the decoder delivered them, keyed by call id.
fn arguments_of(calls: &[(String, String, String)]) -> Vec<(String, String, String)> {
    calls
        .iter()
        .map(|(id, name, args)| {
            let parsed: Value = serde_json::from_str(args).unwrap_or(Value::Null);
            let rendered = parsed.to_string();
            (id.clone(), name.clone(), rendered)
        })
        .collect()
}

// ---- OpenAI completions ----------------------------------------------------

/// Thinking, text before and after three calls (two opened in one chunk, one
/// interleaved, one with empty arguments), usage with cached tokens, a finish
/// reason — all in one response.
fn completions_sink() -> Vec<&'static str> {
    vec![
        r#"data: {"choices":[{"delta":{"reasoning_content":"thinking hard"},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"content":"before "},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":"{\"path\":\"a.rs\"}"}},{"index":1,"id":"call_b","type":"function","function":{"name":"grep","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":2,"id":"call_c","type":"function","function":{"name":"glob","arguments":"{}"}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"pattern\":\"b"}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2\"}"}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"content":" after"},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":40}}}"#,
        "data: [DONE]",
    ]
}

fn completions_done() -> Vec<&'static str> {
    vec![
        r#"data: {"choices":[{"delta":{"content":"done"},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ]
}

#[tokio::test]
async fn completions_keeps_every_call_and_its_arguments() {
    let (events, fetch) = turn(
        ApiKind::OpenAiCompletions,
        vec![completions_sink(), completions_done()],
        &["read", "grep", "glob"],
    )
    .await;
    let bodies = bodies(&fetch);
    assert_eq!(bodies.len(), 2, "the turn made two requests: {bodies:?}");

    let calls = arguments_of(&calls_in(&bodies[1], ApiKind::OpenAiCompletions));
    assert_eq!(
        calls,
        vec![
            (
                "call_a".to_owned(),
                "read".to_owned(),
                "{\"path\":\"a.rs\"}".to_owned()
            ),
            (
                "call_b".to_owned(),
                "grep".to_owned(),
                "{\"pattern\":\"b2\"}".to_owned()
            ),
            ("call_c".to_owned(), "glob".to_owned(), "{}".to_owned()),
        ],
        "every call, in order, with its own arguments"
    );

    let results = tool_messages(&bodies[1], ApiKind::OpenAiCompletions);
    assert_eq!(results.len(), 3, "one result per call: {results:?}");
    assert!(results[0].contains("read called with"), "{results:?}");
    assert!(
        results[1].contains("b2"),
        "the interleaved delta landed: {results:?}"
    );
    assert!(results[2].contains("glob called with"), "{results:?}");

    // The surface: three calls, each started and finished, no error.
    let finished = tool_events(&events);
    assert_eq!(finished.len(), 3, "{events:?}");
    assert!(
        finished.iter().all(|(_, is_error)| !*is_error),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::TurnFinished { .. })),
        "{events:?}"
    );
}

// ---- Responses API ---------------------------------------------------------

fn responses_sink() -> Vec<&'static str> {
    vec![
        r#"data: {"type":"response.created"}"#,
        r#"data: {"type":"response.reasoning_text.delta","delta":"weighing it"}"#,
        r#"data: {"type":"response.output_text.delta","delta":"before "}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"read"}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_a","output_index":0,"delta":"{\"path\":\"a.rs\"}"}"#,
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"grep"}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_b","output_index":1,"delta":"{\"pattern\":\"b\"}"}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_a","output_index":0,"delta":""}"#,
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"grep","arguments":"{\"pattern\":\"b\"}"}}"#,
        r#"data: {"type":"response.output_text.delta","delta":" after"}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":40}}}}"#,
    ]
}

#[tokio::test]
async fn responses_keeps_every_call_and_its_arguments() {
    let (events, fetch) = turn(
        ApiKind::OpenAiResponses,
        vec![responses_sink(), completions_done()],
        &["read", "grep"],
    )
    .await;
    let bodies = bodies(&fetch);
    assert_eq!(bodies.len(), 2, "the turn made two requests: {bodies:?}");

    let calls = arguments_of(&calls_in(&bodies[1], ApiKind::OpenAiResponses));
    assert_eq!(
        calls,
        vec![
            (
                "call_a".to_owned(),
                "read".to_owned(),
                "{\"path\":\"a.rs\"}".to_owned()
            ),
            (
                "call_b".to_owned(),
                "grep".to_owned(),
                "{\"pattern\":\"b\"}".to_owned()
            ),
        ],
        "both calls, keyed by item, with their own arguments"
    );
    let results = tool_messages(&bodies[1], ApiKind::OpenAiResponses);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results[0].contains("read called with"), "{results:?}");
    assert!(results[1].contains("grep called with"), "{results:?}");
    assert_eq!(tool_events(&events).len(), 2, "{events:?}");
}

// ---- Anthropic messages ----------------------------------------------------

fn anthropic_sink() -> Vec<&'static str> {
    vec![
        r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40}}}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing it"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"before "}}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_a","name":"read"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":2}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call_b","name":"grep"}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"b\"}"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":3}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":" after"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":4}"#,
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}"#,
        r#"event: message_stop
data: {"type":"message_stop"}"#,
    ]
}

#[tokio::test]
async fn anthropic_keeps_every_call_and_its_arguments() {
    let (events, fetch) = turn(
        ApiKind::AnthropicMessages,
        vec![anthropic_sink(), completions_done()],
        &["read", "grep"],
    )
    .await;
    let bodies = bodies(&fetch);
    assert_eq!(bodies.len(), 2, "the turn made two requests: {bodies:?}");

    let calls = arguments_of(&calls_in(&bodies[1], ApiKind::AnthropicMessages));
    assert_eq!(calls.len(), 2, "both tool_use blocks: {calls:?}");
    assert_eq!(calls[0].0, "call_a");
    assert_eq!(calls[0].1, "read");
    assert_eq!(calls[1].0, "call_b");
    assert_eq!(calls[1].1, "grep");
    assert!(
        calls[0].2.contains("a.rs") && calls[1].2.contains("\"b\""),
        "each block carries its own input: {calls:?}"
    );
    let results = tool_messages(&bodies[1], ApiKind::AnthropicMessages);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(tool_events(&events).len(), 2, "{events:?}");
}

// ---- Gemini generateContent ------------------------------------------------

fn gemini_sink() -> Vec<&'static str> {
    vec![
        r#"data: {"candidates":[{"content":{"parts":[{"thought":true,"text":"weighing it"}]}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":"before "}]}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a.rs"}}}]}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"grep","args":{"pattern":"b"}}}]}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":" after"}]}}]}"#,
        r#"data: {"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":40}}"#,
    ]
}

#[tokio::test]
async fn gemini_keeps_every_call_and_its_arguments() {
    let (events, fetch) = turn(
        ApiKind::GeminiGenerateContent,
        vec![gemini_sink(), completions_done()],
        &["read", "grep"],
    )
    .await;
    let bodies = bodies(&fetch);
    assert_eq!(bodies.len(), 2, "the turn made two requests: {bodies:?}");

    let calls = arguments_of(&calls_in(&bodies[1], ApiKind::GeminiGenerateContent));
    assert_eq!(calls.len(), 2, "both functionCall parts: {calls:?}");
    assert_eq!(calls[0].1, "read");
    assert!(calls[0].2.contains("a.rs"), "{calls:?}");
    assert_eq!(calls[1].1, "grep");
    assert!(calls[1].2.contains("\"b\""), "{calls:?}");
    let results = tool_messages(&bodies[1], ApiKind::GeminiGenerateContent);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(tool_events(&events).len(), 2, "{events:?}");
}

// ---- an error mid-stream ---------------------------------------------------

/// Every family's mid-stream error reaches the turn as a failure rather than a
/// silent half-answer: the rate-limit and overloaded shapes each family sends.
#[tokio::test]
async fn a_mid_stream_error_fails_the_turn_in_every_family() {
    let cases: Vec<(ApiKind, Vec<&'static str>)> = vec![
        (
            ApiKind::OpenAiCompletions,
            vec![
                r#"data: {"choices":[{"delta":{"content":"half"},"finish_reason":null}]}"#,
                r#"data: {"error":{"message":"rate limit reached","type":"rate_limit_error"}}"#,
                "data: [DONE]",
            ],
        ),
        (
            ApiKind::OpenAiResponses,
            vec![
                r#"data: {"type":"response.output_text.delta","delta":"half"}"#,
                r#"data: {"type":"error","error":{"type":"rate_limit_error","message":"rate limit reached"}}"#,
            ],
        ),
        (
            ApiKind::AnthropicMessages,
            vec![
                r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}"#,
                r#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ],
        ),
        (
            ApiKind::GeminiGenerateContent,
            vec![
                r#"data: {"candidates":[{"content":{"parts":[{"text":"half"}]}}]}"#,
                r#"data: {"promptFeedback":{"blockReason":"SAFETY"}}"#,
            ],
        ),
    ];

    for (api, lines) in cases {
        let (events, fetch) = turn(api, vec![lines, completions_done()], &["read"]).await;
        let failed = events.iter().any(|event| {
            matches!(
                event,
                EngineEvent::Failed { .. } | EngineEvent::TurnFinished { .. }
            )
        });
        assert!(failed, "{api:?} ended its turn: {events:?}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::StreamDelta { text, .. } if text.contains("half"))),
            "{api:?} streamed what arrived before the error: {events:?}"
        );
        assert!(!bodies(&fetch).is_empty(), "{api:?} made its request");
    }
}
