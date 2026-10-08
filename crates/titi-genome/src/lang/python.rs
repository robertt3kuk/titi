//! Python: module-level definitions and the methods of module-level classes.
//!
//! The names come from the grammar's nodes, so an indented `def` inside a
//! class is a symbol and a commented-out one is not. Import lines are still
//! matched by pattern: a specifier is a string, not a node this crate walks.
//!
//! Both import forms are read — `import a.b` and `from .a import b` — and a
//! dotted module path resolves to the workspace file it names, so an absolute
//! intra-repo import like `from app.util import helper` is an edge like any
//! other. A specifier that resolves to nothing is probed against the file set
//! once — one pass per unresolved import: a first segment the workspace holds
//! as a directory is a genuinely missing workspace file and warns, anything
//! else is a std or third-party package and stays out of the graph.
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use super::ParsedFile;
use super::support::{
    Comments, Placement, finish, first_known, mask_comments, record, resolve_python_relative,
    resolve_suffix, root_segment,
};
use crate::patterns::literal_regex;
use crate::symbols::{self, Grammar, push_site, text};
use crate::{Candidate, UnresolvedImport};

/// The files a Python module path can land on: a module is a `.py` file, or a
/// package's `__init__.py`.
const EXTS: &[&str] = &["py"];

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static FROM: LazyLock<Regex> = LazyLock::new(|| {
        literal_regex(
            r"(?ms)^[ \t]*from[ \t]+(\.*[A-Za-z0-9_.]*)[ \t]+import[ \t]+(\([^)]*\)|[^\n]*)",
        )
    });
    static IMPORT: LazyLock<Regex> =
        LazyLock::new(|| literal_regex(r"(?m)^[ \t]*import[ \t]+([^\n]+)"));
    let Some(tree) = symbols::parse(Grammar::Python, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let mut sites = Vec::new();
    python_module(tree.root_node(), source.as_bytes(), &mut sites);
    let masked = mask_comments(source, Comments::Hash);
    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    for cap in FROM.captures_iter(&masked) {
        let module = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let names = cap
            .get(2)
            .map(|m| clause_names(m.as_str()))
            .unwrap_or_default();
        from_clause(path, module, &names, files, &mut imports, &mut unresolved);
    }
    for cap in IMPORT.captures_iter(&masked) {
        let clause = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for spec in clause_modules(clause) {
            import_module(&spec, files, &mut imports, &mut unresolved);
        }
    }
    finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    )
}

/// A `from … import …` clause. The module path names a file when it can; when
/// it cannot — a dotted-only `from . import sibling`, or a package whose
/// submodule is what is really meant — the named submodules are tried before
/// giving up, because in Python `from app import sibling` reaches
/// `app/sibling.py`.
fn from_clause(
    path: &str,
    module: &str,
    names: &[String],
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    if module.is_empty() {
        return;
    }
    let dotted_only = module.chars().all(|c| c == '.');
    if !dotted_only {
        let candidates = module_candidates(path, module);
        if let Some(resolved) = first_known(&candidates, files) {
            imports.push(resolved);
            return;
        }
    }
    let mut resolved_any = false;
    let mut missed: Vec<(String, Vec<Candidate>)> = Vec::new();
    for name in names {
        // A dotted-only module already ends in its dot: `from . import x` is
        // `.x`, not `..x`.
        let spec = if dotted_only {
            format!("{module}{name}")
        } else {
            format!("{module}.{name}")
        };
        let candidates = module_candidates(path, &spec);
        match first_known(&candidates, files) {
            Some(resolved) => {
                imports.push(resolved);
                resolved_any = true;
            }
            None => missed.push((spec, candidates)),
        }
    }
    if resolved_any {
        return;
    }
    if dotted_only {
        // Nothing but dots names no file of its own; the names after `import`
        // are the submodules, and a bare `from . import x` names `.x`.
        if names.is_empty() {
            unresolved.push(UnresolvedImport {
                spec: module.to_owned(),
                candidates: module_candidates(path, module),
            });
        } else {
            for (spec, candidates) in missed {
                unresolved.push(UnresolvedImport { spec, candidates });
            }
        }
        return;
    }
    let candidates = resolve_absolute(module);
    record(
        module,
        placement(module, candidates, files),
        files,
        imports,
        unresolved,
    );
}

/// A plain `import a.b`: always absolute, so a specifier that names no file is
/// a workspace dependency only when the workspace has a directory of that
/// first name; otherwise it is a std or third-party package.
fn import_module(
    spec: &str,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    let candidates = resolve_absolute(spec);
    let placement = if first_known(&candidates, files).is_some() {
        Placement::Candidates(candidates)
    } else {
        placement(spec, candidates, files)
    };
    record(spec, placement, files, imports, unresolved);
}

/// Where a dotted module path lands: a relative path walks the file's own
/// package, an absolute one is looked up by its trailing module path.
fn module_candidates(path: &str, spec: &str) -> Vec<Candidate> {
    if spec.starts_with('.') {
        return resolve_python_relative(path, spec);
    }
    resolve_absolute(spec)
}

/// `app.util` → the workspace files for `app/util`.
fn resolve_absolute(spec: &str) -> Vec<Candidate> {
    resolve_suffix(&spec.replace('.', "/"), EXTS)
}

/// A specifier that resolved to nothing: the first segment decides. When the
/// workspace holds a directory of that name the import is workspace-shaped and
/// its target is genuinely missing; when it does not, the specifier names a
/// std or third-party package and is not this workspace's business. Probing
/// the file set costs one pass per unresolved import, which is why it only
/// runs once the candidates have all missed.
fn placement(spec: &str, candidates: Vec<Candidate>, files: &HashSet<String>) -> Placement {
    let root = root_segment(spec);
    if root.is_empty() {
        return Placement::External;
    }
    let top = format!("{root}/");
    let nested = format!("/{root}/");
    if files
        .iter()
        .any(|known| known.starts_with(&top) || known.contains(&nested))
    {
        Placement::Candidates(candidates)
    } else {
        Placement::External
    }
}

/// The module paths an `import a.b as c, d.e` clause brings in; an `as` alias
/// names a local binding, not a file.
fn clause_modules(clause: &str) -> Vec<String> {
    let cleaned = plain(clause);
    cleaned
        .split(',')
        .filter_map(|entry| entry.split_whitespace().next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The names a `from m import a, b as c` clause brings in, wrapped form
/// included; `*` is not a name.
fn clause_names(clause: &str) -> Vec<String> {
    let cleaned = plain(clause);
    cleaned
        .split(',')
        .filter_map(|entry| entry.split_whitespace().next())
        .filter(|name| !name.is_empty() && *name != "*")
        .map(str::to_owned)
        .collect()
}

/// Drops the parentheses a wrapped import clause is written with.
fn plain(clause: &str) -> String {
    clause.chars().filter(|c| *c != '(' && *c != ')').collect()
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
    use crate::lang::test_support::exports;

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
