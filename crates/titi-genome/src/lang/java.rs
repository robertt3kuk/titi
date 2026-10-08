//! Java: types, public and protected methods, and `import`/`package` lines,
//! read off the grammar's nodes.
//!
//! Java has no header-to-implementation split to exploit: `import a.b.C;` maps
//! straight onto a path, so resolution needs no module index — only the file's
//! extension and a suffix match — while the declarations are the grammar's own
//! nodes, so a commented-out declaration is not one and neither is a method
//! written inside a text block. Every type declaration is an export, as before;
//! a method is one only when its `modifiers` actually name it `public` or
//! `protected`.
//!
//! An import that resolves to nothing is only a missing file when it shares
//! the file's own `package` root: `java.util.List` is the JDK's, not this
//! workspace's, so it is neither an edge nor a diagnostic. `import static
//! a.b.C.member;` names a member of the type, so the whole dotted path is
//! tried first and the path without its trailing segment second. A wildcard
//! `import a.b.*;` names a package, for which no single file stands, so it
//! carries neither an edge nor a warning.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, is_workspace_spec, record, resolve_suffix, root_segment};

use crate::symbols::{self, Grammar, has_child_kind, push_field, text};

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Java, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    java_items(tree.root_node(), bytes, &mut sites);

    let own = package_name(tree.root_node(), bytes);
    let own = own.as_deref().map(root_segment);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    collect_java_imports(
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

/// Types a caller can name — a `class`, `interface`, `enum` or `record`, at
/// any nesting — plus the methods whose modifiers make them visible outside
/// the type. A constructor is a `constructor_declaration`, so it is not here.
fn java_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => push_field(child, source, out),
            "method_declaration" if method_is_visible(child) => push_field(child, source, out),
            _ => {}
        }
        // Nesting is included: a member type or a local class publishes its
        // name the same way a top-level one does.
        java_items(child, source, out);
    }
}

/// Whether a method's own `modifiers` name it `public` or `protected`. The
/// keywords are the grammar's anonymous tokens inside the named `modifiers`
/// node, so the check looks at every child, not the named ones.
fn method_is_visible(method: Node) -> bool {
    let mut cursor = method.walk();
    method
        .named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
        .is_some_and(|modifiers| {
            has_child_kind(modifiers, "public") || has_child_kind(modifiers, "protected")
        })
}

/// The file's own `package`, whose first segment decides what "workspace
/// shaped" means for its imports.
fn package_name(root: Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let declaration = root
        .named_children(&mut cursor)
        .find(|child| child.kind() == "package_declaration")?;
    let mut names = declaration.walk();
    let name = declaration
        .named_children(&mut names)
        .find(|child| matches!(child.kind(), "scoped_identifier" | "identifier"))?;
    text(name, source)
}

/// Every `import_declaration` at the top of the file. One written inside a
/// comment or a text block is not a node, so it cannot reach here.
fn collect_java_imports(
    root: Node,
    source: &[u8],
    own: Option<&str>,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    let mut cursor = root.walk();
    for declaration in root.named_children(&mut cursor) {
        if declaration.kind() == "import_declaration" {
            java_import(declaration, source, own, files, imports, unresolved);
        }
    }
}

/// One `import`: a wildcard names a package and is dropped, a `static` import
/// may name a member of a type and so falls back to the type's own path.
fn java_import(
    node: Node,
    source: &[u8],
    own: Option<&str>,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    if has_child_kind(node, "asterisk") {
        return;
    }
    let mut cursor = node.walk();
    let Some(name) = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "scoped_identifier" | "identifier"))
    else {
        return;
    };
    let Some(spec) = text(name, source) else {
        return;
    };
    let is_static = {
        let mut children = node.walk();
        node.children(&mut children)
            .any(|child| child.kind() == "static")
    };
    let path = spec.replace('.', "/");
    let resolved = resolve_suffix(&path, &["java"], files).or_else(|| {
        // `a.b.C.member`: the type is the import, so drop the trailing member
        // and retry.
        if !is_static {
            return None;
        }
        let type_path = path.rsplit_once('/').map(|(head, _)| head)?;
        resolve_suffix(type_path, &["java"], files)
    });
    let placement = match resolved {
        Some(path) => Placement::Resolved(path),
        None if is_workspace_spec(root_segment(&spec), own) => Placement::Missing,
        None => Placement::External,
    };
    record(&spec, placement, imports, unresolved);
}
