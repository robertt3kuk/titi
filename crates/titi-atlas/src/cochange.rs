//! Files that change together.
//!
//! The dependency graph knows what imports what. History knows something it
//! cannot: that `parser.rs` and `formatter.rs` are edited in the same commit
//! every time, though neither mentions the other, and that a migration, its
//! model and its fixture travel together. On replayed commits 58 % of the files
//! of a multi-file change had such a partner among the others, and together
//! with the import/use edges 77 % had some link to a sibling (see
//! `docs/research/atlas-v2`).
//!
//! The signal is read from `git log` once per `HEAD`, off the hot path: a
//! background thread with a commit cap and a time cap, published into the
//! index when it answers, never waited for. A repository with no history, a
//! shallow clone, a failing `git` or a slow one simply has no co-change edges.
//!
//! Weighting: each commit that touches 2 to 12 source files contributes to
//! every pair of them `0.5^(age / 50) / sqrt(files)` — recent commits count
//! more (a halving every 50 commits), and a commit that touched many files says
//! less about any two of them. Merges and bulk commits are ignored. Each file
//! keeps its 8 strongest partners.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Commits read, newest first.
const MAX_COMMITS: usize = 500;
/// All of the probe's git calls together.
const BUDGET: Duration = Duration::from_millis(2500);
/// A commit touching fewer or more source files says nothing about pairs.
const MIN_FILES: usize = 2;
const MAX_FILES: usize = 12;
/// A commit's weight halves every this many commits back.
const HALF_LIFE: f64 = 50.0;
/// Partners kept per file.
const KEEP: usize = 8;
/// Pairs lighter than this are noise from one old commit.
const MIN_WEIGHT: f32 = 0.03;

/// The strongest co-change partners of each file.
#[derive(Debug, Clone, Default)]
pub struct CoChange {
    partners: HashMap<String, Vec<(String, f32)>>,
    /// Commits that contributed at least one pair.
    pub commits: usize,
}

impl CoChange {
    /// Read a `git log --name-only --format=@@%H` listing, newest commit first.
    /// `is_source` says which paths count: a lockfile that every commit
    /// touches pairs with everything and means nothing.
    pub fn from_log(log: &str, is_source: impl Fn(&str) -> bool) -> Self {
        let mut pairs: HashMap<(String, String), f32> = HashMap::new();
        let mut commits = 0;
        for (age, chunk) in log.split("@@").skip(1).enumerate() {
            let mut files: Vec<&str> = chunk
                .lines()
                .skip(1)
                .map(str::trim)
                .filter(|path| !path.is_empty() && is_source(path))
                .collect();
            files.sort_unstable();
            files.dedup();
            if !(MIN_FILES..=MAX_FILES).contains(&files.len()) {
                continue;
            }
            commits += 1;
            let weight = (0.5f64.powf(age as f64 / HALF_LIFE) / (files.len() as f64).sqrt()) as f32;
            for (at, a) in files.iter().enumerate() {
                for b in &files[at + 1..] {
                    *pairs.entry(((*a).to_owned(), (*b).to_owned())).or_default() += weight;
                }
            }
        }
        let mut partners: HashMap<String, Vec<(String, f32)>> = HashMap::new();
        for ((a, b), weight) in pairs {
            if weight < MIN_WEIGHT {
                continue;
            }
            partners
                .entry(a.clone())
                .or_default()
                .push((b.clone(), weight));
            partners.entry(b).or_default().push((a, weight));
        }
        for list in partners.values_mut() {
            list.sort_by(|x, y| {
                y.1.partial_cmp(&x.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| x.0.cmp(&y.0))
            });
            list.truncate(KEEP);
        }
        Self { partners, commits }
    }

    /// The partners of `path`, strongest first.
    pub fn partners(&self, path: &str) -> &[(String, f32)] {
        self.partners.get(path).map_or(&[], Vec::as_slice)
    }

    /// Whether history said anything at all.
    pub fn is_empty(&self) -> bool {
        self.partners.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &Vec<(String, f32)>)> {
        self.partners.iter()
    }
}

/// Read `root`'s history: the commits touching it, paths relative to it. `None`
/// outside a repository, without commits, or when `git` does not answer in
/// time.
pub fn probe(root: &Path) -> Option<CoChange> {
    let deadline = Instant::now() + BUDGET;
    let count = format!("-n{MAX_COMMITS}");
    let log = run_git(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "log",
            &count,
            "--no-merges",
            "--name-only",
            "--relative",
            "--no-renames",
            "--format=@@%H",
        ],
        deadline,
    )?;
    let log = String::from_utf8_lossy(&log);
    let found = CoChange::from_log(&log, crate::lang::is_source_file);
    (!found.is_empty()).then_some(found)
}

/// What `HEAD` is, cheaply and without a process: the file `HEAD` and the
/// reflog's modification time, which moves on every commit, checkout and
/// reset. Two equal stamps are the same history for this purpose; `None` when
/// `root` is not in a git working tree.
pub fn head_stamp(root: &Path) -> Option<String> {
    let git_dir = git_dir(root)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reflog = std::fs::metadata(git_dir.join("logs/HEAD"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_nanos());
    Some(format!("{}@{reflog}", head.trim()))
}

/// The `.git` directory of the working tree containing `root`, following the
/// `gitdir:` file a linked worktree has.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let mut at = Some(root);
    while let Some(dir) = at {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let target = Path::new(target);
            return Some(if target.is_absolute() {
                target.to_path_buf()
            } else {
                dir.join(target)
            });
        }
        at = dir.parent();
    }
    None
}

fn run_git(root: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).ok().map(|_| out)
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok().flatten();
                return if status.success() { out } else { None };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
}

/// Keeps a [`crate::SharedAtlas`] supplied with the history of its root: one
/// probe per `HEAD`, run on a thread of its own and published when it answers.
///
/// `poll` is cheap (two small file reads) and never waits for a probe; one
/// that is already running makes the next poll a no-op, and the poll after it
/// notices if `HEAD` moved meanwhile.
#[derive(Default)]
pub(crate) struct Follower {
    last: Option<String>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Follower {
    pub(crate) fn poll(&mut self, root: &Path, atlas: &crate::SharedAtlas) {
        use std::sync::atomic::Ordering;
        let Some(stamp) = head_stamp(root) else {
            return;
        };
        if self.last.as_deref() == Some(stamp.as_str()) || self.running.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.last = Some(stamp);
        let root = root.to_path_buf();
        let atlas = atlas.clone();
        let running = std::sync::Arc::clone(&self.running);
        let spawned = std::thread::Builder::new()
            .name("titi-atlas-cochange".to_owned())
            .spawn(move || {
                if let Some(found) = probe(&root) {
                    atlas.set_cochange(found);
                }
                running.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.running.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CoChange;

    fn source(path: &str) -> bool {
        path.ends_with(".rs")
    }

    #[test]
    fn files_edited_together_are_partners_and_recent_pairs_weigh_more() {
        // newest first: a+b twice, then (long ago) a+c once
        let mut log = String::new();
        log.push_str("@@h1\n\nsrc/a.rs\nsrc/b.rs\n");
        log.push_str("@@h2\n\nsrc/a.rs\nsrc/b.rs\nCargo.lock\n");
        for n in 0..120 {
            log.push_str(&format!("@@x{n}\n\nsrc/solo{n}.rs\n"));
        }
        log.push_str("@@old\n\nsrc/a.rs\nsrc/c.rs\n");
        let found = CoChange::from_log(&log, source);

        let of_a = found.partners("src/a.rs");
        assert_eq!(of_a[0].0, "src/b.rs", "{of_a:?}");
        let c = of_a.iter().find(|(path, _)| path == "src/c.rs");
        assert!(
            c.is_none_or(|(_, w)| *w < of_a[0].1),
            "old pairs fade: {of_a:?}"
        );
        assert_eq!(found.partners("src/b.rs")[0].0, "src/a.rs", "symmetric");
        assert!(found.partners("Cargo.lock").is_empty(), "not a source file");
    }

    #[test]
    fn a_bulk_commit_and_a_lone_file_say_nothing() {
        let mut log = String::from("@@solo\n\nsrc/a.rs\n@@bulk\n\n");
        for n in 0..20 {
            log.push_str(&format!("src/f{n}.rs\n"));
        }
        let found = CoChange::from_log(&log, source);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(found.commits, 0);
    }
}
