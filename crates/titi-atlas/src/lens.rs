//! The lens: the part of the repository this task is about.
//!
//! The prompt map answers "what matters in this repository", once, the same
//! for every question. The lens answers "what matters for *this* one": it reads
//! the user's text, the files the session has touched and the working tree's
//! changes for **seeds**, spreads their weight over the dependency graph with
//! a personalised PageRank, and lists the few files where that weight lands,
//! each with the reason it is there.
//!
//! No model is involved and nothing is fetched: seeds are matched against
//! indexes built from the records the atlas already holds, and the walk is 20
//! passes over the edge list. A prompt with no signal produces no lens at all,
//! and the prompt map stands alone.
//!
//! What a seed is, and how much it weighs:
//!
//! | seed | weight |
//! |---|---|
//! | a file named in the prompt (`src/a.rs`, `a.rs:42`, a trace frame) | 5 |
//! | an identifier the prompt spells like an exported symbol, exactly | 3 (shared by its definers) |
//! | the same ignoring case, 5+ characters | 1.5 |
//! | a prompt word that is a file stem / a path component | 2 / 0.5 |
//! | a file the session touched, newest first | 2, decaying |
//! | a file the working tree changed | 2 |
//!
//! A dependency edge carries weight both ways — a change flows to what a file
//! uses and to what uses it — the reverse at half. Measured on replayed
//! commits (`docs/research/atlas-v2`), dropping the reverse edge costs about a
//! fifth of the recall at 10.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::Atlas;
use crate::graph::WeightedEdges;
use crate::patterns::literal_regex;
use crate::role::{Role, classify};

/// Damping of the personalised walk: the share of rank that moves along an
/// edge rather than returning to the seeds.
const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 20;
/// What the reverse of a dependency edge weighs against the edge itself.
const REVERSE: f64 = 0.5;
/// What the static rank adds to a lens score, against the walk's 1.0: enough
/// to order files the seeds treat alike, never enough to outvote a neighbour.
const STATIC_MIX: f64 = 0.05;
/// What a seed's own weight adds to its score, against the walk's 1.0 for the
/// heaviest file: a seed whose only neighbour is a busy file would otherwise
/// hand its rank away and be listed below it, and the first line of a lens is
/// the file the task is about.
const SEED_BONUS: f64 = 0.6;
/// A file lighter than this share of the heaviest is the tail of the walk, not
/// a place the task points at; it is not listed however much budget is left.
const MIN_RELATIVE: f64 = 0.06;
/// Seeds kept, heaviest first.
const MAX_SEEDS: usize = 16;
/// A name defined in more files than this carries no signal as a seed.
const MAX_DEFINERS: usize = 4;
/// A prompt word that names more files than this is a topic, not a pointer.
const MAX_WORD_FILES: usize = 30;
/// Files a lens lists at most, whatever its token budget allows.
pub const MAX_LENS_FILES: usize = 12;
/// Symbols named under one lens file.
const LENS_SYMBOLS: usize = 3;
/// Identifier-like prompt words with no symbol behind them that are reported.
const MAX_UNMAPPED: usize = 6;

/// Words that name nothing: too common in prose and in paths to be a pointer.
const STOP: &[&str] = &[
    "the", "and", "for", "fix", "add", "use", "with", "from", "that", "this", "when", "not",
    "update", "remove", "make", "test", "tests", "docs", "doc", "src", "lib", "file", "files",
    "new", "into", "also", "now", "was", "are", "can", "all", "any", "does", "has", "have", "bug",
    "issue", "error", "support", "allow", "change", "should", "would", "could", "please", "need",
    "want", "let", "its", "but", "how", "why", "what", "where", "which", "there", "here", "then",
    "than", "code", "function", "method", "class", "type", "name", "value", "data", "mod", "main",
    "index",
];

/// Why a file is a seed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SeedKind {
    /// The prompt names the file.
    Named,
    /// The prompt names a symbol this file defines.
    Symbol(String),
    /// A word of the prompt is the file's stem or a component of its path.
    Word,
    /// The session read or edited it.
    Touched,
    /// The working tree changed it.
    Changed,
}

/// Where a lens gets its weight: files, each with the reasons.
#[derive(Debug, Clone, Default)]
pub struct Seeds {
    weights: Vec<(u32, f64)>,
    kinds: HashMap<u32, Vec<SeedKind>>,
    /// Identifier-looking words of the prompt that no symbol of the index
    /// carries: said out loud, so a lens that is guessing can be told from one
    /// that is not.
    pub unmapped: Vec<String>,
}

impl Seeds {
    /// No file got any weight.
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    /// How many files carry weight.
    pub fn len(&self) -> usize {
        self.weights.len()
    }
}

/// What a lens is built from.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeedInput<'a> {
    /// The user's text, as typed.
    pub prompt: &'a str,
    /// Paths the session read or edited, oldest first, in any spelling.
    pub touched: &'a [String],
    /// Paths the working tree changed, in any spelling.
    pub changed: &'a [String],
}

/// How much a lens may hold.
#[derive(Debug, Clone, Copy)]
pub struct LensBudget {
    /// Estimated tokens, header and footer included.
    pub tokens: usize,
    /// Files; [`LensBudget::tokens`] sets [`MAX_LENS_FILES`].
    pub files: usize,
}

impl LensBudget {
    pub fn tokens(tokens: usize) -> Self {
        Self {
            tokens,
            files: MAX_LENS_FILES,
        }
    }
}

/// One file of a lens.
#[derive(Debug, Clone)]
pub struct LensEntry {
    pub path: String,
    /// The walk's weight plus the static mix, 1.0 for the heaviest.
    pub score: f64,
    /// Why it is listed, in words a model can weigh.
    pub why: String,
    /// Distinct files that depend on it.
    pub dependents: usize,
    /// Symbols worth naming, the ones the prompt named first.
    pub symbols: Vec<String>,
}

/// The files this task is about, heaviest first.
#[derive(Debug, Clone, Default)]
pub struct Lens {
    pub entries: Vec<LensEntry>,
    pub unmapped: Vec<String>,
}

/// How a file is linked to a neighbour in the lens's walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeKind {
    /// The file itself depends on the neighbour.
    Uses,
    /// The neighbour depends on the file.
    UsedBy,
    /// History edits them together.
    CoChange,
}

/// What a co-change edge weighs against a dependency edge of one import: a
/// pair that history edits together every time is as tied as a few uses.
const COCHANGE: f64 = 1.5;
/// A pair's history weight is capped here before it is scaled; a pair edited
/// together in every recent commit saturates, it does not dominate.
const COCHANGE_CAP: f64 = 2.0;

/// What the lens needs from the records, built once per graph.
#[derive(Debug, Default)]
pub(crate) struct LensIndex {
    pub(crate) paths: Vec<String>,
    pub(crate) at: HashMap<String, u32>,
    /// `(neighbour, weight, kind)`; see [`EdgeKind`].
    pub(crate) adj: Vec<Vec<(u32, f64, EdgeKind)>>,
    out_sum: Vec<f64>,
    exact: HashMap<String, Vec<u32>>,
    lower: HashMap<String, Vec<u32>>,
    by_basename: HashMap<String, Vec<u32>>,
    words: HashMap<String, Vec<u32>>,
    stems: HashMap<String, Vec<u32>>,
}

/// The lens index of an [`Atlas`], built on first use and dropped whenever the
/// graph is rebuilt. Cloning shares it: a published snapshot builds it once.
#[derive(Debug, Clone, Default)]
pub(crate) struct LensCache(std::sync::Arc<OnceLock<LensIndex>>);

impl LensCache {
    fn get_or_build(&self, atlas: &Atlas) -> &LensIndex {
        self.0.get_or_init(|| LensIndex::build(atlas))
    }
}

impl LensIndex {
    fn build(atlas: &Atlas) -> Self {
        let WeightedEdges { nodes, out } =
            crate::graph::weighted_edges(&atlas.files, &atlas.layout);
        let n = nodes.len();
        let at: HashMap<String, u32> = nodes
            .iter()
            .enumerate()
            .map(|(i, path)| (path.clone(), i as u32))
            .collect();
        let mut adj: Vec<Vec<(u32, f64, EdgeKind)>> = vec![Vec::new(); n];
        for (from, edges) in out.iter().enumerate() {
            for &(to, weight) in edges {
                adj[from].push((to, weight, EdgeKind::Uses));
                adj[to as usize].push((from as u32, weight * REVERSE, EdgeKind::UsedBy));
            }
        }
        for (path, partners) in atlas.cochange.iter() {
            let Some(&from) = at.get(path) else {
                continue;
            };
            for (other, weight) in partners {
                if let Some(&to) = at.get(other) {
                    let weight = COCHANGE * f64::from(*weight).min(COCHANGE_CAP);
                    adj[from as usize].push((to, weight, EdgeKind::CoChange));
                }
            }
        }
        let out_sum: Vec<f64> = adj
            .iter()
            .map(|edges| edges.iter().map(|(_, w, _)| w).sum())
            .collect();
        let mut exact: HashMap<String, Vec<u32>> = HashMap::new();
        let mut lower: HashMap<String, Vec<u32>> = HashMap::new();
        let mut by_basename: HashMap<String, Vec<u32>> = HashMap::new();
        let mut words: HashMap<String, Vec<u32>> = HashMap::new();
        let mut stems: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, path) in nodes.iter().enumerate() {
            let i = i as u32;
            let Some(record) = atlas.files.get(path) else {
                continue;
            };
            for name in &record.exports {
                exact.entry(name.clone()).or_default().push(i);
                lower.entry(name.to_lowercase()).or_default().push(i);
            }
            let basename = path.rsplit('/').next().unwrap_or(path);
            by_basename
                .entry(basename.to_lowercase())
                .or_default()
                .push(i);
            let stem = basename.rsplit_once('.').map_or(basename, |(stem, _)| stem);
            stems.entry(stem.to_lowercase()).or_default().push(i);
            let mut seen: HashSet<String> = HashSet::new();
            for component in path.split('/') {
                for word in split_words(component) {
                    if seen.insert(word.clone()) {
                        words.entry(word).or_default().push(i);
                    }
                }
            }
        }
        Self {
            paths: nodes,
            at,
            adj,
            out_sum,
            exact,
            lower,
            by_basename,
            words,
            stems,
        }
    }

    /// Files a spelled path names: the exact key, else every file whose path
    /// ends with it (`runtime.rs`, `engine/runtime.rs`).
    pub(crate) fn resolve_path_public(&self, atlas: &Atlas, spelled: &str) -> Vec<u32> {
        self.resolve_path(atlas, spelled)
    }

    fn resolve_path(&self, atlas: &Atlas, spelled: &str) -> Vec<u32> {
        let spelled = spelled.replace('\\', "/");
        if let Some(key) = atlas.key(&spelled)
            && let Some(&at) = self.at.get(&key)
        {
            return vec![at];
        }
        let trimmed = spelled.trim_start_matches("./");
        let basename = trimmed.rsplit('/').next().unwrap_or(trimmed).to_lowercase();
        let suffix = format!("/{}", trimmed.to_lowercase());
        self.by_basename
            .get(&basename)
            .map(|candidates| {
                candidates
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let path = self.paths[i as usize].to_lowercase();
                        path == trimmed.to_lowercase() || path.ends_with(&suffix)
                    })
                    .take(MAX_DEFINERS)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Lowercase words of an identifier or a path component: `runtimeConfig.rs`
/// and `runtime_config` both give `runtime` and `config`.
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for ch in text.chars() {
        if !ch.is_alphanumeric() {
            flush(&mut current, &mut words);
            previous_lower = false;
            continue;
        }
        if ch.is_uppercase() && previous_lower {
            flush(&mut current, &mut words);
        }
        previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        current.extend(ch.to_lowercase());
    }
    flush(&mut current, &mut words);
    words
}

fn flush(current: &mut String, words: &mut Vec<String>) {
    if current.chars().count() >= 3 {
        words.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

impl Atlas {
    /// The graph index the lens and the query layer walk, built on first use.
    pub(crate) fn lens_index(&self) -> &LensIndex {
        self.lens.get_or_build(self)
    }

    /// The seeds `input` gives: see the module's table.
    pub fn seeds(&self, input: &SeedInput) -> Seeds {
        let index = self.lens.get_or_build(self);
        let mut weights: HashMap<u32, f64> = HashMap::new();
        let mut kinds: HashMap<u32, Vec<SeedKind>> = HashMap::new();
        let mut add = |at: u32, weight: f64, kind: SeedKind| {
            *weights.entry(at).or_default() += weight;
            let list = kinds.entry(at).or_default();
            if !list.contains(&kind) {
                list.push(kind);
            }
        };

        // Files the prompt names, with or without a line: the path-looking
        // words, which is also what a stack frame is.
        let named = literal_regex(r"[A-Za-z0-9_@./\\-]+\.[A-Za-z0-9]{1,5}(?::\d+)?");
        let mut named_spans: Vec<(usize, usize)> = Vec::new();
        for found in named.find_iter(input.prompt) {
            let spelled = found.as_str().trim_matches(['.', '-', '/', '@']);
            let spelled = spelled.split(':').next().unwrap_or(spelled);
            let hits = index.resolve_path(self, spelled);
            if hits.is_empty() {
                continue;
            }
            named_spans.push((found.start(), found.end()));
            let share = 5.0 / (hits.len() as f64).sqrt();
            for at in hits {
                add(at, share, SeedKind::Named);
            }
        }

        // Identifiers: exact first, then ignoring case for longer ones.
        let backticked: HashSet<&str> = literal_regex(r"`([A-Za-z_][A-Za-z0-9_]*)")
            .captures_iter(input.prompt)
            .filter_map(|caps| caps.get(1).map(|m| m.as_str()))
            .collect();
        let ident = literal_regex(r"[A-Za-z_][A-Za-z0-9_]{3,}");
        let mut seen_idents: HashSet<&str> = HashSet::new();
        let mut unmapped: Vec<String> = Vec::new();
        let mut word_seen: HashSet<String> = HashSet::new();
        for found in ident.find_iter(input.prompt) {
            if named_spans
                .iter()
                .any(|&(start, end)| found.start() >= start && found.end() <= end)
            {
                continue;
            }
            let token = found.as_str();
            if !seen_idents.insert(token) {
                continue;
            }
            // A plain lowercase word is prose until it is quoted: "line" in a
            // prompt is not a pointer to the function called `line`.
            let plain = token.chars().all(|ch| ch.is_ascii_lowercase());
            let boost = if backticked.contains(token) {
                2.0
            } else if plain {
                if token.len() < 6 || crate::scope::is_common(token) {
                    0.0
                } else {
                    0.4
                }
            } else {
                1.0
            };
            let mut matched = false;
            if boost == 0.0 {
                // prose; the word seeds by path below
            } else if let Some(definers) = index.exact.get(token)
                && definers.len() <= MAX_DEFINERS
            {
                let share = 3.0 * boost / (definers.len() as f64).sqrt();
                for &at in definers {
                    add(at, share, SeedKind::Symbol(token.to_owned()));
                }
                matched = true;
            } else if token.chars().count() >= 5
                && let Some(definers) = index.lower.get(&token.to_lowercase())
                && definers.len() <= MAX_DEFINERS
            {
                let share = 1.5 * boost / (definers.len() as f64).sqrt();
                for &at in definers {
                    let name = self.files[&index.paths[at as usize]]
                        .exports
                        .iter()
                        .find(|name| name.eq_ignore_ascii_case(token))
                        .cloned()
                        .unwrap_or_else(|| token.to_owned());
                    add(at, share, SeedKind::Symbol(name));
                }
                matched = true;
            }
            let looks_like_code = token.contains('_')
                || token.chars().skip(1).any(char::is_uppercase)
                || backticked.contains(token);
            if !matched && looks_like_code && unmapped.len() < MAX_UNMAPPED {
                // A word that is also a path component is not unmapped: the
                // word seeds below.
                let words = split_words(token);
                if !words.iter().any(|word| index.words.contains_key(word)) {
                    unmapped.push(token.to_owned());
                }
            }
            for word in split_words(token) {
                word_seen.insert(word);
            }
        }
        // Plain prose words, too: they seed by stem and path component.
        let prose = literal_regex(r"[A-Za-z][A-Za-z0-9]{2,}");
        for found in prose.find_iter(input.prompt) {
            for word in split_words(found.as_str()) {
                word_seen.insert(word);
            }
        }
        for word in &word_seen {
            if STOP.contains(&word.as_str()) {
                continue;
            }
            if let Some(files) = index.stems.get(word)
                && files.len() <= MAX_WORD_FILES
            {
                for &at in files {
                    add(at, 2.0, SeedKind::Word);
                }
            }
            if let Some(files) = index.words.get(word)
                && files.len() <= MAX_WORD_FILES
            {
                for &at in files {
                    add(at, 0.5, SeedKind::Word);
                }
            }
        }

        // The session: newest first, each older one weighing a little less.
        let mut decay = 1.0;
        for path in input.touched.iter().rev() {
            if let Some(key) = self.key(path)
                && let Some(&at) = index.at.get(&key)
            {
                add(at, 2.0 * decay, SeedKind::Touched);
                decay *= 0.85;
            }
        }
        for path in input.changed {
            if let Some(key) = self.key(path)
                && let Some(&at) = index.at.get(&key)
            {
                add(at, 2.0, SeedKind::Changed);
            }
        }

        let mut weights: Vec<(u32, f64)> = weights.into_iter().collect();
        weights.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| index.paths[a.0 as usize].cmp(&index.paths[b.0 as usize]))
        });
        weights.truncate(MAX_SEEDS);
        Seeds {
            weights,
            kinds,
            unmapped,
        }
    }

    /// The lens `seeds` give, within `budget`. Empty when there are no seeds.
    pub fn lens(&self, seeds: &Seeds, budget: LensBudget) -> Lens {
        if seeds.is_empty() {
            return Lens {
                entries: Vec::new(),
                unmapped: seeds.unmapped.clone(),
            };
        }
        let index = self.lens.get_or_build(self);
        let n = index.paths.len();
        let total: f64 = seeds.weights.iter().map(|(_, w)| w).sum();
        let mut tele = vec![0.0; n];
        for &(at, weight) in &seeds.weights {
            tele[at as usize] = weight / total;
        }
        let mut score = tele.clone();
        let mut next = vec![0.0; n];
        for _ in 0..ITERATIONS {
            for (at, slot) in next.iter_mut().enumerate() {
                *slot = (1.0 - DAMPING) * tele[at];
            }
            let mut dangling = 0.0;
            for (at, mass) in score.iter().enumerate() {
                if *mass == 0.0 {
                    continue;
                }
                if index.out_sum[at] <= 0.0 {
                    dangling += mass;
                    continue;
                }
                let per_weight = DAMPING * mass / index.out_sum[at];
                for &(to, weight, _) in &index.adj[at] {
                    next[to as usize] += per_weight * weight;
                }
            }
            if dangling > 0.0 {
                for &(at, _) in &seeds.weights {
                    next[at as usize] += DAMPING * dangling * tele[at as usize];
                }
            }
            std::mem::swap(&mut score, &mut next);
        }

        let statics: Vec<f64> = index
            .paths
            .iter()
            .map(|path| {
                let base = self.ranks.get(path).copied().unwrap_or(0.0);
                match classify(path) {
                    Role::Auxiliary => base * 0.25,
                    Role::Entry => base * 2.0,
                    Role::Source => base,
                }
            })
            .collect();
        let top_walk = score
            .iter()
            .copied()
            .fold(0.0, f64::max)
            .max(f64::MIN_POSITIVE);
        let top_static = statics
            .iter()
            .copied()
            .fold(0.0, f64::max)
            .max(f64::MIN_POSITIVE);
        let top_seed = seeds
            .weights
            .iter()
            .map(|(_, weight)| *weight)
            .fold(0.0, f64::max)
            .max(f64::MIN_POSITIVE);
        let mut order: Vec<(usize, f64)> = (0..n)
            .map(|at| {
                let bonus = SEED_BONUS * tele[at] * total / top_seed;
                (
                    at,
                    score[at] / top_walk + bonus + STATIC_MIX * statics[at] / top_static,
                )
            })
            .filter(|(at, value)| *value > 0.0 && score[*at] > 0.0)
            .collect();
        let best = order.iter().map(|(_, value)| *value).fold(0.0, f64::max);
        order.retain(|(_, value)| *value >= MIN_RELATIVE * best);
        order.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| index.paths[a.0].cmp(&index.paths[b.0]))
        });

        let mut lens = Lens {
            entries: Vec::new(),
            unmapped: seeds.unmapped.clone(),
        };
        let mut spent = crate::estimate_tokens(HEADER) + crate::estimate_tokens(FOOTER);
        if !lens.unmapped.is_empty() {
            spent += crate::estimate_tokens(&unmapped_line(&lens.unmapped));
        }
        let seed_set: HashSet<u32> = seeds.weights.iter().map(|(at, _)| *at as u32).collect();
        for (at, value) in order {
            if lens.entries.len() >= budget.files {
                break;
            }
            let path = &index.paths[at];
            if path.contains(['<', '>']) {
                continue;
            }
            let entry = LensEntry {
                path: path.clone(),
                score: value,
                why: self.why(index, seeds, &seed_set, &score, at as u32),
                dependents: self.dependents.get(path).copied().unwrap_or(0),
                symbols: self.lens_symbols(seeds, path, at as u32),
            };
            let cost = crate::estimate_tokens(&entry.line());
            if spent + cost > budget.tokens && !lens.entries.is_empty() {
                break;
            }
            spent += cost;
            lens.entries.push(entry);
        }
        lens
    }

    fn why(
        &self,
        index: &LensIndex,
        seeds: &Seeds,
        seed_set: &HashSet<u32>,
        score: &[f64],
        at: u32,
    ) -> String {
        if seed_set.contains(&at)
            && let Some(kinds) = seeds.kinds.get(&at)
        {
            let mut parts: Vec<String> = Vec::new();
            for kind in kinds {
                let part = match kind {
                    SeedKind::Named => "named in the prompt".to_owned(),
                    SeedKind::Symbol(name) => format!("defines {}", safe_name(name)),
                    SeedKind::Word => "path matches a prompt word".to_owned(),
                    SeedKind::Touched => "touched this session".to_owned(),
                    SeedKind::Changed => "changed in the working tree".to_owned(),
                };
                if !parts.contains(&part) {
                    parts.push(part);
                }
            }
            return parts.join(", ");
        }
        let best = index.adj[at as usize]
            .iter()
            .filter(|(to, _, _)| score[*to as usize] > 0.0)
            .max_by(|a, b| {
                (score[a.0 as usize] * a.1)
                    .partial_cmp(&(score[b.0 as usize] * b.1))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| index.paths[b.0 as usize].cmp(&index.paths[a.0 as usize]))
            });
        match best {
            Some(&(to, _, EdgeKind::Uses)) => format!("uses {}", index.paths[to as usize]),
            Some(&(to, _, EdgeKind::UsedBy)) => format!("used by {}", index.paths[to as usize]),
            Some(&(to, _, EdgeKind::CoChange)) => {
                format!("changes with {}", index.paths[to as usize])
            }
            None => "near the seeds".to_owned(),
        }
    }

    /// Symbols to name under a lens file: the ones the prompt named, then the
    /// most used.
    fn lens_symbols(&self, seeds: &Seeds, path: &str, at: u32) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(kinds) = seeds.kinds.get(&at) {
            for kind in kinds {
                if let SeedKind::Symbol(name) = kind {
                    let name = safe_name(name);
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
        for (name, _) in self.used_by.get(path).into_iter().flatten() {
            if out.len() >= LENS_SYMBOLS {
                break;
            }
            let name = safe_name(name);
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out.truncate(LENS_SYMBOLS);
        out
    }
}

const HEADER: &str = "<atlas-lens>\n";
const FOOTER: &str = "</atlas-lens>";

/// A name safe to print inside the block: identifier characters only.
fn safe_name(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$' | '.' | ':' | '-'))
        .take(48)
        .collect()
}

fn unmapped_line(words: &[String]) -> String {
    let safe: Vec<String> = words.iter().map(|word| safe_name(word)).collect();
    format!("unmapped: {}\n", safe.join(", "))
}

impl LensEntry {
    /// `path (→N) why · +Sym +Sym`, one line, newline included.
    fn line(&self) -> String {
        let mut line = format!("{} (→{})", self.path, self.dependents);
        if let Some(label) = crate::role::tag(&self.path) {
            line.push_str(" [");
            line.push_str(label);
            line.push(']');
        }
        line.push(' ');
        line.push_str(&self.why);
        for symbol in &self.symbols {
            line.push_str(" +");
            line.push_str(symbol);
        }
        line.push('\n');
        line
    }
}

impl Lens {
    /// Whether there is anything to say.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The block the model reads. `pending` is the indexer's backlog, named as
    /// the prompt map names it.
    pub fn render(&self, pending: usize) -> String {
        let mut out = if pending == 0 {
            HEADER.to_owned()
        } else {
            format!("<atlas-lens pending=\"{pending}\">\n")
        };
        for entry in &self.entries {
            out.push_str(&entry.line());
        }
        if !self.unmapped.is_empty() {
            out.push_str(&unmapped_line(&self.unmapped));
        }
        out.push_str(FOOTER);
        out
    }

    /// The files a rendered lens lists, in order: what a later turn compares.
    pub fn paths_in(block: &str) -> Vec<&str> {
        block
            .lines()
            .filter(|line| !line.starts_with('<') && !line.starts_with("unmapped:"))
            .filter_map(|line| line.split(" (→").next())
            .collect()
    }
}
