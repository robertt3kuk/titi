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
use super::support::{
    Placement, finish, is_workspace_spec, record, resolve_suffix, root_segment, symbols_from_sites,
};

use crate::symbols::{self, Grammar, push_field, text};
use crate::{SymbolKind, UnresolvedImport};

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
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    php_imports(
        tree.root_node(),
        bytes,
        own,
        files,
        &mut imports,
        &mut unresolved,
    );
    let mut parsed = finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    );
    parsed.symbols = symbols_from_sites(tree.root_node(), bytes, &parsed.export_sites, &php_kind);
    parsed
}

/// The kind of a PHP declaration node, for the symbols list.
fn php_kind(node: Node, _name: Node) -> Option<SymbolKind> {
    Some(match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "interface_declaration" => SymbolKind::Interface,
        "trait_declaration" => SymbolKind::Trait,
        "enum_declaration" => SymbolKind::Enum,
        "function_definition" => SymbolKind::Function,
        "method_declaration" => SymbolKind::Method,
        _ => return None,
    })
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
    unresolved: &mut Vec<UnresolvedImport>,
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
    unresolved: &mut Vec<UnresolvedImport>,
) {
    let candidates = resolve_suffix(&spec.replace('\\', "/"), &["php"]);
    // A specifier under another root is a library, not a missing file; a miss
    // of this workspace's own shape is.
    let placement = if is_workspace_spec(root_segment(spec), own) {
        Placement::Candidates(candidates)
    } else {
        Placement::Optional(candidates)
    };
    record(spec, placement, files, imports, unresolved);
}

#[cfg(test)]
mod tests {
    use crate::SymbolKind;
    use crate::lang::test_support::symbol;

    const SRC: &str = r#"<?php
namespace App;

/** Docs. */
final class Client extends Base implements Contract
{
    public function send(string $body, int $retries = 3): void
    {
    }
}

interface Contract {}

trait Loud {}

function helper(array $items): int
{
    return 1;
}
"#;

    #[test]
    fn php_symbols_carry_kind_span_and_signature() {
        let client = symbol("src/Client.php", SRC, "Client");
        assert_eq!((client.kind, client.span), (SymbolKind::Class, (5, 10)));
        assert_eq!(
            client.signature.as_deref(),
            Some("final class Client extends Base implements Contract")
        );
        let send = symbol("src/Client.php", SRC, "send");
        assert_eq!((send.kind, send.span), (SymbolKind::Method, (7, 9)));
        assert_eq!(
            send.signature.as_deref(),
            Some("public function send(string $body, int $retries = 3): void")
        );
        assert_eq!(
            symbol("src/Client.php", SRC, "Contract").kind,
            SymbolKind::Interface
        );
        assert_eq!(
            symbol("src/Client.php", SRC, "Loud").kind,
            SymbolKind::Trait
        );
        let helper = symbol("src/Client.php", SRC, "helper");
        assert_eq!((helper.kind, helper.span), (SymbolKind::Function, (16, 19)));
        assert_eq!(
            helper.signature.as_deref(),
            Some("function helper(array $items): int")
        );
    }

    #[test]
    fn a_long_php_signature_is_cut_to_the_limit() {
        let params: Vec<String> = (0..30).map(|n| format!("int $argument{n}")).collect();
        let source = format!("<?php\nfunction long({}) {{}}\n", params.join(", "));
        let signature = symbol("a.php", &source, "long")
            .signature
            .expect("a signature");
        assert_eq!(signature.chars().count(), crate::MAX_SIGNATURE);
        assert!(signature.ends_with('…'), "{signature}");
    }
}
