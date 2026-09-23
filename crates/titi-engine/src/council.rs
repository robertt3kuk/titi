//! A council: two to four briefs answer one question, then one fold.
//!
//! Each member is a brief of its own on a model of its own at an effort of
//! its own, and none of them sees the others while it answers — a member
//! shown the others' words converges on them, and a panel of one opinion has
//! nothing left to fold.
//!
//! The fold is not a summary. It separates what the members agree on from
//! what they do not, and the dissent is named with the member that holds it.
//! A synthesis that quietly drops a disagreement reads like a panel that
//! agreed, so every live answer reaches the synthesizer verbatim and stays in
//! [`CouncilReport::answers`] afterwards.
//!
//! A member that has no key, errors, or answers with nothing is dropped and
//! recorded; the rest carry on. Below [`MIN_MEMBERS`] live members there is
//! no council left to fold, and [`CouncilError::NotEnough`] says so instead
//! of dressing one opinion up as a verdict.

use std::sync::Arc;

use futures::future::join_all;
use smol_str::SmolStr;
use titi_providers::Effort;

use crate::agents::{AgentContext, AgentRequest, AgentRunner};
use crate::protocol::AgentKind;

/// Fewest members a council can seat. One answer is an opinion, not a
/// council, and there is nothing for a fold to compare it against.
pub const MIN_MEMBERS: usize = 2;

/// Most members a council can seat. Every seat is a paid model turn and the
/// fold has to hold all of them at once.
pub const MAX_MEMBERS: usize = 4;

/// The synthesizer's standing instructions. Kept verbatim here so every fold
/// is asked for the same two sections.
pub const SYNTHESIZER_BRIEF: &str = "\
You are folding a council's answers into one text. You did not sit on the \
council and you have no opinion of your own: every claim below belongs to a \
member, and you name the member it belongs to.\n\
Write exactly two sections, in this order:\n\
Agreement: what the members actually converge on, and only that.\n\
Dissent: every point they do not agree on, each with the member that holds \
it. Write \"Dissent: none\" only when the answers genuinely do not conflict.\n\
Never drop a disagreement to make the answer read cleanly, and never invent \
one no member raised.";

/// The panel `/council` seats: three briefs that pull against each other by
/// construction, so the fold has something to disagree about that is not an
/// accident of sampling.
pub const DEFAULT_BRIEFS: [(&str, &str, Effort); 3] = [
    (
        "advocate",
        "You argue for the change: say what it buys and why it is worth doing now.",
        Effort::High,
    ),
    (
        "skeptic",
        "You argue against the change: say what it costs, what it breaks, and what \
         the cheaper answer is.",
        Effort::High,
    ),
    (
        "pragmatist",
        "You say what you would actually do this week, in order, with what is \
         already in the repository.",
        Effort::Medium,
    ),
];

/// Why a council produced no report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CouncilError {
    #[error("a council seats {MIN_MEMBERS} to {MAX_MEMBERS} members, not {seated}")]
    Size { seated: usize },
    #[error("a council needs a question")]
    EmptyQuestion,
    /// Too many members fell out to fold what is left.
    #[error(
        "not enough participants: {live} of {seated} members answered, {MIN_MEMBERS} are needed"
    )]
    NotEnough {
        live: usize,
        seated: usize,
        dropped: Vec<DroppedMember>,
    },
    #[error("the synthesizer {0}")]
    Synthesis(SmolStr),
}

/// One seat: its own brief, its own model, its own effort, and the runner it
/// speaks through.
#[derive(Clone)]
pub struct CouncilMember {
    pub name: SmolStr,
    pub brief: SmolStr,
    pub model: SmolStr,
    pub effort: Effort,
    runner: Arc<dyn AgentRunner>,
}

impl CouncilMember {
    pub fn new(
        name: impl Into<SmolStr>,
        brief: impl Into<SmolStr>,
        model: impl Into<SmolStr>,
        effort: Effort,
        runner: Arc<dyn AgentRunner>,
    ) -> Self {
        Self {
            name: name.into(),
            brief: brief.into(),
            model: model.into(),
            effort,
            runner,
        }
    }

    /// What this member is asked: its own brief and the shared question, and
    /// nothing any other member said.
    pub fn prompt(&self, question: &str) -> SmolStr {
        format!(
            "{}\n\nYou are the {} on a council. Every member answers the same \
             question on its own; you cannot see the others and they cannot see \
             you, so answer from your brief alone.\nReasoning effort: {}.\n\n\
             Question:\n{}\n\nSay what you hold and why. End with the one thing \
             you are least sure about.",
            self.brief,
            self.name,
            effort_word(self.effort),
            question
        )
        .into()
    }

    /// One member's turn. An empty answer is a failure, not an opinion.
    async fn answer(&self, question: &str) -> Result<SmolStr, SmolStr> {
        let reply = self
            .runner
            .run(
                AgentRequest {
                    id: format!("council-{}", self.name).into(),
                    name: self.name.clone(),
                    task: self.prompt(question),
                    kind: AgentKind::Subagent,
                    parent_id: None,
                },
                AgentContext::detached(),
            )
            .await?;
        let trimmed = reply.trim();
        if trimmed.is_empty() {
            return Err("answered with nothing".into());
        }
        Ok(trimmed.into())
    }
}

/// What one member held, and on what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberAnswer {
    pub name: SmolStr,
    pub model: SmolStr,
    pub effort: Effort,
    /// The member's reply, verbatim.
    pub answer: SmolStr,
}

/// A member that left no opinion, and why. Recorded rather than ignored: a
/// council of two that was seated as four is a weaker answer, and the reader
/// has to be able to see that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedMember {
    pub name: SmolStr,
    pub model: SmolStr,
    pub reason: SmolStr,
}

/// What a council produced: who spoke, who fell out, and the fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CouncilReport {
    pub question: SmolStr,
    /// Live members, in seating order. The fold is one model's reading of
    /// these; they stay here verbatim so nothing it left out is lost.
    pub answers: Vec<MemberAnswer>,
    pub dropped: Vec<DroppedMember>,
    /// The synthesizer's reply, verbatim.
    pub synthesis: SmolStr,
}

/// Runs the members, then the fold.
pub struct Council {
    members: Vec<CouncilMember>,
    synthesizer: Arc<dyn AgentRunner>,
}

impl Council {
    pub fn new(
        members: Vec<CouncilMember>,
        synthesizer: Arc<dyn AgentRunner>,
    ) -> Result<Self, CouncilError> {
        let seated = members.len();
        if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&seated) {
            return Err(CouncilError::Size { seated });
        }
        Ok(Self {
            members,
            synthesizer,
        })
    }

    /// Every member answers at once — they do not read each other, so there
    /// is nothing to order them by — and a member that fails takes only its
    /// own seat down.
    pub async fn run(&self, question: impl Into<SmolStr>) -> Result<CouncilReport, CouncilError> {
        let question = question.into();
        let question = question.trim();
        if question.is_empty() {
            return Err(CouncilError::EmptyQuestion);
        }
        let question = SmolStr::from(question);
        let replies = join_all(self.members.iter().map(|member| member.answer(&question))).await;

        let mut answers = Vec::with_capacity(replies.len());
        let mut dropped = Vec::new();
        for (member, reply) in self.members.iter().zip(replies) {
            match reply {
                Ok(answer) => answers.push(MemberAnswer {
                    name: member.name.clone(),
                    model: member.model.clone(),
                    effort: member.effort,
                    answer,
                }),
                Err(reason) => dropped.push(DroppedMember {
                    name: member.name.clone(),
                    model: member.model.clone(),
                    reason,
                }),
            }
        }
        if answers.len() < MIN_MEMBERS {
            return Err(CouncilError::NotEnough {
                live: answers.len(),
                seated: self.members.len(),
                dropped,
            });
        }

        let synthesis = self.fold(&question, &answers, &dropped).await?;
        Ok(CouncilReport {
            question,
            answers,
            dropped,
            synthesis,
        })
    }

    async fn fold(
        &self,
        question: &str,
        answers: &[MemberAnswer],
        dropped: &[DroppedMember],
    ) -> Result<SmolStr, CouncilError> {
        let reply = self
            .synthesizer
            .run(
                AgentRequest {
                    id: "council-synthesis".into(),
                    name: "synthesizer".into(),
                    task: fold_prompt(question, answers, dropped).into(),
                    kind: AgentKind::Subagent,
                    parent_id: None,
                },
                AgentContext::detached(),
            )
            .await
            .map_err(|reason| CouncilError::Synthesis(format!("failed: {reason}").into()))?;
        let trimmed = reply.trim();
        if trimmed.is_empty() {
            return Err(CouncilError::Synthesis("answered with nothing".into()));
        }
        Ok(trimmed.into())
    }
}

/// The entry a surface uses. Mirrors [`crate::run_goal`]: seat the council,
/// run it, and let the caller render either outcome.
pub async fn run_council(
    members: Vec<CouncilMember>,
    synthesizer: Arc<dyn AgentRunner>,
    question: impl Into<SmolStr>,
) -> Result<CouncilReport, CouncilError> {
    Council::new(members, synthesizer)?.run(question).await
}

/// What the synthesizer is shown: the question, every live answer verbatim
/// with the member that gave it, and who fell out. The dropped members are
/// named so the fold cannot present a short panel as a full one.
fn fold_prompt(question: &str, answers: &[MemberAnswer], dropped: &[DroppedMember]) -> String {
    let mut prompt = format!("{SYNTHESIZER_BRIEF}\n\nQuestion:\n{question}\n");
    for answer in answers {
        prompt.push_str(&format!(
            "\n--- {} · model {} · effort {} ---\n{}\n",
            answer.name,
            answer.model,
            effort_word(answer.effort),
            answer.answer
        ));
    }
    if !dropped.is_empty() {
        prompt.push_str("\nMembers that left no answer, so the panel is short:\n");
        for gone in dropped {
            prompt.push_str(&format!(
                "- {} ({}): {}\n",
                gone.name, gone.model, gone.reason
            ));
        }
    }
    prompt.push_str("\nWrite the two sections now.");
    prompt
}

/// The transcript block a surface shows: who sat, who fell out, the fold, and
/// the seating so every position keeps its owner.
pub fn council_report(report: &CouncilReport) -> String {
    let seated = report.answers.len() + report.dropped.len();
    let mut out = format!("council · {} of {seated} members", report.answers.len());
    for gone in &report.dropped {
        out.push_str(&format!(" · dropped {} ({})", gone.name, gone.reason));
    }
    out.push('\n');
    out.push_str(&report.synthesis);
    for answer in &report.answers {
        out.push_str(&format!(
            "\n· {} · {} · effort {}",
            answer.name,
            answer.model,
            effort_word(answer.effort)
        ));
    }
    out
}

/// The effort as a member is told it, since a runner takes a prompt and not a
/// budget. Same ladder as [`titi_providers::EFFORT_LADDER`].
fn effort_word(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "extra high",
        Effort::Max => "maximum",
    }
}
