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
use super::support::{Placement, finish, record, resolve_suffix, symbols_from_sites};

use crate::symbols::{self, Grammar, push_site, text};
use crate::{SymbolKind, UnresolvedImport};

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
    let mut parsed = finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    );
    parsed.symbols = symbols_from_sites(tree.root_node(), bytes, &parsed.export_sites, &go_kind);
    parsed
}

/// The kind of a Go declaration node, for the symbols list. A `type` spec is
/// a struct or an interface by what it names, otherwise a plain type.
fn go_kind(node: Node, _name: Node) -> Option<SymbolKind> {
    Some(match node.kind() {
        "function_declaration" => SymbolKind::Function,
        "method_declaration" => SymbolKind::Method,
        "type_spec" => match node.child_by_field_name("type").map(|ty| ty.kind()) {
            Some("struct_type") => SymbolKind::Struct,
            Some("interface_type") => SymbolKind::Interface,
            _ => SymbolKind::Type,
        },
        "type_alias" => SymbolKind::Type,
        "const_spec" | "var_spec" => SymbolKind::Const,
        _ => return None,
    })
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

#[cfg(test)]
mod tests {
    use crate::SymbolKind;
    use crate::lang::test_support::symbol;

    const SRC: &str = r#"package app

// Limit docs.
const Limit = 24

// Server serves.
type Server struct {
	addr string
}

type Handler interface {
	Serve(addr string) error
}

type ID = string

func New(addr string, opts ...Option) (*Server, error) {
	return nil, nil
}

func (s *Server) Start(ctx context.Context) error {
	return nil
}
"#;

    #[test]
    fn go_symbols_carry_kind_span_and_signature() {
        let limit = symbol("app/a.go", SRC, "Limit");
        assert_eq!((limit.kind, limit.span), (SymbolKind::Const, (4, 4)));
        assert_eq!(limit.signature.as_deref(), Some("const Limit"));
        let server = symbol("app/a.go", SRC, "Server");
        assert_eq!((server.kind, server.span), (SymbolKind::Struct, (7, 9)));
        assert_eq!(server.signature.as_deref(), Some("type Server struct"));
        let handler = symbol("app/a.go", SRC, "Handler");
        assert_eq!(
            (handler.kind, handler.span),
            (SymbolKind::Interface, (11, 13))
        );
        assert_eq!(symbol("app/a.go", SRC, "ID").kind, SymbolKind::Type);
        let new = symbol("app/a.go", SRC, "New");
        assert_eq!((new.kind, new.span), (SymbolKind::Function, (17, 19)));
        assert_eq!(
            new.signature.as_deref(),
            Some("func New(addr string, opts ...Option) (*Server, error)")
        );
        let start = symbol("app/a.go", SRC, "Start");
        assert_eq!((start.kind, start.span), (SymbolKind::Method, (21, 23)));
        assert_eq!(
            start.signature.as_deref(),
            Some("func (s *Server) Start(ctx context.Context) error")
        );
    }

    #[test]
    fn a_long_go_signature_is_cut_to_the_limit() {
        let params: Vec<String> = (0..30).map(|n| format!("argument{n} int")).collect();
        let source = format!("package a\n\nfunc Long({}) {{}}\n", params.join(", "));
        let signature = symbol("a.go", &source, "Long")
            .signature
            .expect("a signature");
        assert_eq!(signature.chars().count(), crate::MAX_SIGNATURE);
        assert!(signature.ends_with('…'), "{signature}");
    }
}
