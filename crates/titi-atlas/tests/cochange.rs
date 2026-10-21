#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Files that change together are linked, though nothing imports anything.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use titi_atlas::live::Options;
use titi_atlas::{Atlas, AtlasHandle, LensBudget, SeedInput, probe_cochange};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.email=t@example.invalid", "-c", "user.name=t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `parser.rs` and `printer.rs` never mention each other and are edited
/// together in every commit; `lonely.rs` is edited alone.
fn repo_with_history() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    write(root, "src/parser.rs", "pub fn parse_tokens() {}\n");
    write(root, "src/printer.rs", "pub fn print_tree() {}\n");
    write(root, "src/lonely.rs", "pub fn alone() {}\n");
    git(root, &["add", "src/lonely.rs"]);
    git(root, &["commit", "-q", "-m", "start"]);
    for n in 0..4 {
        write(
            root,
            "src/parser.rs",
            &format!("pub fn parse_tokens() {{ /* {n} */ }}\n"),
        );
        write(
            root,
            "src/printer.rs",
            &format!("pub fn print_tree() {{ /* {n} */ }}\n"),
        );
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", &format!("together {n}")]);
        write(
            root,
            "src/lonely.rs",
            &format!("pub fn alone() {{ /* {n} */ }}\n"),
        );
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", &format!("alone {n}")]);
    }
    dir
}

#[test]
fn history_links_files_nothing_imports() {
    let dir = repo_with_history();
    let root = dir.path();
    let mut atlas = Atlas::index(root).unwrap();
    let prompt = "parse_tokens drops the last token";
    let seeds = |atlas: &Atlas| {
        atlas.seeds(&SeedInput {
            prompt,
            ..SeedInput::default()
        })
    };

    let before = atlas.lens(&seeds(&atlas), LensBudget::tokens(400));
    assert_eq!(
        before.entries.len(),
        1,
        "no edge, so only the seed: {before:?}"
    );

    atlas.set_cochange(probe_cochange(root).expect("history"));
    let after = atlas.lens(&seeds(&atlas), LensBudget::tokens(400));
    let listed: Vec<&str> = after.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(listed[0], "src/parser.rs", "{listed:?}");
    assert!(listed.contains(&"src/printer.rs"), "{listed:?}");
    assert!(!listed.contains(&"src/lonely.rs"), "{listed:?}");
    let partner = after
        .entries
        .iter()
        .find(|e| e.path == "src/printer.rs")
        .unwrap();
    assert_eq!(partner.why, "changes with src/parser.rs");
}

#[test]
fn a_tree_without_history_has_no_cochange() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.rs", "pub fn a() {}\n");
    assert!(probe_cochange(dir.path()).is_none());
}

/// The handle reads history on a thread of its own and publishes it: the first
/// frame and the first turn never wait for `git`.
#[test]
fn the_handle_publishes_history_when_it_arrives() {
    let dir = repo_with_history();
    let handle = AtlasHandle::spawn(dir.path(), Options::default()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (atlas, _) = handle.snapshot();
        if !atlas.cochange().is_empty() {
            assert!(!atlas.cochange().partners("src/parser.rs").is_empty());
            break;
        }
        assert!(Instant::now() < deadline, "history never arrived");
        std::thread::sleep(Duration::from_millis(25));
    }
}
