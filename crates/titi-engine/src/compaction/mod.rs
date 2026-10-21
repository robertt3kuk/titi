//! Context compaction for the request the engine is about to send.
//!
//! The policy lives in `titi-core`; this is the request-shaped half. A turn
//! accumulates tool results without bound, and the provider eventually
//! refuses the request — folding the oldest messages into one digest is what
//! keeps a long turn alive.
//!
//! Spec: `docs/research/compaction-context`.

mod mask;
mod reported;

use smol_str::SmolStr;
use titi_core::compaction::{CompactionPolicy, StructuredSummarizer, Summarizer, estimate_tokens};
use titi_core::session::{Entry, Role as EntryRole};
use titi_providers::{ChatMessage, Role};

use mask::mask_tool_results;
pub use reported::{PromptMemory, Reported, context_used};

/// What a compaction needs to know about the request it may shrink.
#[derive(Debug, Clone, Copy)]
pub struct Fold<'a> {
    pub policy: &'a CompactionPolicy,
    pub context_window: u64,
    /// The provider's count for the request, when there is one.
    pub reported: Option<Reported>,
}

impl<'a> Fold<'a> {
    pub fn new(policy: &'a CompactionPolicy, context_window: u64) -> Self {
        Self {
            policy,
            context_window,
            reported: None,
        }
    }

    pub fn reported(mut self, reported: Option<Reported>) -> Self {
        self.reported = reported;
        self
    }
}

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

/// The system prompt: the leading `System` messages. They are the request's
/// identity, rebuilt per turn, and never part of what a fold takes away.
fn leading_system(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count()
}

/// The digest as a message.
///
/// A `user` message right behind the system prompt, not a `system` one in the
/// middle of the history: some providers drop a system message that is not
/// first and Anthropic's wire has no such role at all, so only a user message
/// reaches every family. Behind the system prompt is also the most stable
/// place for it — everything above is rebuilt identically each turn and
/// everything below only grows, so the provider's prompt cache keeps the
/// whole prefix up to the digest until the next fold rewrites it.
fn digest_message(summary: &str, folded: usize) -> ChatMessage {
    ChatMessage {
        role: Role::User,
        content: format!(
            "<conversation-summary folded=\"{folded}\">\n\
             Earlier messages were folded to keep the context short; this is what they held.\n\
             {summary}\n\
             </conversation-summary>"
        )
        .into(),
        ..Default::default()
    }
}

/// Shrinks the request when it crosses the policy threshold, and returns
/// what happened. Tool results older than the kept tail are masked first; if
/// that brings the request under the threshold it stops there, otherwise the
/// oldest messages are folded into one digest. `None` when nothing was done:
/// below the threshold, or no prefix left to shrink.
pub fn compact(messages: &mut Vec<ChatMessage>, fold: &Fold<'_>) -> Option<Compacted> {
    let Fold {
        policy,
        context_window,
        reported,
    } = *fold;
    let used = context_used(messages, reported);
    if !policy.should_compact(used, context_window) {
        return None;
    }

    let lead = leading_system(messages);
    let entries: Vec<Entry> = messages[lead..].iter().map(to_entry).collect();
    let target = policy.compute_target(&entries, policy.keep_recent_tokens);
    if target.first_kept == 0 {
        return None;
    }
    let mut first_kept = lead + target.first_kept;
    // A tool result without the assistant call that produced it is an invalid
    // request, so the kept tail may not begin with one.
    while first_kept < messages.len() && messages[first_kept].role == Role::Tool {
        first_kept += 1;
    }
    if first_kept >= messages.len() {
        return None;
    }

    let masked = mask_tool_results(messages, first_kept);
    let masked_only = Compacted {
        folded: 0,
        masked: masked.count,
        tokens_before: used,
        strategy: "mask".into(),
    };
    if masked.count > 0
        && !policy.should_compact(used.saturating_sub(masked.saved_tokens), context_window)
    {
        return Some(masked_only);
    }

    let prefix = &entries[..first_kept - lead];
    let mut summarizer = StructuredSummarizer;
    let mut chosen = None;
    for strategy in &policy.method_order {
        if let Ok(summary) = summarizer.summarize(*strategy, prefix) {
            chosen = Some((*strategy, summary));
            break;
        }
    }
    let Some((strategy, summary)) = chosen else {
        // No strategy could write a digest; what masking already did stands.
        return (masked.count > 0).then_some(masked_only);
    };

    let folded = first_kept - lead;
    messages.splice(lead..first_kept, [digest_message(&summary, folded)]);
    Some(Compacted {
        folded,
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

    fn compact(
        messages: &mut Vec<ChatMessage>,
        policy: &CompactionPolicy,
        context_window: u64,
    ) -> Option<Compacted> {
        super::compact(messages, &Fold::new(policy, context_window))
    }

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
        assert_eq!(messages[0].role, Role::User);
        assert!(
            messages[0].content.contains("<conversation-summary"),
            "{}",
            messages[0].content
        );
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

    fn system(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::System,
            content: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_fold_keeps_the_system_prompt_and_puts_the_digest_right_behind_it() {
        let mut messages = vec![
            system("you are titi"),
            user("first question"),
            assistant("first answer"),
            user("second question"),
            assistant("second answer"),
            user("the newest question"),
        ];
        let done = compact(&mut messages, &eager(), 100).expect("a compaction");

        assert!(done.folded >= 1);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].content, "you are titi");
        assert_eq!(messages[1].role, Role::User);
        assert!(messages[1].content.contains("<conversation-summary"));
        assert_eq!(
            messages.iter().filter(|m| m.role == Role::System).count(),
            1,
            "no system message mid-history: {messages:#?}"
        );
    }

    #[test]
    fn a_history_that_is_only_the_system_prompt_is_left_alone() {
        let mut messages = vec![system("you are titi"), user("hi")];
        assert_eq!(compact(&mut messages, &eager(), 100), None);
        assert_eq!(messages.len(), 2);
    }

    /// A request the estimate sees as small but the provider counted as
    /// nearly full: the count wins, because the estimate ignores the tool
    /// declarations and runs low for code.
    #[test]
    fn the_providers_count_arms_the_trigger_the_estimate_misses() {
        let policy = CompactionPolicy {
            threshold_percent: 80.0,
            keep_recent_tokens: 4,
            method_order: vec![titi_core::compaction::Strategy::SnapCompact],
        };
        let history = || {
            vec![
                user("first question, long enough to fold"),
                assistant("first answer"),
                user("newest"),
            ]
        };
        let mut by_estimate = history();
        assert_eq!(
            super::compact(&mut by_estimate, &Fold::new(&policy, 1_000)),
            None,
            "a dozen tokens of a thousand-token window is nowhere near"
        );

        let mut by_count = history();
        let seen = Reported {
            prompt_tokens: 900,
            messages: 3,
        };
        let done = super::compact(
            &mut by_count,
            &Fold::new(&policy, 1_000).reported(Some(seen)),
        )
        .expect("the provider says 90% full");
        assert_eq!(
            done.tokens_before, 900,
            "the figure it reports is the real one"
        );
        assert!(done.folded >= 1);
    }

    /// The reverse: a long-looking request the provider counted as small is
    /// not worth a fold.
    #[test]
    fn a_small_provider_count_overrides_a_large_estimate() {
        let policy = CompactionPolicy {
            threshold_percent: 80.0,
            keep_recent_tokens: 4,
            ..CompactionPolicy::default()
        };
        let mut messages = vec![user(&"x".repeat(4_000)), assistant("ok"), user("go")];
        let seen = Reported {
            prompt_tokens: 100,
            messages: 3,
        };
        assert_eq!(
            super::compact(
                &mut messages,
                &Fold::new(&policy, 1_200).reported(Some(seen))
            ),
            None
        );
    }
}
