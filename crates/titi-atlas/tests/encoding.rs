#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A source file that is not valid UTF-8 is still source. Latin-1 and
//! Windows-1252 comments, a stray byte in a string literal and a Shift-JIS
//! header are common in older trees, and a file that indexed as empty drops
//! every symbol it defines and every edge it carries.

use std::fs;

use titi_atlas::Atlas;

#[test]
fn a_file_with_invalid_utf8_keeps_its_symbols_and_its_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod legacy;\nmod user;\n").unwrap();
    // 0xE9 is `é` in Latin-1 and a lone continuation-less byte in UTF-8.
    let mut legacy = b"// caf\xe9 society\npub fn survives_latin1() {}\n".to_vec();
    legacy.extend_from_slice(b"pub fn label() -> &'static str { \"na\xefve\" }\n");
    fs::write(root.join("src/legacy.rs"), legacy).unwrap();
    fs::write(
        root.join("src/user.rs"),
        "use crate::legacy::survives_latin1;\npub fn run() { survives_latin1(); }\n",
    )
    .unwrap();

    let atlas = Atlas::index(root).unwrap();
    let record = &atlas.files["src/legacy.rs"];
    assert!(
        record.exports.iter().any(|name| name == "survives_latin1"),
        "{:?}",
        record.exports
    );
    assert!(record.exports.iter().any(|name| name == "label"));
    assert!(
        atlas.files["src/user.rs"]
            .imports
            .contains(&"src/legacy.rs".to_owned()),
        "the importer still resolves to it"
    );
    let uses = atlas.uses("survives_latin1", None).unwrap();
    assert_eq!(uses.users.len(), 1, "{uses:?}");
}

/// The lines a use-site query quotes come from the same decoding.
#[test]
fn a_use_in_a_file_with_invalid_utf8_is_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod core_fn;\nmod caller;\n").unwrap();
    fs::write(root.join("src/core_fn.rs"), "pub fn shared_helper() {}\n").unwrap();
    fs::write(
        root.join("src/caller.rs"),
        b"use crate::core_fn::shared_helper;\n// r\xe9sum\xe9\npub fn go() { shared_helper(); }\n"
            .to_vec(),
    )
    .unwrap();
    let atlas = Atlas::index(root).unwrap();
    let uses = atlas.uses("shared_helper", None).unwrap();
    let rendered = uses.render();
    assert!(rendered.contains("shared_helper();"), "{rendered}");
}
