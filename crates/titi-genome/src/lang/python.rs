//! Python: module-level definitions and the methods of module-level classes.
//!
//! The names come from the grammar's nodes, so an indented `def` inside a
//! class is a symbol and a commented-out one is not. Import lines are still
//! matched by pattern: a specifier is a string, not a node this crate walks.
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use super::ParsedFile;
use super::support::{finish, internal, record, resolve_python_relative};
use crate::symbols::{self, Grammar, push_site, text};
pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^from\s+(\.+[A-Za-z0-9_\.]*)\s+import").expect("py imports")
    });
    let imports_re = &*IMPORTS;
    let Some(tree) = symbols::parse(Grammar::Python, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let mut sites = Vec::new();
    python_module(tree.root_node(), source.as_bytes(), &mut sites);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in imports_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        record(
            spec,
            internal(resolve_python_relative(path, spec, files)),
            &mut imports,
            &mut unresolved,
        );
    }
    finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    )
}

/// Python: module-level definitions, the methods of module-level classes, and
/// screaming-case module constants. Leading-underscore names are private by
/// the language's own convention and stay out.
fn python_module(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let child = unwrap_decorated(child);
        match child.kind() {
            "function_definition" => push_python_name(child, source, out),
            "class_definition" => {
                push_python_name(child, source, out);
                if let Some(body) = child.child_by_field_name("body") {
                    python_class_members(body, source, out);
                }
            }
            "expression_statement" => {
                let mut assignments = child.walk();
                for assignment in child.named_children(&mut assignments) {
                    if assignment.kind() != "assignment" {
                        continue;
                    }
                    let Some(left) = assignment.child_by_field_name("left") else {
                        continue;
                    };
                    if left.kind() != "identifier" {
                        continue;
                    }
                    if text(left, source).is_some_and(|name| {
                        !name.starts_with('_')
                            && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                    }) {
                        push_site(left, source, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn python_class_members(body: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let member = unwrap_decorated(member);
        if member.kind() == "function_definition" {
            push_python_name(member, source, out);
        }
    }
}

/// `@decorator`-wrapped definitions hang under `decorated_definition`.
fn unwrap_decorated(node: Node) -> Node {
    if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition").unwrap_or(node)
    } else {
        node
    }
}

fn push_python_name(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    if text(name, source).is_some_and(|name| !name.starts_with('_')) {
        push_site(name, source, out);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::exports;

    fn names(path: &str, source: &str) -> Vec<String> {
        exports(path, source).unwrap_or_else(|| panic!("{path} parses"))
    }

    #[test]
    fn python_symbols_include_methods_and_constants() {
        let names = names(
            "app/service.py",
            r#"
MAX_RETRIES = 3
_internal = 1
lowercase_global = 2

class Service:
    def handle(self, request):
        pass

    @property
    def name(self):
        return "s"

    def _private(self):
        pass

    def __init__(self):
        pass

@decorator
def entry():
    pass

def _helper():
    pass
"#,
        );
        assert!(names.contains(&"Service".to_owned()));
        // Indented methods: invisible to a column-anchored regex.
        assert!(names.contains(&"handle".to_owned()), "{names:?}");
        assert!(names.contains(&"name".to_owned()));
        // A decorated module-level function still has its name.
        assert!(names.contains(&"entry".to_owned()));
        assert!(names.contains(&"MAX_RETRIES".to_owned()));
        assert!(!names.contains(&"_internal".to_owned()));
        assert!(!names.contains(&"lowercase_global".to_owned()));
        assert!(!names.contains(&"_private".to_owned()));
        assert!(!names.contains(&"__init__".to_owned()));
        assert!(!names.contains(&"_helper".to_owned()));
    }
}
