//! Git helpers: `git` for status and diff, `git_commit` for a commit, and
//! `diagnose` for a one-glance summary of the repository.
//!
//! The model never gets a general `git` escape hatch here. Only the verbs
//! below exist, and the ones this project forbids outright — `push`, `reset`,
//! `clean`, anything with `--force` or `--hard` — are refused with a typed
//! error instead of being forwarded. Every call runs `git` directly with an
//! argv (never through a shell, so a commit message cannot become a command),
//! non-interactively, with the pager off and a deadline: a hook that waits on
//! a terminal would otherwise hang the whole turn.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;
use titi_providers::ToolSpec;

use crate::fs::{arg_bool, arg_str, err, ok, readable_path};
use crate::pty::OUTPUT_CAP;
use crate::sensitive::SensitivePolicy;
use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// Deadline for one `git` call. Generous enough for a commit hook, short
/// enough that a hook waiting on input does not hold the turn.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the run loop looks at the child and the deadline.
const POLL: Duration = Duration::from_millis(10);

/// Verbs this project does not allow a tool to run, whatever the model asks.
const FORBIDDEN_VERBS: &[&str] = &[
    "push",
    "reset",
    "clean",
    "rebase",
    "checkout",
    "restore",
    "filter-branch",
];

/// Flags that turn an otherwise ordinary verb destructive.
const FORBIDDEN_FLAGS: &[&str] = &["--force", "-f", "--hard", "--force-with-lease", "-D"];

#[derive(Debug, Error)]
pub enum GitError {
    #[error("missing {0}")]
    MissingArg(&'static str),
    #[error("{op} is refused: titi never runs push, reset, clean, or a forced git command")]
    ForbiddenOp { op: String },
    #[error("{op} is not one of status, diff")]
    UnknownOp { op: String },
    #[error("{reason}")]
    Path { reason: String },
    #[error("not a git repository")]
    NotARepo,
    #[error("git could not be run: {reason}")]
    Spawn { reason: String },
    #[error("git {op} timed out after {seconds}s and was killed")]
    TimedOut { op: String, seconds: u64 },
    #[error("git {op} failed: {output}")]
    Failed { op: String, output: String },
}

/// `status` and `diff` for the workspace. Read-tier: it reports on the tree
/// and changes nothing.
pub struct GitTool {
    pub root: PathBuf,
    /// Credential files are cut out of a diff: `read` refuses `.env`, and a
    /// read-tier diff must not be the way around that refusal.
    pub policy: SensitivePolicy,
}

#[async_trait]
impl ToolHandler for GitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "git".into(),
                description: "Show git status or diff for the workspace".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["status", "diff"],
                            "description": "status: what changed. diff: how it changed."
                        },
                        "path": {
                            "type": "string",
                            "description": "Limit to one path inside the workspace."
                        },
                        "staged": {
                            "type": "boolean",
                            "description": "diff only: show the staged changes instead of the \
                                            unstaged ones. Default false."
                        }
                    },
                    "required": ["op"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match self.run(args) {
            Ok(output) => ok(output),
            Err(error) => err(error.to_string()),
        }
    }
}

impl GitTool {
    fn run(&self, args: Value) -> Result<String, GitError> {
        let op = arg_str(&args, "op").ok_or(GitError::MissingArg("op"))?;
        let op = op.trim().to_owned();
        check_allowed(&op)?;
        // No path means "everything under the workspace root": the `.`
        // pathspec keeps a repository that extends above the jail from
        // reporting files the other tools cannot even read.
        let pathspec = match arg_str(&args, "path") {
            Some(raw) => jailed_pathspec(&self.root, &raw, &self.policy)?,
            None => ".".to_owned(),
        };
        match op.as_str() {
            "status" => {
                let run = run_git(
                    &self.root,
                    "status",
                    &["status", "--short", "--branch", "--", &pathspec],
                )?;
                finish("status", run)
            }
            "diff" => {
                let mut argv = vec!["diff"];
                if arg_bool(&args, "staged").unwrap_or(false) {
                    argv.push("--cached");
                }
                argv.extend(["--", &pathspec]);
                let run = run_git(&self.root, "diff", &argv)?;
                finish("diff", run).map(|diff| withhold_credentials(&diff, &self.policy))
            }
            other => Err(GitError::UnknownOp {
                op: other.to_owned(),
            }),
        }
    }
}

/// Stage the named paths and commit. Write-tier: it writes to the repository.
pub struct GitCommitTool {
    pub root: PathBuf,
    /// A credential file is refused as a commit path: a key in the history is
    /// worse than a key on disk.
    pub policy: SensitivePolicy,
}

#[async_trait]
impl ToolHandler for GitCommitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "git_commit".into(),
                description: "Commit staged changes; optionally stage paths first".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Staged before committing. Omit to commit what is \
                                            already staged."
                        }
                    },
                    "required": ["message"]
                }),
            },
            approval: ApprovalTier::Write,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match self.run(args) {
            Ok(output) => ok(output),
            Err(error) => err(error.to_string()),
        }
    }
}

impl GitCommitTool {
    fn run(&self, args: Value) -> Result<String, GitError> {
        let message = arg_str(&args, "message").ok_or(GitError::MissingArg("message"))?;
        let message = message.trim().to_owned();
        if message.is_empty() {
            return Err(GitError::MissingArg("message"));
        }
        let mut staged = Vec::new();
        if let Some(paths) = args.get("paths").and_then(Value::as_array) {
            for path in paths {
                let raw = path
                    .as_str()
                    .ok_or(GitError::MissingArg("paths[] string"))?;
                staged.push(jailed_pathspec(&self.root, raw, &self.policy)?);
            }
        }
        if !staged.is_empty() {
            let mut argv = vec!["add", "--"];
            argv.extend(staged.iter().map(String::as_str));
            finish("add", run_git(&self.root, "add", &argv)?)?;
        }
        finish(
            "commit",
            run_git(&self.root, "commit", &["commit", "-m", &message])?,
        )
    }
}

/// Branch, cleanliness, last commit, conflicts — the four things worth
/// knowing before touching a repository. Read-tier.
pub struct DiagnoseTool {
    pub root: PathBuf,
}

#[async_trait]
impl ToolHandler for DiagnoseTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "diagnose".into(),
                description: "Summarise the repository: branch, tree state, last commit, conflicts"
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        match self.run() {
            Ok(output) => ok(output),
            Err(error) => err(error.to_string()),
        }
    }
}

impl DiagnoseTool {
    fn run(&self) -> Result<String, GitError> {
        let inside = run_git(
            &self.root,
            "rev-parse",
            &["rev-parse", "--is-inside-work-tree"],
        )?;
        if !inside.success {
            return Err(GitError::NotARepo);
        }
        let branch = self
            .capture(&["rev-parse", "--abbrev-ref", "HEAD"])?
            .unwrap_or_else(|| "(no commits yet)".to_owned());
        let changes = self
            .capture(&["status", "--porcelain", "--", "."])?
            .unwrap_or_default();
        let tree = if changes.is_empty() {
            "clean".to_owned()
        } else {
            format!("dirty ({} changed)", changes.lines().count())
        };
        let last = self
            .capture(&["log", "-1", "--format=%h %s"])?
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| "none yet".to_owned());
        let conflicted = self
            .capture(&["diff", "--name-only", "--diff-filter=U", "--", "."])?
            .unwrap_or_default();
        let conflicts = if conflicted.is_empty() {
            "none".to_owned()
        } else {
            conflicted.lines().collect::<Vec<_>>().join(", ")
        };
        Ok(format!(
            "branch: {branch}\ntree: {tree}\nlast commit: {last}\nconflicts: {conflicts}"
        ))
    }

    /// `None` when git answered non-zero: an empty repository has no `HEAD`
    /// and no log, and neither is a failure worth surfacing as an error.
    fn capture(&self, argv: &[&str]) -> Result<Option<String>, GitError> {
        let op = argv.first().copied().unwrap_or("git");
        let run = run_git(&self.root, op, argv)?;
        Ok(run.success.then(|| run.stdout.trim().to_owned()))
    }
}

pub fn git_tools(root: impl Into<PathBuf>, policy: SensitivePolicy) -> Vec<Box<dyn ToolHandler>> {
    let root = root.into();
    vec![
        Box::new(GitTool {
            root: root.clone(),
            policy: policy.clone(),
        }),
        Box::new(GitCommitTool {
            root: root.clone(),
            policy,
        }),
        Box::new(DiagnoseTool { root }),
    ]
}

/// Rejects the verbs this project forbids before anything is spawned.
fn check_allowed(op: &str) -> Result<(), GitError> {
    let lowered = op.to_ascii_lowercase();
    let mut tokens = lowered.split_whitespace();
    let verb = tokens.next().unwrap_or_default();
    if FORBIDDEN_VERBS.contains(&verb) {
        return Err(GitError::ForbiddenOp { op: op.to_owned() });
    }
    if lowered
        .split_whitespace()
        .any(|token| FORBIDDEN_FLAGS.contains(&token))
    {
        return Err(GitError::ForbiddenOp { op: op.to_owned() });
    }
    Ok(())
}

/// A path argument turned into a pathspec relative to the workspace root.
/// Outside the jail, or a credential file, is a typed refusal.
fn jailed_pathspec(root: &Path, raw: &str, policy: &SensitivePolicy) -> Result<String, GitError> {
    let resolved = readable_path(root, raw, policy).map_err(|reason| GitError::Path { reason })?;
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // Relative, never absolute: on macOS the root arrives as `/var/...` and
    // canonicalises to `/private/var/...`, and git rejects an absolute
    // pathspec that does not match the work tree it computed.
    let relative = resolved.strip_prefix(&canonical).unwrap_or(&resolved);
    Ok(relative.to_string_lossy().into_owned())
}

/// Turns one finished run into output or a typed error.
fn finish(op: &str, run: GitRun) -> Result<String, GitError> {
    if run.success {
        let mut text = run.stdout;
        if !run.stderr.trim().is_empty() {
            text.push_str(&run.stderr);
        }
        return Ok(text);
    }
    if run.stderr.contains("not a git repository") {
        return Err(GitError::NotARepo);
    }
    let mut output = run.stdout;
    output.push_str(&run.stderr);
    Err(GitError::Failed {
        op: op.to_owned(),
        output: output.trim().to_owned(),
    })
}

/// Drops the body of any file the credential policy blocks. `read` refuses
/// `.env`, so a read-tier diff must not print it instead.
fn withhold_credentials(diff: &str, policy: &SensitivePolicy) -> String {
    let mut out = String::with_capacity(diff.len());
    let mut withheld = false;
    for (index, section) in diff.split("\ndiff --git ").enumerate() {
        let section = if index == 0 {
            section.to_owned()
        } else {
            format!("\ndiff --git {section}")
        };
        let Some(header) = section.lines().next() else {
            continue;
        };
        if !header.starts_with("diff --git ") {
            out.push_str(&section);
            continue;
        }
        match diff_header_path(header) {
            // An unparseable header is withheld rather than trusted: a name
            // git had to quote is exactly where a disguised `.env` would hide.
            Some(path) if !policy.blocks(Path::new(&path)) => out.push_str(&section),
            _ => {
                withheld = true;
                out.push_str(&format!(
                    "\n{header}\n[titi: diff withheld; this file holds credentials]"
                ));
            }
        }
    }
    if withheld && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `diff --git a/src/x.rs b/src/x.rs` -> `src/x.rs`.
fn diff_header_path(header: &str) -> Option<String> {
    let rest = header.strip_prefix("diff --git a/")?;
    let (left, right) = rest.split_once(" b/")?;
    (left == right).then(|| left.to_owned())
}

/// What one `git` call produced.
struct GitRun {
    stdout: String,
    stderr: String,
    success: bool,
}

fn run_git(root: &Path, op: &str, args: &[&str]) -> Result<GitRun, GitError> {
    let mut child = Command::new("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(root)
        // No prompt, no pager, no editor: every one of them waits on a
        // terminal this call does not have.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitError::Spawn {
            reason: error.to_string(),
        })?;

    // Drained on threads: a diff larger than the pipe buffer would otherwise
    // block the child forever and turn every big diff into a timeout.
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || out_pipe.map(drain).unwrap_or_default());
    let err_reader = std::thread::spawn(move || err_pipe.map(drain).unwrap_or_default());

    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::Spawn {
                    reason: error.to_string(),
                });
            }
            Ok(Some(status)) => break status,
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitError::TimedOut {
                op: op.to_owned(),
                seconds: GIT_TIMEOUT.as_secs(),
            });
        }
        std::thread::sleep(POLL);
    };

    Ok(GitRun {
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        success: status.success(),
    })
}

/// Reads a pipe to the end, keeping at most [`OUTPUT_CAP`] bytes. The tail is
/// still read and dropped so the child never blocks on a full pipe.
fn drain(mut source: impl Read) -> String {
    let mut buffer = [0_u8; 4096];
    let mut kept: Vec<u8> = Vec::new();
    while let Ok(read) = source.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let room = OUTPUT_CAP.saturating_sub(kept.len());
        let take = room.min(read);
        kept.extend_from_slice(&buffer[..take]);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A real repository in a temp directory: `git` is the thing under test,
    /// so faking it would test nothing.
    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let repo = Self { dir };
            repo.git(&["init", "-q", "-b", "master"]);
            repo.git(&["config", "user.email", "titi@example.invalid"]);
            repo.git(&["config", "user.name", "titi test"]);
            repo
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .args(args)
                .current_dir(self.path())
                .output()
                .expect("git runs");
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text
        }

        fn write(&self, name: &str, body: &str) {
            fs::write(self.path().join(name), body).expect("the fixture file is written");
        }

        /// One tracked file and an initial commit, so a diff has a baseline.
        fn with_history(self) -> Self {
            self.write("tracked.txt", "one\n");
            self.git(&["add", "tracked.txt"]);
            self.git(&["commit", "-qm", "first"]);
            self
        }

        fn tools(&self) -> (GitTool, GitCommitTool, DiagnoseTool) {
            (
                GitTool {
                    root: self.path().to_path_buf(),
                    policy: SensitivePolicy::default(),
                },
                GitCommitTool {
                    root: self.path().to_path_buf(),
                    policy: SensitivePolicy::default(),
                },
                DiagnoseTool {
                    root: self.path().to_path_buf(),
                },
            )
        }
    }

    #[tokio::test]
    async fn status_shows_a_dirty_file() {
        let repo = Repo::new().with_history();
        repo.write("dirty.txt", "new\n");
        let (git, _, _) = repo.tools();

        let result = git.invoke(serde_json::json!({ "op": "status" })).await;

        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("dirty.txt"), "{}", result.output);
    }

    #[tokio::test]
    async fn diff_shows_the_change() {
        let repo = Repo::new().with_history();
        repo.write("tracked.txt", "one\ntwo\n");
        let (git, _, _) = repo.tools();

        let result = git.invoke(serde_json::json!({ "op": "diff" })).await;

        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("+two"), "{}", result.output);
    }

    /// A diff far bigger than a pipe buffer must come back capped, not hang:
    /// if the child's output were not drained while it runs, git would block
    /// on a full pipe and every large diff would end as a timeout.
    #[tokio::test]
    async fn a_huge_diff_is_capped_and_still_returns() {
        let repo = Repo::new().with_history();
        repo.write("tracked.txt", &"line\n".repeat(60_000));
        let (git, _, _) = repo.tools();

        let result = git.invoke(serde_json::json!({ "op": "diff" })).await;

        assert!(
            !result.is_error,
            "{}",
            &result.output[..200.min(result.output.len())]
        );
        assert!(result.output.len() <= OUTPUT_CAP, "{}", result.output.len());
        assert!(result.output.contains("+line"));
    }

    #[tokio::test]
    async fn a_credential_file_is_cut_out_of_the_diff() {
        let repo = Repo::new().with_history();
        repo.write(".env", "API_KEY=sk-test-0000\n");
        repo.git(&["add", "-f", ".env"]);
        repo.git(&["commit", "-qm", "env"]);
        repo.write(".env", "API_KEY=sk-test-1111\n");
        repo.write("tracked.txt", "one\ntwo\n");
        let (git, _, _) = repo.tools();

        let result = git.invoke(serde_json::json!({ "op": "diff" })).await;

        assert!(!result.is_error, "{}", result.output);
        assert!(!result.output.contains("sk-test"), "{}", result.output);
        assert!(result.output.contains("+two"), "the rest survives");
        assert!(result.output.contains("withheld"), "the cut is announced");
    }

    #[tokio::test]
    async fn commit_creates_a_commit() {
        let repo = Repo::new().with_history();
        repo.write("added.txt", "body\n");
        let (_, commit, _) = repo.tools();

        let result = commit
            .invoke(serde_json::json!({
                "message": "add a file",
                "paths": ["added.txt"]
            }))
            .await;

        assert!(!result.is_error, "{}", result.output);
        let log = repo.git(&["log", "--oneline"]);
        assert!(log.contains("add a file"), "{log}");
        assert_eq!(log.lines().count(), 2, "{log}");
    }

    #[tokio::test]
    async fn commit_refuses_a_credential_path() {
        let repo = Repo::new().with_history();
        repo.write(".env", "API_KEY=sk-test-0000\n");
        let (_, commit, _) = repo.tools();

        let result = commit
            .invoke(serde_json::json!({ "message": "oops", "paths": [".env"] }))
            .await;

        assert!(result.is_error, "a key must not reach the history");
        assert!(result.output.contains("credentials"), "{}", result.output);
    }

    #[tokio::test]
    async fn push_and_reset_hard_are_refused() {
        let repo = Repo::new().with_history();
        let (git, _, _) = repo.tools();

        for op in ["push", "reset --hard", "clean -f", "diff --force"] {
            let result = git.invoke(serde_json::json!({ "op": op })).await;
            assert!(result.is_error, "{op} must be refused");
            assert!(result.output.contains("refused"), "{}", result.output);
        }
    }

    #[tokio::test]
    async fn an_unknown_op_is_an_error() {
        let repo = Repo::new().with_history();
        let (git, _, _) = repo.tools();

        let result = git.invoke(serde_json::json!({ "op": "log" })).await;

        assert!(result.is_error);
        assert!(
            result.output.contains("not one of status, diff"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn a_path_outside_the_jail_is_refused() {
        let repo = Repo::new().with_history();
        let (git, _, _) = repo.tools();

        let result = git
            .invoke(serde_json::json!({ "op": "diff", "path": "../outside.txt" }))
            .await;

        assert!(result.is_error, "{}", result.output);
        assert!(
            result.output.contains("outside the workspace"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn diagnose_reports_branch_tree_and_last_commit() {
        let repo = Repo::new().with_history();
        repo.write("dirty.txt", "new\n");
        let (_, _, diagnose) = repo.tools();

        let result = diagnose.invoke(serde_json::json!({})).await;

        assert!(!result.is_error, "{}", result.output);
        assert!(
            result.output.contains("branch: master"),
            "{}",
            result.output
        );
        assert!(result.output.contains("dirty"), "{}", result.output);
        assert!(
            result.output.contains("first"),
            "last commit: {}",
            result.output
        );
        assert!(
            result.output.contains("conflicts: none"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn diagnose_names_a_conflict() {
        let repo = Repo::new().with_history();
        repo.git(&["checkout", "-q", "-b", "other"]);
        repo.write("tracked.txt", "theirs\n");
        repo.git(&["commit", "-qam", "theirs"]);
        repo.git(&["checkout", "-q", "master"]);
        repo.write("tracked.txt", "ours\n");
        repo.git(&["commit", "-qam", "ours"]);
        repo.git(&["merge", "other"]);
        let (_, _, diagnose) = repo.tools();

        let result = diagnose.invoke(serde_json::json!({})).await;

        assert!(!result.is_error, "{}", result.output);
        assert!(
            result.output.contains("conflicts: tracked.txt"),
            "{}",
            result.output
        );
    }

    #[test]
    fn the_tiers_split_reading_from_writing() {
        let tools = git_tools(".", SensitivePolicy::default());
        for tool in &tools {
            let definition = tool.definition();
            let expected = match definition.spec.name.as_str() {
                "git_commit" => ApprovalTier::Write,
                _ => ApprovalTier::Read,
            };
            assert_eq!(definition.approval, expected, "{}", definition.spec.name);
        }
        assert_eq!(tools.len(), 3);
    }
}
