//! Family transports: one SSE pump shared by all endpoint families, with a
//! per-family HTTP request shape. Dispatch is by [`ApiKind`], never provider
//! name.

use futures::{Stream, StreamExt};
use serde_json::Value;
use smol_str::SmolStr;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::compat::StreamDecodePolicy;
use crate::creds::{CredKind, Credential};
use crate::http::{BodyChunk, HttpFetch, HttpRequest, ReqwestFetch};
use crate::sse::{SseDecoder, SseFrame};
use crate::stream::{ErrorReason, StopReason, StreamEvent};
use crate::transport::{
    ApiKind, EventStream, RequestCtx, Role, StallPhase, Transport, TransportError, WatchdogConfig,
    WireRequest,
};

// ---------------------------------------------------------------------------
// Wire serialization (normalized → family request)
// ---------------------------------------------------------------------------

impl Role {
    fn openai_role(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    fn anthropic_role(&self) -> Option<&'static str> {
        match self {
            Role::System => None, // system goes to the top-level field
            Role::User | Role::Tool => Some("user"),
            Role::Assistant => Some("assistant"),
        }
    }
}

/// The longest a provider may ask this client to wait.
///
/// Past it the attempt fails with the provider's own number in the message: a
/// `retry-after: 600` is not a pause a turn can sit through, and holding one
/// would look like a hang.
pub const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// The wait a `retry-after`-family header asks for, in the order the families
/// document them.
///
/// `retry-after-ms` (Anthropic and OpenAI both send it beside the seconds
/// form), then `retry-after` — seconds, or an HTTP-date, which is what a
/// gateway in front of a provider tends to send — then OpenAI's
/// `x-ratelimit-reset-requests` / `-tokens`, whose values are durations like
/// `1s`, `20ms` or `6m0s`. Nothing parseable means nothing asked for.
fn retry_after_of(headers: &[(SmolStr, SmolStr)]) -> Option<std::time::Duration> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    if let Some(ms) = header("retry-after-ms").and_then(|value| value.trim().parse::<u64>().ok()) {
        return Some(std::time::Duration::from_millis(ms));
    }
    if let Some(value) = header("retry-after") {
        let value = value.trim();
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(std::time::Duration::from_secs(seconds));
        }
        if let Some(at) = parse_http_date(value) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0);
            return Some(std::time::Duration::from_secs(at.saturating_sub(now)));
        }
    }
    for name in ["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens"] {
        if let Some(wait) = header(name).and_then(parse_duration_text) {
            return Some(wait);
        }
    }
    None
}

/// `1s`, `20ms`, `6m0s` — the shape OpenAI's reset headers use.
fn parse_duration_text(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let mut total = std::time::Duration::ZERO;
    let mut number = String::new();
    let mut saw_unit = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        saw_unit = true;
        total += match c {
            'h' => std::time::Duration::from_secs_f64(value * 3600.0),
            // `ms` is milliseconds; a bare `m` is minutes. The families send
            // both.
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                std::time::Duration::from_millis(value as u64)
            }
            'm' => std::time::Duration::from_secs_f64(value * 60.0),
            's' => std::time::Duration::from_secs_f64(value),
            'u' | 'µ' => std::time::Duration::from_micros(value as u64),
            'n' => std::time::Duration::from_nanos(value as u64),
            _ => return None,
        };
    }
    // A bare number of milliseconds is what the header sends when it has no
    // unit at all.
    if !number.is_empty() {
        let value: f64 = number.parse().ok()?;
        total += std::time::Duration::from_millis(value as u64);
    }
    (saw_unit || total > std::time::Duration::ZERO).then_some(total)
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) as seconds since the
/// epoch. The one date shape `retry-after` documents; anything else is `None`.
fn parse_http_date(text: &str) -> Option<u64> {
    let rest = text.split_once(", ").map(|(_, rest)| rest).unwrap_or(text);
    let mut parts = rest.split_whitespace();
    let day: u64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 || day == 0 || day > 31 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm), then the time of day.
    let (y, m) = if month <= 2 { (year - 1, month + 12) } else { (year, month) };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

/// Whether a rejection is the request being longer than the model's window.
///
/// Each family says it its own way, and all three say it in words: OpenAI's
/// code is `context_length_exceeded`, Anthropic's message reads "prompt is too
/// long", Gemini answers `INVALID_ARGUMENT` about tokens. The message carries
/// the provider's own words; this is what a caller can act on.
fn context_length_rejection(api: ApiKind, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    match api {
        ApiKind::OpenAiCompletions | ApiKind::OpenAiResponses => {
            lower.contains("context_length_exceeded")
                || lower.contains("maximum context length")
                || lower.contains("too many tokens")
        }
        ApiKind::AnthropicMessages => {
            lower.contains("prompt is too long") || lower.contains("context length")
        }
        ApiKind::GeminiGenerateContent => {
            lower.contains("invalid_argument") && lower.contains("token")
                || lower.contains("input token count")
                || lower.contains("exceeds the maximum")
        }
    }
}

/// The completions family's messages, tool calls included.
///
/// An assistant message that called tools must carry them: a `tool` message
/// with no preceding `tool_calls` is a result answering a call the provider
/// was never shown, which a strict OpenAI-compatible endpoint rejects outright
/// and a lenient one answers as if the model had called nothing. The same
/// pairing the Responses path does applies here: a tool result carries no call
/// id of its own, but `execute_tools` appends one result per call in the order
/// it recorded them, so each result takes the oldest still-unmatched call id
/// of the assistant message in front of it.
fn openai_messages_wire(req: &WireRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.messages.len() + 1);
    if let Some(sys) = &req.system {
        out.push(serde_json::json!({"role": "system", "content": sys.as_str()}));
    }
    let mut pending: VecDeque<SmolStr> = VecDeque::new();
    for m in &req.messages {
        match m.role {
            Role::Assistant if !m.tool_calls.is_empty() => {
                pending.clear();
                pending.extend(m.tool_calls.iter().map(|call| call.call_id.clone()));
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": m.content.as_str(),
                    "tool_calls": m
                        .tool_calls
                        .iter()
                        .map(|call| serde_json::json!({
                            "id": call.call_id.as_str(),
                            "type": "function",
                            "function": {
                                "name": call.name.as_str(),
                                "arguments": call.arguments.as_str(),
                            },
                        }))
                        .collect::<Vec<_>>(),
                }));
            }
            Role::Tool => {
                // The result names its call; the order fallback is only for a
                // history persisted before results carried an id.
                let id = m
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| pending.pop_front().unwrap_or_default());
                out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id.as_str(),
                    "content": m.content.as_str(),
                }));
            }
            _ => {
                out.push(
                    serde_json::json!({"role": m.role.openai_role(), "content": m.content.as_str()}),
                );
            }
        }
    }
    out
}

/// The Responses family has no `tool` role. A result travels as a
/// `{"type": "function_call_output", "call_id": …, "output": …}` item, and the
/// ChatGPT subscription backend refuses the chat shape outright (400,
/// `Invalid value: 'tool'. Supported values are: 'assistant', 'system',
/// 'developer', and 'user'`). It refuses an unpaired output too (`No tool call
/// found for function call output with call_id …`), so the call the assistant
/// made is replayed as a `function_call` item as well.
///
/// The replayed item carries the call's own arguments. A result names its call
/// through `ChatMessage::tool_call_id`; for a history persisted before that
/// field existed it falls back to the oldest still-unmatched call id of the
/// assistant message in front of it, which is the order `execute_tools` wrote
/// them in.
fn responses_input_wire(req: &WireRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.messages.len() + 1);
    if let Some(sys) = &req.system {
        out.push(serde_json::json!({"role": "system", "content": sys.as_str()}));
    }
    let mut pending: VecDeque<SmolStr> = VecDeque::new();
    for m in &req.messages {
        match m.role {
            Role::Assistant => {
                pending.clear();
                pending.extend(m.tool_calls.iter().map(|call| call.call_id.clone()));
                // A reasoning item goes back before the turn's own items, and
                // only to this family.
                for block in m
                    .thinking
                    .iter()
                    .filter(|block| block.api == crate::transport::ApiKind::OpenAiResponses)
                {
                    if block.signature.is_empty() {
                        continue;
                    }
                    let mut item = serde_json::json!({
                        "type": "reasoning",
                        "encrypted_content": block.signature,
                    });
                    if !block.id.is_empty() {
                        item["id"] = Value::String(block.id.clone());
                    }
                    if !block.text.is_empty() {
                        item["summary"] =
                            serde_json::json!([{"type": "summary_text", "text": block.text}]);
                    }
                    out.push(item);
                }
                if !m.content.is_empty() {
                    // A turn that only called tools has no prose, and an empty
                    // easy message is not part of the shape the API documents.
                    out.push(serde_json::json!({
                        "role": "assistant",
                        "content": m.content.as_str(),
                    }));
                }
                for call in &m.tool_calls {
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call.call_id.as_str(),
                        "name": call.name.as_str(),
                        "arguments": call.arguments.as_str(),
                    }));
                }
            }
            Role::Tool => match m.tool_call_id.clone().or_else(|| pending.pop_front()) {
                Some(call_id) => out.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id.as_str(),
                    "output": m.content.as_str(),
                })),
                // Only a history whose assistant call was folded away reaches
                // this; the result keeps its text under a role the API accepts
                // rather than as an output item it would reject.
                None => out.push(serde_json::json!({
                    "role": "user",
                    "content": m.content.as_str(),
                })),
            },
            other => out.push(serde_json::json!({
                "role": other.openai_role(),
                "content": m.content.as_str(),
            })),
        }
    }
    out
}

/// Anthropic and Gemini carry the system prompt in a top-level field, not in
/// the message list. The engine hands it over as a leading `Role::System`
/// message (`runtime.rs` builds it, compaction inserts its digest the same
/// way), so without folding those here they are dropped on the floor and the
/// model runs with no identity, no project rules and no repository map.
fn leading_system(req: &WireRequest) -> (Option<String>, usize) {
    let mut folded = req
        .messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    // Both APIs reject a request with no messages at all, so a conversation
    // that is nothing but system messages keeps its last one as a turn: it
    // arrives as user text instead of as the system field, which loses the
    // role but keeps the content, and a request that is merely odd beats one
    // that is rejected outright. With a single system message that is the
    // whole request, so the system field ends up empty. The engine always
    // appends the user prompt, so this guards a caller that does not rather
    // than a path the turn loop takes.
    if folded == req.messages.len() {
        folded = folded.saturating_sub(1);
    }
    let mut parts: Vec<&str> = Vec::with_capacity(folded + 1);
    if let Some(system) = &req.system {
        parts.push(system.as_str());
    }
    parts.extend(req.messages[..folded].iter().map(|m| m.content.as_str()));
    let joined = parts.join("\n\n");
    ((!joined.is_empty()).then_some(joined), folded)
}

/// Anthropic's cache breakpoint marker.
///
/// A breakpoint caches everything in front of it — `tools`, then `system`,
/// then `messages`, in that order — and up to four may be set. Whether a
/// block carries the marker is not part of what is hashed: a block cached
/// under a breakpoint on one request still hits when the next request has
/// moved its breakpoint further down, which is what makes the rolling
/// breakpoint on the newest message work.
fn ephemeral() -> Value {
    serde_json::json!({"type": "ephemeral"})
}

/// Every message travels as a one-element block array rather than as a bare
/// string: the breakpoint attaches to a block, and a shape that changed as
/// the newest message aged into history would rewrite bytes the cache has
/// already committed to.
/// Anthropic's messages carry content *blocks*: an assistant turn that called
/// tools has a `tool_use` block per call with its input, and a result is a
/// `tool_result` block naming the call it answers. Writing either as plain
/// text loses the call entirely, which is what this did.
///
/// A call's arguments are JSON text; the API wants an object, so they are
/// parsed. **Unparseable arguments become `{}`** and nothing else — a call
/// that never streamed valid JSON cannot be repaired into the input the model
/// meant, and an object is the only shape this field accepts.
fn anthropic_messages_wire(req: &WireRequest, folded: usize) -> Vec<Value> {
    let last = req.messages.len().saturating_sub(1);
    let mut pending: VecDeque<SmolStr> = VecDeque::new();
    req.messages
        .iter()
        .enumerate()
        .skip(folded)
        .map(|(index, m)| {
            // A system message further down is a compaction digest, and it
            // belongs where it sits in the history. The role does not exist
            // in this API, so it travels as user text rather than vanishing.
            let role = m.role.anthropic_role().unwrap_or("user");
            let mut blocks: Vec<Value> = Vec::new();
            match m.role {
                Role::Assistant => {
                    pending.clear();
                    pending.extend(m.tool_calls.iter().map(|call| call.call_id.clone()));
                    // Thinking blocks come first in the turn that produced
                    // them, and only ever to this family: another provider
                    // would not understand an Anthropic signature.
                    for block in m
                        .thinking
                        .iter()
                        .filter(|block| block.api == crate::transport::ApiKind::AnthropicMessages)
                    {
                        if !block.data.is_empty() {
                            blocks.push(serde_json::json!({
                                "type": "redacted_thinking",
                                "data": block.data,
                            }));
                        } else if !block.signature.is_empty() {
                            blocks.push(serde_json::json!({
                                "type": "thinking",
                                "thinking": block.text,
                                "signature": block.signature,
                            }));
                        }
                    }
                    if !m.content.is_empty() {
                        blocks
                            .push(serde_json::json!({"type": "text", "text": m.content.as_str()}));
                    }
                    for call in &m.tool_calls {
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": call.call_id.as_str(),
                            "name": call.name.as_str(),
                            "input": input_of(&call.arguments),
                        }));
                    }
                }
                Role::Tool => {
                    let id = m
                        .tool_call_id
                        .clone()
                        .unwrap_or_else(|| pending.pop_front().unwrap_or_default());
                    blocks.push(serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": id.as_str(),
                        "content": [{"type": "text", "text": m.content.as_str()}],
                    }));
                }
                _ => blocks.push(serde_json::json!({"type": "text", "text": m.content.as_str()})),
            }
            if blocks.is_empty() {
                blocks.push(serde_json::json!({"type": "text", "text": ""}));
            }
            if index == last
                && let Some(block) = blocks.last_mut()
            {
                block["cache_control"] = ephemeral();
            }
            serde_json::json!({"role": role, "content": blocks})
        })
        .collect()
}

/// A call's arguments as the object the Anthropic and Gemini wires want.
/// Unparseable text becomes `{}`; nothing else can be said about it.
fn input_of(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}))
}

/// Gemini's turns carry `parts`: a model turn that called tools has a
/// `functionCall` part per call with its args, and a result is a
/// `functionResponse` part named after the call it answers (this API pairs
/// them by name, not by id). Writing either as text loses the call entirely.
///
/// Arguments are JSON text and this API wants an object; unparseable text
/// becomes `{}` by the same rule as the Anthropic wire — see [`input_of`].
fn gemini_contents_wire(req: &WireRequest, folded: usize) -> Vec<Value> {
    let mut pending: VecDeque<(SmolStr, SmolStr)> = VecDeque::new();
    req.messages
        .iter()
        .skip(folded)
        .map(|m| match m.role {
            Role::Assistant => {
                pending.clear();
                pending.extend(
                    m.tool_calls
                        .iter()
                        .map(|call| (call.call_id.clone(), call.name.clone())),
                );
                let mut parts: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(serde_json::json!({"text": m.content.as_str()}));
                }
                for call in &m.tool_calls {
                    let mut part = serde_json::json!({
                        "functionCall": {
                            "name": call.name.as_str(),
                            "args": input_of(&call.arguments),
                        },
                    });
                    if !call.thought_signature.is_empty() {
                        // The signature belongs to the part, beside the call
                        // it signs.
                        part["thoughtSignature"] = Value::String(call.thought_signature.to_string());
                    }
                    parts.push(part);
                }
                if parts.is_empty() {
                    parts.push(serde_json::json!({"text": ""}));
                }
                serde_json::json!({"role": "model", "parts": parts})
            }
            Role::Tool => {
                // Named, not id'd: a result takes the name of the call it
                // answers, by id when the result has one and by order
                // otherwise.
                let name = m
                    .tool_call_id
                    .clone()
                    .and_then(|id| {
                        pending
                            .iter()
                            .find(|(call_id, _)| *call_id == id)
                            .map(|(_, name)| name.clone())
                    })
                    .or_else(|| pending.pop_front().map(|(_, name)| name))
                    .unwrap_or_default();
                serde_json::json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name.as_str(),
                            "response": {"output": m.content.as_str()},
                        },
                    }],
                })
            }
            // User, and a compaction digest that sits mid-history, travel as
            // user turns; this API has no other inbound role.
            _ => serde_json::json!(
                {"role": "user", "parts": [{"text": m.content.as_str()}]}
            ),
        })
        .collect()
}

/// Chat Completions nests the declaration under `function`.
fn openai_tools_wire(req: &WireRequest) -> Vec<Value> {
    req.tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name.as_str(),
                    "description": t.description.as_str(),
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

/// The Responses family takes the same declaration flat: `name` sits beside
/// `type`, not under a `function` object. Sending the chat shape to the
/// ChatGPT subscription backend is refused outright with `Missing required
/// parameter: 'tools[0].name'`, so the two families cannot share one
/// serializer even though the tool set is identical.
///
/// `strict` is left out: it is opt-in schema enforcement, the endpoint
/// answers 200 without it, and asking for it would reject a schema the
/// backend can otherwise accept as a hint.
fn responses_tools_wire(req: &WireRequest) -> Vec<Value> {
    req.tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name.as_str(),
                "description": t.description.as_str(),
                "parameters": t.parameters,
            })
        })
        .collect()
}

/// The breakpoint sits on the last tool: it caches the whole tool array,
/// which the engine keeps in a stable order so the prefix holds.
fn anthropic_tools_wire(req: &WireRequest) -> Vec<Value> {
    let last = req.tools.len().saturating_sub(1);
    req.tools
        .iter()
        .enumerate()
        .map(|(index, t)| {
            let mut tool = serde_json::json!({
                "name": t.name.as_str(),
                "description": t.description.as_str(),
                "input_schema": t.parameters,
            });
            if index == last {
                tool["cache_control"] = ephemeral();
            }
            tool
        })
        .collect()
}

fn gemini_tools_wire(req: &WireRequest) -> Vec<Value> {
    if req.tools.is_empty() {
        return Vec::new();
    }
    vec![serde_json::json!({
        "functionDeclarations": req.tools.iter().map(|t| serde_json::json!({
            "name": t.name.as_str(),
            "description": t.description.as_str(),
            "parameters": t.parameters,
        })).collect::<Vec<_>>()
    })]
}

/// Claude Code's OAuth inference fingerprint: the versions and profile omp
/// spoofs (`providers/anthropic.ts`, `providers/claude-code-fingerprint.ts`;
/// omp is MIT, Stencil Labs). These are protocol facts about the subscription
/// endpoint, which version-gates on the `User-Agent`/`X-Stainless-*` pair.
const CLAUDE_CODE_VERSION: &str = "2.1.280";
/// `@anthropic-ai/sdk` version bundled by that Claude Code release
/// (`claude-code-fingerprint.ts:17`); omp's Anthropic refresh rule templates
/// the same version into its user agent (`rules/auth/anthropic.kdl`).
pub(crate) const CLAUDE_CODE_SDK_VERSION: &str = "0.112.1";
/// Node version Claude Code's `X-Stainless-Runtime-Version` reports
/// (`anthropic.ts:597`).
const CLAUDE_CODE_NODE_VERSION: &str = "v26.3.0";

/// The beta profile a Claude Code agent request carries, in omp's order:
/// `claudeCodeAgentBetaDefaults` plus the thinking-gated `effort` beta and
/// `fallback-credit` (omp `anthropic.ts:243-253`).
const CLAUDE_CODE_BETAS: &[&str] = &[
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "mid-conversation-system-2026-04-07",
    "effort-2025-11-24",
    "fallback-credit-2026-06-01",
];

/// Codex CLI version the subscription backend is told we are; it gates model
/// availability on this value, on `/models?client_version=` as well as on
/// `/responses` (`pi-catalog/src/wire/codex.ts`).
pub(crate) const CODEX_CLIENT_VERSION: &str = "0.155.1";

/// The Stainless wire values for the machine this process runs on, or `None`
/// where omp's map has no honest answer. Arch and OS are facts about the host;
/// the Node runtime version omp reports is not a fact here, so it is absent.
fn stainless_arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x64"),
        "aarch64" => Some("arm64"),
        "x86" | "i686" => Some("x86"),
        _ => None,
    }
}

fn stainless_os() -> Option<&'static str> {
    match std::env::consts::OS {
        "macos" => Some("MacOS"),
        "linux" => Some("Linux"),
        "windows" => Some("Windows"),
        "freebsd" => Some("FreeBSD"),
        _ => None,
    }
}

/// The `X-Stainless-*` set a Claude Code request carries (omp
/// `anthropic.ts:590-599`).
fn push_stainless_headers(headers: &mut Vec<(SmolStr, SmolStr)>) {
    headers.push(("x-stainless-lang".into(), "js".into()));
    headers.push((
        "x-stainless-package-version".into(),
        CLAUDE_CODE_SDK_VERSION.into(),
    ));
    headers.push(("x-stainless-retry-count".into(), "0".into()));
    headers.push(("x-stainless-runtime".into(), "node".into()));
    headers.push((
        "x-stainless-runtime-version".into(),
        CLAUDE_CODE_NODE_VERSION.into(),
    ));
    headers.push(("x-stainless-timeout".into(), "600".into()));
    if let Some(arch) = stainless_arch() {
        headers.push(("x-stainless-arch".into(), arch.into()));
    }
    if let Some(os) = stainless_os() {
        headers.push(("x-stainless-os".into(), os.into()));
    }
}

/// The headers a subscription (OAuth) request carries on `anthropic-messages`:
/// `Authorization: Bearer` replaces `x-api-key`, and the Claude Code profile
/// rides along (omp `anthropic.ts:376-406`).
///
/// `accept-encoding` is the one header of that profile titi does not send:
/// the reqwest build behind [`ReqwestFetch`] enables no decompression feature,
/// so asking for a compressed body would hand the SSE pump bytes it cannot
/// read.
fn anthropic_oauth_headers(access: &str) -> Vec<(SmolStr, SmolStr)> {
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        // OAuth requests ask for JSON even though the body streams frames.
        ("accept".into(), "application/json".into()),
        (
            "user-agent".into(),
            format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)").into(),
        ),
        ("anthropic-beta".into(), CLAUDE_CODE_BETAS.join(",").into()),
        (
            "anthropic-dangerous-direct-browser-access".into(),
            "true".into(),
        ),
        ("anthropic-version".into(), "2023-06-01".into()),
        ("authorization".into(), format!("Bearer {access}").into()),
        ("x-app".into(), "cli".into()),
        // Stripped by hyper on HTTP/2, where it is illegal and unnecessary.
        ("connection".into(), "keep-alive".into()),
    ];
    push_stainless_headers(&mut headers);
    headers
}

/// Whether `base_url` is the ChatGPT subscription backend the Codex CLI
/// speaks to (`https://chatgpt.com/backend-api/codex`) rather than the
/// pay-per-token API. Only the subscription wire carries the OAuth profile,
/// and only it gates its model list on the client version.
pub(crate) fn is_chatgpt_backend(base_url: &str) -> bool {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    host.split(':')
        .next()
        .is_some_and(|host| host.eq_ignore_ascii_case("chatgpt.com"))
        && path.trim_end_matches('/').ends_with("backend-api/codex")
}

/// Claim path the Codex backend nests the ChatGPT account id under
/// (`pi-catalog/src/wire/codex.ts`).
const CODEX_JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// The `chatgpt_account_id` claim of an access token. Rows stored before titi
/// kept the account id have none, and the token itself carries it; anything
/// that is not a JWT yields `None` rather than a wrong header.
fn account_id_from_jwt(access: &str) -> Option<SmolStr> {
    let mut parts = access.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let decoded = crate::oauth::encode::base64url_decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get(CODEX_JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(SmolStr::new)
}

/// Serialize a normalized request into a family-specific HTTP request.
///
/// The auth scheme follows the credential's kind, not the family alone: the
/// same `anthropic-messages` endpoint is reached with `x-api-key` on an API
/// key and with `Authorization: Bearer` plus the Claude Code profile on a
/// subscription token.
pub fn build_http_request(
    api: ApiKind,
    base_url: &str,
    req: &WireRequest,
    credential: Option<&Credential>,
) -> HttpRequest {
    let auth_headers = |headers: &mut Vec<(SmolStr, SmolStr)>, style: &str| {
        let Some(cred) = credential else {
            return;
        };
        match style {
            "anthropic" => {
                headers.push(("x-api-key".into(), cred.access.clone()));
                headers.push(("anthropic-version".into(), "2023-06-01".into()));
            }
            "query" => {} // Gemini key appended to URL
            _ => headers.push((
                "authorization".into(),
                format!("Bearer {}", cred.access).into(),
            )),
        }
    };
    match api {
        ApiKind::OpenAiCompletions => {
            let mut headers = vec![
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "text/event-stream".into()),
            ];
            auth_headers(&mut headers, "bearer");
            let body = serde_json::json!({
                "model": req.model.as_str(),
                "messages": openai_messages_wire(req),
                "stream": true,
                // Without it a stream carries no usage at all.
                "stream_options": {"include_usage": true},
                "tools": openai_tools_wire(req),
                "max_tokens": req.max_tokens,
                "temperature": req.temperature,
            });
            HttpRequest {
                method: "POST".into(),
                url: format!("{}/chat/completions", base_url.trim_end_matches('/')).into(),
                headers,
                body: Some(body.to_string().into_bytes()),
            }
        }
        ApiKind::OpenAiResponses => {
            let codex_cred = credential
                .filter(|cred| cred.kind == CredKind::BearerToken)
                .filter(|_| is_chatgpt_backend(base_url));
            let mut headers = vec![
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "text/event-stream".into()),
            ];
            let mut body = serde_json::json!({
                "model": req.model.as_str(),
                // The Responses API's "easy input message" (`{"role": …,
                // "content": "text"}`) is its shape for a turn of prose; tool
                // results are item-shaped, which is what `responses_input_wire`
                // adds. A probe against the live endpoint answered 200 with SSE
                // deltas for the prose shape, so the prose is not reshaped here.
                "input": responses_input_wire(req),
                "stream": true,
                "tools": responses_tools_wire(req),
                "max_output_tokens": req.max_tokens,
            });
            if let Some(sys) = &req.system {
                body["instructions"] = Value::String(sys.to_string());
            }
            if let Some(cred) = codex_cred {
                headers.push((
                    "authorization".into(),
                    format!("Bearer {}", cred.access).into(),
                ));
                // The account id rides the wire as an identifier; without one
                // the header is omitted rather than guessed.
                let account_id = cred
                    .account_id
                    .clone()
                    .or_else(|| account_id_from_jwt(&cred.access));
                if let Some(account_id) = account_id {
                    headers.push(("chatgpt-account-id".into(), account_id));
                }
                headers.push(("openai-beta".into(), "responses=experimental".into()));
                // The subscription backend attributes the client by name; titi
                // names itself, the way omp names itself `omp`.
                headers.push(("originator".into(), "titi".into()));
                headers.push(("version".into(), CODEX_CLIENT_VERSION.into()));
                headers.push((
                    "user-agent".into(),
                    format!("titi/{}", crate::VERSION).into(),
                ));
                // The Codex backend stores nothing (`store: false` in omp's
                // transform), requires the encrypted reasoning payload back,
                // and rejects caller-supplied output caps.
                body["store"] = Value::Bool(false);
                body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
                if let Some(object) = body.as_object_mut() {
                    object.remove("max_output_tokens");
                }
            } else {
                auth_headers(&mut headers, "bearer");
            }
            HttpRequest {
                method: "POST".into(),
                url: format!("{}/responses", base_url.trim_end_matches('/')).into(),
                headers,
                body: Some(body.to_string().into_bytes()),
            }
        }
        ApiKind::AnthropicMessages => {
            let mut headers = vec![
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "text/event-stream".into()),
            ];
            let bearer = match credential {
                Some(cred) if cred.kind == CredKind::BearerToken => {
                    headers = anthropic_oauth_headers(&cred.access);
                    true
                }
                _ => {
                    auth_headers(&mut headers, "anthropic");
                    false
                }
            };
            let (system, folded) = leading_system(req);
            let mut body = serde_json::json!({
                "model": req.model.as_str(),
                "messages": anthropic_messages_wire(req, folded),
                "stream": true,
                "tools": anthropic_tools_wire(req),
                "max_tokens": req.max_tokens.unwrap_or(4096),
            });
            // The block form is what carries a breakpoint; with nothing to
            // say the field is left out rather than sent as an empty block,
            // which the API rejects.
            if let Some(system) = system {
                body["system"] = serde_json::json!([{
                    "type": "text",
                    "text": system,
                    "cache_control": ephemeral(),
                }]);
            }
            HttpRequest {
                method: "POST".into(),
                url: format!(
                    "{}/v1/messages{}",
                    base_url.trim_end_matches('/'),
                    if bearer { "?beta=true" } else { "" }
                )
                .into(),
                headers,
                body: Some(body.to_string().into_bytes()),
            }
        }
        ApiKind::GeminiGenerateContent => {
            let mut headers = vec![("content-type".into(), "application/json".into())];
            auth_headers(&mut headers, "query");
            let mut url = format!(
                "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
                base_url.trim_end_matches('/'),
                req.model.as_str()
            );
            if let Some(cred) = credential {
                url.push_str(&format!("&key={}", cred.access));
            }
            let (system, folded) = leading_system(req);
            let mut body = serde_json::json!({
                "contents": gemini_contents_wire(req, folded),
                "tools": gemini_tools_wire(req),
            });
            if let Some(sys) = &system {
                body["systemInstruction"] = serde_json::json!({"parts": [{"text": sys.as_str()}]});
            }
            if let Some(max) = req.max_tokens {
                body["generationConfig"] = serde_json::json!({"maxOutputTokens": max});
            }
            HttpRequest {
                method: "POST".into(),
                url: url.into(),
                headers,
                body: Some(body.to_string().into_bytes()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared SSE pump: byte stream → SseFrames → family decoder → StreamEvents
// ---------------------------------------------------------------------------

/// Per-family mutable decode state.
pub enum FamilyDecoder {
    OpenAi(crate::openai::OpenAiStreamState),
    Anthropic(crate::anthropic::AnthropicStreamState),
    Gemini(crate::gemini::GeminiStreamState),
}

struct PumpState {
    body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>>,
    decoder: SseDecoder,
    api: ApiKind,
    policy: StreamDecodePolicy,
    family: FamilyDecoder,
    /// Events decoded from frames already read, drained before polling more
    /// bytes (a single SSE frame can decode to several events).
    queued: std::collections::VecDeque<StreamEvent>,
    /// No more bytes are read.
    done: bool,
    /// The body ended or said `[DONE]`: the stream finished by agreement.
    eof: bool,
    /// The terminal event went out; nothing follows it.
    ended: bool,
    /// A `Done` decoded before any usage report, kept back while the count
    /// may still come: Chat Completions sends it in a chunk of its own after
    /// the one that carries `finish_reason`.
    held: Option<StreamEvent>,
    usage_seen: bool,
    /// How long a held `Done` waits for the rest of the stream.
    grace: std::time::Duration,
    /// How long the next chunk may be silent before the reply counts as
    /// stalled.
    idle: std::time::Duration,
    /// The model this stream belongs to, for the stall message.
    model: SmolStr,
    /// The turn's abort flag: a set flag ends the stream at the next chunk.
    aborted: Arc<AtomicBool>,
}

impl PumpState {
    /// Whether a decoded event goes out now.
    fn admit(&mut self, event: StreamEvent) -> Option<StreamEvent> {
        if self.ended {
            return None;
        }
        match &event {
            StreamEvent::Usage(_) => {
                self.usage_seen = true;
                if self.held.is_some() {
                    self.done = true;
                }
                Some(event)
            }
            // Past the finish only the count is still wanted.
            _ if self.held.is_some() => None,
            StreamEvent::Done { .. } if !self.usage_seen => {
                self.held = Some(event);
                None
            }
            _ if event.is_terminal() => {
                self.done = true;
                self.ended = true;
                Some(event)
            }
            _ => Some(event),
        }
    }

    fn decode_frame(&mut self, frame: SseFrame) {
        let SseFrame::Data { event, data } = frame else {
            return;
        };
        if data.trim() == "[DONE]" {
            self.done = true;
            self.eof = true;
            return;
        }
        let payload: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => {
                self.queued.push_back(StreamEvent::Error {
                    reason: ErrorReason::Malformed,
                    message: format!("{api}: undecodable SSE data payload", api = self.api).into(),
                });
                return;
            }
        };
        let events = match &mut self.family {
            FamilyDecoder::OpenAi(s) => match self.api {
                ApiKind::OpenAiResponses => {
                    crate::openai::decode_responses_event(&event, &payload, s, &self.policy)
                }
                _ => crate::openai::decode_completions_chunk(&payload, s, &self.policy),
            },
            FamilyDecoder::Anthropic(s) => crate::anthropic::decode_event(&event, &payload, s),
            FamilyDecoder::Gemini(s) => crate::gemini::decode_chunk(&payload, s, &self.policy),
        };
        self.queued.extend(events);
    }
}

/// Pump raw body bytes into normalized events.
///
/// A `Done` that comes before any usage report waits up to `grace` for the
/// count to follow, then goes out with or without it. Between chunks the
/// stream may be silent for at most `watchdog.idle_timeout` before the reply
/// is reported as stalled; a set abort flag ends the stream without a word.
/// The first-event timeout is applied by `FamilyTransport::stream` before
/// this pump starts.
pub fn sse_event_stream(
    body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>>,
    api: ApiKind,
    policy: StreamDecodePolicy,
    watchdog: WatchdogConfig,
    model: SmolStr,
    aborted: Arc<AtomicBool>,
) -> impl Stream<Item = StreamEvent> + Send {
    let family = match api {
        ApiKind::AnthropicMessages => FamilyDecoder::Anthropic(Default::default()),
        ApiKind::GeminiGenerateContent => FamilyDecoder::Gemini(Default::default()),
        _ => FamilyDecoder::OpenAi(crate::openai::OpenAiStreamState::new(api)),
    };
    futures::stream::unfold(
        PumpState {
            body: Box::pin(body),
            decoder: SseDecoder::new(),
            api,
            policy,
            family,
            queued: std::collections::VecDeque::new(),
            done: false,
            eof: false,
            ended: false,
            held: None,
            usage_seen: false,
            grace: watchdog.post_finish_grace,
            idle: watchdog.idle_timeout,
            model,
            aborted,
        },
        pump_step,
    )
}

async fn pump_step(mut state: PumpState) -> Option<(StreamEvent, PumpState)> {
    loop {
        if let Some(event) = state.queued.pop_front() {
            if let Some(event) = state.admit(event) {
                return Some((event, state));
            }
            continue;
        }
        if state.done {
            // A body that ends without a terminal event reads as a natural
            // stop.
            let last = state.held.take().or_else(|| {
                (state.eof && !state.ended).then_some(StreamEvent::Done {
                    reason: StopReason::Stop,
                })
            })?;
            state.ended = true;
            return Some((last, state));
        }
        let limit = if state.held.is_some() {
            state.grace
        } else {
            state.idle
        };
        let next = match read_next(&mut state.body, limit, &state.aborted).await {
            // The turn is over and nobody will read the rest; leave without a
            // terminal event, which the caller treats as a cancel.
            ReadStep::Aborted => return None,
            // The server keeps the response open after the finish; the answer
            // is complete without the count.
            ReadStep::Idle if state.held.is_some() => {
                state.done = true;
                continue;
            }
            ReadStep::Idle => {
                state.done = true;
                state.queued.push_back(StreamEvent::Error {
                    reason: ErrorReason::Connection,
                    message: stall_message(&state.model, state.idle),
                });
                continue;
            }
            ReadStep::Chunk(next) => next,
        };
        match next {
            Some(Ok(chunk)) => {
                for frame in state.decoder.feed(&chunk) {
                    if state.eof {
                        break;
                    }
                    state.decode_frame(frame);
                }
            }
            Some(Err(e)) => {
                state.done = true;
                // Lost while only the count was awaited, the connection takes
                // nothing from an answer that already finished.
                if state.held.is_none() {
                    state.queued.push_back(StreamEvent::Error {
                        reason: ErrorReason::Connection,
                        message: e.into(),
                    });
                }
            }
            None => {
                state.done = true;
                state.eof = true;
                if let Some(frame) = state.decoder.finish() {
                    state.decode_frame(frame);
                }
            }
        }
    }
}

/// How often a read re-checks the abort flag while it waits for bytes.
///
/// The flag is a plain `AtomicBool` shared with the turn; there is no wakeup
/// channel, so the read polls it on a short tick. The tick bounds how long
/// Ctrl+C can wait, and is only spent while the body is silent.
const ABORT_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// One step of waiting for a body chunk.
enum ReadStep {
    /// The body produced an item, or ended (`None`).
    Chunk(Option<Result<BodyChunk, String>>),
    /// Nothing arrived within the limit.
    Idle,
    /// The turn was cancelled while the read waited.
    Aborted,
}

/// Wait for the next body chunk, the abort flag, or the limit, whichever
/// comes first. This is the one place a silent server can be waited on, so
/// both the watchdog and Cancel resolve here rather than at the mercy of the
/// socket.
async fn read_next(
    body: &mut Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>>,
    limit: std::time::Duration,
    aborted: &AtomicBool,
) -> ReadStep {
    let read = tokio::time::timeout(limit, body.next());
    tokio::pin!(read);
    loop {
        tokio::select! {
            // A ready read wins even with `biased`: the sleep is pending and
            // is polled first only once it has already elapsed.
            biased;
            _ = tokio::time::sleep(ABORT_POLL) => {
                if aborted.load(Ordering::Relaxed) {
                    return ReadStep::Aborted;
                }
            }
            outcome = &mut read => {
                return match outcome {
                    Ok(next) => ReadStep::Chunk(next),
                    Err(_) => ReadStep::Idle,
                };
            }
        }
    }
}

/// The words for a mid-stream stall: which model, and how long it was silent.
fn stall_message(model: &str, waited: std::time::Duration) -> SmolStr {
    SmolStr::new(format!(
        "the reply from {model} stalled for {waited:?} mid-stream"
    ))
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

/// Bytes of an error body read before giving up, and characters of the
/// provider's own text kept in the failure.
const ERROR_BODY_LIMIT: usize = 8 * 1024;
const ERROR_TEXT_LIMIT: usize = 400;
/// A diagnostic body is small and arrives at once; a server that opens a
/// stream and then says nothing must not hold the turn here.
const ERROR_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The provider's own words for a rejected request.
///
/// Chat Completions, Anthropic and the Responses API all nest the reason
/// under `error.message`/`error.code`; a gateway may send a bare `message`.
/// Only that text travels: the request body is never read back into it, so no
/// request material can leak through the failure.
async fn upstream_error_message(
    status: u16,
    mut body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>>,
    credential: Option<&Credential>,
) -> SmolStr {
    let read = async {
        let mut raw = Vec::new();
        while raw.len() < ERROR_BODY_LIMIT {
            match body.next().await {
                Some(Ok(chunk)) => raw.extend(chunk),
                _ => break,
            }
        }
        raw
    };
    let raw = tokio::time::timeout(ERROR_BODY_TIMEOUT, read)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    match provider_error_text(&text) {
        Some(message) => SmolStr::new(redact_error_text(&message, credential)),
        // A body that is not JSON at all still says more than the status
        // alone, and the truncation keeps an HTML error page to one line.
        None if !text.trim().is_empty() => {
            SmolStr::new(redact_error_text(text.as_ref(), credential))
        }
        None => SmolStr::new(format!("upstream status {status}")),
    }
}

/// `error.message` plus `error.code`, or a plain top-level `message`, from a
/// provider's error body. `error` may itself be the string.
fn provider_error_text(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| error.and_then(Value::as_str))?;
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .or_else(|| value.get("code").and_then(Value::as_str));
    Some(match code {
        Some(code) => format!("{message} (code {code})"),
        None => message.to_owned(),
    })
}

/// One line, cut to a diagnostic length, with the request's own access
/// material masked the way the rest of the crate masks a secret.
///
/// A provider that echoes the credential back in its refusal — `Incorrect API
/// key provided: sk-…` — would otherwise put it on the screen.
fn redact_error_text(message: &str, credential: Option<&Credential>) -> String {
    let mut text = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(access) = credential.map(|credential| credential.access.as_str())
        // Short material would mask ordinary words in the message; a real key
        // or token is never that short.
        && access.chars().count() >= 8
        && text.contains(access)
    {
        text = text.replace(access, &crate::creds::mask_secret(access));
    }
    if text.chars().count() > ERROR_TEXT_LIMIT {
        text = text.chars().take(ERROR_TEXT_LIMIT).collect();
        text.push('…');
    }
    text
}

/// Generic family transport over an injectable [`HttpFetch`].
pub struct FamilyTransport {
    api: ApiKind,
    base_url: SmolStr,
    fetch: Arc<dyn HttpFetch>,
    watchdog: WatchdogConfig,
}

impl FamilyTransport {
    pub fn new(api: ApiKind, base_url: impl Into<SmolStr>, fetch: Arc<dyn HttpFetch>) -> Self {
        Self {
            api,
            base_url: base_url.into(),
            fetch,
            watchdog: WatchdogConfig::default(),
        }
    }

    /// Override the declared watchdog timings. The defaults are the
    /// production ones; tests give themselves short windows this way rather
    /// than by reaching for a process-global.
    pub fn with_watchdog(mut self, watchdog: WatchdogConfig) -> Self {
        self.watchdog = watchdog;
        self
    }

    pub fn with_default_fetch(
        api: ApiKind,
        base_url: impl Into<SmolStr>,
    ) -> Result<Self, TransportError> {
        let watchdog = WatchdogConfig::default();
        Ok(Self {
            api,
            base_url: base_url.into(),
            fetch: Arc::new(ReqwestFetch::with_watchdog(&watchdog)?),
            watchdog,
        })
    }
}

#[async_trait::async_trait]
impl Transport for FamilyTransport {
    fn api(&self) -> ApiKind {
        self.api
    }

    fn watchdog(&self) -> WatchdogConfig {
        self.watchdog.clone()
    }

    async fn stream(
        &self,
        req: WireRequest,
        ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        let http_req = build_http_request(self.api, &self.base_url, &req, ctx.credential.as_ref());
        let resp = self.fetch.fetch(http_req).await?;
        if resp.status >= 400 {
            // The headers are read before the body is consumed: a provider
            // that says how long to wait says it there.
            let asked = retry_after_of(&resp.headers);
            let message =
                upstream_error_message(resp.status, resp.body, ctx.credential.as_ref()).await;
            let context_too_long = context_length_rejection(self.api, &message);
            return Err(if resp.status == 429 || resp.status >= 500 {
                match asked {
                    // A wait past the cap is not a pause, it is a refusal: the
                    // turn fails with the provider's own number in the message
                    // rather than holding a person's session for minutes.
                    Some(wait) if wait > MAX_RETRY_AFTER => TransportError::Fatal {
                        status: Some(resp.status),
                        message: format!(
                            "{message} (the provider asked to wait {}s, past the {}-second cap)",
                            wait.as_secs(),
                            MAX_RETRY_AFTER.as_secs()
                        )
                        .into(),
                        context_too_long,
                    },
                    wait => TransportError::Retryable {
                        status: Some(resp.status),
                        message,
                        retry_after: wait,
                    },
                }
            } else {
                TransportError::Fatal {
                    status: Some(resp.status),
                    message,
                    context_too_long,
                }
            });
        }
        let watchdog = self.watchdog.clone();
        let model = req.model.clone();
        let aborted = Arc::clone(&ctx.aborted);
        // The first-event timeout is the one the caller can be told about in
        // typed form: a server that opens the response and then says nothing
        // is the documented stall, and `stream` is still on the stack to
        // return it. The chunk, if any, goes back in front of the body.
        let mut body = resp.body;
        let first = match read_next(&mut body, watchdog.first_event_timeout, &aborted).await {
            ReadStep::Chunk(item) => item,
            ReadStep::Aborted => {
                return Err(TransportError::Fatal {
                    status: None,
                    message: "cancelled before the stream opened".into(),
                    context_too_long: false,
                });
            }
            ReadStep::Idle => {
                return Err(TransportError::Stalled {
                    phase: StallPhase::FirstEvent,
                    model,
                    waited: watchdog.first_event_timeout,
                });
            }
        };
        let body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>> =
            Box::pin(futures::stream::iter(first).chain(body));
        Ok(Box::pin(sse_event_stream(
            body,
            self.api,
            StreamDecodePolicy::default(),
            watchdog,
            model,
            aborted,
        )))
    }
}

/// OpenAI-compatible Chat Completions transport (OpenRouter, vLLM, Ollama
/// compat endpoints, gateways). Any compatible base URL works — dispatch is
/// by [`ApiKind`].
pub type OpenAiCompatTransport = FamilyTransport;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::LadderLevel;
    use crate::mock::{MockFetch, MockFetchResponse};
    use crate::transport::ChatMessage;

    fn key(access: &str) -> Credential {
        Credential::api_key(access, LadderLevel::LoginKey)
    }

    fn bearer(access: &str) -> Credential {
        Credential {
            access: access.into(),
            kind: CredKind::BearerToken,
            account_id: None,
            level: LadderLevel::OAuth,
        }
    }

    /// Header lookup: the wire carries lower-case names, and a fingerprint
    /// test must not pass because of a spelling.
    fn header<'a>(hr: &'a HttpRequest, name: &str) -> Option<&'a str> {
        hr.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn req() -> WireRequest {
        let mut r = WireRequest::new("gpt-test");
        r.system = Some("be brief".into());
        r.messages = vec![
            ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::Assistant,
                content: "hello".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
        ];
        r.max_tokens = Some(128);
        r
    }

    #[test]
    fn openai_completions_wire_shape() {
        let r = req();
        let hr = build_http_request(
            ApiKind::OpenAiCompletions,
            "http://x/v1",
            &r,
            Some(&key("sk")),
        );
        assert!(hr.url.ends_with("/chat/completions"));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        let auth = hr
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .expect("auth");
        assert_eq!(auth.1, "Bearer sk");
    }

    /// Chat Completions reports usage in a stream only when asked; the
    /// other families report it unasked, and the Responses API refuses a
    /// `stream_options` key it does not know.
    #[test]
    fn only_chat_completions_ask_for_usage_in_the_stream() {
        let r = req();
        let chat = body_of(ApiKind::OpenAiCompletions, &r);
        assert_eq!(
            chat["stream_options"],
            serde_json::json!({"include_usage": true})
        );
        for api in [
            ApiKind::OpenAiResponses,
            ApiKind::AnthropicMessages,
            ApiKind::GeminiGenerateContent,
        ] {
            assert!(body_of(api, &r).get("stream_options").is_none(), "{api}");
        }
    }

    #[test]
    fn openai_responses_wire_shape() {
        let r = req();
        let hr = build_http_request(ApiKind::OpenAiResponses, "http://x/v1", &r, None);
        assert!(hr.url.ends_with("/responses"));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["instructions"], "be brief");
        assert!(body["input"].is_array());
    }

    fn req_with_tools() -> WireRequest {
        let mut r = req();
        r.tools = vec![tool("bash")];
        r.messages.push(ChatMessage {
            role: Role::User,
            content: "PROMPT-MARKER-9f3a".into(),
            tool_calls: Vec::new(),
            ..Default::default()
        });
        r
    }

    /// The second request of a tool round must replay the assistant's calls,
    /// and pair each result with one: a provider that sees results answering
    /// calls it was never shown rejects the request.
    #[test]
    fn completions_replays_the_assistants_tool_calls() {
        let mut req = WireRequest::new("m");
        req.messages = vec![
            ChatMessage {
                role: Role::User,
                content: "go".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::Assistant,
                content: "before".into(),
                tool_calls: vec![
                    crate::stream::ToolCallRef {
                        call_id: "call_a".into(),
                        name: "read".into(),
                        arguments: "{\"path\":\"a.rs\"}".into(),
                        ..Default::default()
                    },
                    crate::stream::ToolCallRef {
                        call_id: "call_b".into(),
                        name: "grep".into(),
                        arguments: "{\"pattern\":\"b\"}".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ChatMessage {
                role: Role::Tool,
                content: "a.rs".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::Tool,
                content: "b".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
        ];
        let body = body_of(ApiKind::OpenAiCompletions, &req);
        let messages = body["messages"].as_array().expect("messages");
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("the assistant message");
        let calls = assistant["tool_calls"]
            .as_array()
            .expect("the assistant's calls are on the wire");
        assert_eq!(calls.len(), 2, "{assistant}");
        assert_eq!(calls[0]["id"], "call_a");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(
            calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}",
            "the arguments the model asked with, not an empty string"
        );
        assert_eq!(calls[1]["id"], "call_b");
        let ids: Vec<&str> = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].as_str().expect("a tool_call_id"))
            .collect();
        assert_eq!(ids, vec!["call_a", "call_b"], "each result takes its call");
    }

    /// A session file written before calls carried arguments and results
    /// carried ids still loads: both fields default, so an old history replays
    /// through the order fallback rather than failing to parse.
    #[test]
    fn an_old_session_entry_still_loads() {
        let message: ChatMessage = serde_json::from_str(
            r#"{"role":"assistant","content":"","tool_calls":[{"call_id":"c1","name":"read"}]}"#,
        )
        .expect("an old assistant entry loads");
        assert_eq!(message.tool_calls.len(), 1);
        assert_eq!(message.tool_calls[0].arguments, "");
        assert!(message.tool_call_id.is_none());

        let tool: ChatMessage =
            serde_json::from_str(r#"{"role":"tool","content":"ok","tool_calls":[]}"#)
                .expect("an old result loads");
        assert!(tool.tool_call_id.is_none());
    }

    /// A block is only replayed to the family that signed it: an Anthropic
    /// signature must never travel to another provider, and vice versa.
    #[test]
    fn a_thinking_block_is_only_replayed_to_its_own_family() {
        let mut req = WireRequest::new("m");
        req.messages = vec![ChatMessage {
            role: Role::Assistant,
            content: "answer".into(),
            thinking: vec![
                crate::stream::ThinkingBlock {
                    text: "anthropic thought".into(),
                    signature: "anthropic-sig".into(),
                    api: ApiKind::AnthropicMessages,
                    ..Default::default()
                },
                crate::stream::ThinkingBlock {
                    text: "responses thought".into(),
                    signature: "responses-enc".into(),
                    api: ApiKind::OpenAiResponses,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];

        let anthropic = body_of(ApiKind::AnthropicMessages, &req);
        let blocks = anthropic["messages"][0]["content"].as_array().expect("blocks");
        let types: Vec<&str> = blocks
            .iter()
            .filter_map(|block| block["type"].as_str())
            .collect();
        assert!(types.contains(&"thinking"), "{anthropic}");
        assert!(
            !anthropic.to_string().contains("responses-enc"),
            "the Responses payload does not go to Anthropic: {anthropic}"
        );

        let responses = body_of(ApiKind::OpenAiResponses, &req);
        let input = responses["input"].as_array().expect("input");
        assert!(
            input.iter().any(|item| item["encrypted_content"] == "responses-enc"),
            "{responses}"
        );
        assert!(
            !responses.to_string().contains("anthropic-sig"),
            "and the Anthropic signature does not go to Responses: {responses}"
        );
    }

    fn body_of(api: ApiKind, r: &WireRequest) -> Value {
        let hr = build_http_request(api, "http://x/v1", r, None);
        serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json")
    }

    /// A 429 that says how long to wait carries that number, in every shape
    /// the families send it.
    #[test]
    fn a_rate_limit_carries_the_wait_it_asked_for() {
        let header = |name: &str, value: &str| vec![(name.into(), value.into())];
        assert_eq!(
            retry_after_of(&header("retry-after-ms", "1500")),
            Some(std::time::Duration::from_millis(1500))
        );
        assert_eq!(
            retry_after_of(&header("retry-after", "2")),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(
            retry_after_of(&header("x-ratelimit-reset-requests", "6m0s")),
            Some(std::time::Duration::from_secs(360))
        );
        assert_eq!(
            retry_after_of(&header("x-ratelimit-reset-tokens", "20ms")),
            Some(std::time::Duration::from_millis(20))
        );
        // An HTTP-date, the shape a gateway in front of a provider sends.
        let at = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").expect("the documented shape");
        assert_eq!(at, 784111777);
        // Nothing parseable is nothing asked for.
        assert_eq!(retry_after_of(&header("retry-after", "soon")), None);
        assert_eq!(retry_after_of(&[]), None);
    }

    /// A wait past the cap is a refusal, not a pause: the attempt fails with
    /// the provider's own number in the message.
    #[tokio::test]
    async fn a_wait_past_the_cap_fails_the_attempt_by_name() {
        let fetch = Arc::new(MockFetch::new(vec![Ok(
            MockFetchResponse::sse(Vec::new())
                .with_status(429)
                .with_header("retry-after", "600"),
        )]));
        let transport = FamilyTransport::new(
            ApiKind::OpenAiCompletions,
            "http://x/v1",
            Arc::clone(&fetch) as Arc<dyn HttpFetch>,
        );
        let error = transport
            .stream(WireRequest::new("m"), RequestCtx::default())
            .await
            .err()
            .expect("a 429 is an error");
        assert!(
            matches!(&error, TransportError::Fatal { .. }),
            "past the cap is fatal: {error}"
        );
        assert!(
            error.to_string().contains("600"),
            "and names the wait: {error}"
        );
        assert!(
            error.to_string().contains("cap"),
            "and the cap: {error}"
        );
    }

    /// The two OpenAI families declare a tool differently: Completions nests
    /// it under `function`, the Responses family keeps `name` flat beside
    /// `type`. The ChatGPT subscription backend refuses the chat shape with
    /// `Missing required parameter: 'tools[0].name'`, so one serializer
    /// cannot serve both.
    #[test]
    fn responses_tools_are_flat_where_completions_nest_them() {
        let r = req_with_tools();

        let chat = body_of(ApiKind::OpenAiCompletions, &r);
        assert_eq!(chat["tools"][0]["type"], "function");
        assert_eq!(chat["tools"][0]["function"]["name"], "bash");
        assert_eq!(chat["tools"][0]["function"]["description"], "a tool");
        assert_eq!(chat["tools"][0]["function"]["parameters"]["type"], "object");
        assert!(chat["tools"][0].get("name").is_none(), "{chat}");

        let responses = body_of(ApiKind::OpenAiResponses, &r);
        assert_eq!(responses["tools"][0]["type"], "function");
        assert_eq!(responses["tools"][0]["name"], "bash");
        assert_eq!(responses["tools"][0]["description"], "a tool");
        assert_eq!(responses["tools"][0]["parameters"]["type"], "object");
        assert!(
            responses["tools"][0].get("function").is_none(),
            "{responses}"
        );
        // Opt-in schema enforcement, never asked for.
        assert!(responses["tools"][0].get("strict").is_none(), "{responses}");
    }

    /// A tool round trip on the Responses family: the assistant's calls replay
    /// as `function_call` items and each result as the `function_call_output`
    /// paired with it, never as a `tool`-role message — the ChatGPT backend
    /// answers that role with 400 (`Invalid value: 'tool'. Supported values
    /// are: 'assistant', 'system', 'developer', and 'user'`) and an unpaired
    /// output with `No tool call found for function call output with call_id …`.
    #[test]
    fn responses_tool_results_are_output_items_paired_with_their_call() {
        let mut r = req_with_tools();
        r.messages.push(ChatMessage {
            role: Role::Assistant,
            content: "reading it".into(),
            tool_calls: vec![
                crate::stream::ToolCallRef {
                    call_id: "call_a".into(),
                    name: "read".into(),
                    ..Default::default()
                },
                crate::stream::ToolCallRef {
                    call_id: "call_b".into(),
                    name: "read".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        r.messages.push(ChatMessage {
            role: Role::Tool,
            content: "OUTPUT-A".into(),
            tool_calls: Vec::new(),
            ..Default::default()
        });
        r.messages.push(ChatMessage {
            role: Role::Tool,
            content: "OUTPUT-B".into(),
            tool_calls: Vec::new(),
            ..Default::default()
        });
        // A result whose call was folded out of the history keeps its text
        // under a role the API accepts.
        r.messages.push(ChatMessage {
            role: Role::Tool,
            content: "OUTPUT-ORPHAN".into(),
            tool_calls: Vec::new(),
            ..Default::default()
        });

        let body = body_of(ApiKind::OpenAiResponses, &r);
        let input = body["input"].as_array().expect("input");
        assert!(input.iter().all(|item| item["role"] != "tool"), "{body}");

        let tail = &input[input.len() - 6..];
        assert_eq!(tail[0]["role"], "assistant");
        assert_eq!(tail[0]["content"], "reading it");
        assert_eq!(tail[1]["type"], "function_call");
        assert_eq!(tail[1]["call_id"], "call_a");
        assert_eq!(tail[1]["name"], "read");
        assert_eq!(tail[2]["type"], "function_call");
        assert_eq!(tail[2]["call_id"], "call_b");
        assert_eq!(tail[3]["type"], "function_call_output");
        assert_eq!(tail[3]["call_id"], "call_a");
        assert_eq!(tail[3]["output"], "OUTPUT-A");
        assert_eq!(tail[4]["type"], "function_call_output");
        assert_eq!(tail[4]["call_id"], "call_b");
        assert_eq!(tail[4]["output"], "OUTPUT-B");
        // The unpaired result is the item after the pair, as user text.
        assert_eq!(tail[5]["role"], "user");
        assert_eq!(tail[5]["content"], "OUTPUT-ORPHAN");
    }

    /// A turn that only called tools has no prose; the empty easy message it
    /// would otherwise send is not part of the documented shape.
    #[test]
    fn responses_tool_only_assistant_turn_has_no_empty_message() {
        let mut r = req();
        r.messages.push(ChatMessage {
            role: Role::Assistant,
            content: SmolStr::default(),
            tool_calls: vec![crate::stream::ToolCallRef {
                call_id: "call_a".into(),
                name: "read".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let body = body_of(ApiKind::OpenAiResponses, &r);
        let input = body["input"].as_array().expect("input");
        assert!(
            input
                .iter()
                .all(|item| !(item["role"] == "assistant" && item["content"] == "")),
            "{body}"
        );
        assert_eq!(input.last().expect("call")["type"], "function_call");
    }

    fn scripted(status: u16, body: &str) -> (Arc<MockFetch>, FamilyTransport) {
        let fetch = Arc::new(MockFetch::new(vec![Ok(MockFetchResponse::sse(vec![
            body.into(),
        ])
        .with_status(status))]));
        let transport = FamilyTransport::new(
            ApiKind::OpenAiResponses,
            "https://chatgpt.com/backend-api/codex",
            Arc::clone(&fetch) as Arc<dyn HttpFetch>,
        );
        (fetch, transport)
    }

    /// The failure of a scripted non-2xx. `expect_err` cannot be used: a
    /// successful stream is not `Debug`.
    async fn refusal(transport: &FamilyTransport, r: WireRequest) -> TransportError {
        match transport
            .stream(r, RequestCtx::with_key("sk-secret-token-abcdef"))
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("a scripted failure must not open a stream"),
        }
    }

    /// A non-2xx is the provider's answer and its body says why. Before this
    /// the screen read only `upstream status 400`, and the reason — the
    /// message the endpoint objected with — was thrown away.
    #[tokio::test]
    async fn a_rejected_request_surfaces_the_providers_message() {
        let (fetch, transport) = scripted(
            400,
            r#"{"error":{"message":"Missing required parameter: 'tools[0].name'.","param":"tools[0].name","code":"missing_required_parameter"}}"#,
        );
        let err = refusal(&transport, req_with_tools()).await;
        assert!(
            matches!(
                err,
                TransportError::Fatal {
                    status: Some(400),
                    ..
                }
            ),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("Missing required parameter: 'tools[0].name'."),
            "{text}"
        );
        assert!(text.contains("missing_required_parameter"), "{text}");
        // Neither the request's prompt nor its credential rides out with the
        // failure; only the provider's own words do.
        assert!(!text.contains("PROMPT-MARKER-9f3a"), "{text}");
        assert!(!text.contains("sk-secret-token-abcdef"), "{text}");
        // The request that earned the 400 really did carry flat tools.
        assert_eq!(fetch.last_body().expect("body")["tools"][0]["name"], "bash");
    }

    /// A rate limit is retryable and its body says for how long, so the
    /// message travels on that path too — masked where the provider echoed
    /// the credential back.
    #[tokio::test]
    async fn a_rate_limit_keeps_its_message_and_masks_the_credential() {
        let (_fetch, transport) = scripted(
            429,
            r#"{"error":{"message":"Rate limit reached for sk-secret-token-abcdef.","code":"rate_limit_exceeded"}}"#,
        );
        let err = refusal(&transport, req_with_tools()).await;
        assert!(
            matches!(
                err,
                TransportError::Retryable {
                    status: Some(429),
                    ..
                }
            ),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("Rate limit reached"), "{text}");
        assert!(text.contains("rate_limit_exceeded"), "{text}");
        assert!(!text.contains("sk-secret-token-abcdef"), "{text}");
        assert!(text.contains("…cdef"), "{text}");
    }

    /// Not every endpoint speaks the JSON error shape: a bare `message`
    /// counts, and a body that is not JSON at all is still better than the
    /// status alone — cut to one line, so an HTML page cannot flood the
    /// screen.
    #[tokio::test]
    async fn a_plain_message_and_a_non_json_body_are_both_kept() {
        let (_fetch, transport) = scripted(503, r#"{"message":"the model is loading"}"#);
        let err = refusal(&transport, req()).await;
        assert!(err.to_string().contains("the model is loading"), "{err}");

        let (_fetch, transport) = scripted(502, "<html>\n  <body>bad gateway</body>\n</html>");
        let err = refusal(&transport, req()).await;
        assert!(
            err.to_string()
                .contains("<html> <body>bad gateway</body> </html>"),
            "{err}"
        );

        // An empty body leaves the status, which is all there is to say.
        let (_fetch, transport) = scripted(500, "");
        let err = refusal(&transport, req()).await;
        assert!(err.to_string().contains("upstream status 500"), "{err}");
    }

    #[test]
    fn anthropic_wire_shape() {
        let r = req();
        let hr = build_http_request(ApiKind::AnthropicMessages, "http://x", &r, Some(&key("k")));
        assert!(hr.url.ends_with("/v1/messages"));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["max_tokens"], 128);
        assert!(
            body["messages"]
                .as_array()
                .expect("msgs")
                .iter()
                .all(|m| m["role"] != "system")
        );
        let key = hr
            .headers
            .iter()
            .find(|(k, _)| k == "x-api-key")
            .expect("key");
        assert_eq!(key.1, "k");
        let ver = hr
            .headers
            .iter()
            .find(|(k, _)| k == "anthropic-version")
            .expect("ver");
        assert_eq!(ver.1, "2023-06-01");
    }

    #[test]
    fn gemini_wire_shape() {
        let r = req();
        let hr = build_http_request(
            ApiKind::GeminiGenerateContent,
            "http://x",
            &r,
            Some(&key("gk")),
        );
        assert!(hr.url.contains(":streamGenerateContent?alt=sse"));
        assert!(hr.url.contains("key=gk"));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 128);
    }

    /// The engine never fills `WireRequest::system`: it puts the system
    /// prompt at the head of the messages. Anthropic has no system role, so
    /// before this was folded the identity, the project rules, the skills and
    /// the repository map were dropped and the model ran blind.
    #[test]
    fn a_leading_system_message_reaches_anthropic() {
        let mut r = WireRequest::new("claude-test");
        r.messages = vec![
            ChatMessage {
                role: Role::System,
                content: "you are titi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
        ];
        let hr = build_http_request(ApiKind::AnthropicMessages, "http://x", &r, Some(&key("k")));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["system"][0]["text"], "you are titi");
        assert_eq!(body["messages"].as_array().expect("msgs").len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn a_leading_system_message_reaches_gemini() {
        let mut r = WireRequest::new("gemini-test");
        r.messages = vec![
            ChatMessage {
                role: Role::System,
                content: "you are titi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
        ];
        let hr = build_http_request(ApiKind::GeminiGenerateContent, "http://x", &r, None);
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "you are titi"
        );
        assert_eq!(body["contents"].as_array().expect("contents").len(), 1);
        assert_eq!(body["contents"][0]["role"], "user");
    }

    /// Compaction replaces the folded prefix with a digest it marks as a
    /// system message. It sits inside the history, not at its head, so it
    /// travels as a user turn rather than disappearing.
    #[test]
    fn a_compaction_digest_inside_the_history_is_not_dropped() {
        let mut r = WireRequest::new("claude-test");
        r.messages = vec![
            ChatMessage {
                role: Role::System,
                content: "you are titi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::User,
                content: "first".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::System,
                content: "3 earlier message(s) folded".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::User,
                content: "second".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
        ];
        let hr = build_http_request(ApiKind::AnthropicMessages, "http://x", &r, Some(&key("k")));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["system"][0]["text"], "you are titi");
        let sent: Vec<&str> = body["messages"]
            .as_array()
            .expect("msgs")
            .iter()
            .map(|m| m["content"][0]["text"].as_str().expect("text"))
            .collect();
        assert_eq!(sent, ["first", "3 earlier message(s) folded", "second"]);
    }

    /// Folding every message away would send an empty `messages` array, which
    /// both APIs reject outright. The turn loop always adds the user prompt,
    /// so this guards the boundary rather than a path taken today.
    #[test]
    fn a_conversation_of_nothing_but_system_messages_still_has_a_turn() {
        let mut r = WireRequest::new("claude-test");
        r.messages = vec![
            ChatMessage {
                role: Role::System,
                content: "you are titi".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
            ChatMessage {
                role: Role::System,
                content: "and nothing else was said".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            },
        ];
        let hr = build_http_request(ApiKind::AnthropicMessages, "http://x", &r, Some(&key("k")));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["system"][0]["text"], "you are titi");
        assert_eq!(body["messages"].as_array().expect("msgs").len(), 1);
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "and nothing else was said"
        );

        let hr = build_http_request(ApiKind::GeminiGenerateContent, "http://x", &r, None);
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["contents"].as_array().expect("contents").len(), 1);
    }

    /// The degenerate end of the same guard: one system message and nothing
    /// else. Its content has to survive, and the only place left for it is a
    /// user turn — an empty `messages` array would be rejected and inventing
    /// a turn to keep it company would put words in the user's mouth.
    #[test]
    fn a_single_system_message_arrives_as_the_turn_rather_than_vanishing() {
        let mut r = WireRequest::new("claude-test");
        r.messages = vec![ChatMessage {
            role: Role::System,
            content: "you are titi".into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }];
        let hr = build_http_request(ApiKind::AnthropicMessages, "http://x", &r, Some(&key("k")));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        // Nothing is left for the system field, and an empty block array is
        // not a request Anthropic accepts, so the field is simply absent.
        assert!(body.get("system").is_none(), "{body}");
        assert_eq!(body["messages"].as_array().expect("msgs").len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "you are titi");
    }

    #[test]
    fn openrouter_style_omits_max_tokens_when_absent() {
        let mut r = WireRequest::new("m");
        r.messages = vec![ChatMessage {
            role: Role::User,
            content: "x".into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }];
        let hr = build_http_request(ApiKind::OpenAiCompletions, "http://x", &r, None);
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert!(body.get("max_tokens").map(Value::is_null).unwrap_or(true));
    }

    fn tool(name: &str) -> crate::transport::ToolSpec {
        crate::transport::ToolSpec {
            name: name.into(),
            description: "a tool".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn anthropic_body(r: &WireRequest) -> Value {
        let hr = build_http_request(ApiKind::AnthropicMessages, "http://x", r, Some(&key("k")));
        serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json")
    }

    /// Strips every breakpoint marker. Anthropic hashes the blocks, not the
    /// markers: a block cached behind a breakpoint on one request still hits
    /// when the next request has moved its breakpoint further down. Without
    /// that, a rolling breakpoint could never be compared across turns.
    fn without_breakpoints(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(without_breakpoints).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| key.as_str() != "cache_control")
                    .map(|(key, item)| (key.clone(), without_breakpoints(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn turn(system: &str, tools: &[&str], history: &[(Role, &str)]) -> WireRequest {
        let mut r = WireRequest::new("claude-test");
        r.tools = tools.iter().map(|name| tool(name)).collect();
        r.messages = std::iter::once(ChatMessage {
            role: Role::System,
            content: system.into(),
            tool_calls: Vec::new(),
            ..Default::default()
        })
        .chain(history.iter().map(|(role, text)| ChatMessage {
            role: *role,
            content: (*text).into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }))
        .collect();
        r
    }

    /// Three of the four breakpoints Anthropic allows: the last tool, the
    /// system field, and the newest message. Everything in front of each one
    /// is what gets cached.
    #[test]
    fn anthropic_marks_tools_system_and_the_newest_message() {
        let r = turn(
            "identity",
            &["bash", "read"],
            &[
                (Role::User, "first"),
                (Role::Assistant, "answer"),
                (Role::User, "second"),
            ],
        );
        let body = anthropic_body(&r);

        let tools = body["tools"].as_array().expect("tools");
        assert!(tools[0].get("cache_control").is_none(), "{body}");
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let messages = body["messages"].as_array().expect("msgs");
        assert_eq!(messages.len(), 3);
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        assert!(messages[1]["content"][0].get("cache_control").is_none());
        assert_eq!(
            messages[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        let markers = body.to_string().matches("cache_control").count();
        assert!(markers <= 4, "{markers} breakpoints, the cap is 4");
    }

    /// The whole point of the cascade: everything the previous turn sent is
    /// still there, byte for byte, in front of the message this turn adds.
    /// One changed byte anywhere behind the breakpoint and the provider
    /// reprocesses the entire conversation at full price.
    #[test]
    fn a_turn_is_a_byte_prefix_of_the_next_one() {
        let first = turn(
            "identity",
            &["bash", "read"],
            &[(Role::User, "<recall/>\n\nfirst")],
        );
        let second = turn(
            "identity",
            &["bash", "read"],
            &[
                (Role::User, "<recall/>\n\nfirst"),
                (Role::Assistant, "answer"),
                (Role::User, "<recall rebuilt/>\n\nsecond"),
            ],
        );
        let before = anthropic_body(&first);
        let after = anthropic_body(&second);

        // Tools and the system field are hashed before any message, so they
        // have to match exactly, markers included.
        assert_eq!(before["tools"], after["tools"]);
        assert_eq!(before["system"], after["system"]);

        let before_messages = without_breakpoints(&before["messages"]);
        let after_messages = without_breakpoints(&after["messages"]);
        let before_bytes = before_messages.to_string();
        let after_bytes = after_messages.to_string();
        let shared = before_bytes
            .strip_suffix(']')
            .expect("a serialized array ends with ]");
        assert!(
            after_bytes.starts_with(shared),
            "the second turn rewrote the first one:\n{before_bytes}\n{after_bytes}"
        );
        // And the only thing behind that prefix is the new message.
        assert_eq!(
            after_messages.as_array().expect("msgs").len(),
            before_messages.as_array().expect("msgs").len() + 2
        );
    }

    /// A subscription token authenticates the same endpoint differently: the
    /// bearer replaces `x-api-key` and the Claude Code profile rides along.
    /// omp `providers/anthropic.ts:376-406`, betas from `:226-253`.
    #[test]
    fn anthropic_bearer_sends_the_claude_code_profile() {
        let r = req();
        let hr = build_http_request(
            ApiKind::AnthropicMessages,
            "https://api.anthropic.com",
            &r,
            Some(&bearer("sk-ant-oat-xyz")),
        );
        assert_eq!(hr.url, "https://api.anthropic.com/v1/messages?beta=true");
        assert_eq!(header(&hr, "authorization"), Some("Bearer sk-ant-oat-xyz"));
        assert_eq!(header(&hr, "x-api-key"), None);
        assert_eq!(header(&hr, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(header(&hr, "accept"), Some("application/json"));
        assert_eq!(
            header(&hr, "anthropic-dangerous-direct-browser-access"),
            Some("true")
        );
        assert_eq!(header(&hr, "x-app"), Some("cli"));
        assert_eq!(
            header(&hr, "user-agent"),
            Some("claude-cli/2.1.280 (external, cli)")
        );
        assert_eq!(
            header(&hr, "anthropic-beta"),
            Some(
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 effort-2025-11-24,fallback-credit-2026-06-01"
            )
        );
        // The whole Stainless set is pinned; arch and OS describe this host
        // and are the only two that may be absent.
        assert_eq!(header(&hr, "x-stainless-lang"), Some("js"));
        assert_eq!(header(&hr, "x-stainless-package-version"), Some("0.112.1"));
        assert_eq!(header(&hr, "x-stainless-retry-count"), Some("0"));
        assert_eq!(header(&hr, "x-stainless-runtime"), Some("node"));
        assert_eq!(header(&hr, "x-stainless-timeout"), Some("600"));
        assert_eq!(header(&hr, "x-stainless-runtime-version"), Some("v26.3.0"));
        assert_eq!(header(&hr, "connection"), Some("keep-alive"));
    }

    /// The API-key path is untouched by the OAuth branch: no bearer, no beta
    /// profile, and no `?beta=true`.
    #[test]
    fn anthropic_api_key_wire_is_unchanged() {
        let r = req();
        let hr = build_http_request(
            ApiKind::AnthropicMessages,
            "https://api.anthropic.com",
            &r,
            Some(&key("sk-ant-key")),
        );
        assert_eq!(hr.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(header(&hr, "x-api-key"), Some("sk-ant-key"));
        assert_eq!(header(&hr, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(header(&hr, "authorization"), None);
        assert_eq!(header(&hr, "anthropic-beta"), None);
        assert_eq!(header(&hr, "user-agent"), None);
    }

    /// An OpenAI API key on `/responses` keeps the plain body: no `store`,
    /// no `include`, and the caller's output cap is still forwarded.
    #[test]
    fn openai_responses_api_key_wire_is_unchanged() {
        let r = req();
        let hr = build_http_request(
            ApiKind::OpenAiResponses,
            "https://api.openai.com/v1",
            &r,
            Some(&key("sk-openai")),
        );
        assert_eq!(header(&hr, "authorization"), Some("Bearer sk-openai"));
        assert_eq!(header(&hr, "chatgpt-account-id"), None);
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["max_output_tokens"], 128);
        assert!(body.get("store").is_none());
        assert!(body.get("include").is_none());
    }

    /// A subscription token on the ChatGPT backend: bearer plus account id,
    /// and a body the Codex backend accepts (`store: false`, encrypted
    /// reasoning requested, no caller-supplied output cap).
    #[test]
    fn codex_bearer_carries_the_account_id_and_its_own_body() {
        let r = req();
        let cred = bearer("sk-oat-codex").with_account_id(Some("acct-42"));
        let hr = build_http_request(
            ApiKind::OpenAiResponses,
            "https://chatgpt.com/backend-api/codex",
            &r,
            Some(&cred),
        );
        assert_eq!(hr.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(header(&hr, "authorization"), Some("Bearer sk-oat-codex"));
        assert_eq!(header(&hr, "chatgpt-account-id"), Some("acct-42"));
        assert_eq!(header(&hr, "openai-beta"), Some("responses=experimental"));
        assert_eq!(header(&hr, "originator"), Some("titi"));
        assert_eq!(header(&hr, "version"), Some("0.155.1"));
        assert_eq!(
            header(&hr, "user-agent"),
            Some(format!("titi/{}", crate::VERSION).as_str())
        );
        assert_eq!(header(&hr, "accept"), Some("text/event-stream"));
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["store"], false);
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "be brief");
        assert!(body.get("max_output_tokens").is_none(), "{body}");
    }

    /// A subscription token stored before titi kept the account id still
    /// names it: the id is in the token's claims. Without one, the header is
    /// absent rather than wrong.
    #[test]
    fn codex_account_id_falls_back_to_the_token_claim() {
        // {"https://api.openai.com/auth":{"chatgpt_account_id":"acct-jwt"}}
        let claims = crate::oauth::encode::base64url_encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-jwt"}}"#,
        );
        let token = format!("header.{claims}.signature");
        assert_eq!(account_id_from_jwt(&token).as_deref(), Some("acct-jwt"));
        assert_eq!(account_id_from_jwt("sk-plain-key"), None);
        assert_eq!(account_id_from_jwt("a.!!!.c"), None);

        let r = req();
        let hr = build_http_request(
            ApiKind::OpenAiResponses,
            "https://chatgpt.com/backend-api/codex",
            &r,
            Some(&bearer(&token)),
        );
        assert_eq!(header(&hr, "chatgpt-account-id"), Some("acct-jwt"));

        let hr = build_http_request(
            ApiKind::OpenAiResponses,
            "https://chatgpt.com/backend-api/codex",
            &r,
            Some(&bearer("sk-plain-key")),
        );
        assert_eq!(header(&hr, "chatgpt-account-id"), None);
    }

    /// The Codex profile belongs to the ChatGPT backend alone: the same
    /// bearer against the pay-per-token API is a plain bearer request.
    #[test]
    fn a_bearer_on_the_plain_openai_api_is_not_codex() {
        let r = req();
        let hr = build_http_request(
            ApiKind::OpenAiResponses,
            "https://api.openai.com/v1",
            &r,
            Some(&bearer("tok")),
        );
        assert_eq!(header(&hr, "authorization"), Some("Bearer tok"));
        assert_eq!(header(&hr, "openai-beta"), None);
        assert_eq!(header(&hr, "originator"), None);
        let body: Value = serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json");
        assert!(body.get("store").is_none());
        assert_eq!(body["max_output_tokens"], 128);
    }

    fn frame(payload: Value) -> String {
        format!("data: {payload}\n\n")
    }

    fn says(text: &str) -> String {
        let delta = serde_json::json!({"content": text});
        frame(serde_json::json!({"choices": [{"index": 0, "delta": delta}], "usage": null}))
    }

    fn finish() -> String {
        let choice = serde_json::json!({"index": 0, "delta": {}, "finish_reason": "stop"});
        frame(serde_json::json!({"choices": [choice], "usage": null}))
    }

    fn usage_chunk() -> String {
        let usage = serde_json::json!({"prompt_tokens": 321, "completion_tokens": 7});
        frame(serde_json::json!({"choices": [], "usage": usage}))
    }

    const REPORTED: crate::stream::TokenUsage = crate::stream::TokenUsage {
        prompt_tokens: 321,
        completion_tokens: 7,
        cached_tokens: 0,
    };

    /// What follows the scripted chunks of a [`TailFetch`] body.
    #[derive(Clone, Copy)]
    enum Tail {
        /// The server keeps the response open and says nothing more.
        Hang,
        /// The connection breaks.
        Fail,
    }

    /// A completions body that does not end the way [`MockFetch`]'s does.
    struct TailFetch {
        chunks: Vec<String>,
        tail: Tail,
    }

    impl HttpFetch for TailFetch {
        fn fetch<'a>(
            &'a self,
            _req: HttpRequest,
        ) -> futures::future::BoxFuture<'a, Result<crate::http::HttpResponse, TransportError>>
        {
            let head = futures::stream::iter(
                self.chunks
                    .clone()
                    .into_iter()
                    .map(|chunk| Ok(chunk.into_bytes())),
            );
            let body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>> =
                match self.tail {
                    Tail::Hang => Box::pin(head.chain(futures::stream::pending())),
                    Tail::Fail => Box::pin(head.chain(futures::stream::once(async {
                        Err("connection reset".to_owned())
                    }))),
                };
            Box::pin(async move {
                Ok(crate::http::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body,
                })
            })
        }
    }

    async fn completions_events(fetch: Arc<dyn HttpFetch>) -> Vec<StreamEvent> {
        let transport = FamilyTransport::new(ApiKind::OpenAiCompletions, "http://x/v1", fetch);
        collect_from(transport).await
    }

    /// Drive an already-built transport to its end, with a wall-clock guard so
    /// a test that hangs fails instead of wedging the suite.
    async fn collect_from(transport: FamilyTransport) -> Vec<StreamEvent> {
        let stream = match transport
            .stream(req(), RequestCtx::with_key("sk-test"))
            .await
        {
            Ok(stream) => stream,
            Err(error) => panic!("the stream did not open: {error}"),
        };
        tokio::time::timeout(std::time::Duration::from_secs(60), stream.collect())
            .await
            .expect("the stream must end on its own")
    }

    /// A body that emits its chunks with a real pause before each one, so an
    /// idle window can be exercised without a live socket.
    struct SlowFetch {
        chunks: Vec<String>,
        gap: std::time::Duration,
    }

    impl HttpFetch for SlowFetch {
        fn fetch<'a>(
            &'a self,
            _req: HttpRequest,
        ) -> futures::future::BoxFuture<'a, Result<crate::http::HttpResponse, TransportError>>
        {
            let chunks = self.chunks.clone();
            let gap = self.gap;
            Box::pin(async move {
                let body =
                    futures::stream::unfold(chunks.into_iter(), move |mut rest| async move {
                        tokio::time::sleep(gap).await;
                        rest.next()
                            .map(|chunk| (Ok::<BodyChunk, String>(chunk.into_bytes()), rest))
                    });
                Ok(crate::http::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(body),
                })
            })
        }
    }

    /// A watched transport with test-sized windows, and the model it will ask
    /// for.
    fn watched(fetch: Arc<dyn HttpFetch>, watchdog: WatchdogConfig) -> FamilyTransport {
        FamilyTransport::new(ApiKind::OpenAiCompletions, "http://x/v1", fetch)
            .with_watchdog(watchdog)
    }

    fn assert_ends_once(events: &[StreamEvent]) {
        let terminals = events.iter().filter(|e| e.is_terminal()).count();
        assert_eq!(terminals, 1, "{events:?}");
        assert!(
            events.last().is_some_and(StreamEvent::is_terminal),
            "{events:?}"
        );
    }

    /// `stream_options.include_usage` sends the count after the chunk that
    /// carries `finish_reason`; stopping at the finish lost it every time.
    #[tokio::test]
    async fn a_usage_chunk_after_the_finish_lands_before_done() {
        let fetch = Arc::new(MockFetch::sse(vec![
            says("hi"),
            finish(),
            usage_chunk(),
            "data: [DONE]\n\n".into(),
        ]));
        let events = completions_events(fetch).await;
        assert_ends_once(&events);
        assert_eq!(
            &events[events.len() - 2..],
            &[
                StreamEvent::Usage(REPORTED),
                StreamEvent::Done {
                    reason: StopReason::Stop
                }
            ]
        );
    }

    /// `[DONE]` is how an OpenAI-compatible server says the stream is over;
    /// some send it without any `finish_reason`. It is not a JSON payload to
    /// be refused.
    #[tokio::test]
    async fn the_done_sentinel_ends_the_stream_instead_of_failing_it() {
        let fetch = Arc::new(MockFetch::sse(vec![says("hi"), "data: [DONE]\n\n".into()]));
        let events = completions_events(fetch).await;
        assert_ends_once(&events);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::Stop
            })
        );
    }

    /// A server that keeps the response open after the finish must not keep
    /// the turn: the finished answer goes out once the grace window closes.
    #[tokio::test(start_paused = true)]
    async fn a_server_that_holds_the_stream_open_after_the_finish_is_let_go() {
        let started = tokio::time::Instant::now();
        let fetch = Arc::new(TailFetch {
            chunks: vec![says("hi"), finish()],
            tail: Tail::Hang,
        });
        let events = completions_events(fetch).await;
        assert_ends_once(&events);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::Stop
            })
        );
        let grace = WatchdogConfig::default().post_finish_grace;
        assert!(started.elapsed() >= grace, "{:?}", started.elapsed());
        assert!(started.elapsed() < grace * 2, "{:?}", started.elapsed());
    }

    /// Once the count is in, nothing more is awaited, even from a server
    /// that never closes.
    #[tokio::test(start_paused = true)]
    async fn a_reported_count_releases_the_finish_at_once() {
        let started = tokio::time::Instant::now();
        let fetch = Arc::new(TailFetch {
            chunks: vec![says("hi"), finish(), usage_chunk()],
            tail: Tail::Hang,
        });
        let events = completions_events(fetch).await;
        assert_ends_once(&events);
        assert_eq!(events[events.len() - 2], StreamEvent::Usage(REPORTED));
        assert!(
            started.elapsed() < WatchdogConfig::default().post_finish_grace,
            "{:?}",
            started.elapsed()
        );
    }

    /// The answer was complete when the connection broke while its count
    /// was awaited; the turn must not fail over a missing count.
    #[tokio::test]
    async fn a_connection_lost_after_the_finish_does_not_undo_the_answer() {
        let fetch = Arc::new(TailFetch {
            chunks: vec![says("hi"), finish()],
            tail: Tail::Fail,
        });
        let events = completions_events(fetch).await;
        assert_ends_once(&events);
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                reason: StopReason::Stop
            })
        );
    }

    /// A connection lost mid-answer is still a connection error.
    #[tokio::test]
    async fn a_connection_lost_before_the_finish_is_still_an_error() {
        let fetch = Arc::new(TailFetch {
            chunks: vec![says("hi")],
            tail: Tail::Fail,
        });
        let events = completions_events(fetch).await;
        assert_ends_once(&events);
        assert!(
            matches!(
                events.last(),
                Some(StreamEvent::Error {
                    reason: ErrorReason::Connection,
                    ..
                })
            ),
            "{events:?}"
        );
    }

    fn short_watchdog(first: std::time::Duration, idle: std::time::Duration) -> WatchdogConfig {
        WatchdogConfig {
            first_event_timeout: first,
            idle_timeout: idle,
            ..WatchdogConfig::default()
        }
    }

    /// The audited hang: a server opens the response and never sends a byte.
    /// The read must give up inside the configured window and say which model
    /// and how long it waited, not hold the turn until the process dies.
    #[tokio::test]
    async fn a_body_that_never_starts_stalls_at_the_first_event_timeout() {
        let window = std::time::Duration::from_millis(40);
        let fetch = Arc::new(TailFetch {
            chunks: Vec::new(),
            tail: Tail::Hang,
        });
        let transport = watched(fetch, short_watchdog(window, window));
        let started = std::time::Instant::now();
        match transport
            .stream(req(), RequestCtx::with_key("sk-test"))
            .await
        {
            Err(TransportError::Stalled {
                phase,
                model,
                waited,
            }) => {
                assert_eq!(phase, StallPhase::FirstEvent);
                assert_eq!(model, "gpt-test");
                assert_eq!(waited, window);
            }
            Ok(_) => panic!("a stalled open must not return a stream"),
            Err(other) => panic!("expected a first-event stall, got {other:?}"),
        }
        let elapsed = started.elapsed();
        assert!(elapsed >= window, "gave up too early: {elapsed:?}");
        assert!(elapsed < std::time::Duration::from_secs(1), "{elapsed:?}");
    }

    /// A slow server is not a stalled one: a chunk every 10ms stays under a
    /// 50ms idle window from open to finish, and the reply arrives whole.
    #[tokio::test]
    async fn a_chunk_inside_the_idle_window_keeps_the_stream_alive() {
        let fetch = Arc::new(SlowFetch {
            chunks: vec![says("a"), says("b"), finish()],
            gap: std::time::Duration::from_millis(10),
        });
        let transport = watched(
            fetch,
            short_watchdog(
                std::time::Duration::from_millis(500),
                std::time::Duration::from_millis(50),
            ),
        );
        let events = collect_from(transport).await;
        assert_ends_once(&events);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error { .. })),
            "{events:?}"
        );
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "ab");
    }

    /// A gap wider than the idle window ends the reply as a connection error
    /// naming the model and the waited time, rather than hanging.
    #[tokio::test]
    async fn a_gap_wider_than_the_idle_window_is_reported_as_stalled() {
        let gap = std::time::Duration::from_millis(200);
        let idle = std::time::Duration::from_millis(40);
        let fetch = Arc::new(SlowFetch {
            chunks: vec![says("a"), says("b"), finish()],
            gap,
        });
        let transport = watched(fetch, short_watchdog(gap * 4, idle));
        let events = collect_from(transport).await;
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Error {
                reason: ErrorReason::Connection,
                message: "the reply from gpt-test stalled for 40ms mid-stream".into(),
            })
        );
    }

    /// Cancel must end a read the server is holding open at the first byte:
    /// the read polls the existing abort flag and returns within the stated
    /// bound (well under a second here; the tick is 25ms).
    #[tokio::test]
    async fn cancel_ends_a_stalled_open_at_the_first_byte() {
        let fetch = Arc::new(TailFetch {
            chunks: Vec::new(),
            tail: Tail::Hang,
        });
        let transport = watched(
            fetch,
            short_watchdog(
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
            ),
        );
        let ctx = RequestCtx::with_key("sk-test");
        let flag = Arc::clone(&ctx.aborted);
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let result = transport.stream(req(), ctx).await;
        let elapsed = started.elapsed();
        canceller.await.expect("the canceller task");
        match result {
            Err(TransportError::Fatal { .. }) => {}
            Err(other) => panic!("a cancel is not a stall: {other:?}"),
            Ok(_) => panic!("a cancelled open must not return a stream"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "cancel took {elapsed:?}"
        );
    }

    /// Cancel must also end a read already inside the pump, where a server
    /// went silent after its first chunk.
    #[tokio::test]
    async fn cancel_ends_a_stalled_mid_stream_read() {
        let fetch = Arc::new(TailFetch {
            chunks: vec![says("hi")],
            tail: Tail::Hang,
        });
        let transport = watched(
            fetch,
            short_watchdog(
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(30),
            ),
        );
        let ctx = RequestCtx::with_key("sk-test");
        let flag = Arc::clone(&ctx.aborted);
        // `collect` takes the stream by value, so nothing here needs it `mut`.
        let stream = transport
            .stream(req(), ctx)
            .await
            .expect("the stream opens");
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let events: Vec<StreamEvent> =
            tokio::time::timeout(std::time::Duration::from_secs(60), stream.collect())
                .await
                .expect("cancel must end the stream");
        let elapsed = started.elapsed();
        canceller.await.expect("the canceller task");
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "cancel took {elapsed:?}"
        );
    }
}
