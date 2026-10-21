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
use super::support::{
    Placement, finish, inside, internal, record, resolve_relative, resolve_suffix,
    symbols_from_sites,
};

use crate::symbols::{self, Grammar, push_site, text};
use crate::{SymbolKind, UnresolvedImport};

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
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    ruby_items(
        tree.root_node(),
        bytes,
        path,
        files,
        &mut sites,
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
    parsed.symbols = symbols_from_sites(tree.root_node(), bytes, &parsed.export_sites, &ruby_kind);
    parsed
}

/// The kind of a Ruby declaration node, for the symbols list. A `def` inside a
/// class or module is a method; an `attr_*` call publishes accessors.
fn ruby_kind(node: Node, _name: Node) -> Option<SymbolKind> {
    Some(match node.kind() {
        "method" => {
            if inside(node, &["class", "module"]) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "singleton_method" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "module" => SymbolKind::Module,
        "call" => SymbolKind::Method,
        _ => return None,
    })
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
    unresolved: &mut Vec<UnresolvedImport>,
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
    unresolved: &mut Vec<UnresolvedImport>,
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
                let candidates = resolve_relative(path, &spec, &["rb"]);
                record(&spec, internal(candidates), files, imports, unresolved);
            } else {
                let stem = spec.strip_suffix(".rb").unwrap_or(&spec);
                // Nothing found means a gem or a `-I` directory, not a
                // missing file.
                let placement = Placement::Optional(resolve_suffix(stem, &["rb"]));
                record(&spec, placement, files, imports, unresolved);
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

#[cfg(test)]
mod tests {
    use crate::SymbolKind;
    use crate::lang::test_support::symbol;

    const SRC: &str = r#"# Docs.
module App
  class Client < Base
    attr_reader :id

    def send(body, retries = 3)
      body
    end
  end

  def self.build(options = {})
    new
  end
end

def helper(a, b)
  a + b
end
"#;

    #[test]
    fn ruby_symbols_carry_kind_span_and_signature() {
        let app = symbol("lib/app.rb", SRC, "App");
        assert_eq!((app.kind, app.span), (SymbolKind::Module, (2, 14)));
        assert_eq!(app.signature.as_deref(), Some("module App"));
        let client = symbol("lib/app.rb", SRC, "Client");
        assert_eq!((client.kind, client.span), (SymbolKind::Class, (3, 9)));
        assert_eq!(client.signature.as_deref(), Some("class Client < Base"));
        let send = symbol("lib/app.rb", SRC, "send");
        assert_eq!((send.kind, send.span), (SymbolKind::Method, (6, 8)));
        assert_eq!(
            send.signature.as_deref(),
            Some("def send(body, retries = 3)")
        );
        let build = symbol("lib/app.rb", SRC, "build");
        assert_eq!(build.kind, SymbolKind::Method);
        assert_eq!(
            build.signature.as_deref(),
            Some("def self.build(options = {})")
        );
        let helper = symbol("lib/app.rb", SRC, "helper");
        assert_eq!((helper.kind, helper.span), (SymbolKind::Function, (16, 18)));
        assert_eq!(symbol("lib/app.rb", SRC, "id").kind, SymbolKind::Method);
    }

    #[test]
    fn a_long_ruby_signature_is_cut_to_the_limit() {
        let params: Vec<String> = (0..30).map(|n| format!("argument_{n}")).collect();
        let source = format!("def long({})\n  1\nend\n", params.join(", "));
        let signature = symbol("a.rb", &source, "long")
            .signature
            .expect("a signature");
        assert_eq!(signature.chars().count(), crate::MAX_SIGNATURE);
        assert!(signature.ends_with('…'), "{signature}");
    }
}
