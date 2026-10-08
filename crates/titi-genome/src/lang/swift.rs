//! Swift: declarations and `import` lines, read off the grammar's nodes.
//!
//! Comments are comments to the grammar, so `// public func ghost()` is not an
//! export — and neither is a declaration written inside a multi-line `"""`
//! string, whose odd quote count confused the old masker. A `class`, `struct`,
//! `enum`, `actor` and `extension` are all the grammar's `class_declaration`;
//! the name is what matters here, not the keyword.
//!
//! A Swift `import` names a module, never a path, so the one honest edge is a
//! workspace module whose own primary file exists under the
//! `MyLib/MyLib.swift` (or `MyLib.swift`) convention; every other module is a
//! dependency or another target and is left alone rather than reported.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, record, resolve_suffix};

use crate::symbols::{self, Grammar, push_field, push_site, text};
use crate::{Candidate, UnresolvedImport};

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Swift, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    swift_items(tree.root_node(), bytes, &mut sites);

    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    swift_imports(
        tree.root_node(),
        bytes,
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

/// Declarations a caller can name: a `func`, a `class`/`struct`/`enum`/
/// `actor`/`extension`, a `protocol`, a `typealias`, and a `public`/`open`
/// property. A property without a visibility modifier — a local inside a
/// function body, most often — is not one, as the pattern had it.
fn swift_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration"
            | "class_declaration"
            | "protocol_declaration"
            | "typealias_declaration" => push_field(child, source, out),
            "property_declaration" if is_public(child, source) => {
                if let Some(name) = property_name(child) {
                    push_site(name, source, out);
                }
            }
            _ => {}
        }
        swift_items(child, source, out);
    }
}

/// Whether a declaration carries `public` or `open`. The keyword is a
/// `visibility_modifier` inside the declaration's `modifiers` node.
fn is_public(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
        .is_some_and(|modifiers| {
            let mut modifiers_cursor = modifiers.walk();
            modifiers
                .named_children(&mut modifiers_cursor)
                .any(|modifier| {
                    modifier.kind() == "visibility_modifier"
                        && matches!(text(modifier, source).as_deref(), Some("public" | "open"))
                })
        })
}

/// A property's name: the `bound_identifier` of its binding pattern, not the
/// pattern node itself.
fn property_name(declaration: Node) -> Option<Node> {
    declaration
        .child_by_field_name("name")?
        .child_by_field_name("bound_identifier")
}

/// Every `import`: the module is the first dotted name after the optional
/// kind keyword, and `import class MyLib.Widget` names `MyLib`.
fn swift_imports(
    node: Node,
    source: &[u8],
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    if node.kind() == "import_declaration" {
        if let Some(module) = first_identifier(node, source) {
            record(
                &module,
                Placement::Optional(module_candidates(&module)),
                files,
                imports,
                unresolved,
            );
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        swift_imports(child, source, files, imports, unresolved);
    }
}

/// The first `simple_identifier` under a node: the module of an import, before
/// any kind keyword or dotted member.
fn first_identifier(node: Node, source: &[u8]) -> Option<String> {
    if node.kind() == "simple_identifier" {
        return text(node, source);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| first_identifier(child, source))
}

/// The module-file convention: the path that names the module is named after
/// it (`MyLib.swift`, or `MyLib/MyLib.swift`). Anything else a suffix match
/// would find is not the module's own file, so it is not a candidate, and an
/// `import` of the module names no file when only those exist.
fn module_candidates(module: &str) -> Vec<Candidate> {
    let sibling = format!("/{module}/{module}.swift");
    let primary = format!("{module}.swift");
    resolve_suffix(module, &["swift"])
        .into_iter()
        .filter(|candidate| match candidate {
            Candidate::Exact(path) | Candidate::Suffix(path) => {
                path.ends_with(&sibling) || path.ends_with(&primary)
            }
        })
        .collect()
}
