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

mod c_family;
mod csharp;
mod go;
mod java;
mod kotlin;
mod php;
mod python;
mod ruby;
mod rust;
pub(crate) mod support;
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
    /// The same specifiers, each with the paths it would have named, so an
    /// update that adds a file can re-ask without re-parsing this one. See
    /// [`crate::FileRecord::unresolved_candidates`].
    pub unresolved_candidates: Vec<crate::UnresolvedImport>,
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
/// A property of the language, not of one file, and every row in [`LANGS`]
/// states it. It is the field that stops a row from overstating itself, and
/// what a user reads in `titi genome capabilities` and in the LSP handshake.
///
/// Every row in this build is `Full`; the other two variants each name a
/// state a row can be in, and `grammar` is the field that decides which.
///
/// - `Heuristic` — a row that reads the text with patterns instead of a
///   syntax tree, which is exactly what `grammar: None` means. **No row is in
///   this state today.** It becomes reachable by adding a language with no
///   grammar, whose row must then claim this level rather than `Full`, and
///   `no_row_is_heuristic_yet_and_unparsed_paths_say_unsupported` fails when
///   that happens, so becoming heuristic is a deliberate act rather than a
///   silent downgrade of the claim.
/// - `Unsupported` — a path indexed for reads that contributes no symbols.
///   That has a live instance today: it is the level of
///   [`Language::Unsupported`], the catch-all for a path no row claims
///   (`a.txt`). "Unknown extension" is only the current instance of it — the
///   variant is the word for a language the index reads but cannot parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// Read off a real syntax tree: the grammar's own nodes name every
    /// declaration this crate reports, so a commented-out declaration is not
    /// one and an indented method still is.
    Full,
    /// Patterns over the source text: a declaration the patterns do not
    /// recognise is missed, and an import may not resolve because nothing
    /// indexes the language's module system. Exports and imports can be wrong.
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

/// The grammar that turns a heuristic language into a parsed one, as
/// researched on 2026-10-08. A row that has been linked carries a grammar and
/// says `Full`; a language the table below still lists as `Heuristic` has
/// nothing linked yet rather than claiming a syntax tree it does not have.
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
        level: Level::Full,
        name: "go",
        note: "exported identifiers come from a syntax tree; import paths are resolved by package suffix",
        extensions: &["go"],
        grammar: Some(Grammar::Go),
        parse: go::parse,
    },
    Lang {
        language: Language::Java,
        level: Level::Full,
        name: "java",
        note: "types and public/protected methods come from a syntax tree; import paths are resolved by package suffix",
        extensions: &["java"],
        grammar: Some(Grammar::Java),
        parse: java::parse,
    },
    Lang {
        language: Language::C,
        level: Level::Full,
        name: "c",
        note: "declarations and quoted `#include` paths come from a syntax tree; a `.h` that the C grammar cannot read is retried with the C++ grammar",
        extensions: &["c", "h"],
        grammar: Some(Grammar::C),
        parse: c_family::parse,
    },
    Lang {
        language: Language::Cpp,
        level: Level::Full,
        name: "c++",
        note: "declarations and quoted `#include` paths come from a syntax tree; a class's members are exports like any other declaration",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        grammar: Some(Grammar::Cpp),
        parse: c_family::parse,
    },
    Lang {
        language: Language::CSharp,
        level: Level::Full,
        name: "c#",
        note: "type declarations come from a syntax tree; `using` lines are resolved by namespace suffix",
        extensions: &["cs"],
        grammar: Some(Grammar::CSharp),
        parse: csharp::parse,
    },
    Lang {
        language: Language::Ruby,
        level: Level::Full,
        name: "ruby",
        note: "`def`/`class`/`module`/`attr_*` come from a syntax tree; `require_relative` resolves beside the file and a plain `require` stays external",
        extensions: &["rb"],
        grammar: Some(Grammar::Ruby),
        parse: ruby::parse,
    },
    Lang {
        language: Language::Kotlin,
        level: Level::Full,
        name: "kotlin",
        note: "declarations come from a syntax tree; a local inside a function body is not an export; import paths are resolved by package suffix",
        extensions: &["kt", "kts"],
        grammar: Some(Grammar::Kotlin),
        parse: kotlin::parse,
    },
    Lang {
        language: Language::Swift,
        level: Level::Full,
        name: "swift",
        note: "declarations come from a syntax tree; only a `public`/`open` property is an export; an `import` names a module, resolved by its own file convention",
        extensions: &["swift"],
        grammar: Some(Grammar::Swift),
        parse: swift::parse,
    },
    Lang {
        language: Language::Php,
        level: Level::Full,
        name: "php",
        note: "declarations come from a syntax tree; a private method is not an export; `use` paths are resolved by vendor suffix",
        extensions: &["php"],
        grammar: Some(Grammar::Php),
        parse: php::parse,
    },
];

/// What the index knows about one language: the level its exports rest on,
/// the extensions it is recognised by, and the one line on what that level
/// costs.
///
/// Reference information about the index itself, deliberately not a
/// diagnostic: see [`capabilities`] and `Genome::capabilities`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// The lower-case id a user sees: `rust`, `c++`, `c#`.
    pub language: &'static str,
    pub level: Level,
    /// Every extension the language answers to, in table order.
    pub extensions: Vec<&'static str>,
    pub note: &'static str,
}

/// One entry per language this index understands, sorted by language.
///
/// Read off [`LANGS`], so it cannot drift from the parsers that report it: a
/// row whose `grammar` is `Some(..)` says `Full` or the table test fails.
/// The rows sharing a language (TypeScript's two grammars) merge into one
/// entry, which is the whole reason a caller wants a list of languages rather
/// than a list of rows.
pub fn capabilities() -> Vec<Capability> {
    let mut roster: Vec<Capability> = Vec::new();
    for row in LANGS {
        match roster.iter_mut().find(|cap| cap.language == row.name) {
            Some(cap) => cap.extensions.extend(row.extensions.iter().copied()),
            None => roster.push(Capability {
                language: row.name,
                level: row.level,
                extensions: row.extensions.to_vec(),
                note: row.note,
            }),
        }
    }
    roster.sort_by_key(|cap| cap.language);
    roster
}

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
    fn a_path_with_no_grammar_is_not_claimed() {
        // A language the table still lists without a grammar has nothing to
        // read a symbol off, so the helper answers `None` for it. As the
        // grammars land this loop empties; only the non-source path is left.
        for row in LANGS.iter().filter(|row| row.grammar.is_none()) {
            let path = format!("a.{}", row.extensions[0]);
            assert!(exports(&path, "anything").is_none(), "{path}");
        }
        assert!(exports("README.md", "# title").is_none());
    }

    /// Every literal pattern a language module holds compiles.
    ///
    /// The patterns live in `LazyLock`s, so nothing compiles them until a parse
    /// reaches them, and a broken one would first be seen in a user's session.
    /// This parses a file through each module that has one: the import scan
    /// derefs every pattern the module holds in one pass, so reaching it is
    /// what compiles them, and the assertion is that the scan ran at all.
    ///
    /// The known-file set is not incidental. A specifier whose first segment
    /// names no directory in the workspace is a std or third-party package and
    /// is deliberately not recorded, so an empty set would reach the patterns
    /// and still find nothing.
    #[test]
    fn the_literal_patterns_compile() {
        let files: HashSet<String> = ["app/service.py", "app/service.ts"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for (path, source) in [
            ("app/service.py", "from app.missing import x\nimport app\n"),
            (
                "app/service.ts",
                "import { a } from './missing'\nconst c = require('./gone')\n",
            ),
        ] {
            let parsed = parse(path, source, &files);
            assert!(
                !parsed.imports.is_empty() || !parsed.unresolved_imports.is_empty(),
                "{path} reached no pattern: {parsed:?}"
            );
        }
    }

    /// The rows that share a language must agree about it: a note or a level
    /// that drifted between the `.ts` row and the `.tsx` row would be a lie
    /// whose text depended on which file a caller happened to look at first.
    #[test]
    fn rows_sharing_a_language_share_its_level_and_note() {
        for row in LANGS {
            let first = row_for_language(row.language).expect("row");
            assert_eq!(row.level, first.level, "{}", row.name);
            assert_eq!(row.note, first.note, "{}", row.name);
            assert_eq!(row.name, first.name, "{}", row.name);
        }
    }

    /// One entry per language, sorted, with every extension of every row that
    /// claims it — the shape the `capabilities` verb and the LSP print.
    #[test]
    fn capabilities_merge_the_rows_of_a_language_and_sort_by_language() {
        let roster = capabilities();
        let names: Vec<&str> = roster.iter().map(|cap| cap.language).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "sorted by language");
        assert_eq!(
            names.len(),
            names.iter().collect::<HashSet<_>>().len(),
            "one entry per language: {names:?}"
        );
        assert!(roster.len() >= LANGS.len() - 1, "{names:?}");

        let rust = roster.iter().find(|cap| cap.language == "rust").unwrap();
        assert_eq!(rust.level, Level::Full);
        assert_eq!(rust.extensions, vec!["rs"]);

        // TypeScript's two grammars are two rows and one language.
        let typescript = roster
            .iter()
            .find(|cap| cap.language == "typescript")
            .unwrap();
        assert_eq!(typescript.level, Level::Full);
        assert_eq!(typescript.extensions, vec!["ts", "mts", "cts", "tsx"]);
        assert_eq!(
            roster
                .iter()
                .filter(|cap| cap.language == "typescript")
                .count(),
            1
        );

        // A parsed language and a pattern language must not read alike; a
        // pattern language's note says what its level costs. Once every
        // grammar lands there is no pattern row left and this loop empties.
        for heuristic in roster.iter().filter(|cap| cap.level == Level::Heuristic) {
            assert!(
                heuristic.note.contains("pattern"),
                "the note says what the level costs: {}",
                heuristic.note
            );
        }
        assert_eq!(rust.level.as_str(), "Full");
    }

    /// No row is heuristic today: every language this build recognises reads a
    /// syntax tree. `Heuristic` is still the honest word for a row with no
    /// grammar, so this is the gate on becoming one — flipping a language back
    /// to patterns has to change this line and the note in its row, which is
    /// the point.
    #[test]
    fn no_row_is_heuristic_yet_and_unparsed_paths_say_unsupported() {
        let roster = capabilities();
        let not_full: Vec<&str> = roster
            .iter()
            .filter(|capability| capability.level != Level::Full)
            .map(|capability| capability.language)
            .collect();
        assert!(
            not_full.is_empty(),
            "a row without a grammar is `Heuristic` and must say so here: {not_full:?}"
        );
        for row in LANGS {
            assert_eq!(
                row.grammar.is_some(),
                row.level == Level::Full,
                "`{}` claims a level its grammar field does not support",
                row.name
            );
        }

        // The third level is not unused: it is the catch-all's, which is how a
        // path the index reads but cannot parse is described.
        assert_eq!(Language::Unsupported.level(), Level::Unsupported);
        assert_eq!(Language::Unsupported.name(), "unsupported");
        assert_eq!(Language::from_path("a.txt"), Language::Unsupported);
        assert!(!is_source_file("a.txt"));
        assert_eq!(Level::Unsupported.as_str(), "Unsupported");
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
