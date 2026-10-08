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

/// What one [`Genome::refresh`] actually did.
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

impl Genome {
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut genome = Self::default();
        genome.refresh(root)?;
        Ok(genome)
    }

    /// Re-walk `root` and re-parse only the files that moved.
    ///
    /// The **fallback** path: it walks the tree, so it catches files no caller
    /// mentioned, and it is what the engine's per-turn call uses. A file is
    /// examined only when its `size` or `mtime` differs from the recorded one
    /// — a `stat` per file and no read — and a file that passes that gate is
    /// read, hashed and, if the bytes match, kept without being parsed; see
    /// [`FileRecord::hash`].
    ///
    /// `ranks`, `dependents` and `symbols` are rebuilt every time (it is cheap
    /// relative to parsing).
    pub fn refresh(&mut self, root: impl AsRef<Path>) -> std::io::Result<RefreshStats> {
        let root = root.as_ref();
        self.root = root.to_path_buf();
        let listed = scan::list_files(root)?;
        let known: HashSet<String> = listed.iter().map(|file| file.path.clone()).collect();
        let stale: Vec<&scan::ListedFile> = listed
            .iter()
            .filter(|file| {
                !self
                    .files
                    .get(&file.path)
                    .is_some_and(|record| record.size == file.size && record.mtime == file.mtime)
            })
            .collect();
        let mut parsed = 0;
        let mut content_unchanged = 0;
        for (record, was_parsed) in parse_batch(&stale, &known, &self.files) {
            if was_parsed {
                parsed += 1;
            } else {
                content_unchanged += 1;
            }
            self.files.insert(record.path.clone(), record);
        }

        let before = self.files.len();
        self.files.retain(|path, _| known.contains(path));
        let removed = before - self.files.len();

        self.index_symbols();
        let (ranks, dependents) = graph::rank(&self.files, &self.symbols);
        self.ranks = ranks;
        self.dependents = dependents;
        Ok(RefreshStats {
            parsed,
            removed,
            total: self.files.len(),
            content_unchanged,
        })
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
            let own: HashSet<&str> = record.exports.iter().map(String::as_str).collect();
            let used: Vec<String> = record
                .used_symbols
                .iter()
                .filter(|name| {
                    symbols
                        .get(*name)
                        .is_some_and(|symbol| symbol.files.len() <= MAX_DEFINERS)
                        && !own.contains(name.as_str())
                })
                .cloned()
                .collect();
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
