#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The same tree gives the same bytes. The prompt cache is keyed on them: a
//! map that differs between two runs over an unchanged tree — in a rank's last
//! digits, in the order of two files that tie, in which of two equal-looking
//! paths an import lands on — is a cache miss with nothing to show for it.
//!
//! Every `Atlas` hashes with its own random seed, so indexing one fixture
//! several times in one process is as good as several processes.

use std::fs;
use std::path::Path;

use titi_atlas::{Atlas, LensBudget, SeedInput};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// Symmetric modules: their ranks tie in exact arithmetic and differ only by
/// summation order, which is what reorders them between runs.
fn fixture(root: &Path) {
    write(
        root,
        "src/hub.rs",
        "pub fn hub_call() {}\npub struct HubConfig;\n",
    );
    for n in 0..24 {
        write(
            root,
            &format!("src/mod{n}/mod.rs"),
            &format!(
                "use crate::hub::{{hub_call, HubConfig}};\npub fn run{n}() {{ hub_call(); let _ = HubConfig; }}\n"
            ),
        );
    }
    for n in 0..6 {
        write(
            root,
            &format!("tests/case{n}.rs"),
            "use crate::hub::hub_call;\nfn t() { hub_call(); }\n",
        );
    }
}

struct Bytes {
    map: String,
    skeleton: String,
    lens: String,
    ranks: Vec<(String, u64)>,
    edges: Vec<(String, Vec<String>, usize)>,
}

fn observe(root: &Path) -> Bytes {
    let atlas = Atlas::index(root).unwrap();
    let seeds = atlas.seeds(&SeedInput {
        prompt: "hub_call and HubConfig misbehave; see src/mod3/mod.rs",
        touched: &["src/mod7/mod.rs".to_owned()],
        changed: &[],
    });
    let mut ranks: Vec<(String, u64)> = atlas
        .ranks
        .iter()
        .map(|(path, rank)| (path.clone(), rank.to_bits()))
        .collect();
    ranks.sort();
    let mut edges: Vec<(String, Vec<String>, usize)> = atlas
        .files
        .iter()
        .map(|(path, record)| {
            let mut imports = record.imports.clone();
            imports.sort();
            (path.clone(), imports, atlas.dependents[path])
        })
        .collect();
    edges.sort();
    Bytes {
        map: atlas.project(24),
        skeleton: atlas.skeleton(700, 0),
        lens: atlas.lens(&seeds, LensBudget::tokens(400)).render(0),
        ranks,
        edges,
    }
}

#[test]
fn the_same_tree_gives_the_same_map_lens_ranks_and_edges_every_time() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let first = observe(dir.path());
    for run in 1..10 {
        let again = observe(dir.path());
        assert_eq!(
            first.edges, again.edges,
            "run {run}: imports and dependents"
        );
        assert_eq!(first.ranks, again.ranks, "run {run}: rank bits");
        assert_eq!(first.map, again.map, "run {run}: the file-count map");
        assert_eq!(first.skeleton, again.skeleton, "run {run}: the skeleton");
        assert_eq!(first.lens, again.lens, "run {run}: the lens");
    }
}

/// Files created in a different order are the same tree.
#[test]
fn creation_order_does_not_matter() {
    let forward = tempfile::tempdir().unwrap();
    fixture(forward.path());
    let backward = tempfile::tempdir().unwrap();
    // The same files, written in reverse.
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push(path.strip_prefix(base).unwrap().to_path_buf());
            }
        }
    }
    walk(forward.path(), forward.path(), &mut files);
    files.sort();
    files.reverse();
    for rel in files {
        let body = fs::read_to_string(forward.path().join(&rel)).unwrap();
        write(backward.path(), rel.to_str().unwrap(), &body);
    }
    let a = observe(forward.path());
    let b = observe(backward.path());
    assert_eq!(a.edges, b.edges);
    assert_eq!(a.ranks, b.ranks);
    assert_eq!(a.map, b.map);
    assert_eq!(a.lens, b.lens);
}
