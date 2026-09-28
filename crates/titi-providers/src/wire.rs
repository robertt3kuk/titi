//! Family transports: one SSE pump shared by all endpoint families, with a
//! per-family HTTP request shape. Dispatch is by [`ApiKind`], never provider
//! name.

use futures::{Stream, StreamExt};
use serde_json::Value;
use smol_str::SmolStr;
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
                "input": openai_messages_wire(req),
                "stream": true,
                "tools": openai_tools_wire(req),
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
    done: bool,
}

impl PumpState {
    fn decode_frame(&mut self, frame: SseFrame) {
        let SseFrame::Data { event, data } = frame else {
            return;
        };
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
pub fn sse_event_stream(
    body: Pin<Box<dyn Stream<Item = Result<BodyChunk, String>> + Send>>,
    api: ApiKind,
    policy: StreamDecodePolicy,
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
        },
        pump_step,
    )
}

async fn pump_step(mut state: PumpState) -> Option<(StreamEvent, PumpState)> {
    loop {
        if let Some(ev) = state.queued.pop_front() {
            if ev.is_terminal() {
                state.done = true;
            }
            return Some((ev, state));
        }
        if state.done {
            return None;
        }
        match state.body.next().await {
            Some(Ok(chunk)) => {
                for frame in state.decoder.feed(&chunk) {
                    state.decode_frame(frame);
                }
            }
            Some(Err(e)) => {
                state.done = true;
                return Some((
                    StreamEvent::Error {
                        reason: ErrorReason::Connection,
                        message: e.into(),
                    },
                    state,
                ));
            }
            None => {
                state.done = true;
                if let Some(frame) = state.decoder.finish() {
                    state.decode_frame(frame);
                }
                // Fall through: next loop iteration drains decoded events or
                // emits the default Done.
                if let Some(ev) = state.queued.pop_front() {
                    return Some((ev, state));
                }
                return Some((
                    StreamEvent::Done {
                        reason: StopReason::Stop,
                    },
                    state,
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

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
        if resp.status == 429 || resp.status >= 500 {
            return Err(TransportError::Retryable {
                status: Some(resp.status),
                message: format!("upstream status {}", resp.status).into(),
            });
        }
        if resp.status >= 400 {
            return Err(TransportError::Fatal {
                status: Some(resp.status),
                message: format!("upstream status {}", resp.status).into(),
            });
        }
        Ok(Box::pin(sse_event_stream(
            resp.body,
            self.api,
            StreamDecodePolicy::default(),
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
}
