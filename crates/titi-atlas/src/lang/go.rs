//! Go: exported identifiers and import paths, read off the grammar's nodes.
//!
//! Go's own rule is what keeps this short: an identifier is exported exactly
//! when it starts with an uppercase letter, so the tree is filtered, not the
//! text. Only package-level declarations count — a `var` inside a function
//! body is not an export — and `func Quoted() {}` written inside a raw string
//! literal is not a declaration at all, which is exactly where the old line
//! patterns and the tree disagree.
//!
//! An import path resolves by its trailing segments over `.go`; when it does
//! not, the first segment decides the honest placement: `example.com/x` is a
//! module path of some kind and names no file, so it is a real
//! `unresolved-import`, while a dotless `fmt`/`os` is a standard-library path
//! and carries no warning at all. An alias (`x "p"`), a blank (`_ "p"`) and a
//! dot (`. "p"`) import all resolve the same way.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, record, resolve_suffix};

use crate::UnresolvedImport;
use crate::symbols::{self, Grammar, push_site, text};

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Go, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    go_decls(tree.root_node(), bytes, &mut sites);
    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    collect_go_imports(
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

/// Package-level declarations a caller can reach: a `func`, a method, and the
/// `type`/`var`/`const` specs of a single-line or grouped declaration. Go
/// exports a name exactly when it starts with an uppercase letter, so a
/// lowercase group member and a local both drop out here.
fn go_decls(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "method_declaration" => {
                if let Some(name) = child.child_by_field_name("name")
                    && is_exported(name, source)
                {
                    push_site(name, source, out);
                }
            }
            "type_declaration" | "var_declaration" | "const_declaration" => {
                go_specs(child, source, out);
            }
            _ => {}
        }
    }
}

/// The specs of one `type`/`var`/`const` declaration, single-line or grouped:
/// the grammar nests a group's specs inside a `<kind>_spec_list`, so both
/// shapes reach the same field.
fn go_specs(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "type_spec" | "type_alias" => {
                if let Some(name) = child.child_by_field_name("name")
                    && is_exported(name, source)
                {
                    push_site(name, source, out);
                }
            }
            "var_spec" | "const_spec" => {
                // `var a, B int` names several identifiers under one field.
                let mut names = child.walk();
                for name in child.children_by_field_name("name", &mut names) {
                    if is_exported(name, source) {
                        push_site(name, source, out);
                    }
                }
            }
            "type_spec_list" | "var_spec_list" | "const_spec_list" => {
                go_specs(child, source, out);
            }
            _ => {}
        }
    }
}

/// Go's own export rule: an identifier is exported exactly when its first
/// character is an uppercase letter.
fn is_exported(name: Node, source: &[u8]) -> bool {
    text(name, source)
        .and_then(|name| name.chars().next())
        .is_some_and(char::is_uppercase)
}

/// Every `import_spec` in the file, wherever the `import` statement nests it.
/// A spec inside a comment or a string is not a node, so it cannot reach here.
fn collect_go_imports(
    node: Node,
    source: &[u8],
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    if node.kind() == "import_spec" {
        if let Some(path) = node.child_by_field_name("path")
            && let Some(raw) = text(path, source)
        {
            place(raw.trim_matches(['"', '`']), files, imports, unresolved);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_go_imports(child, source, files, imports, unresolved);
    }
}

/// Where one import path lands. Go paths are module-qualified; the package
/// directory is the trailing segment, so a suffix match is the honest
/// resolution. A dotted first segment is a module path of some kind and names
/// no file, so its miss is a real diagnostic; a dotless one is a
/// standard-library path and is not this workspace's business.
fn place(
    spec: &str,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    let last = spec.rsplit('/').next().unwrap_or(spec);
    let mut candidates = resolve_suffix(&format!("{last}/{last}"), &["go"]);
    candidates.extend(resolve_suffix(last, &["go"]));
    let module_shaped = spec.split('/').next().unwrap_or(spec).contains('.');
    let placement = if module_shaped {
        Placement::Candidates(candidates)
    } else {
        Placement::Optional(candidates)
    };
    record(spec, placement, files, imports, unresolved);
}
