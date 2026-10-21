#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A workspace crate is named by its manifest, and a file that names it leans
//! on the crate's `lib.rs`.
//!
//! `crates/core` can be `foo-core`, and everything else then writes
//! `use foo_core::X`. The index took the crate's name from its directory, so
//! the use named nothing, and no file ever drew an edge to a facade `lib.rs`.

use std::fs;
use std::path::Path;

use titi_genome::Genome;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn workspace() -> (tempfile::TempDir, Genome) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"foo-core\"\nversion = \"0.1.0\"\n",
    );
    write(root, "crates/core/src/lib.rs", "pub mod engine;\n");
    write(root, "crates/core/src/engine.rs", "pub struct Engine {}\n");
    // A second definer of the same name: only the crate the file names can
    // settle which one it means.
    write(root, "crates/other/Cargo.toml", "[package]\nname = \"other\"\n");
    write(root, "crates/other/src/lib.rs", "pub struct Engine {}\n");
    write(
        root,
        "crates/app/src/main.rs",
        "use foo_core::Engine;\nfn main() { let _ = Engine {}; }\n",
    );
    let genome = Genome::index(root).unwrap();
    (dir, genome)
}

#[test]
fn a_crate_is_found_by_its_manifest_name_not_its_directory() {
    let (_dir, genome) = workspace();
    let uses = &genome.files["crates/app/src/main.rs"].uses;
    let engine = uses.iter().find(|used| used.name == "Engine").unwrap();
    assert_eq!(engine.definer, "crates/core/src/engine.rs");
}

#[test]
fn naming_a_crate_is_an_edge_to_its_lib_rs() {
    let (_dir, genome) = workspace();
    assert_eq!(genome.dependents["crates/core/src/lib.rs"], 1);
    assert_eq!(genome.dependents["crates/other/src/lib.rs"], 0);
}
