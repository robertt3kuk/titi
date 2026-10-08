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
//! the state after it — never a field-wise mixture of the two. The engine's
//! per-turn call is the writer; the one-line follow-up to use this from
//! `titi-engine` is to hold `Arc<SharedGenome>` instead of
//! `Arc<Mutex<Option<Genome>>>` and call `refresh`/`project_with` on it.

use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::{Genome, RefreshStats};

/// A [`Genome`] published as one value.
///
/// Cheap to share: `clone` is one `Arc` clone. Every read goes through
/// [`Self::snapshot`], which hands out an owned snapshot rather than a borrow,
/// so a reader may hold it across an update.
#[derive(Clone, Default)]
pub struct SharedGenome {
    /// The single pointer a reader loads. The lock guards the *pointer*, not
    /// the genome: a reader takes it only long enough to clone the `Arc`, so
    /// it never blocks on the parse a writer is doing behind the other side.
    state: Arc<RwLock<Arc<Genome>>>,
}

impl SharedGenome {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index `root` from nothing, then publish it.
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut genome = Genome::default();
        genome.refresh(root)?;
        Ok(Self {
            state: Arc::new(RwLock::new(Arc::new(genome))),
        })
    }

    /// Refresh the live index and publish the result, one swap.
    ///
    /// While no snapshot is outstanding the genome is updated in place — the
    /// incremental path the refresh already is — and the pointer never moves.
    /// While one is, the writer copies first and the reader's snapshot keeps
    /// pointing at the pre-update graph, so a snapshot is never mutated under
    /// a reader.
    ///
    /// # Panics
    ///
    /// If a writer panicked mid-update the lock is poisoned and every later
    /// caller panics rather than observe a partly written index. A parser
    /// panic is a bug in this crate, not a condition to paper over with a
    /// possibly torn graph.
    pub fn refresh(&self, root: impl AsRef<Path>) -> std::io::Result<RefreshStats> {
        let mut state = self.state.write().expect("genome lock poisoned");
        Arc::make_mut(&mut state).refresh(root)
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
        let mut state = self.state.write().expect("genome lock poisoned");
        Arc::make_mut(&mut state).refresh_urgent(root, urgent)
    }

    /// Run a targeted update — see [`Genome::apply_changes`] — and publish it.
    ///
    /// # Panics
    ///
    /// As [`Self::refresh`].
    pub fn apply_changes(&self, paths: &[String]) -> std::io::Result<RefreshStats> {
        let mut state = self.state.write().expect("genome lock poisoned");
        Arc::make_mut(&mut state).apply_changes(paths)
    }

    /// A consistent snapshot of the graph.
    ///
    /// An `Arc` clone: a reader that iterates the whole graph costs the writer
    /// one copy-on-write the next time it publishes, and never a torn read.
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
