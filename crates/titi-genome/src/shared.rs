//! The publish point: one consistent [`Genome`] a reader can hold while a
//! writer moves the index forward.
//!
//! [`Genome`]'s fields are read together — the projection pairs every `files`
//! entry with its `ranks` and `dependents`, a reference query pairs `symbols`
//! with `files` — so a reader that saw `files` from one refresh and `ranks`
//! from the next would answer from a graph that never existed. `Genome`
//! itself offers no way to say that: `refresh` takes `&mut self` and moves
//! every field in turn, so a reader that shares a `Genome` with a writer has
//! no consistent moment to read it.
//!
//! [`SharedGenome`] is that moment. The index is mutated privately and
//! published as one `Arc`: a reader gets an owned snapshot, cheap to clone and
//! never mutated afterwards, so it sees either the state before an update or
//! the state after it — never a field-wise mixture of the two, and never a
//! wait for the parse an update is doing on its private copy.
//!
//! The engine holds one of these — wrapped in a [`crate::GenomeHandle`], which
//! owns the background writer — instead of `Arc<Mutex<Option<Genome>>>`. It
//! has two writers and they are serialised here rather than by a lock a reader
//! takes: the tool loop folds a file a tool wrote in as the call returns, and
//! the handle's worker applies the batches nothing named a tool for.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::{Genome, RefreshStats};

/// A [`Genome`] published as one value.
///
/// Cheap to share: `clone` is one `Arc` clone. Every read goes through
/// [`Self::snapshot`], which hands out an owned snapshot rather than a borrow,
/// so a reader may hold it across an update.
#[derive(Clone, Default)]
pub struct SharedGenome {
    /// The single pointer a reader loads. The lock guards the *pointer*, not
    /// the genome, and no update holds it for longer than the store: a reader
    /// takes it only long enough to clone the `Arc`, so it never waits for a
    /// parse, and the graph it gets is the one that was complete when it
    /// asked.
    state: Arc<RwLock<Arc<Genome>>>,
    /// Serialises *writers*, of which there may be two: the tool loop folding
    /// in what a tool just wrote, and [`crate::GenomeHandle`]'s worker
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

impl SharedGenome {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index `root` from nothing, then publish it.
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut genome = Genome::default();
        let stats = genome.refresh(root)?;
        Ok(Self {
            state: Arc::new(RwLock::new(Arc::new(genome))),
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

    /// [`Self::refresh`], parsing `urgent` first — see [`Genome::refresh_urgent`].
    ///
    /// # Panics
    ///
    /// As [`Self::refresh`].
    pub fn refresh_urgent(
        &self,
        root: impl AsRef<Path>,
        urgent: &[String],
    ) -> std::io::Result<RefreshStats> {
        self.update(|genome| genome.refresh_urgent(root, urgent))
    }

    /// Run a targeted update — see [`Genome::apply_changes`] — and publish it.
    ///
    /// # Panics
    ///
    /// As [`Self::refresh`].
    pub fn apply_changes(&self, paths: &[String]) -> std::io::Result<RefreshStats> {
        self.update(|genome| genome.apply_changes(paths))
    }

    /// Do one update against a private copy and publish it whole.
    ///
    /// A failed update publishes nothing and bumps nothing: the caller that
    /// asked keeps the graph it had, and the generation still names it.
    fn update<F>(&self, work: F) -> std::io::Result<RefreshStats>
    where
        F: FnOnce(&mut Genome) -> std::io::Result<RefreshStats>,
    {
        let _sole_writer = self.writer.lock().expect("genome writer lock poisoned");
        let mut next = (*self.snapshot()).clone();
        let stats = work(&mut next)?;
        *self.state.write().expect("genome lock poisoned") = Arc::new(next);
        self.generation.fetch_add(1, Ordering::Release);
        self.record(stats);
        Ok(stats)
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
    /// background thread needs [`crate::GenomeHandle::pending`] for that. The
    /// engine builds this with `default()` and updates it at the first turn,
    /// so `None` means "no root has been indexed yet".
    pub fn last_stats(&self) -> Option<RefreshStats> {
        *self.last.lock().expect("genome stats lock poisoned")
    }

    fn record(&self, stats: RefreshStats) {
        *self.last.lock().expect("genome stats lock poisoned") = Some(stats);
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
    pub fn snapshot(&self) -> Arc<Genome> {
        Arc::clone(&self.state.read().expect("genome lock poisoned"))
    }

    /// Read once, under one snapshot.
    pub fn read<R>(&self, f: impl FnOnce(&Genome) -> R) -> R {
        f(&self.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::SystemTime;

    use super::SharedGenome;
    use crate::Genome;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The invariant a half-updated graph breaks: the three per-path maps are
    /// keyed by the same paths, and every symbol is defined by a known file.
    fn consistent(genome: &Genome) -> bool {
        genome.files.len() == genome.ranks.len()
            && genome.files.len() == genome.dependents.len()
            && genome
                .ranks
                .keys()
                .all(|path| genome.files.contains_key(path))
            && genome
                .dependents
                .keys()
                .all(|path| genome.files.contains_key(path))
            && genome.symbols.values().all(|symbol| {
                symbol
                    .files
                    .iter()
                    .all(|file| genome.files.contains_key(file))
            })
            // The mention index is the inversion of every record's raw refs:
            // no entry names a file the graph dropped, and no raw mention is
            // missing from it. A reader that saw one half of that pair from
            // before an update and one from after would read it as broken.
            && genome.ref_index.iter().all(|(name, files)| {
                !files.is_empty()
                    && files.iter().all(|file| {
                        genome
                            .files
                            .get(file)
                            .is_some_and(|record| record.raw_refs.contains(name))
                    })
            })
            && genome.files.iter().all(|(path, record)| {
                record.raw_refs.iter().all(|name| {
                    genome
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

        let live = SharedGenome::index(root).unwrap();
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
    /// detectably inconsistent. Built here with the public `Genome` API, the
    /// only way `SharedGenome` could expose it is by publishing mid-update.
    #[test]
    fn a_partial_update_is_detectably_inconsistent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "pub fn a() {}\n");
        let mut genome = Genome::index(root).unwrap();
        assert!(consistent(&genome));

        let mut extra = genome.files["src/a.rs"].clone();
        extra.path = "src/brand-new.rs".to_owned();
        extra.mtime = SystemTime::now();
        genome.files.insert(extra.path.clone(), extra);
        assert!(
            !consistent(&genome),
            "a new path with no rank must not read as consistent"
        );
    }
}
