//! `/review` and `titi --review`: an independent verdict on what this
//! session changed.
//!
//! The engine does the reviewing (`EngineCommand::RunReview`); this is the
//! surface half — the banner, `/review apply`, the session's last verdicts,
//! and the exit codes a CI job reads.

use std::collections::VecDeque;

use titi_engine::{EngineCommand, ReviewReport, Verdict};

use crate::chat::{Applied, Chat, LogWrite};
use crate::transcript::{LineKind, TranscriptLine};
use titi_core::session::Role;

/// Verdicts `/review list` remembers.
pub(crate) const KEEP: usize = 8;

/// Raw reply lines shown when a review has no findings to list instead.
const RAW_LINES: usize = 12;

/// Exit code of `titi --review`. `PASS` is 0 and `FAIL` is 3 like `--goal`;
/// `PARTIAL` is 4 so it never reads as a plain failure of the tool itself;
/// 5 is "nothing to review", which is not a pass either. 2 is a usage error
/// and is decided before the engine starts.
pub fn exit_code(report: &ReviewReport) -> i32 {
    match report.verdict {
        Some(Verdict::Pass) => 0,
        Some(Verdict::Fail) => 3,
        Some(Verdict::Partial) => 4,
        None => 5,
    }
}

/// The exit code of a review that never produced a report: the engine
/// stopped first. The change is unjudged, so it is `PARTIAL`'s code.
pub const UNFINISHED: i32 = 4;

/// The one stdout line of a headless review.
pub fn json_line(report: &ReviewReport) -> String {
    serde_json::to_string(report).unwrap_or_default()
}

/// The prompt `/review apply` hands the coder, or `None` when the review
/// found nothing to fix.
pub(crate) fn apply_prompt(report: &ReviewReport) -> Option<String> {
    if report.findings.is_empty() {
        return None;
    }
    let verdict = report.verdict.map_or("", Verdict::as_str);
    let mut text = format!(
        "An independent reviewer read your changes and returned {verdict}{}. \
         Fix these findings, then say what you changed:\n",
        if report.reason.is_empty() {
            String::new()
        } else {
            format!(" ({})", report.reason)
        }
    );
    for finding in &report.findings {
        text.push_str("- ");
        text.push_str(&finding.render());
        text.push('\n');
    }
    Some(text)
}

/// The session's last [`KEEP`] verdicts, and whether one is running.
#[derive(Debug, Default)]
pub(crate) struct ReviewLog {
    entries: VecDeque<ReviewReport>,
    pub(crate) running: bool,
}

impl ReviewLog {
    /// Keeps a report that holds a verdict; "nothing to review" is not one.
    pub(crate) fn record(&mut self, report: ReviewReport) {
        if report.verdict.is_none() {
            return;
        }
        self.entries.push_back(report);
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
    }

    pub(crate) fn latest(&self) -> Option<&ReviewReport> {
        self.entries.back()
    }

    /// One line per kept verdict, newest first.
    pub(crate) fn list(&self) -> Vec<String> {
        self.entries
            .iter()
            .rev()
            .enumerate()
            .map(|(at, report)| {
                let reason = if report.reason.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", report.reason)
                };
                format!(
                    "{}. {}{reason}",
                    at + 1,
                    report.banner().trim_start_matches("review · ")
                )
            })
            .collect()
    }
}

impl Chat {
    /// `/review`, `/review apply`, `/review list`.
    pub(crate) fn review(&mut self, args: &str) -> Applied {
        match args.trim() {
            "" => self.review_run(),
            "apply" => self.review_apply(),
            "list" => self.review_list(),
            _ => {
                self.push(LineKind::Error, "usage: /review [apply|list]".to_owned());
                Applied::none()
            }
        }
    }

    fn review_run(&mut self) -> Applied {
        if self.reviews.running {
            self.push(LineKind::Error, "review: one is already running".to_owned());
            return Applied::none();
        }
        self.reviews.running = true;
        self.push(LineKind::Note, "review: checking what changed…".to_owned());
        let edited = self
            .agent_edits
            .paths()
            .into_iter()
            .map(Into::into)
            .collect();
        Applied::effect(crate::chat::ChatEffect::Send(EngineCommand::RunReview {
            edited,
        }))
    }

    fn review_list(&mut self) -> Applied {
        let rows = self.reviews.list();
        if rows.is_empty() {
            self.push(
                LineKind::Note,
                "review: no verdicts this session".to_owned(),
            );
        }
        for row in rows {
            self.push(LineKind::Note, row);
        }
        Applied::none()
    }

    fn review_apply(&mut self) -> Applied {
        let Some(report) = self.reviews.latest() else {
            self.push(
                LineKind::Error,
                "review: nothing to apply · /review first".to_owned(),
            );
            return Applied::none();
        };
        let Some(text) = apply_prompt(report) else {
            self.push(
                LineKind::Error,
                "review: the last review has no findings to apply".to_owned(),
            );
            return Applied::none();
        };
        let count = report.findings.len();
        self.push(
            LineKind::Note,
            format!(
                "review: sending {count} {} to the coder",
                if count == 1 { "finding" } else { "findings" }
            ),
        );
        let log = Some(LogWrite::text(Role::User, text.clone()));
        if self.turn_active {
            return Applied::send(EngineCommand::Steer { text: text.into() }, log);
        }
        self.turn_active = true;
        self.turn_started = Some(std::time::Instant::now());
        self.begin_usage_ledger();
        self.begin_rate();
        Applied::send(EngineCommand::SubmitPrompt { text: text.into() }, log)
    }

    /// The reviewer answered: the banner, the findings, and what to do next.
    pub(crate) fn review_finished(&mut self, report: ReviewReport) {
        self.reviews.running = false;
        let Some(verdict) = report.verdict else {
            let reason = if report.reason.is_empty() {
                "nothing to review"
            } else {
                report.reason.as_str()
            };
            self.push(LineKind::Note, reason.to_owned());
            return;
        };
        let kind = if verdict == Verdict::Pass {
            LineKind::Note
        } else {
            LineKind::Error
        };
        // Red, because a FAIL needs reading; not logged, because it is the
        // reviewer's answer rather than something that went wrong.
        let banner = TranscriptLine::new(kind, report.banner()).stamped(&self.model);
        self.lines.push(banner);
        self.scroll_offset = 0;
        if !report.reason.is_empty() {
            self.push(LineKind::Note, report.reason.to_string());
        }
        for finding in &report.findings {
            self.push(LineKind::Note, format!("  {}", finding.render()));
        }
        if report.findings.is_empty() && verdict != Verdict::Pass {
            for line in report.raw.lines().take(RAW_LINES) {
                self.push(LineKind::Note, format!("  {line}"));
            }
        }
        let hint = if report.findings.is_empty() {
            "/review list shows this session's verdicts"
        } else {
            "/review apply sends the findings to the coder · /review list"
        };
        self.push(LineKind::Note, hint.to_owned());
        self.reviews.record(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(text: &str) -> ReviewReport {
        ReviewReport::parse(text, Vec::new())
    }

    #[test]
    fn exit_codes_are_pass_zero_fail_three_partial_four_nothing_five() {
        assert_eq!(exit_code(&report("VERDICT: PASS — ok")), 0);
        assert_eq!(exit_code(&report("VERDICT: FAIL — bad")), 3);
        assert_eq!(exit_code(&report("VERDICT: PARTIAL — unsure")), 4);
        assert_eq!(exit_code(&report("no idea")), 4, "malformed is PARTIAL");
        assert_eq!(exit_code(&ReviewReport::nothing("nothing to review")), 5);
    }

    #[test]
    fn the_json_line_names_the_verdict_and_is_one_line() {
        let line = json_line(&report("VERDICT: FAIL — x\n- a.rs:2 — y"));
        assert!(!line.contains('\n'));
        assert!(line.contains(r#""verdict":"FAIL""#), "{line}");
        assert!(line.contains(r#""line":2"#), "{line}");
        let none = json_line(&ReviewReport::nothing("nothing to review"));
        assert!(none.contains(r#""verdict":null"#), "{none}");
    }

    #[test]
    fn apply_composes_the_findings_into_a_prompt() {
        let prompt = apply_prompt(&report(
            "VERDICT: FAIL — missing test\n- src/a.rs:12 — empty input panics\n- general note",
        ))
        .unwrap();
        assert_eq!(
            prompt,
            "An independent reviewer read your changes and returned FAIL (missing test). \
             Fix these findings, then say what you changed:\n\
             - src/a.rs:12 \u{2014} empty input panics\n\
             - general note\n"
        );
    }

    #[test]
    fn apply_has_nothing_to_send_without_findings() {
        assert_eq!(apply_prompt(&report("VERDICT: PASS — fine")), None);
        assert_eq!(apply_prompt(&report("garbled")), None);
    }

    fn chat() -> (tempfile::TempDir, Chat) {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        };
        let theme = titi_tui::theme::loader::load_theme("titanium", &options).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut chat = Chat::new("openai/gpt-4.1", "session-1", std::sync::Arc::new(theme));
        chat.set_agent_dir(dir.path());
        (dir, chat)
    }

    fn enter(chat: &mut Chat, text: &str) -> Applied {
        let now = std::time::Instant::now();
        for ch in text.chars() {
            chat.on_key(crate::chat::Key::Char(ch), now);
        }
        chat.on_key(crate::chat::Key::Enter, now)
    }

    fn said(chat: &Chat) -> Vec<String> {
        chat.lines.iter().map(|line| line.text.clone()).collect()
    }

    fn edit(chat: &mut Chat, id: &str, path: &str) {
        use titi_engine::{EngineEvent, TurnId};
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: id.into(),
            name: "edit".into(),
            detail: Some(format!("edit {path}").into()),
        });
        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: id.into(),
            output: "edited".into(),
            is_error: false,
            detail: None,
        });
    }

    fn finished(chat: &mut Chat, text: &str) {
        chat.on_event(titi_engine::EngineEvent::ReviewFinished {
            report: report(text),
        });
    }

    #[test]
    fn slash_review_sends_the_files_the_agent_edited() {
        let (_dir, mut chat) = chat();
        edit(&mut chat, "c1", "src/b.rs");
        edit(&mut chat, "c2", "README.md");
        let applied = enter(&mut chat, "/review");
        assert_eq!(
            applied.effect,
            Some(crate::chat::ChatEffect::Send(EngineCommand::RunReview {
                edited: vec!["README.md".into(), "src/b.rs".into()],
            }))
        );
        assert!(applied.log.is_none(), "a review is not a user prompt");
        assert!(!chat.turn_active);
    }

    #[test]
    fn a_second_review_is_refused_until_the_first_answers() {
        let (_dir, mut chat) = chat();
        assert!(enter(&mut chat, "/review").effect.is_some());
        assert!(enter(&mut chat, "/review").effect.is_none());
        assert!(said(&chat).iter().any(|l| l.contains("already running")));
        finished(&mut chat, "VERDICT: PASS — fine");
        assert!(enter(&mut chat, "/review").effect.is_some());
    }

    #[test]
    fn a_verdict_shows_the_banner_then_the_findings_then_the_next_step() {
        let (_dir, mut chat) = chat();
        finished(
            &mut chat,
            "VERDICT: FAIL — missing test\n- a.rs:1 — one\n- b.rs:2 — two\n- c.rs:3 — three",
        );
        let shown = said(&chat);
        let at = shown
            .iter()
            .position(|l| l == "review · FAIL · 3 findings")
            .unwrap_or_else(|| panic!("{shown:?}"));
        assert_eq!(shown[at + 1], "missing test");
        assert_eq!(shown[at + 2], "  a.rs:1 \u{2014} one");
        assert_eq!(shown[at + 4], "  c.rs:3 \u{2014} three");
        assert!(shown[at + 5].contains("/review apply"), "{shown:?}");
        assert_eq!(chat.lines[at].kind, LineKind::Error, "a FAIL is red");
        assert!(
            chat.errors.log.is_empty(),
            "a verdict is the reviewer's answer, not a failure of titi's"
        );
    }

    #[test]
    fn a_pass_is_quiet_and_a_malformed_reply_shows_its_raw_text() {
        let (_dir, mut chat) = chat();
        finished(&mut chat, "VERDICT: PASS — fine");
        assert!(said(&chat).contains(&"review · PASS · 0 findings".to_owned()));
        let at = chat.lines.len();
        finished(&mut chat, "the code looks ok\nbut who knows");
        let shown = &said(&chat)[at..];
        assert_eq!(shown[0], "review · PARTIAL · 0 findings");
        assert!(
            shown.contains(&"  the code looks ok".to_owned()),
            "{shown:?}"
        );
        assert!(shown.contains(&"  but who knows".to_owned()), "{shown:?}");
    }

    #[test]
    fn nothing_to_review_says_so_and_is_never_recorded_as_a_verdict() {
        let (_dir, mut chat) = chat();
        chat.on_event(titi_engine::EngineEvent::ReviewFinished {
            report: ReviewReport::nothing("nothing to review"),
        });
        let shown = said(&chat);
        assert!(shown.contains(&"nothing to review".to_owned()), "{shown:?}");
        assert!(!shown.iter().any(|l| l.contains("PASS")), "{shown:?}");
        assert!(chat.reviews.latest().is_none());
        enter(&mut chat, "/review list");
        assert!(said(&chat).iter().any(|l| l.contains("no verdicts")));
    }

    #[test]
    fn apply_sends_the_last_findings_as_the_next_prompt() {
        let (_dir, mut chat) = chat();
        finished(&mut chat, "VERDICT: FAIL — x\n- a.rs:7 — broken");
        let applied = enter(&mut chat, "/review apply");
        let Some(crate::chat::ChatEffect::Send(EngineCommand::SubmitPrompt { text })) =
            applied.effect
        else {
            panic!("expected a prompt, got {:?}", applied.effect);
        };
        assert!(text.contains("returned FAIL (x)"), "{text}");
        assert!(text.contains("- a.rs:7 \u{2014} broken"), "{text}");
        assert!(chat.turn_active, "it opens a turn like any prompt");
        assert!(
            said(&chat).contains(&"review: sending 1 finding to the coder".to_owned()),
            "{:?}",
            said(&chat)
        );
        assert_eq!(applied.log.map(|log| log.text), Some(text.to_string()));
    }

    #[test]
    fn apply_steers_a_running_turn_and_refuses_without_findings() {
        let (_dir, mut chat) = chat();
        assert!(enter(&mut chat, "/review apply").effect.is_none());
        assert!(said(&chat).iter().any(|l| l.contains("/review first")));
        finished(&mut chat, "VERDICT: PASS — fine");
        assert!(enter(&mut chat, "/review apply").effect.is_none());
        assert!(
            said(&chat)
                .iter()
                .any(|l| l.contains("no findings to apply"))
        );
        finished(&mut chat, "VERDICT: FAIL — x\n- a.rs:7 — broken");
        chat.turn_active = true;
        let applied = enter(&mut chat, "/review apply");
        assert!(matches!(
            applied.effect,
            Some(crate::chat::ChatEffect::Send(EngineCommand::Steer { .. }))
        ));
    }

    #[test]
    fn list_shows_the_last_eight_and_an_unknown_argument_is_a_usage_error() {
        let (_dir, mut chat) = chat();
        for n in 0..10 {
            finished(
                &mut chat,
                &format!("VERDICT: FAIL — run {n}\n- a.rs:{n} — x"),
            );
        }
        let before = chat.lines.len();
        enter(&mut chat, "/review list");
        let rows = &said(&chat)[before..];
        assert_eq!(rows.len(), KEEP);
        assert_eq!(rows[0], "1. FAIL · 1 finding · run 9");
        enter(&mut chat, "/review frobnicate");
        assert!(
            said(&chat)
                .last()
                .is_some_and(|l| l.contains("usage: /review"))
        );
    }

    #[test]
    fn the_log_keeps_eight_newest_first_and_skips_nothing_to_review() {
        let mut log = ReviewLog::default();
        log.record(ReviewReport::nothing("nothing to review"));
        assert!(log.list().is_empty());
        for n in 0..10 {
            log.record(report(&format!("VERDICT: FAIL — run {n}\n- a.rs:{n} — x")));
        }
        let rows = log.list();
        assert_eq!(rows.len(), KEEP);
        assert_eq!(rows[0], "1. FAIL · 1 finding · run 9");
        assert_eq!(rows[7], "8. FAIL · 1 finding · run 2");
        assert_eq!(log.latest().unwrap().reason, "run 9");
    }
}
