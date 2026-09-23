//! Council: two to four briefs answer one question on their own, and one
//! fold turns their answers into a single text that still shows where they
//! disagree.
//!
//! The fold is the part worth testing: a synthesis that quietly drops the
//! dissent reads like a panel that agreed, which is the one thing it must
//! never be mistaken for.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use async_trait::async_trait;
use smol_str::SmolStr;
use titi_engine::{
    AgentContext, AgentRequest, AgentRunner, Council, CouncilError, CouncilMember, EngineCommand,
    EngineConfig, EngineEvent, EngineRuntime, council_report, run_council,
};
use titi_providers::{BlockId, Effort, MockBody, MockTransport, StopReason, StreamEvent};

/// A member that always answers the same way and keeps what it was asked.
struct ScriptedRunner {
    reply: Result<&'static str, &'static str>,
    prompts: Sender<SmolStr>,
}

#[async_trait]
impl AgentRunner for ScriptedRunner {
    async fn run(&self, request: AgentRequest, _context: AgentContext) -> Result<SmolStr, SmolStr> {
        let _ = self.prompts.send(request.task.clone());
        self.reply.map(SmolStr::from).map_err(SmolStr::from)
    }
}

fn runner(reply: Result<&'static str, &'static str>) -> (Arc<ScriptedRunner>, Receiver<SmolStr>) {
    let (tx, rx) = mpsc::channel();
    (Arc::new(ScriptedRunner { reply, prompts: tx }), rx)
}

fn member(
    name: &str,
    brief: &str,
    model: &str,
    effort: Effort,
    reply: Result<&'static str, &'static str>,
) -> (CouncilMember, Receiver<SmolStr>) {
    let (runner, prompts) = runner(reply);
    (
        CouncilMember::new(name, brief, model, effort, runner),
        prompts,
    )
}

fn drain(rx: &Receiver<SmolStr>) -> Vec<SmolStr> {
    rx.try_iter().collect()
}

const SHARED: &str = "the parser has to be rewritten";

fn panel() -> (Vec<CouncilMember>, Vec<Receiver<SmolStr>>) {
    let (advocate, advocate_prompts) = member(
        "advocate",
        "Argue for the change.",
        "big-model",
        Effort::High,
        Ok("the parser has to be rewritten, and the rewrite starts this week"),
    );
    let (skeptic, skeptic_prompts) = member(
        "skeptic",
        "Argue against the change.",
        "small-model",
        Effort::Low,
        Ok("the parser has to be rewritten, but not before the release ships"),
    );
    let (pragmatist, pragmatist_prompts) = member(
        "pragmatist",
        "Say what you would actually do.",
        "mid-model",
        Effort::Medium,
        Ok("the parser has to be rewritten behind a flag, one grammar at a time"),
    );
    (
        vec![advocate, skeptic, pragmatist],
        vec![advocate_prompts, skeptic_prompts, pragmatist_prompts],
    )
}

const FOLD: &str = "Agreement: all three want the parser rewritten.\n\
Dissent: advocate says start this week, skeptic says only after the release, \
pragmatist says behind a flag.";

#[tokio::test]
async fn every_answer_reaches_the_fold_and_the_dissent_survives_it() {
    let (members, member_prompts) = panel();
    let (synthesizer, fold_prompts) = runner(Ok(FOLD));
    let council = Council::new(members, synthesizer).expect("three members seat a council");

    let report = council
        .run("do we rewrite the parser?")
        .await
        .expect("three live members produce a report");

    // The fold saw the members' words, not a count of them.
    let folds = drain(&fold_prompts);
    assert_eq!(folds.len(), 1);
    let fold = folds[0].as_str();
    for answer in &report.answers {
        assert!(fold.contains(answer.answer.as_str()), "{fold}");
        assert!(fold.contains(answer.name.as_str()), "{fold}");
    }
    assert!(fold.contains("this week"), "{fold}");
    assert!(fold.contains("not before the release ships"), "{fold}");
    assert!(fold.contains("behind a flag"), "{fold}");
    assert!(fold.contains("do we rewrite the parser?"), "{fold}");

    // Both the shared view and every dissenting position survive the fold.
    assert_eq!(report.synthesis.as_str(), FOLD);
    assert!(report.synthesis.contains("Agreement:"));
    assert!(report.synthesis.contains("Dissent:"));
    assert!(report.answers.len() == 3 && report.dropped.is_empty());
    assert!(report.answers.iter().all(|a| a.answer.contains(SHARED)));

    // The line a surface shows carries the dissent too.
    let line = council_report(&report);
    assert!(line.contains("Dissent:"), "{line}");
    assert!(line.contains("3 of 3 members"), "{line}");

    // Each member answered alone: its own brief, never another's answer.
    let seen: Vec<SmolStr> = member_prompts
        .iter()
        .map(|rx| drain(rx).pop().expect("each member was asked once"))
        .collect();
    assert!(seen[0].contains("Argue for the change."));
    assert!(seen[1].contains("Argue against the change."));
    assert!(seen[2].contains("Say what you would actually do."));
    for prompt in &seen {
        assert!(prompt.contains("do we rewrite the parser?"), "{prompt}");
        assert!(!prompt.contains("behind a flag, one grammar"), "{prompt}");
        assert!(
            !prompt.contains("Argue against the change.\nArgue"),
            "{prompt}"
        );
    }
    // The member's own effort is in its prompt, not another member's.
    assert!(seen[0].contains("high"), "{}", seen[0]);
    assert!(seen[1].contains("low"), "{}", seen[1]);
    assert!(seen[2].contains("medium"), "{}", seen[2]);
}

#[tokio::test]
async fn a_failed_member_is_dropped_and_the_council_still_finishes() {
    let (advocate, _advocate_prompts) = member(
        "advocate",
        "Argue for the change.",
        "big-model",
        Effort::High,
        Ok("rewrite it now"),
    );
    let (skeptic, _skeptic_prompts) = member(
        "skeptic",
        "Argue against the change.",
        "small-model",
        Effort::Low,
        Ok("rewrite it after the release"),
    );
    let (absent, absent_prompts) = member(
        "historian",
        "Say what happened last time.",
        "keyless-model",
        Effort::Medium,
        Err("no api key for keyless-model"),
    );
    let (mute, _mute_prompts) = member(
        "pragmatist",
        "Say what you would actually do.",
        "mid-model",
        Effort::Medium,
        Ok("   \n  "),
    );
    let (synthesizer, fold_prompts) = runner(Ok(FOLD));

    let report = run_council(
        vec![advocate, skeptic, absent, mute],
        synthesizer,
        "do we rewrite the parser?",
    )
    .await
    .expect("two live members are enough");

    assert_eq!(report.answers.len(), 2);
    assert_eq!(report.dropped.len(), 2);
    let historian = &report.dropped[0];
    assert_eq!(historian.name.as_str(), "historian");
    assert_eq!(historian.model.as_str(), "keyless-model");
    assert!(
        historian.reason.contains("no api key"),
        "{}",
        historian.reason
    );
    let mute = &report.dropped[1];
    assert_eq!(mute.name.as_str(), "pragmatist");
    assert!(mute.reason.contains("nothing"), "{}", mute.reason);

    // The dropped member was asked, and its silence did not reach the fold.
    assert_eq!(drain(&absent_prompts).len(), 1);
    let folds = drain(&fold_prompts);
    let fold = folds[0].as_str();
    assert!(fold.contains("rewrite it now"), "{fold}");
    assert!(fold.contains("rewrite it after the release"), "{fold}");
    assert!(fold.contains("historian"), "{fold}");
    assert!(fold.contains("no api key"), "{fold}");

    let line = council_report(&report);
    assert!(line.contains("2 of 4 members"), "{line}");
    assert!(line.contains("dropped historian"), "{line}");
    assert!(line.contains("dropped pragmatist"), "{line}");
}

#[tokio::test]
async fn fewer_than_two_live_members_is_not_enough_and_nothing_is_folded() {
    let (advocate, _advocate_prompts) = member(
        "advocate",
        "Argue for the change.",
        "big-model",
        Effort::High,
        Ok("rewrite it now"),
    );
    let (absent, _absent_prompts) = member(
        "skeptic",
        "Argue against the change.",
        "keyless-model",
        Effort::Low,
        Err("no api key for keyless-model"),
    );
    let (mute, _mute_prompts) = member(
        "pragmatist",
        "Say what you would actually do.",
        "mid-model",
        Effort::Medium,
        Ok(""),
    );
    let (synthesizer, fold_prompts) = runner(Ok(FOLD));

    let error = run_council(
        vec![advocate, absent, mute],
        synthesizer,
        "do we rewrite the parser?",
    )
    .await
    .expect_err("one live member is not a council");

    match &error {
        CouncilError::NotEnough {
            live,
            seated,
            dropped,
        } => {
            assert_eq!(*live, 1);
            assert_eq!(*seated, 3);
            assert_eq!(dropped.len(), 2);
            assert!(dropped.iter().any(|gone| gone.name == "skeptic"));
            assert!(dropped.iter().any(|gone| gone.name == "pragmatist"));
        }
        other => panic!("expected not enough participants, got {other:?}"),
    }
    assert!(error.to_string().contains("not enough"), "{error}");
    // A synthesis of one opinion would read as a council's verdict.
    assert!(drain(&fold_prompts).is_empty());
}

#[tokio::test]
async fn a_council_seats_two_to_four_members_and_needs_a_question() {
    let (only, _prompts) = member(
        "advocate",
        "Argue for the change.",
        "big-model",
        Effort::High,
        Ok("rewrite it"),
    );
    let (synthesizer, _folds) = runner(Ok(FOLD));
    match Council::new(vec![only], Arc::clone(&synthesizer) as Arc<dyn AgentRunner>) {
        Err(CouncilError::Size { seated }) => assert_eq!(seated, 1),
        other => panic!("expected a size error, got {}", describe(&other)),
    }

    let mut crowd = Vec::new();
    for index in 0..5 {
        let (seat, _prompts) = member(
            "advocate",
            "Argue for the change.",
            "big-model",
            Effort::High,
            Ok("rewrite it"),
        );
        let _ = index;
        crowd.push(seat);
    }
    match Council::new(crowd, Arc::clone(&synthesizer) as Arc<dyn AgentRunner>) {
        Err(CouncilError::Size { seated }) => assert_eq!(seated, 5),
        other => panic!("expected a size error, got {}", describe(&other)),
    }

    let (members, _member_prompts) = panel();
    let error = run_council(members, synthesizer, "   \n ")
        .await
        .expect_err("a council needs a question");
    assert!(matches!(error, CouncilError::EmptyQuestion), "{error}");
}

fn describe(outcome: &Result<Council, CouncilError>) -> String {
    match outcome {
        Ok(_) => "a council".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// The product command runs the same council through the session runner and
/// does not open a chat turn.
#[tokio::test]
async fn the_run_council_command_reports_a_fold() {
    let answer = |text: &'static str| {
        MockBody::Events(vec![
            StreamEvent::TextDelta {
                id: BlockId::new("m"),
                text: text.into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
            },
        ])
    };
    let transport = Arc::new(MockTransport::new(vec![
        answer("the parser is the risk\n"),
        answer("the parser is the risk\n"),
        answer("the parser is the risk\n"),
        answer(FOLD),
    ]));
    let captured = Arc::clone(&transport);
    let resolver = Arc::new(move |model: &str| {
        Ok(titi_engine::ResolvedModel::without_credential(
            model,
            Arc::clone(&captured) as Arc<dyn titi_providers::Transport>,
        ))
    });
    let mut engine = EngineRuntime::start(EngineConfig::new("primary"), resolver);
    engine
        .send(EngineCommand::RunCouncil {
            question: "do we rewrite the parser?".into(),
        })
        .await
        .unwrap();

    let event = engine.recv().await.expect("a council report");
    match event {
        EngineEvent::CouncilFinished { report } => {
            assert!(report.contains("3 of 3 members"), "{report}");
            assert!(report.contains("Dissent:"), "{report}");
        }
        other => panic!("expected a council report, got {other:?}"),
    }
    assert_eq!(transport.call_count(), 4);
    let requests = transport.requests();
    let fold = requests
        .last()
        .and_then(|request| request.messages.last().cloned())
        .expect("the fold was sent");
    assert!(
        fold.content.contains("the parser is the risk"),
        "{}",
        fold.content
    );
}

#[tokio::test]
async fn an_empty_council_question_answers_with_usage() {
    let transport = Arc::new(MockTransport::new(Vec::new()));
    let resolver = Arc::new(move |model: &str| {
        Ok(titi_engine::ResolvedModel::without_credential(
            model,
            Arc::clone(&transport) as Arc<dyn titi_providers::Transport>,
        ))
    });
    let mut engine = EngineRuntime::start(EngineConfig::new("primary"), resolver);
    engine
        .send(EngineCommand::RunCouncil {
            question: "   ".into(),
        })
        .await
        .unwrap();
    match engine.recv().await.expect("a council report") {
        EngineEvent::CouncilFinished { report } => {
            assert!(report.contains("usage: /council"), "{report}")
        }
        other => panic!("expected a council report, got {other:?}"),
    }
}
