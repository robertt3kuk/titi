//! The model-written summary: the `Remote` strategy of a fold.
//!
//! The structural digest keeps what was asked and how much happened; it
//! cannot say what was decided, what broke, or what comes next, and those are
//! what the model needs to carry on. So the session's own model reads the
//! folded messages and writes the memory. It is told to prefer recall over
//! brevity — a detail left out is lost for good, one kept costs a few tokens —
//! and is capped, in the request and again on what comes back.
//!
//! Any failure — no key, a refusal, a timeout, a cancel, an empty answer —
//! hands the fold back to the structural digest, so compaction never depends
//! on the provider being reachable.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use smol_str::SmolStr;
use titi_providers::{
    ChatMessage, Credential, RequestCtx, Role, StreamEvent, TokenUsage, Transport, WireRequest,
};

use super::clamp_chars;
use super::digest::{digest_summary, is_digest};
use super::mask::{call_for, target_of};
use crate::registry::ResolvedModel;
use crate::runtime::TransportResolver;

/// The `modelRoles` slot that picks the summary model. Unset, the session's
/// own model writes it.
pub const COMPACT_ROLE: &str = "compact";

/// What the request lets the model write.
const MAX_OUTPUT_TOKENS: u32 = 4_096;

/// What is kept of an answer from a provider that ignored the cap.
const MAX_SUMMARY_CHARS: usize = 16_000;

/// A summary nobody can wait for forever; a cancel ends it sooner.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

/// How often a silent stream is checked for a cancel.
const POLL: Duration = Duration::from_millis(100);

/// How much of the window the folded messages may take up in the request:
/// the answer and the instructions need room too.
const WINDOW_SHARE_CHARS: u64 = 2;
const DEFAULT_INPUT_CHARS: usize = 200_000;
const MIN_INPUT_CHARS: usize = 20_000;

/// The marker the fake provider keys on, as `[titi-review]` does for a review.
const MARKER: &str = "[titi-compact]";

/// Why no summary was written.
#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    #[error("cancelled")]
    Cancelled,
    #[error("no answer within {0} seconds")]
    TimedOut(u64),
    #[error("the provider refused: {0}")]
    Provider(String),
    #[error("the model wrote no summary")]
    Empty,
}

/// A written summary and what the call cost.
#[derive(Debug)]
pub struct Summary {
    pub text: String,
    pub usage: Option<TokenUsage>,
}

/// The model that writes the summary, with everything one call needs.
#[derive(Clone)]
pub struct SummaryModel {
    pub label: SmolStr,
    wire_model: SmolStr,
    transport: Arc<dyn Transport>,
    credential: Option<Credential>,
    aborted: Arc<AtomicBool>,
    timeout: Duration,
}

impl SummaryModel {
    pub fn new(
        label: impl Into<SmolStr>,
        resolved: ResolvedModel,
        aborted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            label: label.into(),
            wire_model: resolved.wire_model,
            transport: resolved.transport,
            credential: resolved.credential,
            aborted,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Asks the model for the memory of `prefix`.
    pub async fn summarize(
        &self,
        prefix: &[ChatMessage],
        focus: Option<&str>,
        context_window: u64,
    ) -> Result<Summary, SummaryError> {
        if self.aborted.load(Ordering::SeqCst) {
            return Err(SummaryError::Cancelled);
        }
        let mut request = WireRequest::new(self.wire_model.clone());
        request.system = Some(instructions(focus).into());
        request.max_tokens = Some(MAX_OUTPUT_TOKENS);
        request.messages.push(ChatMessage {
            role: Role::User,
            content: format!(
                "<transcript>\n{}\n</transcript>",
                transcript(prefix, context_window)
            )
            .into(),
            ..Default::default()
        });
        let context = RequestCtx {
            credential: self.credential.clone(),
            aborted: Arc::clone(&self.aborted),
        };
        let call = async {
            let mut stream = self
                .transport
                .stream(request, context)
                .await
                .map_err(|error| SummaryError::Provider(error.to_string()))?;
            let mut text = String::new();
            let mut usage = None;
            loop {
                tokio::select! {
                    event = stream.next() => match event {
                        Some(StreamEvent::TextDelta { text: delta, .. }) => text.push_str(&delta),
                        Some(StreamEvent::Usage(reported)) => usage = Some(reported),
                        Some(StreamEvent::Error { message, .. }) => {
                            return Err(SummaryError::Provider(message.to_string()));
                        }
                        Some(StreamEvent::Done { .. }) => break,
                        Some(_) => {}
                        None => {
                            return Err(SummaryError::Provider(
                                "the stream ended without a terminal event".into(),
                            ));
                        }
                    },
                    _ = tokio::time::sleep(POLL) => {
                        if self.aborted.load(Ordering::SeqCst) {
                            return Err(SummaryError::Cancelled);
                        }
                    }
                }
            }
            Ok((text, usage))
        };
        let (text, usage) = tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| SummaryError::TimedOut(self.timeout.as_secs()))??;
        let text = text.trim();
        if text.is_empty() {
            return Err(SummaryError::Empty);
        }
        Ok(Summary {
            text: clamp_chars(text, MAX_SUMMARY_CHARS),
            usage,
        })
    }
}

/// The model behind the summary: the `compact` role when the settings name
/// one and it resolves, else `chat_model`. `None` when neither resolves, which
/// leaves the fold to the structural digest.
pub async fn summary_model(
    resolver: &dyn TransportResolver,
    chat_model: &str,
    agent_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    aborted: Arc<AtomicBool>,
) -> Option<SummaryModel> {
    let role = match (
        agent_dir,
        workspace.or_else(|| std::env::current_dir().ok()),
    ) {
        (Some(agent_dir), Some(workspace)) => {
            let chat = chat_model.to_owned();
            tokio::task::spawn_blocking(move || {
                let settings =
                    titi_config::settings::Settings::load(&agent_dir, &workspace, &[]).ok()?;
                titi_config::roles::resolve_model_role(&settings, COMPACT_ROLE, &chat).ok()
            })
            .await
            .ok()
            .flatten()
        }
        _ => None,
    };
    role.iter()
        .map(String::as_str)
        .chain([chat_model])
        .find_map(|id| {
            resolver
                .resolve(id)
                .ok()
                .map(|resolved| SummaryModel::new(id, resolved, Arc::clone(&aborted)))
        })
}

/// What the model is told to write.
fn instructions(focus: Option<&str>) -> String {
    let mut text = format!(
        "{MARKER} You write the memory of a coding agent whose conversation is about to be cut \
short. The agent will carry on from your summary alone, so anything you leave out is lost to it. \
Prefer recall over brevity: keep every concrete detail the next steps could need — exact file \
paths, symbol and function names, commands, error messages, numbers, identifiers, and the user's \
own wording for requirements. Do not call tools. Do not answer or continue the conversation; only \
summarize it. If the transcript opens with an earlier summary, fold what it holds into yours.\n\n\
Write these sections, in this order, under these exact headings (write \"none\" under a heading \
with nothing to report):\n\n\
## Goal\n\
What the user is trying to achieve, in the user's terms, including any change of direction.\n\
## Decisions made\n\
Choices and the reasons for them; approaches ruled out.\n\
## Files and symbols touched\n\
One line per file: path — symbols — what changed, or what was learned from reading it.\n\
## Commands run and outcomes\n\
Each command with its result: passed, failed, the line that mattered.\n\
## Unresolved errors and failing checks\n\
Anything still broken, with the exact message and where it happens.\n\
## User preferences and constraints\n\
Instructions, style rules and limits the user stated that must keep holding.\n\
## Next step\n\
The one thing to do next, concretely.\n\n\
Stay under about 1500 words."
    );
    if let Some(focus) = focus.map(str::trim).filter(|focus| !focus.is_empty()) {
        text.push_str(&format!(
            "\n\nThe user asked for this compaction to keep particular detail on: {focus}. \
Give everything about it in full."
        ));
    }
    text
}

/// How much of each kind of message the transcript keeps.
#[derive(Clone, Copy)]
struct Clamps {
    user: usize,
    assistant: usize,
    arguments: usize,
    head: usize,
    tail: usize,
}

const FULL: Clamps = Clamps {
    user: 6_000,
    assistant: 4_000,
    arguments: 400,
    head: 1_200,
    tail: 800,
};

const TIGHT: Clamps = Clamps {
    user: 1_500,
    assistant: 1_000,
    arguments: 100,
    head: 300,
    tail: 200,
};

/// The folded messages as text the model can read, small enough to leave the
/// answer room in the window: clamped per message first, tighter if that is
/// not enough, and cut in the middle as the last resort, so the opening
/// request and the latest state both survive.
fn transcript(prefix: &[ChatMessage], context_window: u64) -> String {
    let budget = match context_window {
        0 => DEFAULT_INPUT_CHARS,
        window => usize::try_from(window.saturating_mul(WINDOW_SHARE_CHARS))
            .unwrap_or(DEFAULT_INPUT_CHARS)
            .max(MIN_INPUT_CHARS),
    };
    let full = render(prefix, FULL);
    if full.len() <= budget {
        return full;
    }
    let tight = render(prefix, TIGHT);
    if tight.len() <= budget {
        return tight;
    }
    let head_end = floor_boundary(&tight, budget * 3 / 10);
    let tail_start = ceil_boundary(&tight, tight.len() - budget * 7 / 10);
    format!(
        "{}\n[… the middle of the transcript is omitted …]\n{}",
        &tight[..head_end],
        &tight[tail_start..]
    )
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

fn render(prefix: &[ChatMessage], clamps: Clamps) -> String {
    let mut out = String::new();
    for (at, message) in prefix.iter().enumerate() {
        match message.role {
            Role::User if is_digest(message) => {
                out.push_str("[earlier summary]\n");
                out.push_str(&clamp_chars(digest_summary(message), MAX_SUMMARY_CHARS));
            }
            Role::User => {
                out.push_str("[user]\n");
                out.push_str(&clamp_chars(&message.content, clamps.user));
            }
            Role::Assistant => {
                if !message.content.is_empty() {
                    out.push_str("[assistant]\n");
                    out.push_str(&clamp_chars(&message.content, clamps.assistant));
                    out.push('\n');
                }
                for call in &message.tool_calls {
                    out.push_str(&format!(
                        "[assistant called {} {}]\n",
                        call.name,
                        clamp_chars(&call.arguments, clamps.arguments)
                    ));
                }
            }
            Role::Tool => {
                let call = call_for(prefix, at);
                out.push_str("[result of ");
                out.push_str(call.map_or("a tool", |call| call.name.as_str()));
                if let Some(target) = call.and_then(target_of) {
                    out.push(' ');
                    out.push_str(&target);
                }
                out.push_str("]\n");
                out.push_str(&excerpt(&message.content, clamps.head, clamps.tail));
            }
            Role::System => {
                out.push_str("[system]\n");
                out.push_str(&clamp_chars(&message.content, clamps.assistant));
            }
        }
        out.push_str("\n\n");
    }
    out
}

/// The start and the end of a long result: a command's outcome is usually at
/// its tail, a file's shape at its head.
fn excerpt(text: &str, head: usize, tail: usize) -> String {
    let total = text.chars().count();
    if total <= head + tail {
        return text.to_owned();
    }
    let head_text: String = text.chars().take(head).collect();
    let tail_start = text
        .char_indices()
        .nth(total - tail)
        .map_or(text.len(), |(index, _)| index);
    format!(
        "{head_text}\n[… {} chars omitted …]\n{}",
        total - head - tail,
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use titi_providers::{
        ApiKind, EventStream, MockBody, MockTransport, StopReason, ToolCallRef, TransportError,
    };

    fn message(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: text.into(),
            ..Default::default()
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            tool_calls: vec![ToolCallRef {
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

    fn reply(text: &str) -> MockBody {
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: titi_providers::BlockId::new("text"),
                text: text.into(),
            },
            StreamEvent::Usage(TokenUsage {
                prompt_tokens: 700,
                completion_tokens: 90,
                cached_tokens: 0,
            }),
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])
    }

    fn model(transport: Arc<dyn Transport>, aborted: Arc<AtomicBool>) -> SummaryModel {
        SummaryModel::new(
            "scripted/summarizer",
            ResolvedModel::without_credential("scripted/summarizer", transport),
            aborted,
        )
    }

    /// A provider that accepts the request and never answers.
    struct Silent;

    #[async_trait::async_trait]
    impl Transport for Silent {
        fn api(&self) -> ApiKind {
            ApiKind::OpenAiCompletions
        }

        async fn stream(
            &self,
            _req: WireRequest,
            _ctx: RequestCtx,
        ) -> Result<EventStream, TransportError> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    fn prefix() -> Vec<ChatMessage> {
        vec![
            message(Role::User, "make the parser accept trailing commas"),
            call("c1", "bash", r#"{"command":"cargo test -p parser"}"#),
            result(
                "c1",
                &format!("{}\ntest result: FAILED. 1 failed", "ok\n".repeat(900)),
            ),
            message(Role::Assistant, "one test fails on the empty list"),
        ]
    }

    #[tokio::test]
    async fn the_model_is_asked_with_the_structured_prompt_and_a_readable_transcript() {
        let transport = Arc::new(MockTransport::new(vec![reply("## Goal\ntrailing commas")]));
        let summarizer = model(Arc::clone(&transport) as _, Arc::default());

        let summary = summarizer
            .summarize(&prefix(), Some("the empty list"), 32_000)
            .await
            .expect("a summary");

        assert_eq!(summary.text, "## Goal\ntrailing commas");
        assert_eq!(summary.usage.map(|u| u.completion_tokens), Some(90));
        let requests = transport.requests();
        let sent = &requests[0];
        let system = sent.system.as_deref().expect("instructions");
        for heading in [
            "## Goal",
            "## Decisions made",
            "## Files and symbols touched",
            "## Commands run and outcomes",
            "## Unresolved errors and failing checks",
            "## User preferences and constraints",
            "## Next step",
        ] {
            assert!(system.contains(heading), "{heading} missing: {system}");
        }
        assert!(system.contains("recall over brevity"));
        assert!(system.contains("the empty list"), "the focus is passed on");
        assert_eq!(sent.max_tokens, Some(MAX_OUTPUT_TOKENS));
        assert!(sent.tools.is_empty(), "the summary calls no tools");
        let body = &sent.messages[0].content;
        assert!(body.contains("[user]\nmake the parser accept trailing commas"));
        assert!(body.contains("[result of bash cargo test -p parser]"));
        assert!(
            body.contains("test result: FAILED. 1 failed"),
            "the outcome at the tail of a long result survives: {body}"
        );
        assert!(body.contains("chars omitted"), "the middle is cut");
    }

    #[tokio::test]
    async fn a_refusal_a_stream_error_and_an_empty_answer_are_typed() {
        for (body, expect) in [
            (
                MockBody::Err(TransportError::Fatal {
                    status: Some(401),
                    message: "no key".into(),
                    context_too_long: false,
                }),
                "refused",
            ),
            (
                MockBody::Events(vec![StreamEvent::Error {
                    reason: titi_providers::ErrorReason::Connection,
                    message: "reset".into(),
                }]),
                "reset",
            ),
            (reply("   "), "no summary"),
        ] {
            let transport = Arc::new(MockTransport::new(vec![body]));
            let error = model(transport, Arc::default())
                .summarize(&prefix(), None, 0)
                .await
                .expect_err("no summary");
            assert!(error.to_string().contains(expect), "{error}");
        }
    }

    #[tokio::test]
    async fn a_cancel_ends_a_silent_call() {
        let aborted = Arc::new(AtomicBool::new(false));
        let summarizer = model(Arc::new(Silent), Arc::clone(&aborted));
        let flag = Arc::clone(&aborted);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let error = summarizer
            .summarize(&prefix(), None, 0)
            .await
            .expect_err("cancelled");
        assert!(matches!(error, SummaryError::Cancelled), "{error}");
    }

    #[tokio::test]
    async fn a_cancel_before_the_call_sends_nothing() {
        let transport = Arc::new(MockTransport::new(vec![reply("never asked")]));
        let aborted = Arc::new(AtomicBool::new(true));
        let error = model(Arc::clone(&transport) as _, aborted)
            .summarize(&prefix(), None, 0)
            .await
            .expect_err("cancelled");
        assert!(matches!(error, SummaryError::Cancelled));
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn a_model_that_never_answers_times_out() {
        let summarizer =
            model(Arc::new(Silent), Arc::default()).with_timeout(Duration::from_millis(150));
        let error = summarizer
            .summarize(&prefix(), None, 0)
            .await
            .expect_err("timed out");
        assert!(matches!(error, SummaryError::TimedOut(_)), "{error}");
    }

    #[tokio::test]
    async fn an_answer_past_the_cap_is_cut() {
        let transport = Arc::new(MockTransport::new(vec![reply(
            &"detail ".repeat(MAX_SUMMARY_CHARS),
        )]));
        let summary = model(transport, Arc::default())
            .summarize(&prefix(), None, 0)
            .await
            .expect("a summary");
        assert!(summary.text.chars().count() <= MAX_SUMMARY_CHARS + 1);
        assert!(summary.text.ends_with('…'));
    }

    #[test]
    fn an_earlier_summary_leads_the_transcript_and_a_small_window_cuts_the_middle() {
        let earlier = super::super::digest::digest_message("goal: the lexer", 3, &[]);
        let mut folded = vec![earlier];
        folded.extend((0..400).map(|n| {
            message(
                Role::User,
                &format!("request number {n} {}", "w".repeat(300)),
            )
        }));
        let text = transcript(&folded, 5_000);
        assert!(text.starts_with("[earlier summary]\ngoal: the lexer"));
        assert!(text.contains("the middle of the transcript is omitted"));
        assert!(text.len() <= MIN_INPUT_CHARS + 100, "{}", text.len());
        assert!(
            text.contains("request number 399"),
            "the latest state survives"
        );
    }
}
