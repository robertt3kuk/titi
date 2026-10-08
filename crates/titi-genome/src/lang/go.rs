//! Go: exported identifiers and import paths, from patterns over the text.
//!
//! Go's own rule is what makes this affordable: an identifier is exported
//! exactly when it starts with an uppercase letter. The pattern still misses
//! what a grammar would not — a grouped `type (...)` block, an import written
//! across an unusual layout — which is why this language reports itself as
//! `Level::Heuristic` rather than pretending to be parsed.
use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{export_sites_from, finish, internal, record, resolve_suffix};
pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORT_BLOCK: OnceLock<Regex> = OnceLock::new();
    static IMPORT_LINE: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    // `import ( "a/b" \n "c/d" )` and the single-line form.
    let block_re = IMPORT_BLOCK
        .get_or_init(|| Regex::new(r#"(?ms)^\s*import\s*\((.*?)\)"#).expect("go import block"));
    let line_re = IMPORT_LINE.get_or_init(|| {
        Regex::new(r#"(?m)^\s*import\s+(?:[\w.]+\s+)?"([^"]+)""#).expect("go import")
    });
    let exports_re = EXPORTS.get_or_init(|| {
        // Exported Go identifiers start uppercase.
        Regex::new(r"(?m)^(?:func|type|var|const)\s+(?:\([^)]*\)\s*)?([A-Z][A-Za-z0-9_]*)")
            .expect("go exports")
    });

    let sites = export_sites_from(exports_re, source);
    let mut specs: Vec<String> = Vec::new();
    for cap in block_re.captures_iter(source) {
        let body = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for quoted in body.split('"').skip(1).step_by(2) {
            specs.push(quoted.to_owned());
        }
    }
    for cap in line_re.captures_iter(source) {
        if let Some(spec) = cap.get(1) {
            specs.push(spec.as_str().to_owned());
        }
    }

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for spec in specs {
        // Go import paths are module-qualified; the package directory is the
        // trailing segment, so a suffix match is the honest resolution.
        let last = spec.rsplit('/').next().unwrap_or(&spec);
        let resolved = resolve_suffix(&format!("{last}/{last}"), &["go"], files)
            .or_else(|| resolve_suffix(last, &["go"], files));
        record(&spec, internal(resolved), &mut imports, &mut unresolved);
    }
    finish(source, sites, imports, unresolved, 0)
}
