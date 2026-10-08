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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{Genome, RefreshStats, SharedGenome};

/// How the worker coalesces, how much it will hold, and how a test can hold
/// it.
#[derive(Clone)]
pub struct Options {
    /// How long the worker keeps collecting after the last request before it
    /// applies the window. Long enough to swallow a save burst, short enough
    /// that a producer with no one to ask for freshness still sees it.
    pub debounce: Duration,
    /// The most paths one window accumulates before it becomes a tree walk,
    /// and the most urgent paths it keeps in the caller's order.
    pub batch_cap: usize,
    /// Called by the worker once a window is over — applied or failed — and
    /// before it waits for the next request. `None` everywhere but a test.
    ///
    /// It exists for one state, the only one a reader must never mistake for
    /// "nothing outstanding" and the only one that cannot be observed without
    /// it: a request the handle has *sent* and the worker has not yet
    /// *received*. That state lasts microseconds, so a test that raced it
    /// would prove nothing; held open here it is exact. A hook inside a window
    /// would not do, because the state under test is the gap after one.
    pub hold: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("debounce", &self.debounce)
            .field("batch_cap", &self.batch_cap)
            .field("hold", &self.hold.is_some())
            .finish()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(120),
            batch_cap: 1024,
            hold: None,
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
    progress: Arc<Progress>,
    worker: Arc<Worker>,
}

/// How far the worker is behind what the handle has been asked to do.
///
/// The item count of the window the worker happens to be holding cannot answer
/// `pending` on its own, and the gap it leaves is not theoretical: a request
/// the caller has sent and the worker has not yet *received* is invisible to
/// it, so a turn that gives up waiting on its own resync — sent while the
/// worker was finishing a previous batch — can read "nothing outstanding" and
/// render a graph that does not contain the changes it just handed over. That
/// is the silent stale map the contract forbids.
///
/// Two monotonic counters close it. `sent` is bumped by the handle *before*
/// the send, so a request in the channel is counted from the instant it
/// exists; `applied` is set by the worker from its own receive count after a
/// window it folded in; `sent > applied` is exactly "there is work in the
/// channel or on the worker's hands". `items` is the window's path count, for
/// the number the header shows.
#[derive(Default)]
struct Progress {
    sent: AtomicU64,
    applied: AtomicU64,
    items: AtomicUsize,
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
            progress: Arc::clone(&self.progress),
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
        let progress = Arc::new(Progress::default());
        let worker_genome = genome.clone();
        let worker_progress = Arc::clone(&progress);
        let join = thread::Builder::new()
            .name("titi-genome-index".to_owned())
            .spawn(move || run(rx, root, worker_genome, worker_progress, options))?;
        Ok(Self {
            genome,
            tx: tx.clone(),
            progress,
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

    /// Path-level work items that have been asked for and not yet folded in.
    ///
    /// Named paths count once each, and so does a queued tree walk, whose
    /// dirty set is not known until it runs. It is a count of *work*, not of
    /// changed files: a batch of one path that turns out to be unchanged still
    /// reads `1` until it lands.
    ///
    /// Zero means the graph a reader is handed contains everything this handle
    /// has been asked for — including the requests still in the channel, which
    /// is the state a turn's own resync is in when it gives up waiting. Two
    /// deliberate exceptions, both in the safe direction: a window whose apply
    /// **failed** advances nothing and keeps its work, so the graph is named as
    /// behind until a later window folds that work in; and the count can lag a
    /// publish by the instruction it takes the worker to store it, which
    /// over-reports and never hides.
    pub fn pending(&self) -> usize {
        if self.progress.sent.load(Ordering::Acquire)
            == self.progress.applied.load(Ordering::Acquire)
        {
            return 0;
        }
        // `max(1)`: work that is outstanding is worth naming even when the
        // window it belongs to has no paths yet — a walk not yet run, or a
        // window the worker has not opened.
        self.progress.items.load(Ordering::Acquire).max(1)
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
        // Counted *before* the send, because a request that is in the channel
        // and not yet received is precisely what the count has to cover.
        self.progress.sent.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(request).is_ok() {
            return true;
        }
        // Nobody will ever take it, so leaving the deficit standing would
        // report a backlog for the rest of the process's life.
        self.progress.sent.fetch_sub(1, Ordering::AcqRel);
        false
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
        // Through `request`, so the wait itself is counted: while this request
        // is unread the count says work is outstanding, which is exactly what
        // a caller that timed out must not be told otherwise about.
        if !self.request(Request::Quiesce(tx)) {
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
    options: &Options,
    progress: &Progress,
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
    progress.items.store(queue.items(), Ordering::Release);
}

/// The worker loop: window, apply, answer, repeat.
fn run(
    rx: Receiver<Request>,
    root: PathBuf,
    genome: SharedGenome,
    progress: Arc<Progress>,
    options: Options,
) {
    let mut queue = Queue::default();
    // Requests this worker has taken out of the channel. It is the worker's
    // own count, so it is exact at the moment it is read: nothing else bumps
    // it, and the worker is single-threaded.
    let mut received: u64 = 0;
    loop {
        let Ok(first) = rx.recv() else {
            return;
        };
        received += 1;
        let mut waiters = Vec::new();
        let mut stop = false;
        accumulate(
            first,
            &mut queue,
            &mut waiters,
            &mut stop,
            &options,
            &progress,
        );
        while !queue.hurry {
            match rx.recv_timeout(options.debounce) {
                Ok(request) => {
                    received += 1;
                    accumulate(
                        request,
                        &mut queue,
                        &mut waiters,
                        &mut stop,
                        &options,
                        &progress,
                    );
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    stop = true;
                    break;
                }
            }
        }
        // A window's work is folded in or kept, never dropped. `applied`
        // advances only for a window that folded everything it was holding,
        // which is why a failed one keeps its queue: the next window applies
        // that work together with whatever arrived since, so a later success
        // really has covered it and `applied = received` is the truth. A
        // permanently failing root therefore names the backlog for as long as
        // it lasts and does not spin — the worker blocks in `recv` until the
        // next request and retries then, rather than looping on the failure.
        //
        // A window with nothing in it can only be reached when nothing is
        // outstanding, because a failed window keeps its queue; it clears the
        // count because there is genuinely nothing left to clear.
        let folded = queue.is_empty() || apply(&genome, &root, &queue).is_ok();
        if folded {
            progress.applied.store(received, Ordering::Release);
            progress.items.store(0, Ordering::Release);
            let generation = genome.generation();
            for waiter in waiters {
                let _ = waiter.send(generation);
            }
            queue = Queue::default();
        } else {
            // Nobody waiting on this window is told it landed: dropping the
            // senders ends each `quiesce` with `Disconnected`, which reads as
            // "not caught up" — the truth, where a generation would be a claim
            // the worker cannot stand behind. Dropped here rather than at the
            // end of the iteration so the answer does not wait on anything
            // else this loop does. The window flag is cleared so the kept work
            // accumulates afresh instead of applying on the first request the
            // next window receives.
            drop(waiters);
            queue.hurry = false;
        }
        if stop {
            return;
        }
        if let Some(hold) = &options.hold {
            hold();
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{GenomeHandle, Options, Request};

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A gate the worker blocks on at the end of every window.
    ///
    /// It is the only way to observe the state the `sent`/`applied` pair
    /// exists for — a request in the channel the worker has not received —
    /// without racing a window that closes in microseconds. A spin, not a
    /// condvar: it is held for a few assertions in one test, and a lock here
    /// would be state with nothing to protect.
    #[derive(Clone, Default)]
    struct Gate {
        open: Arc<AtomicBool>,
    }

    impl Gate {
        /// Blocks until [`Self::release`] is called; returns immediately after.
        fn hold(&self) {
            while !self.open.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }

        fn release(&self) {
            self.open.store(true, Ordering::Release);
        }
    }

    /// Releases the gate when the test ends, assertion or panic.
    ///
    /// Without it a failed assertion unwinds past the release and the handle's
    /// `Drop` joins a worker that is still spinning in [`Gate::hold`] — the
    /// test would hang instead of failing, which is the one way a test can be
    /// worse than useless. Declared *after* the handle, so it drops first.
    struct ReleaseOnDrop(Gate);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
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
        // Wait for the *publish* rather than for `pending` to move: the count
        // is bumped before the send, so it is non-zero from the instant the
        // request exists — what is waited for here is the window closing with
        // no `Quiesce`, which is the property under test.
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.generation() == published && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(live.generation(), published + 1, "one window, one publish");
        assert!(live.snapshot().0.files.contains_key("src/late.rs"));
        // The count can lag the publish by the instruction it takes to store
        // it, so it is polled rather than asserted outright.
        let deadline = Instant::now() + Duration::from_secs(5);
        while live.pending() > 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(live.pending(), 0, "and it is idle again");
    }

    /// A request the handle has *sent* counts as pending until the worker has
    /// taken it, even when the window before it has already landed.
    ///
    /// This is the state the count used to miss, and it is the state a turn's
    /// own resync is in when it gives up waiting: the worker is busy on a
    /// previous batch, the turn's requests sit in the channel, the deadline
    /// expires, and the count has to say so. Racing that gap would prove
    /// nothing — it is microseconds wide — so the worker is held at the end of
    /// its first window by [`Options::hold`] and the second request is sent
    /// into a worker that provably cannot receive it.
    #[test]
    fn a_sent_request_is_pending_until_the_worker_takes_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");

        let gate = Gate::default();
        let held = gate.clone();
        let live = GenomeHandle::spawn(
            root,
            Options {
                debounce: Duration::from_secs(600),
                hold: Some(Arc::new(move || held.hold())),
                ..Options::default()
            },
        )
        .unwrap();
        // After the handle, so it drops before it: see `ReleaseOnDrop`.
        let _release = ReleaseOnDrop(gate.clone());

        // First window: one path, applied, and then the worker blocks at the
        // end of the window instead of waiting for the next request.
        write(root, "src/first.rs", "pub fn first() {}\n");
        assert!(live.request(Request::Changed(vec!["src/first.rs".to_owned()])));
        assert!(
            live.quiesce(Duration::from_secs(30)),
            "the first window landed"
        );
        assert_eq!(live.pending(), 0, "nothing outstanding once it landed");
        let published = live.generation();

        // Second request, sent while the worker is held: it is in the channel
        // and no window owns it, so the count is the only thing that can say
        // there is work.
        write(root, "src/second.rs", "pub fn second() {}\n");
        assert!(live.request(Request::Changed(vec!["src/second.rs".to_owned()])));
        assert!(
            live.pending() > 0,
            "a sent request is work before the worker sees it"
        );
        assert_eq!(
            live.generation(),
            published,
            "and it is genuinely not applied yet"
        );

        // Released, the second window lands and the count follows.
        gate.release();
        assert!(live.quiesce(Duration::from_secs(30)));
        assert_eq!(live.pending(), 0);
        assert!(live.snapshot().0.files.contains_key("src/second.rs"));
    }

    /// A window whose apply failed keeps its work, and a window with nothing
    /// in it does not clear the count that work is standing in.
    ///
    /// A root that has gone away is the failure this can produce: the walk
    /// cannot read it. No timing is involved — the failure is a fact of the
    /// tree, and the count is read after the quiesce that carries it.
    #[test]
    fn a_failed_window_keeps_its_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/seed.rs", "pub fn seed() {}\n");
        let live = GenomeHandle::spawn(root, patient()).unwrap();
        let published = live.generation();

        std::fs::remove_dir_all(root).unwrap();
        assert!(live.request(Request::Resync));
        assert!(
            !live.quiesce(Duration::from_secs(30)),
            "a window that failed did not fold what it was waiting for"
        );
        assert!(live.pending() > 0, "and the work is still outstanding");
        assert_eq!(live.generation(), published, "nothing was published");

        // The next window is the retried work, not an empty one: the queue
        // survives the failure, so there is nothing to clear and a second
        // `Quiesce` cannot make the backlog disappear.
        assert!(!live.quiesce(Duration::from_secs(30)));
        assert!(
            live.pending() > 0,
            "a window with nothing new in it does not clear a failure"
        );
        assert_eq!(live.generation(), published);
    }

    /// The work of a failed window is retried in the next one rather than
    /// dropped, including when that next window was asked for something else.
    ///
    /// The evidence is the retried walk itself: the second window's own
    /// request names one path, so a targeted fold would report
    /// `walked: false` and leave the other file's record stale. It reports a
    /// walk and re-parses the file the failed window was holding.
    #[test]
    fn a_failed_windows_work_is_retried_in_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "pub fn a() {}\n");
        let live = GenomeHandle::spawn(root, patient()).unwrap();
        assert!(
            live.snapshot().0.files["src/a.rs"]
                .exports
                .contains(&"a".to_owned()),
            "the cold start indexed the file"
        );

        // The failed window: a walk, with `src/a.rs` named as its priority.
        std::fs::remove_dir_all(root).unwrap();
        assert!(live.request(Request::Priority(vec!["src/a.rs".to_owned()])));
        assert!(live.request(Request::Resync));
        assert!(!live.quiesce(Duration::from_secs(30)), "the walk failed");
        assert!(live.pending() > 0);

        // The tree comes back with `a.rs` edited and a second file, and the
        // next window is asked only for the second.
        write(root, "src/a.rs", "pub fn a() {}\npub fn a_two() {}\n");
        write(root, "src/b.rs", "pub fn b() {}\n");
        assert!(live.request(Request::Changed(vec!["src/b.rs".to_owned()])));
        assert!(live.quiesce(Duration::from_secs(30)), "the retry landed");
        assert_eq!(live.pending(), 0);

        let stats = live.last_stats().expect("the retry was applied");
        assert!(
            stats.walked,
            "the failed window's walk was retried, not the new path alone: {stats:?}"
        );
        let snapshot = live.snapshot().0;
        assert!(snapshot.files.contains_key("src/b.rs"));
        assert!(
            snapshot.files["src/a.rs"]
                .exports
                .contains(&"a_two".to_owned()),
            "the file the failed window was holding was re-parsed: {:?}",
            snapshot.files["src/a.rs"].exports
        );
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
                hold: None,
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
