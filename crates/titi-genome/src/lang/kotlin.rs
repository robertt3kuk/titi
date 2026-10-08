//! Kotlin: declarations and `import` lines, read off the grammar's nodes.
//!
//! A commented-out `class` is not a class and a declaration written inside a
//! function body is not an export: the grammar knows the difference, so only
//! file-scope and class-member declarations are read. A function exports the
//! name the grammar recorded, so an extension function `fun String.toSlug()`
//! exports `toSlug`, not the receiver, and a generic `fun <T> first(...)`
//! exports `first`. A `val`/`var` is read from its `variable_declaration`, so
//! an inferred top-level `val hit = 1` is an export where the pattern — which
//! needed the explicit `: Type` to tell a property from a local — missed it.
//!
//! `import a.b.C` resolves by suffix over `.kt`/`.kts`. A specifier whose
//! root segment differs from the file's own `package` root is another world —
//! the JDK, a dependency — so `import java.util.List` is not a missing file.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, is_workspace_spec, record, resolve_suffix, root_segment};

use crate::symbols::{self, Grammar, push_field, push_site, text};

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Kotlin, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    kotlin_items(tree.root_node(), bytes, &mut sites);

    let own = package_name(tree.root_node(), bytes);
    let own = own.as_deref().map(root_segment);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    kotlin_imports(
        tree.root_node(),
        bytes,
        own,
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

/// Declarations reachable from outside their body: a `class`/`interface`/
/// `object`, a `fun`, a `val`/`var` and a `typealias`. A function body is not
/// walked — a `val` or a `fun` inside one is a local, not a name a caller can
/// reach, and the pattern could not tell the two apart.
fn kotlin_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" | "object_declaration" | "function_declaration" => {
                push_field(child, source, out);
            }
            "property_declaration" => {
                if let Some(name) = property_name(child) {
                    push_site(name, source, out);
                }
            }
            // `typealias Slug = String`: the grammar stores the alias's own
            // name under the `type` field.
            "type_alias" => {
                if let Some(name) = child.child_by_field_name("type") {
                    push_site(name, source, out);
                }
            }
            _ => {}
        }
        if child.kind() != "function_body" {
            kotlin_items(child, source, out);
        }
    }
}

/// The name of a `val`/`var`, which the grammar keeps on the declaration's
/// `variable_declaration` rather than behind a `name` field.
fn property_name(declaration: Node) -> Option<Node> {
    let mut cursor = declaration.walk();
    let variable = declaration
        .named_children(&mut cursor)
        .find(|child| child.kind() == "variable_declaration")?;
    let mut names = variable.walk();
    variable
        .named_children(&mut names)
        .find(|child| child.kind() == "identifier")
}

/// The file's own `package a.b`, whose first segment decides what "workspace
/// shaped" means for its imports.
fn package_name(root: Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let header = root
        .named_children(&mut cursor)
        .find(|child| child.kind() == "package_header")?;
    let mut names = header.walk();
    let name = header
        .named_children(&mut names)
        .find(|child| matches!(child.kind(), "qualified_identifier" | "identifier"))?;
    text(name, source)
}

/// Every `import` at the top of the file: the dotted name the import names,
/// without its `as` alias. A commented-out import is not a node.
fn kotlin_imports(
    root: Node,
    source: &[u8],
    own: Option<&str>,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    let mut cursor = root.walk();
    for import in root.named_children(&mut cursor) {
        if import.kind() != "import" {
            continue;
        }
        let mut names = import.walk();
        let Some(name) = import
            .named_children(&mut names)
            .find(|child| matches!(child.kind(), "qualified_identifier" | "identifier"))
        else {
            continue;
        };
        let Some(spec) = text(name, source) else {
            continue;
        };
        let resolved = resolve_suffix(&spec.replace('.', "/"), &["kt", "kts"], files);
        let placement = match resolved {
            Some(path) => Placement::Resolved(path),
            // A specifier under another root is a library, not a missing file.
            None if is_workspace_spec(root_segment(&spec), own) => Placement::Missing,
            None => Placement::External,
        };
        record(&spec, placement, imports, unresolved);
    }
}
