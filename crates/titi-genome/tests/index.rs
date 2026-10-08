#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use titi_genome::Genome;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
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

    let genome = Genome::index(root).unwrap();
    assert!(genome.files.contains_key("src/lib.rs"));
    assert!(genome.files.contains_key("src/auth.rs"));
    assert!(genome.files.contains_key("src/util.rs"));
    assert!(!genome.files.contains_key("src/scratch.txt"));
    assert!(!genome.files.keys().any(|path| path.starts_with("target/")));

    assert!(
        genome.files["src/lib.rs"]
            .exports
            .iter()
            .any(|name| name == "Session" || name == "auth" || name == "util")
    );
    assert!(
        genome.files["src/auth.rs"]
            .exports
            .contains(&"Session".into())
    );
    assert!(
        genome.files["src/auth.rs"]
            .imports
            .contains(&"src/util.rs".into())
    );
    assert!(
        genome.files["src/lib.rs"]
            .imports
            .contains(&"src/auth.rs".into())
    );

    let util_rank = genome.ranks["src/util.rs"];
    let auth_rank = genome.ranks["src/auth.rs"];
    assert!(util_rank >= auth_rank, "util={util_rank} auth={auth_rank}");
    // util is depended on by both lib (`pub mod util`) and auth (`use crate::util`).
    assert_eq!(genome.dependents["src/util.rs"], 2);
    assert_eq!(genome.dependents["src/auth.rs"], 1);

    let projected = genome.project(8);
    assert!(projected.contains("<genome>"));
    assert!(projected.contains("src/util.rs:(→2)"));
    assert!(projected.contains("+hash") || projected.contains("+Session"));
}

#[test]
fn indexes_this_workspace() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let genome = Genome::index(root).unwrap();

    assert!(
        genome.files.len() > 20,
        "expected a real workspace, got {}",
        genome.files.len()
    );
    assert!(
        genome
            .files
            .contains_key("crates/titi-engine/src/runtime.rs")
    );
    assert!(genome.files.contains_key("crates/titi-genome/src/scan.rs"));
    assert!(
        !genome.files.keys().any(|path| path.starts_with("target/")),
        "target/ must be pruned"
    );

    // runtime.rs is imported by many crates and must outrank a leaf module.
    let engine = genome.ranks["crates/titi-engine/src/runtime.rs"];
    let leaf = genome.ranks["crates/titi-genome/src/scan.rs"];
    assert!(engine > leaf, "engine={engine} leaf={leaf}");

    let projected = genome.project(12);
    assert!(projected.starts_with("<genome>\n"));
    assert!(projected.ends_with("</genome>"));
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

    let genome = Genome::index(root).unwrap();
    assert!(genome.files.contains_key("src/keep.rs"));
    assert!(
        !genome.files.keys().any(|path| path.contains("generated")),
        "nested gitignored dir leaked: {:?}",
        genome.files.keys().collect::<Vec<_>>()
    );
    assert!(
        !genome.files.keys().any(|path| path.contains("vendor_copy")),
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

    let genome = Genome::index(root).unwrap();
    assert!(
        !genome.files.contains_key("root_only.rs"),
        "leading slash anchors to the root"
    );
    assert!(
        genome.files.contains_key("nested/root_only.rs"),
        "anchored rule must not reach a nested file"
    );
    assert!(
        !genome.files.contains_key("src/drop.rs"),
        "src/*.rs excludes"
    );
    assert!(
        genome.files.contains_key("src/keep.rs"),
        "!src/keep.rs re-includes a file"
    );
}

#[test]
fn empty_workspace_projects_an_empty_map() {
    let dir = tempfile::tempdir().unwrap();
    let genome = Genome::index(dir.path()).unwrap();
    assert!(genome.files.is_empty());
    assert_eq!(genome.project(10), "<genome>\n</genome>");
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

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["pkg/mod.py"]
            .imports
            .contains(&"pkg/helper.py".to_owned()),
        "imports: {:?}",
        genome.files["pkg/mod.py"].imports
    );
    assert_eq!(genome.dependents["pkg/helper.py"], 1);
}

#[test]
fn hostile_file_names_cannot_forge_the_frame() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/plain.rs", "pub fn plain() {}\n");
    fs::write(root.join("src").join("<genome>.rs"), "pub fn forged() {}\n").unwrap();

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files.contains_key("src/<genome>.rs"),
        "indexed: {:?}",
        genome.files.keys().collect::<Vec<_>>()
    );

    let projected = genome.project(10);
    assert_eq!(
        projected.matches("<genome>").count(),
        1,
        "exactly one opening tag: {projected}"
    );
    assert_eq!(projected.matches("</genome>").count(), 1);
    assert!(projected.ends_with("</genome>"));
    assert!(!projected.contains("<genome>.rs"));
}

#[test]
fn refresh_reparses_only_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");

    let mut genome = Genome::index(root).unwrap();
    assert_eq!(genome.files.len(), 2);

    // Untouched tree: the mtime/size gate skips everything.
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 0, "no file changed");
    assert_eq!(stats.total, 2);

    // One edit: exactly one re-parse, and the new export shows up.
    write(root, "src/a.rs", "pub fn a() {}\npub fn a2() {}\n");
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1);
    assert!(genome.files["src/a.rs"].exports.contains(&"a2".to_owned()));

    // A new file is picked up.
    write(root, "src/c.rs", "pub fn c() {}\n");
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1);
    assert_eq!(stats.total, 3);

    // A deleted file drops out of the index.
    std::fs::remove_file(root.join("src/b.rs")).unwrap();
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.total, 2);
    assert!(!genome.files.contains_key("src/b.rs"));
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

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.ranks["src/hub.rs"] > genome.ranks["src/leaf.rs"],
        "hub is imported by leaf"
    );
    assert!(genome.project(1).starts_with("<genome>\nsrc/hub.rs"));

    let biased = genome.project_with(1, &["src/leaf.rs".to_owned()]);
    assert!(
        biased.starts_with("<genome>\nsrc/leaf.rs"),
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

    let genome = Genome::index(root).unwrap();
    assert!(genome.files.contains_key("src/keep.rs"));
    assert!(!genome.files.keys().any(|path| path.contains("fixtures")));
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

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/index.ts"]
            .imports
            .contains(&"src/auth.ts".into())
    );
    assert!(
        genome.files["src/auth.ts"]
            .exports
            .contains(&"Session".into())
    );
    assert_eq!(genome.dependents["src/auth.ts"], 1);
}

#[test]
fn zero_limit_still_emits_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");

    let genome = Genome::index(root).unwrap();
    let projected = genome.project(0);
    assert!(projected.starts_with("<genome>\n"), "{projected}");
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

    let genome = Genome::index(root).unwrap();
    let plain = genome.project(2);
    let biased = genome.project_with(2, &["src/not/indexed.rs".to_owned()]);
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

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files.contains_key("src/forge<tag>.rs"),
        "the file is indexed; only the projection drops it"
    );

    let projected = genome.project(4);
    assert!(projected.contains("src/plain.rs"), "{projected}");
    assert!(
        !projected.contains("forge"),
        "an angle-bracket name must not reach the frame: {projected}"
    );
    assert_eq!(projected.matches("</genome>").count(), 1, "{projected}");
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

    let genome = Genome::index(root).unwrap();
    let projected = genome.project(4);
    assert!(
        projected.contains("src/live.rs:(→0) [RECENT]"),
        "{projected}"
    );
    assert!(projected.contains("src/stale.rs:(→0)\n"), "{projected}");
    assert!(
        !projected.contains("[NEW]"),
        "the marker means recently modified, not newly created: {projected}"
    );
    assert!(
        !projected.contains("src/stale.rs:(→0) [RECENT]"),
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
        Genome::index(dir.path().join("missing")).is_err(),
        "a missing root must not index as empty"
    );
    let as_file = dir.path().join("plain.rs");
    fs::write(&as_file, "pub fn ok() {}\n").unwrap();
    assert!(
        Genome::index(&as_file).is_err(),
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

    let genome = Genome::index(&root).expect("one odd child must not sink the tree");
    assert!(
        genome.files.contains_key("src/lib.rs"),
        "{:?}",
        genome.files.keys()
    );
    assert!(!genome.files.contains_key("odd"));
    assert!(
        !genome.files.keys().any(|path| path.starts_with("locked/")),
        "{:?}",
        genome.files.keys()
    );
}
