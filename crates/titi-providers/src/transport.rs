//! Transport abstraction: requests are dispatched by endpoint family
//! ([`ApiKind`]), never by provider name, so a new OpenAI-compatible gateway
//! is one descriptor entry with zero transport-code changes.

use std::pin::Pin;
use std::time::Duration;

use crate::stream::StreamEvent;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// Endpoint family. Dispatch key for transports and decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKind {
    #[default]
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "gemini-generate-content")]
    GeminiGenerateContent,
}

impl std::fmt::Display for ApiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ApiKind::OpenAiCompletions => "openai-completions",
            ApiKind::OpenAiResponses => "openai-responses",
            ApiKind::AnthropicMessages => "anthropic-messages",
            ApiKind::GeminiGenerateContent => "gemini-generate-content",
        })
    }
}

/// A single conversation message in normalized (family-agnostic) form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    pub role: Role,
    pub content: SmolStr,
    /// Tool calls issued by the assistant in this message, if any.
    pub tool_calls: Vec<crate::stream::ToolCallRef>,
    /// Thinking blocks this assistant turn produced, in order.
    ///
    /// Kept so a signed turn can be replayed verbatim — see
    /// [`ThinkingBlock`](crate::stream::ThinkingBlock). Empty on every message
    /// that is not an assistant turn, and on a history persisted before this
    /// field existed. `text` is the reasoning as it streamed, so a trace can
    /// read it without knowing the provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<crate::stream::ThinkingBlock>,
    /// For a `Tool` message: the call this result answers.
    ///
    /// `None` on a message persisted before this field existed, and on every
    /// message that is not a result. A wire builder falls back to pairing by
    /// order only for those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<SmolStr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    System,
    User,
    Assistant,
    Tool,
}

/// Tool declared in the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: SmolStr,
    pub description: SmolStr,
    /// JSON Schema for the tool arguments.
    pub parameters: serde_json::Value,
}

/// Family-agnostic request before per-family wire shaping.
#[derive(Debug, Clone)]
pub struct WireRequest {
    pub model: SmolStr,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    /// Hard generation cap requested by the host, if any. Gateways like
    /// OpenRouter treat the absence of this as a routing hint.
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<SmolStr>,
    pub extra: serde_json::Value,
}

impl WireRequest {
    pub fn new(model: impl Into<SmolStr>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            system: None,
            extra: serde_json::Value::Null,
        }
    }
}

/// Per-request context carried alongside the wire payload.
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    /// Resolved credential of the provider the request goes to; `None` for a
    /// keyless local endpoint. The transport picks the auth scheme from its
    /// kind, so a key string never reaches the wire undecided.
    pub credential: Option<crate::creds::Credential>,
    /// Abort signal: set to `true` to cancel the stream.
    pub aborted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RequestCtx {
    /// A context for a bare runtime key (`--api-key`, tests): no ladder ran,
    /// so the key is the runtime rung's `api_key`.
    pub fn with_key(key: impl Into<SmolStr>) -> Self {
        Self {
            credential: Some(crate::creds::Credential::api_key(
                key,
                crate::creds::LadderLevel::Runtime,
            )),
            ..Self::default()
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Errors surfaced by a transport before or during streaming.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportError {
    /// Upstream rate limited / temporarily unavailable (fallback-eligible
    /// between turns).
    Retryable {
        status: Option<u16>,
        message: SmolStr,
    },
    /// Definitive rejection: do not retry, do not fall back.
    Fatal {
        status: Option<u16>,
        message: SmolStr,
    },
    /// Watchdog: no first event or too long between events. `model` and
    /// `waited` are what the person needs to see: which endpoint was silent
    /// and for how long.
    Stalled {
        phase: StallPhase,
        model: SmolStr,
        waited: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallPhase {
    FirstEvent,
    Idle,
}

impl TransportError {
    /// Whether the chain may advance to the next entry between turns.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            TransportError::Retryable { .. } | TransportError::Stalled { .. }
        )
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Retryable {
                status: Some(status),
                message,
            } => write!(f, "temporarily unavailable (HTTP {status}): {message}"),
            TransportError::Retryable {
                status: None,
                message,
            } => write!(f, "temporarily unavailable: {message}"),
            TransportError::Fatal {
                status: Some(status),
                message,
            } => write!(f, "rejected (HTTP {status}): {message}"),
            TransportError::Fatal {
                status: None,
                message,
            } => write!(f, "rejected: {message}"),
            TransportError::Stalled {
                phase: StallPhase::FirstEvent,
                model,
                waited,
            } => write!(
                f,
                "no reply from {model} within {waited:?} (first-event timeout)"
            ),
            TransportError::Stalled {
                phase: StallPhase::Idle,
                model,
                waited,
            } => write!(
                f,
                "the reply from {model} stalled for {waited:?} mid-stream"
            ),
        }
    }
}

impl std::error::Error for TransportError {}

/// Watchdog timings, copied from proven omp defaults.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// Max wait for the first event of the stream.
    pub first_event_timeout: Duration,
    /// Max wait between consecutive events.
    pub idle_timeout: Duration,
    /// Grace window after the terminal event for trailing usage chunks.
    pub post_finish_grace: Duration,
    /// Empty-completion retry: at most N retries (0 disables), backoff base.
    pub empty_completion_retries: u32,
    pub empty_completion_backoff: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            first_event_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(60),
            post_finish_grace: Duration::from_millis(2500),
            empty_completion_retries: 2,
            empty_completion_backoff: Duration::from_millis(500),
        }
    }
}

/// Push stream of normalized events.
pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

/// Transport for one endpoint family.
#[async_trait]
pub trait Transport: Send + Sync {
    fn api(&self) -> ApiKind;

    fn watchdog(&self) -> WatchdogConfig {
        WatchdogConfig::default()
    }

    /// Open a stream for `req`; fails before streaming with
    /// [`TransportError`] or succeeds and then reports in-band
    /// [`StreamEvent::Error`] for malformed payloads.
    async fn stream(
        &self,
        req: WireRequest,
        ctx: RequestCtx,
    ) -> Result<EventStream, TransportError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_defaults_match_omp_timings() {
        let w = WatchdogConfig::default();
        assert_eq!(w.first_event_timeout, Duration::from_secs(120));
        assert_eq!(w.idle_timeout, Duration::from_secs(60));
        assert_eq!(w.post_finish_grace, Duration::from_millis(2500));
        assert_eq!(w.empty_completion_retries, 2);
        assert_eq!(w.empty_completion_backoff, Duration::from_millis(500));
    }

    #[test]
    fn retryability_classes() {
        assert!(
            TransportError::Retryable {
                status: Some(429),
                message: "rl".into()
            }
            .is_retryable()
        );
        assert!(
            TransportError::Stalled {
                phase: StallPhase::Idle,
                model: "m".into(),
                waited: Duration::from_millis(1),
            }
            .is_retryable()
        );
        assert!(
            !TransportError::Fatal {
                status: Some(401),
                message: "no".into()
            }
            .is_retryable()
        );
    }

    #[test]
    fn api_kind_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_value(ApiKind::OpenAiCompletions).unwrap(),
            serde_json::json!("openai-completions")
        );
        assert_eq!(
            serde_json::to_value(ApiKind::GeminiGenerateContent).unwrap(),
            serde_json::json!("gemini-generate-content")
        );
    }

    /// The message reaches the person as written: the status as `HTTP 401`,
    /// never as a debug-printed `Some(401)`, and a stall in words rather than
    /// a variant name.
    #[test]
    fn errors_read_as_sentences_not_debug_output() {
        let cases = [
            (
                TransportError::Fatal {
                    status: Some(401),
                    message: "invalid api key".into(),
                },
                "rejected (HTTP 401): invalid api key",
            ),
            (
                TransportError::Fatal {
                    status: None,
                    message: "bad frame".into(),
                },
                "rejected: bad frame",
            ),
            (
                TransportError::Retryable {
                    status: Some(429),
                    message: "slow down".into(),
                },
                "temporarily unavailable (HTTP 429): slow down",
            ),
            (
                TransportError::Retryable {
                    status: None,
                    message: "connection refused".into(),
                },
                "temporarily unavailable: connection refused",
            ),
            (
                TransportError::Stalled {
                    phase: StallPhase::FirstEvent,
                    model: "gpt-x".into(),
                    waited: Duration::from_secs(120),
                },
                "no reply from gpt-x within 120s (first-event timeout)",
            ),
            (
                TransportError::Stalled {
                    phase: StallPhase::Idle,
                    model: "gpt-x".into(),
                    waited: Duration::from_secs(60),
                },
                "the reply from gpt-x stalled for 60s mid-stream",
            ),
        ];
        for (error, expected) in cases {
            let shown = error.to_string();
            assert_eq!(shown, expected);
            assert!(!shown.contains("Some("), "{shown}");
        }
    }

    #[test]
    fn request_ctx_key_is_runtime_only() {
        let ctx = RequestCtx::with_key("sk-test");
        assert_eq!(
            ctx.credential.as_ref().map(|c| c.access.as_str()),
            Some("sk-test")
        );
        assert_eq!(
            ctx.credential.as_ref().map(|c| c.level),
            Some(crate::creds::LadderLevel::Runtime)
        );
        assert!(!ctx.is_aborted());
    }
}
