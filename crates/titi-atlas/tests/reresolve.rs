#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Imports the language modules could not place and a second look can.

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

/// ESM source names the emitted file: `./b.js` is `./b.ts`, and `..` names the
/// directory's `index`. Both used to be an unresolved-import warning and no
/// edge — 32 warnings on `ky`, 32 on `express`.
#[test]
fn an_esm_import_of_the_emitted_name_finds_the_typescript_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/b.ts", "export const b = 1;\n");
    write(root, "src/index.ts", "export * from './b.js';\n");
    write(
        root,
        "src/sub/c.ts",
        "import { b } from '../b.js';\nimport * as all from '..';\nexport const c = b;\n",
    );
    let atlas = Atlas::index(root).unwrap();

    let index = &atlas.files["src/index.ts"];
    assert_eq!(index.imports, vec!["src/b.ts"]);
    assert_eq!(
        index.reexports,
        vec!["src/b.ts"],
        "export … from re-publishes"
    );
    assert!(index.unresolved_imports.is_empty(), "{index:?}");

    let c = &atlas.files["src/sub/c.ts"];
    assert_eq!(c.imports, vec!["src/b.ts", "src/index.ts"]);
    assert!(c.unresolved_imports.is_empty(), "{c:?}");
    assert!(atlas.check().iter().all(|d| d.code != "unresolved-import"));
}

#[test]
fn a_relative_import_of_nothing_is_still_reported() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.ts", "import { x } from './gone.js';\n");
    let atlas = Atlas::index(dir.path()).unwrap();
    assert_eq!(
        atlas.files["src/a.ts"].unresolved_imports,
        vec!["./gone.js"]
    );
}

/// A Go package is a directory of the module; a path outside the module is a
/// dependency.
#[test]
fn a_go_import_is_a_directory_of_the_module_or_another_modules_package() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/m\n\ngo 1.21\n");
    write(root, "doc/man_docs.go", "package doc\n\nfunc GenMan() {}\n");
    write(
        root,
        "cmd.go",
        "package m\n\nimport (\n\t\"example.com/m/doc\"\n\t\"example.com/m/missing\"\n\t\"github.com/spf13/pflag\"\n)\n",
    );
    let atlas = Atlas::index(root).unwrap();

    let cmd = &atlas.files["cmd.go"];
    assert_eq!(cmd.imports, vec!["doc/man_docs.go"]);
    assert_eq!(
        cmd.unresolved_imports,
        vec!["example.com/m/missing"],
        "a dependency is not a missing file; a missing package of the module is"
    );
}
