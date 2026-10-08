//! `titi genome` — the terminal verbs, against the real binary.
//!
//! Every test points `TITI_AGENT_DIR` at a fresh temp dir, the same way the
//! headless tests do, so no machine's `~/.titi` is ever read or written.

#![allow(clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;
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

/// A directory the process may enter but not read, for the workspace the verbs
/// must refuse. `None` when the run is privileged enough that the mode bits do
/// not stop `read_dir` — the shape cannot be built there, so the caller skips.
///
/// The verbs take their workspace from the process cwd, so this is the one
/// "cannot be read as a directory" shape a child process can be pointed at;
/// the library tests cover the missing-path and regular-file roots.
fn unreadable_root(parent: &std::path::Path) -> Option<std::path::PathBuf> {
    let path = parent.join("locked");
    std::fs::create_dir(&path).expect("temp dir");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o111)).expect("chmod");
    if std::fs::read_dir(&path).is_ok() {
        eprintln!("skipping: mode bits are bypassed, so no unreadable root exists");
        return None;
    }
    Some(path)
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

/// A broken import is named with its file, line, code and message, and the
/// check exits 1. An ambiguous-only tree exits 0 with the lines printed: an
/// ambiguous name is a hint, not a defect. Clean is clean at exit 0. Check
/// reads the workspace, not the agent config, so neither test writes one.
#[test]
fn check_reports_unresolved_imports_and_a_clean_tree_is_clean() {
    let broken = tempfile::tempdir().expect("temp tree");
    let src_dir = broken.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "use crate::missing::Thing;\npub fn present() {}\n",
    )
    .unwrap();
    let (stdout, stderr, code) = run(broken.path(), broken.path(), &["genome", "check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert!(
        stdout.contains("unresolved-import") && stdout.contains("missing"),
        "{stdout}"
    );
    assert!(stdout.contains(":1: unresolved-import"), "{stdout}");

    let clean = tempfile::tempdir().expect("temp tree");
    let src_dir = clean.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub fn ok() {}\n").unwrap();
    let (stdout, stderr, code) = run(clean.path(), clean.path(), &["genome", "check"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(stdout, "genome: clean\n", "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
}

/// A tree whose only diagnostics are ambiguous symbols prints them and exits
/// 0: the same name in two files is a hint that name-only resolution cannot
/// pick one, not a defect. Only a Warning or Error means exit 1.
#[test]
fn check_treats_an_ambiguous_only_tree_as_clean() {
    let dir = tempfile::tempdir().expect("temp tree");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub mod other;\npub struct Session;\npub fn setup() {}\n",
    )
    .unwrap();
    std::fs::create_dir_all(src_dir.join("other")).unwrap();
    std::fs::write(src_dir.join("other/mod.rs"), "pub struct Session;\n").unwrap();
    let (stdout, stderr, code) = run(dir.path(), dir.path(), &["genome", "check"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(
        stdout.lines().count(),
        1,
        "one ambiguous-symbol line for the ambiguous name: {stdout}"
    );
    assert!(
        stdout.lines().all(|line| line.contains("ambiguous-symbol")),
        "{stdout}"
    );
}

/// A tree the indexer cannot even walk blames itself, not a clean report.
///
/// The root is unreadable: that is a different case from a good root holding
/// one odd child, and the only one the verbs must refuse.
#[test]
fn check_names_a_failed_index() {
    let dir = tempfile::tempdir().expect("temp tree");
    let Some(unreadable) = unreadable_root(dir.path()) else {
        return;
    };
    let (_, stderr, code) = run(dir.path(), &unreadable, &["genome", "check"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("genome: check failed ("), "{stderr}");
}

/// The lsp verb is a server on stdio: one `initialize` request and an `exit`
/// notification travel as Content-Length frames, the answer names the server,
/// and the process ends 0 with no socket and no banner before the frames.
#[test]
fn lsp_answers_initialize_and_exits_cleanly() {
    let dir = tempfile::tempdir().expect("temp tree");
    let frame = |body: &str| format!("Content-Length: {}\r\n\r\n{body}", body.len());
    let initialize = frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let exit = frame(r#"{"jsonrpc":"2.0","method":"exit"}"#);
    use std::io::Write as _;
    let mut child = titi(dir.path(), dir.path())
        .args(["genome", "lsp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    {
        let stdin = child.stdin.as_mut().expect("stdin piped");
        stdin.write_all(initialize.as_bytes()).unwrap();
        stdin.write_all(exit.as_bytes()).unwrap();
    }
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("\"name\":\"titi-genome\""), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
}

/// A workspace the server cannot index fails loudly rather than hanging.
///
/// Same unreadable root as `check`: the server must report it instead of
/// answering an empty map.
#[test]
fn lsp_names_a_failed_index() {
    let dir = tempfile::tempdir().expect("temp tree");
    let Some(unreadable) = unreadable_root(dir.path()) else {
        return;
    };
    let (_, stderr, code) = run(dir.path(), &unreadable, &["genome", "lsp"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("genome: lsp failed ("), "{stderr}");
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
