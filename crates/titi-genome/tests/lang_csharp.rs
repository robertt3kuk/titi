//! C#: exports, imports and the capability line, through `Genome`.
//!
//! The fixture is real-shaped: a `namespace Acme` block, a BCL import, a
//! workspace namespace import and the alias form of the same import.

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

const APP: &str = r#"using System;
using Acme.Util;
using Util = Acme.Util;

namespace Acme
{
    public class App
    {
        public void Run() {}
    }
}

// public class Ghost {}
"#;

const UTIL: &str = "namespace Acme.Util\n{\n    public class Util { }\n}\n";

const APP_PATH: &str = "src/App.cs";
const UTIL_PATH: &str = "src/Acme/Util.cs";

#[test]
fn csharp_exports_types_and_places_imports_by_namespace_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, APP_PATH, APP);
    write(root, UTIL_PATH, UTIL);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files[APP_PATH];
    let util = &genome.files[UTIL_PATH];

    // `Acme.Util` and its alias spelling name the same file and dedup; the BCL
    // namespace is neither an edge nor a warning.
    assert_eq!(app.imports, vec![UTIL_PATH.to_owned()], "{:?}", app.imports);
    assert!(
        app.unresolved_imports.is_empty(),
        "{:?}",
        app.unresolved_imports
    );
    assert_eq!(genome.dependents[UTIL_PATH], 1);

    // The row claims type declarations only: `Run` is not an export.
    assert!(app.exports.contains(&"App".to_owned()), "{:?}", app.exports);
    assert!(
        !app.exports.contains(&"Run".to_owned()),
        "{:?}",
        app.exports
    );
    assert!(
        !app.exports.contains(&"Ghost".to_owned()),
        "{:?}",
        app.exports
    );
    assert!(
        util.exports.contains(&"Util".to_owned()),
        "{:?}",
        util.exports
    );
}

#[test]
fn csharp_global_and_static_using_forms_are_imports_too() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        APP_PATH,
        "global using Acme.Util;\nusing static Acme.Util;\n\nnamespace Acme { public class App {} }\n",
    );
    write(root, UTIL_PATH, UTIL);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files[APP_PATH];
    assert_eq!(app.imports, vec![UTIL_PATH.to_owned()], "{:?}", app.imports);
    assert!(
        app.unresolved_imports.is_empty(),
        "{:?}",
        app.unresolved_imports
    );
}

#[test]
fn csharp_reports_a_workspace_shaped_import_that_names_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        APP_PATH,
        "using Acme.Gone;\n\nnamespace Acme { public class App {} }\n",
    );
    write(root, UTIL_PATH, UTIL);

    let genome = Genome::index(root).unwrap();
    let app = &genome.files[APP_PATH];
    assert_eq!(app.unresolved_imports, vec!["Acme.Gone".to_owned()]);
    assert!(app.imports.is_empty(), "{:?}", app.imports);

    let warnings: Vec<_> = genome
        .check()
        .into_iter()
        .filter(|item| item.code == "unresolved-import")
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].message.contains("Acme.Gone"),
        "{}",
        warnings[0].message
    );
}

#[test]
fn csharp_reports_its_level_in_the_capability_roster() {
    let csharp = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "c#")
        .expect("a c# capability");
    assert_eq!(csharp.level, Level::Heuristic, "{}", csharp.note);
    assert_eq!(csharp.extensions, vec!["cs"]);
    assert_eq!(Language::CSharp.level(), Level::Heuristic);
}
