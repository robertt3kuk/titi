#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! `[RECENT]` is about the work, not the clock of the checkout.
//!
//! Inside a git repository a file is recent when it has uncommitted changes
//! or a commit made within 48 hours changed it. A clone, a checkout or a
//! cherry-pick stamps every file with the time it ran, so an mtime says
//! nothing about the history.

use std::fs;
use std::path::Path;
use std::process::Command;

use titi_atlas::Atlas;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn git(root: &Path, args: &[&str], date: Option<&str>) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args);
    if let Some(date) = date {
        command
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_DATE", date);
    }
    let status = command.status().unwrap();
    assert!(status.success(), "git {args:?}");
}

fn init(root: &Path) {
    git(root, &["init", "-q"], None);
}

fn commit_all(root: &Path, date: &str) {
    git(root, &["add", "-A"], None);
    git(root, &["commit", "-q", "-m", "c"], Some(date));
}

const LONG_AGO: &str = "2020-01-01T00:00:00Z";

fn recent_paths(atlas: &Atlas) -> Vec<String> {
    atlas
        .project(64)
        .lines()
        .filter(|line| line.ends_with(" [RECENT]"))
        .map(|line| line.split(' ').next().unwrap().to_owned())
        .collect()
}

/// The reported bug: every file of a clone is `[RECENT]` because its mtime is
/// the minute the checkout ran, though the last commit is years old.
#[test]
fn a_checkout_of_old_history_marks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init(root);
    write(root, "src/a.rs", "pub fn alpha() {}\n");
    write(root, "src/b.rs", "pub fn bravo() {}\n");
    commit_all(root, LONG_AGO);

    let atlas = Atlas::index(root).unwrap();
    assert_eq!(
        recent_paths(&atlas),
        Vec::<String>::new(),
        "{}",
        atlas.project(8)
    );
}

#[test]
fn a_commit_inside_the_window_is_recent_and_an_old_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init(root);
    write(root, "src/old.rs", "pub fn old_one() {}\n");
    commit_all(root, LONG_AGO);
    write(root, "src/new.rs", "pub fn new_one() {}\n");
    git(root, &["add", "-A"], None);
    git(root, &["commit", "-q", "-m", "c2"], None);

    let atlas = Atlas::index(root).unwrap();
    assert_eq!(recent_paths(&atlas), vec!["src/new.rs".to_owned()]);
}

/// Worktree edits, staged edits and untracked files are all work in progress.
#[test]
fn uncommitted_staged_and_untracked_files_are_recent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init(root);
    for name in ["edited", "staged", "quiet"] {
        write(
            root,
            &format!("src/{name}.rs"),
            &format!("pub fn {name}_fn() {{}}\n"),
        );
    }
    commit_all(root, LONG_AGO);
    write(root, "src/edited.rs", "pub fn edited_fn() { /* wip */ }\n");
    write(root, "src/staged.rs", "pub fn staged_fn() { /* wip */ }\n");
    git(root, &["add", "src/staged.rs"], None);
    write(root, "src/fresh.rs", "pub fn fresh_fn() {}\n");

    let atlas = Atlas::index(root).unwrap();
    let mut recent = recent_paths(&atlas);
    recent.sort();
    assert_eq!(
        recent,
        vec!["src/edited.rs", "src/fresh.rs", "src/staged.rs"],
        "{}",
        atlas.project(8)
    );
}

/// An index rooted below the repository's top level still lines the git
/// paths up with its own: `sub/` is stripped, files outside it are ignored.
#[test]
fn a_root_below_the_repository_top_level_lines_up() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path();
    init(top);
    write(top, "sub/src/a.rs", "pub fn alpha() {}\n");
    write(top, "sub/src/b.rs", "pub fn bravo() {}\n");
    write(top, "other/c.rs", "pub fn charlie() {}\n");
    commit_all(top, LONG_AGO);
    write(top, "sub/src/b.rs", "pub fn bravo() { /* wip */ }\n");
    write(top, "other/c.rs", "pub fn charlie() { /* wip */ }\n");

    let atlas = Atlas::index(top.join("sub")).unwrap();
    assert_eq!(recent_paths(&atlas), vec!["src/b.rs".to_owned()]);
}

/// A file the session edits after the index was built is uncommitted work as
/// of that edit; the index does not run git again to find out.
#[test]
fn a_file_edited_after_the_index_was_built_becomes_recent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    init(root);
    write(root, "src/a.rs", "pub fn alpha() {}\n");
    write(root, "src/b.rs", "pub fn bravo() {}\n");
    commit_all(root, LONG_AGO);
    let mut atlas = Atlas::index(root).unwrap();
    assert!(recent_paths(&atlas).is_empty());

    write(root, "src/a.rs", "pub fn alpha() { /* edited */ }\n");
    atlas.apply_changes(&["src/a.rs".to_owned()]).unwrap();
    assert_eq!(recent_paths(&atlas), vec!["src/a.rs".to_owned()]);

    write(root, "src/c.rs", "pub fn charlie() {}\n");
    atlas.refresh(root).unwrap();
    let mut recent = recent_paths(&atlas);
    recent.sort();
    assert_eq!(recent, vec!["src/a.rs", "src/c.rs"]);
}

/// Outside a repository there is no history to ask, so the clock decides.
#[test]
fn outside_git_the_modification_time_decides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/live.rs", "pub fn live_fn() {}\n");
    write(root, "src/stale.rs", "pub fn stale_fn() {}\n");
    let stale_at = std::time::SystemTime::now() - std::time::Duration::from_secs(49 * 3600);
    fs::File::options()
        .write(true)
        .open(root.join("src/stale.rs"))
        .unwrap()
        .set_modified(stale_at)
        .unwrap();

    let atlas = Atlas::index(root).unwrap();
    assert_eq!(recent_paths(&atlas), vec!["src/live.rs".to_owned()]);
}
