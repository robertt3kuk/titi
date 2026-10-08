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
#[cfg(test)]
pub fn exports(path: &str, source: &str) -> Option<Vec<String>> {
    let extracted = extract(path, source);
    if !extracted.parsed {
        return None;
    }
    Some(extracted.sites.into_iter().map(|site| site.name).collect())
}

/// Export sites plus how many tree-sitter `ERROR` nodes the grammar reported.
///
/// `parsed` is false when there is no grammar or the parser refused the input.
/// A refusal is one syntax error, not zero: the file was not clean, it was
/// unreadable. Languages without a grammar stay at zero — this crate does not
/// invent errors it did not parse.
pub(crate) struct Extract {
    pub parsed: bool,
    pub sites: Vec<crate::ExportSite>,
    pub syntax_errors: u32,
    /// Rust `use`/`mod` items, from the syntax tree rather than a line regex.
    /// Empty for every other language and for a file whose grammar refused the
    /// input. The caller must not fall back to pattern matching: an unparsed
    /// file has no established imports, and guessing them from text is what
    /// made comment and string-literal `use` lines into phantom imports.
    pub rust_imports: Vec<RustImport>,
}

/// One Rust import site, flattened to the path it names.
///
/// `use a::{b, c as d}` yields two: `["a", "b"]` and `["a", "c"]`. A bare
/// `mod x;` is one with `is_mod`, whose resolution is relative to the module
/// that declares it and whose spec to report is just `x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RustImport {
    pub segments: Vec<String>,
    /// Inline `mod` names enclosing the declaration, outermost first. This is
    /// what makes `super::*` inside `#[cfg(test)] mod tests` resolve to the
    /// file's own module instead of a directory one level too high.
    pub mods: Vec<String>,
    pub is_mod: bool,
}

pub(crate) fn extract(path: &str, source: &str) -> Extract {
    let Some(grammar) = Grammar::from_path(path) else {
        return Extract {
            parsed: false,
            sites: Vec::new(),
            syntax_errors: 0,
            rust_imports: Vec::new(),
        };
    };
    let Some(tree) = parse(grammar, source) else {
        return Extract {
            parsed: false,
            sites: Vec::new(),
            syntax_errors: 1,
            rust_imports: Vec::new(),
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    let mut rust_imports = Vec::new();
    match grammar {
        Grammar::Rust => {
            rust_items(tree.root_node(), bytes, false, &mut sites);
            let mut cursor = Vec::new();
            collect_rust_imports(tree.root_node(), bytes, &mut cursor, &mut rust_imports);
        }
        Grammar::TypeScript | Grammar::Tsx => ts_module(tree.root_node(), bytes, &mut sites),
        Grammar::Python => python_module(tree.root_node(), bytes, &mut sites),
    }
    Extract {
        parsed: true,
        sites,
        syntax_errors: count_errors(tree.root_node(), source, grammar),
        rust_imports,
    }
}

fn count_errors(node: Node, source: &str, grammar: Grammar) -> u32 {
    let errors = count_error_nodes(node);
    if errors == 0 || grammar != Grammar::Rust {
        return errors;
    }
    // The pinned tree-sitter-rust reads `&raw` as the opening of a raw borrow
    // (`&raw const`/`&raw mut`) and fails on `&raw` where `raw` is an ordinary
    // identifier — `f(&raw)`, `&raw[i]`, `assemble(&raw, false)` — which is
    // everyday code. Reparse with every such `&raw` renamed and keep the
    // smaller count: an error the grammar reports only because of that token
    // ambiguity is not a claim this crate can make about the file.
    let Some(scrubbed) = neutralise_raw_refs(source) else {
        return errors;
    };
    let Some(tree) = parse(grammar, &scrubbed) else {
        return errors;
    };
    count_error_nodes(tree.root_node()).min(errors)
}

fn count_error_nodes(node: Node) -> u32 {
    let mut count = u32::from(node.is_error());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count = count.saturating_add(count_error_nodes(child));
    }
    count
}

/// Rewrites every `&raw` that the grammar cannot tell from a raw borrow into
/// `&rawx`. Returns `None` when there is nothing to rewrite, so the common
/// clean file pays no second parse.
///
/// A `&raw` is genuine raw-borrow syntax only when `const` or `mut` follows it;
/// anything else — `)`, `,`, `.`, `[`, `;`, or end of input — means `raw` is an
/// identifier being borrowed.
fn neutralise_raw_refs(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    let mut index = 0;
    let mut changed = false;
    while index < bytes.len() {
        if bytes[index] == b'&' && source[index + 1..].starts_with("raw") {
            let after = index + 4;
            let word_end = after >= bytes.len() || !is_ident_byte(bytes[after]);
            if word_end
                && !next_word_is(&source[after..], "const")
                && !next_word_is(&source[after..], "mut")
            {
                out.push_str(&source[copied..index]);
                out.push_str("&rawx");
                copied = after;
                index = after;
                changed = true;
                continue;
            }
        }
        index += 1;
    }
    if !changed {
        return None;
    }
    out.push_str(&source[copied..]);
    Some(out)
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether `rest`, after leading whitespace, begins with `word` as a whole word.
fn next_word_is(rest: &str, word: &str) -> bool {
    let trimmed = rest.trim_start_matches(|c: char| c.is_whitespace());
    trimmed
        .strip_prefix(word)
        .is_some_and(|tail| !tail.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
}

fn push_site(name_node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let Some(name) = text(name_node, source) else {
        return;
    };
    let point = name_node.start_position();
    // Tree-sitter columns are UTF-8 bytes. Definition looks up a UTF-8 offset,
    // and the fixtures are ASCII, where that matches an LSP character.
    out.push(crate::ExportSite {
        name,
        line: u32::try_from(point.row)
            .unwrap_or(u32::MAX)
            .saturating_add(1),
        character: u32::try_from(point.column).unwrap_or(u32::MAX),
    });
}

fn text(node: Node, source: &[u8]) -> Option<String> {
    node.utf8_text(source).ok().map(str::to_owned)
}
fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

/// Collects every `use_declaration` and body-less `mod_item`, remembering the
/// inline `mod`s each one sits inside. Walking the tree is what makes a `use`
/// inside a comment, a string, or a raw-string fixture invisible: it never
/// becomes a node. `mods` is the inline-module stack, outermost first.
fn collect_rust_imports(
    node: Node,
    source: &[u8],
    mods: &mut Vec<String>,
    out: &mut Vec<RustImport>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "use_declaration" => {
                if let Some(argument) = child.child_by_field_name("argument") {
                    let mut prefix = Vec::new();
                    flatten_use(argument, source, &mut prefix, mods, out);
                }
            }
            "mod_item" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|name| text(name, source));
                if child.child_by_field_name("body").is_some() {
                    if let Some(name) = name {
                        mods.push(name);
                        collect_rust_imports(child, source, mods, out);
                        mods.pop();
                    }
                } else if let Some(name) = name {
                    out.push(RustImport {
                        segments: vec![name],
                        mods: mods.clone(),
                        is_mod: true,
                    });
                }
            }
            _ => collect_rust_imports(child, source, mods, out),
        }
    }
}

/// Flattens one `use` argument into the full paths it names: `a::{b, c as d}`
/// becomes `a::b` and `a::c`, a glob keeps its `*` tail.
fn flatten_use(
    node: Node,
    source: &[u8],
    prefix: &mut Vec<String>,
    mods: &[String],
    out: &mut Vec<RustImport>,
) {
    match node.kind() {
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                flatten_use(child, source, prefix, mods, out);
            }
        }
        "scoped_use_list" => {
            let saved = prefix.len();
            if let Some(path) = node.child_by_field_name("path") {
                push_path(path, source, prefix);
            }
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use(list, source, prefix, mods, out);
            }
            prefix.truncate(saved);
        }
        "use_as_clause" => {
            // The alias renames the path for the caller; it does not change
            // which file the path names.
            if let Some(path) = node.child_by_field_name("path") {
                flatten_use(path, source, prefix, mods, out);
            }
        }
        "use_wildcard" => {
            let saved = prefix.len();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                push_path(child, source, prefix);
            }
            prefix.push("*".to_owned());
            out.push(RustImport {
                segments: prefix.clone(),
                mods: mods.to_vec(),
                is_mod: false,
            });
            prefix.truncate(saved);
        }
        _ => {
            let saved = prefix.len();
            push_path(node, source, prefix);
            out.push(RustImport {
                segments: prefix.clone(),
                mods: mods.to_vec(),
                is_mod: false,
            });
            prefix.truncate(saved);
        }
    }
}

/// Appends the segments of one path node (`crate`, `super`, `self`, `a::b`).
fn push_path(node: Node, source: &[u8], prefix: &mut Vec<String>) {
    if node.kind() == "scoped_identifier" {
        if let Some(path) = node.child_by_field_name("path") {
            push_path(path, source, prefix);
        }
        if let Some(name) = node.child_by_field_name("name")
            && let Some(segment) = text(name, source)
        {
            prefix.push(segment);
        }
        return;
    }
    if let Some(segment) = text(node, source) {
        prefix.push(segment);
    }
}

/// Rust: everything reachable from outside the module, so `pub` items at any
/// nesting — including inherent and trait methods, which is what a caller
/// actually names. Trait members inherit the trait's visibility.
fn rust_items(node: Node, source: &[u8], inherited: bool, out: &mut Vec<crate::ExportSite>) {
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
                if public {
                    push_field(child, source, out);
                }
            }
            "mod_item" => {
                if public {
                    push_field(child, source, out);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    rust_items(body, source, false, out);
                }
            }
            "trait_item" => {
                if public {
                    push_field(child, source, out);
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

fn push_field(item: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    if let Some(name) = item.child_by_field_name("name") {
        push_site(name, source, out);
    }
}

/// TypeScript and JavaScript: what an `export` publishes, plus the public
/// members of an exported class — `client.send(…)` names a symbol the file
/// owns just as much as the class does.
fn ts_module(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "export_statement" {
            ts_export(child, source, out);
        }
    }
}

fn ts_export(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
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
                    if let Some(name) = name {
                        push_site(name, source, out);
                    }
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                push_field(child, source, out);
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
                push_field(child, source, out);
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
                    if name.kind() == "identifier" {
                        push_site(name, source, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn ts_class_members(body: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
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
        if text(name, source).is_some_and(|name| name != "constructor") {
            push_site(name, source, out);
        }
    }
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
