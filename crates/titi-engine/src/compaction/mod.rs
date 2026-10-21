//! Context compaction for the request the engine is about to send.
//!
//! The policy lives in `titi-core`; this is the request-shaped half. A turn
//! accumulates tool results without bound, and the provider eventually
//! refuses the request — folding the oldest messages into one digest is what
//! keeps a long turn alive.
//!
//! Spec: `docs/research/compaction-context`.

mod digest;
mod mask;
mod reported;
mod summary;

use smol_str::SmolStr;
use titi_core::compaction::{
    CompactionPolicy, Strategy, StructuredSummarizer, Summarizer, estimate_tokens, focus_digest,
};
use titi_core::session::{Entry, Role as EntryRole};
use titi_providers::{ChatMessage, Role, TokenUsage};

use digest::{digest_message, digest_summary, is_digest};
use mask::mask_tool_results;
pub use reported::{PromptMemory, Reported, context_used};
pub use summary::{Summary, SummaryError, SummaryModel, summary_model};

/// What a compaction needs to know about the request it may shrink.
#[derive(Clone, Copy)]
pub struct Fold<'a> {
    pub policy: &'a CompactionPolicy,
    pub context_window: u64,
    /// The provider's count for the request, when there is one.
    pub reported: Option<Reported>,
    /// What the user asked the digest to keep (`/compact <focus>`).
    pub focus: Option<&'a str>,
    /// The model that writes the summary; without one the fold is structural.
    pub summarizer: Option<&'a SummaryModel>,
}

impl<'a> Fold<'a> {
    pub fn new(policy: &'a CompactionPolicy, context_window: u64) -> Self {
        Self {
            policy,
            context_window,
            reported: None,
            focus: None,
            summarizer: None,
        }
    }

    pub fn reported(mut self, reported: Option<Reported>) -> Self {
        self.reported = reported;
        self
    }

    pub fn focus(mut self, focus: Option<&'a str>) -> Self {
        self.focus = focus;
        self
    }

    pub fn summarizer(mut self, summarizer: Option<&'a SummaryModel>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// Whether `messages` have reached the threshold. Cheap, and asked before
    /// the summary model is looked up, so a request under the threshold never
    /// pays for that.
    pub fn due(&self, messages: &[ChatMessage]) -> bool {
        self.policy
            .should_compact(context_used(messages, self.reported), self.context_window)
    }
}

/// What one compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Messages replaced by the digest. Zero when masking alone was enough.
    pub folded: usize,
    /// Tool results whose bodies were replaced by a one-line stub.
    pub masked: usize,
    /// Tokens before folding, by the provider's count when it gave one.
    pub tokens_before: u64,
    /// `mask` when only tool results were elided, else the strategy that
    /// wrote the digest.
    pub strategy: SmolStr,
    /// The model that wrote the digest, when one did.
    pub summary_model: Option<SmolStr>,
    /// What the summary call cost, for the session's meter.
    pub summary_usage: Option<TokenUsage>,
    /// Why the summary model did not write the digest, when it was asked to
    /// and the fold fell back.
    pub summary_failure: Option<String>,
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

/// How much of an earlier digest a model-free fold carries forward.
const CARRIED_CHARS: usize = 3_000;

/// `text` cut to `max` characters, with a mark where it was cut.
fn clamp_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Shrinks the request when it crosses the policy threshold, and returns
/// what happened. Tool results older than the kept tail are masked first; if
/// that brings the request under the threshold it stops there, otherwise the
/// oldest messages are folded into one digest, written by the summary model
/// when there is one and by the structural summarizer when there is not or
/// it fails. `None` when nothing was done: below the threshold, or no prefix
/// left to shrink.
pub async fn compact(messages: &mut Vec<ChatMessage>, fold: &Fold<'_>) -> Option<Compacted> {
    let Fold {
        policy,
        context_window,
        reported,
        focus,
        summarizer: summary_model,
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
    // request, so the kept tail may not begin with one: the cut moves past
    // the results and folds the whole pair.
    let mut cut = first_kept;
    while cut < messages.len() && messages[cut].role == Role::Tool {
        cut += 1;
    }
    if cut < messages.len() {
        first_kept = cut;
    } else {
        // Nothing but results after the cut — the newest result alone is over
        // the tail budget. Keep that pair instead, and fold what is before it.
        while first_kept > lead && messages[first_kept].role == Role::Tool {
            first_kept -= 1;
        }
        if first_kept <= lead {
            return None;
        }
    }

    let masked = mask_tool_results(messages, first_kept);
    let masked_only = Compacted {
        folded: 0,
        masked: masked.count,
        tokens_before: used,
        strategy: "mask".into(),
        summary_model: None,
        summary_usage: None,
        summary_failure: None,
    };
    if masked.count > 0
        && !policy.should_compact(used.saturating_sub(masked.saved_tokens), context_window)
    {
        return Some(masked_only);
    }

    let prefix = &entries[..first_kept - lead];
    let mut structural = StructuredSummarizer;
    let mut chosen = None;
    let mut summary_failure = None;
    for strategy in &policy.method_order {
        if *strategy == Strategy::Remote {
            let Some(model) = summary_model else { continue };
            // The bodies masking just replaced are what the model most needs
            // to read — a command's outcome is in its result — so it gets
            // them back, clamped to what fits.
            let mut readable = messages[lead..first_kept].to_vec();
            for (at, original) in &masked.originals {
                readable[at - lead].content = original.clone();
            }
            match model.summarize(&readable, focus, context_window).await {
                Ok(written) => {
                    chosen = Some((
                        *strategy,
                        written.text,
                        Some(model.label.clone()),
                        written.usage,
                    ));
                    break;
                }
                Err(error) => summary_failure = Some(format!("{}: {error}", model.label)),
            }
        } else if let Ok(digest) = structural.summarize(*strategy, prefix) {
            chosen = Some((*strategy, digest, None, None));
            break;
        }
    }
    let Some((strategy, mut summary, written_by, summary_usage)) = chosen else {
        // No strategy could write a digest; what masking already did stands.
        return (masked.count > 0).then_some(Compacted {
            summary_failure,
            ..masked_only
        });
    };

    let folded_messages = &messages[lead..first_kept];
    if written_by.is_none() {
        // The structural digest names prompts and counts; what an earlier
        // fold already knew would otherwise be lost to a second one.
        if let Some(earlier) = folded_messages.iter().find(|m| is_digest(m)) {
            summary.push_str("\n\nCarried from an earlier fold:\n");
            summary.push_str(&clamp_chars(digest_summary(earlier), CARRIED_CHARS));
        }
        // The model was told the focus; the structural digest keeps only the
        // lines that mention it.
        if let Some(focus) = focus {
            let folded_text: Vec<&str> =
                prefix.iter().map(|entry| entry.content.as_str()).collect();
            summary.push('\n');
            summary.push_str(&focus_digest(focus, &folded_text));
        }
    }
    let folded = first_kept - lead;
    let digest = digest_message(&summary, folded, folded_messages);
    messages.splice(lead..first_kept, [digest]);
    Some(Compacted {
        folded,
        masked: masked.count,
        tokens_before: used,
        strategy: strategy.as_str().into(),
        summary_model: written_by,
        summary_usage,
        summary_failure,
    })
}

fn to_entry(message: &ChatMessage) -> Entry {
    let role = match message.role {
        // An earlier digest is context the user did not type, and listing it
        // among the prompts would name the wrapper instead of the work.
        Role::User if is_digest(message) => EntryRole::System,
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

    fn run(messages: &mut Vec<ChatMessage>, fold: &Fold<'_>) -> Option<Compacted> {
        futures::executor::block_on(super::compact(messages, fold))
    }

    fn compact(
        messages: &mut Vec<ChatMessage>,
        policy: &CompactionPolicy,
        context_window: u64,
    ) -> Option<Compacted> {
        run(messages, &Fold::new(policy, context_window))
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
            run(&mut by_estimate, &Fold::new(&policy, 1_000)),
            None,
            "a dozen tokens of a thousand-token window is nowhere near"
        );

        let mut by_count = history();
        let seen = Reported {
            prompt_tokens: 900,
            messages: 3,
        };
        let done = run(
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
            run(
                &mut messages,
                &Fold::new(&policy, 1_200).reported(Some(seen))
            ),
            None
        );
    }

    #[test]
    fn a_second_fold_carries_the_first_digest_and_its_file_list() {
        let policy = CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 6,
            method_order: vec![titi_core::compaction::Strategy::SnapCompact],
        };
        let mut messages = vec![
            user("fix the lexer"),
            call("c1", "read", r#"{"path":"src/lexer.rs"}"#),
            result("c1", "tokens"),
            assistant("the lexer drops a quote"),
            user("now the parser"),
            assistant("on it"),
            user("newest"),
        ];
        compact(&mut messages, &policy, 100).expect("the first fold");
        let first_digest = messages[0].content.clone();
        assert!(first_digest.contains("- src/lexer.rs"), "{first_digest}");

        messages.extend([
            call("c2", "edit", r#"{"path":"src/parser.rs"}"#),
            result("c2", "edited"),
            assistant("the parser is fixed"),
            user("and now the docs, please, when you can"),
        ]);
        compact(&mut messages, &policy, 100).expect("the second fold");

        let digests: Vec<&ChatMessage> = messages.iter().filter(|m| is_digest(m)).collect();
        assert_eq!(digests.len(), 1, "one digest, not a stack: {messages:#?}");
        let second = &digests[0].content;
        assert!(second.contains("Carried from an earlier fold"), "{second}");
        assert!(second.contains("fix the lexer"), "{second}");
        assert!(second.contains("- src/parser.rs"), "{second}");
        assert!(second.contains("- src/lexer.rs"), "{second}");
    }

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use titi_providers::{
        BlockId, MockBody, MockTransport, StopReason, StreamEvent, Transport, TransportError,
    };

    fn summarizer(
        bodies: Vec<MockBody>,
        aborted: Arc<AtomicBool>,
    ) -> (SummaryModel, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(bodies));
        let model = SummaryModel::new(
            "scripted/summarizer",
            crate::registry::ResolvedModel::without_credential(
                "scripted/summarizer",
                Arc::clone(&transport) as Arc<dyn Transport>,
            ),
            aborted,
        );
        (model, transport)
    }

    fn written(text: &str) -> MockBody {
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("text"),
                text: text.into(),
            },
            StreamEvent::Usage(TokenUsage {
                prompt_tokens: 800,
                completion_tokens: 120,
                cached_tokens: 0,
            }),
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])
    }

    /// Every kind of fold wants the same policy: threshold gone, a small tail.
    fn forced() -> CompactionPolicy {
        CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 20,
            ..CompactionPolicy::default()
        }
    }

    #[tokio::test]
    async fn the_session_model_writes_the_digest() {
        let (model, transport) = summarizer(
            vec![written(
                "## Goal\nfix the parser\n## Next step\nrun the lexer tests",
            )],
            Arc::default(),
        );
        let mut messages = heavy_with_tools();
        let policy = forced();

        let done = compact_with(&mut messages, &policy, Some(&model), None)
            .await
            .expect("a compaction");

        assert_eq!(done.strategy, "remote");
        assert_eq!(done.summary_model.as_deref(), Some("scripted/summarizer"));
        assert_eq!(done.summary_failure, None);
        assert_eq!(done.summary_usage.map(|u| u.completion_tokens), Some(120));
        let digest = &messages[0];
        assert_eq!(digest.role, Role::User);
        assert!(digest.content.contains("## Goal\nfix the parser"));
        assert!(
            !digest.content.contains("earlier message(s) folded"),
            "the structural digest was not used: {}",
            digest.content
        );
        assert!(
            digest.content.contains("- src/parser.rs"),
            "the re-read list is appended to the model's summary: {}",
            digest.content
        );
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn the_model_reads_the_results_masking_replaced() {
        let (model, transport) = summarizer(vec![written("## Goal\nx")], Arc::default());
        let mut messages = heavy_with_tools();

        let done = compact_with(&mut messages, &forced(), Some(&model), None)
            .await
            .expect("a compaction");

        assert_eq!(done.masked, 2);
        let requests = transport.requests();
        let sent = &requests[0].messages[0].content;
        assert!(
            sent.contains("test output"),
            "the outcome is readable: {sent}"
        );
        assert!(!sent.contains("tool result elided"), "{sent}");
    }

    #[tokio::test]
    async fn a_failing_model_leaves_the_structural_digest() {
        let (model, _) = summarizer(
            vec![MockBody::Err(TransportError::Fatal {
                status: Some(401),
                message: "no key".into(),
                context_too_long: false,
            })],
            Arc::default(),
        );
        let mut messages = heavy_with_tools();

        let done = compact_with(&mut messages, &forced(), Some(&model), None)
            .await
            .expect("the fold still happens");

        assert_eq!(done.strategy, "snapcompact");
        assert_eq!(done.summary_model, None);
        let failure = done.summary_failure.expect("the failure is reported");
        assert!(failure.contains("scripted/summarizer"), "{failure}");
        assert!(failure.contains("no key"), "{failure}");
        assert!(messages[0].content.contains("earlier message(s) folded"));
    }

    #[tokio::test]
    async fn a_cancelled_summary_leaves_the_structural_digest() {
        let aborted = Arc::new(AtomicBool::new(true));
        let (model, transport) = summarizer(vec![written("never")], aborted);
        let mut messages = heavy_with_tools();

        let done = compact_with(&mut messages, &forced(), Some(&model), None)
            .await
            .expect("the fold still happens");

        assert_eq!(done.strategy, "snapcompact");
        assert!(done.summary_failure.unwrap().contains("cancelled"));
        assert!(transport.requests().is_empty(), "nothing was asked");
    }

    #[tokio::test]
    async fn masking_alone_never_asks_the_model() {
        let (model, transport) = summarizer(vec![written("unused")], Arc::default());
        let mut messages = heavy_with_tools();
        let policy = CompactionPolicy {
            threshold_percent: 50.0,
            keep_recent_tokens: 20,
            ..CompactionPolicy::default()
        };

        let done = {
            let fold = Fold::new(&policy, 4_000).summarizer(Some(&model));
            super::compact(&mut messages, &fold)
                .await
                .expect("a compaction")
        };

        assert_eq!(done.strategy, "mask");
        assert_eq!(done.summary_model, None);
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn a_focus_goes_to_the_model_and_to_the_structural_fallback() {
        let (model, transport) = summarizer(vec![written("## Goal\nx")], Arc::default());
        let mut messages = heavy_with_tools();
        compact_with(&mut messages, &forced(), Some(&model), Some("the parser"))
            .await
            .expect("a compaction");
        let requests = transport.requests();
        assert!(
            requests[0]
                .system
                .as_deref()
                .unwrap()
                .contains("the parser"),
            "the model is told what to keep"
        );

        let mut fallback = heavy_with_tools();
        compact_with(&mut fallback, &forced(), None, Some("parser"))
            .await
            .expect("a compaction");
        assert!(
            fallback[0]
                .content
                .contains("Kept for the requested focus (parser)"),
            "{}",
            fallback[0].content
        );
    }

    async fn compact_with(
        messages: &mut Vec<ChatMessage>,
        policy: &CompactionPolicy,
        model: Option<&SummaryModel>,
        focus: Option<&str>,
    ) -> Option<Compacted> {
        let fold = Fold::new(policy, 4_000).summarizer(model).focus(focus);
        super::compact(messages, &fold).await
    }

    /// The newest message is a result bigger than the whole tail budget. The
    /// cut may not leave it without its call, and folding nothing would leave
    /// the request stuck over the threshold: the pair stays, what is before
    /// it folds.
    #[test]
    fn an_oversized_newest_result_keeps_its_call_and_folds_what_precedes() {
        let mut messages = vec![
            user("old question"),
            assistant("old answer"),
            user("read it"),
            call("c1", "read", r#"{"path":"big.rs"}"#),
            result("c1", &"line\n".repeat(900)),
        ];
        let policy = CompactionPolicy {
            threshold_percent: 0.0,
            keep_recent_tokens: 4,
            method_order: vec![titi_core::compaction::Strategy::SnapCompact],
        };
        let done = compact(&mut messages, &policy, 4_000).expect("a compaction");
        assert_eq!(done.folded, 3);
        assert_eq!(messages[1].tool_calls.len(), 1, "{messages:#?}");
        assert_eq!(messages[2].role, Role::Tool);
    }
}
