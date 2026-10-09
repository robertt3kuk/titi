//! Context compaction for the request the engine is about to send.
//!
//! The policy lives in `titi-core`; this is the request-shaped half. A turn
//! accumulates tool results without bound, and the provider eventually
//! refuses the request — folding the oldest messages into one digest is what
//! keeps a long turn alive.
//!
//! Spec: `docs/research/compaction-context`.

use smol_str::SmolStr;
use titi_core::compaction::{CompactionPolicy, StructuredSummarizer, Summarizer, estimate_tokens};
use titi_core::session::{Entry, Role as EntryRole};
use titi_providers::{ChatMessage, Role};

/// What one compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Messages replaced by the digest.
    pub folded: usize,
    /// Estimated tokens before folding.
    pub tokens_before: u64,
    pub strategy: SmolStr,
}

/// Estimated tokens of a whole request.
pub fn estimate_request(messages: &[ChatMessage]) -> u64 {
    messages.iter().map(|m| estimate_tokens(&m.content)).sum()
}

/// Folds the oldest messages into one digest when the request crosses the
/// policy threshold, and returns what happened. `None` when nothing was
/// folded: below the threshold, or no prefix left to fold.
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
}
