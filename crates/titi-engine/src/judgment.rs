//! Judgment: a small, fast model that answers one closed question so a
//! capability can improve on its own default.
//!
//! Every path here is best-effort. A capability that has not opted in never
//! reaches the model at all; an opted-in one gets `None` — "no judgment" —
//! whenever the model is absent, slow, broken, or answers something that is
//! not one of the shapes it was asked for. `None` always means "keep the
//! behaviour you would have had without me", so a judgment can improve a
//! decision and never break one, and no caller has to handle an error it
//! cannot act on.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt;
use smol_str::SmolStr;
use titi_providers::{ChatMessage, RequestCtx, Role, StreamEvent, WireRequest};

use crate::runtime::TransportResolver;

/// Role the judgment model is looked up under.
const JUDGMENT_ROLE: &str = "judgment";
/// Role used when the map names no dedicated judge: the cheap model the
/// namer already runs on is the right shape for this too.
const FALLBACK_ROLE: &str = "smol";
/// What the judge is told it is. One token, no prose, no reasoning.
const JUDGMENT_SYSTEM: &str = "You are a decision function inside a coding agent. Answer with the single \
     token the question asks for, nothing else: no prose, no punctuation, no \
     explanation.";
/// A decision nobody is waiting for is worth nothing: the caller has a
/// default and takes it the moment this elapses.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(200);
/// One token of answer, plus slack for a chatty tokenizer.
const MAX_TOKENS: u32 = 8;
/// Options a single choice may offer. Past this the prompt is no longer
/// small, the answer is no longer fast, and the caller's own ranking is
/// better than a guess.
pub const MAX_OPTIONS: usize = 16;

/// Why one ask of the judgment model produced nothing.
///
/// Callers never see this: [`JudgmentProvider`] turns every variant into "no
/// judgment". It exists so an implementation of [`JudgmentModel`] can fail
/// precisely instead of swallowing the reason.
#[derive(Debug, thiserror::Error)]
pub enum JudgmentError {
    #[error("no model resolves the `{JUDGMENT_ROLE}` or `{FALLBACK_ROLE}` role")]
    NoModel,
    #[error("judgment transport failed: {0}")]
    Transport(String),
    #[error("judgment stream failed: {0}")]
    Stream(String),
    #[error("judgment model answered nothing")]
    Empty,
}

/// A place in the agent that may ask for a judgment.
///
/// Opting in is per capability, because the cost is paid per capability: a
/// turn that asks for none spends nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Which skill, if any, the request in front of the agent matches.
    SkillHint,
    /// Which of several file-search hits the user meant.
    FileSearch,
    /// Whether a provider error is worth another attempt.
    ProviderError,
    /// Whether an action touching something sensitive needs confirmation.
    SensitiveClick,
}

impl Capability {
    /// Every capability, in config order.
    pub const ALL: [Capability; 4] = [
        Capability::SkillHint,
        Capability::FileSearch,
        Capability::ProviderError,
        Capability::SensitiveClick,
    ];

    /// How the capability is spelled in settings.
    pub fn name(self) -> &'static str {
        match self {
            Capability::SkillHint => "skillHint",
            Capability::FileSearch => "fileSearch",
            Capability::ProviderError => "providerError",
            Capability::SensitiveClick => "sensitiveClick",
        }
    }

    /// Reads a settings spelling. An unknown name is not a capability.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        Capability::ALL
            .into_iter()
            .find(|capability| capability.name().eq_ignore_ascii_case(name))
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// The set of capabilities allowed to ask. Empty by default: judgment is
/// opt-in, so a surface that says nothing gets the built-in behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities(u8);

impl Capabilities {
    /// Nothing opted in.
    pub const NONE: Self = Self(0);

    /// Everything opted in.
    pub fn all() -> Self {
        Capability::ALL
            .into_iter()
            .fold(Self::NONE, |set, capability| set.with(capability))
    }

    /// Reads the settings list, ignoring names that are not capabilities.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        names
            .into_iter()
            .filter_map(|name| Capability::from_name(name.as_ref()))
            .fold(Self::NONE, |set, capability| set.with(capability))
    }

    #[must_use]
    pub fn with(self, capability: Capability) -> Self {
        Self(self.0 | capability.bit())
    }

    pub fn allows(self, capability: Capability) -> bool {
        self.0 & capability.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A yes/no question. Borrowed throughout: asking costs no allocation until
/// the prompt is actually built, which an opted-out capability never reaches.
#[derive(Debug, Clone, Copy)]
pub struct Confirm<'a> {
    pub question: &'a str,
    /// What the judge needs to know to answer — a command line, a path, an
    /// error body.
    pub context: Option<&'a str>,
}

impl<'a> Confirm<'a> {
    pub fn new(question: &'a str) -> Self {
        Self {
            question,
            context: None,
        }
    }

    #[must_use]
    pub fn with_context(mut self, context: &'a str) -> Self {
        self.context = Some(context);
        self
    }
}

/// A pick-one-of-N question.
#[derive(Debug, Clone, Copy)]
pub struct Choice<'a, S> {
    pub question: &'a str,
    pub options: &'a [S],
}

impl<'a, S: AsRef<str>> Choice<'a, S> {
    pub fn new(question: &'a str, options: &'a [S]) -> Self {
        Self { question, options }
    }
}

/// One ask of the judgment model.
///
/// The whole model surface is this method, so the provider's rules — opt-in,
/// timeout, defensive parsing — are written once and tested against a fake.
#[async_trait::async_trait]
pub trait JudgmentModel: Send + Sync + 'static {
    async fn ask(&self, prompt: &str) -> Result<String, JudgmentError>;
}

/// Opt-in judgment for a set of capabilities.
#[derive(Clone)]
pub struct JudgmentProvider {
    model: Arc<dyn JudgmentModel>,
    capabilities: Capabilities,
    timeout: Duration,
}

impl std::fmt::Debug for JudgmentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgmentProvider")
            .field("capabilities", &self.capabilities)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl JudgmentProvider {
    pub fn new(model: Arc<dyn JudgmentModel>, capabilities: Capabilities) -> Self {
        Self {
            model,
            capabilities,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.capabilities.allows(capability)
    }

    /// `Some(true)`/`Some(false)` only for an answer that is unambiguously
    /// one of them; anything else is no judgment.
    pub async fn confirm(&self, capability: Capability, question: &Confirm<'_>) -> Option<bool> {
        if !self.capabilities.allows(capability) || question.question.trim().is_empty() {
            return None;
        }
        let answer = self.ask(confirm_prompt(question)).await?;
        parse_confirm(&answer)
    }

    /// The index of the chosen option, or no judgment.
    pub async fn choose<S: AsRef<str>>(
        &self,
        capability: Capability,
        choice: &Choice<'_, S>,
    ) -> Option<usize> {
        if !self.capabilities.allows(capability) || choice.question.trim().is_empty() {
            return None;
        }
        let count = choice.options.len();
        // One option is not a choice; past the cap the prompt stops being
        // small and the caller's own ranking is the better answer.
        if !(2..=MAX_OPTIONS).contains(&count) {
            return None;
        }
        if choice
            .options
            .iter()
            .any(|option| option.as_ref().trim().is_empty())
        {
            return None;
        }
        let answer = self.ask(choice_prompt(choice)).await?;
        parse_choice(&answer, count)
    }

    /// The one place a judgment is allowed to cost time, and the only place
    /// it is capped: whatever the model does — slow, hung, never answering —
    /// the caller is back with its default when the timeout elapses.
    async fn ask(&self, prompt: String) -> Option<String> {
        tokio::time::timeout(self.timeout, self.model.ask(&prompt))
            .await
            .ok()?
            .ok()
    }
}

/// Asks through an optional provider: no provider is no judgment, so a
/// caller needs one `await` and no branch.
pub async fn confirm(
    provider: Option<&JudgmentProvider>,
    capability: Capability,
    question: &Confirm<'_>,
) -> Option<bool> {
    provider?.confirm(capability, question).await
}

/// Choice half of [`confirm`].
pub async fn choose<S: AsRef<str>>(
    provider: Option<&JudgmentProvider>,
    capability: Capability,
    choice: &Choice<'_, S>,
) -> Option<usize> {
    provider?.choose(capability, choice).await
}

fn confirm_prompt(question: &Confirm<'_>) -> String {
    let mut prompt = String::with_capacity(128);
    prompt.push_str(question.question.trim());
    if let Some(context) = question.context {
        let context = context.trim();
        if !context.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(context);
        }
    }
    prompt.push_str("\n\nAnswer YES or NO.");
    prompt
}

fn choice_prompt<S: AsRef<str>>(choice: &Choice<'_, S>) -> String {
    let mut prompt = String::with_capacity(256);
    prompt.push_str(choice.question.trim());
    prompt.push_str("\n\n");
    for (index, option) in choice.options.iter().enumerate() {
        prompt.push_str(&(index + 1).to_string());
        prompt.push_str(". ");
        prompt.push_str(option.as_ref().trim());
        prompt.push('\n');
    }
    prompt.push_str("\nAnswer with the number of the best option only.");
    prompt
}

/// Reads a decision out of whatever the model said.
///
/// Only whole words count, so `not` is not a `no`, and a label the model
/// could not resist (`Answer: YES`) still parses. An answer that names both
/// decisions, or neither, decided nothing.
fn parse_confirm(answer: &str) -> Option<bool> {
    let mut decision: Option<bool> = None;
    for word in answer.split(|c: char| !c.is_ascii_alphanumeric()) {
        let value = if word.eq_ignore_ascii_case("yes")
            || word.eq_ignore_ascii_case("y")
            || word.eq_ignore_ascii_case("true")
        {
            true
        } else if word.eq_ignore_ascii_case("no")
            || word.eq_ignore_ascii_case("n")
            || word.eq_ignore_ascii_case("false")
        {
            false
        } else {
            continue;
        };
        match decision {
            Some(seen) if seen != value => return None,
            Some(_) => {}
            None => decision = Some(value),
        }
    }
    decision
}

fn parse_choice(answer: &str, options: usize) -> Option<usize> {
    let mut picked: Option<usize> = None;
    for run in answer.split(|c: char| !c.is_ascii_digit()) {
        if run.is_empty() {
            continue;
        }
        let Ok(value) = run.parse::<usize>() else {
            return None;
        };
        match picked {
            // Two different numbers: the judge did not pick one.
            Some(seen) if seen != value => return None,
            Some(_) => {}
            None => picked = Some(value),
        }
    }
    let value = picked?;
    if value == 0 || value > options {
        return None;
    }
    Some(value - 1)
}

/// The judgment model the role map points at.
///
/// Mirrors the namer: the `judgment` role first, the `smol` role when the map
/// does not name a judge, and the turn's own model when there is no map at
/// all. Nothing here hardcodes a model id.
pub struct RoleJudgmentModel {
    resolver: Arc<dyn TransportResolver>,
    /// Agent directory the settings layers are read from.
    agent_dir: PathBuf,
    /// Project the settings layers are read for.
    workspace: PathBuf,
    /// Model the turn runs on: what a role resolves to when the user
    /// configured no role map.
    current_model: SmolStr,
}

impl RoleJudgmentModel {
    pub fn new(
        resolver: Arc<dyn TransportResolver>,
        agent_dir: PathBuf,
        workspace: PathBuf,
        current_model: impl Into<SmolStr>,
    ) -> Self {
        Self {
            resolver,
            agent_dir,
            workspace,
            current_model: current_model.into(),
        }
    }

    async fn model(&self) -> Result<SmolStr, JudgmentError> {
        let agent_dir = self.agent_dir.clone();
        let workspace = self.workspace.clone();
        let current = self.current_model.to_string();
        // Settings come off disk: read on a blocking thread so a judgment
        // never stalls the executor a turn is running on.
        tokio::task::spawn_blocking(move || {
            let settings =
                titi_config::settings::Settings::load(&agent_dir, &workspace, &[]).ok()?;
            titi_config::roles::resolve_model_role(&settings, JUDGMENT_ROLE, &current)
                .or_else(|_| {
                    titi_config::roles::resolve_model_role(&settings, FALLBACK_ROLE, &current)
                })
                .ok()
                .map(SmolStr::from)
        })
        .await
        .ok()
        .flatten()
        .ok_or(JudgmentError::NoModel)
    }
}

#[async_trait::async_trait]
impl JudgmentModel for RoleJudgmentModel {
    async fn ask(&self, prompt: &str) -> Result<String, JudgmentError> {
        let model = self.model().await?;
        let resolved = self
            .resolver
            .resolve(&model)
            .map_err(|error| JudgmentError::Transport(error.to_string()))?;
        let mut wire = WireRequest::new(resolved.wire_model.clone());
        wire.max_tokens = Some(MAX_TOKENS);
        wire.temperature = Some(0.0);
        wire.system = Some(JUDGMENT_SYSTEM.into());
        wire.messages.push(ChatMessage {
            role: Role::User,
            content: prompt.into(),
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
            .map_err(|error| JudgmentError::Transport(error.to_string()))?;
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event {
                // Thinking is not the answer; a judge that reasons out loud
                // still only gets parsed on what it finally says.
                StreamEvent::TextDelta { text, .. } => answer.push_str(&text),
                StreamEvent::Error { message, .. } => {
                    return Err(JudgmentError::Stream(message.to_string()));
                }
                StreamEvent::Done { .. } => break,
                _ => {}
            }
        }
        if answer.trim().is_empty() {
            return Err(JudgmentError::Empty);
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A judge that answers whatever the test says, after however long the
    /// test says, and counts how often it was asked.
    struct FakeModel {
        answer: Option<&'static str>,
        delay: Duration,
        calls: AtomicUsize,
    }

    impl FakeModel {
        fn answering(answer: &'static str) -> Arc<Self> {
            Arc::new(Self {
                answer: Some(answer),
                delay: Duration::ZERO,
                calls: AtomicUsize::new(0),
            })
        }

        fn slow() -> Arc<Self> {
            Arc::new(Self {
                answer: Some("YES"),
                delay: Duration::from_secs(10),
                calls: AtomicUsize::new(0),
            })
        }

        fn broken() -> Arc<Self> {
            Arc::new(Self {
                answer: None,
                delay: Duration::ZERO,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl JudgmentModel for FakeModel {
        async fn ask(&self, _prompt: &str) -> Result<String, JudgmentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.answer
                .map(str::to_owned)
                .ok_or(JudgmentError::Transport("upstream refused".into()))
        }
    }

    fn provider(model: Arc<FakeModel>, capabilities: Capabilities) -> JudgmentProvider {
        JudgmentProvider::new(model, capabilities)
    }

    #[tokio::test]
    async fn a_clear_answer_is_the_decision() {
        let yes = FakeModel::answering("YES");
        let decision = provider(Arc::clone(&yes), Capabilities::all())
            .confirm(
                Capability::SensitiveClick,
                &Confirm::new("Does this command delete user data?")
                    .with_context("rm -rf ~/.titi/agent"),
            )
            .await;
        assert_eq!(decision, Some(true));
        assert_eq!(yes.calls(), 1);

        let no = FakeModel::answering("Answer: no.");
        let decision = provider(no, Capabilities::all())
            .confirm(
                Capability::ProviderError,
                &Confirm::new("Is this error worth retrying?"),
            )
            .await;
        assert_eq!(decision, Some(false));
    }

    #[tokio::test]
    async fn a_garbage_answer_is_no_judgment() {
        let model = FakeModel::answering("hmm, it depends on what you mean");
        let decision = provider(Arc::clone(&model), Capabilities::all())
            .confirm(
                Capability::SensitiveClick,
                &Confirm::new("Does this command delete user data?"),
            )
            .await;
        assert_eq!(decision, None, "an unparseable answer must not decide");
        assert_eq!(model.calls(), 1);

        let both = FakeModel::answering("yes and no");
        let decision = provider(both, Capabilities::all())
            .confirm(
                Capability::SensitiveClick,
                &Confirm::new("Does this command delete user data?"),
            )
            .await;
        assert_eq!(decision, None, "an ambiguous answer must not decide");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_model_times_out_to_no_judgment() {
        let model = FakeModel::slow();
        let started = tokio::time::Instant::now();
        let decision = provider(model, Capabilities::all())
            .confirm(
                Capability::FileSearch,
                &Confirm::new("Is this the file the user meant?"),
            )
            .await;
        let elapsed = started.elapsed();

        assert_eq!(decision, None);
        assert!(
            elapsed < Duration::from_secs(1),
            "a judgment must not outlive its timeout, waited {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_broken_model_is_no_judgment() {
        let model = FakeModel::broken();
        let decision = provider(model, Capabilities::all())
            .confirm(
                Capability::ProviderError,
                &Confirm::new("Is this error worth retrying?"),
            )
            .await;
        assert_eq!(decision, None);
    }

    #[tokio::test]
    async fn an_opted_out_capability_never_calls_the_model() {
        let model = FakeModel::answering("YES");
        let judge = provider(
            Arc::clone(&model),
            Capabilities::NONE.with(Capability::SkillHint),
        );

        let decision = judge
            .confirm(
                Capability::SensitiveClick,
                &Confirm::new("Does this command delete user data?"),
            )
            .await;
        assert_eq!(decision, None);
        assert_eq!(model.calls(), 0, "an opted-out capability must not ask");

        let decision = judge
            .confirm(
                Capability::SkillHint,
                &Confirm::new("Does this request match the commit skill?"),
            )
            .await;
        assert_eq!(decision, Some(true));
        assert_eq!(model.calls(), 1);
    }

    #[tokio::test]
    async fn a_missing_provider_is_no_judgment() {
        let decision = confirm(
            None,
            Capability::SkillHint,
            &Confirm::new("Does this request match the commit skill?"),
        )
        .await;
        assert_eq!(decision, None);
    }

    #[tokio::test]
    async fn a_choice_is_parsed_within_its_options() {
        let options = ["src/main.rs".to_owned(), "src/lib.rs".to_owned()];
        let picked = provider(FakeModel::answering("2"), Capabilities::all())
            .choose(
                Capability::FileSearch,
                &Choice::new("Which file did the user mean?", &options),
            )
            .await;
        assert_eq!(picked, Some(1));

        let out_of_range = provider(FakeModel::answering("7"), Capabilities::all())
            .choose(
                Capability::FileSearch,
                &Choice::new("Which file did the user mean?", &options),
            )
            .await;
        assert_eq!(out_of_range, None);

        let garbage = provider(
            FakeModel::answering("the second one, probably"),
            Capabilities::all(),
        )
        .choose(
            Capability::FileSearch,
            &Choice::new("Which file did the user mean?", &options),
        )
        .await;
        assert_eq!(garbage, None);
    }

    #[tokio::test]
    async fn an_empty_option_list_asks_nothing() {
        let model = FakeModel::answering("1");
        let empty: [&str; 0] = [];
        let picked = provider(Arc::clone(&model), Capabilities::all())
            .choose(
                Capability::FileSearch,
                &Choice::new("Which file did the user mean?", &empty),
            )
            .await;
        assert_eq!(picked, None);
        assert_eq!(model.calls(), 0);
    }

    #[test]
    fn capability_names_round_trip_and_reject_strangers() {
        let set = Capabilities::from_names(["skillHint", "sensitiveclick", "nonsense"]);
        assert!(set.allows(Capability::SkillHint));
        assert!(set.allows(Capability::SensitiveClick));
        assert!(!set.allows(Capability::FileSearch));
        assert!(Capabilities::NONE.is_empty());
    }

    /// `EngineConfig` derives `Debug` and `Clone`, so the provider it will
    /// hold has to keep its hand-written `Debug`: a trait object is not
    /// `Debug` on its own, and losing this breaks the config, not this file.
    #[test]
    fn a_provider_fits_the_engine_config() {
        fn config_shaped<T: std::fmt::Debug + Clone>() {}
        config_shaped::<Option<Arc<JudgmentProvider>>>();
    }
}
