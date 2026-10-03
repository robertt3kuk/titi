//! Gemini `streamGenerateContent?alt=sse` decoder: parts-chunks →
//! [`StreamEvent`].

use serde_json::Value;
use smol_str::SmolStr;

use crate::compat::StreamDecodePolicy;
use crate::stop::map_stop_reason;
use crate::stream::{BlockId, ErrorReason, StreamEvent, TokenUsage, ToolCallRef};
use crate::transport::ApiKind;

/// Mutable decode state for one Gemini stream.
#[derive(Default)]
pub struct GeminiStreamState {
    started: bool,
    text_open: bool,
    thinking_open: bool,
    /// Open tool calls by index (Gemini has no per-index stream; each chunk
    /// carries complete functionCall parts).
    tools_opened: Vec<bool>,
}

fn text_id() -> BlockId {
    BlockId("text".into())
}

fn thinking_id() -> BlockId {
    BlockId("thinking".into())
}

fn tool_id(i: usize) -> BlockId {
    BlockId(format!("tool_{i}").into())
}

/// Decode one Gemini SSE chunk (one `candidates[0].content.parts` payload).
pub fn decode_chunk(
    payload: &Value,
    state: &mut GeminiStreamState,
    policy: &StreamDecodePolicy,
) -> Vec<StreamEvent> {
    let mut events = chunk_events(payload, state, policy);
    if let Some(usage) = chunk_usage(payload) {
        let at = events
            .iter()
            .position(StreamEvent::is_terminal)
            .unwrap_or(events.len());
        events.insert(at, StreamEvent::Usage(usage));
    }
    events
}

/// The cumulative `usageMetadata` a chunk carries. `promptTokenCount`
/// includes the cached part; thinking is billed as output beside the
/// candidates, and a zero count is left out of the JSON altogether.
fn chunk_usage(payload: &Value) -> Option<TokenUsage> {
    let metadata = payload.get("usageMetadata")?;
    let count = |key: &str| metadata.get(key).and_then(Value::as_u64);
    let prompt = count("promptTokenCount")?;
    let completion = count("candidatesTokenCount")
        .unwrap_or(0)
        .saturating_add(count("thoughtsTokenCount").unwrap_or(0));
    TokenUsage::reported(
        prompt,
        completion,
        count("cachedContentTokenCount").unwrap_or(0),
    )
}

fn chunk_events(
    payload: &Value,
    state: &mut GeminiStreamState,
    policy: &StreamDecodePolicy,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    // Prompt-feedback / error payloads surface as stream errors.
    if let Some(feedback) = payload.get("promptFeedback") {
        if let Some(reason) = feedback.get("blockReason").and_then(Value::as_str) {
            return vec![StreamEvent::Error {
                reason: ErrorReason::Rejected,
                message: format!("blocked by prompt feedback: {reason}").into(),
            }];
        }
    }

    let Some(candidate) = payload.pointer("/candidates/0") else {
        return events;
    };
    let content = candidate.get("content");
    let parts = content
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array);

    if !state.started {
        state.started = true;
        events.push(StreamEvent::Start);
    }

    if let Some(parts) = parts {
        for part in parts {
            let thought = part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // Gemini tool calls arrive as complete functionCall parts.
            if let Some(call) = part.get("functionCall") {
                let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                let index = state.tools_opened.len();
                state.tools_opened.push(true);
                events.push(StreamEvent::ToolcallStart {
                    id: tool_id(index),
                    call: ToolCallRef {
                        call_id: SmolStr::from(format!("gemini_{index}")),
                        name: name.into(),
                    },
                });
                let args = call
                    .get("args")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                events.push(StreamEvent::ToolcallDelta {
                    id: tool_id(index),
                    json: args.to_string().into(),
                });
                events.push(StreamEvent::ToolcallEnd { id: tool_id(index) });
                continue;
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if thought {
                    if !state.thinking_open {
                        state.thinking_open = true;
                        events.push(StreamEvent::ThinkingStart { id: thinking_id() });
                    }
                    events.push(StreamEvent::ThinkingDelta {
                        id: thinking_id(),
                        text: text.into(),
                    });
                } else {
                    if !state.text_open {
                        state.text_open = true;
                        events.push(StreamEvent::TextStart { id: text_id() });
                    }
                    events.push(StreamEvent::TextDelta {
                        id: text_id(),
                        text: text.into(),
                    });
                }
                continue;
            }
            // Unknown part shape: tolerated unless parts-array policy set.
            if policy.content_is_parts_array && part.get("text").is_none() {
                // Mistral-class guard has no meaning here; skip silently.
            }
        }
    }

    // finishReason terminates the stream (may arrive in the same chunk as
    // parts).
    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
        events.extend(close(state, reason));
    }
    events
}

fn close(state: &mut GeminiStreamState, wire: &str) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if state.thinking_open {
        state.thinking_open = false;
        events.push(StreamEvent::ThinkingEnd { id: thinking_id() });
    }
    if state.text_open {
        state.text_open = false;
        events.push(StreamEvent::TextEnd { id: text_id() });
    }
    match map_stop_reason(ApiKind::GeminiGenerateContent, wire) {
        crate::stop::StopMapping::Stop(reason) => events.push(StreamEvent::Done { reason }),
        crate::stop::StopMapping::Error(reason) => {
            events.push(StreamEvent::Error {
                reason,
                message: format!("finishReason {wire} maps to error").into(),
            });
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{StopReason, TokenUsage};
    use serde_json::json;

    #[test]
    fn text_parts_flow() {
        let mut s = GeminiStreamState::default();
        let policy = StreamDecodePolicy::default();
        let ev = decode_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![
                StreamEvent::Start,
                StreamEvent::TextStart { id: text_id() },
                StreamEvent::TextDelta {
                    id: text_id(),
                    text: "Hel".into()
                }
            ]
        );
        let ev = decode_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"lo"}]},"finishReason":"STOP"}]}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![
                StreamEvent::TextDelta {
                    id: text_id(),
                    text: "lo".into()
                },
                StreamEvent::TextEnd { id: text_id() },
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    #[test]
    fn thought_parts_are_thinking() {
        let mut s = GeminiStreamState::default();
        let policy = StreamDecodePolicy::default();
        let ev = decode_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"ponder","thought":true}]}}]}),
            &mut s,
            &policy,
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, StreamEvent::ThinkingDelta { .. }))
        );
        assert!(
            !ev.iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { .. }))
        );
    }

    #[test]
    fn function_call_parts() {
        let mut s = GeminiStreamState::default();
        let policy = StreamDecodePolicy::default();
        let ev = decode_chunk(
            &json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"search","args":{"q":"rust"}}}]}}]}),
            &mut s,
            &policy,
        );
        assert!(matches!(&ev[1], StreamEvent::ToolcallStart { call, .. } if call.name == "search"));
        assert!(
            matches!(&ev[2], StreamEvent::ToolcallDelta { json, .. } if json.contains("\"q\""))
        );
        assert!(matches!(ev[3], StreamEvent::ToolcallEnd { .. }));
    }

    #[test]
    fn stop_reason_table_gemini() {
        let mut s = GeminiStreamState::default();
        let policy = StreamDecodePolicy::default();
        let check = |wire: &str, s: &mut GeminiStreamState| {
            decode_chunk(
                &json!({"candidates":[{"finishReason":wire}]}),
                s,
                &StreamDecodePolicy::default(),
            )
            .pop()
            .expect("terminal")
        };
        assert_eq!(
            check("STOP", &mut s),
            StreamEvent::Done {
                reason: StopReason::Stop
            }
        );
        assert_eq!(
            check("MAX_TOKENS", &mut s),
            StreamEvent::Done {
                reason: StopReason::Length
            }
        );
        assert!(matches!(check("SAFETY", &mut s), StreamEvent::Error { .. }));
        assert!(matches!(
            check("MALFORMED_FUNCTION_CALL", &mut s),
            StreamEvent::Error { .. }
        ));
        let _ = policy;
    }

    fn usages(events: &[StreamEvent]) -> Vec<TokenUsage> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .collect()
    }

    /// `usageMetadata` is cumulative and rides on the chunks; the one on the
    /// closing chunk lands before `Done`. Thinking is billed as output, and
    /// `promptTokenCount` already includes the cached part.
    #[test]
    fn usage_metadata_is_reported_before_done() {
        let mut s = GeminiStreamState::default();
        let policy = StreamDecodePolicy::default();
        let ev = decode_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"Hel"}],"role":"model"},"index":0}],
                    "usageMetadata":{"promptTokenCount":950,"totalTokenCount":950},
                    "modelVersion":"gemini-test"}),
            &mut s,
            &policy,
        );
        assert_eq!(
            usages(&ev),
            vec![TokenUsage {
                prompt_tokens: 950,
                completion_tokens: 0,
                cached_tokens: 0,
            }]
        );
        let ev = decode_chunk(
            &json!({"candidates":[{"content":{"parts":[{"text":"lo"}],"role":"model"},
                                   "finishReason":"STOP","index":0}],
                    "usageMetadata":{"promptTokenCount":950,"candidatesTokenCount":80,
                                     "thoughtsTokenCount":40,"cachedContentTokenCount":600,
                                     "totalTokenCount":1070}}),
            &mut s,
            &policy,
        );
        assert_eq!(
            &ev[ev.len() - 2..],
            &[
                StreamEvent::Usage(TokenUsage {
                    prompt_tokens: 950,
                    completion_tokens: 120,
                    cached_tokens: 600,
                }),
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    /// A count that arrives in a chunk without candidates still counts.
    #[test]
    fn usage_only_chunk_is_reported() {
        let mut s = GeminiStreamState::default();
        let ev = decode_chunk(
            &json!({"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}}),
            &mut s,
            &StreamDecodePolicy::default(),
        );
        assert_eq!(
            ev,
            vec![StreamEvent::Usage(TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                cached_tokens: 0,
            })]
        );
    }

    #[test]
    fn missing_or_malformed_usage_metadata_is_absent() {
        for metadata in [
            Value::Null,
            json!({}),
            json!({"promptTokenCount":"950","candidatesTokenCount":3}),
            json!({"candidatesTokenCount":3}),
            json!({"promptTokenCount":-1,"candidatesTokenCount":3}),
            json!([1, 2]),
        ] {
            let mut s = GeminiStreamState::default();
            let mut chunk =
                json!({"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"STOP"}]});
            if !metadata.is_null() {
                chunk["usageMetadata"] = metadata.clone();
            }
            let ev = decode_chunk(&chunk, &mut s, &StreamDecodePolicy::default());
            assert!(usages(&ev).is_empty(), "{metadata}: {ev:?}");
            assert_eq!(
                ev.last(),
                Some(&StreamEvent::Done {
                    reason: StopReason::Stop
                })
            );
        }
    }

    #[test]
    fn blocked_prompt_becomes_error() {
        let mut s = GeminiStreamState::default();
        let ev = decode_chunk(
            &json!({"promptFeedback":{"blockReason":"SAFETY"}}),
            &mut s,
            &StreamDecodePolicy::default(),
        );
        assert!(matches!(
            ev[0],
            StreamEvent::Error {
                reason: ErrorReason::Rejected,
                ..
            }
        ));
    }
}
