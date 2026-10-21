#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! E3: TypeScript/JavaScript import extraction — relative paths, monorepo
//! aliases, `require` and dynamic `import`, without warning on bare packages.

use std::fs;
use std::path::Path;

use titi_genome::{Genome, Level};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn index(root: &Path) -> Genome {
    Genome::index(root).unwrap()
}

const APP: &str = r#"
import { helper } from './util'
import { Widget } from 'widgets/Widget'
import React from 'react'
const legacy = require('./legacy')
const lazy = () => import('./dynamic')

// import { ghost } from './ghost'

export function main() {
  return new Widget(helper()).send()
}

function privateHelper() {}

// export function gone() {}
"#;

/// A relative path, a monorepo alias and a `require`/dynamic-import call all
/// name workspace files; a bare package is neither an edge nor a warning.
#[test]
fn typescript_relative_alias_and_call_specifiers_reach_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/app.ts", APP);
    write(
        root,
        "src/util.ts",
        "export function helper() {\n  return 1\n}\n",
    );
    write(
        root,
        "src/widgets/Widget.ts",
        "export class Widget {\n  send() {\n    return null\n  }\n}\n",
    );
    write(
        root,
        "src/legacy.js",
        "module.exports = function legacy() {\n  return 1\n}\n",
    );
    write(root, "src/dynamic.ts", "export const value = 42\n");

    let genome = index(root);
    let app = &genome.files["src/app.ts"];
    assert_eq!(
        app.imports,
        vec![
            "src/dynamic.ts".to_owned(),
            "src/legacy.js".to_owned(),
            "src/util.ts".to_owned(),
            "src/widgets/Widget.ts".to_owned(),
        ],
        "imports: {:?}",
        app.imports
    );
    // `react` is a bare package and the commented import is in a comment:
    // neither is an edge and neither is a warning.
    assert!(
        app.unresolved_imports.is_empty(),
        "unresolved: {:?}",
        app.unresolved_imports
    );

    // Exports come from the grammar, so a non-exported and a commented-out
    // function stay out.
    assert_eq!(app.exports, vec!["main".to_owned()]);
}

/// A relative specifier that names no file is a real broken path in the code
/// and stays a warning; a bare package is not.
#[test]
fn typescript_relative_miss_warns_and_a_package_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/app.ts", APP);
    write(
        root,
        "src/util.ts",
        "export function helper() {\n  return 1\n}\n",
    );
    write(
        root,
        "src/widgets/Widget.ts",
        "export class Widget {\n  send() {\n    return null\n  }\n}\n",
    );
    write(
        root,
        "src/legacy.js",
        "module.exports = function legacy() {\n  return 1\n}\n",
    );
    write(root, "src/dynamic.ts", "export const value = 42\n");
    write(
        root,
        "src/broken.ts",
        "import { x } from './missing'\n\nexport const broken = x\n",
    );

    let genome = index(root);
    assert_eq!(
        genome.files["src/broken.ts"].unresolved_imports,
        vec!["./missing".to_owned()]
    );
    let warnings: Vec<String> = genome
        .check()
        .into_iter()
        .filter(|item| item.code == "unresolved-import")
        .map(|item| item.message)
        .collect();
    assert_eq!(warnings, vec!["unresolved import `./missing`".to_owned()]);
}

/// The level a user sees comes from the roster, and the two languages this
/// module serves have separate entries for their separate grammars.
#[test]
fn typescript_reports_its_level_in_the_capability_roster() {
    let typescript = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "typescript")
        .expect("a typescript capability");
    assert_eq!(typescript.level, Level::Full, "{}", typescript.note);
    assert_eq!(typescript.extensions, vec!["ts", "mts", "cts", "tsx"]);

    let javascript = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "javascript")
        .expect("a javascript capability");
    assert_eq!(javascript.level, Level::Full, "{}", javascript.note);
    assert_eq!(javascript.extensions, vec!["js", "jsx", "mjs", "cjs"]);
}

/// An express-shaped tree: `require('../..')` and `require('..')` name a
/// directory's `index.js`, and CommonJS exports give lib files symbols, so a
/// use of `createApplication` in an example is an edge to `lib/express.js`.
#[test]
fn commonjs_directory_requires_and_exports_reach_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "index.js",
        "module.exports = require('./lib/express');\n",
    );
    write(
        root,
        "lib/express.js",
        "exports = module.exports = createApplication;\nfunction createApplication() {}\nexports.Router = function Router() {};\n",
    );
    write(
        root,
        "examples/auth/index.js",
        "var express = require('../..');\nvar app = createApplication();\n",
    );
    write(
        root,
        "lib/use.js",
        "var root = require('..');\nvar router = require('./express');\nRouter();\n",
    );

    let genome = index(root);
    assert_eq!(
        genome.files["lib/express.js"].exports,
        vec!["Router".to_owned(), "createApplication".to_owned()]
    );
    assert_eq!(
        genome.files["examples/auth/index.js"].imports,
        vec!["index.js".to_owned()]
    );
    assert_eq!(
        genome.files["lib/use.js"].imports,
        vec!["index.js".to_owned(), "lib/express.js".to_owned()]
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

/// A barrel publishes what it re-exports: `export { b as bee } from './b'`
/// exports `bee` (an alias the barrel itself defines), and both it and `export * from './a'` are re-exports, so a
/// file importing the barrel reaches the definers.
#[test]
fn typescript_barrels_publish_names_and_credit_the_definers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/index.ts",
        "export * from './a'\nexport { b as bee } from './b'\nexport type { T } from './t'\n",
    );
    write(root, "src/a.ts", "export function alpha() {}\n");
    write(root, "src/b.ts", "export function b() {}\n");
    write(root, "src/t.ts", "export type T = string\n");
    write(
        root,
        "src/use.ts",
        "import { alpha, bee } from './index'\nalpha(); bee()\n",
    );

    let genome = index(root);
    let barrel = &genome.files["src/index.ts"];
    assert_eq!(barrel.exports, vec!["T".to_owned(), "bee".to_owned()]);
    assert_eq!(
        barrel.reexports,
        vec![
            "src/a.ts".to_owned(),
            "src/b.ts".to_owned(),
            "src/t.ts".to_owned()
        ]
    );
    let uses = &genome.files["src/use.ts"].uses;
    for (name, definer) in [("alpha", "src/a.ts"), ("bee", "src/index.ts")] {
        assert!(
            uses.iter().any(|u| u.name == name && u.definer == definer),
            "{name} should reach {definer}: {uses:?}"
        );
    }
}
