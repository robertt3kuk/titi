//! Ranked workspace map: files, symbols, dependency edges, PageRank, prompt
//! projection.
//!
//! The graph has two kinds of edge: a file imports another file, and a file
//! references a symbol another file defines. Both feed the same ranking and
//! the same `(→N)` dependent count, so "what depends on this" answers in
//! symbol terms instead of only file terms.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E3).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

mod graph;
mod parse;
mod project;
mod scan;
mod symbols;

pub use parse::Language;
pub use project::render;
pub use scan::list_files;

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: String,
    pub language: Language,
    pub exports: Vec<String>,
    pub imports: Vec<String>,
    /// Exported symbols defined elsewhere that this file mentions — the
    /// symbol-level half of the dependency graph.
    pub used_symbols: Vec<String>,
    pub size: u64,
    pub mtime: SystemTime,
}

/// A name exported by more than this many files is ambiguous under name-only
/// resolution, so it carries neither edges nor a user count. One definition
/// site means a mention can only mean that file.
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
    /// Files re-read and re-parsed (new or changed).
    pub parsed: usize,
    /// Files dropped because they vanished from disk.
    pub removed: usize,
    /// Files in the index afterwards.
    pub total: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Genome {
    pub files: HashMap<String, FileRecord>,
    pub ranks: HashMap<String, f64>,
    pub dependents: HashMap<String, usize>,
    /// Symbol name → defining files and how many files reference it.
    pub symbols: HashMap<String, SymbolRecord>,
}

impl Genome {
    pub fn index(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut genome = Self::default();
        genome.refresh(root)?;
        Ok(genome)
    }

    /// Re-walk `root` and re-parse only the files whose size or mtime moved.
    /// Removed files drop out; ranks are recomputed every time (it is cheap
    /// relative to parsing).
    pub fn refresh(&mut self, root: impl AsRef<Path>) -> std::io::Result<RefreshStats> {
        let root = root.as_ref();
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
        for record in parse_batch(&stale, &known) {
            self.files.insert(record.path.clone(), record);
            parsed += 1;
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
        let definers: HashSet<String> = symbols
            .iter()
            .filter(|(_, record)| record.files.len() <= MAX_DEFINERS)
            .map(|(name, _)| name.clone())
            .collect();
        if definers.is_empty() {
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
                .filter(|name| definers.contains(*name) && !own.contains(name.as_str()))
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

/// Parses the stale files, spreading them over the available cores.
///
/// Parsing is the only expensive step of a refresh and every file is
/// independent of the others — they share nothing but the read-only set of
/// known paths — so the work splits cleanly. A refresh that touches one file
/// stays on the calling thread.
fn parse_batch(stale: &[&scan::ListedFile], known: &HashSet<String>) -> Vec<FileRecord> {
    let workers = std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
        .min(stale.len());
    if workers <= 1 {
        return stale.iter().map(|file| parse_one(file, known)).collect();
    }
    let chunk = stale.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = stale
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|file| parse_one(file, known))
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

fn parse_one(file: &scan::ListedFile, known: &HashSet<String>) -> FileRecord {
    let source = fs::read_to_string(&file.abs).unwrap_or_default();
    let result = parse::parse(&file.path, &source, known);
    FileRecord {
        language: Language::from_path(&file.path),
        path: file.path.clone(),
        exports: result.exports,
        imports: result.imports,
        // Resolved against the whole repo once every file has been parsed.
        used_symbols: result.refs,
        size: file.size,
        mtime: file.mtime,
    }
}
