#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! An import whose target is deleted stops being an edge and becomes the
//! `unresolved-import` diagnostic it would have been had the file never
//! existed — without waiting for the importer to be edited.

use std::fs;
use std::path::Path;

use titi_atlas::Atlas;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn fixture(root: &Path) {
    write(root, "src/lib.rs", "mod util;\nmod run;\n");
    write(root, "src/util.rs", "pub fn helper_function() {}\n");
    write(
        root,
        "src/run.rs",
        "use crate::util::helper_function;\npub fn go() { helper_function(); }\n",
    );
}

fn unresolved(atlas: &Atlas, path: &str) -> Vec<String> {
    atlas.files[path].unresolved_imports.clone()
}

#[test]
fn a_walk_that_finds_the_target_gone_restores_the_importers_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.files["src/run.rs"]
            .imports
            .contains(&"src/util.rs".to_owned())
    );
    assert!(unresolved(&atlas, "src/run.rs").is_empty());

    fs::remove_file(root.join("src/util.rs")).unwrap();
    atlas.refresh(root).unwrap();
    assert!(
        !atlas.files["src/run.rs"]
            .imports
            .contains(&"src/util.rs".to_owned()),
        "the edge to the deleted file is gone: {:?}",
        atlas.files["src/run.rs"].imports
    );
    assert!(
        !unresolved(&atlas, "src/run.rs").is_empty(),
        "the import is reported as unresolved"
    );
}

#[test]
fn a_named_deletion_does_the_same_without_a_walk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut atlas = Atlas::index(root).unwrap();
    fs::remove_file(root.join("src/util.rs")).unwrap();
    atlas.apply_changes(&["src/util.rs".to_owned()]).unwrap();
    assert!(
        !atlas.files["src/run.rs"]
            .imports
            .contains(&"src/util.rs".to_owned())
    );
    assert!(!unresolved(&atlas, "src/run.rs").is_empty());
}

/// Moving a file leaves its importers pointing at nothing, which is the truth:
/// they are edited or they are broken, and `check` says which.
#[test]
fn a_move_is_a_deletion_and_an_arrival() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut atlas = Atlas::index(root).unwrap();
    fs::create_dir_all(root.join("src/lib")).unwrap();
    fs::rename(root.join("src/util.rs"), root.join("src/lib/helpers.rs")).unwrap();
    atlas.refresh(root).unwrap();
    assert!(atlas.files.contains_key("src/lib/helpers.rs"));
    assert!(!atlas.files.contains_key("src/util.rs"));
    assert!(!unresolved(&atlas, "src/run.rs").is_empty());
}
