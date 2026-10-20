use std::collections::{HashMap, HashSet};

use crate::{FileRecord, SymbolRecord};

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 20;

/// PageRank over the file graph, and each file's dependent count.
///
/// The power iteration runs over `Vec`s indexed by file, so a pass is plain
/// arithmetic: keyed by path it hashed and cloned a `String` per file per
/// iteration, and that was most of what an edit that moved the graph cost on a
/// large tree. Files are visited in the map's iteration order and each file's
/// inbound sources are kept in that order, so the sums are added in the order
/// the keyed version added them.
pub fn rank(
    files: &HashMap<String, FileRecord>,
    symbols: &HashMap<String, SymbolRecord>,
) -> (HashMap<String, f64>, HashMap<String, usize>) {
    let nodes: Vec<&String> = files.keys().collect();
    let n = nodes.len();
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, path)| (path.as_str(), at))
        .collect();
    let mut dependents = vec![0usize; n];
    let mut inbound: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut outbound = vec![1usize; n];

    for (from_at, from) in nodes.iter().enumerate() {
        let record = &files[*from];
        // Two edge kinds, one graph: a file imports another, or it mentions a
        // symbol that other file defines. Either way it depends on it.
        let mut targets: HashSet<&str> = record.imports.iter().map(String::as_str).collect();
        for name in &record.used_symbols {
            if let Some(symbol) = symbols.get(name) {
                for definer in &symbol.files {
                    if definer != *from && index.contains_key(definer.as_str()) {
                        targets.insert(definer);
                    }
                }
            }
        }
        outbound[from_at] = targets.len().max(1);
        for to in targets {
            if let Some(&to_at) = index.get(to) {
                inbound[to_at].push(from_at);
                dependents[to_at] += 1;
            }
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
            for &src in sources {
                incoming += score[src] / outbound[src] as f64;
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::SystemTime;

    use super::{DAMPING, ITERATIONS, rank};
    use crate::{FileRecord, Language, SymbolRecord};

    fn record(path: &str, imports: &[&str], used: &[&str]) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            language: Language::Rust,
            exports: Vec::new(),
            export_sites: Vec::new(),
            imports: imports.iter().map(|s| (*s).to_owned()).collect(),
            unresolved_imports: Vec::new(),
            unresolved_candidates: Vec::new(),
            syntax_errors: 0,
            used_symbols: used.iter().map(|s| (*s).to_owned()).collect(),
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
        symbols: &HashMap<String, SymbolRecord>,
    ) -> (HashMap<String, f64>, HashMap<String, usize>) {
        let nodes: Vec<String> = files.keys().cloned().collect();
        let n = nodes.len();
        let mut dependents: HashMap<String, usize> = nodes.iter().map(|k| (k.clone(), 0)).collect();
        let mut inbound: HashMap<String, Vec<String>> = HashMap::new();
        let mut outbound: HashMap<String, usize> = HashMap::new();
        for (from, record) in files {
            let mut targets: HashSet<&String> = record.imports.iter().collect();
            for name in &record.used_symbols {
                if let Some(symbol) = symbols.get(name) {
                    for definer in &symbol.files {
                        if definer != from && files.contains_key(definer) {
                            targets.insert(definer);
                        }
                    }
                }
            }
            outbound.insert(from.clone(), targets.len().max(1));
            for to in targets {
                if files.contains_key(to) {
                    inbound.entry(to.clone()).or_default().push(from.clone());
                    *dependents.entry(to.clone()).or_default() += 1;
                }
            }
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
                    for src in sources {
                        let out = *outbound.get(src).unwrap_or(&1) as f64;
                        incoming += score.get(src).copied().unwrap_or(0.0) / out;
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
        // file, which still counts toward the importer's out-degree), and
        // mentions of symbols defined in other files and in the file itself.
        let mut files = HashMap::new();
        let mut symbols = HashMap::new();
        for at in 0..60 {
            let path = format!("src/f{at}.rs");
            let imports = [
                format!("src/f{}.rs", (at * 7 + 1) % 60),
                format!("src/f{}.rs", (at * 3 + 2) % 60),
                "src/missing.rs".to_owned(),
            ];
            let imports: Vec<&str> = imports.iter().map(String::as_str).collect();
            let name = format!("sym{}", at % 20);
            files.insert(path.clone(), record(&path, &imports, &[&name, "unknown"]));
            symbols.insert(
                format!("sym{at}"),
                SymbolRecord {
                    files: vec![format!("src/f{at}.rs")],
                    users: 0,
                },
            );
        }
        let (ranks, dependents) = rank(&files, &symbols);
        let (want_ranks, want_dependents) = keyed_rank(&files, &symbols);
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
        let (ranks, dependents) = rank(&HashMap::new(), &HashMap::new());
        assert!(ranks.is_empty() && dependents.is_empty());
    }
}
