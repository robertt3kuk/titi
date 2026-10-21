#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! What an edit did to a file's syntax: errors introduced, fixed, or neither.

use titi_genome::syntax_check::{MAX_BYTES, SyntaxDelta, delta};

const CLEAN_RS: &str = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
const EXTRA_BRACE_RS: &str = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n}\n";
const MISSING_BRACE_RS: &str = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n";

#[test]
fn a_stray_brace_in_rust_is_introduced_and_named() {
    let found = delta("src/main.rs", CLEAN_RS, EXTRA_BRACE_RS).unwrap();
    match &found {
        SyntaxDelta::Introduced { count, first } => {
            assert!(*count >= 1, "{found:?}");
            let first = first.as_ref().unwrap();
            assert_eq!(first.line, 5, "{found:?}");
            assert!(first.message.contains('}'), "{found:?}");
        }
        other => panic!("expected introduced errors, got {other:?}"),
    }
    let line = found.to_string();
    assert!(
        line.starts_with("syntax: this edit introduced ")
            && line.contains("(line 5: unexpected '}')"),
        "{line}"
    );
}

#[test]
fn a_missing_closing_brace_in_rust_is_introduced() {
    let found = delta("src/main.rs", CLEAN_RS, MISSING_BRACE_RS).unwrap();
    assert!(
        matches!(found, SyntaxDelta::Introduced { count, .. } if count >= 1),
        "{found:?}"
    );
}

#[test]
fn repairing_a_broken_rust_file_reports_the_fix() {
    let found = delta("src/main.rs", EXTRA_BRACE_RS, CLEAN_RS).unwrap();
    assert!(
        matches!(found, SyntaxDelta::Fixed { count } if count >= 1),
        "{found:?}"
    );
    let line = found.to_string();
    assert!(line.starts_with("syntax: fixed "), "{line}");
}

#[test]
fn a_count_of_one_is_singular() {
    let line = SyntaxDelta::Fixed { count: 1 }.to_string();
    assert_eq!(line, "syntax: fixed 1 error");
    let line = SyntaxDelta::Fixed { count: 2 }.to_string();
    assert_eq!(line, "syntax: fixed 2 errors");
}

#[test]
fn an_edit_that_leaves_the_error_count_alone_says_nothing() {
    assert_eq!(delta("src/main.rs", CLEAN_RS, CLEAN_RS), None);
    let reworded = EXTRA_BRACE_RS.replace("let x = 1", "let x = 2");
    assert_eq!(delta("src/main.rs", EXTRA_BRACE_RS, &reworded), None);
}

#[test]
fn an_error_that_was_already_there_is_not_blamed_on_the_edit() {
    // Two stray braces after one: the first is the old one, the new one is
    // the one the report must point at.
    let one = "fn a() {}\n}\nfn b() {}\n";
    let two = "fn a() {}\n}\nfn b() {}\n}\n";
    let found = delta("lib.rs", one, two).unwrap();
    match found {
        SyntaxDelta::Introduced { first, .. } => {
            assert_eq!(first.unwrap().line, 4);
        }
        other => panic!("expected introduced errors, got {other:?}"),
    }
}

#[test]
fn typescript_introduced_fixed_and_unchanged() {
    let clean = "export function f(): number {\n  return 1;\n}\n";
    let broken = "export function f(): number {\n  return 1;\n";
    let found = delta("src/f.ts", clean, broken).unwrap();
    assert!(
        matches!(found, SyntaxDelta::Introduced { count, .. } if count >= 1),
        "{found:?}"
    );
    let fixed = delta("src/f.ts", broken, clean).unwrap();
    assert!(
        matches!(fixed, SyntaxDelta::Fixed { count } if count >= 1),
        "{fixed:?}"
    );
    assert_eq!(delta("src/f.ts", clean, clean), None);
    // TSX takes the same path, with its own grammar.
    let tsx = delta(
        "src/App.tsx",
        "export const A = () => <div/>;\n",
        "export const A = () => <div>;\n",
    )
    .unwrap();
    assert!(matches!(tsx, SyntaxDelta::Introduced { .. }), "{tsx:?}");
}

#[test]
fn a_language_without_a_grammar_is_skipped() {
    assert_eq!(delta("notes.txt", "fine", "}}}{{{"), None);
    assert_eq!(delta("Cargo.toml", "[a]\n", "[a\n"), None);
    assert_eq!(delta("no_extension", "fine", "}}}{{{"), None);
}

#[test]
fn a_file_past_the_size_budget_is_skipped_on_either_side() {
    let big = format!("{CLEAN_RS}// {}\n", "x".repeat(MAX_BYTES));
    assert_eq!(delta("big.rs", &big, EXTRA_BRACE_RS), None);
    assert_eq!(delta("big.rs", CLEAN_RS, &format!("{big}}}\n")), None);
}

#[test]
fn a_normal_file_is_checked_quickly() {
    let mut body = String::new();
    for index in 0..2000 {
        body.push_str(&format!(
            "pub fn item_{index}(value: u32) -> u32 {{\n    value + {index}\n}}\n\n"
        ));
    }
    let broken = format!("{body}}}\n");
    let started = std::time::Instant::now();
    let found = delta("src/big.rs", &body, &broken).unwrap();
    let took = started.elapsed();
    assert!(matches!(found, SyntaxDelta::Introduced { .. }), "{found:?}");
    assert!(
        took < std::time::Duration::from_millis(500),
        "a 2000-function file took {took:?}"
    );
}
