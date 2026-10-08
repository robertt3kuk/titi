//! C#: type declarations and `using` lines, read off the grammar's nodes.
//!
//! A `using A.B.C;` names a namespace, not a file: resolution is a suffix
//! match against the workspace, which lands on the type file when the project
//! lays its directories out the way the namespace says. The declarations are
//! the grammar's own nodes, so a class written inside a verbatim string is not
//! an export, though the line patterns read one. Only a type is exported —
//! `class`, `interface`, `record`, `enum`, `struct` — as the row always had
//! it, so a method is not.
//!
//! `using X = A.B;` imports `A.B` under an alias, `using static A.B.C;` and
//! `global using A.B;` are the same namespace import, and all three land here.
//! An import that resolves to nothing is only a missing file when it shares
//! the file's own `namespace` root: `System`, a NuGet package's namespace is
//! not this workspace's, so it gets no diagnostic.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, is_workspace_spec, record, resolve_suffix, root_segment};

use crate::symbols::{self, Grammar, push_field, text};

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::CSharp, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    csharp_items(tree.root_node(), bytes, &mut sites);

    let own = namespace_name(tree.root_node(), bytes);
    let own = own.as_deref().map(root_segment);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    csharp_usings(
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

/// Types a file publishes — `class`, `interface`, `record`, `enum`, `struct`,
/// at any nesting. A method or a field is not one; neither is a declaration
/// that only looks like one inside a string.
fn csharp_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "struct_declaration"
        ) {
            push_field(child, source, out);
        }
        csharp_items(child, source, out);
    }
}

/// The file's own `namespace`, in both the block and the file-scoped form,
/// whose first segment decides what "workspace shaped" means for its imports.
fn namespace_name(root: Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let declaration = root.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "namespace_declaration" | "file_scoped_namespace_declaration"
        )
    })?;
    declaration
        .child_by_field_name("name")
        .and_then(|name| text(name, source))
}

/// Every `using` directive, wherever it sits. A commented-out one is not a
/// node, so it cannot reach here.
fn csharp_usings(
    node: Node,
    source: &[u8],
    own: Option<&str>,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    if node.kind() == "using_directive" {
        if let Some(spec) = using_spec(node, source) {
            let resolved = resolve_suffix(&spec.replace('.', "/"), &["cs"], files);
            let placement = match resolved {
                Some(path) => Placement::Resolved(path),
                None if is_workspace_spec(root_segment(&spec), own) => Placement::Missing,
                None => Placement::External,
            };
            record(&spec, placement, imports, unresolved);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        csharp_usings(child, source, own, files, imports, unresolved);
    }
}

/// The namespace a `using` names. `using X = A.B;` stores the alias in the
/// `name` field, so the qualified path is preferred over a bare identifier.
fn using_spec(node: Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    if let Some(qualified) = children
        .iter()
        .find(|child| child.kind() == "qualified_name")
    {
        return text(*qualified, source);
    }
    children
        .iter()
        .find(|child| child.kind() == "identifier")
        .and_then(|child| text(*child, source))
}
