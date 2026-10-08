//! C and C++: declarations and quoted `#include` paths, from patterns.
//!
//! Both rows of the language table point here: the two share a declaration
//! shape and a header convention, and the quoted include is the only include
//! this crate resolves — a `<stdio.h>` is not a file of the workspace, so the
//! pattern does not match it and it carries no diagnostic.
//!
//! Comments are blanked before any pattern runs, so a commented-out
//! declaration is not one and an include inside a comment is not an edge. A
//! declaration exports the last `::` segment of its name, so an out-of-class
//! C++ definition `void Engine::start()` exports `start`, and pointer,
//! reference and qualified return types are part of the type words that come
//! before the name. A declaration whose modifiers include `static` is
//! file-private in both languages and is not exported at all.
//!
//! What a compiler would still read and this does not: a name with no tag
//! (`typedef int Foo;`), a name produced by a macro, a declaration written
//! inside a multi-line string, and — because the pattern is line-anchored — a
//! declaration that does not begin its line.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, finish, internal, line_character, mask_comments, normalize_join, parent, record,
};

/// `struct Hash {`, `class Engine {`, `enum class Color {`, `typedef union U {`:
/// the leading modifiers are captured so `static` can be recognised, and an
/// `enum class` keeps the `class` keyword out of the tag capture.
const TYPES: &str = r"(?m)^([ \t]*(?:(?:typedef|static|extern|inline)\s+)*)(?:enum\s+(?:class\s+|struct\s+)?|struct\s+|class\s+|union\s+)([A-Za-z_][A-Za-z0-9_]*)";

/// A declaration or definition at the start of a line: type words (with
/// pointers, references, template arguments and qualifiers) up to the name,
/// then `(`. The name may be `::` qualified. The type part cannot cross a
/// `=`, `(` or `#`, so an assignment, a call and a preprocessor line do not
/// look like a declaration.
const FUNCTIONS: &str =
    r"(?m)^[ \t]*([A-Za-z_][A-Za-z0-9_:<>,&* \t]*?)[ \t]*([A-Za-z_][A-Za-z0-9_:]*)[ \t]*\(";

/// `#include "util/hash.h"`. The angle-bracket form is deliberately not
/// matched: a system header is not a file of this workspace.
const INCLUDES: &str = r#"(?m)^\s*#\s*include\s+"([^"]+)""#;

/// Words that can sit where a return type sits but mean a statement
/// (`return foo(x)`) or a control form (`else if (…)`). A declaration whose
/// type words hold one of these is not a declaration.
const STATEMENT_WORDS: &[&str] = &[
    "if", "else", "for", "while", "do", "switch", "case", "default", "return", "goto", "break",
    "continue", "sizeof", "new", "delete", "throw", "catch",
];

static TYPES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(TYPES).expect("c types"));
static FUNCTIONS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(FUNCTIONS).expect("c functions"));
static INCLUDES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(INCLUDES).expect("c include"));

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    // Offsets computed on the masked text still describe the original: it
    // keeps every byte and newline, only comments are blanked.
    let masked = mask_comments(source, Comments::Slashes);

    let mut sites = type_sites(&masked);
    sites.extend(function_sites(&masked));
    // Sources were concatenated, so restore the source order the sites claim.
    sites.sort_by_key(|site| (site.line, site.character));

    let from_dir = parent(path).unwrap_or("");
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in INCLUDES_RE.captures_iter(&masked) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let joined = if from_dir.is_empty() {
            spec.to_owned()
        } else {
            format!("{from_dir}/{spec}")
        };
        record(
            spec,
            internal(normalize_join("", &joined).filter(|candidate| files.contains(candidate))),
            &mut imports,
            &mut unresolved,
        );
    }
    finish(&masked, sites, imports, unresolved, 0)
}

/// A `struct`/`class`/`enum`/`union` tag, unless the declaration is
/// `static` (which makes it file-private).
fn type_sites(text: &str) -> Vec<crate::ExportSite> {
    let mut sites = Vec::new();
    for cap in TYPES_RE.captures_iter(text) {
        let modifiers = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let Some(name) = cap.get(2) else {
            continue;
        };
        if holds_word(modifiers, "static") {
            continue;
        }
        let (line, character) = line_character(text, name.start());
        sites.push(crate::ExportSite {
            name: name.as_str().to_owned(),
            line,
            character,
        });
    }
    sites
}

/// A function declaration or definition: the last `::` segment of the name is
/// the export. A `static` declaration is skipped, as is a line whose type words
/// are really a statement, so `return foo(x)` or `else if (x)` is not one.
fn function_sites(text: &str) -> Vec<crate::ExportSite> {
    let mut sites = Vec::new();
    for cap in FUNCTIONS_RE.captures_iter(text) {
        let (Some(head), Some(name)) = (cap.get(1), cap.get(2)) else {
            continue;
        };
        if holds_word(head.as_str(), "static") || holds_statement_word(head.as_str()) {
            continue;
        }
        // A qualified name exports its last segment: `Engine::start` → `start`
        // — and the site points at that segment, not at the class.
        let exported = name.as_str().rsplit("::").next().unwrap_or(name.as_str());
        if STATEMENT_WORDS.contains(&exported) {
            continue;
        }
        let offset = name.start() + name.as_str().len() - exported.len();
        let (line, character) = line_character(text, offset);
        sites.push(crate::ExportSite {
            name: exported.to_owned(),
            line,
            character,
        });
    }
    sites
}

/// Whether any whitespace-separated word of `text` is exactly `word`.
fn holds_word(text: &str, word: &str) -> bool {
    text.split_whitespace().any(|token| token == word)
}

/// Whether any whitespace-separated word of `text` is a statement keyword.
fn holds_statement_word(text: &str) -> bool {
    text.split_whitespace()
        .any(|token| STATEMENT_WORDS.contains(&token))
}
