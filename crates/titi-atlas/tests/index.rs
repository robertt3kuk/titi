#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use titi_atlas::Atlas;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// Put a file's mtime where the test wants it.
fn set_mtime(root: &Path, rel: &str, at: SystemTime) {
    fs::File::options()
        .write(true)
        .open(root.join(rel))
        .unwrap()
        .set_modified(at)
        .unwrap();
}

/// Move a file's mtime an hour back, so a walk sees it as settled.
///
/// A file written within `RACY_MTIME` of the walk is read whatever its
/// metadata says - two writes inside one timestamp tick look identical - and a
/// record *taken* while its mtime was that fresh is distrusted for good. A
/// test about the *unchanged* case has to age its files past that window
/// before the metadata gate is the thing under test.
fn settle(root: &Path, rel: &str) {
    set_mtime(root, rel, SystemTime::now() - Duration::from_secs(3600));
}

#[test]
fn indexes_rust_graph_and_projects_ranked_map() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        r#"
pub mod auth;
pub mod util;
pub use auth::Session;
"#,
    );
    write(
        root,
        "src/auth.rs",
        r#"
use crate::util::hash;
pub struct Session;
pub fn login() { let _ = hash(); }
"#,
    );
    write(
        root,
        "src/util.rs",
        r#"
pub fn hash() {}
"#,
    );
    write(root, "src/scratch.txt", "ignored");
    write(root, "target/debug/lib.rs", "pub fn noise() {}");
    write(root, ".gitignore", "target/\n");

    let atlas = Atlas::index(root).unwrap();
    assert!(atlas.files.contains_key("src/lib.rs"));
    assert!(atlas.files.contains_key("src/auth.rs"));
    assert!(atlas.files.contains_key("src/util.rs"));
    assert!(!atlas.files.contains_key("src/scratch.txt"));
    assert!(!atlas.files.keys().any(|path| path.starts_with("target/")));

    // `pub mod auth;` points at the file that defines `auth`, and
    // `pub use auth::Session;` re-publishes a definition: neither is one.
    assert!(
        atlas.files["src/lib.rs"].exports.is_empty(),
        "{:?}",
        atlas.files["src/lib.rs"].exports
    );
    assert_eq!(
        atlas.files["src/lib.rs"].reexports,
        vec!["src/auth.rs".to_owned(), "src/util.rs".to_owned()]
    );
    assert!(
        atlas.files["src/auth.rs"]
            .exports
            .contains(&"Session".into())
    );
    assert!(
        atlas.files["src/auth.rs"]
            .imports
            .contains(&"src/util.rs".into())
    );
    assert!(
        atlas.files["src/lib.rs"]
            .imports
            .contains(&"src/auth.rs".into())
    );

    let util_rank = atlas.ranks["src/util.rs"];
    let auth_rank = atlas.ranks["src/auth.rs"];
    assert!(util_rank >= auth_rank, "util={util_rank} auth={auth_rank}");
    // util is depended on by both lib (`pub mod util`) and auth (`use crate::util`).
    assert_eq!(atlas.dependents["src/util.rs"], 2);
    assert_eq!(atlas.dependents["src/auth.rs"], 1);

    let projected = atlas.project(8);
    assert!(projected.contains("<atlas>"));
    assert!(projected.contains("src/util.rs (→2)"));
    assert!(projected.contains("+hash") || projected.contains("+Session"));
}

#[test]
fn indexes_this_workspace() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let atlas = Atlas::index(root).unwrap();

    assert!(
        atlas.files.len() > 20,
        "expected a real workspace, got {}",
        atlas.files.len()
    );
    assert!(
        atlas
            .files
            .contains_key("crates/titi-engine/src/runtime.rs")
    );
    assert!(atlas.files.contains_key("crates/titi-atlas/src/scan.rs"));
    assert!(
        !atlas.files.keys().any(|path| path.starts_with("target/")),
        "target/ must be pruned"
    );

    // runtime.rs is imported by many crates and must outrank a leaf module.
    let engine = atlas.ranks["crates/titi-engine/src/runtime.rs"];
    let leaf = atlas.ranks["crates/titi-atlas/src/scan.rs"];
    assert!(engine > leaf, "engine={engine} leaf={leaf}");

    let projected = atlas.project(12);
    assert!(projected.starts_with("<atlas>\n"));
    assert!(projected.ends_with("</atlas>"));
    assert!(projected.lines().count() > 6, "{projected}");
}

#[test]
fn gitignore_is_honoured_for_dirs_the_prune_list_does_not_know() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/keep.rs", "pub fn keep() {}\n");
    write(root, "src/generated/code.rs", "pub fn generated() {}\n");
    write(root, "vendor_copy/inner.rs", "pub fn inner() {}\n");
    // `generated` and `vendor_copy` are absent from PRUNE_DIRS, so only a real
    // .gitignore read can exclude them.
    write(root, ".gitignore", "generated/\nvendor_copy\n");

    let atlas = Atlas::index(root).unwrap();
    assert!(atlas.files.contains_key("src/keep.rs"));
    assert!(
        !atlas.files.keys().any(|path| path.contains("generated")),
        "nested gitignored dir leaked: {:?}",
        atlas.files.keys().collect::<Vec<_>>()
    );
    assert!(
        !atlas.files.keys().any(|path| path.contains("vendor_copy")),
        "unanchored gitignore rule leaked"
    );
}

#[test]
fn gitignore_anchoring_and_negation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "root_only.rs", "pub fn root_only() {}\n");
    write(root, "nested/root_only.rs", "pub fn nested() {}\n");
    write(root, "src/drop.rs", "pub fn drop_me() {}\n");
    write(root, "src/keep.rs", "pub fn keep_me() {}\n");
    write(
        root,
        ".gitignore",
        "/root_only.rs\nsrc/*.rs\n!src/keep.rs\n",
    );

    let atlas = Atlas::index(root).unwrap();
    assert!(
        !atlas.files.contains_key("root_only.rs"),
        "leading slash anchors to the root"
    );
    assert!(
        atlas.files.contains_key("nested/root_only.rs"),
        "anchored rule must not reach a nested file"
    );
    assert!(
        !atlas.files.contains_key("src/drop.rs"),
        "src/*.rs excludes"
    );
    assert!(
        atlas.files.contains_key("src/keep.rs"),
        "!src/keep.rs re-includes a file"
    );
}

#[test]
fn empty_workspace_projects_an_empty_map() {
    let dir = tempfile::tempdir().unwrap();
    let atlas = Atlas::index(dir.path()).unwrap();
    assert!(atlas.files.is_empty());
    assert_eq!(atlas.project(10), "<atlas>\n</atlas>");
}

/// A map rendered while the background indexer is behind says so, and one
/// rendered when it is caught up is byte-identical to the map before there was
/// a policy: the attribute is the whole of the visible difference.
#[test]
fn a_backlog_is_named_in_the_map_header() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hub.rs", "pub fn hub() {}\n");
    let atlas = Atlas::index(root).unwrap();

    let caught_up = atlas.project_with(10, &[]);
    assert!(caught_up.starts_with("<atlas>\n"), "{caught_up}");
    assert_eq!(caught_up, atlas.project(10));

    let behind = atlas.project_with_pending(10, &[], 3);
    assert!(behind.starts_with("<atlas pending=\"3\">\n"), "{behind}");
    assert!(
        behind.ends_with("</atlas>") && behind.contains("src/hub.rs"),
        "{behind}"
    );
}

#[test]
fn python_relative_imports_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "pkg/__init__.py", "");
    write(
        root,
        "pkg/mod.py",
        "from .helper import thing\n\n\ndef run():\n    pass\n",
    );
    write(root, "pkg/helper.py", "def thing():\n    pass\n");

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.files["pkg/mod.py"]
            .imports
            .contains(&"pkg/helper.py".to_owned()),
        "imports: {:?}",
        atlas.files["pkg/mod.py"].imports
    );
    assert_eq!(atlas.dependents["pkg/helper.py"], 1);
}

#[test]
fn hostile_file_names_cannot_forge_the_frame() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/plain.rs", "pub fn plain() {}\n");
    fs::write(root.join("src").join("<atlas>.rs"), "pub fn forged() {}\n").unwrap();

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.files.contains_key("src/<atlas>.rs"),
        "indexed: {:?}",
        atlas.files.keys().collect::<Vec<_>>()
    );

    let projected = atlas.project(10);
    assert_eq!(
        projected.matches("<atlas>").count(),
        1,
        "exactly one opening tag: {projected}"
    );
    assert_eq!(projected.matches("</atlas>").count(), 1);
    assert!(projected.ends_with("</atlas>"));
    assert!(!projected.contains("<atlas>.rs"));
}

#[test]
fn refresh_reparses_only_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");

    let mut atlas = Atlas::index(root).unwrap();
    assert_eq!(atlas.files.len(), 2);

    // Untouched tree: the mtime/size gate skips everything.
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 0, "no file changed");
    assert_eq!(stats.total, 2);

    // One edit: exactly one re-parse, and the new export shows up.
    write(root, "src/a.rs", "pub fn a() {}\npub fn a2() {}\n");
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1);
    assert!(atlas.files["src/a.rs"].exports.contains(&"a2".to_owned()));

    // A new file is picked up.
    write(root, "src/c.rs", "pub fn c() {}\n");
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1);
    assert_eq!(stats.total, 3);

    // A deleted file drops out of the index.
    std::fs::remove_file(root.join("src/b.rs")).unwrap();
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.total, 2);
    assert!(!atlas.files.contains_key("src/b.rs"));
}

#[test]
fn touched_files_are_boosted_in_projection() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hub.rs", "pub fn hub() {}\n");
    write(
        root,
        "src/leaf.rs",
        "pub use crate::hub::hub;\npub fn leaf() {}\n",
    );

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.ranks["src/hub.rs"] > atlas.ranks["src/leaf.rs"],
        "hub is imported by leaf"
    );
    assert!(atlas.project(1).starts_with("<atlas>\nsrc/hub.rs"));

    let biased = atlas.project_with(1, &["src/leaf.rs".to_owned()]);
    assert!(
        biased.starts_with("<atlas>\nsrc/leaf.rs"),
        "touched file leads: {biased}"
    );
}

#[test]
fn reference_productignore_hides_from_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/keep.rs", "pub fn keep() {}");
    write(root, "fixtures/noise.rs", "pub fn noise() {}");
    write(root, ".reference-productignore", "fixtures/\n");

    let atlas = Atlas::index(root).unwrap();
    assert!(atlas.files.contains_key("src/keep.rs"));
    assert!(!atlas.files.keys().any(|path| path.contains("fixtures")));
}

#[test]
fn typescript_relative_imports_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/index.ts",
        r#"
import { Session } from "./auth";
export function boot() {}
"#,
    );
    write(
        root,
        "src/auth.ts",
        r#"
export class Session {}
"#,
    );

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.files["src/index.ts"]
            .imports
            .contains(&"src/auth.ts".into())
    );
    assert!(
        atlas.files["src/auth.ts"]
            .exports
            .contains(&"Session".into())
    );
    assert_eq!(atlas.dependents["src/auth.ts"], 1);
}

#[test]
fn zero_limit_still_emits_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");

    let atlas = Atlas::index(root).unwrap();
    let projected = atlas.project(0);
    assert!(projected.starts_with("<atlas>\n"), "{projected}");
    let rows = projected
        .lines()
        .filter(|line| line.starts_with("src/"))
        .count();
    assert_eq!(rows, 1, "limit is floored at 1: {projected}");
}

#[test]
fn touched_path_outside_the_index_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");

    let atlas = Atlas::index(root).unwrap();
    let plain = atlas.project(2);
    let biased = atlas.project_with(2, &["src/not/indexed.rs".to_owned()]);
    assert_eq!(
        plain, biased,
        "an unknown touched path must not reorder the map"
    );
}

#[test]
fn angle_bracket_names_cannot_forge_the_frame() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/forge<tag>.rs", "pub fn forge() {}\n");
    write(root, "src/plain.rs", "pub fn plain() {}\n");

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.files.contains_key("src/forge<tag>.rs"),
        "the file is indexed; only the projection drops it"
    );

    let projected = atlas.project(4);
    assert!(projected.contains("src/plain.rs"), "{projected}");
    assert!(
        !projected.contains("forge"),
        "an angle-bracket name must not reach the frame: {projected}"
    );
    assert_eq!(projected.matches("</atlas>").count(), 1, "{projected}");
}

#[test]
fn a_recently_modified_file_is_marked_recent_not_new() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/live.rs", "pub fn live() {}\n");
    write(root, "src/stale.rs", "pub fn stale() {}\n");
    let stale_at = SystemTime::now()
        .checked_sub(Duration::from_secs(49 * 3600))
        .unwrap();
    fs::File::options()
        .write(true)
        .open(root.join("src/stale.rs"))
        .unwrap()
        .set_modified(stale_at)
        .unwrap();

    let atlas = Atlas::index(root).unwrap();
    let projected = atlas.project(4);
    assert!(
        projected.contains("src/live.rs (→0) [RECENT]"),
        "{projected}"
    );
    assert!(projected.contains("src/stale.rs (→0)\n"), "{projected}");
    assert!(
        !projected.contains("[NEW]"),
        "the marker means recently modified, not newly created: {projected}"
    );
    assert!(
        !projected.contains("src/stale.rs (→0) [RECENT]"),
        "a file older than 48h is not recent: {projected}"
    );
}

/// The root and its children are decided separately.
///
/// A root that cannot be read as a directory — a missing path, or a regular
/// file where a directory is required — is an error: an empty map would be a
/// lie. The same shapes *below* a readable root are skipped, and the rest of
/// the tree still indexes, so one odd entry cannot sink a large tree.
#[test]
fn an_unreadable_root_is_an_error_but_an_unreadable_child_is_skipped() {
    let dir = tempfile::tempdir().unwrap();

    assert!(
        Atlas::index(dir.path().join("missing")).is_err(),
        "a missing root must not index as empty"
    );
    let as_file = dir.path().join("plain.rs");
    fs::write(&as_file, "pub fn ok() {}\n").unwrap();
    assert!(
        Atlas::index(&as_file).is_err(),
        "a file where a directory is required must not index as empty"
    );

    let root = dir.path().join("tree");
    write(&root, "src/lib.rs", "pub fn kept() {}\n");
    // A regular file where a directory of that name might be expected.
    fs::write(root.join("odd"), "not a directory").unwrap();
    // A directory the walker may not enter, when the process is not privileged
    // enough to read through mode 000; a privileged process reads it as empty.
    let locked = root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let atlas = Atlas::index(&root).expect("one odd child must not sink the tree");
    assert!(
        atlas.files.contains_key("src/lib.rs"),
        "{:?}",
        atlas.files.keys()
    );
    assert!(!atlas.files.contains_key("odd"));
    assert!(
        !atlas.files.keys().any(|path| path.starts_with("locked/")),
        "{:?}",
        atlas.files.keys()
    );
}

/// A `touch` moves the clock, not the bytes. The size/mtime pre-filter cannot
/// tell that from a real edit, so the file is read — and the content hash
/// settles it without paying for a parse.
#[test]
fn a_no_op_mtime_touch_does_not_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");
    settle(root, "src/a.rs");
    settle(root, "src/b.rs");
    let mut atlas = Atlas::index(root).unwrap();

    // Untouched tree: the pre-filter skips every file, so nothing is read.
    let quiet = atlas.refresh(root).unwrap();
    assert_eq!(quiet.parsed, 0);
    assert_eq!(quiet.content_unchanged, 0, "not read, not counted");
    assert!(!quiet.graph_recomputed);

    let before = fs::metadata(root.join("src/a.rs"))
        .unwrap()
        .modified()
        .unwrap();
    fs::File::options()
        .write(true)
        .open(root.join("src/a.rs"))
        .unwrap()
        .set_modified(before + Duration::from_secs(1))
        .unwrap();

    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 0, "same bytes, no re-parse");
    assert_eq!(stats.content_unchanged, 1, "read, and settled by hash");
    assert!(!stats.graph_recomputed);
    assert_eq!(stats.total, 2);
    assert_eq!(atlas.files["src/a.rs"].exports, vec!["a".to_owned()]);
    assert_ne!(atlas.files["src/a.rs"].mtime, before, "the clock moved");
}

/// The same length is not the same file: `size` cannot clear a rewrite, so the
/// bytes are read and the hash disagrees.
#[test]
fn an_edit_that_keeps_the_length_is_still_reparsed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn aa() {}\n");
    let mut atlas = Atlas::index(root).unwrap();

    write(root, "src/a.rs", "pub fn ab() {}\n");
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1, "same length, different bytes");
    assert_eq!(stats.content_unchanged, 0);
    assert!(stats.graph_recomputed, "an export changed name");
    assert!(atlas.files["src/a.rs"].exports.contains(&"ab".to_owned()));
}

/// A re-parse that yields the same `(exports, imports, used_symbols)` tuple
/// cannot move the graph, so the previous ranking is left in place.
///
/// The sentinel is the proof: a rebuild would overwrite it even if it produced
/// equal values, so equality alone could not tell "recomputed to the same
/// answer" from "not recomputed".
#[test]
fn a_body_only_edit_leaves_the_ranking_maps_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hub.rs", "pub fn hub() {}\n");
    write(
        root,
        "src/leaf.rs",
        "use crate::hub::hub;\npub fn leaf() { hub(); }\n",
    );
    // The hub is not what this test edits; a just-written file would be read
    // on its own account and counted, so age it into a settled one.
    settle(root, "src/hub.rs");
    let mut atlas = Atlas::index(root).unwrap();
    assert_eq!(atlas.dependents["src/hub.rs"], 1, "the fixture has an edge");

    atlas.ranks.insert("src/hub.rs".to_owned(), 123.5);
    atlas.dependents.insert("src/leaf.rs".to_owned(), 9);

    write(
        root,
        "src/leaf.rs",
        "// a comment\nuse crate::hub::hub;\npub fn leaf() { hub(); }\n",
    );
    let stats = atlas.refresh(root).unwrap();

    assert_eq!(stats.parsed, 1, "the bytes changed");
    assert_eq!(stats.content_unchanged, 0, "…and they did change");
    assert!(
        !stats.graph_recomputed,
        "nothing the graph is built from moved"
    );
    assert_eq!(atlas.ranks["src/hub.rs"], 123.5, "ranks untouched");
    assert_eq!(atlas.dependents["src/leaf.rs"], 9, "dependents untouched");
    assert_eq!(
        atlas.files["src/leaf.rs"].used_symbols(),
        vec!["hub".to_owned()],
        "the record still carries its resolved uses"
    );
}

/// The negative half of the condition: one more export is one more graph
/// input, and the ranking is rebuilt.
#[test]
fn an_edit_that_adds_an_export_recomputes_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hub.rs", "pub fn hub() {}\n");
    write(root, "src/leaf.rs", "pub fn leaf() { hub(); }\n");
    let mut atlas = Atlas::index(root).unwrap();
    atlas.ranks.insert("src/hub.rs".to_owned(), 123.5);
    let before = atlas.symbols.clone();

    write(root, "src/hub.rs", "pub fn hub() {}\npub fn extra() {}\n");
    let stats = atlas.refresh(root).unwrap();

    assert_eq!(stats.parsed, 1);
    assert!(stats.graph_recomputed, "a new export is a new graph input");
    assert_ne!(atlas.ranks["src/hub.rs"], 123.5, "the maps were rebuilt");
    assert_ne!(atlas.symbols, before);
    assert!(atlas.symbols.contains_key("extra"));
}

/// A targeted update touches the paths it was given and nothing else, so a
/// file the caller did not name stays as it was even though it changed on
/// disk.
#[test]
fn apply_changes_updates_only_the_named_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");
    let mut atlas = Atlas::index(root).unwrap();

    write(root, "src/a.rs", "pub fn a() {}\npub fn a2() {}\n");
    write(root, "src/c.rs", "pub fn c() {}\n");
    let stats = atlas.apply_changes(&["src/a.rs".to_owned()]).unwrap();
    assert_eq!(stats.parsed, 1, "only the named path was read");
    assert_eq!(stats.total, 2, "src/c.rs was not named");
    assert!(atlas.files["src/a.rs"].exports.contains(&"a2".to_owned()));

    // The walk is what finds the file nobody named.
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1);
    assert_eq!(stats.total, 3);
    assert!(atlas.files.contains_key("src/c.rs"));

    // A named path that is gone leaves the index, and only that path.
    fs::remove_file(root.join("src/b.rs")).unwrap();
    let stats = atlas.apply_changes(&["src/b.rs".to_owned()]).unwrap();
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.total, 2);
    assert!(stats.graph_recomputed, "a node left the graph");
    assert!(!atlas.files.contains_key("src/b.rs"));
}

/// An unchanged named path is a `stat` and no more: not parsed, not read, and
/// the ranking is not rebuilt.
#[test]
fn apply_changes_on_unchanged_paths_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    settle(root, "src/a.rs");
    let mut atlas = Atlas::index(root).unwrap();
    atlas.ranks.insert("src/a.rs".to_owned(), 7.0);

    let stats = atlas
        .apply_changes(&["src/a.rs".to_owned(), "src/absent.rs".to_owned()])
        .unwrap();
    assert_eq!(stats.parsed, 0);
    assert_eq!(stats.content_unchanged, 0);
    assert_eq!(stats.removed, 0, "a path never in the index is no removal");
    assert!(!stats.graph_recomputed);
    assert_eq!(atlas.ranks["src/a.rs"], 7.0);
}

/// The metadata gate is not the whole truth. A second write inside the
/// filesystem's timestamp tick leaves `size` and `mtime` exactly as the record
/// has them; only the bytes tell the two apart, and only a read finds them.
/// A file that fresh is read on the walk's behalf, so the edit lands instead
/// of being silently skipped.
#[test]
fn a_write_inside_one_mtime_tick_is_still_caught() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn aa() {}\n");
    let mut atlas = Atlas::index(root).unwrap();
    let recorded = atlas.files["src/a.rs"].clone();

    // Same length, different bytes, and the clock put back: as far as the walk
    // can tell from the metadata, this is the file it already has.
    write(root, "src/a.rs", "pub fn ab() {}\n");
    fs::File::options()
        .write(true)
        .open(root.join("src/a.rs"))
        .unwrap()
        .set_modified(recorded.mtime)
        .unwrap();
    let now = fs::metadata(root.join("src/a.rs")).unwrap();
    assert_eq!(now.len(), recorded.size, "the same length");
    assert_eq!(now.modified().unwrap(), recorded.mtime, "the same clock");

    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1, "the bytes moved inside one tick");
    assert!(atlas.files["src/a.rs"].exports.contains(&"ab".to_owned()));
}

/// A record taken while its mtime was still racy can never be trusted by
/// metadata again, however settled the file looks later.
///
/// The sequence metadata alone loses: write at T, walk at T+0.5 - the file is
/// racy, so it is read and the record holds mtime T - a second write of the
/// same length at T+0.9 inside the same tick, which leaves the mtime at T, and
/// a walk at T+3. The file has aged out of the racy window by then, so a rule
/// that consults only the file's clock trusts it, size and mtime match, and
/// the second write is never read by anything again. The record knows it was
/// taken inside the window, so it is re-read and the hash settles it.
#[test]
fn a_record_taken_while_its_mtime_was_racy_is_never_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn aa() {}\n");
    // An mtime that is racy at the moment it is read.
    set_mtime(root, "src/a.rs", SystemTime::now() - Duration::from_secs(1));
    let mut atlas = Atlas::index(root).unwrap();
    let recorded = atlas.files["src/a.rs"].clone();
    assert!(
        recorded.mtime + titi_atlas::RACY_MTIME > recorded.read_at,
        "the fixture's record was taken while its mtime was racy"
    );

    // A second write inside the same tick: the same length, and the mtime put
    // back to the record's.
    write(root, "src/a.rs", "pub fn ab() {}\n");
    set_mtime(root, "src/a.rs", recorded.mtime);

    // The file ages out of the window; the record does not.
    std::thread::sleep(titi_atlas::RACY_MTIME + Duration::from_millis(500));
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1, "the record was racy, so the file is read");
    assert!(atlas.files["src/a.rs"].exports.contains(&"ab".to_owned()));

    let fresh = atlas.files["src/a.rs"].clone();
    assert!(
        fresh.mtime + titi_atlas::RACY_MTIME <= fresh.read_at,
        "and the record it wrote is settled, so this costs one read, not every walk"
    );
}

/// A targeted update confirms content by hash exactly as a walk does: a
/// stream of touches the watcher reports costs reads, not parses.
#[test]
fn apply_changes_confirms_content_by_hash_too() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    let mut atlas = Atlas::index(root).unwrap();

    let before = fs::metadata(root.join("src/a.rs"))
        .unwrap()
        .modified()
        .unwrap();
    fs::File::options()
        .write(true)
        .open(root.join("src/a.rs"))
        .unwrap()
        .set_modified(before + Duration::from_secs(1))
        .unwrap();

    let stats = atlas.apply_changes(&["src/a.rs".to_owned()]).unwrap();
    assert_eq!(stats.parsed, 0);
    assert_eq!(stats.content_unchanged, 1);
    assert!(!stats.graph_recomputed);
}

/// An export that moves the definer set re-resolves the files whose *raw*
/// mentions name it and nobody else.
///
/// `src/other.rs` calls `fresh_export` before any file exports it, so the
/// mention is stored and unresolved; adding the export to `src/lib.rs` makes
/// that one name meaningful. The pass that re-resolves is the exporter and the
/// one file that mentions the name — not the four files in the tree — which is
/// the whole of what keeping raw mentions buys.
#[test]
fn an_export_change_reresolves_only_the_files_that_mention_the_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub fn alpha() {}\n");
    write(root, "src/user.rs", "pub fn user() { alpha(); }\n");
    write(root, "src/other.rs", "pub fn other() { fresh_export(); }\n");
    write(root, "src/quiet.rs", "pub fn quiet() {}\n");

    let mut atlas = Atlas::index(root).unwrap();
    assert_eq!(atlas.files.len(), 4);
    assert!(
        atlas.files["src/user.rs"]
            .used_symbols()
            .contains(&"alpha".to_owned())
    );
    assert!(
        atlas.files["src/other.rs"].used_symbols().is_empty(),
        "nothing exports `fresh_export` yet"
    );
    assert_eq!(
        atlas.ref_index["fresh_export"],
        vec!["src/other.rs".to_owned()]
    );

    write(
        root,
        "src/lib.rs",
        "pub fn alpha() {}\npub fn fresh_export() {}\n",
    );
    let stats = atlas.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1, "one file changed");
    assert!(stats.walked, "a refresh lists the tree");
    assert_eq!(
        stats.reresolved, 2,
        "the edited file and the file that mentions the new name"
    );
    assert!(
        stats.reresolved < atlas.files.len(),
        "every other file kept the resolution it had"
    );

    assert_eq!(
        atlas.files["src/other.rs"].used_symbols(),
        vec!["fresh_export".to_owned()]
    );
    assert!(atlas.files["src/quiet.rs"].used_symbols().is_empty());
    assert_eq!(
        atlas.symbols["fresh_export"].files,
        vec!["src/lib.rs".to_owned()]
    );
    assert_eq!(atlas.symbols["fresh_export"].users, 1);
}

/// A specifier that named no file is answered from the index when a file
/// appears: the importer is not read and not parsed.
///
/// The specifier kept the paths it would have named (`src/missing.ts` among
/// them), so the new file is a membership test. Nothing in `src/app.ts` moves
/// except its `imports`, which is the edge the graph was missing.
#[test]
fn a_specifier_that_now_resolves_is_found_without_reparsing_the_importer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/app.ts",
        "import { helper } from \"./missing\";\n",
    );

    let mut atlas = Atlas::index(root).unwrap();
    assert!(atlas.files["src/app.ts"].imports.is_empty());
    assert_eq!(
        atlas.files["src/app.ts"].unresolved_imports,
        vec!["./missing".to_owned()]
    );

    write(root, "src/missing.ts", "export function helper() {}\n");
    let stats = atlas.apply_changes(&["src/missing.ts".to_owned()]).unwrap();
    assert_eq!(stats.parsed, 1, "only the new file is read");
    assert!(
        !stats.walked,
        "a targeted update trusts the path it was handed"
    );
    assert!(
        stats.graph_recomputed,
        "a path appeared and an edge with it"
    );

    assert_eq!(
        atlas.files["src/app.ts"].imports,
        vec!["src/missing.ts".to_owned()],
        "the importer was fixed without being re-parsed"
    );
    assert!(atlas.files["src/app.ts"].unresolved_imports.is_empty());
    assert_eq!(atlas.ranks.len(), 2, "the new file is ranked");
    assert_eq!(atlas.dependents["src/missing.ts"], 1, "the edge ranks");
}
