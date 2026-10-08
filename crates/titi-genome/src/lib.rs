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
mod symbols;

pub use lang::{Capability, Language, Level};
pub use lsp::serve_lsp;
pub use project::render;
pub use scan::list_files;

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
    /// Tree-sitter `ERROR` nodes. Zero for languages without a grammar.
    /// A grammar that refuses the file counts as 1.
    pub syntax_errors: u32,
    /// Exported symbols defined elsewhere that this file mentions — the
    /// symbol-level half of the dependency graph.
    pub used_symbols: Vec<String>,
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
}

#[derive(Debug, Clone, Default)]
pub struct Genome {
    pub files: HashMap<String, FileRecord>,
    pub ranks: HashMap<String, f64>,
    pub dependents: HashMap<String, usize>,
    /// Symbol name → defining files and how many files reference it.
    pub symbols: HashMap<String, SymbolRecord>,
    /// Root of the last refresh, so definition can read the identifier.
    root: std::path::PathBuf,
}

/// One refresh's file-level outcome, before the graph decision is folded in.
struct Absorption {
    parsed: usize,
    content_unchanged: usize,
    /// Whether anything that feeds `ranks`/`dependents`/`symbols` moved.
    graph_moved: bool,
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
        let before = self.files.len();
        self.files.retain(|path, _| known.contains(path));
        let removed = before - self.files.len();
        let graph_recomputed = absorbed.graph_moved || removed > 0;
        self.finish(graph_recomputed);
        Ok(RefreshStats {
            parsed: absorbed.parsed,
            removed,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
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
                // `remove` answers both without a second question.
                None => removed += usize::from(self.files.remove(path).is_some()),
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
        self.finish(graph_recomputed);
        Ok(RefreshStats {
            parsed: absorbed.parsed,
            removed,
            total: self.files.len(),
            content_unchanged: absorbed.content_unchanged,
            graph_recomputed,
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
    fn absorb(&mut self, listed: &[scan::ListedFile], known: &HashSet<String>) -> Absorption {
        let candidates: Vec<&scan::ListedFile> = listed
            .iter()
            .filter(|file| {
                !self
                    .files
                    .get(&file.path)
                    .is_some_and(|record| record.size == file.size && record.mtime == file.mtime)
            })
            .collect();
        let records = parse_batch(&candidates, known, &self.files);

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
                Some(old)
                    if old.exports == record.exports && old.imports == record.imports => {}
                _ => graph_moved = true,
            }
        }
        if !graph_moved {
            for (record, parsed) in &records {
                if !*parsed {
                    continue;
                }
                let resolved = resolve(&record.used_symbols, &record.exports, &self.symbols);
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
        for (mut record, reparsed) in records {
            if reparsed {
                parsed += 1;
                if !graph_moved {
                    // The graph is not rebuilt, so this record must carry the
                    // resolved form the previous one did; the raw refs would
                    // be the only record left unresolved.
                    if let Some(old) = self.files.get(&record.path) {
                        record.used_symbols = old.used_symbols.clone();
                    }
                }
            } else {
                content_unchanged += 1;
            }
            self.files.insert(record.path.clone(), record);
        }
        Absorption {
            parsed,
            content_unchanged,
            graph_moved,
        }
    }

    /// Rebuild `symbols`, `ranks` and `dependents`, or leave them alone.
    ///
    /// A refresh that found nothing the graph is built from leaves all three
    /// maps exactly as they were: not "equal after a rebuild", the previous
    /// values, untouched.
    fn finish(&mut self, graph_recomputed: bool) {
        if !graph_recomputed {
            return;
        }
        self.index_symbols();
        let (ranks, dependents) = graph::rank(&self.files, &self.symbols);
        self.ranks = ranks;
        self.dependents = dependents;
    }

    /// Resolves each file's candidate identifiers against the symbols the
    /// workspace actually exports, then counts who uses what.
    ///
    /// Resolution is by name alone, so a name several files export — `is_empty`,
    /// `new`, `len` — cannot say which definition a mention refers to. Those
    /// names are ambiguous and carry no edges or user counts: counting them
    /// would make every file depend on every other file that happens to share
    /// a method name.
    fn index_symbols(&mut self) {
        let mut symbols: HashMap<String, SymbolRecord> = HashMap::new();
        for (path, record) in &self.files {
            for symbol in &record.exports {
                let entry = symbols.entry(symbol.clone()).or_insert(SymbolRecord {
                    files: Vec::new(),
                    users: 0,
                });
                entry.files.push(path.clone());
            }
        }
        if !symbols.values().any(|record| record.files.len() <= MAX_DEFINERS) {
            self.symbols = symbols;
            for record in self.files.values_mut() {
                record.used_symbols.clear();
            }
            return;
        }

        // A file's own exports are definitions, not uses of itself. Resolved
        // into a side list so the pass over `self.files` stays immutable.
        let mut usage: HashMap<String, HashSet<String>> = HashMap::new();
        let mut resolved: Vec<(String, Vec<String>)> = Vec::with_capacity(self.files.len());
        for (path, record) in &self.files {
            let used = resolve(&record.used_symbols, &record.exports, &symbols);
            for name in &used {
                usage.entry(name.clone()).or_default().insert(path.clone());
            }
            resolved.push((path.clone(), used));
        }
        for (path, used) in resolved {
            if let Some(record) = self.files.get_mut(&path) {
                record.used_symbols = used;
            }
        }
        for (name, record) in &mut symbols {
            record.files.sort();
            record.files.dedup();
            if let Some(users) = usage.get(name) {
                record.users = users.len();
            }
        }
        self.symbols = symbols;
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
            syntax_errors: result.syntax_errors,
            // Raw candidate identifiers; resolved against the whole repo once
            // every file has been parsed.
            used_symbols: result.refs,
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
