//! C and C++: declarations and quoted `#include` paths, from patterns.
//!
//! Both rows of the language table point here: the two share a declaration
//! shape and a header convention, and the quoted include is the only include
//! this crate resolves — a `<system.h>` is not a file of the workspace.
use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{export_sites_from, finish, internal, normalize_join, parent, record};
pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static INCLUDES: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let include_re = INCLUDES
        .get_or_init(|| Regex::new(r#"(?m)^\s*#\s*include\s+"([^"]+)""#).expect("c include"));
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:typedef\s+)?(?:struct|class|enum|union)\s+([A-Za-z_][A-Za-z0-9_]*)|^\s*(?:[A-Za-z_][A-Za-z0-9_]*\s+)+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
        )
        .expect("c exports")
    });
    let sites = export_sites_from(exports_re, source);
    let from_dir = parent(path).unwrap_or("");
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in include_re.captures_iter(source) {
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
    finish(source, sites, imports, unresolved, 0)
}
