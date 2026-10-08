//! C#: types and `using` lines, from patterns over the text.
//!
//! A `using A.B.C;` names a namespace, not a file: resolution is a suffix
//! match against the workspace, which lands on the type file when the project
//! lays its directories out the way the namespace says. Declarations come from
//! the text and can be missed. Comments are blanked first, so a commented-out
//! declaration is not a declaration; string literals are not, so a declaration
//! written inside one is still a gap.
//!
//! `using X = A.B;` imports `A.B` under an alias, `using static A.B.C;` and
//! `global using A.B;` are the same namespace import, and all three land here.
//! An import that resolves to nothing is only a missing file when it shares
//! the file's own `namespace` root: `System`, `Microsoft.*`, a NuGet package's
//! namespace is not this workspace's, so it gets no diagnostic.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, is_workspace_spec, mask_comments, record,
    resolve_suffix, root_segment,
};

/// Types a file publishes, `partial` and file-scoped classes included.
const EXPORTS: &str = r"(?m)^\s*(?:public\s+|internal\s+|static\s+|abstract\s+|sealed\s+|partial\s+)*(?:class|interface|enum|record|struct)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// `using A.B;`, `using X = A.B;`, `using static A.B.C;` and
/// `global using A.B;`. The alias and the `static`/`global` prefixes are not
/// captured; the namespace path is.
const IMPORT: &str = r"(?m)^\s*(?:global\s+)?using\s+(?:static\s+)?(?:[A-Za-z_][A-Za-z0-9_]*\s*=\s*)?([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)";

/// The file's own `namespace`, in both the block (`namespace A.B {`) and the
/// file-scoped (`namespace A.B;`) form, whose first segment decides what
/// "workspace shaped" means for its imports.
const NAMESPACE: &str =
    r"(?m)^\s*namespace\s+([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)";

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static EXPORTS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(EXPORTS).expect("csharp exports"));
    static IMPORT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(IMPORT).expect("csharp import"));
    static NAMESPACE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(NAMESPACE).expect("csharp namespace"));

    let masked = mask_comments(source, Comments::Slashes);

    let sites = export_sites_from(&EXPORTS_RE, &masked);

    // `Acme` for `namespace Acme { ... }`. Without a namespace, every
    // specifier is workspace-shaped and one that resolves to nothing is
    // reported.
    let own = NAMESPACE_RE
        .captures(&masked)
        .and_then(|cap| cap.get(1))
        .map(|m| root_segment(m.as_str()));

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in IMPORT_RE.captures_iter(&masked) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if spec.is_empty() {
            continue;
        }
        // The trailing segment is the type; the rest is the namespace path.
        let resolved = resolve_suffix(&spec.replace('.', "/"), &["cs"], files);
        let placement = match resolved {
            Some(path) => Placement::Resolved(path),
            None if is_workspace_spec(root_segment(spec), own) => Placement::Missing,
            None => Placement::External,
        };
        record(spec, placement, &mut imports, &mut unresolved);
    }
    finish(source, sites, imports, unresolved, 0)
}
