//! The grammar limitation that made 13 clean files look broken.
//!
//! The pinned `tree-sitter-rust` 0.23 reads `&raw` as the opening of a raw
//! borrow — `&raw const x`, `&raw mut x` — and fails on `&raw` where `raw` is
//! an ordinary identifier being borrowed in place: `consume(&raw)`,
//! `&raw[0]`, `assemble(&raw, false)`. Every one of the 13 files `genome check`
//! reported on this workspace was that token, not a macro, not edition-2024
//! syntax, and not a truncated read. These tests pin both halves: the borrow
//! stays quiet, and a genuinely unparseable file is still reported.

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

/// The counter's false positive: borrowing an identifier named `raw`.
#[test]
fn borrowing_an_identifier_named_raw_is_not_a_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "fn consume(units: &[u8]) -> usize {\n\
         \x20   units.len()\n\
         }\n\
         \n\
         pub fn size(raw: &[u8]) -> usize {\n\
         \x20   let head = &raw[0];\n\
         \x20   let mut total = consume(&raw);\n\
         \x20   total += consume(&raw);\n\
         \x20   total + *head as usize\n\
         }\n",
    );
    let genome = Genome::index(root).unwrap();
    assert_eq!(
        genome.files["src/lib.rs"].syntax_errors, 0,
        "a borrow of `raw` is not a syntax error"
    );
    assert!(
        genome
            .check()
            .iter()
            .all(|item| item.code != "syntax-error"),
        "{:?}",
        genome.check()
    );
}

/// The raw borrow the grammar does support stays parseable, so neutralising
/// the ambiguous `&raw` must not have swallowed it.
#[test]
fn a_real_raw_borrow_parses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "pub fn addr(raw: u8) -> *const u8 {\n\
         \x20   &raw const raw\n\
         }\n",
    );
    let genome = Genome::index(root).unwrap();
    assert_eq!(genome.files["src/lib.rs"].syntax_errors, 0);
}

/// Neutralising the grammar's `&raw` ambiguity must not hide a real error: a
/// file with both still reports one.
#[test]
fn a_genuinely_broken_file_still_reports_a_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/broken.rs",
        "pub fn ok(raw: &[u8]) -> usize {\n\
         \x20   let head = &raw[0];\n\
         \x20   *head as usize\n\
         }\n\
         pub fn broken( {\n",
    );
    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/broken.rs"].syntax_errors > 0,
        "the unmatched brace is still an error"
    );
    let hit = genome
        .check()
        .into_iter()
        .find(|item| item.code == "syntax-error")
        .expect("syntax-error");
    assert!(hit.message.contains("src/broken.rs"), "{}", hit.message);
}
