//! Swift: declaration lines, from patterns over the text.
//!
//! Swift publishes no file-to-file import: `import Foo` names a module, not a
//! path, so the module name is all this language has to offer an edge.
use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{export_sites_from, finish};
pub(super) fn parse(_path: &str, source: &str, _files: &HashSet<String>) -> ParsedFile {
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public\s+|open\s+|final\s+|internal\s+)*(?:func|struct|class|enum|protocol|extension|typealias)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("swift exports")
    });
    finish(
        source,
        export_sites_from(exports_re, source),
        Vec::new(),
        Vec::new(),
        0,
    )
}
