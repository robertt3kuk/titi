//! PHP: declarations and `use` lines, read off the grammar's nodes.
//!
//! A commented-out `// class Ghost {}` is not a class — and neither is a
//! declaration behind PHP's `#` form, which the pattern's masker did not know
//! about. A `use` inside a class body is a trait `use_declaration`, not a
//! `namespace_use_declaration`, so it is no longer read as a namespace import.
//!
//! A `use` may import a class (`Acme\Util`), a grouped set
//! (`Acme\{Hash, Cache}`), a function (`use function Acme\helper`) or a
//! constant (`use const Acme\LIMIT`), optionally with an `as Alias` tail;
//! each name resolves by suffix over `.php`. A specifier whose root differs
//! from the file's own `namespace` root is another vendor — `Psr\Log\…` — and
//! is not a missing file. A `private` method is not part of the surface, so it
//! is not an export; a method with no visibility modifier is public and is.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, is_workspace_spec, record, resolve_suffix, root_segment};

use crate::symbols::{self, Grammar, push_field, text};

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Php, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    php_items(tree.root_node(), bytes, &mut sites);

    let own = namespace_name(tree.root_node(), bytes);
    let own = own.as_deref().map(root_segment);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    php_imports(
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

/// Declarations a caller can name: a `class`/`interface`/`trait`/`enum`, a
/// top-level `function`, and a method that is not `private`. Nesting is
/// included, so a method inside a class body is reached by the same walk.
fn php_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition" => push_field(child, source, out),
            "method_declaration" if !is_private(child, source) => push_field(child, source, out),
            _ => {}
        }
        php_items(child, source, out);
    }
}

/// A `private` method is not part of the surface. The keyword is the grammar's
/// `visibility_modifier`; a method without one is public.
fn is_private(method: Node, source: &[u8]) -> bool {
    let mut cursor = method.walk();
    method.named_children(&mut cursor).any(|child| {
        child.kind() == "visibility_modifier" && text(child, source).as_deref() == Some("private")
    })
}

/// The file's own `namespace`, whose first segment marks this workspace's
/// vendor root.
fn namespace_name(root: Node, source: &[u8]) -> Option<String> {
    if root.kind() == "namespace_definition" {
        return root
            .child_by_field_name("name")
            .and_then(|name| text(name, source));
    }
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find_map(|child| namespace_name(child, source))
}

/// Every `use` clause, grouped or not: the name the clause imports, without
/// its `as` alias. A grouped clause takes the declaration's own prefix.
fn php_imports(
    node: Node,
    source: &[u8],
    own: Option<&str>,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    if node.kind() == "namespace_use_declaration" {
        let prefix = {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "namespace_name" | "qualified_name"))
                .and_then(|child| text(child, source))
        };
        let mut clauses = Vec::new();
        collect_use_clauses(node, &mut clauses);
        for clause in clauses {
            if let Some(spec) = use_clause_spec(clause, prefix.as_deref(), source) {
                place(&spec, own, files, imports, unresolved);
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        php_imports(child, source, own, files, imports, unresolved);
    }
}

fn collect_use_clauses<'tree>(node: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "namespace_use_clause" {
            out.push(child);
        } else {
            collect_use_clauses(child, out);
        }
    }
}

/// The full name one clause imports: a `qualified_name` names it outright, a
/// bare `name` inside a group takes the declaration's prefix.
fn use_clause_spec(clause: Node, prefix: Option<&str>, source: &[u8]) -> Option<String> {
    let mut cursor = clause.walk();
    if let Some(qualified) = clause
        .named_children(&mut cursor)
        .find(|child| child.kind() == "qualified_name")
    {
        return text(qualified, source);
    }
    let mut names = clause.walk();
    let name = clause
        .named_children(&mut names)
        .find(|child| child.kind() == "name")
        .and_then(|child| text(child, source))?;
    Some(match prefix {
        Some(prefix) => format!("{prefix}\\{name}"),
        None => name,
    })
}

/// Where one `use` specifier lands: a suffix match over `.php`, a specifier
/// under another vendor root is a library rather than a missing file.
fn place(
    spec: &str,
    own: Option<&str>,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    let resolved = resolve_suffix(&spec.replace('\\', "/"), &["php"], files);
    let placement = match resolved {
        Some(path) => Placement::Resolved(path),
        None if is_workspace_spec(root_segment(spec), own) => Placement::Missing,
        None => Placement::External,
    };
    record(spec, placement, imports, unresolved);
}
