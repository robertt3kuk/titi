//! LLM provider layer for titi: wire transports dispatched by endpoint family,
//! a single SSE engine, per-family stream decoders normalizing into
//! [`StreamEvent`], a repairing partial-JSON parser for streamed tool
//! arguments, stop-reason tables, compat policies, the credential ladder and a
//! one-shot fallback chain (see `docs/research/providers-streaming`).

pub mod anthropic;
pub mod compat;
pub mod creds;
pub mod discovery;
pub mod fallback;
pub mod gemini;
pub mod http;
pub mod mock;
pub mod openai;
pub mod partial_json;
pub mod sse;
pub mod stop;
pub mod stream;
pub mod transport;
pub mod wire;

pub use anthropic::AnthropicStreamState;
pub use compat::{
    CompatPolicy, EFFORT_LADDER, Effort, MaxTokensField, ModelCompat, RequestOpts,
    StreamDecodePolicy, StrictMode, ThinkingFormat, clamp_effort, resolve_compat,
};
pub use creds::{CredKind, Credential, LadderCtx, LadderLevel, parse_env_file, resolve_credential};
pub use discovery::{DiscoveryError, MAX_DISCOVERY_BODY, list_models};
pub use fallback::{FallbackChain, ModelRef};
pub use gemini::GeminiStreamState;
pub use http::{HttpFetch, HttpRequest, HttpResponse, ReqwestFetch};
pub use mock::{MockBody, MockFetch, MockFetchResponse, MockTransport};
pub use partial_json::{PartialJson, STREAMING_JSON_PARSE_MIN_GROWTH, relaxed_parse};
pub use sse::{MarkerStripper, SseDecoder, SseFrame};
pub use stop::{StopMapping, map_stop_reason, promote_stop_for_tools};
pub use stream::{BlockId, ErrorReason, StopReason, StreamEvent, ToolCallRef};
pub use transport::{
    ApiKind, ChatMessage, EventStream, RequestCtx, Role, ToolSpec, Transport, TransportError,
    WatchdogConfig, WireRequest,
};
pub use wire::OpenAiCompatTransport;
pub use wire::{FamilyTransport, build_http_request, sse_event_stream};

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
