//! P4-4: structural edits resolve through the grammar, or are refused.
//!
//! Spec: `docs/PLAN.md` (P4-4).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use titi_genome::ast_edit::{AstEditError, ItemKind, Part, Target, apply, find};

#[test]
fn rust_function_body_replacement_touches_only_that_span() {
    let source = "\
fn keep() -> u8 {
    1
}

fn target() -> u8 {
    2
}

fn also_keep() -> u8 {
    3
}
";
    let edited = apply(
        "src/lib.rs",
        source,
        &Target::function_body("target"),
        "{\n    42\n}",
    );

    assert_eq!(
        edited.as_deref(),
        Ok("\
fn keep() -> u8 {
    1
}

fn target() -> u8 {
    42
}

fn also_keep() -> u8 {
    3
}
")
    );
}

#[test]
fn typescript_class_body_is_replaced_in_place() {
    let source = "\
export class Client {
    send(): void {}
}

export function build(): Client {
    return new Client();
}
";
    let edited = apply(
        "src/client.ts",
        source,
        &Target::type_body("Client"),
        "{\n    send(): void {}\n    close(): void {}\n}",
    );

    assert_eq!(
        edited.as_deref(),
        Ok("\
export class Client {
    send(): void {}
    close(): void {}
}

export function build(): Client {
    return new Client();
}
")
    );
}

#[test]
fn python_function_is_replaced_whole() {
    let source = "\
def keep():
    return 1


def greet(name):
    return name
";
    let edited = apply(
        "app/main.py",
        source,
        &Target::function("greet"),
        "def greet(name, punct=\"!\"):\n    return name + punct",
    );

    assert_eq!(
        edited.as_deref(),
        Ok("\
def keep():
    return 1


def greet(name, punct=\"!\"):
    return name + punct
")
    );
}

#[test]
fn two_functions_with_one_name_are_refused() {
    let source = "\
struct A;
struct B;

impl A {
    fn run(&self) -> u8 {
        1
    }
}

impl B {
    fn run(&self) -> u8 {
        2
    }
}
";
    let refused = apply(
        "src/run.rs",
        source,
        &Target::function_body("run"),
        "{\n    0\n}",
    );

    assert_eq!(
        refused,
        Err(AstEditError::Ambiguous {
            path: "src/run.rs".to_owned(),
            kind: "function",
            name: "run".to_owned(),
            lines: vec![5, 11],
        })
    );
}

#[test]
fn a_missing_target_is_refused() {
    let source = "fn present() {}\n";

    assert_eq!(
        apply(
            "src/lib.rs",
            source,
            &Target::function("absent"),
            "fn x() {}"
        ),
        Err(AstEditError::NotFound {
            path: "src/lib.rs".to_owned(),
            kind: "function",
            name: "absent".to_owned(),
        })
    );
}

#[test]
fn a_name_that_only_occurs_in_a_comment_or_string_is_not_a_target() {
    let source = "\
// fn ghost() -> u8 { 0 }
/// Doc: `fn ghost()` is not real.
const SNIPPET: &str = \"fn ghost() -> u8 { 0 }\";

fn real() -> u8 {
    0
}
";

    assert_eq!(
        apply(
            "src/lib.rs",
            source,
            &Target::function_body("ghost"),
            "{\n    1\n}"
        ),
        Err(AstEditError::NotFound {
            path: "src/lib.rs".to_owned(),
            kind: "function",
            name: "ghost".to_owned(),
        })
    );
    assert!(find("src/lib.rs", source, &Target::function_body("real")).is_ok());
}

#[test]
fn a_python_class_named_in_a_docstring_is_not_a_target() {
    let source = "\
\"\"\"Module docstring mentioning class Ghost: pass.\"\"\"
# class Ghost: pass


class Real:
    pass
";

    assert_eq!(
        apply(
            "app/m.py",
            source,
            &Target::type_item("Ghost"),
            "class X:\n    pass"
        ),
        Err(AstEditError::NotFound {
            path: "app/m.py".to_owned(),
            kind: "type",
            name: "Ghost".to_owned(),
        })
    );
}

#[test]
fn a_source_that_does_not_parse_is_never_edited() {
    let source = "fn broken( -> u8 {\n";

    assert_eq!(
        apply(
            "src/lib.rs",
            source,
            &Target::function_body("broken"),
            "{\n    0\n}"
        ),
        Err(AstEditError::Unparsable {
            path: "src/lib.rs".to_owned(),
        })
    );
}

#[test]
fn a_replacement_that_breaks_the_file_is_refused() {
    let source = "fn run() -> u8 {\n    1\n}\n";

    assert_eq!(
        apply("src/lib.rs", source, &Target::function_body("run"), "{ 1 "),
        Err(AstEditError::BrokenResult {
            path: "src/lib.rs".to_owned(),
        })
    );
}

#[test]
fn an_unknown_language_is_refused_before_anything_else() {
    assert_eq!(
        apply("README.md", "# fn run\n", &Target::function("run"), "x"),
        Err(AstEditError::UnsupportedLanguage {
            path: "README.md".to_owned(),
        })
    );
}

#[test]
fn a_declaration_without_a_body_reports_that() {
    let source = "\
trait Run {
    fn run(&self) -> u8;
}
";

    assert_eq!(
        apply("src/lib.rs", source, &Target::function_body("run"), "{ 0 }"),
        Err(AstEditError::NoBody {
            path: "src/lib.rs".to_owned(),
            kind: "function",
            name: "run".to_owned(),
        })
    );
}

#[test]
fn find_reports_the_span_without_editing() {
    let source = "\
export function build(): number {
    return 1;
}
";
    let span = find(
        "src/build.js",
        source,
        &Target {
            kind: ItemKind::Function,
            name: "build".to_owned(),
            part: Part::Whole,
        },
    )
    .expect("build is declared once");

    assert_eq!(span.start_line, 1);
    assert_eq!(span.end_line, 3);
    // `export` is a wrapper around the declaration, not part of it: replacing
    // the function leaves the file exporting whatever replaced it.
    assert_eq!(
        &source[span.start..span.end],
        "function build(): number {\n    return 1;\n}"
    );
}
