//! Go extraction: single-line and grouped declarations, masked comments, and
//! the honest placement of an import path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use titi_genome::{Genome, Severity};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// The fixture is a package a compiler would accept: a grouped import block
/// holds a standard-library path, a blank import, an aliased import of a
/// workspace package and a module path that names no file; the declarations
/// cover the single-line forms, all three group keywords, an unexported
/// function, lowercase group members and a commented-out declaration.
fn go_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "cmd/app/main.go",
        r#"package main

import st "example.com/proj/internal/store"

import (
	"fmt"
	_ "os"
	_ "example.com/proj/internal/missing"
)

/*
func Ghost() {}
import "example.com/proj/internal/ghost"
*/

func Main() {
	fmt.Println(st.Open)
}

func helper() {}

// func Gone() {}

type (
	Store struct{}
	cache struct{} )

const (
	MaxRetries = 3
	secret     = "s"
)

var (
	ErrNotFound = st.ErrNotFound
	hidden      = 1
)
"#,
    );
    write(
        root,
        "internal/store/store.go",
        r#"package store

var ErrNotFound = error(nil)

type Store struct{}

func Open() {}
"#,
    );
    dir
}

#[test]
fn go_exports_come_from_single_lines_and_groups() {
    let dir = go_repo();
    let genome = Genome::index(dir.path()).unwrap();
    let main = &genome.files["cmd/app/main.go"];

    for exported in ["Main", "Store", "MaxRetries", "ErrNotFound"] {
        assert!(
            main.exports.contains(&exported.to_owned()),
            "`{exported}` should be an export of cmd/app/main.go: {:?}",
            main.exports
        );
    }
    // Lowercase group members, an unexported function and commented-out
    // declarations are not exports — and neither the group members nor the
    // comments are visible to the patterns at all.
    for private in ["helper", "cache", "secret", "hidden", "Ghost", "Gone"] {
        assert!(
            !main.exports.contains(&private.to_owned()),
            "`{private}` must not be an export of cmd/app/main.go: {:?}",
            main.exports
        );
    }

    let store = &genome.files["internal/store/store.go"];
    assert!(
        store.exports.contains(&"Open".to_owned()),
        "{:?}",
        store.exports
    );
    assert!(
        store.exports.contains(&"Store".to_owned()),
        "{:?}",
        store.exports
    );
}

#[test]
fn go_imports_resolve_and_std_paths_stay_quiet() {
    let dir = go_repo();
    let genome = Genome::index(dir.path()).unwrap();
    let main = &genome.files["cmd/app/main.go"];

    // The aliased single-line workspace import resolves to the file it names.
    assert!(
        main.imports.contains(&"internal/store/store.go".to_owned()),
        "imports: {:?}",
        main.imports
    );
    assert_eq!(genome.dependents["internal/store/store.go"], 1);

    // An import written inside a block comment is not an import.
    assert!(
        !main
            .unresolved_imports
            .iter()
            .any(|spec| spec.contains("ghost")),
        "{:?}",
        main.unresolved_imports
    );

    // A standard-library path (no dot in its first segment) is not this
    // workspace's business: no edge and no warning.
    assert!(
        !main.unresolved_imports.contains(&"fmt".to_owned()),
        "{:?}",
        main.unresolved_imports
    );
    assert!(
        !main.unresolved_imports.contains(&"os".to_owned()),
        "{:?}",
        main.unresolved_imports
    );
    assert!(!main.imports.iter().any(|path| path.contains("fmt")));

    // A module path that names no file is a real diagnostic.
    assert!(
        main.unresolved_imports
            .contains(&"example.com/proj/internal/missing".to_owned()),
        "unresolved: {:?}",
        main.unresolved_imports
    );

    let diagnostics = genome.check();
    let unresolved: Vec<_> = diagnostics
        .iter()
        .filter(|item| item.code == "unresolved-import")
        .collect();
    assert!(
        unresolved
            .iter()
            .any(|item| item.message.contains("missing")),
        "{unresolved:?}"
    );
    assert!(
        !unresolved.iter().any(|item| item.message.contains("fmt")),
        "std paths must not produce a warning: {unresolved:?}"
    );
    assert!(
        !unresolved.iter().any(|item| item.message.contains("ghost")),
        "a commented-out import must not produce a warning: {unresolved:?}"
    );

    let capability = diagnostics
        .iter()
        .find(|item| item.code == "capability" && item.message.contains("go: Heuristic"))
        .unwrap_or_else(|| panic!("no go capability line: {diagnostics:?}"));
    assert_eq!(
        capability.severity,
        Severity::Info,
        "{}",
        capability.message
    );
}
