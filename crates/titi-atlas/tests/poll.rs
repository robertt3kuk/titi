#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The index notices a change nobody told it about: no request, no turn, only
//! `stat` ticks.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use titi_atlas::live::{AtlasHandle, Options, Poll};

fn quick() -> Options {
    Options {
        debounce: Duration::from_millis(5),
        poll: Some(Poll {
            active: Duration::from_millis(20),
            idle: Duration::from_millis(20),
            active_for: Duration::from_secs(60),
            off_after: Duration::from_secs(120),
        }),
        ..Options::default()
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("never saw: {what}");
}

#[test]
fn a_file_created_deleted_and_edited_behind_the_workers_back_shows_up() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/gone.rs", "pub fn gone() {}\n");
    let handle = AtlasHandle::spawn(root, quick()).unwrap();
    let has = |path: &str| handle.snapshot().0.files.contains_key(path);
    assert!(has("src/gone.rs") && !has("src/new.rs"));
    // Let the watcher prime itself on the quiet tree, and let the filesystem's
    // clock tick past the index's own reads.
    std::thread::sleep(Duration::from_millis(1200));

    write(root, "src/new.rs", "pub fn fresh() {}\n");
    until("a created file", || has("src/new.rs"));

    fs::remove_file(root.join("src/gone.rs")).unwrap();
    until("a deleted file", || !has("src/gone.rs"));

    std::thread::sleep(Duration::from_millis(1200));
    write(
        root,
        "src/a.rs",
        "pub fn a() {}\npub fn edited_elsewhere() {}\n",
    );
    until("an edit of a file nobody named", || {
        handle
            .snapshot()
            .0
            .files
            .get("src/a.rs")
            .is_some_and(|record| record.exports.iter().any(|n| n == "edited_elsewhere"))
    });
}

#[test]
fn without_a_poll_nothing_moves_until_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    let handle = AtlasHandle::spawn(root, Options::default()).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    write(root, "src/new.rs", "pub fn fresh() {}\n");
    std::thread::sleep(Duration::from_millis(600));
    assert!(!handle.snapshot().0.files.contains_key("src/new.rs"));
    handle.request(titi_atlas::live::Request::Resync);
    assert!(handle.quiesce(Duration::from_secs(5)));
    assert!(handle.snapshot().0.files.contains_key("src/new.rs"));
}
