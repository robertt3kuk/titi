//! PHP: declarations and `use` lines, from patterns over the text.
use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{export_sites_from, finish, internal, record, resolve_suffix};
pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORTS: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORTS
        .get_or_init(|| Regex::new(r"(?m)^\s*use\s+([A-Za-z_][A-Za-z0-9_\\]*)").expect("php use"));
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:final\s+|abstract\s+)*(?:class|interface|trait|function)\s+([A-Za-z_][A-Za-z0-9_]*)")
            .expect("php exports")
    });
    let sites = export_sites_from(exports_re, source);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        record(
            spec,
            internal(resolve_suffix(&spec.replace('\\', "/"), &["php"], files)),
            &mut imports,
            &mut unresolved,
        );
    }
    finish(source, sites, imports, unresolved, 0)
}
