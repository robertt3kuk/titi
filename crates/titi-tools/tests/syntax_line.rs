#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The line a write, an edit or a hashline edit appends when it changed how
//! many syntax errors the file has — and the silence when it did not.

use serde_json::{Value, json};
use titi_tools::hashline::line_anchor;
use titi_tools::{ToolHandler, workspace_tools};

fn tool(root: &std::path::Path, name: &str) -> Box<dyn ToolHandler> {
    workspace_tools(root)
        .into_iter()
        .find(|tool| tool.definition().spec.name == name)
        .unwrap()
}

async fn run(root: &std::path::Path, name: &str, args: Value) -> (String, bool) {
    let result = tool(root, name).invoke(args).await;
    (result.output.to_string(), result.is_error)
}

const CLEAN: &str = "fn main() {\n    let x = 1;\n}\n";
const BROKEN: &str = "fn main() {\n    let x = 1;\n}\n}\n";

#[tokio::test]
async fn a_write_that_breaks_a_file_says_so_on_the_last_line() {
    let dir = tempfile::tempdir().unwrap();
    let (output, is_error) = run(
        dir.path(),
        "write",
        json!({ "path": "a.rs", "content": BROKEN }),
    )
    .await;
    assert!(!is_error, "{output}");
    let last = output.lines().last().unwrap();
    assert!(
        last.starts_with("syntax: this edit introduced ") && last.contains("line 4"),
        "{output}"
    );
    assert!(output.starts_with("wrote "), "{output}");
}

#[tokio::test]
async fn a_clean_write_and_a_clean_edit_say_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (output, _) = run(
        dir.path(),
        "write",
        json!({ "path": "a.rs", "content": CLEAN }),
    )
    .await;
    assert!(!output.contains("syntax:"), "{output}");
    let (output, is_error) = run(
        dir.path(),
        "edit",
        json!({ "path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;" }),
    )
    .await;
    assert!(!is_error);
    assert_eq!(output, "edited");
}

#[tokio::test]
async fn an_edit_that_introduces_then_repairs_an_error_reports_both() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), CLEAN).unwrap();
    let (output, is_error) = run(
        dir.path(),
        "edit",
        json!({ "path": "a.rs", "old_string": "let x = 1;\n}", "new_string": "let x = 1;\n}\n}" }),
    )
    .await;
    assert!(!is_error, "{output}");
    assert!(
        output.starts_with("edited\nsyntax: this edit introduced "),
        "{output}"
    );

    let (output, _) = run(
        dir.path(),
        "edit",
        json!({ "path": "a.rs", "old_string": "}\n}", "new_string": "}" }),
    )
    .await;
    assert_eq!(output, "edited\nsyntax: fixed 1 error", "{output}");
}

#[tokio::test]
async fn a_hashline_edit_is_checked_too() {
    let dir = tempfile::tempdir().unwrap();
    let source = "export function f(): number {\n  return 1;\n}\n";
    std::fs::write(dir.path().join("f.ts"), source).unwrap();
    let anchor = line_anchor("}");
    let (output, is_error) = run(
        dir.path(),
        "hashline_edit",
        json!({ "path": "f.ts", "start_line": 3, "anchors": [anchor], "replacement": "" }),
    )
    .await;
    assert!(!is_error, "{output}");
    assert!(
        output.starts_with("edited f.ts:3-3\nsyntax: this edit introduced "),
        "{output}"
    );
}

#[tokio::test]
async fn a_file_with_no_grammar_is_never_checked() {
    let dir = tempfile::tempdir().unwrap();
    let (output, _) = run(
        dir.path(),
        "write",
        json!({ "path": "notes.txt", "content": "}}}{{{" }),
    )
    .await;
    assert!(!output.contains("syntax:"), "{output}");
}

#[tokio::test]
async fn a_refused_edit_gets_no_syntax_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), BROKEN).unwrap();
    let (output, is_error) = run(
        dir.path(),
        "edit",
        json!({ "path": "a.rs", "old_string": "nowhere", "new_string": "x" }),
    )
    .await;
    assert!(is_error);
    assert!(!output.contains("syntax:"), "{output}");
}
