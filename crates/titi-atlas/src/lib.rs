//! Ranked workspace map: files, symbols, dependency edges, PageRank, prompt
//! projection.
//!
//! The graph has two kinds of edge: a file imports another file, and a file
//! references a symbol another file defines. Both feed the same ranking and
//! the same `(→N)` dependent count, so "what depends on this" answers in
//! symbol terms instead of only file terms. A reference links to the one file
//! it is a use of, decided by the language's scope rules rather than by the
//! name alone: see the `scope` module.
//!
//! The research note that used to specify this graph is gone; the paragraphs
//! above are the description.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use lang::support::first_known;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

pub mod ast_edit;

mod cochange;
mod content;
mod graph;
mod lang;
mod lens;
pub mod live;
mod lsp;
mod patterns;
mod project;
mod query;
mod recency;
mod refs;
mod reresolve;
mod role;
mod scan;
mod scope;
mod shared;
mod symbol;
mod symbols;
pub mod syntax_check;

pub use cochange::{CoChange, head_stamp as cochange_head_stamp, probe as probe_cochange};
pub use lang::{Capability, Language, Level};
pub use lens::{Lens, LensBudget, LensEntry, MAX_LENS_FILES, SeedInput, SeedKind, Seeds};
pub use live::AtlasHandle;
pub use lsp::serve_lsp;
pub use project::{RankedFile, ranking, render, skeleton};
pub use query::format_check_report;
pub use scan::list_files;
pub use shared::SharedAtlas;
pub use symbol::{MAX_SIGNATURE, SymbolKind, SymbolSite, one_line_signature};

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
    /// Every symbol the file defines, with its extent and, where its language
    /// module extracts one, its one-line signature. One entry per name in
    /// [`Self::exports`] and in the same order, so the two never disagree; see
    /// [`SymbolSite`].
    pub symbols: Vec<SymbolSite>,
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
    /// Files this file re-publishes: the subset of [`Self::imports`] that is a
    /// `pub use`, a `pub mod x;`, an `export … from`, or any import of a
    /// Python package's `__init__.py`. A name defined behind one of them is
    /// reachable through this file.
    pub reexports: Vec<String>,
    /// Files whose scope this one extends (Rust's `use super::*`): what they
    /// import, this file can name too.
    pub inherits: Vec<String>,
    /// First segments of the paths that name another crate (Rust), which the
    /// index maps to that crate's `lib.rs` when the crate is in the workspace.
    pub external_uses: Vec<String>,
    /// Exported symbols defined elsewhere that this file mentions, each with
    /// the one file it resolves to and why — the symbol-level half of the
    /// dependency graph. Resolved from [`Self::raw_refs`] against the
    /// workspace's exports by `scope::Resolver`.
    pub uses: Vec<SymbolUse>,
    /// The identifiers this file mentions, as the parser collected them.
    ///
    /// [`Self::uses`] is the subset of these that resolve: a name has to be
    /// reachable from this file, or be the one distinctive definition in the
    /// workspace, to carry an edge, and a file's own exports are never uses.
    /// The raw list is what answers "which files does an export change
    /// concern" without reading them — see [`Atlas::ref_index`]. Bounded by
    /// `refs::MAX_REFS`.
    pub raw_refs: Vec<String>,
    pub size: u64,
    pub mtime: SystemTime,
    /// The moment this record's bytes were read.
    ///
    /// Together with `mtime` it answers a question the file cannot answer for
    /// itself later: whether the record was taken while its `mtime` was still
    /// inside [`RACY_MTIME`] of the read. A record that was can never be
    /// trusted by metadata again - see `Atlas::absorb` - because a second
    /// write inside that same timestamp tick leaves `size` and `mtime` exactly
    /// as this record has them, and by the time the file looks settled the
    /// write that landed in between is no longer visible to any clock.
    pub read_at: SystemTime,
    /// FNV-1a of the bytes this record was parsed from.
    ///
    /// `size` and `mtime` are the cheap pre-filter; this is the confirmation
    /// that costs a read. A file whose `size` and `hash` both match the
    /// previous record is the same file even when `mtime` moved, so `refresh`
    /// keeps the record — new `mtime`, same parse — instead of re-parsing.
    /// It is not a security digest and must not be used as one.
    pub hash: u64,
}

/// A name exported by more than this many files is ambiguous: goto-definition
/// has no single answer for it and `titi atlas check` reports it when the
/// definers share a crate. The graph does not use this number: a use resolves
/// to one definer by scope (see `scope`), whatever else defines the name.
pub const MAX_DEFINERS: usize = 1;

/// How fresh an `mtime` may be, against the clock of a read, before the walk
/// stops trusting metadata about it.
///
/// Two writes inside one filesystem timestamp tick leave `size` and `mtime`
/// exactly as they were, and the content hash that would tell them apart is
/// only computed for a file that was read. So a file modified within this
/// window of the moment it is listed is read whatever its metadata says - and
/// a *record* taken while its own `mtime` was within this window of its read
/// ([`FileRecord::read_at`]) is never trusted by metadata again, however
/// settled the file looks later: the file ages out of the window, the write
/// that landed inside it does not become visible again. Two seconds is the
/// margin git's index uses for the same reason. A settled tree pays nothing;
/// a just-written file pays at most one extra read after it settles.
pub const RACY_MTIME: std::time::Duration = std::time::Duration::from_secs(2);

/// One symbol the workspace defines, and who leans on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    /// Files that define (export) the symbol, sorted.
    pub files: Vec<String>,
    /// Distinct files that reference it, excluding the definers.
    pub users: usize,
}

/// Why a mention was linked to its definer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The definer is in the file's scope: imported, re-exported to it, in
    /// its crate facade or package directory.
    Scope,
    /// No scope evidence, but the name is long, uncommon and defined by one
    /// file only. A guess, weighed as one.
    Unique,
}

/// One mention that resolves to a definition in another file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolUse {
    pub name: String,
    /// The one file this use is a use of.
    pub definer: String,
    pub basis: Basis,
}

impl FileRecord {
    /// The names of [`Self::uses`], in order.
    pub fn used_symbols(&self) -> Vec<String> {
        self.uses.iter().map(|used| used.name.clone()).collect()
    }

    /// Whether `other` resolves its imports the same way, which is everything
    /// but its own exports that decides what *other* files can reach through
    /// it and what it can reach.
    fn same_scope(&self, other: &Self) -> bool {
        self.imports == other.imports
            && self.reexports == other.reexports
            && self.inherits == other.inherits
            && self.external_uses == other.external_uses
    }
}

impl SymbolRecord {
    pub fn is_public(&self) -> bool {
        !self.files.is_empty()
    }
}

/// What one [`Atlas::refresh`] or [`Atlas::apply_changes`] actually did.
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
    /// re-exports, uses)` as the record it replaced. A body-only edit is the
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
    /// Whether this update listed the tree.
    ///
    /// [`Atlas::refresh`] walks, [`Atlas::apply_changes`] trusts the paths
    /// it was handed. A caller that logs this can tell a fallback resync from
    /// a targeted update, and a test can tell that a tool's write reached the
    /// index without a walk.
    pub walked: bool,
}

/// The workspace graph and its derived ranking.
///
/// Every field is public and mutated in place by [`Self::refresh`] and
/// [`Self::apply_changes`], which take `&mut self`: a single reader beside a
/// writer needs no more than that, because Rust will not hand out a `&Atlas`
/// while a `&mut Atlas` exists. A reader that is *concurrent* with the writer
/// — a second agent, a watcher thread — needs a publish point, since it would
/// otherwise have to hold a borrow across an update; [`SharedAtlas`] is that
/// point and hands out whole snapshots.
#[derive(Debug, Clone, Default)]
pub struct Atlas {
    pub files: HashMap<String, FileRecord>,
    pub ranks: HashMap<String, f64>,
    pub dependents: HashMap<String, usize>,
    /// Symbol name → defining files and how many files reference it, summed
    /// over the definers.
    pub symbols: HashMap<String, SymbolRecord>,
    /// Defining file → its exports that other files use, each with the number
    /// of distinct files whose uses resolve to *this* file, most used first.
    /// A name two files define is counted separately for each.
    pub used_by: HashMap<String, Vec<(String, usize)>>,
    /// Symbol name → the files whose raw mentions name it.
    ///
    /// The inversion of [`FileRecord::raw_refs`]. A name resolves in a file
    /// only if the workspace exports it and only if that file mentions it, so
    /// when a definer set moves this answers "which files can that have
    /// changed for" without reading one — the re-resolution pass visits these
    /// lists and the files that were re-parsed, and nothing else.
    pub ref_index: HashMap<String, Vec<String>>,
    /// Text an editor holds for a path, which supersedes the file on disk.
    ///
    /// The one consumer is the LSP server: a buffer with unsaved edits is the
    /// document the client is asking about, so both halves of an answer — the
    /// record, parsed from this text by [`Self::open_buffer`], and the
    /// identifier a position points at, which `source_of` reads through here
    /// — have to come from the buffer rather than from the file. Empty for
    /// every other caller, and an empty overlay changes nothing: a path not
    /// in it is read and hashed exactly as before.
    overlay: HashMap<String, String>,
    /// Root of the last refresh, so definition can read the identifier.
    root: std::path::PathBuf,
    /// `root` with symlinks resolved, so a path spelled through `/tmp` finds a
    /// root the process knows as `/private/tmp`. See [`Self::key`].
    canonical_root: std::path::PathBuf,
    /// What `[RECENT]` means for `root`; see [`recency`].
    recency: Option<recency::Recency>,
    /// What the set of paths says about scope; see [`scope`].
    layout: scope::Layout,
    /// What the lens walks, built on first use; see [`lens`].
    lens: lens::LensCache,
    /// Files that change together, from history; see [`cochange`]. Empty until
    /// a probe has answered.
    cochange: std::sync::Arc<CoChange>,
}

/// One refresh's file-level outcome, before the graph decision is folded in.
struct Absorption {
    parsed: usize,
    content_unchanged: usize,
    /// Whether anything that feeds `ranks`/`dependents`/`symbols` moved.
    graph_moved: bool,
    /// Whether what a file can *reach* moved: a path appeared, or a file's
    /// imports, re-exports or crate names changed. Every file's resolution
    /// can differ then, not only those that mention a moved name.
    scope_moved: bool,
    /// The paths that were re-parsed, so the symbol pass knows which files
    /// can have a resolution to redo beyond the ones the moved names reach.
    dirty: Vec<String>,
}

impl Atlas {
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut atlas = Self::default();
        atlas.refresh(root)?;
        Ok(atlas)
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
    /// file's `(exports, imports, re-exports, uses)`. [`RefreshStats`] reports
    /// which happened.
    ///
    /// This is [`Self::refresh_urgent`] with nothing urgent: the walk's own
    /// order is the parse order.
    pub fn refresh(&mut self, root: impl AsRef<Path>) -> std::io::Result<RefreshStats> {
        self.refresh_urgent(root, &[])
    }

    /// [`Self::refresh`], parsing `urgent` first.
    ///
    /// `urgent` names the files the caller is working in, in the order it
    /// wants them parsed — the engine's touched set, most recent first. The
    /// walk's order is otherwise kept, so the files it did not name come
    /// after. Parsing is spread over the cores in list order, so this hands
    /// the urgent files to the first threads instead of to whichever one
    /// happens to hold them. That is the whole of what order buys here: the
    /// threads run concurrently, and *finishing* first is theirs to decide,
    /// not the list's.
    pub fn refresh_urgent(
        &mut self,
        root: impl AsRef<Path>,
        urgent: &[String],
    ) -> std::io::Result<RefreshStats> {
        let root = root.as_ref();
        let listed = scan::list_files(root)?;
        Ok(self.refresh_listed(root, &listed, urgent, usize::MAX))
    }

    /// [`Self::refresh_urgent`] over a listing the caller already took.
    pub(crate) fn refresh_listed(
        &mut self,
        root: &Path,
        listed: &[scan::ListedFile],
        urgent: &[String],
        max_workers: usize,
    ) -> RefreshStats {
        self.root = root.to_path_buf();
        self.canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        // The git questions run while the files parse, once per root.
        let probe = (!self
            .recency
            .as_ref()
            .is_some_and(|known| known.is_for(root)))
        .then(|| recency::Probe::start(root));
        // A path with an open buffer is part of the workspace even when it is
        // not on disk yet, so it is kept rather than swept as a removal.
        let known: HashSet<String> = listed
            .iter()
            .map(|file| file.path.clone())
            .chain(self.overlay.keys().cloned())
            .collect();

        let absorbed = self.absorb(listed, &known, urgent, max_workers);
        match probe {
            Some(probe) => self.recency = Some(probe.finish()),
            None => self.note_changed(&absorbed.dirty),
        }
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
        let reresolved = self.finish(
            graph_recomputed,
            &absorbed.dirty,
            absorbed.scope_moved || removed > 0,
        );
        RefreshStats {
            parsed: absorbed.parsed,
            removed,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
            reresolved,
            walked: true,
        }
    }

    /// Whether a refresh over `listed` would change nothing: the same root,
    /// the same set of paths, no open buffer, and every file passing the
    /// metadata gate. Answers from `&self`, so a caller can ask before it
    /// pays for a private copy to update.
    pub(crate) fn is_current(&self, root: &Path, listed: &[scan::ListedFile]) -> bool {
        if self.root != root || !self.overlay.is_empty() || listed.len() != self.files.len() {
            return false;
        }
        // Every listed file has a record (a file without one is stale), so
        // equal counts of distinct paths mean no record is left over.
        let distinct: HashSet<&str> = listed.iter().map(|file| file.path.as_str()).collect();
        distinct.len() == self.files.len() && listed.iter().all(|file| !self.is_stale(file))
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
    ///
    /// The order of `paths` is the parse priority: the caller puts the files
    /// it is working in first. See [`Self::refresh_urgent`] for what that
    /// buys and what it does not.
    pub fn apply_changes(&mut self, paths: &[String]) -> std::io::Result<RefreshStats> {
        if self.root.as_os_str().is_empty() {
            // Nothing has been indexed, so an index-relative path has no root
            // to be relative to, and `"src/a.rs"` would be read against the
            // process's working directory instead.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "apply_changes needs a root: refresh first",
            ));
        }
        let mut listed = Vec::new();
        let mut removed = 0;
        let keys: Vec<String> = paths.iter().filter_map(|path| self.key(path)).collect();
        let paths = keys.as_slice();
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

        let absorbed = self.absorb(&listed, &known, paths, usize::MAX);
        self.note_changed(&absorbed.dirty);
        let graph_recomputed = absorbed.graph_moved || removed > 0;
        let reresolved = self.finish(
            graph_recomputed,
            &absorbed.dirty,
            absorbed.scope_moved || removed > 0,
        );
        Ok(RefreshStats {
            parsed: absorbed.parsed,
            removed,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
            reresolved,
            walked: false,
        })
    }

    /// Fold an editor's buffer for `path` in as that file's content, without
    /// reading or writing the disk.
    ///
    /// The overlay wins over the file for as long as it is open: the record is
    /// parsed from `source` and the graph is rebuilt if the parse moved its
    /// tuple, exactly as a re-parse of the file would do. A path the index has
    /// never carried is added, which is how a buffer for a file that was never
    /// saved joins the graph.
    ///
    /// This is a **synchronous** call on purpose. Its caller is the LSP server
    /// answering the notification that carried the text, and the very next
    /// request has to see it; putting it behind a channel would make the
    /// answer to `didOpen` a race with its own `documentSymbol`. The cost is
    /// one parse of one file, which is the cost a `didChange` already is.
    pub fn open_buffer(&mut self, path: &str, source: &str) -> std::io::Result<RefreshStats> {
        if self.root.as_os_str().is_empty() {
            // An index-relative path with no root would be read against the
            // process's working directory. See `apply_changes`.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "open_buffer needs a root: refresh first",
            ));
        }
        let listed = buffer_file(&self.root, path, source);
        let known: HashSet<String> = self
            .files
            .keys()
            .cloned()
            .chain(self.overlay.keys().cloned())
            .chain(std::iter::once(path.to_owned()))
            .collect();
        self.overlay.insert(path.to_owned(), source.to_owned());
        let absorbed = self.absorb(&[listed], &known, &[path.to_owned()], usize::MAX);
        self.note_changed(&absorbed.dirty);
        let graph_recomputed = absorbed.graph_moved;
        let reresolved = self.finish(graph_recomputed, &absorbed.dirty, absorbed.scope_moved);
        Ok(RefreshStats {
            parsed: absorbed.parsed,
            removed: 0,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
            reresolved,
            walked: false,
        })
    }

    /// Drop `path`'s buffer and put the index's view of it back to the file.
    ///
    /// A path with no buffer is a no-op: it reports the empty [`RefreshStats`]
    /// rather than re-reading a file nothing had overridden.
    pub fn close_buffer(&mut self, path: &str) -> std::io::Result<RefreshStats> {
        if self.overlay.remove(path).is_none() {
            return Ok(RefreshStats::default());
        }
        self.apply_changes(&[path.to_owned()])
    }

    /// The text of `path`: the buffer if one is open, otherwise the file.
    ///
    /// Two callers read a file's text to answer a question about a position in
    /// it — `definition` and `references` — and both would locate the wrong
    /// identifier if they read disk while the client is looking at an edited
    /// buffer.
    pub(crate) fn source_of(&self, path: &str) -> Option<String> {
        if let Some(source) = self.overlay.get(path) {
            return Some(source.clone());
        }
        fs::read_to_string(self.root.join(path)).ok()
    }

    /// Give the index the co-change partners history found. The lens reads
    /// them as edges from the next call on.
    pub fn set_cochange(&mut self, found: CoChange) {
        self.cochange = std::sync::Arc::new(found);
        self.lens = lens::LensCache::default();
    }

    /// The co-change partners the index holds.
    pub fn cochange(&self) -> &CoChange {
        &self.cochange
    }

    /// The index key of `path`: relative to the root, `/`-separated, without
    /// `.`/`..` segments — however the caller spelled it.
    ///
    /// A tool argument (`/abs/src/a.rs`, `./src/a.rs`), a diff path and an
    /// editor URI name one file three ways, and the index keys by exactly one.
    /// Everything that comes from outside goes through here before it is
    /// compared with a key or used as one, so a second spelling can neither
    /// create a duplicate record nor miss the file it means. `None` for a path
    /// that is empty or outside the root.
    pub fn key(&self, path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        let relative = if path.starts_with('/') {
            let spelled = Path::new(path);
            let rest = spelled
                .strip_prefix(&self.root)
                .or_else(|_| spelled.strip_prefix(&self.canonical_root))
                .map(Path::to_path_buf)
                .or_else(|_| {
                    // A symlinked spelling of a root the process knows by its
                    // real name: resolve what exists of the path and retry.
                    let real = resolve_existing(spelled);
                    real.strip_prefix(&self.canonical_root)
                        .map(Path::to_path_buf)
                })
                .ok()?;
            rest.to_str()?.to_owned()
        } else {
            path.to_owned()
        };
        let mut parts: Vec<&str> = Vec::new();
        for segment in relative.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                other => parts.push(other),
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// Whether the prompt map marks `path`, last written at `mtime`, recent.
    pub(crate) fn is_recent(&self, path: &str, mtime: SystemTime) -> bool {
        match &self.recency {
            Some(recency) => recency.is_recent(path, mtime),
            None => recency::by_clock(mtime),
        }
    }

    /// Files re-parsed after the recency set was read were changed since.
    fn note_changed(&mut self, dirty: &[String]) {
        if let Some(recency) = &mut self.recency {
            recency.note_changed(dirty);
        }
    }

    /// Whether `file`, as the walk lists it, has to be read again.
    fn is_stale(&self, file: &scan::ListedFile) -> bool {
        // An open buffer is always stale, whatever the clock says: its text is
        // the file as the editor has it, and the record it has to replace was
        // parsed from something else.
        if self.overlay.contains_key(&file.path) {
            return true;
        }
        // A file whose mtime is this fresh is not trusted by its metadata: the
        // filesystem's timestamp tick can swallow a second write inside it,
        // leaving size and mtime exactly as they were - the case the content
        // hash exists to settle, but the hash only runs once the file is read,
        // and this is the gate in front of that read. git's own index calls
        // this the racy-timestamp case and re-checks the file; so does this.
        // The cost is one re-read of whatever was written in the last couple
        // of seconds, and nothing at all for a settled tree. An mtime in the
        // future (a skewed clock) is no better: the elapsed age is an error
        // there, and that is not settled.
        let settled = file.mtime.elapsed().is_ok_and(|age| age >= RACY_MTIME);
        if !settled {
            return true;
        }
        let Some(record) = self.files.get(&file.path) else {
            return true;
        };
        if record.size != file.size || record.mtime != file.mtime {
            return true;
        }
        // The metadata matches, and that is still not enough. The file above
        // is settled *now*; the record may have been taken while it was not.
        // Sequence that metadata alone loses: write at T, walk at T+0.5 (racy,
        // so it is read; the record holds mtime T), a second same-length write
        // at T+0.9 inside the same tick (mtime still T), walk at T+3 - the
        // file has aged out, size and mtime match, and the second write is
        // never read by anything again. So the record answers for its own
        // clock: one taken inside the window is re-read on every walk until it
        // is replaced by one taken outside it, and the hash then decides
        // whether anything moved. That is at most one extra read for a file
        // that was just written, and none for a settled tree.
        record.mtime + RACY_MTIME > record.read_at
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
        urgent: &[String],
        max_workers: usize,
    ) -> Absorption {
        // Every path this update adds. It is the only way a specifier that
        // named nothing can come to name something — the candidates a specifier
        // carries do not move, only the file set does.
        let appeared: HashSet<String> = listed
            .iter()
            .filter(|file| !self.files.contains_key(&file.path))
            .map(|file| file.path.clone())
            .collect();
        let stale: Vec<&scan::ListedFile> =
            listed.iter().filter(|file| self.is_stale(file)).collect();
        let stale = prioritize(stale, urgent);
        let records = parse_batch(&stale, known, &self.files, &self.overlay, max_workers);

        // A parse can only move the graph by changing the tuple the graph is
        // built from. `exports` and the scope (`imports`, re-exports, crate
        // names) are compared exactly; the resolved uses are computed here
        // against the previous symbols — valid exactly when no `exports` and
        // no scope moved, which is what the first pass establishes — and
        // compared to the record's. Same tuple on every re-parse, same path
        // set, same graph.
        let mut graph_moved = false;
        let mut scope_moved = !appeared.is_empty();
        for (record, parsed) in &records {
            if !*parsed {
                continue;
            }
            match self.files.get(&record.path) {
                Some(old) if old.exports == record.exports && old.same_scope(record) => {}
                Some(old) => {
                    graph_moved = true;
                    scope_moved |= !old.same_scope(record);
                }
                None => graph_moved = true,
            }
        }
        if !graph_moved {
            let resolver = scope::Resolver {
                files: &self.files,
                symbols: &self.symbols,
                layout: &self.layout,
            };
            for (record, parsed) in &records {
                if !*parsed {
                    continue;
                }
                let resolved = resolver.resolve(record);
                if self
                    .files
                    .get(&record.path)
                    .is_some_and(|old| old.uses != resolved)
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
                .map(|old| (old.raw_refs.clone(), old.uses.clone()));
            if reparsed {
                parsed += 1;
                dirty.push(path.clone());
                if !graph_moved {
                    // The graph is not rebuilt, so this record must carry the
                    // resolved form the previous one did: nothing that decides
                    // a resolution moved, and the parse only produced raw
                    // mentions.
                    if let Some((_, resolved)) = &previous {
                        record.uses = resolved.clone();
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
        let arrived = self.resolve_new_imports(&appeared, known);
        graph_moved |= arrived;
        scope_moved |= arrived;
        Absorption {
            parsed,
            content_unchanged,
            graph_moved,
            scope_moved,
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
    fn finish(&mut self, graph_recomputed: bool, dirty: &[String], scope_moved: bool) -> usize {
        if !graph_recomputed {
            return 0;
        }
        let reresolved = self.index_symbols(dirty, scope_moved);
        // The lens index is a function of the same inputs as the ranks.
        self.lens = lens::LensCache::default();
        let (ranks, dependents) = graph::rank(&self.files, &self.layout);
        self.ranks = ranks;
        self.dependents = dependents;
        reresolved
    }

    /// Rebuild the symbol table, re-resolving only the files it can have
    /// changed, and count them.
    ///
    /// A mention resolves to one definer by scope — see [`scope`] — so a name
    /// several files export, `new` or `len` or `render_item`, is no longer
    /// all-or-nothing: each file that mentions it leans on the definer it can
    /// reach, and on none when it can reach several or none.
    ///
    /// The definer table is rebuilt from every file's `exports`, which is a
    /// pass over stored names and reads nothing. What that pass does *not*
    /// cost is the other half: re-resolving every file's identifiers. A file
    /// can only have a different resolution if it was re-parsed, if a name
    /// it mentions moved definers — [`Self::ref_index`] answers that without
    /// a scan — or if what files can reach moved (`scope_moved`), which is a
    /// changed import list or path set and re-resolves everyone. Everything
    /// else keeps the resolution it already had.
    fn index_symbols(&mut self, dirty: &[String], scope_moved: bool) -> usize {
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
        if scope_moved {
            self.layout = scope::Layout::build(&self.files, &self.root);
        }

        let symbols: HashMap<String, SymbolRecord> = definers
            .into_iter()
            .map(|(name, files)| (name, SymbolRecord { files, users: 0 }))
            .collect();

        let targets: Vec<String> = if scope_moved {
            self.files.keys().cloned().collect()
        } else {
            let mut targets: HashSet<String> = dirty.iter().cloned().collect();
            for name in &moved {
                if let Some(files) = self.ref_index.get(name) {
                    targets.extend(files.iter().cloned());
                }
            }
            targets.into_iter().collect()
        };
        let resolver = scope::Resolver {
            files: &self.files,
            symbols: &symbols,
            layout: &self.layout,
        };
        let mut reresolved = 0;
        let mut changed: Vec<(String, Vec<SymbolUse>)> = Vec::new();
        for path in targets {
            let Some(record) = self.files.get(&path) else {
                continue;
            };
            let resolved = resolver.resolve(record);
            reresolved += 1;
            if record.uses != resolved {
                changed.push((path, resolved));
            }
        }
        for (path, resolved) in changed {
            if let Some(record) = self.files.get_mut(&path) {
                record.uses = resolved;
            }
        }

        // Users are counted from the uses themselves, per definer: a file
        // resolves a name to one definer, so the per-definer counts of a name
        // add up to the name's distinct users.
        let mut symbols = symbols;
        let mut counts: HashMap<&str, HashMap<&str, usize>> = HashMap::new();
        for record in self.files.values() {
            for used in &record.uses {
                // A file that re-publishes the definer is the road to the name,
                // not a user of it.
                if record.reexports.contains(&used.definer) {
                    continue;
                }
                *counts
                    .entry(used.definer.as_str())
                    .or_default()
                    .entry(used.name.as_str())
                    .or_default() += 1;
                if let Some(symbol) = symbols.get_mut(&used.name) {
                    symbol.users += 1;
                }
            }
        }
        self.used_by = counts
            .into_iter()
            .map(|(definer, names)| {
                let mut names: Vec<(String, usize)> = names
                    .into_iter()
                    .map(|(name, users)| (name.to_owned(), users))
                    .collect();
                names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                (definer.to_owned(), names)
            })
            .collect();
        self.symbols = symbols;
        reresolved
    }

    /// The prompt map within `budget` estimated tokens; see [`skeleton`].
    pub fn skeleton(&self, budget: usize, pending: usize) -> String {
        project::skeleton(self, budget, pending)
    }

    pub fn project(&self, limit: usize) -> String {
        render(self, limit, &HashSet::new(), 0)
    }

    /// Projection biased toward files this session edited or read.
    pub fn project_with(&self, limit: usize, touched: &[String]) -> String {
        self.project_with_pending(limit, touched, 0)
    }

    /// [`Self::project_with`], with the background indexer's backlog named in
    /// the header. See [`render`].
    pub fn project_with_pending(&self, limit: usize, touched: &[String], pending: usize) -> String {
        let touched: HashSet<String> = touched.iter().filter_map(|path| self.key(path)).collect();
        render(self, limit, &touched, pending)
    }
}

/// The default `atlas.budget`, in tokens, for a repository of `files` source
/// files: what the map and the lens may add to a turn together. A small
/// repository needs less orientation than a large one, and the prompt cache
/// pays for every token, so it grows slowly.
pub fn default_budget(files: usize) -> usize {
    match files {
        0..200 => 800,
        200..2000 => 1000,
        _ => 1300,
    }
}

/// How a budget divides: the prompt map (sent once, rarely) gets 60%, the
/// per-prompt lens 40%. Returns `(map, lens)` in tokens.
pub fn split_budget(total: usize) -> (usize, usize) {
    let lens = total * 2 / 5;
    (total - lens, lens)
}

/// What `text` costs in prompt tokens, without a tokenizer: about one token per
/// 3.4 characters of code or paths. The budgets in this crate and the engine's
/// accounting both use it, so a block sized here is the size the engine sees.
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() as f64 / 3.4).ceil() as usize
}

/// `path` with the symlinks of its longest existing prefix resolved.
fn resolve_existing(path: &Path) -> std::path::PathBuf {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = fs::canonicalize(at) {
            return missing.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The files to parse, the caller's urgent ones first.
///
/// `urgent` is a priority order, not a set: the urgent files lead the batch in
/// the order the caller gave, and everything else keeps the order the walk or
/// the caller's path list had. The list is what [`parse_batch`] chunks, and
/// the chunks go to the threads in order, so urgent files are handed out at
/// the front of the batch. That is the whole of it: the threads run
/// concurrently and finish in whatever order the scheduler and the parser
/// decide, so this orders the *work handed out*, not the work completed. With
/// one file, or one core, it is the parse order.
fn prioritize<'a>(
    stale: Vec<&'a scan::ListedFile>,
    urgent: &[String],
) -> Vec<&'a scan::ListedFile> {
    if urgent.is_empty() {
        return stale;
    }
    let rank: HashMap<&str, usize> = urgent
        .iter()
        .enumerate()
        .map(|(position, path)| (path.as_str(), position))
        .collect();
    let mut stale = stale;
    // Every urgent file sorts by where the caller put it; the rest share one
    // key and a stable sort leaves them in the order they came in.
    stale.sort_by_key(|file| rank.get(file.path.as_str()).copied().unwrap_or(usize::MAX));
    stale
}

/// Parse threads for the first walk of a fresh index.
///
/// The first walk runs while the surface that started the engine is still
/// drawing its first frame, and a parse thread per core leaves that thread
/// competing for a core. On a ten-core host under load the first frame took
/// 111 ms with every core parsing and 51 ms with two left free, against 34 ms
/// with no index at all, while the index took no longer (docs/research/perf).
/// Later walks have no one waiting on a frame and use every core.
pub(crate) fn first_walk_workers() -> usize {
    first_walk_workers_for(
        std::thread::available_parallelism()
            .map(|cores| cores.get())
            .unwrap_or(1),
    )
}

/// [`first_walk_workers`] for a host with `cores` cores: two are left free
/// from eight cores upwards and one below that, and a single parser always
/// remains.
fn first_walk_workers_for(cores: usize) -> usize {
    let free = if cores >= 8 { 2 } else { 1 };
    cores.saturating_sub(free).max(1)
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
    overlay: &HashMap<String, String>,
    max_workers: usize,
) -> Vec<(FileRecord, bool)> {
    let workers = std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
        .min(max_workers)
        .min(stale.len());
    if workers <= 1 {
        return stale
            .iter()
            .map(|file| parse_one(file, known, previous.get(&file.path), overlay))
            .collect();
    }
    // Files are claimed one at a time from a shared counter, in list order,
    // rather than cut into one contiguous slice per thread: a slice that
    // happens to hold the few large files finishes long after the rest, and
    // the refresh waits for it. Claiming in order also keeps what `prioritize`
    // promises — the urgent files are the first ones taken.
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(file) = stale.get(at) else {
                            return done;
                        };
                        done.push((
                            at,
                            parse_one(file, known, previous.get(&file.path), overlay),
                        ));
                    }
                })
            })
            .collect();
        let mut slots: Vec<Option<(FileRecord, bool)>> = (0..stale.len()).map(|_| None).collect();
        for handle in handles {
            match handle.join() {
                Ok(done) => {
                    for (at, parsed) in done {
                        slots[at] = Some(parsed);
                    }
                }
                // A parser panic is a bug in this crate, not a file-level
                // condition to swallow: carry it to the caller's thread.
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
        slots.into_iter().flatten().collect()
    })
}

/// The [`scan::ListedFile`] shape of an editor buffer: the text is the file.
///
/// `size` is the text's, so a later walk that finds the file a different
/// length re-parses it, and `mtime` is now, which is later than any file the
/// buffer was opened over.
fn buffer_file(root: &Path, path: &str, source: &str) -> scan::ListedFile {
    scan::ListedFile {
        path: path.to_owned(),
        abs: root.join(path),
        size: source.len() as u64,
        mtime: SystemTime::now(),
    }
}

fn parse_one(
    file: &scan::ListedFile,
    known: &HashSet<String>,
    previous: Option<&FileRecord>,
    overlay: &HashMap<String, String>,
) -> (FileRecord, bool) {
    // A buffer needs no read: its text *is* the read, and the fingerprint over
    // it is the fingerprint of what the editor holds, so a later walk that
    // finds the file still different re-parses it.
    let disk;
    let source: &str = match overlay.get(&file.path) {
        Some(source) => source,
        None => {
            disk = fs::read_to_string(&file.abs).unwrap_or_default();
            &disk
        }
    };
    let hash = content::fingerprint(source.as_bytes());
    if let Some(old) = previous {
        // The pre-filter already said size or mtime moved. Equal size and
        // equal bytes is the case it cannot tell from a real edit: keep the
        // parse, move the clock. Never for a buffer — there the text is new
        // by the caller's declaration, not by a clock that could have been
        // reset.
        if overlay.get(&file.path).is_none() && old.size == file.size && old.hash == hash {
            let mut record = old.clone();
            record.mtime = file.mtime;
            // The bytes were read just now and agreed with the record, so the
            // clock that matters for the next walk is this one, not the one
            // the previous record was taken under.
            record.read_at = SystemTime::now();
            return (record, false);
        }
    }
    let language = Language::from_path(&file.path);
    let mut result = lang::parse(&file.path, source, known);
    let root = file
        .abs
        .ancestors()
        .nth(file.path.split('/').count())
        .unwrap_or(Path::new(""));
    reresolve::retry(root, &file.path, language, source, &mut result, known);
    let symbols = symbol::sites_for(
        std::mem::take(&mut result.symbols),
        &result.exports,
        &result.export_sites,
    );
    (
        FileRecord {
            language,
            path: file.path.clone(),
            exports: result.exports,
            symbols,
            export_sites: result.export_sites,
            imports: result.imports,
            unresolved_imports: result.unresolved_imports,
            unresolved_candidates: result.unresolved_candidates,
            syntax_errors: result.syntax_errors,
            reexports: result.reexports,
            inherits: result.inherits,
            external_uses: result.external_uses,
            // Resolved against the workspace's exports by `index_symbols`,
            // once the batch is in — a mention needs the whole file set to
            // find its definer, which one file's parse cannot see. Empty here
            // means "not resolved yet", and a parse the graph pass skips
            // carries the previous record's list instead.
            uses: Vec::new(),
            raw_refs: result.refs,
            size: file.size,
            mtime: file.mtime,
            read_at: SystemTime::now(),
            hash,
        },
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use std::collections::{HashMap, HashSet};

    use super::{first_walk_workers_for, parse_batch, prioritize, scan};

    fn listed(path: &str) -> scan::ListedFile {
        scan::ListedFile {
            path: path.to_owned(),
            abs: PathBuf::from(path),
            size: 1,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    fn order(files: Vec<&scan::ListedFile>) -> Vec<String> {
        files.into_iter().map(|file| file.path.clone()).collect()
    }

    /// The urgent files lead the batch in the caller's order, and the rest
    /// keep the order they came in: an engine that touched `d` last gets `d`
    /// handed to a thread before `b`, and both before the untouched tail.
    #[test]
    fn priority_leads_with_the_urgent_files_in_the_callers_order() {
        let stale = [
            listed("a.rs"),
            listed("b.rs"),
            listed("c.rs"),
            listed("d.rs"),
        ];
        let urgent = vec!["d.rs".to_owned(), "b.rs".to_owned()];
        let ordered = prioritize(stale.iter().collect(), &urgent);
        assert_eq!(order(ordered), vec!["d.rs", "b.rs", "a.rs", "c.rs"]);
    }

    #[test]
    fn without_an_urgent_file_the_list_is_left_alone() {
        let stale = [listed("a.rs"), listed("b.rs")];
        let ordered = prioritize(stale.iter().collect(), &[]);
        assert_eq!(order(ordered), vec!["a.rs", "b.rs"]);
    }

    /// A name the caller names but the batch does not contain — a path that
    /// vanished between the touch and the walk — does not disturb the rest.
    #[test]
    fn an_urgent_path_outside_the_batch_is_harmless() {
        let stale = [listed("a.rs"), listed("b.rs")];
        let urgent = vec!["gone.rs".to_owned(), "b.rs".to_owned()];
        let ordered = prioritize(stale.iter().collect(), &urgent);
        assert_eq!(order(ordered), vec!["b.rs", "a.rs"]);
    }

    /// However the files are shared out over the threads, the records come back
    /// in the order the files were given, one per file, each parsed from its own
    /// bytes — a record answering for the wrong path would corrupt the index
    /// silently. Sizes vary by two orders of magnitude so the threads finish in
    /// a different order than they started.
    #[test]
    fn a_parallel_parse_returns_one_record_per_file_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for at in 0..64 {
            let repeat = if at % 9 == 0 { 400 } else { 1 };
            let body: String = (0..repeat)
                .map(|n| format!("pub fn f_{at}_{n}() {{}}\n"))
                .collect();
            let path = format!("f{at}.rs");
            std::fs::write(dir.path().join(&path), &body).unwrap();
            files.push(scan::ListedFile {
                abs: dir.path().join(&path),
                path,
                size: body.len() as u64,
                mtime: SystemTime::UNIX_EPOCH,
            });
        }
        files.reverse();
        let stale: Vec<&scan::ListedFile> = files.iter().collect();
        let known: HashSet<String> = files.iter().map(|file| file.path.clone()).collect();

        let records = parse_batch(&stale, &known, &HashMap::new(), &HashMap::new(), usize::MAX);

        assert_eq!(records.len(), 64);
        for ((record, parsed), file) in records.iter().zip(&files) {
            assert_eq!(record.path, file.path);
            assert!(parsed, "{} was read from disk, so it was parsed", file.path);
            let at: &str = file.path.trim_start_matches('f').trim_end_matches(".rs");
            assert!(
                record
                    .exports
                    .iter()
                    .any(|name| name == &format!("f_{at}_0")),
                "{}: {:?}",
                file.path,
                record.exports
            );
        }
    }

    /// The first walk leaves cores to the thread that started it: always at
    /// least one worker, never all of the cores once there is more than one,
    /// and two cores kept back from eight upwards, where one is not enough to
    /// keep a busy host's terminal responsive (see docs/research/perf).
    #[test]
    fn the_first_walk_leaves_cores_to_its_starter() {
        let workers: Vec<usize> = [1, 2, 4, 6, 8, 10, 16]
            .into_iter()
            .map(first_walk_workers_for)
            .collect();
        assert_eq!(workers, [1, 1, 3, 5, 6, 8, 14]);
    }

    /// Capping the threads changes how the work is shared, never its result.
    #[test]
    fn a_capped_parse_returns_what_an_uncapped_one_does() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for at in 0..40 {
            let body: String = (0..=at % 7)
                .map(|n| format!("pub fn f_{at}_{n}() {{}}\n"))
                .collect();
            let path = format!("f{at}.rs");
            std::fs::write(dir.path().join(&path), &body).unwrap();
            files.push(scan::ListedFile {
                abs: dir.path().join(&path),
                path,
                size: body.len() as u64,
                mtime: SystemTime::UNIX_EPOCH,
            });
        }
        let stale: Vec<&scan::ListedFile> = files.iter().collect();
        let known: HashSet<String> = files.iter().map(|file| file.path.clone()).collect();
        let exports = |max_workers: usize| -> Vec<(String, Vec<String>)> {
            parse_batch(
                &stale,
                &known,
                &HashMap::new(),
                &HashMap::new(),
                max_workers,
            )
            .into_iter()
            .map(|(record, _)| (record.path, record.exports))
            .collect()
        };
        let all = exports(usize::MAX);
        assert_eq!(all.len(), 40);
        assert_eq!(exports(1), all);
        assert_eq!(exports(3), all);
    }
}
