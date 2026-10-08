//! Kotlin: comment masking, receiver names and package-rooted import
//! placement, through the [`Genome`] a user gets.

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

fn app(root: &Path) {
    write(
        root,
        "src/main/kotlin/com/acme/App.kt",
        r#"package com.acme

import com.acme.Util
import java.util.List

class App {
    fun run() {}
}

fun String.toSlug(): String = this

fun <T> first(list: List<T>): T? = null

val retries: Int = 3

typealias Slug = String

// class Ghost
"#,
    );
    write(
        root,
        "src/main/kotlin/com/acme/Util.kt",
        "package com.acme\n\nobject Util\n",
    );
}

/// A file whose import names a workspace-shaped package with no file behind it.
fn legacy(root: &Path) {
    write(
        root,
        "src/main/kotlin/com/acme/Legacy.kt",
        "package com.acme\n\nimport com.acme.missing.Thing\n\nclass Legacy\n",
    );
}

#[test]
fn kotlin_exports_skip_comments_and_unwrap_the_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["src/main/kotlin/com/acme/App.kt"];
    for name in ["App", "run", "toSlug", "first", "retries", "Slug"] {
        assert!(
            app.exports.iter().any(|export| export == name),
            "missing export {name}: {:?}",
            app.exports
        );
    }
    assert!(
        !app.exports.iter().any(|export| export == "String"),
        "the receiver is not the function name: {:?}",
        app.exports
    );
    assert!(
        !app.exports.iter().any(|export| export == "Ghost"),
        "a commented-out declaration is not an export: {:?}",
        app.exports
    );
    assert!(
        genome.files["src/main/kotlin/com/acme/Util.kt"]
            .exports
            .contains(&"Util".to_owned())
    );
}

#[test]
fn kotlin_imports_resolve_by_package_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["src/main/kotlin/com/acme/App.kt"];
    assert!(
        app.imports
            .contains(&"src/main/kotlin/com/acme/Util.kt".to_owned()),
        "imports: {:?}",
        app.imports
    );
    assert_eq!(genome.dependents["src/main/kotlin/com/acme/Util.kt"], 1);
}

#[test]
fn kotlin_splits_library_imports_from_missing_workspace_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);
    legacy(root);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files["src/main/kotlin/com/acme/App.kt"];
    assert!(
        !app.unresolved_imports
            .iter()
            .any(|spec| spec.contains("java")),
        "a JDK import is not a missing file: {:?}",
        app.unresolved_imports
    );
    assert!(
        !app.imports.iter().any(|path| path.contains("java")),
        "a JDK import is not an edge: {:?}",
        app.imports
    );
    assert!(
        genome.files["src/main/kotlin/com/acme/Legacy.kt"]
            .unresolved_imports
            .iter()
            .any(|spec| spec == "com.acme.missing.Thing"),
        "a workspace-shaped specifier with no file is reported: {:?}",
        genome.files["src/main/kotlin/com/acme/Legacy.kt"].unresolved_imports
    );
    let warning = genome
        .check()
        .into_iter()
        .find(|item| item.code == "unresolved-import")
        .expect("unresolved-import diagnostic");
    assert!(
        warning.message.contains("com.acme.missing.Thing"),
        "message: {}",
        warning.message
    );
}

#[test]
fn kotlin_is_a_heuristic_language_in_check() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    app(root);

    let genome = Genome::index(root).unwrap();
    let capability = genome
        .check()
        .into_iter()
        .find(|item| item.code == "capability" && item.path.ends_with(".kt"))
        .expect("kotlin capability line");
    assert!(
        capability.message.contains("kotlin") && capability.message.contains("Heuristic"),
        "message: {}",
        capability.message
    );
}
