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
use super::support::{Placement, finish, inside, record, resolve_suffix, symbols_from_sites};

use crate::symbols::{self, Grammar, push_field, push_site, text};
use crate::{Candidate, SymbolKind, UnresolvedImport};

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
    let mut parsed = finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    );
    parsed.symbols = symbols_from_sites(
        tree.root_node(),
        bytes,
        &parsed.export_sites,
        &|node: Node, _name: Node| swift_kind(node, bytes),
    );
    parsed
}

/// The kind of a Swift declaration node, for the symbols list.
fn swift_kind(node: Node, source: &[u8]) -> Option<SymbolKind> {
    Some(match node.kind() {
        "class_declaration" => {
            match node
                .child_by_field_name("declaration_kind")
                .and_then(|kind| text(kind, source))
                .as_deref()
            {
                Some("struct") => SymbolKind::Struct,
                Some("enum") => SymbolKind::Enum,
                _ => SymbolKind::Class,
            }
        }
        "protocol_declaration" => SymbolKind::Interface,
        "function_declaration" => {
            if inside(node, &["class_body", "enum_class_body", "protocol_body"]) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "typealias_declaration" => SymbolKind::Type,
        "property_declaration" => SymbolKind::Const,
        _ => return None,
    })
}

/// Declarations a caller can name: a `func`, a `class`/`struct`/`enum`/
/// `actor`/`extension`, a `protocol`, a `typealias`, and a `public`/`open`
/// property. A property without a visibility modifier — a local inside a
/// function body, most often — is not one, as the pattern had it.
fn swift_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" if is_extension(child) => {}
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

/// `extension Type { … }` is a `class_declaration` whose name is the type it
/// extends. That type belongs to whoever declared it — often the standard
/// library or a framework — so the extension does not export it; the members
/// inside are exports on their own.
fn is_extension(node: Node) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .take_while(|child| child.kind() != "class_body" && child.kind() != "enum_class_body")
        .any(|child| child.kind() == "extension")
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
#[cfg(test)]
mod tests {
    use crate::lang::test_support::exports;

    fn names(source: &str) -> Vec<String> {
        exports("Source/A.swift", source).unwrap_or_else(|| panic!("swift parses"))
    }

    #[test]
    fn an_extension_exports_its_members_not_the_type_it_extends() {
        let names = names(
            r#"
public struct Own {}
extension NSNumber {
    func isBool() -> Bool { true }
    public var flag: Bool { true }
}
extension Array: Extended {}
extension Own { public func more() {} }
public func send(completion: Int, _ other: Int, label inner: Int) {}
"#,
        );
        for bad in ["NSNumber", "Array", "completion", "other", "inner", "label"] {
            assert!(!names.contains(&bad.to_owned()), "{bad}: {names:?}");
        }
        for good in ["Own", "isBool", "flag", "more", "send"] {
            assert!(names.contains(&good.to_owned()), "{good}: {names:?}");
        }
    }

    use crate::SymbolKind;
    use crate::lang::test_support::symbol;

    const SRC: &str = r#"
/// Docs.
public final class Session: NSObject {
    public func send(body: Data, retries: Int = 3) throws -> Response {
        return Response()
    }
}

public struct Point { public var x: Int }

public enum Level { case info, warn }

public protocol Handler { func handle(event: String) }

public typealias Slug = String

public func helper(_ items: [String], completion: () -> Void) -> Int {
    return 1
}

public let limit = 24
"#;

    #[test]
    fn swift_symbols_carry_kind_span_and_signature() {
        let session = symbol("Source/A.swift", SRC, "Session");
        assert_eq!((session.kind, session.span), (SymbolKind::Class, (3, 7)));
        assert_eq!(
            session.signature.as_deref(),
            Some("public final class Session: NSObject")
        );
        let send = symbol("Source/A.swift", SRC, "send");
        assert_eq!((send.kind, send.span), (SymbolKind::Method, (4, 6)));
        assert_eq!(
            send.signature.as_deref(),
            Some("public func send(body: Data, retries: Int = 3) throws -> Response")
        );
        assert_eq!(
            symbol("Source/A.swift", SRC, "Point").kind,
            SymbolKind::Struct
        );
        assert_eq!(
            symbol("Source/A.swift", SRC, "Level").kind,
            SymbolKind::Enum
        );
        assert_eq!(
            symbol("Source/A.swift", SRC, "Handler").kind,
            SymbolKind::Interface
        );
        assert_eq!(symbol("Source/A.swift", SRC, "Slug").kind, SymbolKind::Type);
        let helper = symbol("Source/A.swift", SRC, "helper");
        assert_eq!((helper.kind, helper.span), (SymbolKind::Function, (17, 19)));
        assert_eq!(
            helper.signature.as_deref(),
            Some("public func helper(_ items: [String], completion: () -> Void) -> Int")
        );
        let limit = symbol("Source/A.swift", SRC, "limit");
        assert_eq!(limit.kind, SymbolKind::Const);
        assert_eq!(limit.signature.as_deref(), Some("public let limit"));
    }

    #[test]
    fn a_long_swift_signature_is_cut_to_the_limit() {
        let params: Vec<String> = (0..30).map(|n| format!("argument{n}: Int")).collect();
        let source = format!("public func long({}) {{}}\n", params.join(", "));
        let signature = symbol("a.swift", &source, "long")
            .signature
            .expect("a signature");
        assert_eq!(signature.chars().count(), crate::MAX_SIGNATURE);
        assert!(signature.ends_with('…'), "{signature}");
    }
}
