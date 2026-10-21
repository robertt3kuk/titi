//! Context compaction for the request the engine is about to send.
//!
//! The policy lives in `titi-core`; this is the request-shaped half. A turn
//! accumulates tool results without bound, and the provider eventually
//! refuses the request — folding the oldest messages into one digest is what
//! keeps a long turn alive.
//!
//! Spec: `docs/research/compaction-context`.

mod mask;

use smol_str::SmolStr;
use titi_core::compaction::{CompactionPolicy, StructuredSummarizer, Summarizer, estimate_tokens};
use titi_core::session::{Entry, Role as EntryRole};
use titi_providers::{ChatMessage, Role};

use mask::mask_tool_results;

/// What one compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Messages replaced by the digest. Zero when masking alone was enough.
    pub folded: usize,
    /// Tool results whose bodies were replaced by a one-line stub.
    pub masked: usize,
    /// Estimated tokens before folding.
    pub tokens_before: u64,
    /// `mask` when only tool results were elided, else the strategy that
    /// wrote the digest.
    pub strategy: SmolStr,
}

/// Estimated tokens of a whole request.
pub fn estimate_request(messages: &[ChatMessage]) -> u64 {
    messages.iter().map(|m| estimate_tokens(&m.content)).sum()
}

/// Shrinks the request when it crosses the policy threshold, and returns
/// what happened. Tool results older than the kept tail are masked first; if
/// that brings the request under the threshold it stops there, otherwise the
/// oldest messages are folded into one digest. `None` when nothing was done:
/// below the threshold, or no prefix left to shrink.
pub fn compact(
    messages: &mut Vec<ChatMessage>,
    policy: &CompactionPolicy,
    context_window: u64,
) -> Option<Compacted> {
    let used = estimate_request(messages);
    if !policy.should_compact(used, context_window) {
        return None;
    }

    let entries: Vec<Entry> = messages.iter().map(to_entry).collect();
    let target = policy.compute_target(&entries, policy.keep_recent_tokens);
    let mut first_kept = target.first_kept;
    if first_kept == 0 {
        return None;
    }
    // A tool result without the assistant call that produced it is an invalid
    // request, so the kept tail may not begin with one.
    while first_kept < messages.len() && messages[first_kept].role == Role::Tool {
        first_kept += 1;
    }
    if first_kept >= messages.len() {
        return None;
    }

    let masked = mask_tool_results(messages, first_kept);
    if masked.count > 0
        && !policy.should_compact(used.saturating_sub(masked.saved_tokens), context_window)
    {
        return Some(Compacted {
            folded: 0,
            masked: masked.count,
            tokens_before: used,
            strategy: "mask".into(),
        });
    }

    let prefix = &entries[..first_kept];
    let mut summarizer = StructuredSummarizer;
    let mut chosen = None;
    for strategy in &policy.method_order {
        if let Ok(summary) = summarizer.summarize(*strategy, prefix) {
            chosen = Some((*strategy, summary));
            break;
        }
    }
    let (strategy, summary) = chosen?;

    messages.drain(..first_kept);
    messages.insert(
        0,
        ChatMessage {
            role: Role::System,
            content: summary.into(),
            tool_calls: Vec::new(),
            ..Default::default()
        },
    );
    Some(Compacted {
        folded: first_kept,
        masked: masked.count,
        tokens_before: used,
        strategy: strategy.as_str().into(),
    })
}

fn to_entry(message: &ChatMessage) -> Entry {
    let role = match message.role {
        Role::User => EntryRole::User,
        Role::Assistant => EntryRole::Assistant,
        // Tool output is context the model reads, not something it said.
        Role::System | Role::Tool => EntryRole::System,
    };
    Entry::new(None, role, message.content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: text.into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }
    }

    fn tool(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: text.into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }
    }

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: text.into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }
    }

    /// A policy that fires immediately and keeps a small tail.
    fn eager() -> CompactionPolicy {
        CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 10,
            method_order: vec![
                titi_core::compaction::Strategy::Remote,
                titi_core::compaction::Strategy::SnapCompact,
            ],
        }
    }

    #[test]
    fn below_the_threshold_nothing_is_folded() {
        let mut messages = vec![user("hello")];
        let policy = CompactionPolicy::default();
        assert_eq!(compact(&mut messages, &policy, 1_000_000), None);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn the_oldest_messages_become_one_digest() {
        let mut messages = vec![
            user("first question"),
            assistant("first answer"),
            user("second question"),
            assistant("second answer"),
            user("the newest question"),
        ];
        let folded = compact(&mut messages, &eager(), 100).expect("a compaction");

        // The provider-backed strategy failed and the chain fell through.
        assert_eq!(folded.strategy, "snapcompact");
        assert!(folded.folded >= 1);
        assert_eq!(messages[0].role, Role::System);
        assert!(
            messages[0].content.contains("first question"),
            "the digest names what was dropped: {}",
            messages[0].content
        );
        // The newest turn survives verbatim.
        assert_eq!(messages.last().unwrap().content, "the newest question");
    }

    #[test]
    fn a_kept_tail_never_starts_with_a_tool_result() {
        let mut messages = vec![
            user("go"),
            tool("a big tool result that must not be split from its call"),
            tool("another one"),
            user("and now this"),
        ];
        let policy = CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 6,
            ..CompactionPolicy::default()
        };
        let folded = compact(&mut messages, &policy, 100).expect("a compaction");
        assert!(folded.folded >= 1);
        assert_ne!(
            messages[0].role,
            Role::Tool,
            "an orphaned tool result is an invalid request"
        );
    }

    #[test]
    fn an_empty_history_is_left_alone() {
        let mut messages = Vec::new();
        assert_eq!(compact(&mut messages, &eager(), 100), None);
        assert!(messages.is_empty());
    }

    #[test]
    fn the_estimate_covers_every_message() {
        let messages = vec![user("1234"), assistant("12345678")];
        assert_eq!(estimate_request(&messages), 1 + 2);
    }

    fn call(id: &str, name: &str, arguments: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            tool_calls: vec![titi_providers::ToolCallRef {
                call_id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn result(id: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: text.into(),
            tool_call_id: Some(id.into()),
            ..Default::default()
        }
    }

    /// A request whose weight is two old tool results, then a short tail.
    fn heavy_with_tools() -> Vec<ChatMessage> {
        vec![
            user("fix the parser"),
            call("c1", "read", r#"{"path":"src/parser.rs"}"#),
            result("c1", &"fn parse() {}\n".repeat(600)),
            call("c2", "bash", r#"{"command":"cargo test"}"#),
            result("c2", &"test output\n".repeat(600)),
            assistant("the parser is fixed"),
            user("thanks, now the lexer"),
            assistant("on it"),
            user("newest"),
        ]
    }

    #[test]
    fn masking_alone_is_enough_when_the_tool_results_were_the_weight() {
        let mut messages = heavy_with_tools();
        let before = messages.len();
        let policy = CompactionPolicy {
            threshold_percent: 50.0,
            keep_recent_tokens: 20,
            ..CompactionPolicy::default()
        };

        let done = compact(&mut messages, &policy, 4_000).expect("a compaction");

        assert_eq!(done.strategy, "mask");
        assert_eq!(done.folded, 0);
        assert_eq!(done.masked, 2);
        assert_eq!(messages.len(), before, "nothing was folded away");
        assert_eq!(
            messages[2].content,
            "[tool result elided: read src/parser.rs · 8400 chars · re-run to see it]"
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert!(messages[4].content.contains("bash cargo test"));
        assert_eq!(messages.last().unwrap().content, "newest");
        assert_eq!(compact(&mut messages, &policy, 4_000), None, "one rewrite");
    }

    #[test]
    fn when_masking_is_not_enough_the_fold_follows_it() {
        let mut messages = heavy_with_tools();
        let policy = CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 20,
            method_order: vec![titi_core::compaction::Strategy::SnapCompact],
        };

        let done = compact(&mut messages, &policy, 4_000).expect("a compaction");

        assert_eq!(done.strategy, "snapcompact");
        assert_eq!(done.masked, 2);
        assert_eq!(done.folded, 5, "{done:?}");
    }

    #[test]
    fn a_tool_result_in_the_kept_tail_is_not_masked() {
        let mut messages = vec![
            user("old question"),
            assistant("old answer"),
            user("read it"),
            call("c1", "read", r#"{"path":"big.rs"}"#),
            result("c1", &"line\n".repeat(900)),
        ];
        let policy = CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 1_126,
            method_order: vec![titi_core::compaction::Strategy::SnapCompact],
        };
        let done = compact(&mut messages, &policy, 4_000).expect("a compaction");
        assert_eq!(done.folded, 3);
        assert_eq!(done.masked, 0, "nothing older than the tail was a result");
        assert_eq!(
            messages.last().unwrap().content,
            "line\n".repeat(900),
            "the newest result is read whole"
        );
    }
}
