//! What a path says about a file's job.
//!
//! The graph counts every file alike, and a repository's tests, examples and
//! generated code are most of its files and most of its edges: a helper that a
//! hundred tests import is the busiest node in the graph, and it is not what
//! someone asked to change the product needs on the page. The reading here is
//! by path only — directory names and file-name shapes that every ecosystem
//! uses for these jobs — so it needs no manifest and answers for a file that
//! has not been read yet.

/// A file's job, as its path reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Everything not named below.
    Source,
    /// The root a reader starts from: a binary's `main`, a crate's `lib.rs`, a
    /// package's `index` or `__init__`. Little imports it; a lot hangs from it.
    Entry,
    /// Tests, examples, docs, fixtures, benchmarks, vendored and generated
    /// code: files that lean on the product and that the product never needs.
    Auxiliary,
}

/// Directory names that mark everything below them. Compared lowercased.
const AUXILIARY_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "spec",
    "specs",
    "testing",
    "testdata",
    "test_data",
    "testutil",
    "testutils",
    "test_utils",
    "fixtures",
    "fixture",
    "__fixtures__",
    "__mocks__",
    "__snapshots__",
    "snapshots",
    "e2e",
    "evals",
    "example",
    "examples",
    "sample",
    "samples",
    "demo",
    "demos",
    "doc",
    "docs",
    "documentation",
    "bench",
    "benches",
    "benchmark",
    "benchmarks",
    "third_party",
    "thirdparty",
    "generated",
    "__generated__",
];

/// File-name suffixes (before the extension) of generated code.
const GENERATED_SUFFIXES: &[&str] = &[
    ".generated",
    ".gen",
    ".g",
    ".designer",
    ".pb",
    ".min",
    ".bundle",
    "_generated",
    "_pb2",
    "_pb2_grpc",
    "_pb",
];

/// How many directories deep `index.*` and `__init__.py` still are a package's
/// own entry rather than one of many sub-barrels: `src/index.ts`, and
/// `src/requests/__init__.py` for the layout where the package sits in `src`.
const INDEX_DEPTH: usize = 1;
const INIT_DEPTH: usize = 2;

/// Read `path`, relative to the index root and `/`-separated.
pub(crate) fn classify(path: &str) -> Role {
    let (dirs, name) = match path.rsplit_once('/') {
        Some((dirs, name)) => (dirs, name),
        None => ("", path),
    };
    if dirs.split('/').any(is_auxiliary_dir) || is_auxiliary_name(name) {
        return Role::Auxiliary;
    }
    if is_entry(dirs, name) {
        Role::Entry
    } else {
        Role::Source
    }
}

fn is_auxiliary_dir(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    if AUXILIARY_DIRS.contains(&segment.to_ascii_lowercase().as_str()) {
        return true;
    }
    // `UnitTests`, `jvmTest`, `Foo.Tests`: a capital `T`, so `Latest` and
    // `Contest` do not read as tests.
    has_test_suffix(segment)
}

fn has_test_suffix(word: &str) -> bool {
    ["Test", "Tests"].iter().any(|suffix| {
        word.strip_suffix(suffix)
            .is_some_and(|rest| rest.chars().next_back().is_some_and(|c| !c.is_uppercase()))
    }) || matches!(word, "Test" | "Tests")
}

fn is_auxiliary_name(name: &str) -> bool {
    let (stem, _extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let lower = stem.to_ascii_lowercase();
    if lower.starts_with("test_")
        || lower.ends_with("_test")
        || lower.ends_with("_tests")
        || lower.ends_with("_spec")
        || lower.ends_with(".test")
        || lower.ends_with(".spec")
        || matches!(
            lower.as_str(),
            "test" | "tests" | "spec" | "conftest" | "test_helper" | "spec_helper"
        )
    {
        return true;
    }
    if has_test_suffix(stem) || stem.ends_with("Spec") || stem.ends_with("Specs") {
        return true;
    }
    GENERATED_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn is_entry(dirs: &str, name: &str) -> bool {
    let (stem, _extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let depth = if dirs.is_empty() {
        0
    } else {
        dirs.split('/').count()
    };
    match stem {
        "main" | "Main" | "__main__" | "Program" => true,
        "lib" => name == "lib.rs",
        "index" => depth <= INDEX_DEPTH,
        "__init__" => depth <= INIT_DEPTH,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Role, classify};

    #[test]
    fn tests_examples_docs_and_generated_code_are_auxiliary() {
        for path in [
            "test/support/utils.js",
            "tests/hay.rs",
            "src/UnitTests/AutoMapperSpecBase.cs",
            "okio/src/jvmTest/kotlin/okio/TestUtil.kt",
            "Tests/AlamofireTests/Foo.swift",
            "examples/ejs/index.js",
            "samples/src/Golden.kt",
            "docs/_themes/flask_theme_support.py",
            "src/api.generated.ts",
            "proto/user_pb2.py",
            "static/app.min.js",
            "pkg/user_test.go",
            "tests.rs",
            "spec/models/user_spec.rb",
            "src/Foo/BarTests.cs",
            "src/main/java/FooTest.java",
            "src/components/Button.spec.tsx",
            "conftest.py",
            "third_party/zlib/zlib.c",
            "evals/tasks/x/hidden/test_wc.py",
        ] {
            assert_eq!(classify(path), Role::Auxiliary, "{path}");
        }
    }

    #[test]
    fn ordinary_source_is_not() {
        for path in [
            "src/lib/format.ts",
            "crates/titi-engine/src/runtime.rs",
            "lib/express.js",
            "src/Latest.java",
            "src/contest/Contest.java",
            "src/protest.py",
            "okio/src/commonMain/kotlin/okio/Buffer.kt",
            "src/AutoMapper/Mapper.cs",
            "gen_utils.py",
        ] {
            assert_eq!(classify(path), Role::Source, "{path}");
        }
    }

    #[test]
    fn entry_points_are_the_roots_a_reader_starts_from() {
        for path in [
            "crates/core/main.rs",
            "cmd/tool/main.go",
            "crates/titi-engine/src/lib.rs",
            "src/index.ts",
            "index.js",
            "src/requests/__init__.py",
            "src/Program.cs",
        ] {
            assert_eq!(classify(path), Role::Entry, "{path}");
        }
        // A deep `index` is one of many, not the package's own.
        assert_eq!(classify("src/a/b/c/index.ts"), Role::Source);
        assert_eq!(classify("src/ui/index.ts"), Role::Source);
        assert_eq!(classify("src/lib.go"), Role::Source);
    }
}
