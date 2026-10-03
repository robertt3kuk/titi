//! Unified stream-event contract shared by every provider transport.
//!
//! Mirrors omp's `AssistantMessageEvent`: content arrives as triplets
//! (`*_start` → `*_delta*` → `*_end`) for text, thinking and tool calls, and
//! the stream is terminated by exactly one terminal event
//! ([`StreamEvent::Done`] or [`StreamEvent::Error`]).

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// Opaque identifier of one content block within a turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockId(pub SmolStr);

impl BlockId {
    pub fn new(id: impl Into<SmolStr>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reference to a tool being invoked inside a `toolcall_*` triplet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRef {
    /// Provider-side call id (used for tool-result correlation).
    pub call_id: SmolStr,
    /// Tool name as declared in the request.
    pub name: SmolStr,
}

/// Why the model finished the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural end of turn.
    Stop,
    /// Token/context budget exhausted.
    Length,
    /// Turn ended because a tool call was issued.
    ToolUse,
}

/// Why a stream failed (retryability decisions live with the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorReason {
    /// Malformed wire payload (undecodable JSON, protocol violation).
    Malformed,
    /// Upstream rejected the request (auth, quota, validation).
    Rejected,
    /// Connection broke or watchdog fired mid-stream.
    Connection,
    /// Consumer cancelled the stream.
    Aborted,
}

/// Token counts the provider itself reported for one request.
///
/// `prompt_tokens` is everything the request was billed as input, prompt-cache
/// reads and writes included, so it stands where an estimate of the whole
/// request would; `cached_tokens` is the part of it read from the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

impl TokenUsage {
    /// A report, or `None` when it measured nothing: some compatible servers
    /// fill the field with zeros, and a request always has input.
    pub(crate) fn reported(prompt: u64, completion: u64, cached: u64) -> Option<Self> {
        if prompt == 0 && completion == 0 {
            return None;
        }
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached.min(prompt),
        })
    }
}

/// Normalized stream event, identical for every endpoint family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    Start,
    TextStart {
        id: BlockId,
    },
    TextDelta {
        id: BlockId,
        text: SmolStr,
    },
    TextEnd {
        id: BlockId,
    },
    ThinkingStart {
        id: BlockId,
    },
    ThinkingDelta {
        id: BlockId,
        text: SmolStr,
    },
    ThinkingEnd {
        id: BlockId,
    },
    ToolcallStart {
        id: BlockId,
        call: ToolCallRef,
    },
    ToolcallDelta {
        id: BlockId,
        json: SmolStr,
    },
    ToolcallEnd {
        id: BlockId,
    },
    /// The provider's own count for the request, ahead of the terminal event.
    /// Counts are cumulative, so a later report in the stream replaces an
    /// earlier one; a provider that reports nothing sends none.
    Usage(TokenUsage),
    Done {
        reason: StopReason,
    },
    Error {
        reason: ErrorReason,
        message: SmolStr,
    },
}

impl StreamEvent {
    /// True once user-visible content (text or tool arguments) has been
    /// emitted; after this point retry/fallback is forbidden for the turn.
    pub fn is_content(&self) -> bool {
        matches!(
            self,
            StreamEvent::TextDelta { .. } | StreamEvent::ToolcallDelta { .. }
        )
    }

    /// True for terminal events (`Done`/`Error`).
    pub fn is_terminal(&self) -> bool {
        matches!(self, StreamEvent::Done { .. } | StreamEvent::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_flag_covers_only_deltas() {
        let text = StreamEvent::TextDelta {
            id: BlockId::new("b1"),
            text: "hi".into(),
        };
        let tool = StreamEvent::ToolcallDelta {
            id: BlockId::new("b2"),
            json: "{}".into(),
        };
        let thinking = StreamEvent::ThinkingDelta {
            id: BlockId::new("b3"),
            text: "hmm".into(),
        };
        let start = StreamEvent::Start;
        assert!(text.is_content());
        assert!(tool.is_content());
        assert!(!thinking.is_content());
        assert!(!start.is_content());
    }

    #[test]
    fn terminal_flag() {
        assert!(
            StreamEvent::Done {
                reason: StopReason::Stop
            }
            .is_terminal()
        );
        assert!(
            StreamEvent::Error {
                reason: ErrorReason::Malformed,
                message: "x".into()
            }
            .is_terminal()
        );
        assert!(!StreamEvent::Start.is_terminal());
        assert!(
            !StreamEvent::TextEnd {
                id: BlockId::new("b")
            }
            .is_terminal()
        );
    }

    #[test]
    fn stop_reason_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(StopReason::ToolUse).unwrap(),
            serde_json::json!("tool_use")
        );
    }
}
