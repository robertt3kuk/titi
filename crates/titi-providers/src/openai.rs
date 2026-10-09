//! OpenAI-family stream decoders: Chat Completions chunks and Responses
//! lifecycle events → [`StreamEvent`] sequences.

use serde_json::Value;
use smol_str::SmolStr;

use crate::compat::StreamDecodePolicy;
use crate::sse::MarkerStripper;
use crate::stop::map_stop_reason;
use crate::stream::{BlockId, StreamEvent, TokenUsage, ToolCallRef};
use crate::transport::ApiKind;

/// Mutable decode state for one OpenAI-family stream.
pub struct OpenAiStreamState {
    started: bool,
    text_opened: bool,
    thinking_opened: bool,
    tools_opened: Vec<bool>,
    tools: Vec<OpenToolCall>,
    seen_tools: bool,
    /// Which OpenAI endpoint family this stream belongs to (stop-reason
    /// tables differ between Completions and Responses).
    family: ApiKind,
    /// DeepSeek-class leak stripping (set by policy in transports).
    pub marker_strip: Option<MarkerStripper>,
}

impl Default for OpenAiStreamState {
    fn default() -> Self {
        Self::new(ApiKind::OpenAiCompletions)
    }
}

impl OpenAiStreamState {
    /// Construct state for the given OpenAI endpoint family.
    pub fn new(family: ApiKind) -> Self {
        Self {
            started: false,
            text_opened: false,
            thinking_opened: false,
            tools_opened: Vec::new(),
            tools: Vec::new(),
            seen_tools: false,
            family,
            marker_strip: None,
        }
    }
}

#[derive(Default)]
struct OpenToolCall {
    block_id: String,
    call_id: String,
    name: String,
    /// Responses-only: the wire `item_id` of the announced output item, which
    /// is what keys an argument frame to this block when the calls interleave.
    item_id: String,
    /// Responses-only: at least one argument fragment was handed over, so a
    /// whole-arguments payload later in the stream must not be re-emitted.
    args_streamed: bool,
}

impl OpenToolCall {
    fn new(index: usize) -> Self {
        Self {
            block_id: format!("tool_{index}"),
            ..Self::default()
        }
    }
}

fn text_id() -> BlockId {
    BlockId("text".into())
}

fn thinking_id() -> BlockId {
    BlockId("thinking".into())
}

/// Decode one Chat Completions SSE chunk into 0..N events.
pub fn decode_completions_chunk(
    payload: &Value,
    state: &mut OpenAiStreamState,
    policy: &StreamDecodePolicy,
) -> Vec<StreamEvent> {
    let mut events = completions_chunk_events(payload, state, policy);
    if let Some(usage) = completions_usage(payload) {
        let at = events
            .iter()
            .position(StreamEvent::is_terminal)
            .unwrap_or(events.len());
        events.insert(at, StreamEvent::Usage(usage));
    }
    events
}

/// The count on a Chat Completions chunk: the top-level `usage` of the
/// trailing chunk `stream_options.include_usage` asks for (or of the finish
/// chunk, on gateways that put it there), else a `usage` on the choice, where
/// a few compatible servers put it instead.
fn completions_usage(payload: &Value) -> Option<TokenUsage> {
    let usage = payload
        .get("usage")
        .filter(|usage| usage.is_object())
        .or_else(|| {
            payload
                .pointer("/choices/0/usage")
                .filter(|usage| usage.is_object())
        })?;
    let prompt = usage.get("prompt_tokens")?.as_u64()?;
    let completion = usage.get("completion_tokens")?.as_u64()?;
    // DeepSeek reports its cache hits at the top level instead.
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.get("prompt_cache_hit_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage::reported(prompt, completion, cached)
}

/// The count on a Responses terminal event (`response.usage`), where input
/// already includes the cached part.
fn responses_usage(payload: &Value) -> Option<TokenUsage> {
    let usage = payload.pointer("/response/usage")?;
    let prompt = usage.get("input_tokens")?.as_u64()?;
    let completion = usage.get("output_tokens")?.as_u64()?;
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage::reported(prompt, completion, cached)
}

fn completions_chunk_events(
    payload: &Value,
    state: &mut OpenAiStreamState,
    policy: &StreamDecodePolicy,
) -> Vec<StreamEvent> {
    let mut events: Vec<StreamEvent> = Vec::new();

    // A chunk without choices is the usage-only trailer; its count is read
    // by the caller.
    let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
        return events;
    };
    let Some(choice) = choices.first() else {
        return events;
    };

    if !state.started {
        state.started = true;
        events.push(StreamEvent::Start);
    }

    let Some(delta) = choice.get("delta") else {
        return events;
    };

    // Reasoning deltas (`reasoning` / `reasoning_content`).
    let reasoning = delta
        .get("reasoning")
        .or_else(|| delta.get("reasoning_content"))
        .and_then(Value::as_str);
    if let Some(text) = reasoning {
        if !state.thinking_opened {
            state.thinking_opened = true;
            events.push(StreamEvent::ThinkingStart { id: thinking_id() });
        }
        events.push(StreamEvent::ThinkingDelta {
            id: thinking_id(),
            text: text.into(),
        });
    }

    // Visible content.
    let content = match delta.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) if policy.content_is_parts_array => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            Some(out)
        }
        _ => None,
    };
    if let Some(text) = content {
        let stripped = match &mut state.marker_strip {
            Some(s) => s.feed(&text),
            None => text,
        };
        if !stripped.is_empty() {
            if !state.text_opened {
                state.text_opened = true;
                events.push(StreamEvent::TextStart { id: text_id() });
            }
            events.push(StreamEvent::TextDelta {
                id: text_id(),
                text: stripped.into(),
            });
        }
    }

    // Streaming tool calls: indexed partial arguments.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            while state.tools.len() <= index {
                let i = state.tools.len();
                state.tools.push(OpenToolCall::new(i));
                state.tools_opened.push(false);
            }
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                state.tools[index].call_id = id.to_owned();
            }
            if let Some(name) = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                state.tools[index].name = name.to_owned();
            }
            let started_now = !state.tools_opened[index] && !state.tools[index].name.is_empty();
            if started_now {
                state.tools_opened[index] = true;
                state.seen_tools = true;
                let slot = &state.tools[index];
                events.push(StreamEvent::ToolcallStart {
                    id: BlockId(slot.block_id.clone().into()),
                    call: ToolCallRef {
                        call_id: SmolStr::from(slot.call_id.clone()),
                        name: SmolStr::from(slot.name.clone()),
                    },
                });
            }
            if let Some(args) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
            {
                // A fragment, not the accumulated buffer: consumers concatenate
                // `ToolcallDelta.json` across chunks (see the Anthropic decoder,
                // which emits `partial_json` the same way). Emitting the whole
                // buffer here would make every consumer double-count.
                events.push(StreamEvent::ToolcallDelta {
                    id: BlockId(state.tools[index].block_id.clone().into()),
                    json: args.into(),
                });
            }
        }
    }

    // Terminal finish_reason.
    if let Some(wire) = choice.get("finish_reason").and_then(Value::as_str) {
        events.extend(close_all(state, wire));
    }
    events
}

/// Emit closing events for open blocks and the terminal event, raising a
/// bare `stop` to `ToolUse` when structural tool blocks were seen.
fn close_all(state: &mut OpenAiStreamState, wire_reason: &str) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if state.thinking_opened {
        state.thinking_opened = false;
        events.push(StreamEvent::ThinkingEnd { id: thinking_id() });
    }
    // Flush any tail held by the marker stripper.
    if let Some(s) = &mut state.marker_strip {
        let tail = s.flush();
        if !tail.is_empty() {
            if !state.text_opened {
                state.text_opened = true;
                events.push(StreamEvent::TextStart { id: text_id() });
            }
            events.push(StreamEvent::TextDelta {
                id: text_id(),
                text: tail.into(),
            });
        }
    }
    if state.text_opened {
        state.text_opened = false;
        events.push(StreamEvent::TextEnd { id: text_id() });
    }
    for (i, opened) in state.tools_opened.iter_mut().enumerate() {
        if *opened {
            *opened = false;
            // No closing arguments delta: the fragments already carried
            // them, and re-sending the whole JSON would duplicate them.
            events.push(StreamEvent::ToolcallEnd {
                id: BlockId(state.tools[i].block_id.clone().into()),
            });
        }
    }
    match map_stop_reason(state.family, wire_reason) {
        crate::stop::StopMapping::Stop(reason) => {
            events.push(StreamEvent::Done {
                reason: crate::stop::promote_stop_for_tools(state.seen_tools, reason),
            });
        }
        crate::stop::StopMapping::Error(reason) => {
            events.push(StreamEvent::Error {
                reason,
                message: format!("stop reason {wire_reason:?} maps to error").into(),
            });
        }
    }
    events
}

/// The id a Responses output item is referred to by afterwards: the argument
/// frames carry it as `item_id`.
fn output_item_id(item: &Value) -> Option<&str> {
    item.get("item_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
}

/// The tool slot a Responses frame belongs to.
///
/// The `item_id` announced by `response.output_item.added` decides it: the
/// Codex backend puts no name and no call id on the argument frames, only that
/// id, and two calls may interleave their argument deltas. `output_index` is
/// the fallback for a stream that never announced the item.
fn responses_tool_slot(
    state: &mut OpenAiStreamState,
    payload: &Value,
    item: Option<&Value>,
) -> usize {
    let item_id = payload
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| item.and_then(output_item_id));
    if let Some(item_id) = item_id
        && let Some(index) = state.tools.iter().position(|t| t.item_id == item_id)
    {
        return index;
    }
    let index = match payload.get("output_index").and_then(Value::as_u64) {
        Some(index) => index as usize,
        // Nothing identifies the frame. An argument frame belongs to the call
        // in progress, so it reuses the newest slot instead of opening a
        // second block for the same call; a frame that does name an item is
        // taken as a new one.
        None if item_id.is_none() => state.tools.len().saturating_sub(1),
        None => state.tools.len(),
    };
    while state.tools.len() <= index {
        let i = state.tools.len();
        state.tools.push(OpenToolCall::new(i));
        state.tools_opened.push(false);
    }
    if let Some(item_id) = item_id {
        state.tools[index].item_id = item_id.to_owned();
    }
    index
}

/// Open the block for a slot once the call has a name. Idempotent.
fn open_tool_call(state: &mut OpenAiStreamState, index: usize) -> Option<StreamEvent> {
    if state.tools_opened[index] || state.tools[index].name.is_empty() {
        return None;
    }
    state.tools_opened[index] = true;
    state.seen_tools = true;
    Some(StreamEvent::ToolcallStart {
        id: BlockId(state.tools[index].block_id.clone().into()),
        call: ToolCallRef {
            call_id: SmolStr::from(state.tools[index].call_id.clone()),
            name: SmolStr::from(state.tools[index].name.clone()),
        },
    })
}

fn close_tool_call(state: &mut OpenAiStreamState, index: usize) -> Option<StreamEvent> {
    if std::mem::take(&mut state.tools_opened[index]) {
        Some(StreamEvent::ToolcallEnd {
            id: BlockId(state.tools[index].block_id.clone().into()),
        })
    } else {
        None
    }
}

/// Hand over arguments that arrive whole, when no fragment was streamed for
/// the block: re-sending them after fragments would make a concatenating
/// consumer duplicate the arguments.
fn finish_tool_args(
    state: &mut OpenAiStreamState,
    index: usize,
    args: Option<&str>,
) -> Option<StreamEvent> {
    let args = args.filter(|args| !args.is_empty())?;
    if state.tools[index].args_streamed {
        return None;
    }
    state.tools[index].args_streamed = true;
    Some(StreamEvent::ToolcallDelta {
        id: BlockId(state.tools[index].block_id.clone().into()),
        json: args.into(),
    })
}

/// Decode one Responses-API SSE event (name comes from `event:` line).
pub fn decode_responses_event(
    event: &str,
    payload: &Value,
    state: &mut OpenAiStreamState,
    _policy: &StreamDecodePolicy,
) -> Vec<StreamEvent> {
    let typ = payload.get("type").and_then(Value::as_str).unwrap_or(event);
    let mut events = Vec::new();
    match typ {
        "response.created" => {
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
        }
        "response.output_text.delta" => {
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
            if let Some(text) = payload.get("delta").and_then(Value::as_str) {
                if !state.text_opened {
                    state.text_opened = true;
                    events.push(StreamEvent::TextStart { id: text_id() });
                }
                events.push(StreamEvent::TextDelta {
                    id: text_id(),
                    text: text.into(),
                });
            }
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
            if let Some(text) = payload.get("delta").and_then(Value::as_str) {
                if !state.thinking_opened {
                    state.thinking_opened = true;
                    events.push(StreamEvent::ThinkingStart { id: thinking_id() });
                }
                events.push(StreamEvent::ThinkingDelta {
                    id: thinking_id(),
                    text: text.into(),
                });
            }
        }
        "response.output_item.added" => {
            let Some(item) = payload.get("item") else {
                return events;
            };
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return events;
            }
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
            // The Codex backend names the call here, once, before the argument
            // frames — which carry neither name nor call id.
            let index = responses_tool_slot(state, payload, Some(item));
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                state.tools[index].name = name.to_owned();
            }
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                state.tools[index].call_id = call_id.to_owned();
            }
            events.extend(open_tool_call(state, index));
        }
        "response.function_call_arguments.delta" => {
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
            let index = responses_tool_slot(state, payload, None);
            if !state.tools_opened[index] {
                // The first-party API names the call on the delta itself; the
                // Codex backend named it earlier in `output_item.added`, so
                // that stash is what opens the block here.
                if state.tools[index].name.is_empty()
                    && let Some(name) = payload.get("name").and_then(Value::as_str)
                {
                    state.tools[index].name = name.to_owned();
                }
                if state.tools[index].call_id.is_empty()
                    && let Some(call_id) = payload
                        .get("call_id")
                        .or_else(|| payload.get("item_id"))
                        .and_then(Value::as_str)
                {
                    state.tools[index].call_id = call_id.to_owned();
                }
                if state.tools[index].name.is_empty() {
                    return events;
                }
                events.extend(open_tool_call(state, index));
            }
            if let Some(args) = payload.get("delta").and_then(Value::as_str) {
                // A fragment, not the accumulated buffer: consumers concatenate
                // `ToolcallDelta.json` across frames.
                state.tools[index].args_streamed |= !args.is_empty();
                events.push(StreamEvent::ToolcallDelta {
                    id: BlockId(state.tools[index].block_id.clone().into()),
                    json: args.into(),
                });
            }
        }
        "response.function_call_arguments.done" => {
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
            let index = responses_tool_slot(state, payload, None);
            if let Some(args) = finish_tool_args(
                state,
                index,
                payload.get("arguments").and_then(Value::as_str),
            ) {
                events.push(args);
            }
        }
        "response.output_item.done" => {
            let Some(item) = payload.get("item") else {
                return events;
            };
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return events;
            }
            if !state.started {
                state.started = true;
                events.push(StreamEvent::Start);
            }
            let index = responses_tool_slot(state, payload, Some(item));
            if state.tools[index].name.is_empty()
                && let Some(name) = item.get("name").and_then(Value::as_str)
            {
                state.tools[index].name = name.to_owned();
            }
            if state.tools[index].call_id.is_empty()
                && let Some(call_id) = item.get("call_id").and_then(Value::as_str)
            {
                state.tools[index].call_id = call_id.to_owned();
            }
            events.extend(open_tool_call(state, index));
            if let Some(args) =
                finish_tool_args(state, index, item.get("arguments").and_then(Value::as_str))
            {
                events.push(args);
            }
            if let Some(end) = close_tool_call(state, index) {
                events.push(end);
            }
        }
        "response.completed" | "response.failed" | "response.incomplete" => {
            let wire = match typ {
                "response.completed" => "completed",
                "response.incomplete" => "incomplete",
                _ => "failed",
            };
            events.extend(responses_usage(payload).map(StreamEvent::Usage));
            events.extend(close_all(state, wire));
        }
        _ => {}
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{StopReason, TokenUsage};
    use serde_json::json;

    fn state() -> OpenAiStreamState {
        OpenAiStreamState::default()
    }
    fn responses_state() -> OpenAiStreamState {
        OpenAiStreamState::new(ApiKind::OpenAiResponses)
    }

    #[test]
    fn completions_text_flow() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let mut ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"role":"assistant"}}]}),
            &mut s,
            &policy,
        );
        assert_eq!(ev.remove(0), StreamEvent::Start);
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"content":"Hel"}}]}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![
                StreamEvent::TextStart { id: text_id() },
                StreamEvent::TextDelta {
                    id: text_id(),
                    text: "Hel".into()
                }
            ]
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}),
            &mut s,
            &policy,
        );
        assert_eq!(ev.len(), 1);
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![
                StreamEvent::TextEnd { id: text_id() },
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    /// Two calls in one response, both in one chunk and then interleaved: the
    /// block ids must keep them apart, because a consumer that keys on the id
    /// is the only thing standing between the model and a lost call.
    #[test]
    fn completions_keeps_two_tool_calls_of_one_chunk() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_1","function":{"name":"read","arguments":""}},
                {"index":1,"id":"call_2","function":{"name":"grep","arguments":""}}
            ]}}]}),
            &mut s,
            &policy,
        );
        let starts: Vec<(BlockId, String)> = ev
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolcallStart { id, call } => {
                    Some((id.clone(), call.name.to_string()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                (BlockId("tool_0".into()), "read".to_owned()),
                (BlockId("tool_1".into()), "grep".to_owned()),
            ],
            "one start per call, each with its own block: {ev:?}"
        );

        // Interleaved arguments, second call first.
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"pattern\":\"b\"}"}}]}}]}),
            &mut s,
            &policy,
        );
        assert!(
            matches!(&ev[0], StreamEvent::ToolcallDelta { id, .. } if id == &BlockId("tool_1".into())),
            "the second call's delta carries the second call's id: {ev:?}"
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.rs\"}"}}]}}]}),
            &mut s,
            &policy,
        );
        assert!(
            matches!(&ev[0], StreamEvent::ToolcallDelta { id, .. } if id == &BlockId("tool_0".into())),
            "and the first call's delta the first call's id: {ev:?}"
        );
    }

    #[test]
    fn completions_tool_call_with_partial_args() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let _ = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}),
            &mut s,
            &policy,
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
            &mut s,
            &policy,
        );
        // A fragment, never the accumulated buffer: consumers concatenate.
        assert_eq!(
            ev,
            vec![StreamEvent::ToolcallDelta {
                id: BlockId("tool_0".into()),
                json: "{\"path\":".into()
            }]
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]}}]}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![StreamEvent::ToolcallDelta {
                id: BlockId("tool_0".into()),
                json: "\"a\"}".into()
            }]
        );

        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            &mut s,
            &policy,
        );
        // Closing emits the end only — re-sending the whole JSON would make a
        // concatenating consumer duplicate the arguments.
        assert_eq!(
            ev,
            vec![
                StreamEvent::ToolcallEnd {
                    id: BlockId("tool_0".into())
                },
                StreamEvent::Done {
                    reason: StopReason::ToolUse
                }
            ]
        );
    }

    #[test]
    fn completions_tool_arguments_concatenate_to_valid_json() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let chunks = [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}),
        ];
        let mut args = String::new();
        for chunk in &chunks {
            for event in decode_completions_chunk(chunk, &mut s, &policy) {
                if let StreamEvent::ToolcallDelta { json, .. } = event {
                    args.push_str(&json);
                }
            }
        }
        assert_eq!(args, r#"{"path":"a.rs"}"#);
        let parsed: Value = serde_json::from_str(&args).expect("concatenated arguments parse");
        assert_eq!(parsed["path"], "a.rs");
    }

    #[test]
    fn completions_stop_promoted_to_tooluse() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let _ = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{}"}}]}}]}),
            &mut s,
            &policy,
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            &mut s,
            &policy,
        );
        assert!(matches!(
            ev.last(),
            Some(StreamEvent::Done {
                reason: StopReason::ToolUse
            })
        ));
    }

    #[test]
    fn malformed_tool_args_repaired_not_panicked() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let _ = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":""}}]}}]}),
            &mut s,
            &policy,
        );
        // Garbage arguments must not panic; finalize repairs to {}.
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"garbage"}}]}}]}),
            &mut s,
            &policy,
        );
        assert!(!ev.is_empty());
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            &mut s,
            &policy,
        );
        let last = ev.last().expect("terminal");
        assert!(matches!(last, StreamEvent::Done { .. }));
    }

    #[test]
    fn usage_only_chunk_skipped() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_completions_chunk(&json!({"usage":{"total_tokens":10}}), &mut s, &policy);
        assert!(ev.is_empty());
    }

    #[test]
    fn reasoning_and_content_both_streamed() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"reasoning_content":"hmm"}}]}),
            &mut s,
            &policy,
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, StreamEvent::ThinkingDelta { .. }))
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{"content":"answer"}}]}),
            &mut s,
            &policy,
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { .. }))
        );
    }

    #[test]
    fn responses_lifecycle() {
        let mut s = responses_state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_responses_event(
            "response.created",
            &json!({"type":"response.created"}),
            &mut s,
            &policy,
        );
        assert_eq!(ev, vec![StreamEvent::Start]);
        let ev = decode_responses_event(
            "response.output_text.delta",
            &json!({"type":"response.output_text.delta","delta":"hi"}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![
                StreamEvent::TextStart { id: text_id() },
                StreamEvent::TextDelta {
                    id: text_id(),
                    text: "hi".into()
                }
            ]
        );
        let ev = decode_responses_event(
            "response.completed",
            &json!({"type":"response.completed"}),
            &mut s,
            &policy,
        );
        assert_eq!(
            ev,
            vec![
                StreamEvent::TextEnd { id: text_id() },
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    #[test]
    fn responses_failed_maps_to_error() {
        let mut s = responses_state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_responses_event(
            "response.failed",
            &json!({"type":"response.failed"}),
            &mut s,
            &policy,
        );
        assert!(matches!(ev.last(), Some(StreamEvent::Error { .. })));
    }

    #[test]
    fn unknown_responses_event_ignored() {
        let mut s = responses_state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_responses_event(
            "response.heartbeat",
            &json!({"type":"response.heartbeat"}),
            &mut s,
            &policy,
        );
        assert!(ev.is_empty());
    }

    /// Feed a scripted Responses stream frame by frame; collect every event.
    fn drive_responses(frames: &[(&str, Value)]) -> Vec<StreamEvent> {
        let mut s = responses_state();
        let policy = StreamDecodePolicy::default();
        let mut all = Vec::new();
        for (name, payload) in frames {
            all.extend(decode_responses_event(name, payload, &mut s, &policy));
        }
        all
    }

    /// Every tool block opened, in order.
    fn tool_starts(events: &[StreamEvent]) -> Vec<(BlockId, ToolCallRef)> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolcallStart { id, call } => Some((id.clone(), call.clone())),
                _ => None,
            })
            .collect()
    }

    /// The arguments a consumer assembles for one block by concatenation.
    fn args_of(events: &[StreamEvent], block: &str) -> String {
        let block = BlockId(block.into());
        let mut out = String::new();
        for e in events {
            if let StreamEvent::ToolcallDelta { id, json } = e
                && id == &block
            {
                out.push_str(json);
            }
        }
        out
    }

    fn tool_ends(events: &[StreamEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolcallEnd { .. }))
            .count()
    }

    #[test]
    fn responses_codex_order_assembles_tool_call() {
        // The ChatGPT subscription backend names the call once, on
        // `response.output_item.added`; its argument frames carry neither
        // name nor call id, only `item_id`.
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added","output_index":0,
                    "item":{"id":"fc_1","type":"function_call","status":"in_progress",
                            "name":"read","call_id":"call_1","arguments":""}
                }),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"path\":"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"\"Cargo"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":".toml\"}"}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":0,"arguments":"{\"path\":\"Cargo.toml\"}"}),
            ),
            (
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done","output_index":0,
                    "item":{"id":"fc_1","type":"function_call","status":"completed",
                            "name":"read","call_id":"call_1","arguments":"{\"path\":\"Cargo.toml\"}"}
                }),
            ),
            ("response.completed", json!({"type":"response.completed"})),
        ]);

        assert_eq!(events[0], StreamEvent::Start);
        assert_eq!(
            tool_starts(&events),
            vec![(
                BlockId("tool_0".into()),
                ToolCallRef {
                    call_id: "call_1".into(),
                    name: "read".into()
                }
            )]
        );
        assert_eq!(args_of(&events, "tool_0"), r#"{"path":"Cargo.toml"}"#);
        let parsed: Value = serde_json::from_str(&args_of(&events, "tool_0")).expect("args parse");
        assert_eq!(parsed["path"], "Cargo.toml");
        // Three fragments streamed: the whole-arguments frames add nothing.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolcallDelta { .. }))
                .count(),
            3
        );
        assert_eq!(tool_ends(&events), 1);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn responses_first_party_order_names_the_call_on_the_delta() {
        // The other shape: no `output_item.added`; every argument delta names
        // the call itself.
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","output_index":0,
                       "name":"read","item_id":"call_2","delta":"{\"path\":\"a.rs\"}"}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"a.rs\"}"}),
            ),
            ("response.completed", json!({"type":"response.completed"})),
        ]);

        assert_eq!(
            tool_starts(&events),
            vec![(
                BlockId("tool_0".into()),
                ToolCallRef {
                    call_id: "call_2".into(),
                    name: "read".into()
                }
            )]
        );
        assert_eq!(args_of(&events, "tool_0"), r#"{"path":"a.rs"}"#);
        assert_eq!(tool_ends(&events), 1);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn responses_frames_without_an_index_stay_one_block() {
        // An endpoint that sends neither `item_id` nor `output_index` on the
        // argument frames: they all belong to the call in progress.
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","name":"read","delta":"{\"path\":"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","delta":"\"a.rs\"}"}),
            ),
            ("response.completed", json!({"type":"response.completed"})),
        ]);
        assert_eq!(tool_starts(&events).len(), 1);
        assert_eq!(args_of(&events, "tool_0"), r#"{"path":"a.rs"}"#);
        assert_eq!(tool_ends(&events), 1);
    }

    #[test]
    fn responses_interleaved_tool_calls_key_on_the_item() {
        // Two calls announced before either streams, and their argument frames
        // carry no `output_index`: only `item_id` can keep them apart.
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,
                       "item":{"id":"fc_a","type":"function_call","name":"read","call_id":"call_a"}}),
            ),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":1,
                       "item":{"id":"fc_b","type":"function_call","name":"grep","call_id":"call_b"}}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"{\"pattern\":"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"path\":"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"\"x\"}"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"\"a.rs\"}"}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","item_id":"fc_b"}),
            ),
            (
                "response.function_call_arguments.done",
                json!({"type":"response.function_call_arguments.done","item_id":"fc_a"}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","item_id":"fc_b",
                       "item":{"id":"fc_b","type":"function_call","name":"grep","call_id":"call_b"}}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","item_id":"fc_a",
                       "item":{"id":"fc_a","type":"function_call","name":"read","call_id":"call_a"}}),
            ),
            ("response.completed", json!({"type":"response.completed"})),
        ]);

        assert_eq!(
            tool_starts(&events),
            vec![
                (
                    BlockId("tool_0".into()),
                    ToolCallRef {
                        call_id: "call_a".into(),
                        name: "read".into()
                    }
                ),
                (
                    BlockId("tool_1".into()),
                    ToolCallRef {
                        call_id: "call_b".into(),
                        name: "grep".into()
                    }
                ),
            ]
        );
        assert_eq!(args_of(&events, "tool_0"), r#"{"path":"a.rs"}"#);
        assert_eq!(args_of(&events, "tool_1"), r#"{"pattern":"x"}"#);
        assert_eq!(tool_ends(&events), 2);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::ToolUse
            })
        );
    }

    #[test]
    fn responses_whole_arguments_on_done_frames_still_yield_the_call() {
        // No argument deltas: the item carries the arguments whole.
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,
                       "item":{"id":"fc_1","type":"function_call","name":"read","call_id":"call_1"}}),
            ),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":0,
                       "item":{"id":"fc_1","type":"function_call","name":"read","call_id":"call_1",
                               "arguments":"{\"path\":\"Cargo.toml\"}"}}),
            ),
            ("response.completed", json!({"type":"response.completed"})),
        ]);
        assert_eq!(tool_starts(&events).len(), 1);
        assert_eq!(args_of(&events, "tool_0"), r#"{"path":"Cargo.toml"}"#);
        assert_eq!(tool_ends(&events), 1);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::ToolUse
            })
        );

        // A stream that never announced the item at all still decodes it.
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":0,
                       "item":{"id":"fc_9","type":"function_call","name":"bash","call_id":"call_9",
                               "arguments":"{\"cmd\":\"ls\"}"}}),
            ),
            ("response.completed", json!({"type":"response.completed"})),
        ]);
        assert_eq!(
            tool_starts(&events),
            vec![(
                BlockId("tool_0".into()),
                ToolCallRef {
                    call_id: "call_9".into(),
                    name: "bash".into()
                }
            )]
        );
        assert_eq!(args_of(&events, "tool_0"), r#"{"cmd":"ls"}"#);
        assert_eq!(tool_ends(&events), 1);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::ToolUse
            })
        );
    }

    fn usage(prompt: u64, completion: u64, cached: u64) -> StreamEvent {
        StreamEvent::Usage(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
        })
    }

    fn usages(events: &[StreamEvent]) -> Vec<&StreamEvent> {
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Usage(_)))
            .collect()
    }

    /// `stream_options.include_usage` puts the count in a chunk of its own
    /// after the finish, with an empty `choices`; every chunk before it says
    /// `"usage": null`.
    #[test]
    fn completions_trailing_usage_chunk_is_reported() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let ev = decode_completions_chunk(
            &json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,
                    "delta":{"content":"hi"},"finish_reason":null}],"usage":null}),
            &mut s,
            &policy,
        );
        assert!(usages(&ev).is_empty(), "{ev:?}");
        let ev = decode_completions_chunk(
            &json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,
                    "delta":{},"finish_reason":"stop"}],"usage":null}),
            &mut s,
            &policy,
        );
        assert!(usages(&ev).is_empty(), "{ev:?}");
        let ev = decode_completions_chunk(
            &json!({"id":"c1","object":"chat.completion.chunk","choices":[],
                    "usage":{"prompt_tokens":1200,"completion_tokens":34,"total_tokens":1234,
                             "prompt_tokens_details":{"cached_tokens":1024,"audio_tokens":0},
                             "completion_tokens_details":{"reasoning_tokens":0}}}),
            &mut s,
            &policy,
        );
        assert_eq!(ev, vec![usage(1200, 34, 1024)]);
    }

    /// OpenRouter-style: the count rides on the finish chunk itself, and it
    /// must land before the terminal event that ends the stream.
    #[test]
    fn completions_usage_on_the_finish_chunk_precedes_done() {
        let mut s = state();
        let policy = StreamDecodePolicy::default();
        let _ = decode_completions_chunk(
            &json!({"choices":[{"delta":{"content":"hi"}}]}),
            &mut s,
            &policy,
        );
        let ev = decode_completions_chunk(
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":40,"completion_tokens":2,"total_tokens":42}}),
            &mut s,
            &policy,
        );
        assert_eq!(
            &ev[ev.len() - 2..],
            &[
                usage(40, 2, 0),
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    /// DeepSeek counts cache hits at the top level, not in the details.
    #[test]
    fn completions_cache_hits_without_details_still_count() {
        let mut s = state();
        let ev = decode_completions_chunk(
            &json!({"choices":[],"usage":{"prompt_tokens":900,"completion_tokens":10,
                    "prompt_cache_hit_tokens":512,"prompt_cache_miss_tokens":388}}),
            &mut s,
            &StreamDecodePolicy::default(),
        );
        assert_eq!(ev, vec![usage(900, 10, 512)]);
    }

    /// A count that is not two numbers is no count: the stream goes on and
    /// still ends normally.
    #[test]
    fn completions_malformed_usage_is_absent() {
        let policy = StreamDecodePolicy::default();
        for bad in [
            json!(null),
            json!("lots"),
            json!({"prompt_tokens":"12","completion_tokens":3}),
            json!({"prompt_tokens":12}),
            json!({"prompt_tokens":-1,"completion_tokens":3}),
            json!({"prompt_tokens":1.5,"completion_tokens":3}),
            json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}),
        ] {
            let mut s = state();
            let ev = decode_completions_chunk(
                &json!({"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}],"usage":bad}),
                &mut s,
                &policy,
            );
            assert!(usages(&ev).is_empty(), "{bad}: {ev:?}");
            assert_eq!(
                ev.last(),
                Some(&StreamEvent::Done {
                    reason: StopReason::Stop
                }),
                "{bad}"
            );
        }
    }

    #[test]
    fn responses_completed_reports_usage_before_done() {
        let events = drive_responses(&[
            ("response.created", json!({"type":"response.created"})),
            (
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","delta":"hi"}),
            ),
            (
                "response.completed",
                json!({"type":"response.completed","response":{"id":"resp_1","status":"completed",
                       "usage":{"input_tokens":2000,"input_tokens_details":{"cached_tokens":1500},
                                "output_tokens":120,"output_tokens_details":{"reasoning_tokens":64},
                                "total_tokens":2120}}}),
            ),
        ]);
        assert_eq!(usages(&events), vec![&usage(2000, 120, 1500)]);
        let at = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Usage(_)))
            .expect("usage");
        assert_eq!(
            &events[at + 1..],
            &[
                StreamEvent::TextEnd { id: text_id() },
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    /// A reply cut at the output cap is still a request that was paid for.
    #[test]
    fn responses_incomplete_reports_usage_too() {
        let events = drive_responses(&[(
            "response.incomplete",
            json!({"type":"response.incomplete","response":{"status":"incomplete",
                   "usage":{"input_tokens":10,"output_tokens":4096}}}),
        )]);
        assert_eq!(usages(&events), vec![&usage(10, 4096, 0)]);
        assert!(events.last().is_some_and(StreamEvent::is_terminal));
    }

    #[test]
    fn responses_missing_or_malformed_usage_is_absent() {
        for response in [
            json!({"status":"completed"}),
            json!({"status":"completed","usage":null}),
            json!({"status":"completed","usage":{"input_tokens":"many","output_tokens":3}}),
            json!({"status":"completed","usage":{"output_tokens":3}}),
            json!("completed"),
        ] {
            let events = drive_responses(&[(
                "response.completed",
                json!({"type":"response.completed","response":response}),
            )]);
            assert!(usages(&events).is_empty(), "{response}: {events:?}");
            assert_eq!(
                events.last(),
                Some(&StreamEvent::Done {
                    reason: StopReason::Stop
                })
            );
        }
    }
}
