//! Noticing that the tree moved, with `stat` and nothing else.
//!
//! The worker folds in what it is told and walks the tree when a turn starts.
//! Between those, a file edited in another window, a `git checkout` in another
//! terminal or a generator is invisible, and a reader that arrives mid-turn
//! gets a graph older than the disk. No watcher dependency is needed to close
//! most of that gap: a tick of three cheap questions is.
//!
//! * **Directories.** A directory's mtime moves when an entry is created,
//!   deleted or renamed in it, so every directory that holds an indexed file
//!   (and each of its ancestors) is stat'ed, and a moved one says the tree
//!   changed. O(directories): a few hundred on a repository of thousands of
//!   files.
//! * **The hot set.** Editing a file's *content* does not move its
//!   directory's mtime, so the files the session has been working in are
//!   stat'ed against their records every tick.
//! * **`.git/HEAD` and `.git/index`.** A branch switch or a checkout moves
//!   one of them.
//!
//! Every [`Self::FULL_EVERY`]th tick also answers "changed" outright, which
//! makes the worker walk: that is the backstop for what the three questions
//! cannot see (a content edit of a file outside the hot set, a filesystem
//! whose directory mtimes do not move).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::Atlas;

/// Files re-stat'ed on every tick: the ones the session named last.
const HOT: usize = 64;

#[derive(Debug, Default)]
pub(crate) struct Watcher {
    dirs: HashMap<String, SystemTime>,
    git: [Option<SystemTime>; 2],
    hot: VecDeque<String>,
    ticks: u64,
}

impl Watcher {
    /// Every this many ticks the answer is "walk", whatever the stats said.
    pub(crate) const FULL_EVERY: u64 = 10;

    /// The files a caller named, most recent last; the oldest fall off.
    pub(crate) fn note(&mut self, paths: &[String]) {
        for path in paths {
            self.hot.retain(|known| known != path);
            self.hot.push_back(path.clone());
        }
        while self.hot.len() > HOT {
            self.hot.pop_front();
        }
    }

    /// Whether anything the three questions can see moved since the last call.
    /// The first call only records what is there.
    pub(crate) fn changed(&mut self, root: &Path, atlas: &Atlas) -> bool {
        self.ticks += 1;
        let primed = self.ticks > 1;
        let mut moved = false;

        let git = [".git/HEAD", ".git/index"].map(|rel| mtime(&root.join(rel)));
        if primed && git != self.git {
            moved = true;
        }
        self.git = git;

        let mut seen: Vec<&str> = Vec::new();
        for path in atlas.files.keys() {
            let mut dir = path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                dir = parent;
                seen.push(dir);
            }
        }
        seen.push("");
        seen.sort_unstable();
        seen.dedup();
        for dir in &seen {
            let now = mtime(&root.join(dir));
            match (self.dirs.get(*dir), now) {
                (Some(before), Some(now)) if *before == now => {}
                (Some(_), Some(now)) => {
                    moved |= primed;
                    self.dirs.insert((*dir).to_owned(), now);
                }
                (None, Some(now)) => {
                    self.dirs.insert((*dir).to_owned(), now);
                }
                (Some(_), None) => {
                    moved |= primed;
                    self.dirs.remove(*dir);
                }
                (None, None) => {}
            }
        }
        // A directory the index no longer holds a file in is not watched.
        self.dirs
            .retain(|dir, _| seen.binary_search(&dir.as_str()).is_ok());

        for path in &self.hot {
            let now = crate::scan::stat(root, path);
            match (atlas.files.get(path), now) {
                (Some(record), Some(file)) => {
                    moved |= record.size != file.size || record.mtime != file.mtime;
                }
                (Some(_), None) | (None, Some(_)) => moved = true,
                (None, None) => {}
            }
        }

        moved || (primed && self.ticks.is_multiple_of(Self::FULL_EVERY))
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::Watcher;
    use crate::Atlas;
    use std::fs;

    fn indexed(root: &std::path::Path) -> Atlas {
        Atlas::index(root).unwrap()
    }

    /// Filesystem timestamps can be coarse; give each change its own tick.
    fn settle() {
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    #[test]
    fn a_quiet_tree_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "pub fn a() {}\n").unwrap();
        let atlas = indexed(dir.path());
        let mut watcher = Watcher::default();
        for tick in 1..Watcher::FULL_EVERY {
            assert!(!watcher.changed(dir.path(), &atlas), "tick {tick}");
        }
        assert!(
            watcher.changed(dir.path(), &atlas),
            "the tenth tick asks for a walk"
        );
    }

    #[test]
    fn a_created_file_a_deleted_file_and_an_edit_of_a_hot_file_are_seen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn b() {}\n").unwrap();
        let atlas = indexed(root);
        let mut watcher = Watcher::default();
        watcher.note(&["src/a.rs".to_owned()]);
        assert!(!watcher.changed(root, &atlas));

        settle();
        fs::write(root.join("src/new.rs"), "pub fn n() {}\n").unwrap();
        assert!(
            watcher.changed(root, &atlas),
            "a new file moves its directory"
        );
        assert!(!watcher.changed(root, &atlas), "and is seen once");

        settle();
        fs::remove_file(root.join("src/b.rs")).unwrap();
        assert!(watcher.changed(root, &atlas), "a deletion moves it too");
        assert!(!watcher.changed(root, &atlas));

        settle();
        fs::write(root.join("src/a.rs"), "pub fn a() {}\npub fn a2() {}\n").unwrap();
        assert!(
            watcher.changed(root, &atlas),
            "an edit moves no directory; the hot set sees it"
        );
    }
}
