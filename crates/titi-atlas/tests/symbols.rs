#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! `FileRecord::symbols`: one entry per export, whichever language module
//! parsed the file, so consumers never special-case a module that has not
//! learned to describe its symbols yet.

use std::fs;
use std::path::Path;

use titi_atlas::{Atlas, SymbolSite};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

#[test]
fn every_export_has_a_symbol_site_on_its_line() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub struct Alpha;\n\npub fn beta() {}\n");
    write(root, "lib/b.py", "def gamma():\n    pass\n");
    let atlas = Atlas::index(root).unwrap();

    for path in ["src/a.rs", "lib/b.py"] {
        let record = &atlas.files[path];
        let names: Vec<&str> = record.symbols.iter().map(|s| s.name.as_str()).collect();
        let exports: Vec<&str> = record.exports.iter().map(String::as_str).collect();
        assert_eq!(names, exports, "{path}");
        for (symbol, site) in record.symbols.iter().zip(&record.export_sites) {
            assert!(symbol.exported, "{symbol:?}");
            assert!(
                symbol.span.0 >= 1 && symbol.span.0 <= symbol.span.1,
                "{symbol:?}"
            );
            assert_eq!(symbol.span.0, site.line, "{symbol:?}");
            assert!(
                symbol
                    .signature
                    .as_deref()
                    .is_none_or(|s| s.chars().count() <= 120)
            );
        }
    }
}

/// The type itself is the contract the language modules fill in.
#[test]
fn a_symbol_site_is_constructible_by_a_language_module() {
    let site = SymbolSite {
        name: "run".to_owned(),
        kind: titi_atlas::SymbolKind::Function,
        span: (10, 24),
        signature: titi_atlas::one_line_signature("pub fn run(&self) -> Out {"),
        exported: true,
    };
    assert_eq!(
        site.signature.as_deref(),
        Some("pub fn run(&self) -> Out {")
    );
}
