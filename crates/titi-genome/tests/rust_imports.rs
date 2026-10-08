//! The import shapes the line-regex scanner could not see.
//!
//! Every one of the 154 `unresolved-import` specs `genome check` produced on
//! this workspace was one of: a glob (`super::*`), a braced list truncated to
//! `crate::` or `super::entry::` by a `[^;{]+` capture, a `super::` path inside
//! an inline `#[cfg(test)] mod` (where `super` is the file's own module, not a
//! directory), a single-segment item of the crate root (`use crate::Genome`),
//! a `mod x;` beside `mod.rs`, or `use` text inside a string literal. Imports
//! now come from the syntax tree and resolve against the enclosing module, so
//! these stay quiet while paths this repo does not contain are still reported.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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

fn unresolved(genome: &Genome, path: &str) -> Vec<String> {
    genome.files[path].unresolved_imports.clone()
}

/// `use crate::Genome` names an item of the crate root, and `use super::*`
/// inside an inline `#[cfg(test)] mod` names the file's own module — neither
/// is a file the path spells out, and neither is unresolved.
#[test]
fn crate_root_items_and_inline_module_globs_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub struct Genome;\n");
    write(
        root,
        "src/uses.rs",
        "use crate::Genome;\n\
         \n\
         pub fn go(genome: Genome) -> usize {\n\
         \x20   core::mem::size_of_val(&genome)\n\
         }\n\
         \n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20   use super::*;\n\
         }\n",
    );
    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/uses.rs"]
            .imports
            .contains(&"src/lib.rs".to_owned()),
        "crate root item resolves to the root file: {:?}",
        genome.files["src/uses.rs"].imports
    );
    assert!(
        genome
            .check()
            .iter()
            .all(|item| item.code != "unresolved-import"),
        "{:?}",
        genome.check()
    );
}

/// A `mod x;` in `theme/mod.rs` names `theme/x.rs`, and a glob of a sibling
/// module is an edge. The directory form is the one the old resolver missed.
#[test]
fn directory_modules_and_sibling_globs_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub mod theme;\npub mod consumer;\n");
    write(root, "src/theme/mod.rs", "mod color;\n");
    write(root, "src/theme/color.rs", "pub struct Color;\n");
    write(root, "src/consumer.rs", "pub use crate::theme::*;\n");
    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/lib.rs"]
            .imports
            .contains(&"src/theme/mod.rs".to_owned()),
        "{:?}",
        genome.files["src/lib.rs"].imports
    );
    assert!(
        genome.files["src/theme/mod.rs"]
            .imports
            .contains(&"src/theme/color.rs".to_owned()),
        "{:?}",
        genome.files["src/theme/mod.rs"].imports
    );
    assert!(
        genome.files["src/consumer.rs"]
            .imports
            .contains(&"src/theme/mod.rs".to_owned()),
        "a glob of a module is an edge: {:?}",
        genome.files["src/consumer.rs"].imports
    );
    assert!(unresolved(&genome, "src/theme/mod.rs").is_empty());
}

/// `use`/`mod` text inside a string literal or a comment is not an import.
/// An integration test that writes a fixture as a raw string is the case that
/// produced phantom imports in this very repo.
#[test]
fn use_text_inside_a_string_or_comment_is_not_an_import() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "tests/fixture.rs",
        "fn main() {\n\
         \x20   let body = r#\"use crate::missing::Thing;\\npub mod ghost;\"#;\n\
         \x20   // use crate::commented_out::Other;\n\
         \x20   println!(\"{body}\");\n\
         }\n",
    );
    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["tests/fixture.rs"].imports.is_empty(),
        "{:?}",
        genome.files["tests/fixture.rs"].imports
    );
    assert!(unresolved(&genome, "tests/fixture.rs").is_empty());
    assert!(
        genome
            .check()
            .iter()
            .all(|item| item.code != "unresolved-import")
    );
}

/// The honest half: paths this repo does not contain are still reported — a
/// missing child module, and a module prefix that names no file.
#[test]
fn a_missing_module_and_a_missing_module_path_are_still_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "mod missing;\nuse crate::gone::thing::Thing;\n",
    );
    write(root, "src/other.rs", "use super::absent::Absent;\n");
    let genome = Genome::index(root).unwrap();
    assert_eq!(
        unresolved(&genome, "src/lib.rs"),
        ["crate::gone::thing::Thing", "missing"]
    );
    assert_eq!(
        unresolved(&genome, "src/other.rs"),
        ["super::absent::Absent"]
    );
    let reported: Vec<String> = genome
        .check()
        .iter()
        .filter(|item| item.code == "unresolved-import")
        .map(|item| item.message.clone())
        .collect();
    assert!(
        reported.iter().any(|msg| msg.contains("missing")),
        "{reported:?}"
    );
    assert!(
        reported.iter().any(|msg| msg.contains("absent")),
        "{reported:?}"
    );
}
