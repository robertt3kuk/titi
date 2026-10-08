//! One module per language, one table row per language.
//!
//! [`LANGS`] is the single source of truth for what the index understands: an
//! extension maps to a [`Language`], and the row that claims it carries the
//! capability [`Level`], the honest one-line note, the linked grammar when
//! there is one, and the parser to call. Adding a language is a module beside
//! this one plus a row here — there is no second extension list to keep in
//! sync, no `match` in three places to grow, and nothing to remember about a
//! language that is not written down in its row.
//!
//! A row may not overstate itself. `grammar: None` is the field that keeps a
//! heuristic language honest: only a row with a grammar may claim exports that
//! came from a syntax tree.

// Every pattern in the language modules is a compile-time literal: a bad one
// is a bug the tests catch, not a runtime condition to thread through callers.
#![allow(clippy::expect_used)]

mod c_family;
mod csharp;
mod go;
mod java;
mod kotlin;
mod php;
mod python;
mod ruby;
mod rust;
mod support;
mod swift;
mod typescript;

use std::collections::HashSet;
use std::path::Path;

use crate::symbols::Grammar;

/// What one file contributes to the graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedFile {
    /// Symbols this file defines and publishes.
    pub exports: Vec<String>,
    /// One site per export, source order. Names match [`Self::exports`].
    pub export_sites: Vec<crate::ExportSite>,
    /// Files this one depends on.
    pub imports: Vec<String>,
    /// Import specifiers of this workspace's own shape that named no file.
    /// Specifiers that name something outside the workspace — a std module, a
    /// dependency, a gem — are not here: they are not this crate's business
    /// and reporting them would put a warning on every file that uses one.
    pub unresolved_imports: Vec<String>,
    /// Distinct identifiers this file mentions, minus its own exports and
    /// keywords. Resolved against the repo's exports to become the
    /// symbol-level half of the graph.
    pub refs: Vec<String>,
    /// Tree-sitter `ERROR` nodes, or 1 if a grammar language refused to parse.
    /// Languages without a grammar stay 0.
    pub syntax_errors: u32,
}

/// How well a language is understood: what its exports and imports rest on.
///
/// This is a property of the language, not of one file, and every row in
/// [`LANGS`] states it. It is what lets a user tell that Java's exports are
/// guesses while Rust's were read off a syntax tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// Read off a real syntax tree: the grammar's own nodes name every
    /// declaration this crate reports, so a commented-out declaration is not
    /// one and an indented method still is.
    Full,
    /// Patterns over the source text: imports may not resolve because nothing
    /// indexes the language's module system, and a declaration the patterns do
    /// not recognise is missed. Exports and imports can be wrong.
    Heuristic,
    /// No parser and no patterns: the file is indexed for reads like any other
    /// file, and contributes no symbols at all.
    Unsupported,
}

impl Level {
    /// The word a user sees in `genome check` and in the LSP capability.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Heuristic => "Heuristic",
            Self::Unsupported => "Unsupported",
        }
    }
}

/// The languages this crate recognises.
///
/// `JavaScript` and `TypeScript` are separate because they are separate
/// languages to a user, even though one grammar family parses both: the row
/// records which grammar, so the split costs nothing but honesty.
///
/// `Unsupported` is the catch-all for a path that is not a source file of any
/// known language. `Language::from_path` returns it for `a.txt` as much as for
/// a name with no extension; such a file is never indexed, and if one were
/// indexed directly it would contribute no symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Kotlin,
    Swift,
    Php,
    Unsupported,
}

/// One language: how to recognise it, how well it is understood, how to read
/// it.
pub(crate) struct Lang {
    pub language: Language,
    pub level: Level,
    /// The word a user sees: `c++`, `c#`, `typescript`.
    pub name: &'static str,
    /// What this language's exports and imports rest on, and what that costs,
    /// in one line. It is the sentence `genome check` prints for the language.
    pub note: &'static str,
    /// Every extension this row claims. The single source of truth for what
    /// the scanner indexes.
    pub extensions: &'static [&'static str],
    /// The grammar that parses this language, when one is linked. The TSX
    /// grammar is a row of its own because which grammar a file needs depends
    /// on its extension, not on its language.
    pub grammar: Option<Grammar>,
    pub parse: fn(&str, &str, &HashSet<String>) -> ParsedFile,
}

/// The grammar that would turn a heuristic language into a parsed one, as
/// researched on 2026-10-08. None of them is linked yet: a row whose grammar
/// column would be filled in says `Heuristic` today rather than claiming a
/// syntax tree it does not have.
///
/// `ABI` is the `LANGUAGE_VERSION` the grammar's own `parser.c` defines. The
/// workspace's tree-sitter 0.24 accepts 14, so a row is only addable when its
/// crate has an ABI-14 release; every language below has one, and the cost of
/// each bump is the dependency, a walker module and its fixtures — no core
/// bump, because the pinned release is not the newest one.
///
/// | language | crate | ABI 14 release | newest release | license | newest ABI |
/// |----------|-------|----------------|----------------|---------|------------|
/// | go       | tree-sitter-go            | 0.23.4   | 0.25.0 (2025-08-29) | MIT | 15 |
/// | java     | tree-sitter-java          | 0.23.5   | 0.23.5 (2024-12-21) | MIT | 14 |
/// | c#       | tree-sitter-c-sharp       | 0.23.1   | 0.23.5 (2026-04-14) | MIT | 15 |
/// | c        | tree-sitter-c             | 0.23.4   | 0.24.2 (2026-04-22) | MIT | 15 |
/// | c++      | tree-sitter-cpp           | 0.23.4   | 0.23.4 (2024-11-11) | MIT | 14 |
/// | ruby     | tree-sitter-ruby          | 0.23.1   | 0.23.1 (2024-11-11) | MIT | 14 |
/// | kotlin   | tree-sitter-kotlin-ng     | 1.1.0    | 1.1.0 (2025-01-09)  | MIT | 14 |
/// | swift    | tree-sitter-swift         | 0.7.0    | 0.7.4 (2026-10-04)  | MIT | 15 |
/// | php      | tree-sitter-php           | 0.23.11  | 0.25.1 (2026-10-06) | MIT | 15 |
///
/// TypeScript and JavaScript already parse: `.ts`/`.mts`/`.cts` through
/// tree-sitter-typescript, `.tsx`/`.js`/`.jsx`/`.mjs`/`.cjs` through its TSX
/// grammar, which is why `JavaScript` has a row of its own instead of a
/// second extension on the TypeScript one. tree-sitter-javascript 0.23.1 is
/// ABI 14 too, but the TSX grammar already reads plain JS/JSX, so linking it
/// would buy nothing.
pub(crate) const LANGS: &[Lang] = &[
    Lang {
        language: Language::Rust,
        level: Level::Full,
        name: "rust",
        note: "exported items and `use`/`mod` paths come from a syntax tree",
        extensions: &["rs"],
        grammar: Some(Grammar::Rust),
        parse: rust::parse,
    },
    Lang {
        language: Language::TypeScript,
        level: Level::Full,
        name: "typescript",
        note: "exports come from a syntax tree; module specifiers are matched by pattern",
        extensions: &["ts", "mts", "cts"],
        grammar: Some(Grammar::TypeScript),
        parse: typescript::parse,
    },
    Lang {
        language: Language::TypeScript,
        level: Level::Full,
        name: "typescript",
        note: "exports come from a syntax tree; module specifiers are matched by pattern",
        extensions: &["tsx"],
        grammar: Some(Grammar::Tsx),
        parse: typescript::parse,
    },
    Lang {
        language: Language::JavaScript,
        level: Level::Full,
        name: "javascript",
        note: "exports come from the TSX grammar, which reads JavaScript and JSX as well; module specifiers are matched by pattern",
        extensions: &["js", "jsx", "mjs", "cjs"],
        grammar: Some(Grammar::Tsx),
        parse: typescript::parse,
    },
    Lang {
        language: Language::Python,
        level: Level::Full,
        name: "python",
        note: "exports come from a syntax tree; import lines are matched by pattern",
        extensions: &["py", "pyi"],
        grammar: Some(Grammar::Python),
        parse: python::parse,
    },
    Lang {
        language: Language::Go,
        level: Level::Heuristic,
        name: "go",
        note: "exported identifiers and import paths are matched by pattern; an unusual layout can hide one",
        extensions: &["go"],
        grammar: None,
        parse: go::parse,
    },
    Lang {
        language: Language::Java,
        level: Level::Heuristic,
        name: "java",
        note: "types, public methods and imports are matched by pattern; comments are masked first, string literals are not",
        extensions: &["java"],
        grammar: None,
        parse: java::parse,
    },
    Lang {
        language: Language::C,
        level: Level::Heuristic,
        name: "c",
        note: "declarations and quoted `#include` paths are matched by pattern; comments are masked first",
        extensions: &["c", "h"],
        grammar: None,
        parse: c_family::parse,
    },
    Lang {
        language: Language::Cpp,
        level: Level::Heuristic,
        name: "c++",
        note: "declarations and quoted `#include` paths are matched by pattern; comments are masked first",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        grammar: None,
        parse: c_family::parse,
    },
    Lang {
        language: Language::CSharp,
        level: Level::Heuristic,
        name: "c#",
        note: "types and `using` lines are matched by pattern; comments are masked first",
        extensions: &["cs"],
        grammar: None,
        parse: csharp::parse,
    },
    Lang {
        language: Language::Ruby,
        level: Level::Heuristic,
        name: "ruby",
        note: "`def`/`class`/`require` lines are matched by pattern; comments are masked first",
        extensions: &["rb"],
        grammar: None,
        parse: ruby::parse,
    },
    Lang {
        language: Language::Kotlin,
        level: Level::Heuristic,
        name: "kotlin",
        note: "declarations and `import` lines are matched by pattern; comments are masked first",
        extensions: &["kt", "kts"],
        grammar: None,
        parse: kotlin::parse,
    },
    Lang {
        language: Language::Swift,
        level: Level::Heuristic,
        name: "swift",
        note: "declaration and `import` lines are matched by pattern; a module import names no file to resolve against",
        extensions: &["swift"],
        grammar: None,
        parse: swift::parse,
    },
    Lang {
        language: Language::Php,
        level: Level::Heuristic,
        name: "php",
        note: "declarations and `use` lines are matched by pattern; comments are masked first",
        extensions: &["php"],
        grammar: None,
        parse: php::parse,
    },
];

impl Language {
    pub fn from_path(path: &str) -> Self {
        row_for_path(path)
            .map(|row| row.language)
            .unwrap_or(Self::Unsupported)
    }

    /// The word a user sees for this language: `c++`, `c#`, `typescript`.
    pub fn name(self) -> &'static str {
        row_for_language(self).map_or("unsupported", |row| row.name)
    }

    /// What this language's exports and imports rest on.
    pub fn level(self) -> Level {
        row_for_language(self).map_or(Level::Unsupported, |row| row.level)
    }

    /// What that level costs, in one line: what the language extracts and what
    /// it can miss.
    pub fn note(self) -> &'static str {
        row_for_language(self).map_or(
            "not a source file of a language this index understands; contributes no symbols",
            |row| row.note,
        )
    }

    /// Every extension the scanner indexes.
    pub fn indexed_extensions() -> impl Iterator<Item = &'static str> {
        LANGS.iter().flat_map(|row| row.extensions.iter().copied())
    }
}

fn row_for_path(path: &str) -> Option<&'static Lang> {
    let extension = Path::new(path).extension().and_then(|ext| ext.to_str())?;
    LANGS.iter().find(|row| row.extensions.contains(&extension))
}

fn row_for_language(language: Language) -> Option<&'static Lang> {
    LANGS.iter().find(|row| row.language == language)
}

/// The grammar that parses `path`, when one is linked. `None` for a heuristic
/// language and for a path no row claims, which is what `ast_edit` reports as
/// an unsupported language.
pub(crate) fn grammar_for(path: &str) -> Option<Grammar> {
    row_for_path(path)?.grammar
}

/// Whether the scanner should read this file at all.
pub fn is_source_file(name: &str) -> bool {
    row_for_path(name).is_some()
}

pub(crate) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    match row_for_path(path) {
        Some(row) => (row.parse)(path, source, files),
        None => ParsedFile::default(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Export names as the parser behind `path` produces them, or `None` when
    /// no grammar is linked for it. The grammar-backed modules test their
    /// walkers through this; the pattern modules are tested through `Genome`
    /// fixtures, which is the surface a caller actually sees.
    pub(crate) fn exports(path: &str, source: &str) -> Option<Vec<String>> {
        let row = row_for_path(path)?;
        row.grammar?;
        let parsed = (row.parse)(path, source, &HashSet::new());
        Some(parsed.exports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::test_support::exports;

    #[test]
    fn a_language_without_a_grammar_is_not_claimed() {
        assert!(exports("main.go", "func Handler() {}").is_none());
        assert!(exports("README.md", "# title").is_none());
    }

    /// The rows that share a language must agree about it: a note or a level
    /// that drifted between the `.ts` row and the `.tsx` row would be a lie in
    /// `genome check` whose text depended on which file came first.
    #[test]
    fn rows_sharing_a_language_share_its_level_and_note() {
        for row in LANGS {
            let first = row_for_language(row.language).expect("row");
            assert_eq!(row.level, first.level, "{}", row.name);
            assert_eq!(row.note, first.note, "{}", row.name);
            assert_eq!(row.name, first.name, "{}", row.name);
        }
    }

    #[test]
    fn every_row_parses_the_extensions_it_claims() {
        for row in LANGS {
            assert!(!row.extensions.is_empty(), "{}", row.name);
            for extension in row.extensions {
                let path = format!("a.{extension}");
                assert_eq!(Language::from_path(&path), row.language, "{path}");
                assert!(is_source_file(&path), "{path}");
            }
        }
        assert!(!is_source_file("a.txt"));
        assert_eq!(Language::from_path("a.txt"), Language::Unsupported);
    }
}
