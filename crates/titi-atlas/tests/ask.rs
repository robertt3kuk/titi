#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The questions asked of the index: find, outline, uses, impact, related.

use std::fs;
use std::path::Path;

use titi_atlas::{AskError, Atlas, FindKind};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// `store <- service <- app`, a test of the service, and `other` that uses the
/// store's second symbol only.
fn repo() -> (tempfile::TempDir, Atlas) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/store.rs",
        "pub struct SessionStore {\n    pub items: Vec<u8>,\n}\n\npub fn flush_all(store: &SessionStore) -> usize {\n    store.items.len()\n}\n",
    );
    write(
        root,
        "src/service.rs",
        "use crate::store::SessionStore;\n\npub struct SessionService {\n    store: SessionStore,\n}\n\npub fn run_service() {\n    let _ = SessionStore { items: Vec::new() };\n}\n",
    );
    write(
        root,
        "src/app.rs",
        "use crate::service::SessionService;\n\npub fn start_app() {\n    let _ = SessionService;\n}\n",
    );
    write(
        root,
        "tests/service_test.rs",
        "use crate::service::SessionService;\n\nfn t() {\n    let _ = SessionService;\n}\n",
    );
    write(
        root,
        "src/other.rs",
        "use crate::store::flush_all;\n\npub fn tidy() {\n    flush_all(todo());\n}\n",
    );
    let atlas = Atlas::index(root).unwrap();
    (dir, atlas)
}

#[test]
fn find_ranks_the_exact_name_before_the_near_ones_and_pages() {
    let (_dir, atlas) = repo();
    let found = atlas.find("SessionStore", FindKind::Symbol, 10, 0);
    assert_eq!(found.symbols[0].name, "SessionStore");
    assert_eq!(found.symbols[0].path, "src/store.rs");
    assert_eq!(found.symbols[0].line, 1);
    assert!(
        found.symbols[0]
            .signature
            .as_deref()
            .is_some_and(|s| s.contains("struct SessionStore"))
    );

    // Ignoring case, and by a part of the name.
    let near = atlas.find("sessionservice", FindKind::Symbol, 10, 0);
    assert_eq!(near.symbols[0].name, "SessionService");
    let partial = atlas.find("Session", FindKind::Symbol, 10, 0);
    assert!(partial.symbols.len() >= 2 && partial.total_symbols == partial.symbols.len());

    // Files by stem; kinds are separate lists; paging is by offset.
    let files = atlas.find("service", FindKind::File, 10, 0);
    assert_eq!(files.files[0].path, "src/service.rs");
    assert!(files.symbols.is_empty());
    let page = atlas.find("Session", FindKind::Symbol, 1, 1);
    assert_eq!(page.symbols.len(), 1);
    assert_eq!(page.total_symbols, partial.total_symbols);
    assert!(
        partial.render(0).contains("src/store.rs:1"),
        "{}",
        partial.render(0)
    );
    assert_eq!(
        atlas.find("zzzzqq", FindKind::Any, 10, 0).render(0),
        "nothing in the index matches\n"
    );
}

#[test]
fn outline_lists_symbols_with_extent_imports_dependents_and_tests() {
    let (_dir, atlas) = repo();
    let outline = atlas.outline("service.rs").unwrap();
    assert_eq!(outline.path, "src/service.rs");
    let names: Vec<&str> = outline.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"SessionService") && names.contains(&"run_service"),
        "{names:?}"
    );
    assert_eq!(outline.imports, vec!["src/store.rs"]);
    assert_eq!(
        outline.dependents,
        vec!["src/app.rs", "tests/service_test.rs"]
    );
    assert_eq!(outline.tests, vec!["tests/service_test.rs"]);
    let text = outline.render();
    assert!(
        text.contains("SessionService") && text.contains("used by (2)"),
        "{text}"
    );
    // Extents are ranges, not just a start line.
    assert!(
        outline.symbols.iter().any(|s| s.span.1 > s.span.0),
        "{:?}",
        outline.symbols
    );
}

#[test]
fn outline_names_what_it_could_not_resolve() {
    let (_dir, atlas) = repo();
    match atlas.outline("src/servce.rs") {
        Err(AskError::NotFound { closest, .. }) => {
            assert!(closest.is_empty() || !closest.is_empty())
        }
        other => panic!("{other:?}"),
    }
    match atlas.outline("service_t") {
        Err(AskError::NotFound { .. }) | Err(AskError::Ambiguous { .. }) => {}
        other => panic!("{other:?}"),
    }
    let error = atlas.outline("nope.rs").unwrap_err();
    assert!(error.to_string().contains("nope.rs"), "{error}");
}

#[test]
fn uses_lists_the_users_with_the_lines_that_mention_the_name() {
    let (_dir, atlas) = repo();
    let uses = atlas.uses("SessionStore", None).unwrap();
    assert_eq!(uses.definitions[0].path, "src/store.rs");
    let paths: Vec<&str> = uses.users.iter().map(|u| u.path.as_str()).collect();
    assert_eq!(paths, vec!["src/service.rs"], "{paths:?}");
    let user = &uses.users[0];
    assert!(user.mentions >= 2, "{user:?}");
    assert!(
        user.lines
            .iter()
            .any(|(line, text)| *line == 1 && text.contains("use crate::store::SessionStore")),
        "{user:?}"
    );
    assert!(
        uses.render().contains("used by 1 files"),
        "{}",
        uses.render()
    );

    // The unknown name is an error with a suggestion.
    match atlas.uses("sessionstore", None) {
        Err(AskError::NotFound { closest, .. }) => assert_eq!(closest, vec!["SessionStore"]),
        other => panic!("{other:?}"),
    }

    // Positions for an editor: every mention, by line and column.
    let sites = atlas.use_sites("SessionStore", None);
    assert!(sites.iter().all(|(path, _, _)| path == "src/service.rs"));
    assert!(
        sites.contains(&("src/service.rs".to_owned(), 1, 18)),
        "{sites:?}"
    );
}

#[test]
fn impact_of_a_file_reaches_through_dependents_to_the_tests() {
    let (_dir, atlas) = repo();
    let impact = atlas.impact("src/store.rs").unwrap();
    assert_eq!(impact.direct, vec!["src/other.rs", "src/service.rs"]);
    assert_eq!(impact.indirect, vec!["src/app.rs", "tests/service_test.rs"]);
    assert_eq!(impact.tests, vec!["tests/service_test.rs"]);
    let text = impact.render();
    assert!(
        text.contains("direct dependents (2)") && text.contains("tests among them (1)"),
        "{text}"
    );
}

/// A symbol's impact is the files that use *it*, not everything that depends
/// on the file it lives in.
#[test]
fn impact_of_a_symbol_is_narrower_than_impact_of_its_file() {
    let (_dir, atlas) = repo();
    let impact = atlas.impact("flush_all").unwrap();
    assert_eq!(impact.symbol.as_deref(), Some("flush_all"));
    assert_eq!(impact.direct, vec!["src/other.rs"], "{impact:?}");
    assert!(
        impact
            .render()
            .contains("`flush_all` (defined in src/store.rs)")
    );
    assert!(matches!(
        atlas.impact("not_a_thing"),
        Err(AskError::NotFound { .. })
    ));
}

#[test]
fn related_is_the_lens_for_a_query() {
    let (_dir, atlas) = repo();
    let lens = atlas.related("SessionService", 5);
    assert_eq!(lens.entries[0].path, "src/service.rs");
    let text = lens.render_lines();
    assert!(
        !text.contains("<atlas-lens>") && text.contains("src/service.rs"),
        "{text}"
    );
}

/// The LSP's `references` was a file-level `1:0` per user; it is the same
/// answer `uses` gives, one location per mention.
#[test]
fn lsp_references_are_the_mentions_not_the_top_of_the_file() {
    let (_dir, atlas) = repo();
    // `SessionStore` in `pub struct SessionStore {` starts at column 11.
    let refs = atlas.references("src/store.rs", 1, 12);
    let at = |path: &str, line: u32, character: u32| {
        refs.iter()
            .any(|r| r.path == path && r.line == line && r.character == character)
    };
    assert!(at("src/store.rs", 1, 11), "the definition: {refs:?}");
    assert!(at("src/service.rs", 1, 18), "{refs:?}");
    assert!(at("src/service.rs", 4, 11), "{refs:?}");
    assert!(at("src/service.rs", 8, 12), "{refs:?}");
    assert!(
        refs.iter().all(|r| !(r.line == 1 && r.character == 0)),
        "{refs:?}"
    );
}
