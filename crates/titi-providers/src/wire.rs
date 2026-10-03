//! Family transports: one SSE pump shared by all endpoint families, with a
//! per-family HTTP request shape. Dispatch is by [`ApiKind`], never provider
//! name.

use futures::{Stream, StreamExt};
use serde_json::Value;
use smol_str::SmolStr;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use crate::compat::StreamDecodePolicy;
use crate::creds::{CredKind, Credential};
use crate::http::{BodyChunk, HttpFetch, HttpRequest, ReqwestFetch};
use crate::sse::{SseDecoder, SseFrame};
use crate::stream::{ErrorReason, StopReason, StreamEvent};
use crate::transport::{
    ApiKind, EventStream, RequestCtx, Role, Transport, TransportError, WatchdogConfig, WireRequest,
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

fn openai_messages_wire(req: &WireRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.messages.len() + 1);
    if let Some(sys) = &req.system {
        out.push(serde_json::json!({"role": "system", "content": sys.as_str()}));
    }
    for m in &req.messages {
        out.push(serde_json::json!({"role": m.role.openai_role(), "content": m.content.as_str()}));
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
/// [`ChatMessage`](crate::transport::ChatMessage) carries the call's name and
/// id but not its arguments — `execute_tools` records the calls without them —
/// so the replayed item carries `""`, which is what the API itself puts on a
/// call item whose arguments have not streamed yet (`output_item.added`).
///
/// A tool result carries no call id either, but `execute_tools` appends one
/// result per call in the order it recorded the calls, so each result takes
/// the oldest still-unmatched call id of the assistant message in front of it.
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
                        "arguments": "",
                    }));
                }
            }
            Role::Tool => match pending.pop_front() {
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
fn anthropic_messages_wire(req: &WireRequest, folded: usize) -> Vec<Value> {
    let last = req.messages.len().saturating_sub(1);
    req.messages
        .iter()
        .enumerate()
        .skip(folded)
        .map(|(index, m)| {
            // A system message further down is a compaction digest, and it
            // belongs where it sits in the history. The role does not exist
            // in this API, so it travels as user text rather than vanishing.
            let role = m.role.anthropic_role().unwrap_or("user");
            let mut block = serde_json::json!({"type": "text", "text": m.content.as_str()});
            if index == last {
                block["cache_control"] = ephemeral();
            }
            serde_json::json!({"role": role, "content": [block]})
        })
        .collect()
}

fn gemini_contents_wire(req: &WireRequest, folded: usize) -> Vec<Value> {
    req.messages
        .iter()
        .skip(folded)
        .map(|m| match m.role {
            Role::Assistant => serde_json::json!(
                {"role": "model", "parts": [{"text": m.content.as_str()}]}
            ),
            // User, Tool, and a compaction digest that sits mid-history all
            // travel as user turns; this API has no other inbound role.
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
/// count to follow, then goes out with or without it.
pub fn sse_event_stream(
    body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>>,
    api: ApiKind,
    policy: StreamDecodePolicy,
    grace: std::time::Duration,
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
            grace,
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
        let next = if state.held.is_some() {
            match tokio::time::timeout(state.grace, state.body.next()).await {
                Ok(next) => next,
                // The server keeps the response open after the finish; the
                // answer is complete without the count.
                Err(_) => {
                    state.done = true;
                    continue;
                }
            }
        } else {
            state.body.next().await
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
}

impl FamilyTransport {
    pub fn new(api: ApiKind, base_url: impl Into<SmolStr>, fetch: Arc<dyn HttpFetch>) -> Self {
        Self {
            api,
            base_url: base_url.into(),
            fetch,
        }
    }

    pub fn with_default_fetch(
        api: ApiKind,
        base_url: impl Into<SmolStr>,
    ) -> Result<Self, TransportError> {
        Ok(Self {
            api,
            base_url: base_url.into(),
            fetch: Arc::new(ReqwestFetch::new()?),
        })
    }
}

#[async_trait::async_trait]
impl Transport for FamilyTransport {
    fn api(&self) -> ApiKind {
        self.api
    }

    fn watchdog(&self) -> WatchdogConfig {
        WatchdogConfig::default()
    }

    async fn stream(
        &self,
        req: WireRequest,
        ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        let http_req = build_http_request(self.api, &self.base_url, &req, ctx.credential.as_ref());
        let resp = self.fetch.fetch(http_req).await?;
        if resp.status >= 400 {
            let message =
                upstream_error_message(resp.status, resp.body, ctx.credential.as_ref()).await;
            return Err(if resp.status == 429 || resp.status >= 500 {
                TransportError::Retryable {
                    status: Some(resp.status),
                    message,
                }
            } else {
                TransportError::Fatal {
                    status: Some(resp.status),
                    message,
                }
            });
        }
        Ok(Box::pin(sse_event_stream(
            resp.body,
            self.api,
            StreamDecodePolicy::default(),
            self.watchdog().post_finish_grace,
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
            },
            ChatMessage {
                role: Role::Assistant,
                content: "hello".into(),
                tool_calls: Vec::new(),
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
        });
        r
    }

    fn body_of(api: ApiKind, r: &WireRequest) -> Value {
        let hr = build_http_request(api, "http://x/v1", r, None);
        serde_json::from_slice(hr.body.as_ref().expect("body")).expect("json")
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
                },
                crate::stream::ToolCallRef {
                    call_id: "call_b".into(),
                    name: "read".into(),
                },
            ],
        });
        r.messages.push(ChatMessage {
            role: Role::Tool,
            content: "OUTPUT-A".into(),
            tool_calls: Vec::new(),
        });
        r.messages.push(ChatMessage {
            role: Role::Tool,
            content: "OUTPUT-B".into(),
            tool_calls: Vec::new(),
        });
        // A result whose call was folded out of the history keeps its text
        // under a role the API accepts.
        r.messages.push(ChatMessage {
            role: Role::Tool,
            content: "OUTPUT-ORPHAN".into(),
            tool_calls: Vec::new(),
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
            }],
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
            },
            ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: Vec::new(),
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
            },
            ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: Vec::new(),
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
            },
            ChatMessage {
                role: Role::User,
                content: "first".into(),
                tool_calls: Vec::new(),
            },
            ChatMessage {
                role: Role::System,
                content: "3 earlier message(s) folded".into(),
                tool_calls: Vec::new(),
            },
            ChatMessage {
                role: Role::User,
                content: "second".into(),
                tool_calls: Vec::new(),
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
            },
            ChatMessage {
                role: Role::System,
                content: "and nothing else was said".into(),
                tool_calls: Vec::new(),
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
        })
        .chain(history.iter().map(|(role, text)| ChatMessage {
            role: *role,
            content: (*text).into(),
            tool_calls: Vec::new(),
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
}
