//! The questions a model, an editor or a person asks the index: where is this
//! defined, what is in this file, who uses it, what would changing it touch,
//! what is related to this.
//!
//! One query layer, several front ends. The prompt map and the lens answer
//! "what matters here" before the model asks; this answers the question it
//! asks next, from the same index, so the prompt can stay small and an answer
//! that needs more than the prompt carries is one call away. The `atlas` tool,
//! the LSP's `references` and `titi atlas` all read from here.
//!
//! Every function is a pure read of a published snapshot, except that
//! [`Atlas::uses`] reads the text of the files that use a name to say on which
//! line, which is exactly what a caller asking "who uses this" wants and what
//! the index does not keep.

use crate::lens::LensIndex;
use crate::role::tag;
use crate::{Atlas, Lens, SeedInput, SymbolKind, SymbolSite, estimate_tokens};

/// What a result list shows when the caller does not say.
pub const DEFAULT_LIMIT: usize = 20;
/// The most a result list shows, however much is asked.
pub const MAX_LIMIT: usize = 40;
/// Files whose use lines [`Atlas::uses`] reads.
const MAX_USER_FILES: usize = 25;
/// Lines shown per using file.
const LINES_PER_USER: usize = 3;
/// Longest source line quoted in a result, in characters.
const QUOTE_CHARS: usize = 110;
/// Co-change partners named in an impact report.
const PARTNERS: usize = 5;
/// Files named per group in an impact report.
const SAMPLE: usize = 12;

/// What `find` looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindKind {
    Any,
    Symbol,
    File,
}

/// A symbol and where it lives.
#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub path: String,
    pub name: String,
    pub kind: SymbolKind,
    pub line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub exported: bool,
    /// Distinct files that depend on the file it is in.
    pub dependents: usize,
    pub tag: Option<&'static str>,
}

/// A file a query matched.
#[derive(Debug, Clone)]
pub struct FileHit {
    pub path: String,
    pub dependents: usize,
    pub tag: Option<&'static str>,
}

/// The answer to `find`: the best matches first, and how many there were.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub symbols: Vec<SymbolHit>,
    pub files: Vec<FileHit>,
    pub total_symbols: usize,
    pub total_files: usize,
}

/// A file's shape.
#[derive(Debug, Clone)]
pub struct Outline {
    pub path: String,
    pub language: &'static str,
    pub tag: Option<&'static str>,
    pub symbols: Vec<SymbolSite>,
    pub imports: Vec<String>,
    pub dependents: Vec<String>,
    pub tests: Vec<String>,
}

/// One file that uses a name, with the lines that do.
#[derive(Debug, Clone)]
pub struct User {
    pub path: String,
    pub mentions: usize,
    pub lines: Vec<(u32, String)>,
}

/// Who uses a name.
#[derive(Debug, Clone, Default)]
pub struct Uses {
    pub name: String,
    pub definitions: Vec<SymbolHit>,
    pub users: Vec<User>,
    /// Users beyond the files read for their lines.
    pub more_users: usize,
}

/// What a change would touch.
#[derive(Debug, Clone, Default)]
pub struct Impact {
    pub target: String,
    /// The files the target is in or names: one for a file, the definers of a
    /// symbol.
    pub definers: Vec<String>,
    pub symbol: Option<String>,
    /// Files that depend on it directly.
    pub direct: Vec<String>,
    /// Files that depend on those, not counting the ones above.
    pub indirect: Vec<String>,
    pub tests: Vec<String>,
    pub cochange: Vec<(String, f32)>,
}

/// Why a target could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    /// Nothing in the index is called that; the closest names, if any.
    NotFound { what: String, closest: Vec<String> },
    /// The path names several files.
    Ambiguous {
        what: String,
        candidates: Vec<String>,
    },
}

impl std::fmt::Display for AskError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { what, closest } if closest.is_empty() => {
                write!(out, "no symbol or file named `{what}` in the index")
            }
            Self::NotFound { what, closest } => write!(
                out,
                "no symbol or file named `{what}`; closest: {}",
                closest.join(", ")
            ),
            Self::Ambiguous { what, candidates } => write!(
                out,
                "`{what}` names several files; say which: {}",
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for AskError {}

impl Atlas {
    fn symbol_hit(&self, path: &str, symbol: &SymbolSite) -> SymbolHit {
        SymbolHit {
            path: path.to_owned(),
            name: symbol.name.clone(),
            kind: symbol.kind,
            line: symbol.span.0,
            end_line: symbol.span.1,
            signature: symbol.signature.clone(),
            exported: symbol.exported,
            dependents: self.dependents.get(path).copied().unwrap_or(0),
            tag: tag(path),
        }
    }

    /// Symbols and files matching `query`, best first.
    ///
    /// A symbol matches by its exact name, by its name ignoring case, or by
    /// containing a term of the query (three characters or more), in that
    /// order of preference; ties go to exported symbols and to files more of
    /// the repository depends on. A file matches by its stem, its name or its
    /// path. `limit` (at most [`MAX_LIMIT`]) and `offset` page both lists.
    pub fn find(&self, query: &str, kind: FindKind, limit: usize, offset: usize) -> Found {
        let limit = limit.clamp(1, MAX_LIMIT);
        let terms: Vec<String> = query
            .split(|ch: char| ch.is_whitespace() || ch == ',')
            .map(|term| term.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_'))
            .filter(|term| term.len() >= 2)
            .map(str::to_owned)
            .collect();
        let mut found = Found::default();
        if terms.is_empty() {
            return found;
        }
        if kind != FindKind::File {
            let mut scored: Vec<(u32, SymbolHit)> = Vec::new();
            for (path, record) in &self.files {
                for symbol in &record.symbols {
                    let lower = symbol.name.to_lowercase();
                    let mut score = 0;
                    for term in &terms {
                        score += if symbol.name == *term {
                            30
                        } else if lower == term.to_lowercase() {
                            20
                        } else if term.len() >= 3 && lower.contains(&term.to_lowercase()) {
                            10
                        } else {
                            0
                        };
                    }
                    if score > 0 {
                        scored.push((score, self.symbol_hit(path, symbol)));
                    }
                }
            }
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.1.exported.cmp(&a.1.exported))
                    .then_with(|| b.1.dependents.cmp(&a.1.dependents))
                    .then_with(|| a.1.path.cmp(&b.1.path))
                    .then_with(|| a.1.line.cmp(&b.1.line))
            });
            found.total_symbols = scored.len();
            found.symbols = scored
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|(_, hit)| hit)
                .collect();
        }
        if kind != FindKind::Symbol {
            let mut scored: Vec<(u32, FileHit)> = Vec::new();
            for path in self.files.keys() {
                if path.contains(['<', '>']) {
                    continue;
                }
                let lower = path.to_lowercase();
                let basename = lower.rsplit('/').next().unwrap_or(&lower);
                let stem = basename.rsplit_once('.').map_or(basename, |(stem, _)| stem);
                let mut score = 0;
                for term in &terms {
                    let term = term.to_lowercase();
                    score += if stem == term {
                        30
                    } else if basename.contains(&term) {
                        20
                    } else if lower.contains(&term) {
                        10
                    } else {
                        0
                    };
                }
                if score > 0 {
                    scored.push((
                        score,
                        FileHit {
                            path: path.clone(),
                            dependents: self.dependents.get(path).copied().unwrap_or(0),
                            tag: tag(path),
                        },
                    ));
                }
            }
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.1.dependents.cmp(&a.1.dependents))
                    .then_with(|| a.1.path.cmp(&b.1.path))
            });
            found.total_files = scored.len();
            found.files = scored
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|(_, hit)| hit)
                .collect();
        }
        found
    }

    /// What is in `path`: every symbol with its extent and signature, what it
    /// imports, what depends on it and which tests are among those.
    pub fn outline(&self, path: &str) -> Result<Outline, AskError> {
        let at = self.resolve_one(path)?;
        let index = self.lens_index();
        let key = index.paths[at as usize].clone();
        let record = &self.files[&key];
        let dependents = self.dependents_of(index, at);
        let mut tests: Vec<String> = dependents
            .iter()
            .filter(|path| tag(path) == Some("test"))
            .cloned()
            .collect();
        tests.extend(self.paired_tests(index, &[at]));
        tests.sort();
        tests.dedup();
        Ok(Outline {
            path: key,
            language: record.language.name(),
            tag: tag(&record.path),
            symbols: record.symbols.clone(),
            imports: record.imports.clone(),
            dependents,
            tests,
        })
    }

    /// Who uses `name`: its definitions, and the files whose uses resolve to
    /// them with the lines that mention it. `in_file` narrows to the one
    /// definition in that file when several files define the name.
    pub fn uses(&self, name: &str, in_file: Option<&str>) -> Result<Uses, AskError> {
        let definers = self.definers_of(name, in_file)?;
        let mut report = Uses {
            name: name.to_owned(),
            ..Uses::default()
        };
        for path in &definers {
            if let Some(record) = self.files.get(path) {
                for symbol in record.symbols.iter().filter(|s| s.name == name) {
                    report.definitions.push(self.symbol_hit(path, symbol));
                }
            }
        }
        let mut users: Vec<&String> = self
            .files
            .iter()
            .filter(|(_, record)| {
                record
                    .uses
                    .iter()
                    .any(|used| used.name == name && definers.contains(&used.definer))
            })
            .map(|(path, _)| path)
            .collect();
        users.sort();
        report.more_users = users.len().saturating_sub(MAX_USER_FILES);
        for path in users.into_iter().take(MAX_USER_FILES) {
            let lines = self
                .source_of(path)
                .map(|source| mentions(&source, name))
                .unwrap_or_default();
            report.users.push(User {
                path: path.clone(),
                mentions: lines.len(),
                lines: lines.into_iter().take(LINES_PER_USER).collect(),
            });
        }
        Ok(report)
    }

    /// Every line of every user of `name` that mentions it, as positions: the
    /// LSP's `references`.
    pub fn use_sites(&self, name: &str, in_file: Option<&str>) -> Vec<(String, u32, u32)> {
        let Ok(definers) = self.definers_of(name, in_file) else {
            return Vec::new();
        };
        let mut users: Vec<&String> = self
            .files
            .iter()
            .filter(|(_, record)| {
                record
                    .uses
                    .iter()
                    .any(|used| used.name == name && definers.contains(&used.definer))
            })
            .map(|(path, _)| path)
            .collect();
        users.sort();
        let mut out = Vec::new();
        for path in users {
            if let Some(source) = self.source_of(path) {
                for (line, column) in mention_positions(&source, name) {
                    out.push((path.clone(), line, column));
                }
            }
        }
        out
    }

    /// What changing `target` — a path, or a symbol name — would touch.
    pub fn impact(&self, target: &str) -> Result<Impact, AskError> {
        let index = self.lens_index();
        let (definers, symbol): (Vec<u32>, Option<String>) = match self.resolve_one(target) {
            Ok(at) => (vec![at], None),
            Err(AskError::Ambiguous { what, candidates }) => {
                return Err(AskError::Ambiguous { what, candidates });
            }
            Err(AskError::NotFound { .. }) => {
                let paths = self.definers_of(target, None)?;
                let ats: Vec<u32> = paths
                    .iter()
                    .filter_map(|path| index.at.get(path).copied())
                    .collect();
                (ats, Some(target.to_owned()))
            }
        };
        let mut report = Impact {
            target: target.to_owned(),
            definers: definers
                .iter()
                .map(|at| index.paths[*at as usize].clone())
                .collect(),
            symbol: symbol.clone(),
            ..Impact::default()
        };
        let mut direct: Vec<u32> = Vec::new();
        for &at in &definers {
            match &symbol {
                None => {
                    for (neighbour, _, kind) in &index.adj[at as usize] {
                        if *kind == crate::lens::EdgeKind::UsedBy && !direct.contains(neighbour) {
                            direct.push(*neighbour);
                        }
                    }
                }
                Some(name) => {
                    let definer = &index.paths[at as usize];
                    for (path, record) in &self.files {
                        if record
                            .uses
                            .iter()
                            .any(|used| used.name == *name && used.definer == *definer)
                            && let Some(&user) = index.at.get(path)
                            && !direct.contains(&user)
                        {
                            direct.push(user);
                        }
                    }
                }
            }
        }
        direct.retain(|at| !definers.contains(at));
        let mut indirect: Vec<u32> = Vec::new();
        for &at in &direct {
            for (neighbour, _, kind) in &index.adj[at as usize] {
                if *kind == crate::lens::EdgeKind::UsedBy
                    && !direct.contains(neighbour)
                    && !definers.contains(neighbour)
                    && !indirect.contains(neighbour)
                {
                    indirect.push(*neighbour);
                }
            }
        }
        let direct_at = direct.clone();
        let name_of = |at: &u32| index.paths[*at as usize].clone();
        let mut direct: Vec<String> = direct.iter().map(name_of).collect();
        let mut indirect: Vec<String> = indirect.iter().map(name_of).collect();
        direct.sort();
        indirect.sort();
        report.tests = direct
            .iter()
            .chain(indirect.iter())
            .filter(|path| tag(path) == Some("test"))
            .cloned()
            .collect();
        let touched: Vec<u32> = definers.iter().chain(&direct_at).copied().collect();
        report.tests.extend(self.paired_tests(index, &touched));
        report.tests.sort();
        report.tests.dedup();
        report.direct = direct;
        report.indirect = indirect;
        for path in &report.definers {
            for (partner, weight) in self.cochange().partners(path).iter().take(PARTNERS) {
                if !report.cochange.iter().any(|(known, _)| known == partner) {
                    report.cochange.push((partner.clone(), *weight));
                }
            }
        }
        report
            .cochange
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        report.cochange.truncate(PARTNERS);
        Ok(report)
    }

    /// The lens for an arbitrary query, not a prompt: where to look next.
    pub fn related(&self, query: &str, files: usize) -> Lens {
        let seeds = self.seeds(&SeedInput {
            prompt: query,
            ..SeedInput::default()
        });
        self.lens(
            &seeds,
            crate::LensBudget {
                tokens: 4000,
                files: files.clamp(1, MAX_LIMIT),
            },
        )
    }

    /// The one file `what` names: a key in any spelling, or a path suffix that
    /// only one file ends with.
    fn resolve_one(&self, what: &str) -> Result<u32, AskError> {
        let index = self.lens_index();
        let hits = index.resolve_path_public(self, what);
        match hits.as_slice() {
            [one] => Ok(*one),
            [] => Err(AskError::NotFound {
                what: what.to_owned(),
                closest: self.closest_paths(what),
            }),
            many => Err(AskError::Ambiguous {
                what: what.to_owned(),
                candidates: many
                    .iter()
                    .map(|at| index.paths[*at as usize].clone())
                    .collect(),
            }),
        }
    }

    /// The files defining `name`; those in `in_file` when it is given.
    fn definers_of(&self, name: &str, in_file: Option<&str>) -> Result<Vec<String>, AskError> {
        let mut definers: Vec<String> = self
            .symbols
            .get(name)
            .map(|record| record.files.clone())
            .unwrap_or_default();
        if definers.is_empty() {
            // A symbol the file keeps but does not export has no entry in the
            // table; the per-file symbols still know it.
            definers = self
                .files
                .iter()
                .filter(|(_, record)| record.symbols.iter().any(|s| s.name == name))
                .map(|(path, _)| path.clone())
                .collect();
            definers.sort();
        }
        if let Some(file) = in_file
            && let Some(key) = self.key(file)
        {
            definers.retain(|path| *path == key);
        }
        if definers.is_empty() {
            return Err(AskError::NotFound {
                what: name.to_owned(),
                closest: self.closest_names(name),
            });
        }
        Ok(definers)
    }

    /// The tests that pair with any of `files` by naming.
    fn paired_tests(&self, index: &LensIndex, files: &[u32]) -> Vec<String> {
        files
            .iter()
            .flat_map(|&at| index.adj[at as usize].iter())
            .filter(|(_, _, kind)| *kind == crate::lens::EdgeKind::TestedBy)
            .map(|(test, _, _)| index.paths[*test as usize].clone())
            .collect()
    }

    fn dependents_of(&self, index: &LensIndex, at: u32) -> Vec<String> {
        let mut out: Vec<String> = index.adj[at as usize]
            .iter()
            .filter(|(_, _, kind)| *kind == crate::lens::EdgeKind::UsedBy)
            .map(|(neighbour, _, _)| index.paths[*neighbour as usize].clone())
            .collect();
        out.sort();
        out
    }

    /// Symbol names near `name`: the same ignoring case or sharing a prefix.
    fn closest_names(&self, name: &str) -> Vec<String> {
        let lower = name.to_lowercase();
        let mut names: Vec<&String> = self
            .symbols
            .keys()
            .filter(|known| {
                let known = known.to_lowercase();
                known == lower
                    || (lower.len() >= 4
                        && (known.starts_with(&lower) || lower.starts_with(&known)))
            })
            .collect();
        names.sort();
        names.into_iter().take(5).cloned().collect()
    }

    fn closest_paths(&self, what: &str) -> Vec<String> {
        let basename = what.rsplit('/').next().unwrap_or(what).to_lowercase();
        let mut paths: Vec<&String> = self
            .files
            .keys()
            .filter(|path| {
                !basename.is_empty()
                    && path.rsplit('/').next().is_some_and(|name| {
                        let name = name.to_lowercase();
                        name.contains(&basename) || basename.contains(&name)
                    })
            })
            .collect();
        paths.sort();
        paths.into_iter().take(5).cloned().collect()
    }
}

/// Lines of `source` that mention `name` as a whole word, 1-based, each cut to
/// a quotable length.
fn mentions(source: &str, name: &str) -> Vec<(u32, String)> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| whole_word(line, name).is_some())
        .map(|(at, line)| {
            let line = line.trim();
            let quoted: String = line.chars().take(QUOTE_CHARS).collect();
            let quoted = if line.chars().count() > QUOTE_CHARS {
                format!("{quoted}…")
            } else {
                quoted
            };
            (at as u32 + 1, quoted)
        })
        .collect()
}

/// `(line, byte column)` of every whole-word mention of `name`, 1-based lines.
fn mention_positions(source: &str, name: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (at, line) in source.lines().enumerate() {
        let mut from = 0;
        while let Some(found) = whole_word(&line[from..], name) {
            out.push((at as u32 + 1, (from + found) as u32));
            from += found + name.len();
        }
    }
    out
}

/// Byte offset of the first whole-word occurrence of `word` in `line`.
fn whole_word(line: &str, word: &str) -> Option<usize> {
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(found) = line[from..].find(word) {
        let start = from + found;
        let end = start + word.len();
        let before = start == 0 || !is_ident(bytes[start - 1]);
        let after = end >= bytes.len() || !is_ident(bytes[end]);
        if before && after {
            return Some(start);
        }
        from = start + 1;
        if from >= line.len() {
            break;
        }
    }
    None
}

// ---- rendering --------------------------------------------------------------

fn tag_text(tag: Option<&str>) -> String {
    tag.map_or(String::new(), |label| format!(" [{label}]"))
}

fn symbol_line(hit: &SymbolHit) -> String {
    let what = hit.signature.clone().unwrap_or_else(|| {
        let word = hit.kind.word();
        if word.is_empty() {
            hit.name.clone()
        } else {
            format!("{word} {}", hit.name)
        }
    });
    format!(
        "{}:{}  {what}  (→{}){}",
        hit.path,
        hit.line,
        hit.dependents,
        tag_text(hit.tag)
    )
}

fn list(title: &str, items: &[String], total: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = format!("{title} ({total}):\n");
    for item in items.iter().take(SAMPLE) {
        out.push_str("  ");
        out.push_str(item);
        out.push('\n');
    }
    if total > SAMPLE.min(items.len()) {
        out.push_str(&format!(
            "  … and {} more\n",
            total - SAMPLE.min(items.len())
        ));
    }
    out
}

impl Found {
    pub fn render(&self, offset: usize) -> String {
        if self.symbols.is_empty() && self.files.is_empty() {
            return "nothing in the index matches\n".to_owned();
        }
        let mut out = String::new();
        if !self.symbols.is_empty() {
            out.push_str(&format!(
                "symbols ({}–{} of {}):\n",
                offset + 1,
                offset + self.symbols.len(),
                self.total_symbols
            ));
            for hit in &self.symbols {
                out.push_str("  ");
                out.push_str(&symbol_line(hit));
                out.push('\n');
            }
        }
        if !self.files.is_empty() {
            out.push_str(&format!(
                "files ({}–{} of {}):\n",
                offset + 1,
                offset + self.files.len(),
                self.total_files
            ));
            for hit in &self.files {
                out.push_str(&format!(
                    "  {} (→{}){}\n",
                    hit.path,
                    hit.dependents,
                    tag_text(hit.tag)
                ));
            }
        }
        let shown = offset + self.symbols.len().max(self.files.len());
        if shown < self.total_symbols.max(self.total_files) {
            out.push_str(&format!("more: pass offset={shown}\n"));
        }
        out
    }
}

impl Outline {
    pub fn render(&self) -> String {
        let mut out = format!(
            "{}  {}  (→{}){}\n",
            self.path,
            self.language,
            self.dependents.len(),
            tag_text(self.tag)
        );
        for symbol in &self.symbols {
            let what = symbol.signature.clone().unwrap_or_else(|| {
                let word = symbol.kind.word();
                if word.is_empty() {
                    symbol.name.clone()
                } else {
                    format!("{word} {}", symbol.name)
                }
            });
            let extent = if symbol.span.0 == symbol.span.1 {
                format!("{}", symbol.span.0)
            } else {
                format!("{}-{}", symbol.span.0, symbol.span.1)
            };
            let private = if symbol.exported { "" } else { "  (private)" };
            out.push_str(&format!("  {extent:>9}  {what}{private}\n"));
        }
        out.push_str(&list("imports", &self.imports, self.imports.len()));
        out.push_str(&list("used by", &self.dependents, self.dependents.len()));
        out.push_str(&list("tests among them", &self.tests, self.tests.len()));
        out
    }
}

impl Uses {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for hit in &self.definitions {
            out.push_str(&format!("defined: {}\n", symbol_line(hit)));
        }
        if self.definitions.len() > 1 {
            out.push_str(&format!(
                "note: {} files define `{}`; users are counted per definition\n",
                self.definitions.len(),
                self.name
            ));
        }
        if self.users.is_empty() {
            out.push_str("no file in the index uses it\n");
            return out;
        }
        out.push_str(&format!(
            "used by {} files:\n",
            self.users.len() + self.more_users
        ));
        for user in &self.users {
            out.push_str(&format!("  {} ({} lines)\n", user.path, user.mentions));
            for (line, text) in &user.lines {
                out.push_str(&format!("    {line}: {text}\n"));
            }
        }
        if self.more_users > 0 {
            out.push_str(&format!("  … and {} more files\n", self.more_users));
        }
        out
    }
}

impl Impact {
    pub fn render(&self) -> String {
        let what = match &self.symbol {
            Some(name) => format!("`{name}` (defined in {})", self.definers.join(", ")),
            None => self.definers.join(", "),
        };
        let mut out = format!("changing {what} touches:\n");
        if self.direct.is_empty() && self.indirect.is_empty() {
            out.push_str("  nothing else in the index depends on it\n");
        }
        out.push_str(&list("direct dependents", &self.direct, self.direct.len()));
        out.push_str(&list(
            "then, through those",
            &self.indirect,
            self.indirect.len(),
        ));
        out.push_str(&list("tests among them", &self.tests, self.tests.len()));
        if !self.cochange.is_empty() {
            out.push_str("edited together in history:\n");
            for (path, weight) in &self.cochange {
                out.push_str(&format!("  {path} ({weight:.2})\n"));
            }
        }
        out
    }
}

impl Lens {
    /// The lens as plain lines, with no block around them.
    pub fn render_lines(&self) -> String {
        let block = self.render(0);
        let mut lines: Vec<&str> = block.lines().collect();
        lines.retain(|line| !line.starts_with("<atlas-lens") && *line != "</atlas-lens>");
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

/// Cut a result to `budget` estimated tokens at a line boundary and say so.
pub fn clip_to_tokens(text: &str, budget: usize) -> String {
    if estimate_tokens(text) <= budget {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut kept = 0;
    for line in text.lines() {
        let cost = estimate_tokens(line) + 1;
        if kept + cost > budget {
            break;
        }
        kept += cost;
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("… truncated; narrow the query or page with offset\n");
    out
}
