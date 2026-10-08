//! Git-backed checkpoints: a rewind point that can undo code, not only text.
//!
//! A session checkpoint records how far the transcript had got. This records
//! where the workspace was, as a git commit, so rewinding can put the files
//! back too. The commit is local and never pushed; a dirty tree is committed
//! as-is, because that is exactly the state worth returning to.

use std::path::Path;
use std::process::Command;

use titi_tools::SensitivePolicy;

/// The credential policy the checkpoint answers to: the same user settings
/// the runtime tools read with, or the built-in list when settings cannot
/// load — a checkpoint is a commit, so it deserves no looser gate.
fn policy_for(workspace: &Path) -> SensitivePolicy {
    let settings = titi_config::settings::Settings::load(
        &titi_config::agent_dir(),
        workspace,
        &[],
    )
    .unwrap_or_default();
    crate::engine::privacy_policy(&settings).0
}

/// A commit that captures the workspace, or why one could not be made.
///
/// Commits only what is already staged. `git add -A` here would sweep up
/// whatever else was dirty — a checkpoint taken from a test once committed
/// the session's own uncommitted work — and it would clobber an index the
/// user was in the middle of building.
pub fn snapshot(workspace: &Path, label: &str) -> Result<String, String> {
    snapshot_with_policy(workspace, label, &policy_for(workspace))
}

/// `snapshot` with the policy made explicit: tests pass a built-in one, so
/// they do not depend on what the developer's own settings allow.
pub fn snapshot_with_policy(
    workspace: &Path,
    label: &str,
    policy: &SensitivePolicy,
) -> Result<String, String> {
    if !is_repo(workspace) {
        return Err("not a git repository".into());
    }
    let staged = run(workspace, &["diff", "--cached", "--name-only"])?;
    if staged.is_empty() {
        // Nothing staged: the tree already matches the index, so HEAD is the
        // snapshot. Unstaged work is left untouched on purpose.
        return run(workspace, &["rev-parse", "HEAD"]);
    }
    // The policy the read-tier tools answer to gates the checkpoint too: it
    // is a commit, so a staged `.env` would ride it into history and a later
    // push would publish it. It refuses rather than unstages in the user's
    // place — hooking `git restore --staged` would rewrite an index the user
    // was building, under the cover of a "checkpoint" — and names the file,
    // so the user can unstage it, or allow-list it if it really is not a
    // credential.
    for name in staged.lines() {
        if policy.blocks(Path::new(name)) {
            return Err(format!(
                "checkpoint not written: {name} is staged and holds credentials — \
                 titi does not commit it; `git restore --staged {name}` to leave it out, \
                 or allow-list it in privacy.allow first"
            ));
        }
    }
    run(
        workspace,
        &[
            "-c",
            "user.name=titi",
            "-c",
            "user.email=titi@localhost",
            "commit",
            "--no-verify",
            "-m",
            &format!("titi checkpoint: {label}"),
        ],
    )?;
    run(workspace, &["rev-parse", "HEAD"])
}

/// Put the workspace back at `commit`.
///
/// Refuses a dirty tree: restoring would throw away work the checkpoint does
/// not know about, and the user can checkpoint that first.
pub fn restore(workspace: &Path, commit: &str) -> Result<(), String> {
    if !is_repo(workspace) {
        return Err("not a git repository".into());
    }
    let dirty = run(workspace, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err("workspace has uncommitted changes; checkpoint them first".into());
    }
    run(workspace, &["reset", "--hard", commit])?;
    Ok(())
}

fn is_repo(workspace: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run(workspace: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {stderr}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        dir
    }

    fn stage(dir: &std::path::Path, file: &str) {
        assert!(
            Command::new("git")
                .args(["add", file])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn a_snapshot_captures_a_later_change() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        stage(dir.path(), "a.txt");
        let first = snapshot(dir.path(), "before").unwrap();

        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        stage(dir.path(), "a.txt");
        let _second = snapshot(dir.path(), "after").unwrap();

        restore(dir.path(), &first).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn restoring_refuses_a_dirty_tree() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        stage(dir.path(), "a.txt");
        let first = snapshot(dir.path(), "before").unwrap();
        std::fs::write(dir.path().join("a.txt"), "uncommitted").unwrap();

        let error = restore(dir.path(), &first).unwrap_err();
        assert!(error.contains("uncommitted"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "uncommitted"
        );
    }

    #[test]
    fn a_directory_that_is_not_a_repo_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let error = snapshot(dir.path(), "x").unwrap_err();
        assert!(error.contains("not a git repository"), "{error}");
    }

    /// A staged credential must not ride into the checkpoint's own history:
    /// assert on the commit's tree, not on a message the next commit could
    /// duplicate by accident. The policy is given explicitly, so the tests do
    /// not depend on what the developer's own settings allow-list.
    #[test]
    fn a_staged_credential_file_is_not_checkpointed() {
        let dir = repo();
        // An initial commit, so HEAD has a tree to compare against.
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "base"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
            .then_some(())
            .unwrap();
        let head_before = run(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(dir.path().join("a.txt"), "keep\n").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
        stage(dir.path(), "a.txt");
        stage(dir.path(), ".env");

        let error = snapshot_with_policy(dir.path(), "with secret", &SensitivePolicy::default())
            .unwrap_err();
        assert!(error.contains(".env"), "{error}");
        assert!(error.contains("restore --staged"), "{error}");

        // The refusal names one file at a time comes after all names are
        // checked for the first hit; the index is untouched either way.
        assert!(
            Command::new("git")
                .args(["diff", "--cached", "--name-only"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout
                .starts_with(b".env"),
            "staging was rewritten"
        );
        // No commit was written: HEAD still points at the base one.
        let head_after = run(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            head_after, head_before,
            "the refusal left a commit behind"
        );

        // Allowed next: once the credential is out of the index, the rest of
        // the staged work still checkpoints.
        assert!(
            Command::new("git")
                .args(["restore", "--staged", ".env"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let commit =
            snapshot_with_policy(dir.path(), "clean", &SensitivePolicy::default()).unwrap();
        let tree = run(dir.path(), &["ls-tree", "-r", "--name-only", &commit]).unwrap();
        assert_eq!(tree, "a.txt", "checkpoint committed more than staged: {tree}");
    }

    #[test]
    fn ordinary_staged_work_still_checkpoints() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "first\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        stage(dir.path(), "a.txt");
        stage(dir.path(), "src/main.rs");

        let commit = snapshot_with_policy(dir.path(), "ordinary", &SensitivePolicy::default())
            .unwrap();
        let status = run(dir.path(), &["status", "--porcelain"]).unwrap();
        assert!(status.is_empty(), "commit failed: {status}");
        let tree = run(dir.path(), &["ls-tree", "-r", "--name-only", &commit]).unwrap();
        assert_eq!(tree, "a.txt\nsrc/main.rs", "{tree}");
    }
}
