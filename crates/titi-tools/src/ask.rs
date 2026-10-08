//! The `ask` tool: the model puts a question to the user and waits for the
//! answer.
//!
//! Read-tier, and no approval prompt: asking changes nothing, and a question
//! that had to be approved before it could be asked would be two dialogs where
//! the user asked for none. Plan mode keeps it for the same reason it keeps
//! `read` — a plan that guesses at a requirement the user could have stated in
//! one line is a worse plan.
//!
//! The tool owns no surface. It hands the question to an [`AskSink`] the
//! session installs (see [`ToolHandler::set_ask`]) and waits: the answer, or a
//! cancellation, comes back through that door. A session with no door — a
//! subagent, a headless one-shot with no client attached — is told so rather
//! than left hanging.
//!
//! There is deliberately **no timeout**. A deadline would have to answer on the
//! user's behalf, and a guessed answer presented as theirs is worse than no
//! answer at all; the turn is interrupted, not timed out.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use titi_providers::ToolSpec;

use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// Most choices one question may offer. A dialog longer than this is not a
/// question, it is a document, and the model should ask it in prose.
pub const MAX_ASK_OPTIONS: usize = 20;
/// Most characters one question holds.
pub const MAX_ASK_CHARS: usize = 1_000;
/// Characters of the question the tool chip shows.
const CHIP_CHARS: usize = 60;

/// One question the model wants the user to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    /// The question, as the model wrote it.
    pub question: String,
    /// The choices offered, in the model's order. Empty is a question with no
    /// list: the user answers in their own words.
    pub options: Vec<String>,
    /// Whether more than one choice may be taken.
    pub multi: bool,
    /// Whether the user may answer in their own words *besides* the list.
    ///
    /// True unless the model turned it off: a list the model wrote cannot know
    /// it is complete, and a user with the real answer in mind must be able to
    /// give it.
    pub free_text: bool,
}

/// What the user answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskAnswer {
    /// The choices taken, in the order they were offered.
    Chosen(Vec<String>),
    /// The user's own words, when the list did not hold their answer.
    Text(String),
    /// Nobody answered: the turn was cancelled, or the surface went away.
    Cancelled,
}

/// The session's door for asking the user.
///
/// Implemented by the engine, which turns one `ask` into one event and one
/// blocking wait; a test can implement it with a scripted answer. The tool
/// knows nothing else about where a question goes.
#[async_trait]
pub trait AskSink: Send + Sync + 'static {
    /// Put the question to the surface and wait for the answer.
    ///
    /// [`AskAnswer::Cancelled`] is the answer when the turn is interrupted or
    /// the surface goes away: an unanswered question is never a yes.
    async fn ask(&self, request: AskRequest) -> AskAnswer;
}

/// Why a call was refused. The text is what the model reads.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AskError {
    EmptyQuestion,
    TooLong { chars: usize },
    TooManyOptions(usize),
    EmptyOption(usize),
    DuplicateOption(String),
    MultiWithoutOptions,
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyQuestion => f.write_str("question must not be empty"),
            Self::TooLong { chars } => write!(
                f,
                "the question is {chars} characters; keep it to {MAX_ASK_CHARS}"
            ),
            Self::TooManyOptions(count) => write!(
                f,
                "{count} options; a question offers at most {MAX_ASK_OPTIONS}"
            ),
            Self::EmptyOption(index) => write!(f, "option {} has no label", index + 1),
            Self::DuplicateOption(label) => {
                write!(f, "option `{label}` is offered twice; labels must differ")
            }
            Self::MultiWithoutOptions => {
                f.write_str("multi needs options: there is nothing to select more than one of")
            }
        }
    }
}

/// Read the arguments into a request, or say why they are not one.
///
/// Shared by [`ToolHandler::refusal`] and [`ToolHandler::invoke`], so a call
/// the engine refuses before approval is the same call the tool would refuse
/// if it were invoked directly.
fn read(args: &Value) -> Result<AskRequest, AskError> {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if question.is_empty() {
        return Err(AskError::EmptyQuestion);
    }
    if question.chars().count() > MAX_ASK_CHARS {
        return Err(AskError::TooLong {
            chars: question.chars().count(),
        });
    }
    let mut options = Vec::new();
    if let Some(list) = args.get("options").and_then(Value::as_array) {
        if list.len() > MAX_ASK_OPTIONS {
            return Err(AskError::TooManyOptions(list.len()));
        }
        for (index, option) in list.iter().enumerate() {
            let label = option.as_str().unwrap_or_default().trim();
            if label.is_empty() {
                return Err(AskError::EmptyOption(index));
            }
            if options.iter().any(|known| known == label) {
                return Err(AskError::DuplicateOption(label.to_owned()));
            }
            options.push(label.to_owned());
        }
    }
    let multi = args.get("multi").and_then(Value::as_bool).unwrap_or(false);
    if multi && options.is_empty() {
        return Err(AskError::MultiWithoutOptions);
    }
    Ok(AskRequest {
        question,
        options,
        multi,
        // Absent means allowed: the list the model wrote is not the only
        // possible answer.
        free_text: args
            .get("free_text")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

/// What the model reads for one answer.
fn render(request: &AskRequest, answer: &AskAnswer) -> (String, bool) {
    match answer {
        AskAnswer::Chosen(picked) if picked.is_empty() => (
            "The user did not choose any option; ask again with a list they can answer, or \
             continue without the answer."
                .to_owned(),
            false,
        ),
        AskAnswer::Chosen(picked) if picked.len() == 1 => {
            (format!("The user chose: {}", picked[0]), false)
        }
        AskAnswer::Chosen(picked) => (format!("The user chose: {}", picked.join(", ")), false),
        AskAnswer::Text(text) => (format!("The user answered: {text}"), false),
        // Not an error the model should retry inside this turn: the turn is
        // over. It is marked as one so a transcript reads as "no answer" rather
        // than as an answer of silence.
        AskAnswer::Cancelled => (
            format!(
                "The user did not answer `{}`: the turn was cancelled. Do not assume an answer.",
                request.question
            ),
            true,
        ),
    }
}

/// The `ask` tool. See the module documentation for what it promises.
pub struct AskTool {
    /// The session's door, installed once after the registry is built. Empty
    /// is a session with no surface: the tool says so rather than waiting
    /// forever.
    ///
    /// A `OnceLock` because the tool is shared behind `Arc<dyn ToolHandler>`
    /// and reached only through `&self`: the session installs its door while
    /// the registry is being built, before any turn can call the tool, and a
    /// second install is the same session's door arriving again — the first
    /// one is kept rather than replaced.
    sink: std::sync::OnceLock<std::sync::Arc<dyn AskSink>>,
}

impl AskTool {
    pub fn new() -> Self {
        Self {
            sink: std::sync::OnceLock::new(),
        }
    }
}

impl Default for AskTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolHandler for AskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "ask".into(),
                description: "Ask the user a question and wait for their answer. Use it when \
                              the task turns on something only they know — a preference, a \
                              credential, which of two designs they want — instead of guessing. \
                              Offer `options` when the choices are enumerable; the user can \
                              always answer in their own words unless `free_text` is false. \
                              The turn stops until they answer or cancel it, so ask one thing \
                              at a time and do not use it to confirm what you can verify \
                              yourself."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "What to ask, in one sentence."
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "The choices, in the order to show them. Omit for a \
                                            free-text question."
                        },
                        "multi": {
                            "type": "boolean",
                            "description": "Whether the user may choose more than one option. \
                                            Default false."
                        },
                        "free_text": {
                            "type": "boolean",
                            "description": "Whether the user may answer in their own words \
                                            besides the options. Default true; set false only \
                                            when the list really is exhaustive."
                        }
                    },
                    "required": ["question"]
                }),
            },
            // Asking reaches nothing outside the session, so nothing has to be
            // approved before it can be asked.
            approval: ApprovalTier::Read,
        }
    }

    fn describe(&self, args: &Value) -> Option<String> {
        let question = args.get("question").and_then(Value::as_str)?;
        let question = question.trim();
        if question.is_empty() {
            return None;
        }
        let mut chip: String = question.chars().take(CHIP_CHARS).collect();
        if question.chars().count() > CHIP_CHARS {
            chip.push('…');
        }
        Some(chip)
    }

    fn refusal(&self, args: &Value) -> Option<String> {
        read(args).err().map(|error| error.to_string())
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let request = match read(&args) {
            Ok(request) => request,
            Err(error) => {
                return ToolResult {
                    output: error.to_string().into(),
                    is_error: true,
                    detail: None,
                };
            }
        };
        let Some(sink) = self.sink.get() else {
            return ToolResult {
                output: "there is no user to ask in this session: a subagent runs without a \
                         surface, so return to your caller and let it ask."
                    .into(),
                is_error: true,
                detail: None,
            };
        };
        let answer = sink.ask(request.clone()).await;
        let (output, is_error) = render(&request, &answer);
        let detail = match &answer {
            AskAnswer::Chosen(picked) => {
                Some(format!("{} → {}", request.question, picked.join(", ")))
            }
            AskAnswer::Text(text) => Some(format!("{} → {text}", request.question)),
            AskAnswer::Cancelled => Some(format!("{} → no answer", request.question)),
        };
        ToolResult {
            output: output.into(),
            is_error,
            detail: detail.map(Into::into),
        }
    }

    fn set_ask(&self, sink: std::sync::Arc<dyn AskSink>) {
        // `set` refuses a second install, which is the honest outcome: the
        // first door belongs to the session that built this registry, and a
        // later one would be a different session reaching the same tool.
        let _ = self.sink.set(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A sink that answers with whatever the test scripted.
    struct Scripted(AskAnswer);

    #[async_trait]
    impl AskSink for Scripted {
        async fn ask(&self, _request: AskRequest) -> AskAnswer {
            self.0.clone()
        }
    }

    fn tool(answer: Option<AskAnswer>) -> AskTool {
        let tool = AskTool::new();
        if let Some(answer) = answer {
            tool.set_ask(Arc::new(Scripted(answer)));
        }
        tool
    }

    #[tokio::test]
    async fn an_answer_becomes_the_tool_result_the_model_reads() {
        let tool = tool(Some(AskAnswer::Chosen(vec!["blue".to_owned()])));
        let result = tool
            .invoke(json!({"question": "Which colour?", "options": ["blue", "red"]}))
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "The user chose: blue");
        assert_eq!(result.detail.as_deref(), Some("Which colour? → blue"));
    }

    #[tokio::test]
    async fn free_text_is_an_answer_and_several_choices_read_as_a_list() {
        let typed = tool(Some(AskAnswer::Text("neither, use green".to_owned())));
        let result = typed.invoke(json!({"question": "Which colour?"})).await;
        assert_eq!(result.output, "The user answered: neither, use green");
        assert!(!result.is_error);

        let many = tool(Some(AskAnswer::Chosen(vec![
            "blue".to_owned(),
            "red".to_owned(),
        ])));
        let result = many
            .invoke(json!({"question": "Which?", "options": ["blue", "red"], "multi": true}))
            .await;
        assert_eq!(result.output, "The user chose: blue, red");
    }

    #[tokio::test]
    async fn a_cancelled_question_says_so_and_is_not_an_answer() {
        let tool = tool(Some(AskAnswer::Cancelled));
        let result = tool.invoke(json!({"question": "Deploy now?"})).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("did not answer"), "{result:?}");
        assert!(result.output.contains("turn was cancelled"), "{result:?}");
    }

    #[tokio::test]
    async fn a_session_with_no_surface_refuses_instead_of_waiting() {
        let tool = tool(None);
        let result = tool.invoke(json!({"question": "Which colour?"})).await;
        assert!(result.is_error);
        assert!(result.output.contains("no user to ask"), "{result:?}");
    }

    /// The engine refuses the call before approval with the same words the
    /// tool would use, so nobody approves a question that cannot be asked.
    #[tokio::test]
    async fn the_refusal_and_the_invocation_agree_about_bad_arguments() {
        let tool = AskTool::new();
        for args in [
            json!({}),
            json!({"question": "   "}),
            json!({"question": "ok?", "options": ["a", "a"]}),
            json!({"question": "ok?", "options": [""]}),
            json!({"question": "ok?", "multi": true}),
        ] {
            let refusal = tool.refusal(&args).expect("refused");
            let invoked = tool.invoke(args).await;
            assert!(invoked.is_error, "{invoked:?}");
            assert_eq!(refusal, invoked.output, "the two refusals must agree");
        }
        assert!(tool.refusal(&json!({"question": "ok?"})).is_none());
    }

    #[test]
    fn the_schema_offers_one_question_and_a_default_of_free_text() {
        let definition = AskTool::new().definition();
        assert_eq!(definition.spec.name, "ask");
        assert_eq!(definition.approval, ApprovalTier::Read);
        let required = definition.spec.parameters["required"].clone();
        assert_eq!(required, json!(["question"]));
        let request = read(&json!({"question": "Which?"})).expect("readable");
        assert!(request.free_text, "a list is not the only possible answer");
        assert!(request.options.is_empty());
        assert!(!request.multi);
    }
}
