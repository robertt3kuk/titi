//! `titi genome` — the terminal verbs, against the real binary.
//!
//! Every test points `TITI_AGENT_DIR` at a fresh temp dir, the same way the
//! headless tests do, so no machine's `~/.titi` is ever read or written.

#![allow(clippy::unwrap_used)]

use std::process::Command;

fn titi(agent_dir: &std::path::Path, cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_titi"));
    cmd.env("TITI_AGENT_DIR", agent_dir).current_dir(cwd);
    cmd
}

fn run(agent_dir: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let output = titi(agent_dir, cwd)
        .args(args)
        .output()
        .expect("the binary runs");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// A fresh agent directory reports the built-in state: on, unreasoned, at the
/// engine's default cap.
#[test]
fn status_on_an_empty_agent_dir_is_default_on() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    let (stdout, _, code) = run(dir.path(), dir.path(), &["genome"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("genome: on\n"), "{stdout}");
    assert!(stdout.contains("reason: default\n"), "{stdout}");
    assert!(stdout.contains("limit: 24\n"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "config: {}\n",
            dir.path().join("config.yml").display()
        )),
        "{stdout}"
    );
}

/// `off` persists a false `genome.enabled` on the agent's own config, and the
/// next status says so and names the setting as the reason.
#[test]
fn off_then_status_says_off_because_of_the_setting() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    assert!(!dir.path().join("config.yml").exists());
    let (stdout, _, code) = run(dir.path(), dir.path(), &["genome", "off"]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config.yml")).expect("config written"),
        "genome:\n  enabled: false\n"
    );
    let (stdout, _, _) = run(dir.path(), dir.path(), &["genome"]);
    assert!(stdout.contains("genome: off\n"), "{stdout}");
    assert!(stdout.contains("reason: setting\n"), "{stdout}");
}

/// `on` after `off` writes a true boolean; the two verbs are one key apart.
#[test]
fn on_after_off_flips_the_key_back() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    run(dir.path(), dir.path(), &["genome", "off"]);
    let (stdout, _, code) = run(dir.path(), dir.path(), &["genome", "on"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("genome: on\n"), "{stdout}");
    let (stdout, _, _) = run(dir.path(), dir.path(), &["genome"]);
    assert!(stdout.contains("genome: on\n"), "{stdout}");
    assert!(stdout.contains("reason: setting\n"), "{stdout}");
}

/// An in-range cap is stored on its own key, leaving `enabled` untouched, and
/// the status reports the effective value.
#[test]
fn limit_n_is_stored_and_reported() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    let (stdout, _, code) = run(dir.path(), dir.path(), &["genome", "limit", "4"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("genome limit: 4\n"), "{stdout}");
    let config = std::fs::read_to_string(dir.path().join("config.yml")).expect("config written");
    assert!(config.contains("enabled: false") == false, "{config}");
    let (stdout, _, _) = run(dir.path(), dir.path(), &["genome"]);
    assert!(stdout.contains("limit: 4\n"), "{stdout}");
}

/// Zero is refused with exit 2, and nothing is written: a typo must not seed
/// the config with a value every later run has to ignore.
#[test]
fn limit_zero_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    let (stdout, stderr, code) = run(dir.path(), dir.path(), &["genome", "limit", "0"]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("genome limit: expected an integer from 1 to 64"),
        "{stderr}"
    );
    assert!(
        !dir.path().join("config.yml").exists(),
        "nothing was written"
    );
}

/// Sixty-five is the other side of the same refusal.
#[test]
fn limit_above_the_ceiling_is_refused() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    let (_, stderr, code) = run(dir.path(), dir.path(), &["genome", "limit", "65"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("genome limit: expected an integer from 1 to 64"),
        "{stderr}"
    );
    let (_, _, code) = run(dir.path(), dir.path(), &["genome", "limit", "banana"]);
    assert_eq!(code, 2);
}

/// A limit with no number at all is the same refusal.
#[test]
fn limit_without_a_number_is_refused() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    let (_, stderr, code) = run(dir.path(), dir.path(), &["genome", "limit"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("genome limit: expected an integer from 1 to 64"),
        "{stderr}"
    );
}

/// The env switch outranks an explicit on-setting: the run is off and says
/// that it was the env, not the setting, that did it.
#[test]
fn titi_no_genome_names_itself_as_the_reason() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    run(dir.path(), dir.path(), &["genome", "off"]);
    run(dir.path(), dir.path(), &["genome", "on"]);
    let output = titi(dir.path(), dir.path())
        .args(["genome"])
        .env("TITI_NO_GENOME", "1")
        .output()
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("genome: off\n"), "{stdout}");
    assert!(stdout.contains("reason: TITI_NO_GENOME\n"), "{stdout}");
}

/// `check` and `lsp` are verbs this build does not wire: the message is the
/// contract, exit 2, and neither invents a server.
#[test]
fn check_and_lsp_say_they_are_not_wired() {
    for verb in ["check", "lsp"] {
        let dir = tempfile::tempdir().expect("temp agent dir");
        let (stdout, stderr, code) = run(dir.path(), dir.path(), &["genome", verb]);
        assert_eq!(code, 2, "{stdout}");
        assert!(stdout.is_empty(), "{stdout}");
        assert_eq!(
            stderr, "genome: check and lsp are not wired in this build\n",
            "{stderr}"
        );
    }
}

/// Any other subcommand names itself and the usage line, exit 2.
#[test]
fn an_unknown_genome_subcommand_is_named_and_shows_usage() {
    let dir = tempfile::tempdir().expect("temp agent dir");
    let (stdout, stderr, code) = run(dir.path(), dir.path(), &["genome", "wat"]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("genome: unknown command wat\n"), "{stderr}");
    assert!(
        stderr.contains("usage: titi genome [on|off|limit <n>|check|lsp]\n"),
        "{stderr}"
    );
}
