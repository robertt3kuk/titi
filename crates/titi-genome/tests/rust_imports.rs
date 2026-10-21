#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

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

/// `#[cfg(test)] #[path = "x_tests.rs"] mod tests;` names the file beside the
/// declaring one, not `parent/tests.rs`: the attribute decides the path, and a
/// declaration with no such file is still reported. This is how `chat.rs`
/// pulls in four test files, each of which used to be an unresolved import.
#[test]
fn a_path_attribute_names_the_module_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub mod chat;\n");
    write(
        root,
        "src/chat.rs",
        "pub fn chat() {}\n\
         \n\
         #[cfg(test)]\n\
         #[path = \"chat_tests.rs\"]\n\
         mod tests;\n\
         \n\
         #[cfg(test)]\n\
         #[path = \"gone_tests.rs\"]\n\
         mod gone;\n",
    );
    write(root, "src/chat_tests.rs", "use super::*;\n");
    let genome = Genome::index(root).unwrap();
    let chat = &genome.files["src/chat.rs"];
    assert!(
        chat.imports.contains(&"src/chat_tests.rs".to_owned()),
        "{:?}",
        chat.imports
    );
    assert_eq!(
        unresolved(&genome, "src/chat.rs"),
        vec!["gone".to_owned()],
        "only the module whose #[path] file is missing is reported"
    );
}

/// `mod x;` beside a file `x/mod.rs` resolves to the directory form, from a
/// plain `foo.rs` as well as from `mod.rs`.
#[test]
fn a_module_in_a_directory_resolves_from_a_plain_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "mod outer;\n");
    write(root, "src/outer.rs", "mod inner;\n");
    write(root, "src/outer/inner/mod.rs", "pub fn deep() {}\n");
    let genome = Genome::index(root).unwrap();
    assert_eq!(
        genome.files["src/outer.rs"].imports,
        vec!["src/outer/inner/mod.rs".to_owned()]
    );
    assert!(unresolved(&genome, "src/outer.rs").is_empty());
}

/// `mod engine; pub use engine::Engine;` is the facade idiom: `engine` is a
/// child module declared in this very file, so the path is `self::engine::…`
/// and not an external crate. The edge and the re-export must exist.
#[test]
fn a_use_of_a_module_declared_in_the_same_file_is_an_import_and_a_reexport() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "mod engine;\nmod hidden;\npub use engine::Engine;\nuse hidden::Secret;\n",
    );
    write(root, "src/engine.rs", "pub struct Engine;\n");
    write(root, "src/hidden/mod.rs", "pub struct Secret;\n");

    let genome = Genome::index(root).unwrap();
    let lib = &genome.files["src/lib.rs"];
    assert!(
        lib.reexports.contains(&"src/engine.rs".to_owned()),
        "{:?}",
        lib.reexports
    );
    assert!(
        !lib.reexports.contains(&"src/hidden/mod.rs".to_owned()),
        "a private `use` is not a re-export: {:?}",
        lib.reexports
    );
    assert!(
        lib.imports.contains(&"src/engine.rs".to_owned()),
        "{:?}",
        lib.imports
    );
    assert!(
        lib.imports.contains(&"src/hidden/mod.rs".to_owned()),
        "{:?}",
        lib.imports
    );
    assert!(lib.external_uses.is_empty(), "{:?}", lib.external_uses);
}

/// A glob over an enum's variants, over an inline module, or over a type of
/// the parent module names an item, not a file: none of them is unresolved.
#[test]
fn globs_over_enums_and_private_modules_are_not_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub mod human;\npub mod parent;\n");
    write(
        root,
        "src/human.rs",
        "use self::Kind::*;\nuse self::imp::*;\n\npub enum Kind { A, B }\n\nmod imp {\n    pub fn f() {}\n}\n",
    );
    write(
        root,
        "src/parent/mod.rs",
        "pub enum Token { X }\npub mod child;\n",
    );
    write(
        root,
        "src/parent/child.rs",
        "use super::Token::*;\nuse super::Missing::*;\npub fn t() {}\n",
    );

    let genome = Genome::index(root).unwrap();
    assert!(
        unresolved(&genome, "src/human.rs").is_empty(),
        "{:?}",
        unresolved(&genome, "src/human.rs")
    );
    // `Token` is CamelCase: a type of the parent module, which is a file. A
    // second CamelCase glob is indistinguishable from it, so it is quiet too.
    assert!(
        unresolved(&genome, "src/parent/child.rs").is_empty(),
        "{:?}",
        unresolved(&genome, "src/parent/child.rs")
    );
    assert!(
        genome.files["src/parent/child.rs"]
            .imports
            .contains(&"src/parent/mod.rs".to_owned())
    );
    // A snake_case module that does not exist is still a finding.
    write(root, "src/missing_user.rs", "use crate::nowhere::*;\n");
    let genome = Genome::index(root).unwrap();
    assert_eq!(
        unresolved(&genome, "src/missing_user.rs"),
        vec!["crate::nowhere::*".to_owned()]
    );
}
