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

/// A map rendered while the background indexer is behind says so, and one
/// rendered when it is caught up is byte-identical to the map before there was
/// a policy: the attribute is the whole of the visible difference.
#[test]
fn a_backlog_is_named_in_the_map_header() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/hub.rs", "pub fn hub() {}\n");
    let genome = Genome::index(root).unwrap();

    let caught_up = genome.project_with(10, &[]);
    assert!(caught_up.starts_with("<genome>\n"), "{caught_up}");
    assert_eq!(caught_up, genome.project(10));

    let behind = genome.project_with_pending(10, &[], 3);
    assert!(behind.starts_with("<genome pending=\"3\">\n"), "{behind}");
    assert!(
        behind.ends_with("</genome>") && behind.contains("src/hub.rs"),
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

/// A `touch` moves the clock, not the bytes. The size/mtime pre-filter cannot
/// tell that from a real edit, so the file is read — and the content hash
/// settles it without paying for a parse.
#[test]
fn a_no_op_mtime_touch_does_not_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");
    let mut genome = Genome::index(root).unwrap();

    // Untouched tree: the pre-filter skips every file, so nothing is read.
    let quiet = genome.refresh(root).unwrap();
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

    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 0, "same bytes, no re-parse");
    assert_eq!(stats.content_unchanged, 1, "read, and settled by hash");
    assert!(!stats.graph_recomputed);
    assert_eq!(stats.total, 2);
    assert_eq!(genome.files["src/a.rs"].exports, vec!["a".to_owned()]);
    assert_ne!(genome.files["src/a.rs"].mtime, before, "the clock moved");
}

/// The same length is not the same file: `size` cannot clear a rewrite, so the
/// bytes are read and the hash disagrees.
#[test]
fn an_edit_that_keeps_the_length_is_still_reparsed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn aa() {}\n");
    let mut genome = Genome::index(root).unwrap();

    write(root, "src/a.rs", "pub fn ab() {}\n");
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1, "same length, different bytes");
    assert_eq!(stats.content_unchanged, 0);
    assert!(stats.graph_recomputed, "an export changed name");
    assert!(genome.files["src/a.rs"].exports.contains(&"ab".to_owned()));
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
    write(root, "src/leaf.rs", "pub fn leaf() { hub(); }\n");
    let mut genome = Genome::index(root).unwrap();
    assert_eq!(
        genome.dependents["src/hub.rs"], 1,
        "the fixture has an edge"
    );

    genome.ranks.insert("src/hub.rs".to_owned(), 123.5);
    genome.dependents.insert("src/leaf.rs".to_owned(), 9);

    write(
        root,
        "src/leaf.rs",
        "// a comment\npub fn leaf() { hub(); }\n",
    );
    let stats = genome.refresh(root).unwrap();

    assert_eq!(stats.parsed, 1, "the bytes changed");
    assert_eq!(stats.content_unchanged, 0, "…and they did change");
    assert!(
        !stats.graph_recomputed,
        "nothing the graph is built from moved"
    );
    assert_eq!(genome.ranks["src/hub.rs"], 123.5, "ranks untouched");
    assert_eq!(genome.dependents["src/leaf.rs"], 9, "dependents untouched");
    assert_eq!(
        genome.files["src/leaf.rs"].used_symbols,
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
    let mut genome = Genome::index(root).unwrap();
    genome.ranks.insert("src/hub.rs".to_owned(), 123.5);
    let before = genome.symbols.clone();

    write(root, "src/hub.rs", "pub fn hub() {}\npub fn extra() {}\n");
    let stats = genome.refresh(root).unwrap();

    assert_eq!(stats.parsed, 1);
    assert!(stats.graph_recomputed, "a new export is a new graph input");
    assert_ne!(genome.ranks["src/hub.rs"], 123.5, "the maps were rebuilt");
    assert_ne!(genome.symbols, before);
    assert!(genome.symbols.contains_key("extra"));
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
    let mut genome = Genome::index(root).unwrap();

    write(root, "src/a.rs", "pub fn a() {}\npub fn a2() {}\n");
    write(root, "src/c.rs", "pub fn c() {}\n");
    let stats = genome.apply_changes(&["src/a.rs".to_owned()]).unwrap();
    assert_eq!(stats.parsed, 1, "only the named path was read");
    assert_eq!(stats.total, 2, "src/c.rs was not named");
    assert!(genome.files["src/a.rs"].exports.contains(&"a2".to_owned()));

    // The walk is what finds the file nobody named.
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1);
    assert_eq!(stats.total, 3);
    assert!(genome.files.contains_key("src/c.rs"));

    // A named path that is gone leaves the index, and only that path.
    fs::remove_file(root.join("src/b.rs")).unwrap();
    let stats = genome.apply_changes(&["src/b.rs".to_owned()]).unwrap();
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.total, 2);
    assert!(stats.graph_recomputed, "a node left the graph");
    assert!(!genome.files.contains_key("src/b.rs"));
}

/// An unchanged named path is a `stat` and no more: not parsed, not read, and
/// the ranking is not rebuilt.
#[test]
fn apply_changes_on_unchanged_paths_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    let mut genome = Genome::index(root).unwrap();
    genome.ranks.insert("src/a.rs".to_owned(), 7.0);

    let stats = genome
        .apply_changes(&["src/a.rs".to_owned(), "src/absent.rs".to_owned()])
        .unwrap();
    assert_eq!(stats.parsed, 0);
    assert_eq!(stats.content_unchanged, 0);
    assert_eq!(stats.removed, 0, "a path never in the index is no removal");
    assert!(!stats.graph_recomputed);
    assert_eq!(genome.ranks["src/a.rs"], 7.0);
}

/// A targeted update confirms content by hash exactly as a walk does: a
/// stream of touches the watcher reports costs reads, not parses.
#[test]
fn apply_changes_confirms_content_by_hash_too() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    let mut genome = Genome::index(root).unwrap();

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

    let stats = genome.apply_changes(&["src/a.rs".to_owned()]).unwrap();
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

    let mut genome = Genome::index(root).unwrap();
    assert_eq!(genome.files.len(), 4);
    assert!(
        genome.files["src/user.rs"]
            .used_symbols
            .contains(&"alpha".to_owned())
    );
    assert!(
        genome.files["src/other.rs"].used_symbols.is_empty(),
        "nothing exports `fresh_export` yet"
    );
    assert_eq!(
        genome.ref_index["fresh_export"],
        vec!["src/other.rs".to_owned()]
    );

    write(
        root,
        "src/lib.rs",
        "pub fn alpha() {}\npub fn fresh_export() {}\n",
    );
    let stats = genome.refresh(root).unwrap();
    assert_eq!(stats.parsed, 1, "one file changed");
    assert!(stats.walked, "a refresh lists the tree");
    assert_eq!(
        stats.reresolved, 2,
        "the edited file and the file that mentions the new name"
    );
    assert!(
        stats.reresolved < genome.files.len(),
        "every other file kept the resolution it had"
    );

    assert_eq!(
        genome.files["src/other.rs"].used_symbols,
        vec!["fresh_export".to_owned()]
    );
    assert!(genome.files["src/quiet.rs"].used_symbols.is_empty());
    assert_eq!(
        genome.symbols["fresh_export"].files,
        vec!["src/lib.rs".to_owned()]
    );
    assert_eq!(genome.symbols["fresh_export"].users, 1);
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

    let mut genome = Genome::index(root).unwrap();
    assert!(genome.files["src/app.ts"].imports.is_empty());
    assert_eq!(
        genome.files["src/app.ts"].unresolved_imports,
        vec!["./missing".to_owned()]
    );

    write(root, "src/missing.ts", "export function helper() {}\n");
    let stats = genome
        .apply_changes(&["src/missing.ts".to_owned()])
        .unwrap();
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
        genome.files["src/app.ts"].imports,
        vec!["src/missing.ts".to_owned()],
        "the importer was fixed without being re-parsed"
    );
    assert!(genome.files["src/app.ts"].unresolved_imports.is_empty());
    assert_eq!(genome.ranks.len(), 2, "the new file is ranked");
    assert_eq!(genome.dependents["src/missing.ts"], 1, "the edge ranks");
}
