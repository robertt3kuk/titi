#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The lens: what this task is about, from the prompt, the session and the
//! working tree, spread over the dependency graph.

use std::fs;
use std::path::Path;

use titi_atlas::{Atlas, Lens, LensBudget, SeedInput, estimate_tokens};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// A hub everyone uses, a feature chain `app -> service -> store`, and a pile
/// of unrelated files around them.
fn repo() -> (tempfile::TempDir, Atlas) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/hub.rs",
        "pub fn hub_call() {}\npub struct HubConfig;\n",
    );
    for n in 0..12 {
        write(
            root,
            &format!("src/other{n}.rs"),
            "use crate::hub::hub_call;\npub fn other() { hub_call(); }\n",
        );
    }
    write(
        root,
        "src/store.rs",
        "pub struct SessionStore;\npub fn persist_session() {}\n",
    );
    write(
        root,
        "src/service.rs",
        "use crate::store::SessionStore;\npub struct SessionService;\npub fn run_service() { let _ = SessionStore; }\n",
    );
    write(
        root,
        "src/app.rs",
        "use crate::service::SessionService;\npub fn start_app() { let _ = SessionService; }\n",
    );
    let atlas = Atlas::index(root).unwrap();
    (dir, atlas)
}

fn lens_for(atlas: &Atlas, prompt: &str, touched: &[String], changed: &[String]) -> Lens {
    let seeds = atlas.seeds(&SeedInput {
        prompt,
        touched,
        changed,
    });
    atlas.lens(&seeds, LensBudget::tokens(400))
}

fn paths(lens: &Lens) -> Vec<&str> {
    lens.entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect()
}

#[test]
fn a_prompt_with_no_signal_makes_no_lens() {
    let (_dir, atlas) = repo();
    let seeds = atlas.seeds(&SeedInput {
        prompt: "hello there, how are you today",
        ..SeedInput::default()
    });
    assert!(seeds.is_empty());
    assert!(atlas.lens(&seeds, LensBudget::tokens(400)).is_empty());
}

/// The static map would list the hub; this task is about the session chain.
#[test]
fn a_symbol_the_prompt_names_puts_its_file_and_neighbours_first() {
    let (_dir, atlas) = repo();
    let lens = lens_for(&atlas, "SessionService forgets to flush on exit", &[], &[]);
    let listed = paths(&lens);
    assert_eq!(listed[0], "src/service.rs", "{listed:?}");
    assert!(listed.contains(&"src/store.rs"), "a dependency: {listed:?}");
    assert!(listed.contains(&"src/app.rs"), "a dependent: {listed:?}");
    assert!(
        !listed.contains(&"src/hub.rs") && !listed.contains(&"src/other0.rs"),
        "unrelated files stay out: {listed:?}"
    );
    let first = &lens.entries[0];
    assert!(
        first.why.contains("defines SessionService"),
        "{}",
        first.why
    );
    assert_eq!(
        first.symbols.first().map(String::as_str),
        Some("SessionService")
    );
    let dependency = lens
        .entries
        .iter()
        .find(|e| e.path == "src/store.rs")
        .unwrap();
    assert_eq!(dependency.why, "used by src/service.rs", "{dependency:?}");
}

#[test]
fn a_file_named_by_basename_or_with_a_line_is_a_seed() {
    let (_dir, atlas) = repo();
    for prompt in [
        "panic at src/service.rs:12:5 in run",
        "look at service.rs please",
        "error in ./src/service.rs",
    ] {
        let lens = lens_for(&atlas, prompt, &[], &[]);
        assert_eq!(lens.entries[0].path, "src/service.rs", "{prompt}");
        assert!(
            lens.entries[0].why.contains("named in the prompt"),
            "{prompt}"
        );
    }
}

#[test]
fn touched_and_changed_files_seed_in_any_spelling() {
    let (dir, atlas) = repo();
    let abs = dir.path().join("src/store.rs").display().to_string();
    let lens = lens_for(&atlas, "", &[abs], &["./src/app.rs".to_owned()]);
    let listed = paths(&lens);
    assert!(
        listed.contains(&"src/store.rs") && listed.contains(&"src/app.rs"),
        "{listed:?}"
    );
    assert!(
        lens.entries
            .iter()
            .any(|e| e.why.contains("touched this session")),
        "{lens:?}"
    );
    assert!(
        lens.entries
            .iter()
            .any(|e| e.why.contains("changed in the working tree"))
    );
}

/// What the lens exists for: a leaf the static ranking buries comes first when
/// the task is about it.
#[test]
fn a_seeded_leaf_outranks_the_hub() {
    let (_dir, atlas) = repo();
    let lens = lens_for(&atlas, "rename persist_session", &[], &[]);
    assert_eq!(lens.entries[0].path, "src/store.rs");
    let static_map = atlas.project(3);
    assert!(
        !static_map.contains("src/store.rs")
            || static_map.find("src/hub.rs") < static_map.find("src/store.rs"),
        "the static map ranks the hub first: {static_map}"
    );
}

#[test]
fn an_identifier_with_no_symbol_is_reported_not_guessed() {
    let (_dir, atlas) = repo();
    let lens = lens_for(
        &atlas,
        "SessionService calls flush_cache_entries then fails",
        &[],
        &[],
    );
    assert_eq!(lens.entries[0].path, "src/service.rs");
    assert_eq!(lens.unmapped, vec!["flush_cache_entries".to_owned()]);
    assert!(lens.render(0).contains("unmapped: flush_cache_entries"));
}

#[test]
fn the_budget_bounds_the_block() {
    let (_dir, atlas) = repo();
    let seeds = atlas.seeds(&SeedInput {
        prompt: "SessionService hub_call SessionStore persist_session start_app",
        ..SeedInput::default()
    });
    let wide = atlas.lens(&seeds, LensBudget::tokens(400));
    let tight = atlas.lens(&seeds, LensBudget::tokens(60));
    assert!(
        tight.entries.len() < wide.entries.len(),
        "{} vs {}",
        tight.entries.len(),
        wide.entries.len()
    );
    assert!(
        !tight.entries.is_empty(),
        "always at least the heaviest file"
    );
    assert!(estimate_tokens(&wide.render(0)) <= 400);
    assert!(wide.entries.len() <= titi_atlas::MAX_LENS_FILES);
}

#[test]
fn the_rendered_block_names_its_paths_and_marks_a_backlog() {
    let (_dir, atlas) = repo();
    let lens = lens_for(&atlas, "SessionService", &[], &[]);
    let block = lens.render(0);
    assert!(
        block.starts_with("<atlas-lens>\n") && block.ends_with("</atlas-lens>"),
        "{block}"
    );
    let listed: Vec<String> = Lens::paths_in(&block)
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
    assert_eq!(listed, paths(&lens));
    assert!(lens.render(3).starts_with("<atlas-lens pending=\"3\">"));
}

/// The index the lens walks is rebuilt when the graph is: a new export is a
/// seed on the next call.
#[test]
fn a_new_export_is_found_after_an_update() {
    let (dir, mut atlas) = repo();
    assert!(lens_for(&atlas, "frobnicate_widgets", &[], &[]).is_empty());
    write(
        dir.path(),
        "src/store.rs",
        "pub struct SessionStore;\npub fn frobnicate_widgets() {}\n",
    );
    atlas.apply_changes(&["src/store.rs".to_owned()]).unwrap();
    let lens = lens_for(&atlas, "frobnicate_widgets", &[], &[]);
    assert_eq!(lens.entries[0].path, "src/store.rs");
}
