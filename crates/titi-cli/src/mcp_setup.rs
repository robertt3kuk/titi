//! Which MCP servers a session runs: the surface's half of the trust rule.
//!
//! The engine runs only the servers it is handed (`EngineCommand::SetMcp`)
//! and never reads an MCP file or the trust store. This module reads the
//! files, asks the store, and splits what it finds in two, exactly as
//! [`crate::hooks_setup`] does for hooks:
//!
//! * servers from the user's own `<agent_dir>/mcp.json` run unconditionally;
//! * servers from the repository (`.titi/mcp.json`, `.mcp.json`) run only when
//!   the trust store holds the hash of that exact definition. The rest are
//!   *pending*: the screen offers them to the user in the same panel as the
//!   repository's hooks, a headless run skips them and says so unless
//!   `--trust-project` was passed.
//!
//! The approval tier a server's tools run under is `exec` unless the user's
//! own settings (`mcp.<server>.approval`) say otherwise. Only the user's own
//! servers get that: a repository cannot name a server after a setting the
//! user wrote for something else and inherit its lowered tier.

use std::path::{Path, PathBuf};

use titi_config::mcp::{self, McpServerDef};
use titi_config::settings::Settings;
use titi_config::trust::{KIND_MCP, TrustError, TrustStore};

use crate::hooks_setup::{Choice, sanitize};

/// The MCP servers of one session, before and after the user has been asked.
#[derive(Debug)]
pub struct McpStartup {
    /// Handed to the engine at start: the user's servers and the project
    /// servers already trusted.
    pub approved: Vec<McpServerDef>,
    /// Project servers nobody has approved.
    pub pending: Vec<McpServerDef>,
    /// Lines for a file that could not be read, an entry that was skipped, or
    /// a trust store that could not be: shown to the user, never silent.
    pub notices: Vec<String>,
    pub workspace: PathBuf,
    store: TrustStore,
}

impl McpStartup {
    /// Nothing to start and nothing to ask about.
    pub fn none(agent_dir: &Path, workspace: &Path) -> Self {
        Self {
            approved: Vec::new(),
            pending: Vec::new(),
            notices: Vec::new(),
            workspace: workspace.to_owned(),
            store: TrustStore::empty(agent_dir),
        }
    }

    /// Reads the MCP files and sorts them. `trust_project` is
    /// `--trust-project`: the repository's pending servers run this session,
    /// recorded nowhere.
    pub fn resolve(
        agent_dir: &Path,
        workspace: &Path,
        settings: Option<&Settings>,
        trust_project: bool,
    ) -> Self {
        let found = mcp::discover(agent_dir, workspace);
        let mut notices = found.notices;
        let store = match TrustStore::load(agent_dir) {
            Ok(store) => store,
            Err(error) => {
                notices.push(format!("mcp: {error}; no project server is trusted"));
                TrustStore::empty(agent_dir)
            }
        };
        let mut approved = found.user;
        if let Some(settings) = settings {
            for def in &mut approved {
                match mcp::approval_setting(settings, &def.name) {
                    Ok(level) => def.approval = level,
                    Err(reason) => notices.push(format!("mcp: {reason}; {} stays exec", def.name)),
                }
            }
        }
        let mut pending = Vec::new();
        for def in found.project {
            if store.is_trusted(workspace, KIND_MCP, &def.hash(workspace)) {
                approved.push(def);
            } else {
                pending.push(def);
            }
        }
        if trust_project && !pending.is_empty() {
            notices.push(format!(
                "mcp: --trust-project starts {} untrusted project server(s) this session",
                pending.len()
            ));
            approved.append(&mut pending);
        }
        Self {
            approved,
            pending,
            notices,
            workspace: workspace.to_owned(),
            store,
        }
    }

    /// The lines a run with no screen prints: the notices, and what was left
    /// out for lack of trust.
    pub fn headless_notices(&self) -> Vec<String> {
        let mut lines = self.notices.clone();
        if !self.pending.is_empty() {
            lines.push(format!(
                "mcp: skipped {} untrusted project server(s) [{}]; run titi here to review them, or pass --trust-project",
                self.pending.len(),
                self.pending
                    .iter()
                    .map(describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }

    /// Applies the answer. Returns the full server list the engine should now
    /// run when the answer adds any, `None` for [`Choice::No`]. `Trust` is
    /// written to the store before it returns; a store that cannot be written
    /// is an error and the servers are not started.
    pub fn decide(&mut self, choice: Choice) -> Result<Option<Vec<McpServerDef>>, TrustError> {
        match choice {
            Choice::No => Ok(None),
            Choice::Once | Choice::Trust => {
                if choice == Choice::Trust {
                    for def in &self.pending {
                        self.store.trust(
                            &self.workspace,
                            KIND_MCP,
                            &def.hash(&self.workspace),
                            &label(def),
                        )?;
                    }
                }
                let mut all = self.approved.clone();
                all.extend(self.pending.iter().cloned());
                Ok(Some(all))
            }
        }
    }
}

/// `files · npx -y some-server` — how a server is named to a person. The name
/// and the command come from a repository, and a headless run prints this to
/// a terminal: control characters are stripped.
pub fn describe(def: &McpServerDef) -> String {
    format!(
        "{} · {}",
        sanitize(&def.name),
        sanitize(&def.command_line())
    )
}

/// What the store records beside a hash, so the file is readable by hand.
fn label(def: &McpServerDef) -> String {
    format!("mcp {} [{}]: {}", def.name, def.source, def.command_line())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = r#"{"mcpServers": {"theirs": {"command": "./bin/srv", "args": ["--x"]}}}"#;
    const USER: &str = r#"{"mcpServers": {"mine": {"command": "my-srv"}}}"#;

    struct Setup {
        agent: tempfile::TempDir,
        repo: tempfile::TempDir,
    }

    fn setup(repo_file: &str, repo_json: &str) -> Setup {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(agent.path().join("mcp.json"), USER).unwrap();
        let path = repo.path().join(repo_file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, repo_json).unwrap();
        Setup { agent, repo }
    }

    fn resolve(s: &Setup, trust_project: bool) -> McpStartup {
        McpStartup::resolve(s.agent.path(), s.repo.path(), None, trust_project)
    }

    #[test]
    fn user_servers_run_and_untrusted_project_servers_wait() {
        let s = setup(".mcp.json", REPO);
        let start = resolve(&s, false);
        assert_eq!(start.approved.len(), 1);
        assert_eq!(start.approved[0].name, "mine");
        assert_eq!(start.pending.len(), 1);
        assert_eq!(start.pending[0].command, "./bin/srv");
    }

    #[test]
    fn a_headless_run_skips_untrusted_servers_and_says_which_without_control_characters() {
        let hostile = r#"{"mcpServers": {"evil\u001b]52;c;ZXZpbA==\u0007": {"command": "\u001b[2Kdone.sh\u0007"}}}"#;
        let s = setup(".titi/mcp.json", hostile);
        let lines = resolve(&s, false).headless_notices();
        let text = lines.join("\n");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            text.contains("skipped 1 untrusted project server"),
            "{text:?}"
        );
        assert!(text.contains("--trust-project"), "{text:?}");
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "control characters reached the terminal: {text:?}"
        );
    }

    #[test]
    fn trust_project_starts_them_once_and_records_nothing() {
        let s = setup(".titi/mcp.json", REPO);
        let start = resolve(&s, true);
        assert!(start.pending.is_empty());
        assert_eq!(start.approved.len(), 2);
        assert!(
            start
                .notices
                .iter()
                .any(|line| line.contains("--trust-project"))
        );
        assert!(!s.agent.path().join("trust.json").exists());
        assert_eq!(resolve(&s, false).pending.len(), 1);
    }

    #[test]
    fn trusting_persists_and_once_does_not() {
        let s = setup(".mcp.json", REPO);
        let mut start = resolve(&s, false);
        let all = start
            .decide(Choice::Once)
            .unwrap()
            .expect("servers to start");
        assert_eq!(all.len(), 2);
        assert_eq!(
            resolve(&s, false).pending.len(),
            1,
            "once is not remembered"
        );

        let mut start = resolve(&s, false);
        assert!(start.decide(Choice::No).unwrap().is_none());
        assert_eq!(resolve(&s, false).pending.len(), 1);

        let mut start = resolve(&s, false);
        start.decide(Choice::Trust).unwrap();
        let later = resolve(&s, false);
        assert!(later.pending.is_empty(), "trusted servers start unasked");
        assert_eq!(later.approved.len(), 2);
    }

    #[test]
    fn an_edited_project_server_loses_its_trust() {
        let s = setup(".mcp.json", REPO);
        resolve(&s, false).decide(Choice::Trust).unwrap();
        assert!(resolve(&s, false).pending.is_empty());

        // The command, an argument, the cwd, an env key or an env value: each
        // is a different program to start, so each asks again.
        for edited in [
            REPO.replace("./bin/srv", "./bin/srv2"),
            REPO.replace("--x", "--x --y"),
            REPO.replace("\"args\"", "\"cwd\": \"sub\", \"args\""),
            REPO.replace("\"args\"", "\"env\": {\"A\": \"1\"}, \"args\""),
        ] {
            std::fs::write(s.repo.path().join(".mcp.json"), &edited).unwrap();
            let after = resolve(&s, false);
            assert_eq!(after.pending.len(), 1, "{edited}");
            assert_eq!(after.approved.len(), 1, "only the user's own runs");
        }
        // Edits that do not change what runs keep the consent.
        std::fs::write(
            s.repo.path().join(".mcp.json"),
            r#"{"mcpServers": {"theirs": {"timeout": 5, "args": ["--x"], "command": "./bin/srv"}}}"#,
        )
        .unwrap();
        assert!(resolve(&s, false).pending.is_empty());
    }

    #[test]
    fn an_edited_script_the_server_runs_loses_trust_though_the_definition_is_unchanged() {
        let s = setup(
            ".mcp.json",
            r#"{"mcpServers": {"theirs": {"command": "node", "args": ["srv.js"]}}}"#,
        );
        std::fs::write(s.repo.path().join("srv.js"), "console.log(1)\n").unwrap();
        resolve(&s, false).decide(Choice::Trust).unwrap();
        assert!(resolve(&s, false).pending.is_empty());

        std::fs::write(s.repo.path().join("srv.js"), "require('child_process')\n").unwrap();
        assert_eq!(resolve(&s, false).pending.len(), 1);
    }

    #[test]
    fn the_user_can_lower_their_own_servers_approval_and_a_project_cannot() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(agent.path().join("mcp.json"), USER).unwrap();
        std::fs::write(
            repo.path().join(".mcp.json"),
            r#"{"mcpServers": {"theirs": {"command": "x", "approval": "read"}}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo.path().join(".titi")).unwrap();
        // The repository's own config tries to lower both.
        std::fs::write(
            repo.path().join(".titi/config.yml"),
            "mcp:\n  mine:\n    approval: read\n  theirs:\n    approval: read\n",
        )
        .unwrap();
        // The user's lowers `mine` and also names `theirs`, a server that is
        // not theirs to lower.
        std::fs::write(
            agent.path().join("config.yml"),
            "mcp:\n  mine:\n    approval: write\n  theirs:\n    approval: read\n  odd:\n    approval: sure\n",
        )
        .unwrap();
        let settings = Settings::load(agent.path(), repo.path(), &[]).unwrap();
        let mut start = McpStartup::resolve(agent.path(), repo.path(), Some(&settings), false);
        assert_eq!(
            start.approved[0].approval,
            Some(titi_config::mcp::McpApproval::Write)
        );
        let all = start.decide(Choice::Once).unwrap().unwrap();
        let theirs = all.iter().find(|def| def.name == "theirs").unwrap();
        assert_eq!(theirs.approval, None, "a project server stays exec");

        // A word that is not a tier is said, and the server stays exec.
        std::fs::write(
            agent.path().join("mcp.json"),
            r#"{"mcpServers": {"odd": {"command": "y"}}}"#,
        )
        .unwrap();
        let start = McpStartup::resolve(agent.path(), repo.path(), Some(&settings), false);
        assert_eq!(start.approved[0].approval, None);
        assert!(
            start
                .notices
                .iter()
                .any(|line| line.contains("mcp.odd.approval")),
            "{:?}",
            start.notices
        );
    }

    #[test]
    fn a_corrupt_trust_store_trusts_nothing_and_says_so() {
        let s = setup(".mcp.json", REPO);
        std::fs::write(s.agent.path().join("trust.json"), "{nope").unwrap();
        let start = resolve(&s, false);
        assert_eq!(start.pending.len(), 1);
        assert!(start.notices.iter().any(|line| line.contains("trust")));
    }
}
