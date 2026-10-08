#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Cursor;
use std::path::Path;

use titi_genome::{Genome, Level, Severity, serve_lsp};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn pos(source: &str, needle: &str) -> (u32, u32) {
    let byte = source.find(needle).expect(needle);
    let line = source[..byte].bytes().filter(|unit| *unit == b'\n').count() as u32 + 1;
    let character = source[..byte]
        .rfind('\n')
        .map(|index| byte - index - 1)
        .unwrap_or(byte) as u32;
    (line, character)
}

fn frame(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

#[test]
fn unresolved_crate_import_is_a_diagnostic_and_not_an_edge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "use crate::missing::Thing;\npub fn present() {}\n",
    );

    let genome = Genome::index(root).unwrap();
    let diagnostics = genome.check();
    let hit = diagnostics
        .iter()
        .find(|item| item.code == "unresolved-import")
        .expect("unresolved-import");
    assert!(
        hit.message.contains("missing"),
        "message should name the specifier: {}",
        hit.message
    );
    assert!(
        !genome.files["src/lib.rs"]
            .imports
            .iter()
            .any(|path| path.contains("missing")),
        "a failed resolve must not become an edge: {:?}",
        genome.files["src/lib.rs"].imports
    );
    assert!(
        genome.files["src/lib.rs"]
            .unresolved_imports
            .iter()
            .any(|spec| spec.contains("missing"))
    );
}

#[test]
fn a_syntax_error_is_a_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/broken.rs", "pub fn broken( {\n");

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.files["src/broken.rs"].syntax_errors > 0,
        "tree-sitter ERROR nodes must be counted"
    );
    let hit = genome
        .check()
        .into_iter()
        .find(|item| item.code == "syntax-error")
        .expect("syntax-error");
    assert_ne!(hit.severity, Severity::Info);
    assert!(hit.message.contains("src/broken.rs"), "{}", hit.message);
    assert!(
        hit.message.contains("1")
            || hit
                .message
                .contains(&genome.files["src/broken.rs"].syntax_errors.to_string())
    );
}

#[test]
fn ambiguous_session_has_no_definition_and_short_names_stay_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/a.rs",
        "pub struct Session;\npub fn new() {}\npub fn get() {}\npub fn join() {}\n",
    );
    write(
        root,
        "src/b.rs",
        "pub struct Session;\npub fn new() {}\npub fn get() {}\npub fn join() {}\n",
    );
    let mention = "pub fn run(s: Session) {}\n";
    write(root, "src/c.rs", mention);

    let genome = Genome::index(root).unwrap();
    let diagnostics = genome.check();
    let hit = diagnostics
        .iter()
        .find(|item| item.code == "ambiguous-symbol" && item.message.contains("Session"))
        .expect("ambiguous-symbol for Session");
    assert_eq!(hit.severity, Severity::Info, "{}", hit.message);
    assert!(hit.message.contains("src/a.rs"), "{}", hit.message);
    assert!(hit.message.contains("src/b.rs"), "{}", hit.message);
    for quiet in ["`new`", "`get`", "`join`"] {
        assert!(
            !diagnostics
                .iter()
                .any(|item| item.code == "ambiguous-symbol" && item.message.contains(quiet)),
            "{quiet} must not be reported: {diagnostics:?}"
        );
    }

    let (line, character) = pos(mention, "Session");
    assert!(genome.definition("src/c.rs", line, character).is_none());

    let site = genome.files["src/a.rs"]
        .export_sites
        .iter()
        .find(|site| site.name == "Session")
        .expect("export site");
    let on_export = genome
        .definition("src/a.rs", site.line, site.character)
        .expect("definition on the export itself");
    assert_eq!(on_export.path, "src/a.rs");
    assert_eq!(on_export.line, site.line);
    assert_eq!(on_export.character, site.character);
}

#[test]
fn qualified_join_resolves_and_method_join_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let hub = "pub fn join() {}\n";
    let caller =
        "pub fn run(path: &Path) {\n    let _ = hub::join();\n    let _ = path.join(\"x\");\n}\n";
    write(root, "src/hub.rs", hub);
    write(root, "src/caller.rs", caller);

    let genome = Genome::index(root).unwrap();
    let site = genome.files["src/hub.rs"]
        .export_sites
        .iter()
        .find(|site| site.name == "join")
        .expect("join export");
    let (hub_line, hub_character) = pos(hub, "join");
    assert_eq!(site.line, hub_line);
    assert_eq!(site.character, hub_character);

    let (call_line, call_at) = pos(caller, "hub::join");
    let resolved = genome
        .definition("src/caller.rs", call_line, call_at + "hub::".len() as u32)
        .expect("hub::join resolves");
    assert_eq!(resolved.path, "src/hub.rs");
    assert_eq!(resolved.line, site.line);
    assert_eq!(resolved.character, site.character);

    let (method_line, method_at) = pos(caller, "path.join");
    assert!(
        genome
            .definition(
                "src/caller.rs",
                method_line,
                method_at + "path.".len() as u32
            )
            .is_none(),
        "path.join is a method call, not a use of the export"
    );

    let refs = genome.references("src/caller.rs", call_line, call_at + "hub::".len() as u32);
    assert!(
        refs.iter()
            .any(|loc| loc.path == "src/hub.rs" && loc.line == site.line),
        "references include the definer: {refs:?}"
    );
    assert!(
        refs.iter().any(|loc| loc.path == "src/caller.rs"),
        "references include the use file: {refs:?}"
    );
    assert_eq!(
        genome.document_symbols("src/missing.rs").len(),
        0,
        "unknown path is empty"
    );
    assert!(
        genome
            .document_symbols("src/hub.rs")
            .iter()
            .any(|site| site.name == "join")
    );
}

#[test]
fn check_keeps_syntax_ahead_of_imports_and_caps_at_32() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut body = String::from("pub fn broken( {\n");
    for index in 0..40 {
        body.push_str(&format!("use crate::missing{index}::Thing;\n"));
    }
    write(root, "src/broken.rs", &body);

    let diagnostics = Genome::index(root).unwrap().check();
    assert_eq!(diagnostics.len(), 32);
    assert_eq!(diagnostics[0].code, "syntax-error");
    assert!(
        diagnostics[1..]
            .iter()
            .all(|item| item.code == "unresolved-import")
    );
}

#[test]
fn serve_lsp_answers_initialize_and_document_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub struct Widget;\n");
    let uri = format!("file://{}", root.join("src/lib.rs").display());
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let symbols = format!(
        r#"{{"jsonrpc":"2.0","id":"sym","method":"textDocument/documentSymbol","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
    );
    let mut input = frame(init);
    input.extend(frame(&symbols));
    input.extend(frame(
        r#"{"jsonrpc":"2.0","id":9,"method":"textDocument/hover","params":{}}"#,
    ));
    input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

    let mut output = Vec::new();
    serve_lsp(root, Cursor::new(input), &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(
        text.contains("\"name\":\"titi-genome\""),
        "serverInfo.name missing: {text}"
    );
    assert!(
        text.contains("\"name\":\"Widget\""),
        "document symbol missing: {text}"
    );
    assert!(
        text.contains("\"id\":\"sym\""),
        "string id must be echoed: {text}"
    );
    assert!(
        text.contains("-32601"),
        "unknown method must be method-not-found: {text}"
    );
}
/// Capability is reference information, not a finding. `check` reports what is
/// wrong in the tree — so a tree with nothing wrong stays quiet and a `clean`
/// answer keeps its meaning — and the levels live behind an accessor a caller
/// asks for explicitly.
#[test]
fn a_healthy_tree_has_no_findings_and_capability_is_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    write(root, "src/b.rs", "pub fn b() {}\n");
    write(
        root,
        "src/App.java",
        "package com.acme;\npublic class App {}\n",
    );
    write(root, "cmd/main.go", "package main\n\nfunc Main() {}\n");

    let genome = Genome::index(root).unwrap();
    assert!(
        genome.check().is_empty(),
        "nothing is wrong, so nothing is reported: {:?}",
        genome.check()
    );

    // The roster is there for whoever asks.
    let roster = Genome::capabilities();
    let names: Vec<&str> = roster.iter().map(|cap| cap.language).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "sorted by language");
    assert_eq!(
        names.len(),
        names.iter().collect::<std::collections::HashSet<_>>().len(),
        "one entry per language: {names:?}"
    );

    let rust = roster
        .iter()
        .find(|cap| cap.language == "rust")
        .expect("rust");
    assert_eq!(rust.level, Level::Full);
    assert_eq!(rust.extensions, vec!["rs"]);

    let java = roster
        .iter()
        .find(|cap| cap.language == "java")
        .expect("java");
    assert_eq!(java.level, Level::Full);
    assert_eq!(java.extensions, vec!["java"]);
    assert!(java.note.contains("syntax tree"), "{}", java.note);

    // Two rows parse TypeScript and its `.tsx`, and they are one language.
    let typescript = roster
        .iter()
        .find(|cap| cap.language == "typescript")
        .expect("typescript");
    assert_eq!(typescript.extensions, vec!["ts", "mts", "cts", "tsx"]);
}

/// The roster is a property of the build, so it answers without a workspace:
/// no index, no walk, nothing to fail on.
#[test]
fn the_roster_needs_no_workspace() {
    let roster = Genome::capabilities();
    assert!(roster.len() >= 13, "{} entries", roster.len());
    let full: Vec<&str> = roster
        .iter()
        .filter(|cap| cap.level == Level::Full)
        .map(|cap| cap.language)
        .collect();
    assert_eq!(
        full,
        vec![
            "c",
            "c++",
            "go",
            "java",
            "javascript",
            "kotlin",
            "python",
            "rust",
            "typescript"
        ]
    );
    let heuristic = roster
        .iter()
        .filter(|cap| cap.level == Level::Heuristic)
        .count();
    assert_eq!(heuristic, roster.len() - full.len());
}

/// The handshake carries the roster; the per-file reply carries findings and
/// nothing else. A constant hint about the language in every file would be
/// noise in an editor, which is why the two channels are different.
#[test]
fn lsp_reports_the_level_roster_and_keeps_it_out_of_file_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub struct Widget;\n");
    let uri = format!("file://{}", root.join("src/lib.rs").display());
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let diagnostic = format!(
        r#"{{"jsonrpc":"2.0","id":"diag","method":"textDocument/diagnostic","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
    );
    let mut input = frame(init);
    input.extend(frame(&diagnostic));
    input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

    let mut output = Vec::new();
    serve_lsp(root, Cursor::new(input), &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(
        text.contains(r#""language":"swift""#)
            && text.contains(r#""level":"Heuristic""#)
            && text.contains(r#""language":"rust""#)
            && text.contains(r#""level":"Full""#),
        "the handshake must report the roster: {text}"
    );
    assert!(
        text.contains(r#""items":[]"#),
        "a healthy file answers with no findings: {text}"
    );
    assert!(
        !text.contains("capability"),
        "the roster must not ride in the per-file reply: {text}"
    );
}
