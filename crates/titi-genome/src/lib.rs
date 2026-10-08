//! Ranked workspace map: files, symbols, dependency edges, PageRank, prompt
//! projection.
//!
//! The graph has two kinds of edge: a file imports another file, and a file
//! references a symbol another file defines. Both feed the same ranking and
//! the same `(→N)` dependent count, so "what depends on this" answers in
//! symbol terms instead of only file terms.
//!
//! The research note that used to specify this graph is gone; the paragraphs
//! above are the description.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use lang::support::first_known;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub mod ast_edit;

mod content;
mod graph;
mod lang;
mod lsp;
mod project;
mod query;
mod refs;
mod scan;
mod shared;
mod symbols;

pub use lang::{Capability, Language, Level};
pub use lsp::serve_lsp;
pub use project::render;
pub use scan::list_files;
pub use shared::SharedGenome;

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSite {
    pub name: String,
    /// 1-based.
    pub line: u32,
    /// 0-based UTF-8 byte offset on the line. ASCII matches an LSP character.
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub severity: Severity,
    pub code: String,
    pub message: String,
}

/// One path an import specifier may name, before the known-file set decides.
///
/// Resolution used to happen once, inside the parse, and only the answer
/// survived: a specifier that named no file left a string behind and nothing
/// else, so the only way to ask again was to read and parse the file that
/// wrote it. Splitting the answer into a candidate list and a membership test
/// makes "does this specifier name that file now" a question about the index
/// — the candidates depend on the specifier and the importing file, not on
/// which files exist — and it is what [`FileRecord::unresolved_candidates`]
/// keeps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Candidate {
    /// Exactly this path.
    Exact(String),
    /// Any known path ending with `/{path}`: the file may live under a root
    /// prefix the import omitted, which is how a Go package path or a Ruby
    /// gem-style `require` is spelled.
    Suffix(String),
}

impl Candidate {
    /// Whether `path` is a file this candidate names.
    pub fn identifies(&self, path: &str) -> bool {
        match self {
            Self::Exact(candidate) => candidate == path,
            Self::Suffix(candidate) => {
                path == candidate || path.ends_with(&format!("/{candidate}"))
            }
        }
    }
}

/// One specifier that named no known file, and the paths it would have named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedImport {
    /// The specifier as written, which is what a diagnostic quotes.
    pub spec: String,
    /// The paths this specifier would name, in priority order.
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: String,
    pub language: Language,
    pub exports: Vec<String>,
    /// One site per export, same names as [`Self::exports`], source order.
    pub export_sites: Vec<ExportSite>,
    pub imports: Vec<String>,
    /// Specifiers that did not resolve to a known file. Not graph edges.
    pub unresolved_imports: Vec<String>,
    /// The same specifiers, each with the paths it would have named.
    ///
    /// Kept beside the diagnostic list because it is the only evidence a
    /// later update can re-ask the question with: the candidates depend on
    /// the specifier and the importing file, not on which files exist, so a
    /// file appearing later is a membership test over this list instead of a
    /// read and a parse of the file that wrote it. Bounded by the file's own
    /// import count, and empty when every specifier resolved.
    pub unresolved_candidates: Vec<UnresolvedImport>,
    /// Tree-sitter `ERROR` nodes. Zero for languages without a grammar.
    /// A grammar that refuses the file counts as 1.
    pub syntax_errors: u32,
    /// Exported symbols defined elsewhere that this file mentions — the
    /// symbol-level half of the dependency graph. Resolved from
    /// [`Self::raw_refs`] against the workspace's exports.
    pub used_symbols: Vec<String>,
    /// The identifiers this file mentions, as the parser collected them.
    ///
    /// [`Self::used_symbols`] is the subset of these that resolve: a name
    /// has to be exported by exactly one known file and not be this file's
    /// own export to carry an edge. The raw list is what answers "which
    /// files does an export change concern" without reading them — see
    /// [`Genome::ref_index`]. Bounded by `refs::MAX_REFS`.
    pub raw_refs: Vec<String>,
    pub size: u64,
    pub mtime: SystemTime,
    /// FNV-1a of the bytes this record was parsed from.
    ///
    /// `size` and `mtime` are the cheap pre-filter; this is the confirmation
    /// that costs a read. A file whose `size` and `hash` both match the
    /// previous record is the same file even when `mtime` moved, so `refresh`
    /// keeps the record — new `mtime`, same parse — instead of re-parsing.
    /// It is not a security digest and must not be used as one.
    pub hash: u64,
}

/// A name exported by more than this many files is ambiguous under name-only
/// resolution, so it carries neither edges nor a user count. One definition
/// site means a mention that survived collection can only mean that file.
/// A method call or a std-qualified path is not such a mention: `path.join`
/// is not a use of the file that uniquely exports `join`.
pub const MAX_DEFINERS: usize = 1;

/// One symbol the workspace defines, and who leans on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    /// Files that define (export) the symbol, sorted.
    pub files: Vec<String>,
    /// Distinct files that reference it, excluding the definers.
    pub users: usize,
}

impl SymbolRecord {
    pub fn is_public(&self) -> bool {
        !self.files.is_empty()
    }
}

/// What one [`Genome::refresh`] or [`Genome::apply_changes`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshStats {
    /// Files re-read **and re-parsed**: new, or changed in content.
    pub parsed: usize,
    /// Files dropped because they vanished from disk.
    pub removed: usize,
    /// Files in the index afterwards.
    pub total: usize,
    /// Files whose content was read and found identical to the recorded
    /// bytes, despite `size` or `mtime` having moved.
    ///
    /// These passed the pre-filter — the `stat` disagreed — and the content
    /// hash settled it. They cost a read each and saved a parse each. A file
    /// the pre-filter skipped without reading is in none of these counts: it
    /// was not examined, so calling it "unchanged" would claim more evidence
    /// than a `stat` gives.
    pub content_unchanged: usize,
    /// Whether `ranks`, `dependents` and `symbols` were rebuilt this time.
    ///
    /// False only when nothing that feeds the graph moved: no path appeared or
    /// vanished, and every re-parse yielded the same `(exports, imports,
    /// used_symbols)` tuple as the record it replaced. A body-only edit is the
    /// common case — a changed `mtime` and a changed byte range, no change to
    /// what the graph is built from — and it leaves the previous maps in
    /// place rather than paying the rank iteration to reproduce them.
    pub graph_recomputed: bool,
    /// Files the symbol pass re-resolved against the new definer set.
    ///
    /// A file is in that pass when it was re-parsed, or when a name its raw
    /// mentions name changed definers. Every other file keeps the resolution
    /// it had, so adding one export costs the files that mention that export
    /// and not the tree. Zero when the graph was not recomputed at all.
    pub reresolved: usize,
}

/// The workspace graph and its derived ranking.
///
/// Every field is public and mutated in place by [`Self::refresh`] and
/// [`Self::apply_changes`], which take `&mut self`: a single reader beside a
/// writer needs no more than that, because Rust will not hand out a `&Genome`
/// while a `&mut Genome` exists. A reader that is *concurrent* with the writer
/// — a second agent, a watcher thread — needs a publish point, since it would
/// otherwise have to hold a borrow across an update; [`SharedGenome`] is that
/// point and hands out whole snapshots.
#[derive(Debug, Clone, Default)]
pub struct Genome {
    pub files: HashMap<String, FileRecord>,
    pub ranks: HashMap<String, f64>,
    pub dependents: HashMap<String, usize>,
    /// Symbol name → defining files and how many files reference it.
    pub symbols: HashMap<String, SymbolRecord>,
    /// Symbol name → the files whose raw mentions name it.
    ///
    /// The inversion of [`FileRecord::raw_refs`]. A name resolves in a file
    /// only if the workspace exports it and only if that file mentions it, so
    /// when a definer set moves this answers "which files can that have
    /// changed for" without reading one — the re-resolution pass visits these
    /// lists and the files that were re-parsed, and nothing else.
    pub ref_index: HashMap<String, Vec<String>>,
    /// Root of the last refresh, so definition can read the identifier.
    root: std::path::PathBuf,
}

/// One refresh's file-level outcome, before the graph decision is folded in.
struct Absorption {
    parsed: usize,
    content_unchanged: usize,
    /// Whether anything that feeds `ranks`/`dependents`/`symbols` moved.
    graph_moved: bool,
    /// The paths that were re-parsed, so the symbol pass knows which files
    /// can have a resolution to redo beyond the ones the moved names reach.
    dirty: Vec<String>,
}

impl Genome {
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut genome = Self::default();
        genome.refresh(root)?;
        Ok(genome)
    }

    /// Re-walk `root` and fold every file that moved into the index.
    ///
    /// This is the **fallback**: it walks the tree, so it catches files no
    /// caller mentioned — created, edited or deleted behind the index's back —
    /// and it is what the engine's per-turn call uses, because a turn can run
    /// an arbitrary command that changes anything anywhere. Callers that
    /// already know which paths changed should prefer [`Self::apply_changes`];
    /// this walks in addition to the work they know about.
    ///
    /// A file is examined only when its `size` or `mtime` differs from the
    /// recorded one, which is a `stat` per file and no read. A file that does
    /// pass that gate is read, hashed and — if the bytes match — kept without
    /// being parsed; see [`FileRecord::hash`].
    ///
    /// `ranks`, `dependents` and `symbols` are rebuilt only when the graph
    /// inputs moved: a path appeared or vanished, or a re-parse changed some
    /// file's `(exports, imports, used_symbols)`. [`RefreshStats`] reports
    /// which happened.
    pub fn refresh(&mut self, root: impl AsRef<Path>) -> std::io::Result<RefreshStats> {
        let root = root.as_ref();
        self.root = root.to_path_buf();
        let listed = scan::list_files(root)?;
        let known: HashSet<String> = listed.iter().map(|file| file.path.clone()).collect();

        let absorbed = self.absorb(&listed, &known);
        let gone: Vec<String> = self
            .files
            .keys()
            .filter(|path| !known.contains(*path))
            .cloned()
            .collect();
        for path in &gone {
            self.drop_record(path);
        }
        let removed = gone.len();
        let graph_recomputed = absorbed.graph_moved || removed > 0;
        let reresolved = self.finish(graph_recomputed, &absorbed.dirty);
        Ok(RefreshStats {
            parsed: absorbed.parsed,
            removed,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
            reresolved,
        })
    }

    /// Fold exactly `paths` into the index, without walking.
    ///
    /// This is the **targeted** path: a watcher, or the harness's own tool
    /// events, already knows which files its edits touched, and a `stat` per
    /// named path is the whole cost of asking. It cannot see a file nobody
    /// named — a sibling process, a `git checkout`, a generator — so a caller
    /// that has run an arbitrary command should pair it with, or fall back to,
    /// [`Self::refresh`].
    ///
    /// `paths` are index keys as [`Self::files`] spells them, relative to the
    /// root of the last [`Self::refresh`]. A named path that is no longer a
    /// file this index carries — deleted, or never a source file — is dropped
    /// from the index; every other path is left alone, whether or not it
    /// changed, because the caller did not name it.
    pub fn apply_changes(&mut self, paths: &[String]) -> std::io::Result<RefreshStats> {
        let mut listed = Vec::new();
        let mut removed = 0;
        let mut seen: HashSet<&str> = HashSet::new();
        for path in paths {
            // A watcher may report one path twice in a burst; it is one change.
            if !seen.insert(path) {
                continue;
            }
            match scan::stat(&self.root, path) {
                Some(file) => listed.push(file),
                // Gone from disk, or never a file this index would carry. The
                // first is a removal; the second is nothing to do, and
                // `drop_record` answers both without a second question.
                None => removed += usize::from(self.drop_record(path)),
            }
        }
        let known: HashSet<String> = self
            .files
            .keys()
            .cloned()
            .chain(listed.iter().map(|file| file.path.clone()))
            .collect();

        let absorbed = self.absorb(&listed, &known);
        let graph_recomputed = absorbed.graph_moved || removed > 0;
        let reresolved = self.finish(graph_recomputed, &absorbed.dirty);
        Ok(RefreshStats {
            parsed: absorbed.parsed,
            removed,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
            reresolved,
        })
    }

    /// Parse what moved in `listed` and fold it into `files`.
    ///
    /// `known` is every path that exists after this update and feeds import
    /// resolution; `listed` need not be all of it, since a targeted update
    /// names a subset. Paths are neither added nor dropped here — `refresh`
    /// sweeps what the walk did not list, `apply_changes` drops what it was
    /// told is gone — so this only ever replaces records or inserts the ones
    /// its caller already accounted for.
    fn absorb(
        &mut self,
        listed: &[scan::ListedFile],
        known: &HashSet<String>,
    ) -> Absorption {
        // Every path this update adds. It is the only way a specifier that
        // named nothing can come to name something — the candidates a specifier
        // carries do not move, only the file set does.
        let appeared: HashSet<String> = listed
            .iter()
            .filter(|file| !self.files.contains_key(&file.path))
            .map(|file| file.path.clone())
            .collect();
        let stale: Vec<&scan::ListedFile> = listed
            .iter()
            .filter(|file| {
                !self
                    .files
                    .get(&file.path)
                    .is_some_and(|record| record.size == file.size && record.mtime == file.mtime)
            })
            .collect();
        let records = parse_batch(&stale, known, &self.files);

        // A parse can only move the graph by changing the tuple the graph is
        // built from. `exports` and `imports` are compared exactly; `refs` are
        // resolved here against the previous symbols — valid exactly when no
        // `exports` moved, which is what the first pass establishes — and the
        // resolved set compared to the record's. Same tuple on every re-parse,
        // same path set, same graph.
        let mut graph_moved = false;
        for (record, parsed) in &records {
            if !*parsed {
                continue;
            }
            match self.files.get(&record.path) {
                Some(old) if old.exports == record.exports && old.imports == record.imports => {}
                _ => graph_moved = true,
            }
        }
        if !graph_moved {
            for (record, parsed) in &records {
                if !*parsed {
                    continue;
                }
                let resolved = resolve(&record.raw_refs, &record.exports, &self.symbols);
                if self
                    .files
                    .get(&record.path)
                    .is_some_and(|old| old.used_symbols != resolved)
                {
                    graph_moved = true;
                    break;
                }
            }
        }

        let mut parsed = 0;
        let mut content_unchanged = 0;
        let mut dirty = Vec::new();
        for (mut record, reparsed) in records {
            let path = record.path.clone();
            let previous = self
                .files
                .get(&path)
                .map(|old| (old.raw_refs.clone(), old.used_symbols.clone()));
            if reparsed {
                parsed += 1;
                dirty.push(path.clone());
                if !graph_moved {
                    // The graph is not rebuilt, so this record must carry the
                    // resolved form the previous one did: nothing that decides
                    // a resolution moved, and the parse only produced raw
                    // mentions.
                    if let Some((_, resolved)) = &previous {
                        record.used_symbols = resolved.clone();
                    }
                }
            } else {
                content_unchanged += 1;
            }
            // The mention index is the inversion of `raw_refs`, and a parse can
            // change those without the graph moving at all — a renamed local,
            // a new call to a symbol no file exports. It has to follow the
            // record even then, or a later export change would miss this file.
            if let Some((old_refs, _)) = previous {
                self.forget_refs(&path, &old_refs);
            }
            let raw_refs = record.raw_refs.clone();
            self.files.insert(path.clone(), record);
            self.learn_refs(&path, &raw_refs);
        }
        // A specifier that named no file can name one now: this update added
        // it. The candidates the specifier was parsed with answer whether,
        // without reading the file that wrote it.
        graph_moved |= self.resolve_new_imports(&appeared, known);
        Absorption {
            parsed,
            content_unchanged,
            graph_moved,
            dirty,
        }
    }

    /// Drop a path from the index and from the mention index.
    ///
    /// The two move together: a stale mention entry would have this file
    /// re-resolved for a name it no longer mentions, and a missing one would
    /// have it skipped for a name it does.
    fn drop_record(&mut self, path: &str) -> bool {
        match self.files.remove(path) {
            Some(record) => {
                self.forget_refs(path, &record.raw_refs);
                true
            }
            None => false,
        }
    }

    /// Record that `path` mentions every name in `names`.
    fn learn_refs(&mut self, path: &str, names: &[String]) {
        for name in names {
            let files = self.ref_index.entry(name.clone()).or_default();
            if !files.iter().any(|known| known == path) {
                files.push(path.to_owned());
            }
        }
    }

    /// Drop `path` from the mention lists of `names`.
    fn forget_refs(&mut self, path: &str, names: &[String]) {
        for name in names {
            let Some(files) = self.ref_index.get_mut(name) else {
                continue;
            };
            files.retain(|known| known != path);
            if files.is_empty() {
                self.ref_index.remove(name);
            }
        }
    }

    /// Resolve the imports this update's arrivals have made resolvable.
    ///
    /// A specifier that named no file when it was parsed kept the paths it
    /// would have named, so "does it name that file now" is set membership
    /// over the index: no read, no parse, no second walk. The file that wrote
    /// it is not re-parsed, its `imports` simply gains the edge and its
    /// diagnostic goes away. Returns whether any file moved, which the caller
    /// needs because a changed `imports` list is a changed graph input.
    ///
    /// One case this does not reach: an import that *resolved* when it was
    /// parsed and whose target was later deleted keeps the edge in `imports`
    /// (the ranker drops it, since the path is no longer a node) and keeps no
    /// specifier to restore the diagnostic from. Only re-parsing that file
    /// fixes its record; the graph is right either way.
    fn resolve_new_imports(&mut self, appeared: &HashSet<String>, known: &HashSet<String>) -> bool {
        if appeared.is_empty() {
            return false;
        }
        let mut moved = false;
        for record in self.files.values_mut() {
            if record.unresolved_candidates.is_empty() {
                continue;
            }
            let pending = std::mem::take(&mut record.unresolved_candidates);
            let mut still = Vec::with_capacity(pending.len());
            for import in pending {
                // The membership test first: only a candidate that names a
                // path this update added can have changed answer, and asking
                // the known set instead would be a lookup per stored
                // specifier on every update.
                let arrived = import
                    .candidates
                    .iter()
                    .any(|candidate| appeared.iter().any(|path| candidate.identifies(path)));
                match arrived
                    .then(|| first_known(&import.candidates, known))
                    .flatten()
                {
                    Some(path) => {
                        record.imports.push(path);
                        moved = true;
                    }
                    None => still.push(import),
                }
            }
            record.unresolved_candidates = still;
            record.unresolved_imports = record
                .unresolved_candidates
                .iter()
                .map(|import| import.spec.clone())
                .collect();
            record.imports.sort();
            record.imports.dedup();
        }
        moved
    }

    /// Rebuild `symbols`, `ranks` and `dependents`, or leave them alone.
    ///
    /// A refresh that found nothing the graph is built from leaves all three
    /// maps exactly as they were: not "equal after a rebuild", the previous
    /// values, untouched. When they are rebuilt, `dirty` names the files this
    /// update re-parsed; the symbol pass re-resolves those and the mentioners
    /// of any name whose definers moved, and nothing else. Returns how many
    /// files that pass visited.
    fn finish(&mut self, graph_recomputed: bool, dirty: &[String]) -> usize {
        if !graph_recomputed {
            return 0;
        }
        let reresolved = self.index_symbols(dirty);
        let (ranks, dependents) = graph::rank(&self.files, &self.symbols);
        self.ranks = ranks;
        self.dependents = dependents;
        reresolved
    }

    /// Rebuild the symbol table, re-resolving only the files it can have
    /// changed, and count them.
    ///
    /// Resolution is by name alone, so a name several files export — `is_empty`,
    /// `new`, `len` — cannot say which definition a mention refers to. Those
    /// names are ambiguous and carry no edges or user counts: counting them
    /// would make every file depend on every other file that happens to share
    /// a method name.
    ///
    /// The definer table is rebuilt from every file's `exports`, which is a
    /// pass over stored names and reads nothing. What that pass does *not*
    /// cost is the other half: re-resolving every file's identifiers. A file
    /// can only have a different resolution if it was re-parsed or if a name
    /// it mentions moved definers, and [`Self::ref_index`] answers the second
    /// without a scan. Everything else keeps the resolution it already had.
    fn index_symbols(&mut self, dirty: &[String]) -> usize {
        let mut definers: HashMap<String, Vec<String>> = HashMap::new();
        for (path, record) in &self.files {
            for name in &record.exports {
                definers.entry(name.clone()).or_default().push(path.clone());
            }
        }
        let mut moved: HashSet<String> = HashSet::new();
        for (name, files) in &mut definers {
            files.sort();
            files.dedup();
            if self.symbols.get(name).map(|symbol| &symbol.files) != Some(files) {
                moved.insert(name.clone());
            }
        }
        for name in self.symbols.keys() {
            // A name nobody exports any more: its file list is empty in the
            // new table (it is absent), and every file that mentions it has
            // to lose the edge.
            if !definers.contains_key(name) {
                moved.insert(name.clone());
            }
        }

        let previous = std::mem::take(&mut self.symbols);
        let mut symbols: HashMap<String, SymbolRecord> = HashMap::with_capacity(definers.len());
        for (name, files) in definers {
            // A name no definer moved for keeps the count it had; the pass
            // below recomputes the ones that changed.
            let users = if moved.contains(&name) {
                0
            } else {
                previous.get(&name).map_or(0, |symbol| symbol.users)
            };
            symbols.insert(name, SymbolRecord { files, users });
        }

        let mut targets: HashSet<String> = dirty.iter().cloned().collect();
        for name in &moved {
            if let Some(files) = self.ref_index.get(name) {
                targets.extend(files.iter().cloned());
            }
        }
        let mut affected: HashSet<String> = moved;
        let mut reresolved = 0;
        let mut changed: Vec<(String, Vec<String>)> = Vec::new();
        for path in targets {
            let Some(record) = self.files.get(&path) else {
                continue;
            };
            let resolved = resolve(&record.raw_refs, &record.exports, &symbols);
            affected.extend(
                record
                    .raw_refs
                    .iter()
                    .filter(|name| symbols.contains_key(*name))
                    .cloned(),
            );
            reresolved += 1;
            if record.used_symbols != resolved {
                changed.push((path, resolved));
            }
        }
        for (path, resolved) in changed {
            if let Some(record) = self.files.get_mut(&path) {
                record.used_symbols = resolved;
            }
        }

        for name in &affected {
            let Some(symbol) = symbols.get_mut(name) else {
                continue;
            };
            symbol.users = self.ref_index.get(name).map_or(0, |files| {
                files
                    .iter()
                    .filter(|path| {
                        self.files
                            .get(*path)
                            .is_some_and(|record| record.used_symbols.contains(name))
                    })
                    .count()
            });
        }
        self.symbols = symbols;
        reresolved
    }

    pub fn project(&self, limit: usize) -> String {
        render(self, limit, &HashSet::new())
    }

    /// Projection biased toward files this session edited or read.
    pub fn project_with(&self, limit: usize, touched: &[String]) -> String {
        let touched: HashSet<String> = touched.iter().cloned().collect();
        render(self, limit, &touched)
    }
}

/// Reads and parses the stale files, spreading them over the available cores.
///
/// Parsing is the only expensive step of a refresh and every file is
/// independent of the others — they share nothing but the read-only set of
/// known paths and the records they are compared against — so the work splits
/// cleanly. A refresh that touches one file stays on the calling thread.
///
/// Each entry is the record and whether it was *re-parsed*: a read whose bytes
/// match the previous record and whose size is unchanged is returned as that
/// record with the new `mtime`, and no parser runs.
fn parse_batch(
    stale: &[&scan::ListedFile],
    known: &HashSet<String>,
    previous: &HashMap<String, FileRecord>,
) -> Vec<(FileRecord, bool)> {
    let workers = std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
        .min(stale.len());
    if workers <= 1 {
        return stale
            .iter()
            .map(|file| parse_one(file, known, previous.get(&file.path)))
            .collect();
    }
    let chunk = stale.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = stale
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|file| parse_one(file, known, previous.get(&file.path)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| match handle.join() {
                Ok(records) => records,
                // A parser panic is a bug in this crate, not a file-level
                // condition to swallow: carry it to the caller's thread.
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect()
    })
}

fn parse_one(
    file: &scan::ListedFile,
    known: &HashSet<String>,
    previous: Option<&FileRecord>,
) -> (FileRecord, bool) {
    let source = fs::read_to_string(&file.abs).unwrap_or_default();
    let hash = content::fingerprint(source.as_bytes());
    if let Some(old) = previous {
        // The pre-filter already said size or mtime moved. Equal size and
        // equal bytes is the case it cannot tell from a real edit: keep the
        // parse, move the clock.
        if old.size == file.size && old.hash == hash {
            let mut record = old.clone();
            record.mtime = file.mtime;
            return (record, false);
        }
    }
    let result = lang::parse(&file.path, &source, known);
    (
        FileRecord {
            language: Language::from_path(&file.path),
            path: file.path.clone(),
            exports: result.exports,
            export_sites: result.export_sites,
            imports: result.imports,
            unresolved_imports: result.unresolved_imports,
            unresolved_candidates: result.unresolved_candidates,
            syntax_errors: result.syntax_errors,
            // Resolved against the workspace's exports by `index_symbols`,
            // once the batch is in — a name is only an edge when exactly one
            // file exports it, which one file's parse cannot know. Empty here
            // means "not resolved yet", and a parse the graph pass skips
            // carries the previous record's list instead.
            used_symbols: Vec::new(),
            raw_refs: result.refs,
            size: file.size,
            mtime: file.mtime,
            hash,
        },
        true,
    )
}

/// The identifiers of `refs` that can be edges: exported by exactly one known
/// file, and not by this one.
///
/// Resolution is by name alone, so a name several files export — `is_empty`,
/// `new`, `len` — cannot say which definition a mention refers to. Those names
/// are ambiguous and carry no edges, and a file's own exports are definitions,
/// not uses of itself. Shared by [`Genome::index_symbols`], which applies it
/// to every file when the graph is rebuilt, and by [`Genome::absorb`], which
/// applies it to one re-parse to ask whether the graph would even move.
fn resolve(
    refs: &[String],
    own_exports: &[String],
    symbols: &HashMap<String, SymbolRecord>,
) -> Vec<String> {
    refs.iter()
        .filter(|name| {
            symbols
                .get(*name)
                .is_some_and(|record| record.files.len() <= MAX_DEFINERS)
                && !own_exports.contains(name)
        })
        .cloned()
        .collect()
}
