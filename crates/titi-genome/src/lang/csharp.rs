//! C#: types and `using` lines, from patterns over the text.
//!
//! A `using A.B.C;` names a namespace, not a file: resolution is a suffix match
//! against the workspace, which lands on the type file when the project lays
//! its directories out the way the namespace says. Declarations come from the
//! text and can be missed.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{export_sites_from, finish, internal, record, resolve_suffix};

/// Types a file publishes, `partial` and file-scoped classes included.
const EXPORTS: &str = r"(?m)^\s*(?:public\s+|internal\s+|static\s+|abstract\s+|sealed\s+|partial\s+)*(?:class|interface|enum|record|struct)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// `using Acme.Util;` and the `using X = A.B;` alias form.
const IMPORT: &str = r"(?m)^\s*using\s+(?:[A-Za-z_][A-Za-z0-9_]*\s*=\s*)?([A-Za-z_][A-Za-z0-9_.]*)";

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    static EXPORTS_RE: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORT_RE.get_or_init(|| Regex::new(IMPORT).expect("csharp import"));
    let exports_re = EXPORTS_RE.get_or_init(|| Regex::new(EXPORTS).expect("csharp exports"));

    let sites = export_sites_from(exports_re, source);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        // The trailing segment is the type; the rest is the namespace path.
        record(
            spec,
            internal(resolve_suffix(&spec.replace('.', "/"), &["cs"], files)),
            &mut imports,
            &mut unresolved,
        );
    }
    finish(source, sites, imports, unresolved, 0)
}
