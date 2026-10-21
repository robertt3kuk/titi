#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! What a path says about a file's job: a test, an example or a generated file
//! is not the library, and an entry point is the root the library hangs from.
//!
//! The map is for orientation. A repo whose tests import its core a hundred
//! times used to list `test/support/utils.js` first, and a binary's `main.rs`,
//! which nothing imports, never made the page.

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

fn order(genome: &Genome, limit: usize) -> Vec<String> {
    genome
        .project(limit)
        .lines()
        .filter(|line| !line.starts_with(' ') && !line.starts_with('<'))
        .map(|line| line.split(" (").next().unwrap().to_owned())
        .collect()
}

/// A helper that a dozen tests import is the busiest node in the graph and not
/// a file anyone is asked to change when they work on the product.
#[test]
fn a_test_helper_does_not_outrank_the_library_it_exercises() {
    let mut files: Vec<(String, String)> = vec![
        (
            "lib/core.js".into(),
            "export function core() { return 1 }\n".into(),
        ),
        (
            "lib/app.js".into(),
            "import { core } from './core.js'\nexport function app() { return core() }\n".into(),
        ),
        (
            "test/support/utils.js".into(),
            "export function helperThing() { return 2 }\n".into(),
        ),
    ];
    for n in 0..12 {
        files.push((
            format!("test/case{n}.js"),
            "import { helperThing } from './support/utils.js'\nhelperThing()\n".into(),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let (_dir, genome) = index(&refs);

    let map = order(&genome, 24);
    let at = |path: &str| map.iter().position(|p| p == path).unwrap();
    assert!(
        at("lib/core.js") < at("test/support/utils.js"),
        "library first, got {map:?}"
    );
    assert!(at("lib/app.js") < at("test/support/utils.js"), "{map:?}");
    // The demotion is of ranking only: the count is still the number of files
    // that import it.
    assert_eq!(genome.dependents["test/support/utils.js"], 12);
}

/// Naming a file as touched is the way back: the person working on a test
/// wants it on the page.
#[test]
fn a_touched_test_file_is_not_demoted() {
    let mut files: Vec<(String, String)> = vec![(
        "src/lib.rs".into(),
        "pub fn lib_fn() {}\n".into(),
    )];
    for n in 0..6 {
        files.push((
            format!("src/user{n}.rs"),
            "use crate::lib_fn;\npub fn f() { lib_fn(); }\n".into(),
        ));
    }
    files.push((
        "tests/support.rs".into(),
        "pub fn support_fn() {}\n".into(),
    ));
    for name in ["a", "b", "c"] {
        files.push((
            format!("tests/{name}.rs"),
            "use support_fn;\nfn t() { support_fn(); }\n".into(),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let (_dir, genome) = index(&refs);

    let plain = genome.project(2);
    assert!(!plain.contains("tests/support.rs"), "{plain}");
    let touched = genome.project_with(2, &["tests/support.rs".to_owned()]);
    assert!(touched.contains("tests/support.rs"), "{touched}");
}

/// Nothing imports `main.rs`, and the crates it names are the product. It is
/// still where a reader starts.
#[test]
fn an_entry_point_makes_the_map_though_nothing_imports_it() {
    let mut files: Vec<(String, String)> = vec![(
        "z/main.rs".into(),
        "mod engine;\nfn main() { engine::run(); }\n".into(),
    )];
    files.push((
        "z/engine.rs".into(),
        "pub fn run() {}\n".into(),
    ));
    // Plenty of busier files around it.
    for n in 0..10 {
        files.push((
            format!("src/m{n}.rs"),
            format!("pub fn m{n}_fn_name() {{}}\n"),
        ));
    }
    for n in 0..10 {
        for k in 0..4 {
            files.push((
                format!("src/u{n}_{k}.rs"),
                format!("use crate::m{n}::m{n}_fn_name;\nfn f() {{ m{n}_fn_name(); }}\n"),
            ));
        }
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let (_dir, genome) = index(&refs);

    let map = order(&genome, 12);
    assert!(
        map.contains(&"z/main.rs".to_owned()),
        "entry point missing from {map:?}"
    );
}

/// Generated code is a dependency of nobody's attention.
#[test]
fn generated_and_fixture_files_rank_below_source() {
    let mut files: Vec<(String, String)> = vec![
        (
            "src/api.ts".into(),
            "export class ApiClient {}\n".into(),
        ),
        (
            "src/api.generated.ts".into(),
            "export class GeneratedModels {}\n".into(),
        ),
        (
            "tests/fixtures/data.ts".into(),
            "export class FixtureData {}\n".into(),
        ),
    ];
    for n in 0..5 {
        files.push((
            format!("src/use{n}.ts"),
            "import { ApiClient } from './api'\nimport { GeneratedModels } from './api.generated'\nimport { FixtureData } from '../tests/fixtures/data'\nnew ApiClient(); new GeneratedModels(); new FixtureData();\n".into(),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let (_dir, genome) = index(&refs);

    let map = order(&genome, 24);
    let at = |path: &str| map.iter().position(|p| p == path).unwrap();
    assert!(at("src/api.ts") < at("src/api.generated.ts"), "{map:?}");
    assert!(at("src/api.ts") < at("tests/fixtures/data.ts"), "{map:?}");
}

/// `MapFrom` is the library's method name and, once, a class a test file
/// declares for itself. A hundred library files calling `.MapFrom(` are not
/// users of that class.
#[test]
fn a_production_file_does_not_guess_a_definition_into_test_code() {
    let mut files: Vec<(String, String)> = vec![(
        "src/UnitTests/Bug.cs".into(),
        "public class MapFromHelper { }\n".into(),
    )];
    for n in 0..5 {
        files.push((
            format!("src/Lib/Thing{n}.cs"),
            format!("public class Thing{n} {{ void Go() {{ var x = new MapFromHelper(); }} }}\n"),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let (_dir, genome) = index(&refs);

    assert_eq!(
        genome.dependents["src/UnitTests/Bug.cs"],
        0,
        "production code pointed at a test-local class by name alone"
    );
}

