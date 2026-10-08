//! Which tree-sitter grammars this build links, and the node plumbing every
//! walker over one uses.
//!
//! This module knows nothing about a specific language: it hands out a parser
//! per grammar and counts the errors in the tree it produced. The walkers that
//! turn a tree into symbols live with the language that owns them, under
//! `lang/`, so a language's whole story — its grammar, its level and its
//! walker — is in one place.
//!
//! **The grammars are pinned to ABI 14**, the range the workspace's
//! tree-sitter accepts; newer grammar releases emit ABI 15 and need the core
//! bumped first, which is a separate decision. `grammars_match_the_core_abi`
//! below fails if a grammar in this build cannot be loaded, so an unusable
//! grammar can never silently empty the symbol map.

use std::cell::RefCell;

use tree_sitter::{Node, Parser};

/// A grammar this crate links. One parser per grammar is kept per thread:
/// `Parser::set_language` is the expensive part and it never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grammar {
    Rust,
    /// The TypeScript grammar: `.ts`, `.mts`, `.cts`.
    TypeScript,
    /// The TSX grammar, which also accepts plain JavaScript and JSX.
    Tsx,
    Python,
    Go,
    Java,
    C,
    Cpp,
}

impl Grammar {
    fn language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        }
    }
}

#[derive(Default)]
struct Parsers {
    rust: Option<Parser>,
    typescript: Option<Parser>,
    tsx: Option<Parser>,
    python: Option<Parser>,
    go: Option<Parser>,
    java: Option<Parser>,
    c: Option<Parser>,
    cpp: Option<Parser>,
}

impl Parsers {
    fn slot(&mut self, grammar: Grammar) -> &mut Option<Parser> {
        match grammar {
            Grammar::Rust => &mut self.rust,
            Grammar::TypeScript => &mut self.typescript,
            Grammar::Tsx => &mut self.tsx,
            Grammar::Python => &mut self.python,
            Grammar::Go => &mut self.go,
            Grammar::Java => &mut self.java,
            Grammar::C => &mut self.c,
            Grammar::Cpp => &mut self.cpp,
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

/// How many `ERROR` nodes the tree carries, at any depth.
pub(crate) fn error_count(node: Node) -> u32 {
    let mut count = u32::from(node.is_error());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count = count.saturating_add(error_count(child));
    }
    count
}

/// Records a declaration's name at the site the grammar put it.
pub(crate) fn push_site(name_node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
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

pub(crate) fn text(node: Node, source: &[u8]) -> Option<String> {
    node.utf8_text(source).ok().map(str::to_owned)
}

/// Records the name field of a declaration node (`function_item`, `class_declaration`,
/// …): the shared half of every grammar-backed walker.
pub(crate) fn push_field(item: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    if let Some(name) = item.child_by_field_name("name") {
        push_site(name, source, out);
    }
}

pub(crate) fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard behind the pinned grammar versions: a grammar whose ABI the
    /// linked tree-sitter rejects would empty the map silently.
    #[test]
    fn grammars_match_the_core_abi() {
        for grammar in [
            Grammar::Rust,
            Grammar::TypeScript,
            Grammar::Tsx,
            Grammar::Python,
            Grammar::Go,
            Grammar::Java,
            Grammar::C,
            Grammar::Cpp,
        ] {
            assert!(grammar_available(grammar), "{grammar:?} failed to load");
        }
    }
}
