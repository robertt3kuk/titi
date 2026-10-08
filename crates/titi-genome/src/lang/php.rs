//! PHP: declarations and `use` lines, from patterns over the text.
//!
//! Comments are masked before any pattern runs, so `// class Ghost {}` is not
//! an export. `//` and `/* … */` are covered; PHP's `#` comment form is a
//! known gap — a declaration behind `#` is still read as one. Every
//! declaration comes from the text and can be missed, and a `use` inside a
//! class body (a trait use) is read as a namespace import.
//!
//! A `use` may import a class (`Acme\Util`), a grouped set
//! (`Acme\{Hash, Cache}`), a function (`use function Acme\helper`) or a
//! constant (`use const Acme\LIMIT`), optionally with an `as Alias` tail;
//! each name resolves by suffix over `.php`. A specifier whose root differs
//! from the file's own `namespace` root is another vendor — `Psr\Log\…` — and
//! is not a missing file.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, is_workspace_spec, mask_comments, record,
    resolve_suffix, root_segment,
};

/// `use Acme\Util;`, `use Acme\{Hash, Cache};`, `use function Acme\helper;`.
/// The capture is the whole clause before the `;`, parsed by [`use_specs`].
const IMPORT: &str = r"(?m)^\s*use\s+([^;]+);";

/// The file's own `namespace Acme\Sub;`, whose first segment marks this
/// workspace's vendor root.
const NAMESPACE: &str = r"(?m)^\s*namespace\s+([A-Za-z_][A-Za-z0-9_\\]*)";

/// `class`, `interface`, `trait`, `enum`, with any `final`/`abstract`/
/// `readonly` in front.
const TYPES: &str = r"(?m)^\s*(?:(?:final|abstract|readonly)\s+)*(?:class|interface|trait|enum)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// A top-level `function` and a `public`/`protected` method. `private` is
/// deliberately absent: a private method is not part of the surface.
const FUNCTIONS: &str = r"(?m)^\s*(?:(?:public|protected|static|final|abstract|readonly)\s+)*function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(";

static IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(IMPORT).expect("php use"));
static NAMESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(NAMESPACE).expect("php namespace"));
static TYPES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(TYPES).expect("php types"));
static FUNCTIONS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(FUNCTIONS).expect("php functions"));

/// The specifiers one `use` clause names. `function`/`const` prefixes are
/// dropped, a grouped clause is split on `,`, and a trailing `as Alias` is
/// cut from each name.
fn use_specs(clause: &str) -> Vec<String> {
    let clause = clause.trim();
    let clause = clause
        .strip_prefix("function ")
        .or_else(|| clause.strip_prefix("const "))
        .unwrap_or(clause)
        .trim();
    let (prefix, inner) = match (clause.find('{'), clause.rfind('}')) {
        (Some(open), Some(close)) if open < close => (
            clause[..open].trim().trim_end_matches('\\').to_owned(),
            &clause[open + 1..close],
        ),
        _ => (String::new(), clause),
    };
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}\\")
    };
    inner
        .split(',')
        .filter_map(|piece| {
            let name = piece.split_whitespace().next().unwrap_or("");
            if name.is_empty() {
                None
            } else {
                Some(format!("{prefix}{name}"))
            }
        })
        .collect()
}

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    // Offsets computed on the masked text still describe the original: it
    // keeps every byte and newline, only comments are blanked.
    let masked = mask_comments(source, Comments::Slashes);

    let own = NAMESPACE_RE
        .captures(&masked)
        .and_then(|cap| cap.get(1))
        .map(|decl| root_segment(decl.as_str()));

    let mut sites = Vec::new();
    for re in [&*TYPES_RE, &*FUNCTIONS_RE] {
        sites.extend(export_sites_from(re, &masked));
    }
    sites.sort_by_key(|site| (site.line, site.character));

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in IMPORT_RE.captures_iter(&masked) {
        let clause = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for spec in use_specs(clause) {
            let resolved = resolve_suffix(&spec.replace('\\', "/"), &["php"], files);
            let placement = match resolved {
                Some(path) => Placement::Resolved(path),
                // A specifier under another vendor root is a library.
                None if is_workspace_spec(root_segment(&spec), own) => Placement::Missing,
                None => Placement::External,
            };
            record(&spec, placement, &mut imports, &mut unresolved);
        }
    }
    finish(&masked, sites, imports, unresolved, 0)
}
