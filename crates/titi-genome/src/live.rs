//! The background indexer: one thread applies the batches no tool named,
//! readers hold snapshots and are told how far behind it is.
//!
//! A turn used to pay for the walk before every prompt: `genome_map` called
//! `refresh` inside the turn and waited for it, so the parse of every file the
//! agent had touched stood between the user's question and the model's first
//! token. Moving that work to another thread is not enough on its own — a
//! reader that then blocks on the parse has moved the cost, not removed it —
//! so the shape here is deliberately two-sided:
//!
//! * the worker applies batches — everything no tool named — and publishes
//!   them through [`SharedGenome`]'s pointer swap, the same publish point the
//!   tool loop folds a write into. A reader never waits for either.
//! * the reader is told how far behind it is ([`GenomeHandle::pending`]) and
//!   may ask the worker to finish what it has ([`GenomeHandle::quiesce`]).
//!
//! **The channel coalesces.** The worker takes one request, then keeps taking
//! requests until [`Options::debounce`] passes with nothing arriving, and
//! applies the whole window as one update. A burst of forty saves of one file
//! is one entry in a `BTreeSet` and one parse, not forty.
//!
//! **The queue is bounded** twice, in two different units, because a request
//! is cheap and the work it names is not. The channel itself is unbounded —
//! dropping an event would be a silently lost change — but it is drained the
//! moment the worker wakes, so its depth is only what a producer can emit
//! between two drains. What the worker *accumulates* is capped by
//! [`Options::batch_cap`]: past it the path set is thrown away and replaced by
//! a tree walk, which covers every path named and costs one pass, and the
//! urgent list keeps its first `batch_cap` entries in the caller's order so a
//! producer cannot grow it without bound either.
//!
//! The concurrency is `std::thread` and `std::sync::mpsc`, not tokio: this
//! crate is tokio-free, the work is CPU-bound, and the thread is long-lived,
//! which is exactly the case `spawn_blocking` — for a task that ends — is
//! wrong for.
//!
//! Phase 3, the filesystem watcher, is a `notify` dependency this crate does
//! not have. When it lands it is one more producer on [`Request::Changed`];
//! nothing here assumes it is the engine sending.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{Genome, RefreshStats, SharedGenome};

/// How the worker coalesces, and how much it will hold.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// How long the worker keeps collecting after the last request before it
    /// applies the window. Long enough to swallow a save burst, short enough
    /// that a producer with no one to ask for freshness still sees it.
    pub debounce: Duration,
    /// The most paths one window accumulates before it becomes a tree walk,
    /// and the most urgent paths it keeps in the caller's order.
    pub batch_cap: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(120),
            batch_cap: 1024,
        }
    }
}

/// A change the worker should fold in, or a question about what it has.
pub enum Request {
    /// These paths changed; nothing else about the tree is known. Deleted and
    /// renamed paths belong here too — a path that is no longer a file is
    /// dropped by the same update.
    Changed(Vec<String>),
    /// The same, but the caller is working in these files and wants them
    /// parsed before the rest of the window. Order is the caller's.
    Priority(Vec<String>),
    /// Walk the tree and fold in whatever moved, named or not. This is what
    /// catches a file no tool named: a formatter, a `git checkout`, another
    /// process.
    Resync,
    /// Apply the window now and answer with the generation it published.
    ///
    /// This is what ends the debounce early: a caller that sends it has said
    /// it has nothing more to send, and waiting out the window would answer
    /// it with a graph missing the very changes it just handed over.
    Quiesce(Sender<u64>),
    /// Apply nothing further and leave the loop.
    Stop,
}

/// A handle to the background indexer.
///
/// Cloneable and cheap: every clone shares the same worker, which stops when
/// the last one drops. Reads never block on the worker.
pub struct GenomeHandle {
    genome: SharedGenome,
    tx: Sender<Request>,
    pending: Arc<AtomicUsize>,
    worker: Arc<Worker>,
}

/// The worker's end of the handle, so that only the last clone stops it.
struct Worker {
    tx: Sender<Request>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // A stopped worker has left the loop, so the send fails and the join
        // is what remains: wait for the batch it was in the middle of, rather
        // than leave a thread writing to a graph nobody reads.
        let _ = self.tx.send(Request::Stop);
        let joined = self.join.lock().ok().and_then(|mut join| join.take());
        if let Some(join) = joined {
            let _ = join.join();
        }
    }
}

impl Clone for GenomeHandle {
    fn clone(&self) -> Self {
        Self {
            genome: self.genome.clone(),
            tx: self.tx.clone(),
            pending: Arc::clone(&self.pending),
            worker: Arc::clone(&self.worker),
        }
    }
}

impl GenomeHandle {
    /// Index `root` once, then start the worker behind it.
    ///
    /// The first index is **synchronous, on the caller's thread** — a cold
    /// start is a full walk and parse, and this is where it happens. That is
    /// deliberate: a handle is then never without a graph, so `pending` can
    /// only ever mean "changes have not landed yet", never "there is nothing
    /// to read", and a caller that ignores the staleness signal renders a
    /// graph one batch old instead of an empty one.
    pub fn spawn(root: impl AsRef<Path>, options: Options) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let genome = SharedGenome::index(&root)?;
        let (tx, rx) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let worker_genome = genome.clone();
        let worker_pending = Arc::clone(&pending);
        let join = thread::Builder::new()
            .name("titi-genome-index".to_owned())
            .spawn(move || run(rx, root, worker_genome, worker_pending, options))?;
        Ok(Self {
            genome,
            tx: tx.clone(),
            pending,
            worker: Arc::new(Worker {
                tx,
                join: Mutex::new(Some(join)),
            }),
        })
    }

    /// A consistent graph and the generation it was published at.
    ///
    /// Never waits for the worker: it is the graph as of the last publish,
    /// complete, possibly one batch behind — which is what [`Self::pending`]
    /// is for.
    pub fn snapshot(&self) -> (Arc<Genome>, u64) {
        (self.genome.snapshot(), self.genome.generation())
    }

    /// Read once, under one snapshot.
    pub fn read<R>(&self, f: impl FnOnce(&Genome) -> R) -> R {
        self.genome.read(f)
    }

    /// Path-level work items the worker has accepted and not yet folded in.
    ///
    /// Named paths count once each, and so does a queued tree walk, whose
    /// dirty set is not known until it runs. It is a count of *work*, not of
    /// changed files: a batch of one path that turns out to be unchanged still
    /// reads `1` until it lands. Zero means the graph a reader is handed is
    /// the one every accepted batch has reached, with one deliberate
    /// exception: a batch that **failed** to apply keeps its count standing,
    /// so a frozen graph is named rather than passed off as current.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// How many times the graph has been published. Compare two of these to
    /// learn whether anything landed; they never go backwards.
    pub fn generation(&self) -> u64 {
        self.genome.generation()
    }

    /// What the last applied batch did, or `None` before the first one.
    pub fn last_stats(&self) -> Option<RefreshStats> {
        self.genome.last_stats()
    }

    /// Send a request. `false` when the worker is gone, which only happens
    /// once every clone of this handle has been dropped.
    pub fn request(&self, request: Request) -> bool {
        self.tx.send(request).is_ok()
    }

    /// Wait until the worker has folded in everything sent before this call,
    /// or `timeout` passes. `true` means caught up.
    ///
    /// The answer is sent after the window that contains this request has been
    /// applied, because the worker applies in the order it receives. A `true`
    /// therefore certifies a graph that includes every request the caller made
    /// before this one — which is why a caller that cannot wait has to *say*
    /// so rather than read [`Self::snapshot`] and hope.
    pub fn quiesce(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Request::Quiesce(tx)).is_err() {
            // No worker: every update it will ever make has been made.
            return true;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    /// The publish point itself, for a writer that must be current by
    /// construction.
    ///
    /// This is the tool loop's path: a tool wrote a file, and the turn's own
    /// prompt has to see it, so the fold goes in synchronously on the calling
    /// thread instead of into the worker's window. Writers are serialised by
    /// the publish point, so this and a batch cannot lose each other.
    pub fn shared(&self) -> &SharedGenome {
        &self.genome
    }
}

/// What the worker has accumulated for the batch it is about to apply.
#[derive(Default)]
struct Queue {
    /// Every path named since the last apply, deduplicated: a burst of saves
    /// of one file is one entry here and one `stat` when it is applied.
    paths: BTreeSet<String>,
    /// The paths the caller marked urgent, in the order it named them. Fed
    /// only by `Priority`, never reset by the debounce.
    urgent: Vec<String>,
    /// A tree walk is queued. It covers every path above, so those are not
    /// applied separately.
    walk: bool,
    /// A caller has said it is done accumulating, or the handle is dropping.
    hurry: bool,
}

impl Queue {
    /// See [`GenomeHandle::pending`] for what this counts and why a walk is
    /// one.
    fn items(&self) -> usize {
        self.paths.len() + usize::from(self.walk)
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.walk
    }

    /// The paths in the order [`Genome::apply_changes`] should see them: the
    /// urgent ones first, in the caller's order, then the rest sorted.
    fn ordered(&self) -> Vec<String> {
        let mut ordered: Vec<String> = self.urgent.clone();
        let urgent: std::collections::HashSet<&str> =
            self.urgent.iter().map(String::as_str).collect();
        ordered.extend(
            self.paths
                .iter()
                .filter(|path| !urgent.contains(path.as_str()))
                .cloned(),
        );
        ordered
    }
}

/// Take one request into `queue`, and update the reader-visible count.
fn accumulate(
    request: Request,
    queue: &mut Queue,
    waiters: &mut Vec<Sender<u64>>,
    stop: &mut bool,
    options: Options,
    pending: &AtomicUsize,
) {
    match request {
        Request::Changed(paths) => queue.paths.extend(paths),
        Request::Priority(paths) => {
            queue.urgent.extend(paths.iter().cloned());
            queue.paths.extend(paths);
        }
        Request::Resync => queue.walk = true,
        Request::Quiesce(waiter) => {
            waiters.push(waiter);
            queue.hurry = true;
        }
        Request::Stop => {
            *stop = true;
            queue.hurry = true;
        }
    }
    // Past the cap the answer is not a bigger batch but a cheaper one: a walk
    // covers every path named here and costs one pass over the tree, where
    // applying them path by path costs a `stat` each. The urgent list keeps
    // its first `batch_cap` entries — the caller's order is preserved and only
    // the parse *order* of the tail is lost, never the work.
    if queue.paths.len() > options.batch_cap {
        queue.paths.clear();
        queue.walk = true;
    }
    if queue.urgent.len() > options.batch_cap {
        queue.urgent.truncate(options.batch_cap);
    }
    pending.store(queue.items(), Ordering::Release);
}

/// The worker loop: window, apply, answer, repeat.
fn run(
    rx: Receiver<Request>,
    root: PathBuf,
    genome: SharedGenome,
    pending: Arc<AtomicUsize>,
    options: Options,
) {
    let mut queue = Queue::default();
    loop {
        let Ok(first) = rx.recv() else {
            return;
        };
        let mut waiters = Vec::new();
        let mut stop = false;
        accumulate(
            first,
            &mut queue,
            &mut waiters,
            &mut stop,
            options,
            &pending,
        );
        while !queue.hurry {
            match rx.recv_timeout(options.debounce) {
                Ok(request) => accumulate(
                    request,
                    &mut queue,
                    &mut waiters,
                    &mut stop,
                    options,
                    &pending,
                ),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    stop = true;
                    break;
                }
            }
        }
        // A batch that could not be folded in leaves its count standing: the
        // graph a reader gets is the last one that worked, and the count is
        // the only thing that says so — an error here publishes nothing, and
        // the generation still names the graph the reader is looking at. A
        // root that has gone away is the ordinary case. The next batch
        // replaces the count, and a successful one clears it.
        let applied = queue.is_empty() || apply(&genome, &root, &queue).is_ok();
        if applied {
            pending.store(0, Ordering::Release);
        }
        let generation = genome.generation();
        for waiter in waiters {
            let _ = waiter.send(generation);
        }
        queue = Queue::default();
        if stop {
            return;
        }
    }
}

/// One window as one update: a walk when anything asked for one, otherwise the
/// named paths.
fn apply(genome: &SharedGenome, root: &Path, queue: &Queue) -> io::Result<RefreshStats> {
    if queue.walk {
        genome.refresh_urgent(root, &queue.urgent)
    } else {
        genome.apply_changes(&queue.ordered())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::{GenomeHandle, Options, Request};

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A debounce no test will ever wait out, so only an explicit `Quiesce`
    /// ends the window. Tests that would otherwise race a timer become exact.
    fn patient() -> Options {
        Options {
            debounce: Duration::from_secs(600),
            ..Options::default()
        }
    }

    /// A tree whose first walk takes far longer than a test's deadline.
    ///
    /// The slow-batch tests need work that outlasts a scheduling quantum, not
    /// merely a non-empty queue: a `recv_timeout` that expires yields the
    /// thread, and the worker can finish a two-file batch inside that yield.
    /// These files are big enough that no build finishes them in 1 ms, which
    /// is what makes "cannot catch up" an outcome rather than a coin toss.
    fn heavy_tree(root: &Path, files: usize) {
        for index in 0..files {
            let mut body = String::new();
            for line in 0..120 {
                body.push_str(&format!("    let value_{line} = {line}_u64;\n"));
            }
            write(
                root,
                &format!("src/heavy_{index}.rs"),
                &format!("pub fn heavy_{index}() {{\n{body}}}\n"),
            );
        }
    }

    /// A burst of named paths is one update: one parse pass, one publish.
    #[test]
    fn a_burst_of_changes_is_one_apply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(root, patient()).unwrap();
        let published = live.generation();

        let mut burst = Vec::new();
        for index in 0..8 {
            let rel = format!("src/new_{index}.rs");
            write(root, &rel, &format!("pub fn new_{index}() {{}}\n"));
            burst.push(rel);
        }
        for path in &burst {
            assert!(live.request(Request::Changed(vec![path.clone()])));
        }
        assert!(live.quiesce(Duration::from_secs(5)), "the burst landed");

        assert_eq!(live.generation(), published + 1, "one window, one publish");
        let stats = live.last_stats().expect("the batch was applied");
        assert_eq!(
            stats.parsed,
            burst.len(),
            "every path in the burst: {stats:?}"
        );
        let (snapshot, generation) = live.snapshot();
        assert_eq!(generation, published + 1);
        assert_eq!(snapshot.files.len(), burst.len() + 1);
        assert!(snapshot.files.contains_key("src/new_7.rs"));
    }

    /// A window that closes on its own, with nothing sent after it, is still
    /// one apply — the debounce is the timer, the caller does not have to ask.
    #[test]
    fn the_debounce_window_closes_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(
            root,
            Options {
                debounce: Duration::from_millis(50),
                ..Options::default()
            },
        )
        .unwrap();
        let published = live.generation();

        write(root, "src/late.rs", "pub fn late() {}\n");
        assert!(live.request(Request::Changed(vec!["src/late.rs".to_owned()])));
        // Wait for the *publish*, not for `pending`: between the send and the
        // worker's first `recv` the count is still zero, so polling it can
        // read "idle" before the request was ever picked up.
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.generation() == published && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(live.generation(), published + 1, "one window, one publish");
        assert!(live.snapshot().0.files.contains_key("src/late.rs"));
        assert_eq!(live.pending(), 0, "and it is idle again");
    }

    /// `quiesce` answers once the window it was sent in has landed, and is
    /// cheap and immediate when there is nothing to wait for.
    #[test]
    fn quiesce_returns_when_the_worker_is_idle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(root, patient()).unwrap();

        let started = Instant::now();
        assert!(live.quiesce(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no batch to wait for"
        );
        assert_eq!(live.pending(), 0);

        write(root, "src/one.rs", "pub fn one() {}\n");
        assert!(live.request(Request::Changed(vec!["src/one.rs".to_owned()])));
        assert!(live.quiesce(Duration::from_secs(5)));
        assert_eq!(live.pending(), 0, "idle again once it answered");
        assert!(live.snapshot().0.files.contains_key("src/one.rs"));
    }

    /// While a window is open — the batch accepted, the parse not started —
    /// a reader sees the previous graph, whole, and is told how much is
    /// outstanding. This is the phase-0 invariant with a writer that is not
    /// the reader's own call.
    #[test]
    fn a_query_during_an_open_window_sees_the_previous_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(root, patient()).unwrap();

        write(root, "src/late.rs", "pub fn seed() { late(); }\n");
        assert!(live.request(Request::Changed(vec!["src/late.rs".to_owned()])));
        // Give the worker time to have taken the request and be sitting in the
        // window. It cannot apply: only a `Quiesce` ends a 600 s debounce.
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.pending() == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(live.pending(), 1, "the batch is open and named");

        let (before, generation) = live.snapshot();
        assert!(
            !before.files.contains_key("src/late.rs"),
            "the window is open: the old graph is what a reader has"
        );
        assert_eq!(generation, live.generation());

        assert!(live.quiesce(Duration::from_secs(5)));
        let (after, _) = live.snapshot();
        assert!(after.files.contains_key("src/late.rs"));
    }

    /// A reader sampling across a writer's batches never sees a half-built
    /// graph, and the writer that is not the reader's own call does not block
    /// it: every sample is a complete graph.
    #[test]
    fn a_reader_never_sees_a_half_built_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(root, patient()).unwrap();

        let done = std::sync::atomic::AtomicBool::new(false);
        let done = &done;
        let seen = std::thread::scope(|scope| {
            let writer = {
                let live = live.clone();
                let root = root.to_path_buf();
                scope.spawn(move || {
                    for round in 0..40 {
                        let rel = "src/flicker.rs";
                        if round % 2 == 0 {
                            write(&root, rel, "pub fn flicker() { seed(); }\n");
                        } else {
                            fs::remove_file(root.join(rel)).unwrap();
                        }
                        assert!(live.request(Request::Changed(vec![rel.to_owned()])));
                        assert!(live.quiesce(Duration::from_secs(5)));
                    }
                    done.store(true, std::sync::atomic::Ordering::Release);
                })
            };
            let reader = {
                let live = live.clone();
                scope.spawn(move || {
                    let mut lengths = std::collections::HashSet::new();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !done.load(std::sync::atomic::Ordering::Acquire)
                        && Instant::now() < deadline
                    {
                        let (snapshot, _) = live.snapshot();
                        let files = snapshot.files.len();
                        assert_eq!(files, snapshot.ranks.len(), "torn graph");
                        assert_eq!(files, snapshot.dependents.len(), "torn graph");
                        assert!(
                            snapshot
                                .ranks
                                .keys()
                                .all(|path| snapshot.files.contains_key(path)),
                            "a rank without a file"
                        );
                        lengths.insert(files);
                    }
                    lengths
                })
            };
            writer.join().unwrap();
            reader.join().unwrap()
        });

        assert!(
            seen.len() > 1,
            "the reader only ever saw {seen:?}; the test proved nothing"
        );
    }

    /// A `Quiesce` that cannot be answered in time is a `false`, which is what
    /// lets a caller tell "caught up" from "still behind" instead of reading
    /// whatever is published.
    #[test]
    fn quiesce_gives_up_rather_than_lying() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        // Spawned over the small tree, then handed a big one, so the batch
        // under test is the slow walk and not the startup index.
        let live = GenomeHandle::spawn(root, patient()).unwrap();
        heavy_tree(root, 150);

        assert!(live.request(Request::Resync));
        assert!(
            !live.quiesce(Duration::from_millis(1)),
            "a walk this size cannot land in a millisecond"
        );
        assert!(live.pending() > 0, "and the backlog is visible");
        assert!(
            live.quiesce(Duration::from_secs(30)),
            "and lands on request"
        );
        assert_eq!(live.pending(), 0);
        assert!(live.snapshot().0.files.contains_key("src/heavy_149.rs"));
    }

    /// A walk supersedes the paths named with it, and a batch over the cap
    /// becomes a walk instead of an unbounded set of `stat`s.
    #[test]
    fn a_walk_covers_the_paths_named_with_it_and_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(
            root,
            Options {
                debounce: Duration::from_secs(600),
                batch_cap: 4,
            },
        )
        .unwrap();

        // Written but never named: only a walk can see it, and `Changed` with
        // more paths than the cap asks for one.
        write(root, "src/unnamed.rs", "pub fn unnamed() {}\n");
        for index in 0..6 {
            assert!(live.request(Request::Changed(vec![format!("src/never_{index}.rs")])));
        }
        assert!(live.quiesce(Duration::from_secs(5)));
        let (snapshot, _) = live.snapshot();
        assert!(
            snapshot.files.contains_key("src/unnamed.rs"),
            "the cap turned the batch into a walk: {:?}",
            snapshot.files.keys().collect::<Vec<_>>()
        );
        let stats = live.last_stats().unwrap();
        assert!(stats.walked, "it was a walk: {stats:?}");
    }
}
