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
use super::support::{
    Placement, finish, inside, is_workspace_spec, record, resolve_suffix, root_segment,
    symbols_from_sites,
};

use crate::symbols::{self, Grammar, push_field, push_site, text};
use crate::{SymbolKind, UnresolvedImport};

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
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    kotlin_imports(
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
    parsed.symbols =
        symbols_from_sites(tree.root_node(), bytes, &parsed.export_sites, &kotlin_kind);
    parsed
}

/// The kind of a Kotlin declaration node, for the symbols list.
fn kotlin_kind(node: Node, _name: Node) -> Option<SymbolKind> {
    Some(match node.kind() {
        "class_declaration" => {
            let mut cursor = node.walk();
            let mut kind = SymbolKind::Class;
            for child in node.children(&mut cursor) {
                if child.kind() == "interface" {
                    kind = SymbolKind::Interface;
                }
                if child.kind() == "modifiers" && has_enum_modifier(child) {
                    kind = SymbolKind::Enum;
                }
            }
            kind
        }
        "object_declaration" => SymbolKind::Class,
        "function_declaration" => {
            if inside(node, &["class_body", "enum_class_body"]) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "property_declaration" => SymbolKind::Const,
        "type_alias" => SymbolKind::Type,
        _ => return None,
    })
}

/// Whether a `modifiers` node holds the `enum` class modifier.
fn has_enum_modifier(modifiers: Node) -> bool {
    let mut cursor = modifiers.walk();
    modifiers.children(&mut cursor).any(|modifier| {
        modifier.kind() == "enum"
            || modifier
                .child(0)
                .is_some_and(|first| first.kind() == "enum")
    })
}

/// Declarations reachable from outside their body: a `class`/`interface`/
/// `object`, a `fun`, a `val`/`var` and a `typealias`. A function body is not
/// walked — a `val` or a `fun` inside one is a local, not a name a caller can
/// reach, and the pattern could not tell the two apart.
fn kotlin_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        // An `actual` implements an `expect` declared elsewhere, which is the
        // definition: exporting both would make one symbol two definers.
        let actual = is_actual(child, source);
        match child.kind() {
            _ if actual => {}
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

/// Whether a declaration carries the multiplatform `actual` modifier.
fn is_actual(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
        .is_some_and(|modifiers| {
            let mut inner = modifiers.walk();
            modifiers.named_children(&mut inner).any(|modifier| {
                modifier.kind() == "platform_modifier"
                    && text(modifier, source).as_deref() == Some("actual")
            })
        })
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
    unresolved: &mut Vec<UnresolvedImport>,
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
        let candidates = resolve_suffix(&spec.replace('.', "/"), &["kt", "kts"]);
        // A specifier under another root is a library, not a missing file.
        let placement = if is_workspace_spec(root_segment(&spec), own) {
            Placement::Candidates(candidates)
        } else {
            Placement::Optional(candidates)
        };
        record(&spec, placement, files, imports, unresolved);
    }
}

#[cfg(test)]
mod tests {
    use crate::lang::test_support::exports;

    fn names(path: &str, source: &str) -> Vec<String> {
        exports(path, source).unwrap_or_else(|| panic!("{path} parses"))
    }

    #[test]
    fn expect_is_the_definition_and_actual_is_not_exported() {
        let common = names(
            "okio/src/commonMain/kotlin/okio/Buffer.kt",
            "expect class Buffer {\n    fun size(): Long\n}\nexpect fun helper()\n",
        );
        assert!(common.contains(&"Buffer".to_owned()), "{common:?}");
        assert!(common.contains(&"helper".to_owned()), "{common:?}");
        let jvm = names(
            "okio/src/jvmMain/kotlin/okio/Buffer.kt",
            "actual class Buffer {\n    actual fun size(): Long = 0\n    fun jvmOnly() {}\n}\nactual fun helper() {}\nactual typealias IOException = java.io.IOException\ninternal actual val flag: Int = 1\nfun plain() {}\n",
        );
        for bad in ["Buffer", "size", "helper", "IOException", "flag"] {
            assert!(!jvm.contains(&bad.to_owned()), "{bad}: {jvm:?}");
        }
        for good in ["jvmOnly", "plain"] {
            assert!(jvm.contains(&good.to_owned()), "{good}: {jvm:?}");
        }
    }

    use crate::SymbolKind;
    use crate::lang::test_support::symbol;

    const SRC: &str = r#"package app

/** Docs. */
@Suppress("x")
class Client<T>(val url: String) : Base() {
    fun send(body: String, retries: Int = 3): Result<Unit> {
        return Result.success(Unit)
    }
}

interface Handler {
    fun handle(event: String)
}

enum class Level { INFO, WARN }

const val LIMIT = 24

fun helper(items: List<String>): Int = items.size

typealias Slug = String
"#;

    #[test]
    fn kotlin_symbols_carry_kind_span_and_signature() {
        let client = symbol("app/a.kt", SRC, "Client");
        assert_eq!((client.kind, client.span), (SymbolKind::Class, (4, 9)));
        assert_eq!(
            client.signature.as_deref(),
            Some("class Client<T>(val url: String) : Base()")
        );
        let send = symbol("app/a.kt", SRC, "send");
        assert_eq!((send.kind, send.span), (SymbolKind::Method, (6, 8)));
        assert_eq!(
            send.signature.as_deref(),
            Some("fun send(body: String, retries: Int = 3): Result<Unit>")
        );
        assert_eq!(
            symbol("app/a.kt", SRC, "Handler").kind,
            SymbolKind::Interface
        );
        assert_eq!(symbol("app/a.kt", SRC, "Level").kind, SymbolKind::Enum);
        let limit = symbol("app/a.kt", SRC, "LIMIT");
        assert_eq!((limit.kind, limit.span), (SymbolKind::Const, (17, 17)));
        assert_eq!(limit.signature.as_deref(), Some("const val LIMIT"));
        let helper = symbol("app/a.kt", SRC, "helper");
        assert_eq!(helper.kind, SymbolKind::Function);
        assert_eq!(
            helper.signature.as_deref(),
            Some("fun helper(items: List<String>): Int")
        );
        assert_eq!(symbol("app/a.kt", SRC, "Slug").kind, SymbolKind::Type);
    }

    #[test]
    fn a_long_kotlin_signature_is_cut_to_the_limit() {
        let params: Vec<String> = (0..30).map(|n| format!("argument{n}: Int")).collect();
        let source = format!("fun long({}) {{}}\n", params.join(", "));
        let signature = symbol("a.kt", &source, "long")
            .signature
            .expect("a signature");
        assert_eq!(signature.chars().count(), crate::MAX_SIGNATURE);
        assert!(signature.ends_with('…'), "{signature}");
    }
}
