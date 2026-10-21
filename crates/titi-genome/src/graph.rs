use std::collections::HashMap;

use crate::{Basis, FileRecord};

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 20;

const EVIDENCE: f64 = 1.0;
const GUESS: f64 = 1.0;

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
pub fn rank(files: &HashMap<String, FileRecord>) -> (HashMap<String, f64>, HashMap<String, usize>) {
    let nodes: Vec<&String> = files.keys().collect();
    let n = nodes.len();
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, path)| (path.as_str(), at))
        .collect();
    let mut dependents = vec![0usize; n];
    let mut inbound: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut outbound = vec![0.0f64; n];

    for (from_at, from) in nodes.iter().enumerate() {
        let record = &files[*from];
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
            let weight = edge_weight(total);
            outbound[from_at] += weight;
            inbound[to_at].push((from_at, weight));
            dependents[to_at] += 1;
        }
    }

    let dependents: HashMap<String, usize> = nodes
        .iter()
        .zip(dependents)
        .map(|(path, count)| ((*path).clone(), count))
        .collect();
    if n == 0 {
        return (HashMap::new(), dependents);
    }

    let teleport = (1.0 - DAMPING) / n as f64;
    let mut score = vec![1.0 / n as f64; n];
    let mut next = vec![0.0; n];
    for _ in 0..ITERATIONS {
        for (at, sources) in inbound.iter().enumerate() {
            let mut incoming = 0.0;
            for &(src, weight) in sources {
                incoming += score[src] * weight / outbound[src];
            }
            next[at] = teleport + DAMPING * incoming;
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

/// Every edge counts once, however much evidence backs it.
fn edge_weight(_evidence: f64) -> f64 {
    1.0
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::SystemTime;

    use super::{DAMPING, EVIDENCE, GUESS, ITERATIONS, rank};
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
                let weight = if units > 0.0 { 1.0 } else { 0.0 };
                total += weight;
                inbound
                    .entry(to.clone())
                    .or_default()
                    .push((from.clone(), weight));
                *dependents.entry(to.clone()).or_default() += 1;
            }
            outbound.insert(from.clone(), total);
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
        let (ranks, dependents) = rank(&files);
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

    #[test]
    fn an_empty_graph_ranks_to_nothing() {
        let (ranks, dependents) = rank(&HashMap::new());
        assert!(ranks.is_empty() && dependents.is_empty());
    }
}
