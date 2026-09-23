//! Second opinion on the conversation that is happening right now.
//!
//! The advisor reads the exchange and answers in words. It is built with no
//! tools at all, so "it never acts" is a property of the request rather than
//! a promise in a prompt: there is nothing for the model to call.
//!
//! Every way a consult can come back without an opinion — no conversation
//! yet, an unreachable model, a transport error, an empty answer — is an
//! error here. A consult that quietly produced nothing reads on screen like
//! an advisor that had no objection, which is the one thing it must never be
//! mistaken for.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::StreamExt;
use smol_str::SmolStr;
use titi_providers::{ChatMessage, RequestCtx, Role, StreamEvent, WireRequest};

use crate::runtime::TransportResolver;

/// The advisor's standing instructions.
pub const ADVISOR_BRIEF: &str = "\
You are a second opinion on the conversation below. You are not taking part \
in it and you have no tools: you cannot read files, run anything, or change \
anything, so do not offer to.\n\
Say what the other agent is getting wrong, what it is assuming without \
checking, and what you would do differently. Be specific and short. If you \
agree, say so in one line and name the one risk that is still open.";

/// Messages the advisor is shown. Older turns rarely change the judgement
/// and every one of them is paid for.
const CONSULT_MESSAGES: usize = 30;
/// Characters kept per message. A pasted file should not push the rest of
/// the conversation out of the request.
const CONSULT_MESSAGE_CHARS: usize = 2_000;

/// Why a consult produced no opinion.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConsultError {
    #[error("there is no conversation to consult about yet")]
    NoConversation,
    #[error("advisor model {model} is unavailable: {reason}")]
    Unresolved { model: SmolStr, reason: SmolStr },
    #[error("the advisor did not answer: {0}")]
    Transport(SmolStr),
    #[error("the advisor answered with nothing")]
    Empty,
}

/// Asks one model for a second opinion, once.
pub struct Advisor {
    resolver: Arc<dyn TransportResolver>,
    model: SmolStr,
}

impl Advisor {
    pub fn new(resolver: Arc<dyn TransportResolver>, model: impl Into<SmolStr>) -> Self {
        Self {
            resolver,
            model: model.into(),
        }
    }

    /// The advisor's answer, or the reason there is none.
    ///
    /// `question` is what the user typed after `/advisor`; without it the
    /// advisor is asked about the conversation as a whole.
    pub async fn consult(
        &self,
        conversation: &[ChatMessage],
        question: Option<&str>,
    ) -> Result<SmolStr, ConsultError> {
        let transcript = transcript(conversation);
        if transcript.is_empty() {
            return Err(ConsultError::NoConversation);
        }
        let resolved =
            self.resolver
                .resolve(&self.model)
                .map_err(|error| ConsultError::Unresolved {
                    model: self.model.clone(),
                    reason: error.to_string().into(),
                })?;

        let mut wire = WireRequest::new(resolved.wire_model.clone());
        // No tool specs: the advisor has nothing it could call even if the
        // brief failed to convince it.
        wire.messages.push(ChatMessage {
            role: Role::System,
            content: ADVISOR_BRIEF.into(),
            tool_calls: Vec::new(),
        });
        wire.messages.push(ChatMessage {
            role: Role::User,
            content: consult_prompt(&transcript, question).into(),
            tool_calls: Vec::new(),
        });
        let ctx = RequestCtx {
            api_key: resolved.credential.map(|credential| credential.access),
            aborted: Arc::new(AtomicBool::new(false)),
        };

        let mut stream = resolved
            .transport
            .stream(wire, ctx)
            .await
            .map_err(|error| ConsultError::Transport(error.to_string().into()))?;
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event {
                // Thinking is not the opinion, and half of it read as one
                // would be worse than none.
                StreamEvent::TextDelta { text, .. } => answer.push_str(&text),
                StreamEvent::Error { message, .. } => {
                    return Err(ConsultError::Transport(message));
                }
                StreamEvent::Done { .. } => break,
                _ => {}
            }
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(ConsultError::Empty);
        }
        Ok(answer.into())
    }
}

/// What the advisor is asked, around the conversation it is judging.
fn consult_prompt(transcript: &str, question: Option<&str>) -> String {
    let ask = question
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .unwrap_or("Where is this conversation going wrong?");
    format!("<conversation>\n{transcript}\n</conversation>\n\n{ask}")
}

/// Renders the conversation for a reader who did not take part in it.
///
/// Tool traffic is left out: the advisor is judging the reasoning, and a
/// pasted tool dump buys nothing for what it costs.
fn transcript(conversation: &[ChatMessage]) -> String {
    let start = conversation.len().saturating_sub(CONSULT_MESSAGES);
    let mut rendered = Vec::new();
    for message in &conversation[start..] {
        let speaker = match message.role {
            Role::User => "user",
            Role::Assistant => "agent",
            Role::System => "context",
            Role::Tool => continue,
        };
        let body = message.content.trim();
        if body.is_empty() {
            continue;
        }
        rendered.push(format!("{speaker}: {}", clip(body, CONSULT_MESSAGE_CHARS)));
    }
    rendered.join("\n")
}

/// `text` cut to `max` characters, marked where it was cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept} […]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{RegistryError, ResolvedModel};
    use titi_providers::{MockBody, MockTransport, StopReason, Transport};

    struct OneModel(String, Arc<dyn Transport>);

    impl TransportResolver for OneModel {
        fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
            if model == self.0 {
                Ok(ResolvedModel::without_credential(
                    model,
                    Arc::clone(&self.1),
                ))
            } else {
                Err(RegistryError::UnknownModel(model.into()))
            }
        }
    }

    fn advisor(bodies: Vec<MockBody>) -> Advisor {
        let transport: Arc<dyn Transport> = Arc::new(MockTransport::new(bodies));
        Advisor::new(
            Arc::new(OneModel("advisor".to_owned(), transport)),
            "advisor",
        )
    }

    fn said(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: text.into(),
            tool_calls: Vec::new(),
        }
    }

    fn talk(text: &str) -> Vec<MockBody> {
        vec![MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: titi_providers::BlockId::new("0"),
                text: text.into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])]
    }

    #[tokio::test]
    async fn an_answer_comes_back_trimmed() {
        let advisor = advisor(talk("  you skipped the migration  "));
        let opinion = advisor
            .consult(&[said(Role::User, "ship it")], None)
            .await
            .expect("an opinion");
        assert_eq!(opinion.as_str(), "you skipped the migration");
    }

    /// An empty answer must not read as agreement.
    #[tokio::test]
    async fn an_empty_answer_is_a_failed_consult() {
        let advisor = advisor(talk("   \n  "));
        assert_eq!(
            advisor
                .consult(&[said(Role::User, "ship it")], None)
                .await
                .unwrap_err(),
            ConsultError::Empty
        );
    }

    #[tokio::test]
    async fn an_empty_conversation_is_refused_before_a_model_is_called() {
        let advisor = advisor(talk("never asked"));
        assert_eq!(
            advisor.consult(&[], None).await.unwrap_err(),
            ConsultError::NoConversation
        );
    }

    #[tokio::test]
    async fn an_unknown_model_names_itself_in_the_failure() {
        let transport: Arc<dyn Transport> = Arc::new(MockTransport::new(talk("hi")));
        let advisor = Advisor::new(
            Arc::new(OneModel("configured".to_owned(), transport)),
            "missing",
        );
        match advisor
            .consult(&[said(Role::User, "ship it")], None)
            .await
            .unwrap_err()
        {
            ConsultError::Unresolved { model, .. } => assert_eq!(model.as_str(), "missing"),
            other => panic!("expected an unresolved model, got {other:?}"),
        }
    }

    /// The request must carry no tools: that is what stops the advisor from
    /// acting, and a brief alone would not.
    #[tokio::test]
    async fn the_advisor_is_given_no_tools_and_sees_the_conversation() {
        let transport = Arc::new(MockTransport::new(talk("fine")));
        let captured = Arc::clone(&transport);
        let advisor = Advisor::new(
            Arc::new(OneModel("advisor".to_owned(), transport)),
            "advisor",
        );
        advisor
            .consult(
                &[
                    said(Role::User, "rename the column"),
                    said(Role::Assistant, "done, no migration needed"),
                    said(Role::Tool, "tool output nobody needs"),
                ],
                Some("is that safe?"),
            )
            .await
            .expect("an opinion");

        let requests = captured.requests();
        let request = requests.first().expect("one request");
        assert!(request.tools.is_empty(), "{:?}", request.tools);
        let prompt = &request.messages.last().expect("a prompt").content;
        assert!(prompt.contains("user: rename the column"), "{prompt}");
        assert!(prompt.contains("agent: done, no migration"), "{prompt}");
        assert!(!prompt.contains("nobody needs"), "{prompt}");
        assert!(prompt.contains("is that safe?"), "{prompt}");
    }

    #[test]
    fn a_long_message_is_clipped_not_dropped() {
        let long = "x".repeat(CONSULT_MESSAGE_CHARS + 500);
        let rendered = transcript(&[said(Role::User, &long)]);
        assert!(rendered.ends_with("[…]"), "{}", &rendered[..40]);
        assert!(rendered.chars().count() < long.chars().count());
    }
}
