//! Java: types, public methods and `import`/`package` lines, from patterns.
//!
//! Java has no header-to-implementation split to exploit: `import a.b.C;` maps
//! straight onto a path, so resolution needs no module index — only the
//! extension and a suffix match — while the declarations are read off the text
//! and can be missed (a declaration the patterns do not recognise, or one an
//! annotation pushes somewhere odd).

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{export_sites_from, finish, internal, record, resolve_suffix};

/// Types a file publishes. `record` is a Java 16 type like any other.
const EXPORTS: &str = r"(?m)^\s*(?:public\s+|final\s+|abstract\s+|sealed\s+|non-sealed\s+)*(?:class|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// `import a.b.C;` and `import static a.b.C.member;`: the leading path is the
/// package, the trailing segments name the type and possibly a member.
const IMPORT: &str = r"(?m)^\s*import\s+(?:static\s+)?([A-Za-z_][A-Za-z0-9_.]*)";

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    static EXPORTS_RE: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORT_RE.get_or_init(|| Regex::new(IMPORT).expect("java import"));
    let exports_re = EXPORTS_RE.get_or_init(|| Regex::new(EXPORTS).expect("java exports"));

    let sites = export_sites_from(exports_re, source);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        // The trailing segment is the type; the rest is the package path.
        record(
            spec,
            internal(resolve_suffix(&spec.replace('.', "/"), &["java"], files)),
            &mut imports,
            &mut unresolved,
        );
    }
    finish(source, sites, imports, unresolved, 0)
}
