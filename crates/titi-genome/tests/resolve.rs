#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A mention links to the file that defines it only with evidence.
//!
//! The map used to key edges and user counts by the bare *name*: any file that
//! mentioned `sync` (`use std::sync::Arc`) leaned on whichever file exported a
//! `sync`, so one common name made a file the most depended-on in the repo.
//! A use now resolves to a definer through the language's own scope rules —
//! imports, the same package — or, for a long distinctive name defined once,
//! as a weak guess.

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

fn index(files: &[(&str, &str)]) -> (tempfile::TempDir, Genome) {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in files {
        write(dir.path(), path, body);
    }
    let genome = Genome::index(dir.path()).unwrap();
    (dir, genome)
}

/// The `+Name (n)` lines the map prints under `path`.
fn export_lines(genome: &Genome, path: &str) -> Vec<String> {
    let map = genome.project(64);
    let mut lines = map.lines();
    let header = format!("{path} (→");
    if !lines.any(|line| line.starts_with(&header)) {
        return Vec::new();
    }
    lines
        .take_while(|line| line.starts_with("  +"))
        .map(|line| line.trim().to_owned())
        .collect()
}

fn dependents(genome: &Genome, path: &str) -> usize {
    genome.dependents[path]
}

// ---- the reported bug ---------------------------------------------------

/// `use std::sync::Arc` mentions `sync`; that is the standard library's, not
/// the file that happens to export a `sync` method.
#[test]
fn a_std_path_does_not_lean_on_a_file_that_exports_the_same_word() {
    let (_dir, genome) = index(&[
        (
            "src/mention.rs",
            "pub struct Mentions;\nimpl Mentions { pub fn sync(&self) {} }\n",
        ),
        (
            "src/a.rs",
            "use std::sync::Arc;\npub fn a_fn(x: Arc<u8>) -> Arc<u8> { x }\n",
        ),
        (
            "src/b.rs",
            "use std::sync::Mutex;\npub fn b_fn(x: Mutex<u8>) -> Mutex<u8> { x }\n",
        ),
    ]);
    assert_eq!(dependents(&genome, "src/mention.rs"), 0);
    assert!(export_lines(&genome, "src/mention.rs").is_empty());
}

/// A common word is never a guess: even when exactly one file defines `model`
/// and another calls `model(`, nothing says they are the same function.
#[test]
fn a_common_word_defined_once_is_not_guessed_without_an_import() {
    let (_dir, genome) = index(&[
        ("src/status.rs", "pub fn model() {}\npub fn short() {}\n"),
        ("src/other.rs", "pub fn go() { model(); short(); }\n"),
    ]);
    assert_eq!(
        dependents(&genome, "src/status.rs"),
        0,
        "common word, 5 letters, no import"
    );
}

// ---- per-definer resolution ---------------------------------------------

/// Two files define `render_item`; the one the user imports is the one it
/// leans on, and the other gains no dependent and no user.
#[test]
fn a_shared_name_resolves_to_the_definer_the_file_imports() {
    let (_dir, genome) = index(&[
        ("src/lib.rs", "mod a;\nmod b;\nmod user;\n"),
        ("src/a.rs", "pub fn render_item() {}\n"),
        ("src/b.rs", "pub fn render_item() {}\n"),
        (
            "src/user.rs",
            "use crate::a::render_item;\npub fn go() { render_item(); }\n",
        ),
    ]);
    assert_eq!(export_lines(&genome, "src/a.rs"), vec!["+render_item (1)"]);
    assert!(export_lines(&genome, "src/b.rs").is_empty());
    assert_eq!(
        dependents(&genome, "src/a.rs"),
        2,
        "lib.rs declares it, user.rs uses it"
    );
    assert_eq!(
        dependents(&genome, "src/b.rs"),
        1,
        "only the `mod b;` in lib.rs"
    );
}

/// A name two files share and a file that imports neither: no edge.
#[test]
fn an_ambiguous_name_with_no_evidence_creates_no_edge() {
    let (_dir, genome) = index(&[
        ("pkg/a.py", "def render_item():\n    pass\n"),
        ("pkg/b.py", "def render_item():\n    pass\n"),
        ("pkg/user.py", "def go():\n    render_item()\n"),
    ]);
    assert_eq!(dependents(&genome, "pkg/a.py"), 0);
    assert_eq!(dependents(&genome, "pkg/b.py"), 0);
}

/// Users are counted per definer: three files use `Shared` from `a.rs`, one
/// from `b.rs`, and each definer prints its own number.
#[test]
fn users_are_counted_per_definer_not_per_name() {
    let mut files = vec![
        ("src/lib.rs", "mod a;\nmod b;\n".to_owned()),
        ("src/a.rs", "pub struct Shared;\n".to_owned()),
        ("src/b.rs", "pub struct Shared;\n".to_owned()),
    ];
    for at in 0..3 {
        files.push((
            Box::leak(format!("src/ua{at}.rs").into_boxed_str()),
            "use crate::a::Shared;\npub fn go(_: Shared) {}\n".to_owned(),
        ));
    }
    files.push((
        "src/ub.rs",
        "use crate::b::Shared;\npub fn go(_: Shared) {}\n".to_owned(),
    ));
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, b)| (*p, b.as_str())).collect();
    let (_dir, genome) = index(&refs);
    assert_eq!(export_lines(&genome, "src/a.rs"), vec!["+Shared (3)"]);
    assert_eq!(export_lines(&genome, "src/b.rs"), vec!["+Shared (1)"]);
    assert_eq!(genome.symbols["Shared"].users, 4, "the name's total");
}

// ---- scope rules per language family -------------------------------------

/// Rust: an item reaches a file through `crate::` paths and through a crate
/// facade's `pub use`, including from another crate of the workspace.
#[test]
fn rust_resolves_through_crate_paths_and_pub_use_facades() {
    let (_dir, genome) = index(&[
        (
            "crates/prov/src/lib.rs",
            "mod stream;\npub use stream::StreamEvent;\n",
        ),
        ("crates/prov/src/stream.rs", "pub struct StreamEvent;\n"),
        ("crates/other/src/lib.rs", "pub struct StreamEvent;\n"),
        (
            "crates/eng/src/lib.rs",
            "use prov::StreamEvent;\npub fn handle(_: StreamEvent) {}\n",
        ),
    ]);
    assert_eq!(
        export_lines(&genome, "crates/prov/src/stream.rs"),
        vec!["+StreamEvent (1)"]
    );
    assert!(export_lines(&genome, "crates/other/src/lib.rs").is_empty());
    assert_eq!(dependents(&genome, "crates/other/src/lib.rs"), 0);
}

/// Rust: `use super::*` in a test module file inherits what its parent imports.
#[test]
fn rust_a_glob_of_the_parent_inherits_the_parents_imports() {
    let (_dir, genome) = index(&[
        ("src/lib.rs", "mod a;\nmod b;\nmod c;\n"),
        ("src/a.rs", "pub struct Shared;\n"),
        ("src/b.rs", "pub struct Shared;\n"),
        (
            "src/c.rs",
            "use crate::a::Shared;\npub fn go(_: Shared) {}\n#[cfg(test)]\nmod tests;\n",
        ),
        ("src/c/tests.rs", "use super::*;\nfn t(_: Shared) {}\n"),
    ]);
    assert_eq!(export_lines(&genome, "src/a.rs"), vec!["+Shared (2)"]);
}

/// Go: files of one package directory see each other without an import.
#[test]
fn go_resolves_within_the_package_directory() {
    let (_dir, genome) = index(&[
        ("store/a.go", "package store\n\nfunc OpenStore() {}\n"),
        (
            "store/b.go",
            "package store\n\nfunc use() { OpenStore() }\n",
        ),
        ("other/c.go", "package other\n\nfunc OpenStore() {}\n"),
    ]);
    assert_eq!(export_lines(&genome, "store/a.go"), vec!["+OpenStore (1)"]);
    assert_eq!(dependents(&genome, "other/c.go"), 0);
}

/// Java: same package directory, no import needed.
#[test]
fn java_resolves_within_the_package_directory() {
    let (_dir, genome) = index(&[
        ("app/Util.java", "package app;\npublic class Util {}\n"),
        (
            "app/Main.java",
            "package app;\npublic class Main { Util u; }\n",
        ),
        ("lib/Util.java", "package lib;\npublic class Util {}\n"),
    ]);
    assert_eq!(export_lines(&genome, "app/Util.java"), vec!["+Util (1)"]);
    assert_eq!(dependents(&genome, "lib/Util.java"), 0);
}

/// Python, TypeScript: only imports are evidence.
#[test]
fn python_and_typescript_resolve_through_imports_only() {
    let (_dir, genome) = index(&[
        ("py/a.py", "def render_item():\n    pass\n"),
        ("py/b.py", "def render_item():\n    pass\n"),
        (
            "py/user.py",
            "from py.a import render_item\n\ndef go():\n    render_item()\n",
        ),
        ("ts/a.ts", "export function renderItem() {}\n"),
        ("ts/b.ts", "export function renderItem() {}\n"),
        (
            "ts/user.ts",
            "import { renderItem } from \"./b\";\nrenderItem();\n",
        ),
        ("ts/lonely.ts", "renderItem();\n"),
    ]);
    assert_eq!(export_lines(&genome, "py/a.py"), vec!["+render_item (1)"]);
    assert!(export_lines(&genome, "py/b.py").is_empty());
    assert_eq!(export_lines(&genome, "ts/b.ts"), vec!["+renderItem (1)"]);
    assert!(export_lines(&genome, "ts/a.ts").is_empty());
}

/// A TypeScript barrel's `export … from` carries the name through.
#[test]
fn typescript_resolves_through_a_barrel() {
    let (_dir, genome) = index(&[
        ("ui/index.ts", "export { renderItem } from \"./item\";\n"),
        ("ui/item.ts", "export function renderItem() {}\n"),
        ("ui/other.ts", "export function renderItem() {}\n"),
        (
            "app.ts",
            "import { renderItem } from \"./ui\";\nrenderItem();\n",
        ),
    ]);
    assert_eq!(export_lines(&genome, "ui/item.ts"), vec!["+renderItem (1)"]);
}

// ---- the distinctive-name fallback ----------------------------------------

/// A long name one file defines, used with no import in sight, still links —
/// the index may not have understood the import — but weighs less than a
/// real dependency.
#[test]
fn a_distinctive_unique_name_links_without_an_import() {
    let (_dir, genome) = index(&[
        ("src/hub.rs", "pub fn reconcile_ledger() {}\n"),
        ("src/user.rs", "pub fn go() { reconcile_ledger(); }\n"),
    ]);
    assert_eq!(dependents(&genome, "src/hub.rs"), 1);
    assert_eq!(
        export_lines(&genome, "src/hub.rs"),
        vec!["+reconcile_ledger (1)"]
    );
}

#[test]
fn a_short_unique_name_is_not_distinctive() {
    let (_dir, genome) = index(&[
        ("src/hub.rs", "pub fn hub() {}\n"),
        ("src/user.rs", "pub fn go() { hub(); }\n"),
    ]);
    assert_eq!(dependents(&genome, "src/hub.rs"), 0);
}

/// A name `use std::…` binds is the standard library's, even when a project
/// file the importer also reaches defines a `Command` of its own.
#[test]
fn a_std_import_shadows_a_project_type_of_the_same_name() {
    let (_dir, genome) = index(&[
        ("src/lib.rs", "mod pickers;\nmod spawn;\nmod palette;\n"),
        ("src/pickers.rs", "pub enum Command { Quit }\n"),
        (
            "src/spawn.rs",
            "use crate::pickers::Picker;\nuse std::process::Command;\n\
             pub fn go() { let _ = Command::new(\"ls\"); }\n",
        ),
        (
            "src/palette.rs",
            "use crate::pickers::*;\npub fn go(_: Command) {}\n",
        ),
    ]);
    assert_eq!(
        export_lines(&genome, "src/pickers.rs"),
        vec!["+Command (1)"]
    );
    assert!(
        genome.files["src/spawn.rs"]
            .uses
            .iter()
            .all(|used| used.name != "Command")
    );
}

/// `std::process::Command` written out in full is not a use either.
#[test]
fn a_fully_written_std_path_is_not_a_use() {
    let (_dir, genome) = index(&[
        ("src/pickers.rs", "pub enum Command { Quit }\n"),
        (
            "src/spawn.rs",
            "pub fn go() { let _ = std::process::Command::new(\"ls\"); }\n",
        ),
    ]);
    assert_eq!(dependents(&genome, "src/pickers.rs"), 0);
}

/// A guess needs a definition in the same language: a Rust file mentioning
/// `Provider` is not leaning on a Python class of that name.
#[test]
fn a_guess_never_crosses_languages() {
    let (_dir, genome) = index(&[
        ("scripts/bench.py", "class ProviderHarness:\n    pass\n"),
        ("src/user.rs", "pub fn go(_: ProviderHarness) {}\n"),
        (
            "scripts/other.py",
            "def go(x: ProviderHarness):\n    pass\n",
        ),
    ]);
    // The Python file has no import either, so it is a guess — and the only
    // dependent.
    assert_eq!(dependents(&genome, "scripts/bench.py"), 1);
    assert!(genome.files["src/user.rs"].uses.is_empty());
}

/// A common word resolves only to a file the user imports itself: reaching
/// the definer through another crate's facade is no evidence for `table`.
#[test]
fn a_common_word_needs_a_direct_import() {
    let (_dir, genome) = index(&[
        ("crates/lib/src/lib.rs", "pub mod grid;\n"),
        (
            "crates/lib/src/grid.rs",
            "pub fn table() {}\npub fn render_grid() {}\n",
        ),
        (
            "crates/app/src/main.rs",
            "use lib::grid::render_grid;\nfn main() { table(); render_grid(); }\n",
        ),
        (
            "crates/app/src/direct.rs",
            "use crate::local::*;\nfn go() { table(); }\n",
        ),
        ("crates/app/src/local.rs", "pub fn table() {}\n"),
    ]);
    let used: Vec<String> = genome.files["crates/app/src/main.rs"].used_symbols();
    assert_eq!(
        used,
        vec!["render_grid".to_owned()],
        "`table` is not resolved through the facade"
    );
}

/// Adding an import moves what a file can reach, and with it the resolution
/// of names whose definers did not change.
#[test]
fn an_added_import_resolves_a_name_that_was_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (path, body) in [
        ("src/lib.rs", "mod a;\nmod b;\nmod user;\n"),
        ("src/a.rs", "pub fn render_item() {}\n"),
        ("src/b.rs", "pub fn render_item() {}\n"),
        ("src/user.rs", "pub fn go() { render_item(); }\n"),
    ] {
        write(root, path, body);
    }
    let mut genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/user.rs"].uses.is_empty(),
        "two definers, no evidence"
    );

    write(
        root,
        "src/user.rs",
        "use crate::b::render_item;\npub fn go() { render_item(); }\n",
    );
    let stats = genome.refresh(root).unwrap();
    assert!(stats.graph_recomputed);
    assert_eq!(export_lines(&genome, "src/b.rs"), vec!["+render_item (1)"]);
    assert!(export_lines(&genome, "src/a.rs").is_empty());
}

/// `pub mod x;` is a pointer to the file that defines `x`, not a second
/// definition: a `x::` path is not a use of the declaring file.
#[test]
fn a_module_declaration_is_not_a_definition() {
    let (_dir, genome) = index(&[
        ("src/lib.rs", "pub mod theme_store;\npub mod caller;\n"),
        ("src/theme_store.rs", "pub fn load() {}\n"),
        (
            "src/caller.rs",
            "use crate::theme_store;\npub fn go() { theme_store::load(); }\n",
        ),
    ]);
    assert!(genome.files["src/lib.rs"].exports.is_empty());
    assert_eq!(
        genome.files["src/caller.rs"].used_symbols(),
        vec!["load".to_owned()],
        "`load` is a use of the module file; `theme_store` is a use of nothing"
    );
    assert_eq!(
        dependents(&genome, "src/theme_store.rs"),
        2,
        "lib.rs and caller.rs, by import"
    );
}
