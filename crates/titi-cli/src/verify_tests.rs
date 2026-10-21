//! `/verify` and the rows a verification leaves in the transcript.

use std::sync::Arc;

use titi_engine::protocol::VerifyOutcome;
use titi_engine::{EngineCommand, EngineEvent, TurnId};
use titi_tui::theme::Theme;

use crate::chat::{Chat, ChatEffect, WorkPhase};
use crate::transcript::LineKind;

fn theme() -> Arc<Theme> {
    let options = titi_tui::theme::loader::CreateThemeOptions {
        mode: Some(titi_tui::theme::ColorMode::Truecolor),
        ..Default::default()
    };
    match titi_tui::theme::loader::load_theme("titanium", &options) {
        Ok(theme) => Arc::new(theme),
        Err(reason) => panic!("built-in theme: {reason}"),
    }
}

struct Fixture {
    agent: tempfile::TempDir,
    repo: tempfile::TempDir,
    chat: Chat,
}

fn fixture() -> Fixture {
    let agent = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let mut chat = Chat::new("anthropic/claude-sonnet-5-5", "session-1", theme());
    chat.set_agent_dir(agent.path());
    Fixture { agent, repo, chat }
}

fn notes(chat: &Chat, kind: LineKind) -> Vec<String> {
    chat.lines
        .iter()
        .filter(|line| line.kind == kind)
        .map(|line| line.text.clone())
        .collect()
}

fn sent(effect: Option<ChatEffect>) -> Option<(Option<String>, bool)> {
    match effect {
        Some(ChatEffect::Send(EngineCommand::SetVerify { command, on_edit })) => {
            Some((command, on_edit))
        }
        _ => None,
    }
}

fn cargo_project(repo: &std::path::Path) {
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
}

#[test]
fn with_nothing_set_it_detects_once_and_offers_without_saving_or_running() {
    let mut f = fixture();
    cargo_project(f.repo.path());
    let applied = f.chat.verify_command_in("", f.repo.path());
    assert!(applied.effect.is_none(), "showing it runs nothing");
    let said = notes(&f.chat, LineKind::Note).join("\n");
    assert!(said.contains("no verify.command is set"), "{said}");
    assert!(said.contains("cargo check --all-targets"), "{said}");
    assert!(said.contains("/verify save"), "{said}");
    assert!(
        !f.agent.path().join("config.yml").exists(),
        "offering is not saving"
    );
}

#[test]
fn with_nothing_to_detect_it_says_so_and_guesses_nothing() {
    let mut f = fixture();
    let applied = f.chat.verify_command_in("", f.repo.path());
    assert!(applied.effect.is_none());
    let said = notes(&f.chat, LineKind::Note).join("\n");
    assert!(said.contains("no project check was detected"), "{said}");
    let saved = f.chat.verify_command_in("save", f.repo.path());
    assert!(saved.effect.is_none());
    assert!(
        notes(&f.chat, LineKind::Error)
            .join("\n")
            .contains("nothing to save")
    );
}

#[test]
fn save_writes_the_detected_line_to_the_user_config_and_arms_the_engine() {
    let mut f = fixture();
    cargo_project(f.repo.path());
    let applied = f.chat.verify_command_in("save", f.repo.path());
    let (command, on_edit) = sent(applied.effect).expect("the engine is told");
    let command = command.expect("a command");
    assert!(command.starts_with("cargo check"), "{command}");
    assert!(on_edit);

    let settings =
        titi_config::settings::Settings::load(f.agent.path(), f.repo.path(), &[]).unwrap();
    assert_eq!(
        settings.get_user("verify.command"),
        Some(serde_json::json!(command)),
        "it is the user's own setting"
    );
    assert!(
        !f.repo.path().join(".titi").exists(),
        "nothing was written into the repository"
    );

    let status = f.chat.verify_command_in("", f.repo.path());
    assert!(status.effect.is_none());
    assert!(
        notes(&f.chat, LineKind::Note)
            .last()
            .unwrap()
            .contains("your config's"),
    );

    let again = f.chat.verify_command_in("save", f.repo.path());
    assert!(again.effect.is_none());
    assert!(
        notes(&f.chat, LineKind::Error)
            .join("\n")
            .contains("already")
    );
}

#[test]
fn a_repositorys_command_waits_for_trust_and_trust_arms_it() {
    let mut f = fixture();
    std::fs::create_dir_all(f.repo.path().join(".titi")).unwrap();
    std::fs::write(
        f.repo.path().join(".titi/config.yml"),
        "verify:\n  command: make check\n",
    )
    .unwrap();

    let status = f.chat.verify_command_in("", f.repo.path());
    assert!(status.effect.is_none());
    let said = notes(&f.chat, LineKind::Note).join("\n");
    assert!(
        said.contains("make check") && said.contains("not trusted"),
        "{said}"
    );

    let trusted = f.chat.verify_command_in("trust", f.repo.path());
    assert_eq!(
        sent(trusted.effect),
        Some((Some("make check".to_owned()), true))
    );
    assert!(f.agent.path().join("trust.json").exists());

    let after = f.chat.verify_command_in("", f.repo.path());
    assert!(after.effect.is_none());
    assert!(
        notes(&f.chat, LineKind::Note)
            .last()
            .unwrap()
            .contains("this repository's, trusted")
    );
}

#[test]
fn an_unknown_argument_is_refused() {
    let mut f = fixture();
    let applied = f.chat.verify_command_in("run", f.repo.path());
    assert!(applied.effect.is_none());
    assert!(
        notes(&f.chat, LineKind::Error)
            .join("\n")
            .contains("usage: /verify [save|trust]")
    );
}

#[test]
fn a_verification_shows_on_the_status_row_and_leaves_its_result_under_the_turn() {
    let mut f = fixture();
    f.chat.on_event(EngineEvent::VerifyStarted {
        turn_id: TurnId(1),
        command: "cargo test".into(),
    });
    assert!(matches!(
        &f.chat.phase,
        WorkPhase::Tool { name, detail, .. }
            if name == "verify" && detail.as_deref() == Some("cargo test")
    ));
    f.chat.on_event(EngineEvent::VerifyFinished {
        turn_id: TurnId(1),
        command: "cargo test".into(),
        outcome: VerifyOutcome::Passed,
        exit_code: Some(0),
        ms: 3200,
        retrying: false,
    });
    assert!(matches!(f.chat.phase, WorkPhase::Waiting));
    assert_eq!(
        notes(&f.chat, LineKind::Note),
        vec!["✓ verified · cargo test · 3.2s"]
    );
}

#[test]
fn a_failure_sent_back_is_a_note_and_a_final_failure_is_an_error() {
    let mut f = fixture();
    for (retrying, ms) in [(true, 1000), (false, 2000)] {
        f.chat.on_event(EngineEvent::VerifyFinished {
            turn_id: TurnId(1),
            command: "make".into(),
            outcome: VerifyOutcome::Failed,
            exit_code: Some(2),
            ms,
            retrying,
        });
    }
    assert_eq!(
        notes(&f.chat, LineKind::Note),
        vec!["✗ verification failed · make · exit 2 · 1.0s · sent back to the model"]
    );
    assert_eq!(
        notes(&f.chat, LineKind::Error),
        vec!["✗ verification failed · make · exit 2 · 2.0s · the turn ended with it failing"]
    );
}
