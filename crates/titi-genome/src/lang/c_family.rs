//! C and C++: declarations and quoted `#include` paths, read off the grammar.
//!
//! Both rows of the language table point here: the two share a declaration
//! shape and a header convention, and the quoted include is the only include
//! this crate resolves — a `<stdio.h>` is a system header, not a file of the
//! workspace, so it carries no diagnostic. Every name is a node the grammar
//! produced, so a declaration inside a comment is not one, and a `void
//! Engine::start()` definition exports its last `::` segment.
//!
//! One language cannot be told from the other by extension: a `.h` is claimed
//! by the C row, but a C++ header lives in one too. The C grammar turns `class
//! Engine { … }` into a class-less function with `ERROR` nodes where the
//! members are, so a `.h` whose C tree is broken is read with the C++ grammar
//! — a superset that parses C as well — and that tree is kept only when it is
//! cleaner than the C one. `.cpp`/`.cc`/`.hpp` and the rest go straight to the
//! C++ grammar.
//!
//! A declaration exports the last segment of its name, so an out-of-class C++
//! definition `void Engine::start()` exports `start`; a declaration whose
//! modifiers include `static` is file-private in both languages and is not
//! exported at all. A name with no tag (`typedef int Foo;`) is not a tag the
//! grammar records, so it is not an export — the same gap the patterns had.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use super::ParsedFile;
use super::support::{finish, internal, normalize_join, parent, record};

use crate::symbols::{self, Grammar, push_site, text};
use crate::{Candidate, UnresolvedImport};

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(grammar) = super::grammar_for(path) else {
        return ParsedFile::default();
    };
    let Some(tree) = tree_for(grammar, path, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    c_items(tree.root_node(), bytes, &mut sites);

    let from_dir = parent(path).unwrap_or("");
    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    c_includes(
        tree.root_node(),
        bytes,
        from_dir,
        files,
        &mut imports,
        &mut unresolved,
    );
    finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    )
}

/// The tree to read the file from. A `.h` is claimed by C, but the C grammar
/// cannot read a C++ header: `class Engine { … }` becomes a class-less
/// function with the members inside `ERROR` nodes. When the C tree is broken,
/// the C++ grammar — a superset that also parses C — gets a second look, and
/// it is kept only when it is cleaner than the C one.
fn tree_for(grammar: Grammar, path: &str, source: &str) -> Option<Tree> {
    let tree = symbols::parse(grammar, source)?;
    if grammar != Grammar::C || !path.ends_with(".h") || !tree.root_node().has_error() {
        return Some(tree);
    }
    let Some(cpp) = symbols::parse(Grammar::Cpp, source) else {
        return Some(tree);
    };
    if symbols::error_count(cpp.root_node()) < symbols::error_count(tree.root_node()) {
        Some(cpp)
    } else {
        Some(tree)
    }
}

/// Every declaration a caller can name: a `struct`/`union`/`enum`/`class` tag
/// and a function (declaration, definition or class member). Nesting is
/// included, because a header's class body and a function's local declarations
/// are reached through the same walk — the patterns were line-anchored and
/// read those too.
fn c_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier" => {
                if !enclosing_static(child, source)
                    && let Some(name) = child.child_by_field_name("name")
                {
                    push_site(name, source, out);
                }
            }
            "function_definition" | "declaration" | "field_declaration" => {
                c_function(child, source, out);
            }
            _ => {}
        }
        c_items(child, source, out);
    }
}

/// Adds the name of a declaration whose declarator calls something: `int
/// hash(const char *key);` and `void Engine::start() { }` alike, but not `int
/// x = 1;` and not a `static` declaration, which is file-private.
fn c_function(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    if is_static(node, source) {
        return;
    }
    // `int first(void), second(void);` is one declaration with two declarators
    // under the same field, and the patterns only ever saw the first.
    let mut cursor = node.walk();
    for declarator in node.children_by_field_name("declarator", &mut cursor) {
        if !calls_something(declarator) {
            continue;
        }
        if let Some(name) = declarator_name(declarator) {
            push_site(name, source, out);
        }
    }
}

/// Whether a declarator chain goes through a function declarator — the whole
/// difference between a function and a variable declaration.
fn calls_something(node: Node) -> bool {
    if matches!(
        node.kind(),
        "function_declarator" | "abstract_function_declarator"
    ) {
        return true;
    }
    node.child_by_field_name("declarator")
        .is_some_and(calls_something)
}

/// The name at the bottom of a declarator chain. A qualified name keeps its
/// last segment, so `Engine::start` exports `start`, and the site points at
/// that segment rather than at the class.
fn declarator_name(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "operator_name"
        | "destructor_name" => Some(node),
        "qualified_identifier" | "template_function" | "template_method" | "template_type" => {
            node.child_by_field_name("name").and_then(declarator_name)
        }
        _ => node
            .child_by_field_name("declarator")
            .and_then(declarator_name),
    }
}

/// A declaration whose modifiers include `static` is file-private, exactly as
/// the pattern recognised: the keyword is a `storage_class_specifier` child.
fn is_static(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| {
        child.kind() == "storage_class_specifier"
            && text(child, source).as_deref() == Some("static")
    })
}

/// Whether a tag sits inside a `static` declaration: `static struct X { … };`
/// declares a file-private tag, so it is not an export.
fn enclosing_static(node: Node, source: &[u8]) -> bool {
    is_static(node, source)
        || node
            .parent()
            .is_some_and(|parent| is_static(parent, source))
}

/// Every quoted `#include` in the file. An angle-bracket include names a
/// system header and is not this workspace's business, so it is skipped; a
/// comment cannot carry one, because it never becomes a node.
fn c_includes(
    node: Node,
    source: &[u8],
    from_dir: &str,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    if node.kind() == "preproc_include" {
        if let Some(path) = node.child_by_field_name("path")
            && path.kind() == "string_literal"
            && let Some(raw) = text(path, source)
        {
            let spec = raw.trim_matches('"');
            let joined = if from_dir.is_empty() {
                spec.to_owned()
            } else {
                format!("{from_dir}/{spec}")
            };
            let candidates = normalize_join("", &joined)
                .map(|joined| vec![Candidate::Exact(joined)])
                .unwrap_or_default();
            record(spec, internal(candidates), files, imports, unresolved);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        c_includes(child, source, from_dir, files, imports, unresolved);
    }
}
