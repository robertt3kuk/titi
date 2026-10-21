#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! One spelling per file. A tool argument, a diff path and an editor URI all
//! name the same file differently; the index keys by the path relative to its
//! root, and everything that comes from outside is turned into that key first.

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

fn tree() -> (tempfile::TempDir, Atlas) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.rs", "pub fn alpha() {}\n");
    write(dir.path(), "src/b.rs", "pub fn beta() {}\n");
    let atlas = Atlas::index(dir.path()).unwrap();
    (dir, atlas)
}

#[test]
fn key_turns_every_spelling_into_the_relative_path() {
    let (dir, atlas) = tree();
    let abs = dir.path().join("src/a.rs");
    let canonical = fs::canonicalize(&abs).unwrap();
    for spelled in [
        "src/a.rs".to_owned(),
        "./src/a.rs".to_owned(),
        "src//a.rs".to_owned(),
        "src/../src/a.rs".to_owned(),
        abs.display().to_string(),
        canonical.display().to_string(),
    ] {
        assert_eq!(
            atlas.key(&spelled).as_deref(),
            Some("src/a.rs"),
            "{spelled}"
        );
    }
    assert_eq!(atlas.key("/etc/passwd"), None, "outside the root");
    assert_eq!(atlas.key("../elsewhere.rs"), None, "escapes the root");
    assert_eq!(atlas.key(""), None);
}

/// The model writes `/abs/path/src/a.rs` or `./src/a.rs`; the index used to
/// keep that spelling as a second record of the same file until the next walk.
#[test]
fn a_targeted_update_by_any_spelling_updates_the_one_record() {
    let (dir, mut atlas) = tree();
    write(
        dir.path(),
        "src/a.rs",
        "pub fn alpha() {}\npub fn gamma() {}\n",
    );
    let abs = dir.path().join("src/a.rs").display().to_string();
    let stats = atlas
        .apply_changes(&[abs, "./src/b.rs".to_owned()])
        .unwrap();

    assert_eq!(
        atlas.files.len(),
        2,
        "{:?}",
        atlas.files.keys().collect::<Vec<_>>()
    );
    assert!(
        atlas.files["src/a.rs"]
            .exports
            .contains(&"gamma".to_owned())
    );
    assert!(stats.parsed >= 1, "{stats:?}");
}

/// `touched` is what the model spelled; the boost has to find the file anyway.
#[test]
fn a_touched_path_in_any_spelling_boosts_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hub.rs", "pub fn hub_fn() {}\n");
    for n in 0..6 {
        write(
            root,
            &format!("src/user{n}.rs"),
            "use crate::hub::hub_fn;\npub fn f() { hub_fn(); }\n",
        );
    }
    write(root, "src/zleaf.rs", "pub fn leaf_fn() {}\n");
    let atlas = Atlas::index(root).unwrap();

    let plain = atlas.project(1);
    assert!(plain.contains("src/hub.rs"), "{plain}");
    let abs = root.join("src/zleaf.rs").display().to_string();
    // A leaf's boost is too small to beat the hub at limit 1; at limit 2 the
    // first slot is the hub and the second goes to the touched file.
    let boosted = atlas.project_with(2, &[abs]);
    assert!(boosted.contains("src/zleaf.rs"), "{boosted}");
}

/// An editor that opened the folder as `/tmp/x` talks to a server whose
/// working directory is `/private/tmp/x`: every query used to answer empty.
#[cfg(unix)]
#[test]
fn an_lsp_uri_through_a_symlinked_root_still_finds_the_file() {
    use std::io::Cursor;
    let real = tempfile::tempdir().unwrap();
    write(real.path(), "src/a.rs", "pub fn alpha() {}\n");
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("linked");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();

    let uri = format!("file://{}", link.join("src/a.rs").display());
    let body = |text: String| format!("Content-Length: {}\r\n\r\n{text}", text.len());
    let mut input =
        body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.to_owned());
    input.push_str(&body(format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"textDocument/documentSymbol","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
    )));
    input.push_str(&body(r#"{"jsonrpc":"2.0","method":"exit"}"#.to_owned()));
    let mut output = Vec::new();
    titi_atlas::serve_lsp(real.path(), Cursor::new(input.into_bytes()), &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("alpha"), "{text}");
}
