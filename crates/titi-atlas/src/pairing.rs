//! Which test file is the test of which source file.
//!
//! A test is rarely imported by what it tests, so the dependency graph links
//! it only one way, from the test to the code. Naming is how people pair them
//! (`foo.rs` and `tests/foo.rs`, `foo.py` and `test_foo.py`, `Foo.java` and
//! `FooTest.java`, `foo.ts` and `foo.test.ts`, `foo.go` and `foo_test.go`), and
//! the pairing answers two questions the graph cannot: which tests to run for
//! a change, and which source a failing test is about.

use crate::Language;
use crate::role::tag;

/// The normalised name of the thing a test file is about, or `None` when the
/// path is not a test.
///
/// `test_foo.py`, `foo_test.go`, `FooTest.java`, `FooTests.cs`, `foo.test.ts`,
/// `foo.spec.ts`, `foo_spec.rb` and `tests/foo.rs` are all about `foo`;
/// comparison ignores case, `_` and `-`.
fn subject(path: &str) -> Option<String> {
    if tag(path) != Some("test") {
        return None;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let mut stem = stem.to_owned();
    // `foo.test` / `foo.spec` came from `foo.test.ts`.
    for suffix in [".test", ".spec", "_test", "_tests", "_spec", "_specs"] {
        if let Some(rest) = stem.strip_suffix(suffix) {
            stem = rest.to_owned();
            break;
        }
    }
    if let Some(rest) = stem.strip_prefix("test_") {
        stem = rest.to_owned();
    } else {
        for suffix in ["Tests", "Test", "Specs", "Spec"] {
            if let Some(rest) = stem.strip_suffix(suffix)
                && rest.chars().next_back().is_some_and(|c| !c.is_uppercase())
            {
                stem = rest.to_owned();
                break;
            }
        }
    }
    let key = normalise(&stem);
    // A test module's own scaffolding is the subject of nothing.
    const SCAFFOLDING: &[&str] = &[
        "", "test", "tests", "mod", "index", "main", "lib", "init", "common", "helpers", "helper",
        "utils", "util", "support", "conftest", "fixtures", "setup", "all", "suite",
    ];
    (!SCAFFOLDING.contains(&key.as_str())).then_some(key)
}

/// The normalised name a source file answers to: its stem, or its directory's
/// name for the files whose stem says nothing (`mod.rs`, `index.ts`,
/// `__init__.py`, `lib.rs`, `main.rs`).
fn name_of(path: &str) -> String {
    let (dirs, file) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    if matches!(stem, "mod" | "index" | "__init__" | "lib" | "main")
        && let Some(dir) = dirs.rsplit('/').next().filter(|dir| !dir.is_empty())
    {
        return normalise(dir);
    }
    normalise(stem)
}

fn normalise(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, '_' | '-' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn shared_prefix(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

/// `(test, source)` pairs over `paths`, as positions.
///
/// A test pairs with the source of the same language and name; when several
/// files carry that name the one sharing the longest directory prefix with the
/// test wins, and a tie pairs with nothing rather than guessing.
pub(crate) fn pairs(paths: &[String]) -> Vec<(u32, u32)> {
    let mut sources: std::collections::HashMap<String, Vec<u32>> = std::collections::HashMap::new();
    for (at, path) in paths.iter().enumerate() {
        if crate::role::classify(path) != crate::role::Role::Auxiliary {
            sources.entry(name_of(path)).or_default().push(at as u32);
        }
    }
    let mut out = Vec::new();
    for (at, path) in paths.iter().enumerate() {
        let Some(key) = subject(path) else {
            continue;
        };
        let language = Language::from_path(path);
        let Some(candidates) = sources.get(&key) else {
            continue;
        };
        let mut best: Vec<(usize, u32)> = candidates
            .iter()
            .filter(|&&source| Language::from_path(&paths[source as usize]) == language)
            .map(|&source| (shared_prefix(path, &paths[source as usize]), source))
            .collect();
        best.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| paths[a.1 as usize].cmp(&paths[b.1 as usize]))
        });
        match best.as_slice() {
            [] => {}
            [(_, only)] => out.push((at as u32, *only)),
            [(top, first), (next, _), ..] if top > next => out.push((at as u32, *first)),
            _ => {}
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::{pairs, subject};

    #[test]
    fn every_ecosystems_test_naming_names_its_subject() {
        for (path, want) in [
            ("tests/foo.rs", Some("foo")),
            ("pkg/test_foo.py", Some("foo")),
            ("pkg/foo_test.go", Some("foo")),
            ("src/FooTest.java", Some("foo")),
            ("src/FooTests.cs", Some("foo")),
            ("web/foo.test.ts", Some("foo")),
            ("web/foo.spec.tsx", Some("foo")),
            ("spec/foo_spec.rb", Some("foo")),
            ("tests/my_module.rs", Some("mymodule")),
            ("src/foo.rs", None),
            ("tests/mod.rs", None),
            ("tests/common/mod.rs", None),
        ] {
            assert_eq!(subject(path).as_deref(), want, "{path}");
        }
    }

    #[test]
    fn the_nearest_source_of_that_name_wins_and_a_tie_pairs_nothing() {
        let paths: Vec<String> = [
            "a/src/store.rs",
            "a/tests/store.rs",
            "b/src/store.rs",
            "tests/mod_x.rs",
            "x/one/util.py",
            "y/two/util.py",
            "tests/test_util.py",
            "z/lib/session/mod.rs",
            "z/tests/session.rs",
        ]
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
        let found: Vec<(&str, &str)> = pairs(&paths)
            .into_iter()
            .map(|(t, s)| (paths[t as usize].as_str(), paths[s as usize].as_str()))
            .collect();
        assert!(
            found.contains(&("a/tests/store.rs", "a/src/store.rs")),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|(t, _)| *t == "tests/test_util.py"),
            "two equally near sources pair with nothing: {found:?}"
        );
        assert!(
            found.contains(&("z/tests/session.rs", "z/lib/session/mod.rs")),
            "{found:?}"
        );
    }
}
