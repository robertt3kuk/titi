#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A name a file imports is a name it uses, whether or not it is called.
//!
//! The reference collector takes calls, `::name` paths and capitalised names;
//! a bare lowercase identifier is skipped on purpose, because a local called
//! `path` is not a dependency on whatever file exports `path`. An import
//! statement is different evidence: it binds the name to a definition, and the
//! file then uses it as a value, passes it on, or stores it.

use std::fs;
use std::path::Path;

use titi_atlas::Atlas;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn users(atlas: &Atlas, definer: &str, name: &str) -> usize {
    atlas
        .used_by
        .get(definer)
        .and_then(|names| names.iter().find(|(n, _)| n == name))
        .map_or(0, |(_, users)| *users)
}

#[test]
fn a_typescript_import_used_as_a_value_is_a_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/format.ts",
        "export function formatDate(d: string) { return d; }\n",
    );
    write(
        root,
        "src/consumer.ts",
        "import { formatDate as fmt, parse } from \"./format\";\nexport const handlers = [formatDate];\n",
    );
    write(
        root,
        "src/caller.ts",
        "import { formatDate } from \"./format\";\nformatDate(\"x\");\n",
    );
    let atlas = Atlas::index(root).unwrap();
    assert_eq!(users(&atlas, "src/format.ts", "formatDate"), 2);
}

#[test]
fn a_python_from_import_used_as_a_value_is_a_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "pkg/util.py", "def slugify(s):\n    return s\n");
    write(
        root,
        "pkg/core.py",
        "from .util import slugify\n\ndef make():\n    return map(slugify, [])\n\ndef other():\n    pass\n",
    );
    let atlas = Atlas::index(root).unwrap();
    assert_eq!(users(&atlas, "pkg/util.py", "slugify"), 1);
}

#[test]
fn a_rust_use_of_a_function_passed_by_name_is_a_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "mod util;\nmod run;\n");
    write(
        root,
        "src/util.rs",
        "pub fn helper_function() {}\npub fn another_helper() {}\n",
    );
    write(
        root,
        "src/run.rs",
        "use crate::util::{another_helper, helper_function};\n\npub fn go() {\n    let f = helper_function;\n    let _ = (f, another_helper);\n}\n",
    );
    let atlas = Atlas::index(root).unwrap();
    assert_eq!(users(&atlas, "src/util.rs", "helper_function"), 1);
    assert_eq!(users(&atlas, "src/util.rs", "another_helper"), 1);
}

/// A local that merely shares a name with an export is still not a use.
#[test]
fn a_local_with_the_same_name_as_an_export_is_still_not_a_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/format.ts",
        "export function formatDate(d: string) { return d; }\n",
    );
    write(
        root,
        "src/local.ts",
        "export function other() { const formatDate = 1; return formatDate; }\n",
    );
    let atlas = Atlas::index(root).unwrap();
    assert_eq!(users(&atlas, "src/format.ts", "formatDate"), 0);
}
