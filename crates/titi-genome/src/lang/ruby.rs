//! Ruby: `def`/`class`/`module`/`attr_*` and `require` lines, read off the
//! grammar's nodes.
//!
//! Comments are comments to the grammar, `=begin`/`=end` blocks included, so a
//! commented-out `def` is not an export — the pattern masked `#` and nothing
//! else, so it read one out of an `=begin` block. A method is the grammar's
//! own `method`/`singleton_method` node, so `def self.run` publishes `run`,
//! and an operator method the bare-identifier pattern could not name is
//! exported under the name the grammar gives it.
//!
//! A `require_relative` names a file beside this one, so it is resolved
//! against the workspace and reported when it names nothing. A plain `require`
//! searches the load path — this workspace, a `-I` directory, or a gem — so a
//! specifier that names no workspace file is left alone rather than reported.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{Placement, finish, internal, record, resolve_relative, resolve_suffix};

use crate::symbols::{self, Grammar, push_site, text};

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Ruby, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    ruby_items(
        tree.root_node(),
        bytes,
        path,
        files,
        &mut sites,
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

/// Every declaration and every `require`, from the grammar's nodes: a `def`
/// (singleton included), a `class`/`module`, an `attr_*` call and the two
/// require forms. A commented-out one is not a node, so it cannot reach here.
fn ruby_items(
    node: Node,
    source: &[u8],
    path: &str,
    files: &HashSet<String>,
    sites: &mut Vec<crate::ExportSite>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "method" | "singleton_method" => {
                if let Some(name) = child.child_by_field_name("name") {
                    push_site(name, source, sites);
                }
            }
            "class" | "module" => {
                // `class Acme::App` names `App`: the last `::` segment is the
                // constant, and the site points at that segment.
                if let Some(name) = child.child_by_field_name("name") {
                    push_site(last_segment(name), source, sites);
                }
            }
            "call" => ruby_call(child, source, path, files, sites, imports, unresolved),
            _ => {}
        }
        ruby_items(child, source, path, files, sites, imports, unresolved);
    }
}

/// The constant a `class`/`module` name ends with: `Acme::App` → the `App`
/// node, a bare constant as it is.
fn last_segment(node: Node) -> Node {
    if node.kind() == "scope_resolution"
        && let Some(name) = node.child_by_field_name("name")
    {
        return last_segment(name);
    }
    node
}

/// One call: an `attr_*` publishes its symbol arguments, and the two require
/// forms name a file. Any other call is left alone.
fn ruby_call(
    node: Node,
    source: &[u8],
    path: &str,
    files: &HashSet<String>,
    sites: &mut Vec<crate::ExportSite>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    let Some(method) = node.child_by_field_name("method") else {
        return;
    };
    let Some(name) = text(method, source) else {
        return;
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    match name.as_str() {
        "attr_reader" | "attr_writer" | "attr_accessor" => {
            let mut cursor = arguments.walk();
            for argument in arguments.named_children(&mut cursor) {
                if !matches!(argument.kind(), "simple_symbol" | "symbol") {
                    continue;
                }
                let Some(symbol) = text(argument, source) else {
                    continue;
                };
                let symbol = symbol.trim_start_matches(':').trim_matches(['"', '\'']);
                push_site_at(argument, symbol, sites);
            }
        }
        "require_relative" | "require" => {
            let mut cursor = arguments.walk();
            let Some(argument) = arguments
                .named_children(&mut cursor)
                .find(|argument| argument.kind() == "string")
            else {
                return;
            };
            let Some(spec) = string_content(argument, source) else {
                return;
            };
            if name == "require_relative" {
                let resolved = resolve_relative(path, &spec, files, &["rb"]);
                record(&spec, internal(resolved), imports, unresolved);
            } else {
                let stem = spec.strip_suffix(".rb").unwrap_or(&spec);
                // Nothing found means a gem or a `-I` directory, not a
                // missing file.
                let placement = match resolve_suffix(stem, &["rb"], files) {
                    Some(found) => Placement::Resolved(found),
                    None => Placement::External,
                };
                record(&spec, placement, imports, unresolved);
            }
        }
        _ => {}
    }
}

/// The text inside a `string` node, which is its `string_content` child: the
/// quotes are part of the string node, not of what it names.
fn string_content(string: Node, source: &[u8]) -> Option<String> {
    let mut cursor = string.walk();
    string
        .named_children(&mut cursor)
        .find(|child| child.kind() == "string_content")
        .and_then(|content| text(content, source))
}

/// Records `name` at the position of `at`, rather than at a node that may
/// carry a `:` prefix or quote of its own.
fn push_site_at(at: Node, name: &str, out: &mut Vec<crate::ExportSite>) {
    let point = at.start_position();
    out.push(crate::ExportSite {
        name: name.to_owned(),
        line: u32::try_from(point.row)
            .unwrap_or(u32::MAX)
            .saturating_add(1),
        character: u32::try_from(point.column).unwrap_or(u32::MAX),
    });
}
