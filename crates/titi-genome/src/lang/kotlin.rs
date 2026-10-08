//! Kotlin: declarations and `import` lines, from patterns over the text.
//!
//! Comments are masked before any pattern runs, so a commented-out `class`
//! is not an export. Every declaration still comes from the text and can be
//! missed: a `val`/`var` without an explicit type is not an export (its name
//! is not distinguishable from a local), and neither is a declaration written
//! inside a multi-line string.
//!
//! A function exports the last dotted segment of its name, so an extension
//! function `fun String.toSlug()` exports `toSlug`, not the receiver, and a
//! generic `fun <T> first(...)` exports `first`.
//!
//! `import a.b.C` resolves by suffix over `.kt`/`.kts`. A specifier whose
//! root segment differs from the file's own `package` root is another world —
//! the JDK, a dependency — so `import java.util.List` is not a missing file.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, is_workspace_spec, mask_comments, record,
    resolve_suffix, root_segment,
};

/// `import a.b.C` and `import a.b.C as D`: the leading segments name the
/// package, the trailing one the type.
const IMPORT: &str = r"(?m)^\s*import\s+([A-Za-z_][A-Za-z0-9_.]*)";

/// The file's own `package a.b;`, whose first segment marks this workspace's
/// root.
const PACKAGE: &str = r"(?m)^\s*package\s+([A-Za-z_][A-Za-z0-9_.]*)";

/// `class`, `interface`, `object` and their `data`/`enum`/`sealed` variants.
const TYPES: &str = r"(?m)^\s*(?:(?:public|internal|protected|private|open|abstract|final|sealed|data|value|annotation|enum|inner|expect|actual)\s+)*(?:class|interface|object)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// `fun`, with an optional type-parameter list and an optional receiver. The
/// capture is the last dotted segment, so `String.toSlug` exports `toSlug`.
const FUNS: &str = r"(?m)^\s*(?:(?:public|internal|protected|private|open|abstract|final|sealed|data|inline|tailrec|operator|infix|suspend|external|override|expect|actual|const|lateinit|annotation)\s+)*fun\s+(?:<[^>\n]*>\s*)?(?:[A-Za-z_][A-Za-z0-9_]*(?:<[^>\n]*>)?\s*\.\s*)*([A-Za-z_][A-Za-z0-9_]*)\s*\(";

/// `val retries: Int` and `var` with an explicit type.
const PROPERTIES: &str = r"(?m)^\s*(?:(?:public|internal|protected|private|open|abstract|final|sealed|data|override|expect|actual|const|lateinit|annotation)\s+)*(?:val|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*:";

/// `typealias Name = ...`.
const TYPEALIASES: &str =
    r"(?m)^\s*(?:(?:public|internal|private)\s+)*typealias\s+([A-Za-z_][A-Za-z0-9_]*)";

static IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(IMPORT).expect("kotlin import"));
static PACKAGE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(PACKAGE).expect("kotlin package"));
static TYPES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(TYPES).expect("kotlin types"));
static FUNS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(FUNS).expect("kotlin functions"));
static PROPERTIES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PROPERTIES).expect("kotlin properties"));
static TYPEALIASES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(TYPEALIASES).expect("kotlin typealias"));

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    // Offsets computed on the masked text still describe the original: it
    // keeps every byte and newline, only comments are blanked.
    let masked = mask_comments(source, Comments::Slashes);

    let own = PACKAGE_RE
        .captures(&masked)
        .and_then(|cap| cap.get(1))
        .map(|decl| root_segment(decl.as_str()));

    let mut sites = Vec::new();
    for re in [&*TYPES_RE, &*FUNS_RE, &*PROPERTIES_RE, &*TYPEALIASES_RE] {
        sites.extend(export_sites_from(re, &masked));
    }
    sites.sort_by_key(|site| (site.line, site.character));

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in IMPORT_RE.captures_iter(&masked) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let resolved = resolve_suffix(&spec.replace('.', "/"), &["kt", "kts"], files);
        let placement = match resolved {
            Some(path) => Placement::Resolved(path),
            // A specifier under another root is a library, not a missing file.
            None if is_workspace_spec(root_segment(spec), own) => Placement::Missing,
            None => Placement::External,
        };
        record(spec, placement, &mut imports, &mut unresolved);
    }
    finish(&masked, sites, imports, unresolved, 0)
}
