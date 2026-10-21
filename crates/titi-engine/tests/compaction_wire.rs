#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A folded history must still be a valid request on every wire family: each
//! tool result answers a call that is in the request, and each call has its
//! result. Folding drops the oldest messages and rewrites old results to
//! stubs, which is where a pair could be torn.

use std::collections::HashSet;

use serde_json::Value;
use titi_core::compaction::{CompactionPolicy, Strategy};
use titi_engine::compaction::{Fold, compact};
use titi_providers::{ApiKind, ChatMessage, Role, ToolCallRef, WireRequest, build_http_request};

fn user(text: &str) -> ChatMessage {
    ChatMessage {
        role: Role::User,
        content: text.into(),
        ..Default::default()
    }
}

fn calls(ids: &[&str], name: &str) -> ChatMessage {
    ChatMessage {
        role: Role::Assistant,
        content: "looking".into(),
        tool_calls: ids
            .iter()
            .map(|id| ToolCallRef {
                call_id: (*id).into(),
                name: name.into(),
                arguments: format!(r#"{{"path":"src/{id}.rs"}}"#).into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn result(id: &str, body: &str) -> ChatMessage {
    ChatMessage {
        role: Role::Tool,
        content: body.into(),
        tool_call_id: Some(id.into()),
        ..Default::default()
    }
}

/// Parallel calls in the middle of the history, one more pair at the end.
fn history() -> Vec<ChatMessage> {
    vec![
        ChatMessage {
            role: Role::System,
            content: "you are titi".into(),
            ..Default::default()
        },
        user("fix everything"),
        calls(&["a1", "a2"], "read"),
        result("a1", &"first file\n".repeat(200)),
        result("a2", &"second file\n".repeat(200)),
        calls(&["b1"], "read"),
        result("b1", &"third file\n".repeat(200)),
        ChatMessage {
            role: Role::Assistant,
            content: "read all three".into(),
            ..Default::default()
        },
        user("now the last one"),
        calls(&["c1"], "read"),
        result("c1", &"fourth file\n".repeat(200)),
    ]
}

fn body(api: ApiKind, messages: Vec<ChatMessage>) -> Value {
    let mut request = WireRequest::new("m");
    request.messages = messages;
    let http = build_http_request(api, "http://127.0.0.1:9/v1", &request, None);
    serde_json::from_slice(&http.body.expect("a body")).expect("json")
}

/// Calls and results in wire order, as (is_call, id).
fn pairs(api: ApiKind, wire: &Value) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
    match api {
        ApiKind::OpenAiCompletions => {
            for message in wire["messages"].as_array().unwrap() {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    out.push((true, text(call, "id")));
                }
                if message["role"] == "tool" {
                    out.push((false, text(message, "tool_call_id")));
                }
            }
        }
        ApiKind::OpenAiResponses => {
            for item in wire["input"].as_array().unwrap() {
                match item["type"].as_str() {
                    Some("function_call") => out.push((true, text(item, "call_id"))),
                    Some("function_call_output") => out.push((false, text(item, "call_id"))),
                    _ => {}
                }
            }
        }
        ApiKind::AnthropicMessages => {
            for message in wire["messages"].as_array().unwrap() {
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("tool_use") => out.push((true, text(block, "id"))),
                        Some("tool_result") => out.push((false, text(block, "tool_use_id"))),
                        _ => {}
                    }
                }
            }
        }
        ApiKind::GeminiGenerateContent => {
            // Gemini pairs by name, so the names are the ids here.
            for content in wire["contents"].as_array().unwrap() {
                for part in content["parts"].as_array().into_iter().flatten() {
                    if let Some(call) = part.get("functionCall") {
                        out.push((true, text(call, "name")));
                    }
                    if let Some(response) = part.get("functionResponse") {
                        out.push((false, text(response, "name")));
                    }
                }
            }
        }
    }
    out
}

fn assert_paired(api: ApiKind, wire: &Value) {
    let seen = pairs(api, wire);
    let mut open: HashSet<String> = HashSet::new();
    let by_name = api == ApiKind::GeminiGenerateContent;
    let mut calls_seen = 0;
    for (is_call, id) in &seen {
        if *is_call {
            calls_seen += 1;
            if !by_name {
                open.insert(id.clone());
            }
        } else if !by_name {
            assert!(
                open.remove(id),
                "{api}: result {id} has no call before it: {seen:?}"
            );
        }
    }
    assert!(open.is_empty(), "{api}: calls without results: {open:?}");
    let results = seen.iter().filter(|(is_call, _)| !is_call).count();
    assert_eq!(calls_seen, results, "{api}: {seen:?}");
    assert!(calls_seen > 0, "{api}: the tail keeps a pair: {seen:?}");
}

const FAMILIES: [ApiKind; 4] = [
    ApiKind::OpenAiCompletions,
    ApiKind::OpenAiResponses,
    ApiKind::AnthropicMessages,
    ApiKind::GeminiGenerateContent,
];

async fn fold_with(keep_recent_tokens: u64, threshold: f64, window: u64) -> Vec<ChatMessage> {
    let policy = CompactionPolicy {
        threshold_percent: threshold,
        keep_recent_tokens,
        method_order: vec![Strategy::SnapCompact],
    };
    let mut messages = history();
    compact(&mut messages, &Fold::new(&policy, window))
        .await
        .expect("a compaction");
    messages
}

#[tokio::test]
async fn a_full_fold_leaves_every_call_and_result_paired_on_every_wire() {
    // Every cut point the tail budget can pick, including ones that land
    // between the parallel calls and their results.
    for keep in [1, 20, 60, 120, 400, 700] {
        let messages = fold_with(keep, 0.0, 100_000).await;
        for api in FAMILIES {
            assert_paired(api, &body(api, messages.clone()));
        }
    }
}

#[tokio::test]
async fn masking_alone_leaves_every_call_and_result_paired_on_every_wire() {
    // A window the masked history fits in, the unmasked one does not.
    let messages = fold_with(300, 50.0, 4_000).await;
    assert!(
        messages
            .iter()
            .any(|m| m.content.starts_with("[tool result elided")),
        "{messages:#?}"
    );
    for api in FAMILIES {
        assert_paired(api, &body(api, messages.clone()));
    }
}

#[tokio::test]
async fn the_digest_is_a_user_turn_behind_the_system_prompt_on_every_wire() {
    let messages = fold_with(60, 0.0, 100_000).await;
    assert_eq!(messages[0].role, Role::System);
    assert_eq!(messages[1].role, Role::User);
    assert!(messages[1].content.contains("<conversation-summary"));

    let completions = body(ApiKind::OpenAiCompletions, messages.clone());
    let roles: Vec<&str> = completions["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles.iter().filter(|r| **r == "system").count(),
        1,
        "{roles:?}"
    );

    let anthropic = body(ApiKind::AnthropicMessages, messages.clone());
    assert_eq!(
        anthropic["system"]
            .as_str()
            .or_else(|| anthropic["system"][0]["text"].as_str()),
        Some("you are titi")
    );
    assert!(
        anthropic["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<conversation-summary")
    );

    let gemini = body(ApiKind::GeminiGenerateContent, messages);
    assert_eq!(gemini["contents"][0]["role"], "user");
}
