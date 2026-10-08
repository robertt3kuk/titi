//! Java: exports, imports and the capability line, through `Genome`.
//!
//! The fixture is real-shaped: `package com.acme;` in a Maven layout, a
//! workspace type import, a JDK import, a static import and the method shapes
//! a caller can name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use titi_genome::{Genome, Language, Level};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

const APP: &str = r#"package com.acme;

import com.acme.util.Hash;
import java.util.List;
import static com.acme.util.Hash.digest;

public class App {
    public void run() {}
    private void secret() {}
    protected static <T> List<T> wrap(T value) { return null; }
    String doc = """
        public void quoted() {}
        """;
}

// public class Ghost {}
"#;

const HASH: &str =
    "package com.acme.util;\n\npublic class Hash {\n    public static void digest() {}\n}\n";

const APP_PATH: &str = "src/main/java/com/acme/App.java";
const HASH_PATH: &str = "src/main/java/com/acme/util/Hash.java";

#[test]
fn java_exports_named_methods_and_places_imports_by_package_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, APP_PATH, APP);
    write(root, HASH_PATH, HASH);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files[APP_PATH];
    let hash = &genome.files[HASH_PATH];

    // The workspace type import is the only edge: the static import names the
    // same type and dedups, the JDK import is not this workspace's.
    assert_eq!(app.imports, vec![HASH_PATH.to_owned()], "{:?}", app.imports);
    assert!(
        !app.imports.iter().any(|path| path.contains("digest")),
        "a member path is not a file: {:?}",
        app.imports
    );
    assert!(
        app.unresolved_imports.is_empty(),
        "{:?}",
        app.unresolved_imports
    );
    assert_eq!(genome.dependents[HASH_PATH], 1);

    // Public and protected methods are exports; a private method and a
    // commented-out declaration are not.
    assert!(app.exports.contains(&"App".to_owned()), "{:?}", app.exports);
    assert!(app.exports.contains(&"run".to_owned()), "{:?}", app.exports);
    assert!(
        app.exports.contains(&"wrap".to_owned()),
        "{:?}",
        app.exports
    );
    assert!(
        !app.exports.contains(&"secret".to_owned()),
        "{:?}",
        app.exports
    );
    assert!(
        !app.exports.contains(&"Ghost".to_owned()),
        "{:?}",
        app.exports
    );
    // A `public void quoted() {}` inside a text block is not a declaration:
    // the line patterns read it, the grammar knows it is string content.
    assert!(
        !app.exports.contains(&"quoted".to_owned()),
        "{:?}",
        app.exports
    );

    assert!(
        hash.exports.contains(&"Hash".to_owned()),
        "{:?}",
        hash.exports
    );
    assert!(
        hash.exports.contains(&"digest".to_owned()),
        "{:?}",
        hash.exports
    );
}

#[test]
fn java_reports_a_workspace_shaped_import_that_names_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        APP_PATH,
        "package com.acme;\n\nimport com.acme.util.Gone;\n\npublic class App {}\n",
    );
    write(root, HASH_PATH, HASH);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files[APP_PATH];
    assert_eq!(
        app.unresolved_imports,
        vec!["com.acme.util.Gone".to_owned()]
    );
    assert!(app.imports.is_empty(), "{:?}", app.imports);

    let warnings: Vec<_> = genome
        .check()
        .into_iter()
        .filter(|item| item.code == "unresolved-import")
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].message.contains("com.acme.util.Gone"),
        "{}",
        warnings[0].message
    );
}

#[test]
fn java_reports_its_level_in_the_capability_roster() {
    let java = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "java")
        .expect("a java capability");
    assert_eq!(java.level, Level::Full, "{}", java.note);
    assert_eq!(java.extensions, vec!["java"]);
    assert_eq!(Language::Java.level(), Level::Full);
}
