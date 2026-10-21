//! Which files the prompt map marks `[RECENT]`.
//!
//! A file's modification time answers "when did this checkout write it",
//! which for a clone, a branch switch or a cherry-pick is the minute the
//! command ran, for every file at once. Inside a git repository the question
//! worth asking is "did someone work on this": the file has uncommitted
//! changes, or a commit made within [`WINDOW`] changed it. In a shallow clone
//! only the uncommitted changes count: its oldest commit is a boundary that
//! touches every file. Outside git there is no history, and the clock is the
//! only evidence there is.
//!
//! The answer is a set of index paths read once when an index is built, off
//! the parse threads' critical path (see [`Probe`]), and extended with every
//! file the session re-parses afterwards: a file changed after the index was
//! built is uncommitted work as of that change, and git does not have to be
//! asked again to know it.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// A commit this recent, by committer date, makes its files recent.
pub(crate) const WINDOW: Duration = Duration::from_secs(48 * 3600);

/// All of the git calls together get this long. A repository whose `status`
/// cannot answer in that time falls back to modification times rather than
/// holding the first index back.
const BUDGET: Duration = Duration::from_millis(1500);

/// Commits examined; a repository that lands more than this in two days is
/// busy enough that the newest ones say what is being worked on.
const MAX_COMMITS: &str = "--max-count=2000";

/// The fallback outside git: written within [`WINDOW`].
pub(crate) fn by_clock(mtime: SystemTime) -> bool {
    mtime.elapsed().ok().is_some_and(|age| age <= WINDOW)
}

/// What the index knows about recency for one root.
#[derive(Debug, Clone)]
pub(crate) struct Recency {
    root: PathBuf,
    /// `None`: not a git repository, or git could not answer in time.
    git: Option<HashSet<String>>,
}

impl Recency {
    pub(crate) fn is_for(&self, root: &Path) -> bool {
        self.root == root
    }

    /// Whether `path`, last written at `mtime`, is recent.
    pub(crate) fn is_recent(&self, path: &str, mtime: SystemTime) -> bool {
        match &self.git {
            Some(files) => files.contains(path),
            None => by_clock(mtime),
        }
    }

    /// Files this session changed after the set was read.
    pub(crate) fn note_changed(&mut self, paths: &[String]) {
        if let Some(files) = &mut self.git {
            files.extend(paths.iter().cloned());
        }
    }
}

/// The git questions for one root, running beside the parse.
pub(crate) struct Probe {
    root: PathBuf,
    handle: JoinHandle<Option<HashSet<String>>>,
}

impl Probe {
    pub(crate) fn start(root: &Path) -> Self {
        let owned = root.to_path_buf();
        let for_thread = owned.clone();
        Self {
            root: owned,
            handle: std::thread::spawn(move || read_git(&for_thread)),
        }
    }

    pub(crate) fn finish(self) -> Recency {
        Recency {
            root: self.root,
            // A panicked probe is a probe that did not answer.
            git: self.handle.join().unwrap_or(None),
        }
    }
}

/// The recent paths under `root`, relative to it. `None` when `root` is not
/// inside a repository or `git status` did not answer in time.
fn read_git(root: &Path) -> Option<HashSet<String>> {
    let deadline = Instant::now() + BUDGET;
    let prefix = run_git(root, &["rev-parse", "--show-prefix"], deadline)?;
    let prefix = String::from_utf8(prefix).ok()?;
    let prefix = prefix.trim_end_matches(['\n', '\r']);

    // Without renames each entry is `XY path\0`: the two status letters, a
    // space, the path from the repository's top level.
    let status = run_git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
        deadline,
    )?;
    let mut files: HashSet<String> = HashSet::new();
    for entry in status.split(|byte| *byte == 0) {
        let Some(path) = entry
            .get(3..)
            .and_then(|path| std::str::from_utf8(path).ok())
        else {
            continue;
        };
        if let Some(inside) = path.strip_prefix(prefix) {
            files.insert(inside.to_owned());
        }
    }

    // A shallow clone's oldest commit is its boundary: git reports every file
    // as added in it, so a boundary inside the window would mark the whole
    // tree. The history that says what was worked on is not there to ask, and
    // only what the status found is known to be recent. (`git` older than 2.15
    // does not know the flag; its answer is not `true`, which keeps the log.)
    let shallow = run_git(root, &["rev-parse", "--is-shallow-repository"], deadline)
        .is_some_and(|answer| answer.trim_ascii() == b"true");
    if shallow {
        return Some(files);
    }

    // `--relative` limits the log to `root` and strips the prefix itself.
    // A log that fails (no commits yet) or runs out of time leaves the status
    // half, which marks less, never more.
    let since = format!("--since={}.seconds", WINDOW.as_secs());
    if let Some(log) = run_git(
        root,
        &[
            "log",
            &since,
            MAX_COMMITS,
            "--name-only",
            "--relative",
            "--no-renames",
            "-z",
            "--format=",
        ],
        deadline,
    ) {
        for path in log.split(|byte| *byte == 0) {
            if let Ok(path) = std::str::from_utf8(path)
                && !path.is_empty()
            {
                files.insert(path.to_owned());
            }
        }
    }
    Some(files)
}

/// Stdout of `git -C root args…` on success, `None` on any failure or once
/// `deadline` passes, in which case the process is killed.
fn run_git(root: &Path, args: &[&str], deadline: Instant) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        // `status` would otherwise try to refresh the index, which takes a
        // lock a concurrent git command of the user's may be holding.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Drained on its own thread: a child that fills the pipe would block on
    // its write while this one waits for it to exit.
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
