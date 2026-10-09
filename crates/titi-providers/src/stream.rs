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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCallRef {
    /// Provider-side call id (used for tool-result correlation).
    pub call_id: SmolStr,
    /// Tool name as declared in the request.
    pub name: SmolStr,
    /// The arguments the model asked with, as JSON text.
    ///
    /// Empty on a `ToolcallStart`, which the provider emits before the
    /// arguments arrive; the engine fills it from the collected deltas when it
    /// records the assistant message, so a replayed history shows the model
    /// what it actually asked for. Empty on a call that was persisted before
    /// this field existed.
    #[serde(default, skip_serializing_if = "SmolStr::is_empty")]
    pub arguments: SmolStr,
    /// Gemini's `thoughtSignature` for the part this call came on.
    ///
    /// A thinking model requires the signature echoed with the function call
    /// it belongs to; without it the next request is rejected. Empty for every
    /// other family, and for a call persisted before this field existed.
    #[serde(default, skip_serializing_if = "SmolStr::is_empty")]
    pub thought_signature: SmolStr,
}

/// A thinking block an assistant turn produced, kept so it can be replayed.
///
/// Both families that sign their reasoning require the block *back*, verbatim,
/// on the next request: Anthropic rejects a turn whose `thinking` blocks lost
/// their `signature`, and the Responses/Codex backend wants its
/// `encrypted_content` items again. `text` is the reasoning as the model
/// streamed it, plain and public, so a trace can read it without knowing
/// anything about the provider that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ThinkingBlock {
    /// The reasoning text, as streamed. Empty for a redacted block, whose
    /// payload the provider never shows.
    pub text: String,
    /// What has to be echoed back with the block: Anthropic's `signature`, or
    /// a Responses reasoning item's `encrypted_content`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
    /// The provider-side id of a Responses reasoning item, which the backend
    /// expects back with it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Anthropic's `redacted_thinking` payload, which travels as its own
    /// opaque field rather than as text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data: String,
    /// Which family produced the block. A block is only ever replayed to the
    /// family that signed it — another provider must not receive Anthropic
    /// signatures, and would not understand them.
    pub api: crate::transport::ApiKind,
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
    /// One thinking block finished streaming, with everything the provider
    /// needs echoed back. Emitted at the block's end, so the payload is
    /// complete.
    ThinkingBlock { block: ThinkingBlock },
}

impl StreamEvent {
    /// True once output the user can already see has been emitted — answer
    /// text, tool arguments, or reasoning. After this point retry and
    /// fallback are forbidden for the turn: the surface has painted it, and a
    /// second attempt would stream its own reasoning into the same block, so
    /// the transcript reads the previous attempt's thinking twice.
    pub fn is_visible_output(&self) -> bool {
        matches!(
            self,
            StreamEvent::TextDelta { .. }
                | StreamEvent::ToolcallDelta { .. }
                | StreamEvent::ThinkingDelta { .. }
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
    fn visible_output_covers_text_tools_and_reasoning() {
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
        assert!(text.is_visible_output());
        assert!(tool.is_visible_output());
        // Reasoning is on screen, so a retry would replay it: it counts too.
        assert!(thinking.is_visible_output());
        assert!(!start.is_visible_output());
        assert!(
            !StreamEvent::ThinkingStart {
                id: BlockId::new("b4")
            }
            .is_visible_output()
        );
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
