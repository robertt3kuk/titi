//! C and C++: declarations and quoted `#include` paths, read off the grammar.
//!
//! Both rows of the language table point here: the two share a declaration
//! shape and a header convention, and the quoted include is the only include
//! this crate resolves — a `<stdio.h>` is a system header, not a file of the
//! workspace, so it carries no diagnostic. Every name is a node the grammar
//! produced, so a declaration inside a comment is not one, and a `void
//! Engine::start()` definition exports its last `::` segment.
//!
//! One language cannot be told from the other by extension: a `.h` is claimed
//! by the C row, but a C++ header lives in one too. The C grammar turns `class
//! Engine { … }` into a class-less function with `ERROR` nodes where the
//! members are, so a `.h` whose C tree is broken is read with the C++ grammar
//! — a superset that parses C as well — and that tree is kept only when it is
//! cleaner than the C one. `.cpp`/`.cc`/`.hpp` and the rest go straight to the
//! C++ grammar.
//!
//! A declaration exports the last segment of its name, so an out-of-class C++
//! definition `void Engine::start()` exports `start`; a declaration whose
//! modifiers include `static` is file-private in both languages and is not
//! exported at all. A name with no tag (`typedef int Foo;`) is not a tag the
//! grammar records, so it is not an export — the same gap the patterns had.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use super::ParsedFile;
use super::support::{
    finish, inside, internal, normalize_join, parent, record, symbols_from_sites,
};

use crate::symbols::{self, Grammar, push_site, text};
use crate::{Candidate, SymbolKind, UnresolvedImport};

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(grammar) = super::grammar_for(path) else {
        return ParsedFile::default();
    };
    let Some(tree) = tree_for(grammar, path, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    c_items(tree.root_node(), bytes, &mut sites);

    let from_dir = parent(path).unwrap_or("");
    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    c_includes(
        tree.root_node(),
        bytes,
        from_dir,
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
    parsed.symbols = symbols_from_sites(tree.root_node(), bytes, &parsed.export_sites, &c_kind);
    parsed
}

/// The kind of a C/C++ declaration node, for the symbols list. A function
/// defined with a `Class::` qualifier, or declared in a class body, is a method.
fn c_kind(node: Node, name: Node) -> Option<SymbolKind> {
    Some(match node.kind() {
        "struct_specifier" | "union_specifier" => SymbolKind::Struct,
        "enum_specifier" => SymbolKind::Enum,
        "class_specifier" => SymbolKind::Class,
        "function_definition" | "declaration" | "field_declaration" => {
            if node.kind() == "field_declaration"
                || inside(node, &["field_declaration_list"])
                || name
                    .parent()
                    .is_some_and(|parent| parent.kind() == "qualified_identifier")
            {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        _ => return None,
    })
}

/// The tree to read the file from. A `.h` is claimed by C, but the C grammar
/// cannot read a C++ header: `class Engine { … }` becomes a class-less
/// function with the members inside `ERROR` nodes. When the C tree is broken,
/// the C++ grammar — a superset that also parses C — gets a second look, and
/// it is kept only when it is cleaner than the C one.
fn tree_for(grammar: Grammar, path: &str, source: &str) -> Option<Tree> {
    let tree = symbols::parse(grammar, source)?;
    if grammar != Grammar::C || !path.ends_with(".h") || !tree.root_node().has_error() {
        return Some(tree);
    }
    let Some(cpp) = symbols::parse(Grammar::Cpp, source) else {
        return Some(tree);
    };
    if symbols::error_count(cpp.root_node()) < symbols::error_count(tree.root_node()) {
        Some(cpp)
    } else {
        Some(tree)
    }
}

/// Every declaration a caller can name: a `struct`/`union`/`enum`/`class` tag
/// and a function (declaration, definition or class member). Nesting is
/// included, because a header's class body and a function's local declarations
/// are reached through the same walk — the patterns were line-anchored and
/// read those too.
fn c_items(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier" => {
                if !enclosing_static(child, source)
                    && let Some(name) = child.child_by_field_name("name")
                {
                    push_site(name, source, out);
                }
            }
            "function_definition" | "declaration" | "field_declaration" => {
                c_function(child, source, out);
            }
            _ => {}
        }
        // A function body holds statements and locals, not declarations a
        // caller can name: `SECTION("x") { … }` inside a test case parses as a
        // function and is not one.
        if !matches!(child.kind(), "compound_statement" | "lambda_expression") {
            c_items(child, source, out);
        }
    }
}

/// Adds the name of a declaration whose declarator calls something: `int
/// hash(const char *key);` and `void Engine::start() { }` alike, but not `int
/// x = 1;` and not a `static` declaration, which is file-private.
fn c_function(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    if is_static(node, source) {
        return;
    }
    // `int first(void), second(void);` is one declaration with two declarators
    // under the same field, and the patterns only ever saw the first.
    let mut cursor = node.walk();
    for declarator in node.children_by_field_name("declarator", &mut cursor) {
        if !calls_something(declarator) {
            continue;
        }
        if let Some(name) = declarator_name(declarator)
            && !is_macro_call(node, name, source)
            && !KEYWORDS.contains(&text(name, source).unwrap_or_default().as_str())
        {
            push_site(name, source, out);
        }
    }
}

/// Words the grammar can mistake for a declarator name.
const KEYWORDS: &[&str] = &[
    "noexcept", "throw", "decltype", "sizeof", "alignas", "typeof",
];

/// `JSON_CATCH(...) { }` and `TEST_CASE(x) { }`: a SCREAMING_CASE name, or, outside a class, a function
/// with no return type and a bare, unqualified name. Real C++ spells such a
/// thing only as a constructor or destructor, and those live in a class body
/// or carry a `Class::` qualifier, so a bare one at file or namespace scope
/// is a macro invocation the grammar read as a definition.
fn is_macro_call(node: Node, name: Node, source: &[u8]) -> bool {
    // A name in SCREAMING_CASE is a macro by convention. Preprocessor
    // conditionals can derail the grammar so far that `SECTION("x") { … }`
    // inside a test body is read as a typed function at file scope.
    if matches!(name.kind(), "identifier" | "field_identifier")
        && text(name, source).is_some_and(|name| {
            name.len() >= 2
                && name.bytes().any(|byte| byte.is_ascii_uppercase())
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        })
    {
        return true;
    }
    if node.child_by_field_name("type").is_some() || name.kind() != "identifier" {
        return false;
    }
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == "field_declaration_list" {
            return false;
        }
        ancestor = parent.parent();
    }
    text(name, source).is_some()
}

/// Whether a declarator chain goes through a function declarator — the whole
/// difference between a function and a variable declaration.
fn calls_something(node: Node) -> bool {
    if matches!(
        node.kind(),
        "function_declarator" | "abstract_function_declarator"
    ) {
        return true;
    }
    node.child_by_field_name("declarator")
        .is_some_and(calls_something)
}

/// The name at the bottom of a declarator chain. A qualified name keeps its
/// last segment, so `Engine::start` exports `start`, and the site points at
/// that segment rather than at the class.
fn declarator_name(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "operator_name"
        | "destructor_name" => Some(node),
        "qualified_identifier" | "template_function" | "template_method" | "template_type" => {
            node.child_by_field_name("name").and_then(declarator_name)
        }
        _ => node
            .child_by_field_name("declarator")
            .and_then(declarator_name),
    }
}

/// A declaration whose modifiers include `static` is file-private, exactly as
/// the pattern recognised: the keyword is a `storage_class_specifier` child.
fn is_static(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| {
        child.kind() == "storage_class_specifier"
            && text(child, source).as_deref() == Some("static")
    })
}

/// Whether a tag sits inside a `static` declaration: `static struct X { … };`
/// declares a file-private tag, so it is not an export.
fn enclosing_static(node: Node, source: &[u8]) -> bool {
    is_static(node, source)
        || node
            .parent()
            .is_some_and(|parent| is_static(parent, source))
}

/// Every quoted `#include` in the file. An angle-bracket include names a
/// system header and is not this workspace's business, so it is skipped; a
/// comment cannot carry one, because it never becomes a node.
fn c_includes(
    node: Node,
    source: &[u8],
    from_dir: &str,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    if node.kind() == "preproc_include" {
        if let Some(path) = node.child_by_field_name("path")
            && path.kind() == "string_literal"
            && let Some(raw) = text(path, source)
        {
            let spec = raw.trim_matches('"');
            let joined = if from_dir.is_empty() {
                spec.to_owned()
            } else {
                format!("{from_dir}/{spec}")
            };
            let candidates = normalize_join("", &joined)
                .map(|joined| vec![Candidate::Exact(joined)])
                .unwrap_or_default();
            record(spec, internal(candidates), files, imports, unresolved);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        c_includes(child, source, from_dir, files, imports, unresolved);
    }
}

#[cfg(test)]
mod tests {
    use crate::lang::test_support::exports;

    fn names(path: &str, source: &str) -> Vec<String> {
        exports(path, source).unwrap_or_else(|| panic!("{path} parses"))
    }

    #[test]
    fn test_macro_invocations_are_not_exported_functions() {
        let names = names(
            "tests/unit-x.cpp",
            r#"
#include "doctest.h"
TEST_CASE("serialization")
{
    SECTION("person") { CHECK(1 == 1); }
    CHECK_THROWS_AS(parse(), std::exception&);
    json j = json::array();
    JSON_CATCH(...) {}
}
TEST_CASE_TEMPLATE("t", T, int, long) { }
NLOHMANN_DEFINE_TYPE_NON_INTRUSIVE(Person, name)
JSON_CATCH(...) { }
void SECTION_AFTER_BROKEN_IF(int x) { }
namespace ns { TEST_CASE("inner") { } int real(int x) { return x; } }
"#,
        );
        assert_eq!(names, vec!["real".to_owned()], "{names:?}");
    }

    #[test]
    fn keywords_and_local_declarations_are_not_exports() {
        let names = names(
            "include/a.hpp",
            r#"
template <class T>
auto convert(T&& v) noexcept(noexcept(from(v))) -> int;
struct from_json_fn {
    template<typename B, typename T>
    auto operator()(const B& j, T&& val) const
    noexcept(noexcept(from_json(j, std::forward<T>(val))))
    -> decltype(from_json(j, std::forward<T>(val)))
    {
        return from_json(j, std::forward<T>(val));
    }
};
struct Engine {
    Engine() {}
    ~Engine() {}
    void start() noexcept {}
};
int run() { int local(3); return local; }
Engine::Engine(int) {}
"#,
        );
        for bad in ["noexcept", "local"] {
            assert!(!names.contains(&bad.to_owned()), "{bad}: {names:?}");
        }
        for good in ["convert", "Engine", "start", "run"] {
            assert!(names.contains(&good.to_owned()), "{good}: {names:?}");
        }
    }

    use crate::SymbolKind;
    use crate::lang::test_support::symbol;

    const C_SRC: &str = r#"#include <stdio.h>

struct Hash {
    int size;
};

enum Level { INFO, WARN };

int hash(const char *key, unsigned seed);

int hash(const char *key, unsigned seed)
{
    return 0;
}
"#;

    #[test]
    fn c_symbols_carry_kind_span_and_signature() {
        let hash_type = symbol("src/hash.c", C_SRC, "Hash");
        assert_eq!(
            (hash_type.kind, hash_type.span),
            (SymbolKind::Struct, (3, 5))
        );
        assert_eq!(hash_type.signature.as_deref(), Some("struct Hash"));
        assert_eq!(symbol("src/hash.c", C_SRC, "Level").kind, SymbolKind::Enum);
        let hash = symbol("src/hash.c", C_SRC, "hash");
        assert_eq!(hash.kind, SymbolKind::Function);
        assert_eq!(
            hash.signature.as_deref(),
            Some("int hash(const char *key, unsigned seed)")
        );
    }

    const CPP_SRC: &str = r#"class Engine : public Base {
public:
    void start(int id) noexcept;
};

void Engine::start(int id) noexcept
{
}
"#;

    #[test]
    fn cpp_symbols_carry_kind_span_and_signature() {
        let engine = symbol("src/engine.cpp", CPP_SRC, "Engine");
        assert_eq!((engine.kind, engine.span), (SymbolKind::Class, (1, 4)));
        assert_eq!(
            engine.signature.as_deref(),
            Some("class Engine : public Base")
        );
        let start = symbol("src/engine.cpp", CPP_SRC, "start");
        assert_eq!(start.kind, SymbolKind::Method);
        assert_eq!(
            start.signature.as_deref(),
            Some("void start(int id) noexcept")
        );
    }

    #[test]
    fn a_long_c_signature_is_cut_to_the_limit() {
        let params: Vec<String> = (0..30).map(|n| format!("int argument{n}")).collect();
        let source = format!("int long_fn({}) {{ return 0; }}\n", params.join(", "));
        let signature = symbol("a.c", &source, "long_fn")
            .signature
            .expect("a signature");
        assert_eq!(signature.chars().count(), crate::MAX_SIGNATURE);
        assert!(signature.ends_with('…'), "{signature}");
    }
}
