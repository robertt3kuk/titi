//! The publish point: one consistent [`Atlas`] a reader can hold while a
//! writer moves the index forward.
//!
//! [`Atlas`]'s fields are read together — the projection pairs every `files`
//! entry with its `ranks` and `dependents`, a reference query pairs `symbols`
//! with `files` — so a reader that saw `files` from one refresh and `ranks`
//! from the next would answer from a graph that never existed. `Atlas`
//! itself offers no way to say that: `refresh` takes `&mut self` and moves
//! every field in turn, so a reader that shares a `Atlas` with a writer has
//! no consistent moment to read it.
//!
//! [`SharedAtlas`] is that moment. The index is mutated privately and
//! published as one `Arc`: a reader gets an owned snapshot, cheap to clone and
//! never mutated afterwards, so it sees either the state before an update or
//! the state after it — never a field-wise mixture of the two, and never a
//! wait for the parse an update is doing on its private copy.
//!
//! The engine holds one of these — wrapped in a [`crate::AtlasHandle`], which
//! owns the background writer — instead of `Arc<Mutex<Option<Atlas>>>`. It
//! has two writers and they are serialised here rather than by a lock a reader
//! takes: the tool loop folds a file a tool wrote in as the call returns, and
//! the handle's worker applies the batches nothing named a tool for.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::{Atlas, RefreshStats, scan};

/// A [`Atlas`] published as one value.
///
/// Cheap to share: `clone` is one `Arc` clone. Every read goes through
/// [`Self::snapshot`], which hands out an owned snapshot rather than a borrow,
/// so a reader may hold it across an update.
#[derive(Clone, Default)]
pub struct SharedAtlas {
    /// The single pointer a reader loads. The lock guards the *pointer*, not
    /// the atlas, and no update holds it for longer than the store: a reader
    /// takes it only long enough to clone the `Arc`, so it never waits for a
    /// parse, and the graph it gets is the one that was complete when it
    /// asked.
    state: Arc<RwLock<Arc<Atlas>>>,
    /// Serialises *writers*, of which there may be two: the tool loop folding
    /// in what a tool just wrote, and [`crate::AtlasHandle`]'s worker
    /// applying a batch behind it. An update reads the published pointer,
    /// works on a private copy and stores the result; two updates doing that
    /// at once would each build from the same base and the second store would
    /// silently drop the first. Held for the whole of an update — the read,
    /// the parse, the rank — and never taken by a reader.
    writer: Arc<Mutex<()>>,
    /// Bumped by every publish. It is how a reader says "the graph moved"
    /// without comparing two of them, and how a caller counts applies.
    generation: Arc<AtomicU64>,
    /// What the last update did. Not part of the graph, so not published with
    /// it: a reader that wants to know whether the index was walked, or how
    /// much work the last update was, reads it here.
    last: Arc<Mutex<Option<RefreshStats>>>,
}

impl SharedAtlas {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index `root` from nothing, then publish it.
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut atlas = Atlas::default();
        let stats = atlas.refresh(root)?;
        Ok(Self {
            state: Arc::new(RwLock::new(Arc::new(atlas))),
            writer: Arc::new(Mutex::new(())),
            generation: Arc::new(AtomicU64::new(1)),
            last: Arc::new(Mutex::new(Some(stats))),
        })
    }

    /// Refresh the live index and publish the result, one swap.
    ///
    /// The work happens on a private copy of the published graph, outside
    /// every lock a reader takes: readers keep answering from the previous
    /// graph for as long as the update runs, and the new one appears whole at
    /// the store. The copy is the price of that — the previous shape updated
    /// the published value in place under the write lock, which was cheaper
    /// but made every reader wait for the parse, and a background indexer
    /// whose readers wait has moved the parse rather than removed it.
    ///
    /// # Panics
    ///
    /// If a writer panicked mid-update the lock is poisoned and every later
    /// caller panics rather than observe a partly written index. A parser
    /// panic is a bug in this crate, not a condition to paper over with a
    /// possibly torn graph.
    pub fn refresh(&self, root: impl AsRef<Path>) -> std::io::Result<RefreshStats> {
        self.refresh_urgent(root, &[])
    }

    /// [`Self::refresh`], parsing `urgent` first — see [`Atlas::refresh_urgent`].
    ///
    /// # Panics
    ///
    /// As [`Self::refresh`].
    pub fn refresh_urgent(
        &self,
        root: impl AsRef<Path>,
        urgent: &[String],
    ) -> std::io::Result<RefreshStats> {
        self.refresh_capped(root.as_ref(), urgent, usize::MAX)
    }

    /// The first walk of an index a surface is waiting on: [`Self::refresh`]
    /// on fewer parse threads than there are cores — see
    /// [`crate::first_walk_workers`] for why.
    pub(crate) fn refresh_first_walk(&self, root: &Path) -> std::io::Result<RefreshStats> {
        self.refresh_capped(root, &[], crate::first_walk_workers())
    }

    fn refresh_capped(
        &self,
        root: &Path,
        urgent: &[String],
        max_workers: usize,
    ) -> std::io::Result<RefreshStats> {
        let _sole_writer = self.writer.lock().expect("atlas writer lock poisoned");
        let listed = scan::list_files(root)?;
        let current = self.snapshot();
        // The common walk finds nothing: the copy, the publish and the swap
        // of a graph for an identical one are skipped, and a reader's
        // snapshot stays the current graph. [`Self::generation`] counts
        // publishes, so it does not move either.
        if current.is_current(root, &listed) {
            let stats = RefreshStats {
                total: current.files.len(),
                walked: true,
                ..RefreshStats::default()
            };
            self.record(stats);
            return Ok(stats);
        }
        let mut next = (*current).clone();
        let stats = next.refresh_listed(root, &listed, urgent, max_workers);
        self.publish(next, stats);
        Ok(stats)
    }

    /// Run a targeted update — see [`Atlas::apply_changes`] — and publish it.
    ///
    /// # Panics
    ///
    /// As [`Self::refresh`].
    /// Publish the co-change partners history found. A publish of its own,
    /// with no refresh stats to report: no file moved.
    pub fn set_cochange(&self, found: crate::CoChange) {
        let _sole_writer = self.writer.lock().expect("atlas writer lock poisoned");
        let mut next = (*self.snapshot()).clone();
        next.set_cochange(found);
        *self.state.write().expect("atlas lock poisoned") = Arc::new(next);
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn apply_changes(&self, paths: &[String]) -> std::io::Result<RefreshStats> {
        self.update(|atlas| atlas.apply_changes(paths))
    }

    /// Do one update against a private copy and publish it whole.
    ///
    /// A failed update publishes nothing and bumps nothing: the caller that
    /// asked keeps the graph it had, and the generation still names it.
    fn update<F>(&self, work: F) -> std::io::Result<RefreshStats>
    where
        F: FnOnce(&mut Atlas) -> std::io::Result<RefreshStats>,
    {
        let _sole_writer = self.writer.lock().expect("atlas writer lock poisoned");
        let mut next = (*self.snapshot()).clone();
        let stats = work(&mut next)?;
        self.publish(next, stats);
        Ok(stats)
    }

    /// Store `next` as the published graph and count it.
    fn publish(&self, next: Atlas, stats: RefreshStats) {
        *self.state.write().expect("atlas lock poisoned") = Arc::new(next);
        self.generation.fetch_add(1, Ordering::Release);
        self.record(stats);
    }

    /// How many times this handle has published. Monotonic; `0` on a handle
    /// that has never been updated, `1` after [`Self::index`].
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// What the last successful update did, or `None` on a handle that has
    /// never been updated.
    ///
    /// The publish point's own view of its progress: whether the update walked
    /// the tree or trusted the paths it was handed ([`RefreshStats::walked`]),
    /// how many files it re-parsed, and how many the symbol pass re-resolved.
    /// It is not part of the published graph, and it says nothing about
    /// whether an update is in flight — a handle whose only other writer is a
    /// background thread needs [`crate::AtlasHandle::pending`] for that. The
    /// engine builds this with `default()` and updates it at the first turn,
    /// so `None` means "no root has been indexed yet".
    pub fn last_stats(&self) -> Option<RefreshStats> {
        *self.last.lock().expect("atlas stats lock poisoned")
    }

    fn record(&self, stats: RefreshStats) {
        *self.last.lock().expect("atlas stats lock poisoned") = Some(stats);
    }

    /// A consistent snapshot of the graph.
    ///
    /// An `Arc` clone: cheap enough for a reader to take per question, and
    /// never a torn read, because an update builds its result privately and
    /// only then stores the pointer.
    ///
    /// # Panics
    ///
    /// As [`Self::refresh`].
    pub fn snapshot(&self) -> Arc<Atlas> {
        Arc::clone(&self.state.read().expect("atlas lock poisoned"))
    }

    /// Read once, under one snapshot.
    pub fn read<R>(&self, f: impl FnOnce(&Atlas) -> R) -> R {
        f(&self.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::SystemTime;

    use super::SharedAtlas;
    use crate::Atlas;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Backdate a file past the racy-timestamp window, so the walk trusts its
    /// metadata the way it does for a tree that has settled.
    fn settle(root: &std::path::Path, rel: &str, age: std::time::Duration) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(root.join(rel))
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    /// A walk that finds nothing stale has nothing to publish: the graph is
    /// not copied, the generation does not move and a reader's snapshot is
    /// still the current one — while the update is still reported.
    #[test]
    fn a_refresh_that_finds_nothing_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hour = std::time::Duration::from_secs(3600);
        write(root, "src/a.rs", "pub fn a() {}\n");
        write(root, "src/b.rs", "pub fn b() { a(); }\n");
        settle(root, "src/a.rs", hour);
        settle(root, "src/b.rs", hour);

        let live = SharedAtlas::index(root).unwrap();
        let before = live.snapshot();
        let generation = live.generation();

        let stats = live.refresh(root).unwrap();
        assert!(stats.walked, "{stats:?}");
        assert_eq!(
            (stats.parsed, stats.removed, stats.content_unchanged),
            (0, 0, 0)
        );
        assert!(!stats.graph_recomputed);
        assert_eq!(stats.total, 2);
        assert_eq!(live.last_stats(), Some(stats));
        assert_eq!(live.generation(), generation, "nothing was published");
        assert!(
            Arc::ptr_eq(&before, &live.snapshot()),
            "the graph readers hold was replaced by a copy of itself"
        );
    }

    /// Whatever a walk does find still goes through a copy and a publish: an
    /// edit, a new file, a removal, and the first walk of a root.
    #[test]
    fn a_refresh_that_finds_something_still_publishes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hour = std::time::Duration::from_secs(3600);
        write(root, "src/a.rs", "pub fn a() {}\n");
        settle(root, "src/a.rs", hour);
        let live = SharedAtlas::index(root).unwrap();

        let mut expect = live.generation();
        let mut step = |what: &str| {
            let before = live.snapshot();
            let stats = live.refresh(root).unwrap();
            expect += 1;
            assert_eq!(live.generation(), expect, "{what}: {stats:?}");
            assert!(!Arc::ptr_eq(&before, &live.snapshot()), "{what}");
            stats
        };

        write(root, "src/a.rs", "pub fn a() {}\npub fn a2() {}\n");
        settle(root, "src/a.rs", hour / 2);
        assert_eq!(step("an edit").parsed, 1);
        assert!(
            live.snapshot().files["src/a.rs"]
                .exports
                .contains(&"a2".to_owned())
        );

        write(root, "src/new.rs", "pub fn fresh() {}\n");
        settle(root, "src/new.rs", hour);
        assert_eq!(step("a new file").parsed, 1);
        assert!(live.snapshot().files.contains_key("src/new.rs"));

        std::fs::remove_file(root.join("src/new.rs")).unwrap();
        assert_eq!(step("a removal").removed, 1);
        assert!(!live.snapshot().files.contains_key("src/new.rs"));

        // A handle that never indexed has no root: its first walk publishes
        // even over a tree with no source files in it.
        let empty = tempfile::tempdir().unwrap();
        let fresh = SharedAtlas::new();
        fresh.refresh(empty.path()).unwrap();
        assert_eq!(fresh.generation(), 1);
    }

    /// The invariant a half-updated graph breaks: the three per-path maps are
    /// keyed by the same paths, and every symbol is defined by a known file.
    fn consistent(atlas: &Atlas) -> bool {
        atlas.files.len() == atlas.ranks.len()
            && atlas.files.len() == atlas.dependents.len()
            && atlas
                .ranks
                .keys()
                .all(|path| atlas.files.contains_key(path))
            && atlas
                .dependents
                .keys()
                .all(|path| atlas.files.contains_key(path))
            && atlas.symbols.values().all(|symbol| {
                symbol
                    .files
                    .iter()
                    .all(|file| atlas.files.contains_key(file))
            })
            // The mention index is the inversion of every record's raw refs:
            // no entry names a file the graph dropped, and no raw mention is
            // missing from it. A reader that saw one half of that pair from
            // before an update and one from after would read it as broken.
            && atlas.ref_index.iter().all(|(name, files)| {
                !files.is_empty()
                    && files.iter().all(|file| {
                        atlas
                            .files
                            .get(file)
                            .is_some_and(|record| record.raw_refs.contains(name))
                    })
            })
            && atlas.files.iter().all(|(path, record)| {
                record.raw_refs.iter().all(|name| {
                    atlas
                        .ref_index
                        .get(name)
                        .is_some_and(|files| files.contains(path))
                })
            })
    }

    /// A reader loop over a writer's updates sees only whole graphs.
    ///
    /// The writer alternates a file in and out of the tree, so `files` moves in
    /// both directions and a reader sampling often must catch it mid-flight.
    #[test]
    fn a_reader_never_sees_a_half_updated_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/hub.rs", "pub fn hub() {}\n");
        write(root, "src/user.rs", "pub fn run() { hub(); }\n");

        let live = SharedAtlas::index(root).unwrap();
        let stop = AtomicBool::new(false);
        let stop = &stop;

        let lengths = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for round in 0..400 {
                    let name = "src/flicker.rs";
                    if round % 2 == 0 {
                        write(root, name, "pub fn flicker() { hub(); }\n");
                    } else {
                        std::fs::remove_file(root.join(name)).unwrap();
                    }
                    let stats = live
                        .apply_changes(&[name.to_owned()])
                        .expect("apply_changes");
                    assert!(stats.graph_recomputed, "the path set moved: {stats:?}");
                }
                stop.store(true, Ordering::Release);
            });
            let reader = scope.spawn(|| {
                let mut lengths = std::collections::HashSet::new();
                while !stop.load(Ordering::Acquire) {
                    let snapshot = live.snapshot();
                    assert!(
                        consistent(&snapshot),
                        "torn graph, files {:?}",
                        snapshot.files.keys().collect::<Vec<_>>()
                    );
                    lengths.insert(snapshot.files.len());
                }
                lengths
            });
            writer.join().unwrap();
            reader.join().unwrap()
        });

        assert!(
            lengths.len() > 1,
            "the reader only ever saw {lengths:?}; the test proved nothing"
        );
    }

    /// The invariant has teeth: the state a non-atomic publish would expose —
    /// `files` already carrying the new record, `ranks` not yet rebuilt — is
    /// detectably inconsistent. Built here with the public `Atlas` API, the
    /// only way `SharedAtlas` could expose it is by publishing mid-update.
    #[test]
    fn a_partial_update_is_detectably_inconsistent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "pub fn a() {}\n");
        let mut atlas = Atlas::index(root).unwrap();
        assert!(consistent(&atlas));

        let mut extra = atlas.files["src/a.rs"].clone();
        extra.path = "src/brand-new.rs".to_owned();
        extra.mtime = SystemTime::now();
        atlas.files.insert(extra.path.clone(), extra);
        assert!(
            !consistent(&atlas),
            "a new path with no rank must not read as consistent"
        );
    }
}
