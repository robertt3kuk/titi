//! E3: language coverage and the symbol-level graph.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E3).

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

#[test]
fn java_imports_resolve_by_package_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/main/java/com/acme/App.java",
        r#"
package com.acme;
import com.acme.util.Hash;
public class App {
    public static void main(String[] args) {}
}
"#,
    );
    write(
        root,
        "src/main/java/com/acme/util/Hash.java",
        "package com.acme.util;\npublic class Hash {}\n",
    );

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/main/java/com/acme/App.java"]
            .imports
            .contains(&"src/main/java/com/acme/util/Hash.java".to_owned()),
        "imports: {:?}",
        genome.files["src/main/java/com/acme/App.java"].imports
    );
    assert_eq!(
        genome.dependents["src/main/java/com/acme/util/Hash.java"],
        1
    );
}

#[test]
fn go_imports_resolve_by_package_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "cmd/app/main.go",
        r#"
package main

import (
	"example.com/proj/internal/store"
)

func Main() { _ = store.Open }
"#,
    );
    write(
        root,
        "internal/store/store.go",
        "package store\n\nfunc Open() {}\n",
    );

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["cmd/app/main.go"]
            .imports
            .contains(&"internal/store/store.go".to_owned()),
        "imports: {:?}",
        genome.files["cmd/app/main.go"].imports
    );
    // Only capitalized Go names are exported.
    assert!(
        genome.files["internal/store/store.go"]
            .exports
            .contains(&"Open".to_owned())
    );
}

#[test]
fn c_includes_resolve_relative_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/main.c",
        "#include \"util/hash.h\"\nint main(void) { return hash(); }\n",
    );
    write(root, "src/util/hash.h", "int hash(void);\n");

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/main.c"]
            .imports
            .contains(&"src/util/hash.h".to_owned()),
        "imports: {:?}",
        genome.files["src/main.c"].imports
    );
}

#[test]
fn ruby_kotlin_csharp_and_php_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "lib/app.rb", "require 'helper'\n\nclass App\nend\n");
    write(root, "lib/helper.rb", "def helper\nend\n");
    write(
        root,
        "src/main/kotlin/App.kt",
        "import com.acme.Util\n\nclass App\n",
    );
    write(root, "src/main/kotlin/com/acme/Util.kt", "object Util\n");
    write(
        root,
        "src/App.cs",
        "using Acme.Util;\nnamespace Acme { public class App {} }\n",
    );
    write(
        root,
        "src/Acme/Util.cs",
        "namespace Acme { public class Util {} }\n",
    );
    write(
        root,
        "app/App.php",
        "<?php\nuse Acme\\Util;\nclass App {}\n",
    );
    write(root, "app/Acme/Util.php", "<?php\nclass Util {}\n");

    let genome = Genome::index(root).unwrap();
    let imports = |path: &str| genome.files[path].imports.clone();
    assert!(
        imports("lib/app.rb").contains(&"lib/helper.rb".to_owned()),
        "ruby: {:?}",
        imports("lib/app.rb")
    );
    assert!(
        imports("src/main/kotlin/App.kt").contains(&"src/main/kotlin/com/acme/Util.kt".to_owned()),
        "kotlin: {:?}",
        imports("src/main/kotlin/App.kt")
    );
    assert!(
        imports("src/App.cs").contains(&"src/Acme/Util.cs".to_owned()),
        "csharp: {:?}",
        imports("src/App.cs")
    );
    assert!(
        imports("app/App.php").contains(&"app/Acme/Util.php".to_owned()),
        "php: {:?}",
        imports("app/App.php")
    );
}

#[test]
fn a_mention_without_an_import_is_still_a_symbol_edge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hash.rs", "pub fn digest() {}\n");
    write(
        root,
        "src/consumer.rs",
        // No `use`, no `mod`: only a call. The file-level graph sees nothing.
        "pub fn run() { let _ = digest(); }\n",
    );

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/consumer.rs"].imports.is_empty(),
        "no import edge exists"
    );
    assert!(
        genome.files["src/consumer.rs"]
            .used_symbols
            .contains(&"digest".to_owned()),
        "used_symbols: {:?}",
        genome.files["src/consumer.rs"].used_symbols
    );
    // The symbol-level edge still makes consumer depend on hash.
    assert_eq!(
        genome.dependents["src/hash.rs"], 1,
        "a symbol mention must count as a dependent"
    );
    assert_eq!(
        genome.symbols["digest"].files,
        vec!["src/hash.rs".to_owned()]
    );
    assert_eq!(genome.symbols["digest"].users, 1);
}

#[test]
fn symbol_noise_is_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        r#"
pub fn exported() {}

pub fn caller() {
    // Keywords, primitives and the file's own exports are not references.
    let value = std::collections::HashMap::new();
    for entry in value.iter() { println!("{entry:?}"); }
    exported();
}
"#,
    );

    let genome = Genome::index(root).unwrap();
    let used = &genome.files["src/lib.rs"].used_symbols;
    for noise in ["for", "let", "new", "iter", "println", "exported"] {
        assert!(
            !used.contains(&noise.to_owned()),
            "{noise} leaked into {used:?}"
        );
    }
}

#[test]
fn the_projection_carries_symbol_user_counts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/hub.rs",
        "pub fn digest() {}\npub fn rarely_seen() {}\n",
    );
    write(root, "src/a.rs", "pub fn a() { let _ = digest(); }\n");
    write(root, "src/b.rs", "pub fn b() { let _ = digest(); }\n");

    let genome = Genome::index(root).unwrap();
    let projected = genome.project(4);
    assert!(
        projected.contains("+digest (2)"),
        "the symbol's user count must reach the map:\n{projected}"
    );
    assert!(
        projected.contains("+rarely_seen\n"),
        "an unused symbol carries no count:\n{projected}"
    );
    // `digest` is referenced by two files, so hub.rs is depended on by both.
    assert_eq!(genome.dependents["src/hub.rs"], 2);
}

#[test]
fn an_ambiguous_name_carries_no_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // `shared` is exported by two files, so a mention of it cannot be
    // attributed to either one.
    write(root, "src/a.rs", "pub fn shared() {}\n");
    write(root, "src/b.rs", "pub fn shared() {}\n");
    write(root, "src/c.rs", "pub fn c() { let _ = shared(); }\n");

    let genome = Genome::index(root).unwrap();
    assert_eq!(
        genome.dependents["src/a.rs"], 0,
        "an ambiguous name must not create a dependent"
    );
    assert_eq!(genome.dependents["src/b.rs"], 0);
    assert!(genome.files["src/c.rs"].used_symbols.is_empty());
    assert_eq!(genome.symbols["shared"].files.len(), 2, "both are recorded");
    assert_eq!(genome.symbols["shared"].users, 0, "but neither has users");
}

#[test]
fn a_unique_name_resolves_to_its_single_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/only.rs", "pub fn unique_thing() {}\n");
    write(root, "src/c.rs", "pub fn c() { unique_thing(); }\n");

    let genome = Genome::index(root).unwrap();
    assert_eq!(genome.dependents["src/only.rs"], 1);
    assert_eq!(genome.symbols["unique_thing"].users, 1);
    assert!(
        genome.project(4).contains("+unique_thing (1)"),
        "{}",
        genome.project(4)
    );
}

/// Declarations a line-anchored pattern could not see now carry edges — an
/// indented Python method, a member of an exported TS class — and ones it
/// saw but should not have (a block-commented function, a `def` inside a
/// docstring) no longer become symbols.
#[test]
fn nested_declarations_become_symbols_and_quoted_ones_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app/service.py",
        "class Service:\n    def reconcile_ledger(self):\n        pass\n\n\
         HELP = \"\"\"\ndef ghost_helper():\n    pass\n\"\"\"\n",
    );
    write(
        root,
        "app/caller.py",
        "def run(store):\n    store.reconcile_ledger()\n",
    );
    write(
        root,
        "web/client.ts",
        "export class Client {\n  dispatchEnvelope(body: string) {}\n}\n\
         /*\nexport function ghostHandler() {}\n*/\n",
    );
    write(
        root,
        "web/page.ts",
        "export function open(c: any) { c.dispatchEnvelope(\"x\"); }\n",
    );

    let genome = Genome::index(root).unwrap();

    assert_eq!(genome.symbols["reconcile_ledger"].users, 1);
    assert_eq!(
        genome.symbols["reconcile_ledger"].files,
        vec!["app/service.py"]
    );
    assert_eq!(genome.symbols["dispatchEnvelope"].users, 1);
    assert_eq!(genome.dependents["app/service.py"], 1);
    assert_eq!(genome.dependents["web/client.ts"], 1);
    for ghost in ["ghostHandler", "ghost_helper"] {
        assert!(
            !genome.symbols.contains_key(ghost),
            "{ghost} is text, not a declaration"
        );
    }
}

#[test]
fn language_detection_covers_the_indexed_extensions() {
    use titi_genome::Language;
    for (path, expected) in [
        ("a.rs", Language::Rust),
        ("a.tsx", Language::TypeScript),
        ("a.py", Language::Python),
        ("a.go", Language::Go),
        ("A.java", Language::Java),
        ("a.c", Language::C),
        ("a.h", Language::C),
        ("a.cpp", Language::Cpp),
        ("a.hpp", Language::Cpp),
        ("a.cs", Language::CSharp),
        ("a.rb", Language::Ruby),
        ("a.kt", Language::Kotlin),
        ("a.swift", Language::Swift),
        ("a.php", Language::Php),
        ("a.txt", Language::Other),
    ] {
        assert_eq!(Language::from_path(path), expected, "{path}");
    }
}
