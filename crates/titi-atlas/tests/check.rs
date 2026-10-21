#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fs;
use std::io::Cursor;
use std::path::Path;

use titi_atlas::{Atlas, Level, Severity, serve_lsp};

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

    let atlas = Atlas::index(root).unwrap();
    let diagnostics = atlas.check();
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
        !atlas.files["src/lib.rs"]
            .imports
            .iter()
            .any(|path| path.contains("missing")),
        "a failed resolve must not become an edge: {:?}",
        atlas.files["src/lib.rs"].imports
    );
    assert!(
        atlas.files["src/lib.rs"]
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

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.files["src/broken.rs"].syntax_errors > 0,
        "tree-sitter ERROR nodes must be counted"
    );
    let hit = atlas
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
                .contains(&atlas.files["src/broken.rs"].syntax_errors.to_string())
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

    let atlas = Atlas::index(root).unwrap();
    let diagnostics = atlas.check();
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
    assert!(atlas.definition("src/c.rs", line, character).is_none());

    let site = atlas.files["src/a.rs"]
        .export_sites
        .iter()
        .find(|site| site.name == "Session")
        .expect("export site");
    let on_export = atlas
        .definition("src/a.rs", site.line, site.character)
        .expect("definition on the export itself");
    assert_eq!(on_export.path, "src/a.rs");
    assert_eq!(on_export.line, site.line);
    assert_eq!(on_export.character, site.character);
}

/// A name two *crates* export is not a finding: `Level` in one crate and
/// `Level` in another are unrelated types, and nothing in either can be
/// confused by the other. The graph still draws no edge for the name — that
/// is the resolution rule and it does not move — but the user is not told.
#[test]
fn a_name_two_crates_share_is_not_a_finding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for crate_name in ["one", "two"] {
        write(
            root,
            &format!("crates/{crate_name}/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        write(
            root,
            &format!("crates/{crate_name}/src/lib.rs"),
            "pub struct Shared;\n",
        );
    }

    let atlas = Atlas::index(root).unwrap();
    assert_eq!(
        atlas.files.len(),
        2,
        "both crates are indexed: {:?}",
        atlas.files.keys().collect::<Vec<_>>()
    );
    let diagnostics = atlas.check();
    assert!(
        !diagnostics
            .iter()
            .any(|item| item.code == "ambiguous-symbol"),
        "{diagnostics:?}"
    );
    // The name is still ambiguous to the graph: no edge, which is what
    // `MAX_DEFINERS` is for.
    assert!(
        atlas.symbols["Shared"].files.len() == 2,
        "{:?}",
        atlas.symbols["Shared"]
    );
}

/// Two files of *one* crate exporting the same name is the collision a
/// reader can act on, and it is reported with both files named.
#[test]
fn a_name_two_files_of_one_crate_share_is_a_finding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "crates/one/Cargo.toml", "[package]\nname = \"x\"\n");
    write(root, "crates/one/src/a.rs", "pub struct Shared;\n");
    write(root, "crates/one/src/b.rs", "pub struct Shared;\n");

    let diagnostics = Atlas::index(root).unwrap().check();
    let hit = diagnostics
        .iter()
        .find(|item| item.code == "ambiguous-symbol" && item.message.contains("Shared"))
        .unwrap_or_else(|| panic!("ambiguous-symbol for Shared: {diagnostics:?}"));
    assert_eq!(hit.severity, Severity::Info, "{}", hit.message);
    assert!(
        hit.message.contains("crates/one/src/a.rs"),
        "{}",
        hit.message
    );
    assert!(
        hit.message.contains("crates/one/src/b.rs"),
        "{}",
        hit.message
    );
}

#[test]
fn qualified_join_resolves_and_method_join_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let hub = "pub fn join() {}\n";
    let caller = "use crate::hub;\npub fn run(path: &Path) {\n    let _ = hub::join();\n    let _ = path.join(\"x\");\n}\n";
    write(root, "src/hub.rs", hub);
    write(root, "src/caller.rs", caller);

    let atlas = Atlas::index(root).unwrap();
    let site = atlas.files["src/hub.rs"]
        .export_sites
        .iter()
        .find(|site| site.name == "join")
        .expect("join export");
    let (hub_line, hub_character) = pos(hub, "join");
    assert_eq!(site.line, hub_line);
    assert_eq!(site.character, hub_character);

    let (call_line, call_at) = pos(caller, "hub::join");
    let resolved = atlas
        .definition("src/caller.rs", call_line, call_at + "hub::".len() as u32)
        .expect("hub::join resolves");
    assert_eq!(resolved.path, "src/hub.rs");
    assert_eq!(resolved.line, site.line);
    assert_eq!(resolved.character, site.character);

    let (method_line, method_at) = pos(caller, "path.join");
    assert!(
        atlas
            .definition(
                "src/caller.rs",
                method_line,
                method_at + "path.".len() as u32
            )
            .is_none(),
        "path.join is a method call, not a use of the export"
    );

    let refs = atlas.references("src/caller.rs", call_line, call_at + "hub::".len() as u32);
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
        atlas.document_symbols("src/missing.rs").len(),
        0,
        "unknown path is empty"
    );
    assert!(
        atlas
            .document_symbols("src/hub.rs")
            .iter()
            .any(|site| site.name == "join")
    );
}

#[test]
fn check_keeps_syntax_ahead_of_imports_and_keeps_all_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut body = String::from("pub fn broken( {\n");
    for index in 0..40 {
        body.push_str(&format!("use crate::missing{index}::Thing;\n"));
    }
    write(root, "src/broken.rs", &body);

    let diagnostics = Atlas::index(root).unwrap().check();
    // The cap of 32 that used to sit here made a count a lower bound and kept
    // every file past the 32nd from ever getting a per-file diagnostic; the
    // report is what truncates now, and says so.
    assert_eq!(diagnostics.len(), 41);
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
        text.contains("\"name\":\"titi-atlas\""),
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
    assert!(
        text.contains("\"textDocumentSync\":1"),
        "the server must ask for full sync: {text}"
    );
}

/// The replies the server wrote, in order, as JSON.
fn frames(output: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8(output.to_vec()).unwrap();
    let mut out = Vec::new();
    let mut rest = text.as_str();
    while let Some(head) = rest.find("Content-Length: ") {
        let (len, body) = rest[head + "Content-Length: ".len()..]
            .split_once("\r\n\r\n")
            .expect("a header and a body");
        let len: usize = len.trim().parse().expect("a content length");
        let (body, tail) = body.split_at(len);
        out.push(serde_json::from_str(body).expect("a json body"));
        rest = tail;
    }
    out
}

/// The result of the reply to `id`. A client correlates by id, so a test that
/// asserts on the stream's text cannot tell which answer it read.
fn reply(frames: &[serde_json::Value], id: &str) -> serde_json::Value {
    let wanted = serde_json::Value::String(id.to_owned());
    frames
        .iter()
        .find(|frame| frame.get("id") == Some(&wanted))
        .unwrap_or_else(|| panic!("no reply to {id}: {frames:?}"))
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// A document-symbol request for `uri`.
fn symbols_request(id: &str, uri: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":"{id}","method":"textDocument/documentSymbol","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
    )
}

/// A diagnostic request for `uri`.
fn diagnostic_request(id: &str, uri: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":"{id}","method":"textDocument/diagnostic","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
    )
}

/// A `didOpen` carrying `text` — or a `didChange`, which is the same
/// notification minus the language id, because this server advertises full
/// sync and every change is the whole document.
fn change_request(method: &str, uri: &str, text: &str) -> String {
    let text = text.replace('\\', "\\\\").replace('"', "\\\"");
    let text = text.replace('\n', "\\n");
    match method {
        "textDocument/didOpen" => format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","languageId":"rust","version":1,"text":"{text}"}}}}}}"#
        ),
        _ => format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":2}},"contentChanges":[{{"text":"{text}"}}]}}}}"#
        ),
    }
}

fn close_request(uri: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{uri}"}}}}}}"#
    )
}

/// The editor's buffer is what `documentSymbol` answers from, and closing it
/// puts the file back.
///
/// The two texts share no export, so an answer that still names the disk's
/// `Widget` while the buffer holds `Gizmo` cannot pass by accident.
#[test]
fn a_buffer_is_the_truth_for_symbols_and_closing_restores_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub struct Widget;\n");
    let uri = format!("file://{}", root.join("src/lib.rs").display());

    let mut input = frame(r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}"#);
    input.extend(frame(&symbols_request("disk", &uri)));
    input.extend(frame(&change_request(
        "textDocument/didOpen",
        &uri,
        "pub struct Gizmo;\n",
    )));
    input.extend(frame(&symbols_request("open", &uri)));
    input.extend(frame(&close_request(&uri)));
    input.extend(frame(&symbols_request("closed", &uri)));
    input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

    let mut output = Vec::new();
    serve_lsp(root, Cursor::new(input), &mut output).unwrap();
    let frames = frames(&output);

    let disk = reply(&frames, "disk").to_string();
    assert!(
        disk.contains("Widget"),
        "the file on disk is the start: {disk}"
    );
    let open = reply(&frames, "open").to_string();
    assert!(
        open.contains("Gizmo") && !open.contains("Widget"),
        "the buffer must replace the file, not join it: {open}"
    );
    let closed = reply(&frames, "closed").to_string();
    assert!(
        closed.contains("Widget") && !closed.contains("Gizmo"),
        "closing must fall back to the file: {closed}"
    );
}

/// A `didChange` moves the diagnostics, which means the record was re-parsed
/// and not only re-answered.
#[test]
fn a_change_moves_the_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub fn present() {}\n");
    let uri = format!("file://{}", root.join("src/lib.rs").display());

    let mut input = frame(r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}"#);
    input.extend(frame(&change_request(
        "textDocument/didOpen",
        &uri,
        "pub fn present() {}\n",
    )));
    input.extend(frame(&diagnostic_request("before", &uri)));
    input.extend(frame(&change_request(
        "textDocument/didChange",
        &uri,
        "use crate::missing::Thing;\npub fn present() {}\n",
    )));
    input.extend(frame(&diagnostic_request("after", &uri)));
    input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

    let mut output = Vec::new();
    serve_lsp(root, Cursor::new(input), &mut output).unwrap();
    let frames = frames(&output);

    let before = reply(&frames, "before").to_string();
    assert!(
        !before.contains("unresolved-import"),
        "the open buffer starts clean: {before}"
    );
    let after = reply(&frames, "after").to_string();
    assert!(
        after.contains("unresolved-import"),
        "the edit must be what moved it: {after}"
    );
}

/// Closing reverts the *record*, not just the symbol answer: a diagnostic the
/// buffer introduced is gone once the file is back.
#[test]
fn a_close_puts_the_file_back_for_diagnostics_too() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub fn present() {}\n");
    let uri = format!("file://{}", root.join("src/lib.rs").display());

    let mut input = frame(r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}"#);
    input.extend(frame(&change_request(
        "textDocument/didOpen",
        &uri,
        "use crate::missing::Thing;\npub fn present() {}\n",
    )));
    input.extend(frame(&diagnostic_request("open", &uri)));
    input.extend(frame(&close_request(&uri)));
    input.extend(frame(&diagnostic_request("closed", &uri)));
    input.extend(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#));

    let mut output = Vec::new();
    serve_lsp(root, Cursor::new(input), &mut output).unwrap();
    let frames = frames(&output);

    let open = reply(&frames, "open").to_string();
    assert!(
        open.contains("unresolved-import"),
        "the buffer is the record: {open}"
    );
    let closed = reply(&frames, "closed").to_string();
    assert!(
        !closed.contains("unresolved-import"),
        "the file on disk has no such import: {closed}"
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

    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas.check().is_empty(),
        "nothing is wrong, so nothing is reported: {:?}",
        atlas.check()
    );

    // The roster is there for whoever asks.
    let roster = Atlas::capabilities();
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
    let roster = Atlas::capabilities();
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
            "c#",
            "c++",
            "go",
            "java",
            "javascript",
            "kotlin",
            "php",
            "python",
            "ruby",
            "rust",
            "swift",
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
            && text.contains(r#""level":"Full""#)
            && text.contains(r#""language":"rust""#)
            && text.contains(r#""language":"typescript""#),
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

/// `pub type X = path::X;` re-publishes a type under its own name; it is not
/// a second definition. `theme/symbols.rs` does this for `SymbolPreset`, which
/// `titi atlas check` reported as ambiguous with the file that defines it.
#[test]
fn a_same_name_type_alias_is_not_a_second_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/data.rs", "pub enum Preset { A }\n");
    write(
        root,
        "src/symbols.rs",
        "pub type Preset = crate::data::Preset;\npub type Alias = crate::data::Preset;\n",
    );
    let atlas = Atlas::index(root).unwrap();
    assert_eq!(
        atlas.symbols["Preset"].files,
        vec!["src/data.rs".to_owned()]
    );
    assert!(
        atlas.files["src/symbols.rs"]
            .exports
            .contains(&"Alias".to_owned()),
        "a rename is a definition of the new name"
    );
    assert!(
        atlas
            .check()
            .iter()
            .all(|item| item.code != "ambiguous-symbol"),
        "{:?}",
        atlas.check()
    );
}

/// Every diagnostic used to say `:1:`, so the person (or model) had to search
/// the file for the thing it named.
#[test]
fn a_diagnostic_points_at_the_line_it_is_about() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "// header\npub fn present() {}\nuse crate::missing::Thing;\n",
    );
    write(
        root,
        "src/broken.rs",
        "pub fn fine() {}\n\npub fn broken( {\n",
    );
    let atlas = Atlas::index(root).unwrap();
    let diagnostics = atlas.check();

    let import = diagnostics
        .iter()
        .find(|item| item.code == "unresolved-import")
        .unwrap();
    assert_eq!((import.path.as_str(), import.line), ("src/lib.rs", 3));
    let syntax = diagnostics
        .iter()
        .find(|item| item.code == "syntax-error")
        .unwrap();
    assert_eq!(syntax.line, 3, "{syntax:?}");
}

/// A cap that hides the rest makes a count a lower bound. The report says how
/// many it left out, and the diagnostics themselves are all there.
#[test]
fn check_keeps_every_diagnostic_and_the_report_says_how_many_it_cut() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for n in 0..60 {
        write(
            root,
            &format!("src/f{n}.rs"),
            &format!("use crate::gone{n}::Thing;\n"),
        );
    }
    let atlas = Atlas::index(root).unwrap();
    let diagnostics = atlas.check();
    assert_eq!(diagnostics.len(), 60);

    let lines = titi_atlas::format_check_report(&diagnostics, 50);
    assert_eq!(lines.len(), 51, "{lines:?}");
    assert!(
        lines[50].contains("10 more")
            && lines[50].contains("60")
            && lines[50].contains("unresolved-import"),
        "{}",
        lines[50]
    );
    assert_eq!(
        titi_atlas::format_check_report(&diagnostics[..3], 50).len(),
        3
    );
}

/// `export { Button } from './button'` lists `Button` among the barrel's
/// exports; the barrel is the road to it, not a second definition.
#[test]
fn a_barrel_that_re_exports_a_name_does_not_make_it_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/ui/button.ts", "export const Button = 1;\n");
    write(
        root,
        "src/ui/index.ts",
        "export { Button } from './button';\n",
    );
    let atlas = Atlas::index(root).unwrap();
    assert!(
        atlas
            .check()
            .iter()
            .all(|item| item.code != "ambiguous-symbol"),
        "{:?}",
        atlas.check()
    );
}
