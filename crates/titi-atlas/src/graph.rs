use std::collections::HashMap;

use crate::role::{Role, classify};
use crate::scope::Layout;
use crate::{Basis, FileRecord};

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 20;

/// What an import, or a use resolved by scope, is worth as evidence. An edge
/// is as heavy as the square root of the evidence behind it: a file that
/// leans on four names of another is more tied to it than a file that
/// mentions one, but not four times more.
const EVIDENCE: f64 = 1.0;

/// What a use resolved only because the name is distinctive and defined once
/// is worth. Four guesses weigh as much as one real dependency.
const GUESS: f64 = 0.25;

/// What a file's edges weigh at least, however light they are.
const MIN_OUTBOUND: f64 = 1.0;

/// How much of a file's say a test, example or generated file has: the share
/// of rank it starts with and the weight of the edges it draws. A hundred
/// tests leaning on a helper are not a hundred reasons the helper matters.
const AUXILIARY_SHARE: f64 = 0.25;
const AUXILIARY_EDGE: f64 = 0.7;

/// How much more of the start-of-walk rank an entry point holds. Nothing
/// imports a binary's `main`, so by inbound edges alone it is the least
/// important file in the repo, and it is where everything it runs hangs from.
const ENTRY_SHARE: f64 = 4.0;

/// PageRank over the file graph, and each file's dependent count.
///
/// An edge runs from a file to each file it imports and each file one of its
/// uses resolves to, weighted by [`edge_weight`]. A file spreads its rank over
/// its edges in proportion to their weights.
///
/// The power iteration runs over `Vec`s indexed by file, so a pass is plain
/// arithmetic: keyed by path it hashed and cloned a `String` per file per
/// iteration, and that was most of what an edit that moved the graph cost on a
/// large tree. Files are visited in the map's iteration order and each file's
/// inbound sources are kept in that order, so the sums are added in the order
/// the keyed version added them.
pub(crate) fn rank(
    files: &HashMap<String, FileRecord>,
    layout: &Layout,
) -> (HashMap<String, f64>, HashMap<String, usize>) {
    let nodes: Vec<&String> = files.keys().collect();
    let n = nodes.len();
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, path)| (path.as_str(), at))
        .collect();
    let shares: Vec<f64> = nodes
        .iter()
        .map(|path| match classify(path) {
            Role::Source => 1.0,
            Role::Entry => ENTRY_SHARE,
            Role::Auxiliary => AUXILIARY_SHARE,
        })
        .collect();
    let share_total: f64 = shares.iter().sum();
    let implementations = implementations(&nodes);
    let mut dependents = vec![0usize; n];
    let mut inbound: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut outbound = vec![0.0f64; n];

    for (from_at, from) in nodes.iter().enumerate() {
        let record = &files[*from];
        let from_edge = if classify(from) == Role::Auxiliary {
            AUXILIARY_EDGE
        } else {
            1.0
        };
        // Two edge kinds, one graph: a file imports another, or it mentions a
        // symbol that other file defines. Either way it depends on it.
        let mut evidence: Vec<(usize, f64)> = Vec::new();
        for import in &record.imports {
            if let Some(&to_at) = index.get(import.as_str()) {
                evidence.push((to_at, EVIDENCE));
            }
        }
        for used in &record.uses {
            if let Some(&to_at) = index.get(used.definer.as_str()) {
                let worth = match used.basis {
                    Basis::Scope => EVIDENCE,
                    Basis::Unique => GUESS,
                };
                evidence.push((to_at, worth));
            }
        }
        // A file that names a workspace crate leans on that crate's `lib.rs`
        // as much as on a file it imports: the facade is the dependency.
        for facade in layout.facades(record) {
            if let Some(&to_at) = index.get(facade) {
                evidence.push((to_at, EVIDENCE));
            }
        }
        evidence.retain(|(to_at, _)| *to_at != from_at);
        evidence.sort_by_key(|(to_at, _)| *to_at);
        let mut at = 0;
        while at < evidence.len() {
            let to_at = evidence[at].0;
            let mut total = 0.0;
            while at < evidence.len() && evidence[at].0 == to_at {
                total += evidence[at].1;
                at += 1;
            }
            let weight = edge_weight(total) * from_edge;
            outbound[from_at] += weight;
            inbound[to_at].push((from_at, weight));
            dependents[to_at] += 1;
        }
        // A header's rank is its implementation's too: nothing includes the
        // `.c`, and by include edges alone it would be the module's least
        // important file. Not a dependent, so `(→N)` is unchanged.
        if let Some(companions) = implementation_key(from).and_then(|key| implementations.get(&key))
        {
            for &to_at in companions.iter().filter(|to_at| **to_at != from_at) {
                outbound[from_at] += from_edge;
                inbound[to_at].push((from_at, from_edge));
            }
        }
        // The floor is what keeps a file whose only edge is a guess from
        // handing all its rank to it: it passes on what its edges weigh, and
        // never more than one import's worth would carry.
        outbound[from_at] = outbound[from_at].max(MIN_OUTBOUND);
    }

    let dependents: HashMap<String, usize> = nodes
        .iter()
        .zip(dependents)
        .map(|(path, count)| ((*path).clone(), count))
        .collect();
    if n == 0 {
        return (HashMap::new(), dependents);
    }

    let mut score = vec![1.0 / n as f64; n];
    let mut next = vec![0.0; n];
    for _ in 0..ITERATIONS {
        for (at, sources) in inbound.iter().enumerate() {
            let mut incoming = 0.0;
            for &(src, weight) in sources {
                incoming += score[src] * weight / outbound[src];
            }
            next[at] = (1.0 - DAMPING) * shares[at] / share_total + DAMPING * incoming;
        }
        std::mem::swap(&mut score, &mut next);
    }
    let ranks = nodes
        .iter()
        .zip(score)
        .map(|(path, value)| ((*path).clone(), value))
        .collect();
    (ranks, dependents)
}

const HEADERS: &[&str] = &["h", "hh", "hpp", "hxx"];
const IMPLEMENTATIONS: &[&str] = &["c", "cc", "cpp", "cxx", "m", "mm"];

/// `(directory, stem)` of a C-family header: where its implementation sits.
fn implementation_key(path: &str) -> Option<(String, String)> {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let (stem, ext) = name.rsplit_once('.')?;
    HEADERS
        .contains(&ext)
        .then(|| (dir.to_owned(), stem.to_owned()))
}

/// The C-family implementation files by `(directory, stem)`, as node indices.
fn implementations(nodes: &[&String]) -> HashMap<(String, String), Vec<usize>> {
    let mut found: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (at, path) in nodes.iter().enumerate() {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        if let Some((stem, ext)) = name.rsplit_once('.')
            && IMPLEMENTATIONS.contains(&ext)
        {
            found
                .entry((dir.to_owned(), stem.to_owned()))
                .or_default()
                .push(at);
        }
    }
    found
}

/// The weight of an edge backed by `evidence` units of imports and uses.
fn edge_weight(evidence: f64) -> f64 {
    evidence.sqrt()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::SystemTime;

    use super::{DAMPING, EVIDENCE, GUESS, ITERATIONS, rank};
    use crate::scope::Layout;
    use crate::{Basis, FileRecord, Language, SymbolUse};

    fn record(path: &str, imports: &[&str], uses: &[(&str, Basis)]) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            language: Language::Rust,
            exports: Vec::new(),
            export_sites: Vec::new(),
            imports: imports.iter().map(|s| (*s).to_owned()).collect(),
            unresolved_imports: Vec::new(),
            unresolved_candidates: Vec::new(),
            syntax_errors: 0,
            reexports: Vec::new(),
            inherits: Vec::new(),
            external_uses: Vec::new(),
            uses: uses
                .iter()
                .enumerate()
                .map(|(at, (definer, basis))| SymbolUse {
                    name: format!("name{at}"),
                    definer: (*definer).to_owned(),
                    basis: *basis,
                })
                .collect(),
            raw_refs: Vec::new(),
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            read_at: SystemTime::UNIX_EPOCH,
            hash: 0,
        }
    }

    /// The path-keyed form `rank` replaced, kept as the oracle: same graph,
    /// same arithmetic, hashing a `String` per file per iteration.
    fn keyed_rank(
        files: &HashMap<String, FileRecord>,
    ) -> (HashMap<String, f64>, HashMap<String, usize>) {
        let nodes: Vec<String> = files.keys().cloned().collect();
        let n = nodes.len();
        let mut dependents: HashMap<String, usize> = nodes.iter().map(|k| (k.clone(), 0)).collect();
        let mut inbound: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        let mut outbound: HashMap<String, f64> = HashMap::new();
        for (from, record) in files {
            let mut evidence: HashMap<&String, f64> = HashMap::new();
            for import in &record.imports {
                if files.contains_key(import) && import != from {
                    *evidence.entry(import).or_default() += EVIDENCE;
                }
            }
            for used in &record.uses {
                if files.contains_key(&used.definer) && &used.definer != from {
                    *evidence.entry(&used.definer).or_default() += match used.basis {
                        Basis::Scope => EVIDENCE,
                        Basis::Unique => GUESS,
                    };
                }
            }
            let mut total = 0.0;
            for (to, units) in evidence {
                let weight = f64::sqrt(units);
                total += weight;
                inbound
                    .entry(to.clone())
                    .or_default()
                    .push((from.clone(), weight));
                *dependents.entry(to.clone()).or_default() += 1;
            }
            outbound.insert(from.clone(), f64::max(total, super::MIN_OUTBOUND));
        }
        if n == 0 {
            return (HashMap::new(), dependents);
        }
        let mut score: HashMap<String, f64> =
            nodes.iter().map(|k| (k.clone(), 1.0 / n as f64)).collect();
        for _ in 0..ITERATIONS {
            let mut next = HashMap::new();
            for node in &nodes {
                let mut incoming = 0.0;
                if let Some(sources) = inbound.get(node) {
                    for (src, weight) in sources {
                        incoming += score.get(src).copied().unwrap_or(0.0) * weight / outbound[src];
                    }
                }
                next.insert(
                    node.clone(),
                    (1.0 - DAMPING) / n as f64 + DAMPING * incoming,
                );
            }
            score = next;
        }
        (score, dependents)
    }

    #[test]
    fn ranking_by_index_agrees_with_the_keyed_ranking() {
        // 60 files: imports to neighbours (one of them to a path that is not a
        // file, which carries no edge), and uses of symbols other files define
        // — some by scope, some guessed, one resolved to the file itself and
        // one to a definer that is not a file, which carry none.
        let mut files = HashMap::new();
        for at in 0..60 {
            let path = format!("src/f{at}.rs");
            let imports = [
                format!("src/f{}.rs", (at * 7 + 1) % 60),
                format!("src/f{}.rs", (at * 3 + 2) % 60),
                "src/missing.rs".to_owned(),
            ];
            let imports: Vec<&str> = imports.iter().map(String::as_str).collect();
            let near = format!("src/f{}.rs", (at * 7 + 1) % 60);
            let far = format!("src/f{}.rs", (at + 11) % 60);
            let uses = [
                (near.as_str(), Basis::Scope),
                (near.as_str(), Basis::Scope),
                (far.as_str(), Basis::Unique),
                (path.as_str(), Basis::Scope),
                ("src/ghost.rs", Basis::Scope),
            ];
            files.insert(path.clone(), record(&path, &imports, &uses));
        }
        let (ranks, dependents) = rank(&files, &Layout::default());
        let (want_ranks, want_dependents) = keyed_rank(&files);
        assert_eq!(dependents, want_dependents);
        assert_eq!(ranks.len(), want_ranks.len());
        for (path, want) in &want_ranks {
            assert!(
                (ranks[path] - want).abs() < 1e-12,
                "{path}: {} vs {want}",
                ranks[path]
            );
        }
        assert!(dependents.values().any(|count| *count > 1));
    }

    /// An edge's weight is the square root of its evidence, so the same two
    /// importers rank a hub differently by how many of its names they use.
    #[test]
    fn an_edge_weighs_the_square_root_of_its_evidence() {
        let mut files = HashMap::new();
        for (path, uses) in [
            ("deep.rs", vec![]),
            ("thin.rs", vec![]),
            (
                "user.rs",
                vec![
                    ("deep.rs", Basis::Scope),
                    ("deep.rs", Basis::Scope),
                    ("deep.rs", Basis::Scope),
                    ("deep.rs", Basis::Scope),
                    ("thin.rs", Basis::Scope),
                ],
            ),
        ] {
            files.insert(path.to_owned(), record(path, &[], &uses));
        }
        let (ranks, dependents) = rank(&files, &Layout::default());
        assert_eq!(dependents["deep.rs"], 1);
        assert_eq!(dependents["thin.rs"], 1);
        // user.rs spreads its rank 2 : 1 over the two hubs.
        let teleport = (1.0 - DAMPING) / 3.0;
        let share = |score: f64| (score - teleport) / DAMPING;
        let ratio = share(ranks["deep.rs"]) / share(ranks["thin.rs"]);
        assert!((ratio - 2.0).abs() < 1e-9, "{ratio}");
    }

    /// A guess is worth a quarter of a dependency: four of them weigh what one
    /// scoped use does.
    #[test]
    fn four_guesses_weigh_one_scoped_use() {
        let mut files = HashMap::new();
        files.insert("hub.rs".to_owned(), record("hub.rs", &[], &[]));
        files.insert("guess.rs".to_owned(), record("guess.rs", &[], &[]));
        files.insert(
            "user.rs".to_owned(),
            record(
                "user.rs",
                &[],
                &[
                    ("hub.rs", Basis::Scope),
                    ("guess.rs", Basis::Unique),
                    ("guess.rs", Basis::Unique),
                    ("guess.rs", Basis::Unique),
                    ("guess.rs", Basis::Unique),
                ],
            ),
        );
        let (ranks, _) = rank(&files, &Layout::default());
        assert!((ranks["hub.rs"] - ranks["guess.rs"]).abs() < 1e-12);
    }

    /// A file whose only edge is a guess passes on half of what a file with
    /// one import does, not all of it.
    #[test]
    fn a_lone_guess_does_not_carry_the_whole_rank() {
        let mut files = HashMap::new();
        for path in ["by_import.rs", "by_guess.rs"] {
            files.insert(path.to_owned(), record(path, &[], &[]));
        }
        files.insert(
            "importer.rs".to_owned(),
            record("importer.rs", &["by_import.rs"], &[]),
        );
        files.insert(
            "guesser.rs".to_owned(),
            record("guesser.rs", &[], &[("by_guess.rs", Basis::Unique)]),
        );
        let (ranks, _) = rank(&files, &Layout::default());
        let teleport = (1.0 - DAMPING) / 4.0;
        let share = |score: f64| (score - teleport) / DAMPING;
        let ratio = share(ranks["by_import.rs"]) / share(ranks["by_guess.rs"]);
        assert!((ratio - 2.0).abs() < 1e-9, "{ratio}");
    }

    #[test]
    fn an_empty_graph_ranks_to_nothing() {
        let (ranks, dependents) = rank(&HashMap::new(), &Layout::default());
        assert!(ranks.is_empty() && dependents.is_empty());
    }
}
