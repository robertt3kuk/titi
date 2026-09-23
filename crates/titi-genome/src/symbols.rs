//! Symbol extraction over real syntax trees.
//!
//! Regexes used to guess declarations line by line, which meant commented-out
//! code counted, string literals counted, and anything the pattern did not
//! anchor at the start of a line — a method inside a Python class, a member of
//! an exported TypeScript class — did not. Parsing removes the guessing: the
//! names below come out of the grammar's own nodes.
//!
//! Only the three languages with a grammar go through here (Rust, TS/JS,
//! Python). Everything else keeps its heuristics in `parse`, and import
//! resolution stays there too — that works on module specifiers, not on
//! declarations.
//!
//! Spec: `docs/PLAN.md` (P4-3).

use std::cell::RefCell;

use tree_sitter::{Node, Parser};

/// A grammar this crate links. One parser per grammar is kept per thread:
/// `Parser::set_language` is the expensive part and it never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grammar {
    Rust,
    TypeScript,
    /// Also used for plain JS and JSX: the TSX grammar accepts both.
    Tsx,
    Python,
}

impl Grammar {
    /// The grammar a path's extension needs, `None` when no grammar applies.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = std::path::Path::new(path).extension()?.to_str()?;
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::Tsx),
            "py" | "pyi" => Some(Self::Python),
            _ => None,
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }
}

#[derive(Default)]
struct Parsers {
    rust: Option<Parser>,
    typescript: Option<Parser>,
    tsx: Option<Parser>,
    python: Option<Parser>,
}

impl Parsers {
    fn slot(&mut self, grammar: Grammar) -> &mut Option<Parser> {
        match grammar {
            Grammar::Rust => &mut self.rust,
            Grammar::TypeScript => &mut self.typescript,
            Grammar::Tsx => &mut self.tsx,
            Grammar::Python => &mut self.python,
        }
    }
}

thread_local! {
    static PARSERS: RefCell<Parsers> = RefCell::new(Parsers::default());
}

/// Whether `grammar` is usable in this build. A grammar whose ABI the linked
/// tree-sitter does not accept fails here instead of silently emptying the
/// map, which is what the ABI test checks.
#[cfg(test)]
pub fn grammar_available(grammar: Grammar) -> bool {
    let mut parser = Parser::new();
    parser.set_language(&grammar.language()).is_ok()
}

/// Parses `source` with `grammar`, reusing this thread's parser.
///
/// `None` means the parser refused the input outright; it says nothing about
/// syntax errors inside the tree, which tree-sitter reports as `ERROR` nodes.
pub(crate) fn parse(grammar: Grammar, source: &str) -> Option<tree_sitter::Tree> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let slot = parsers.slot(grammar);
        if slot.is_none() {
            let mut parser = Parser::new();
            parser.set_language(&grammar.language()).ok()?;
            *slot = Some(parser);
        }
        slot.as_mut()?.parse(source, None)
    })
}

/// Names `source` declares and publishes, in source order.
///
/// `None` means the file could not be parsed at all (no grammar for the
/// extension, or the parser refused the input); the caller then contributes no
/// symbols for the file rather than guessing at them.
pub fn exports(path: &str, source: &str) -> Option<Vec<String>> {
    let grammar = Grammar::from_path(path)?;
    let tree = parse(grammar, source)?;

    let bytes = source.as_bytes();
    let mut out = Vec::new();
    match grammar {
        Grammar::Rust => rust_items(tree.root_node(), bytes, false, &mut out),
        Grammar::TypeScript | Grammar::Tsx => ts_module(tree.root_node(), bytes, &mut out),
        Grammar::Python => python_module(tree.root_node(), bytes, &mut out),
    }
    Some(out)
}

fn text(node: Node, source: &[u8]) -> Option<String> {
    node.utf8_text(source).ok().map(str::to_owned)
}

fn field_text(node: Node, field: &str, source: &[u8]) -> Option<String> {
    text(node.child_by_field_name(field)?, source)
}

fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

/// Rust: everything reachable from outside the module, so `pub` items at any
/// nesting — including inherent and trait methods, which is what a caller
/// actually names. Trait members inherit the trait's visibility.
fn rust_items(node: Node, source: &[u8], inherited: bool, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let public = inherited || has_child_kind(child, "visibility_modifier");
        match child.kind() {
            "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "associated_type"
            | "macro_definition" => {
                if public && let Some(name) = field_text(child, "name", source) {
                    out.push(name);
                }
            }
            "mod_item" => {
                if public && let Some(name) = field_text(child, "name", source) {
                    out.push(name);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    rust_items(body, source, false, out);
                }
            }
            "trait_item" => {
                if public && let Some(name) = field_text(child, "name", source) {
                    out.push(name);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    rust_items(body, source, public, out);
                }
            }
            "impl_item" => {
                if let Some(body) = child.child_by_field_name("body") {
                    rust_items(body, source, false, out);
                }
            }
            _ => {}
        }
    }
}

/// TypeScript and JavaScript: what an `export` publishes, plus the public
/// members of an exported class — `client.send(…)` names a symbol the file
/// owns just as much as the class does.
fn ts_module(node: Node, source: &[u8], out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "export_statement" {
            ts_export(child, source, out);
        }
    }
}

fn ts_export(node: Node, source: &[u8], out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // `export { a, b as c }`: the exported name is the alias.
            "export_clause" => {
                let mut specs = child.walk();
                for spec in child.named_children(&mut specs) {
                    if spec.kind() != "export_specifier" {
                        continue;
                    }
                    let name = spec
                        .child_by_field_name("alias")
                        .or_else(|| spec.child_by_field_name("name"));
                    if let Some(name) = name.and_then(|node| text(node, source)) {
                        out.push(name);
                    }
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                if let Some(name) = field_text(child, "name", source) {
                    out.push(name);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    ts_class_members(body, source, out);
                }
            }
            "function_declaration"
            | "generator_function_declaration"
            | "enum_declaration"
            | "type_alias_declaration"
            | "interface_declaration"
            | "internal_module"
            | "module" => {
                if let Some(name) = field_text(child, "name", source) {
                    out.push(name);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut declarators = child.walk();
                for declarator in child.named_children(&mut declarators) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    // Destructuring publishes several names; the pattern's own
                    // identifiers are the exported ones.
                    if name.kind() == "identifier"
                        && let Some(name) = text(name, source)
                    {
                        out.push(name);
                    }
                }
            }
            _ => {}
        }
    }
}

fn ts_class_members(body: Node, source: &[u8], out: &mut Vec<String>) {
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if !matches!(
            member.kind(),
            "method_definition" | "public_field_definition" | "method_signature"
        ) {
            continue;
        }
        if has_child_kind(member, "accessibility_modifier")
            && member
                .child_by_field_name("name")
                .is_none_or(|name| name.kind() == "private_property_identifier")
        {
            continue;
        }
        let Some(name) = member.child_by_field_name("name") else {
            continue;
        };
        // `#private` members and the constructor are not names a caller uses.
        if name.kind() == "private_property_identifier" {
            continue;
        }
        if let Some(name) = text(name, source)
            && name != "constructor"
        {
            out.push(name);
        }
    }
}

/// Python: module-level definitions, the methods of module-level classes, and
/// screaming-case module constants. Leading-underscore names are private by
/// the language's own convention and stay out.
fn python_module(node: Node, source: &[u8], out: &mut Vec<String>) {
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
                    if let Some(name) = text(left, source)
                        && !name.starts_with('_')
                        && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                    {
                        out.push(name);
                    }
                }
            }
            _ => {}
        }
    }
}

fn python_class_members(body: Node, source: &[u8], out: &mut Vec<String>) {
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

fn push_python_name(node: Node, source: &[u8], out: &mut Vec<String>) {
    if let Some(name) = field_text(node, "name", source)
        && !name.starts_with('_')
    {
        out.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(path: &str, source: &str) -> Vec<String> {
        exports(path, source).unwrap_or_else(|| panic!("{path} parses"))
    }

    /// The guard behind the pinned grammar versions: a grammar whose ABI the
    /// linked tree-sitter rejects would empty the map silently.
    #[test]
    fn grammars_match_the_core_abi() {
        for grammar in [
            Grammar::Rust,
            Grammar::TypeScript,
            Grammar::Tsx,
            Grammar::Python,
        ] {
            assert!(grammar_available(grammar), "{grammar:?} failed to load");
        }
    }

    #[test]
    fn rust_symbols_include_methods_and_skip_private_ones() {
        let names = names(
            "src/lib.rs",
            r#"
pub struct Engine { field: u32 }
struct Hidden;
pub const LIMIT: usize = 8;
static PRIVATE: u8 = 0;
pub static SHARED: u8 = 1;
pub trait Runner { fn run(&self); }
impl Engine {
    pub fn spawn(&self) {}
    fn internal(&self) {}
}
pub mod inner { pub fn nested() {} }
"#,
        );
        assert!(names.contains(&"Engine".to_owned()));
        assert!(names.contains(&"LIMIT".to_owned()));
        assert!(names.contains(&"SHARED".to_owned()));
        assert!(names.contains(&"Runner".to_owned()));
        // A trait method is as public as its trait.
        assert!(names.contains(&"run".to_owned()));
        // The inherent method the line-anchored regex could only find by luck.
        assert!(names.contains(&"spawn".to_owned()), "{names:?}");
        assert!(names.contains(&"nested".to_owned()));
        assert!(!names.contains(&"Hidden".to_owned()));
        assert!(!names.contains(&"PRIVATE".to_owned()));
        assert!(!names.contains(&"internal".to_owned()));
    }

    /// The old pattern was line-anchored, so a declaration that started its
    /// own line counted even inside a block comment or a string literal:
    /// `^\s*pub\s+(?:fn|struct|…)` matched `block_ghost` and `quoted` here.
    /// The grammar knows a comment from code.
    #[test]
    fn declarations_inside_comments_and_strings_are_not_symbols() {
        let names = names(
            "src/lib.rs",
            "pub fn real() {}\n\
             /*\n\
             pub fn block_ghost() {}\n\
             */\n\
             pub const SNIPPET: &str = \"\n\
             pub fn quoted() {}\n\
             \";\n",
        );
        assert_eq!(names, vec!["real".to_owned(), "SNIPPET".to_owned()]);
    }

    #[test]
    fn typescript_symbols_include_class_members() {
        let names = names(
            "src/client.ts",
            r#"
export class Client {
  private secret = 1;
  constructor(private url: string) {}
  send(body: string) {}
  static create() { return new Client(""); }
  #hidden() {}
}
export function connect() {}
export const TIMEOUT = 30;
export interface Options { retries: number }
export type Handler = (x: number) => void;
export enum Level { Info }
const notExported = 2;
"#,
        );
        assert!(names.contains(&"Client".to_owned()));
        // Members of an exported class, which the regex never saw.
        assert!(names.contains(&"send".to_owned()), "{names:?}");
        assert!(names.contains(&"create".to_owned()));
        assert!(names.contains(&"connect".to_owned()));
        assert!(names.contains(&"TIMEOUT".to_owned()));
        assert!(names.contains(&"Options".to_owned()));
        assert!(names.contains(&"Handler".to_owned()));
        assert!(names.contains(&"Level".to_owned()));
        assert!(!names.contains(&"constructor".to_owned()));
        assert!(!names.contains(&"hidden".to_owned()));
        assert!(!names.contains(&"notExported".to_owned()));
    }

    #[test]
    fn javascript_and_jsx_parse_with_the_tsx_grammar() {
        let names = names(
            "src/view.jsx",
            r#"
export default class View {
  render() { return <div className="x">hi</div>; }
}
export const widgets = [];
export { helper as publicHelper };
function helper() {}
"#,
        );
        assert!(names.contains(&"View".to_owned()), "{names:?}");
        assert!(names.contains(&"render".to_owned()));
        assert!(names.contains(&"widgets".to_owned()));
        // Re-exports publish the alias, not the local name.
        assert!(names.contains(&"publicHelper".to_owned()));
        assert!(!names.contains(&"helper".to_owned()));
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

    /// A half-written file is the normal state of a file being edited: the
    /// parser recovers and the symbols before the damage still land.
    #[test]
    fn a_file_with_a_syntax_error_still_yields_what_parsed() {
        let names = names("src/lib.rs", "pub fn first() {}\npub fn second( {\n");
        assert!(names.contains(&"first".to_owned()), "{names:?}");
    }

    #[test]
    fn a_language_without_a_grammar_is_not_claimed() {
        assert!(exports("main.go", "func Handler() {}").is_none());
        assert!(exports("README.md", "# title").is_none());
    }
}
