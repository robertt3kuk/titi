//! Structural edits over the same syntax trees the genome parses.
//!
//! A line-based or regex-based edit has to guess: `fn run` matches the string
//! literal that mentions it, the commented-out copy above it, and the doc
//! example in the module header just as happily as the declaration. Here the
//! span comes out of the grammar, so a target either resolves to exactly one
//! declaration node or the edit is refused and nothing changes.
//!
//! The languages are the genome's: Rust, TypeScript/JavaScript, Python.
//! Anything else is [`AstEditError::UnsupportedLanguage`] — guessing with a
//! foreign grammar would be worse than refusing.
//!
//! This is a library capability. Input is `(path, source, target, replacement)`
//! and output is the new source; nothing here touches the filesystem.
//!
//! Spec: `docs/PLAN.md` (P4-4).

use tree_sitter::Node;

use crate::symbols::{Grammar, parse};

/// Which family of declaration a target names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// Rust `fn` (free, inherent, trait) · TS/JS `function` declarations and
    /// class methods · Python `def`, including methods.
    Function,
    /// Rust `struct`/`enum`/`union`/`trait`/`type` · TS/JS
    /// `class`/`interface`/`enum`/`type` · Python `class`.
    Type,
}

impl ItemKind {
    fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Type => "type",
        }
    }
}

/// Which part of the matched declaration the edit replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The declaration node itself: signature and body.
    ///
    /// What wraps the declaration stays where it is: a Rust `#[test]`, a
    /// Python `@decorator`, a TypeScript `export` are separate nodes, so
    /// replacing `build` in `export function build()` keeps the `export`.
    Whole,
    /// Only the body: a Rust/TS block, a class or field body, a Python suite.
    /// Declarations without one (a trait method signature, an `interface`
    /// method) are [`AstEditError::NoBody`].
    Body,
}

/// A declaration to edit, named the way a caller names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: ItemKind,
    pub name: String,
    pub part: Part,
}

impl Target {
    /// The whole declaration of function `name`.
    pub fn function(name: impl Into<String>) -> Self {
        Self {
            kind: ItemKind::Function,
            name: name.into(),
            part: Part::Whole,
        }
    }

    /// The body of function `name`, signature untouched.
    pub fn function_body(name: impl Into<String>) -> Self {
        Self {
            kind: ItemKind::Function,
            name: name.into(),
            part: Part::Body,
        }
    }

    /// The whole declaration of type `name`.
    pub fn type_item(name: impl Into<String>) -> Self {
        Self {
            kind: ItemKind::Type,
            name: name.into(),
            part: Part::Whole,
        }
    }

    /// The body of type `name`: a class body, a struct's fields, an enum's
    /// variants.
    pub fn type_body(name: impl Into<String>) -> Self {
        Self {
            kind: ItemKind::Type,
            name: name.into(),
            part: Part::Body,
        }
    }
}

/// Where a resolved target sits in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first byte, inclusive.
    pub start: usize,
    /// Byte offset one past the last byte.
    pub end: usize,
    /// 1-based line the span starts on.
    pub start_line: usize,
    /// 1-based line the span ends on.
    pub end_line: usize,
}

/// Why an edit was refused. Every variant means the source is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AstEditError {
    #[error("ast-edit does not support `{path}`: it parses Rust, TypeScript/JavaScript and Python")]
    UnsupportedLanguage { path: String },

    /// The grammar refused the file, or the tree came back with syntax errors.
    /// Editing a tree that does not describe the file is how a structural edit
    /// corrupts one.
    #[error("`{path}` does not parse cleanly; refusing to edit a broken tree")]
    Unparsable { path: String },

    #[error("no {kind} named `{name}` in `{path}`")]
    NotFound {
        path: String,
        kind: &'static str,
        name: String,
    },

    /// More than one declaration answers to the name. Picking one would be a
    /// coin flip, so the caller has to narrow the target itself.
    #[error("`{name}` is ambiguous in `{path}`: {kind} declared on lines {lines:?}")]
    Ambiguous {
        path: String,
        kind: &'static str,
        name: String,
        /// 1-based start line of every match, in source order.
        lines: Vec<usize>,
    },

    #[error("{kind} `{name}` in `{path}` has no body to replace")]
    NoBody {
        path: String,
        kind: &'static str,
        name: String,
    },

    /// The splice produced a file that no longer parses. The replacement, not
    /// the target, is at fault; the original source is returned untouched.
    #[error("the replacement would leave `{path}` unparsable")]
    BrokenResult { path: String },
}

impl AstEditError {
    fn ambiguous(matches: &[Node], path: &str, target: &Target) -> Self {
        Self::Ambiguous {
            path: path.to_owned(),
            kind: target.kind.label(),
            name: target.name.clone(),
            lines: matches
                .iter()
                .map(|node| node.start_position().row + 1)
                .collect(),
        }
    }
}

/// The span `target` resolves to in `source`, without editing anything.
///
/// Errors exactly as [`apply`] does, minus [`AstEditError::BrokenResult`].
pub fn find(path: &str, source: &str, target: &Target) -> Result<Span, AstEditError> {
    let grammar = Grammar::from_path(path).ok_or_else(|| AstEditError::UnsupportedLanguage {
        path: path.to_owned(),
    })?;
    let tree = parse(grammar, source).ok_or_else(|| AstEditError::Unparsable {
        path: path.to_owned(),
    })?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(AstEditError::Unparsable {
            path: path.to_owned(),
        });
    }

    let mut matches = Vec::new();
    collect(root, source.as_bytes(), grammar, target, &mut matches);
    let node = match matches.as_slice() {
        [] => {
            return Err(AstEditError::NotFound {
                path: path.to_owned(),
                kind: target.kind.label(),
                name: target.name.clone(),
            });
        }
        [only] => *only,
        several => return Err(AstEditError::ambiguous(several, path, target)),
    };

    let node = match target.part {
        Part::Whole => node,
        Part::Body => node
            .child_by_field_name("body")
            .ok_or_else(|| AstEditError::NoBody {
                path: path.to_owned(),
                kind: target.kind.label(),
                name: target.name.clone(),
            })?,
    };
    Ok(Span {
        start: node.start_byte(),
        end: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    })
}

/// `source` with the span `target` resolves to replaced by `replacement`.
///
/// `replacement` is written verbatim: indentation and any delimiters the part
/// needs (the braces of a [`Part::Body`] block) belong to the caller. The
/// result is re-parsed and refused if the splice broke the file.
pub fn apply(
    path: &str,
    source: &str,
    target: &Target,
    replacement: &str,
) -> Result<String, AstEditError> {
    let span = find(path, source, target)?;
    // `find` resolved the span from this very tree, so both ends sit on node
    // boundaries and therefore on char boundaries; `get` keeps that an error
    // rather than a panic if it ever stops holding.
    let (before, after) = source
        .get(..span.start)
        .zip(source.get(span.end..))
        .ok_or_else(|| AstEditError::Unparsable {
            path: path.to_owned(),
        })?;
    let mut edited = String::with_capacity(before.len() + replacement.len() + after.len());
    edited.push_str(before);
    edited.push_str(replacement);
    edited.push_str(after);

    let grammar = Grammar::from_path(path).ok_or_else(|| AstEditError::UnsupportedLanguage {
        path: path.to_owned(),
    })?;
    let reparsed = parse(grammar, &edited).ok_or_else(|| AstEditError::BrokenResult {
        path: path.to_owned(),
    })?;
    if reparsed.root_node().has_error() {
        return Err(AstEditError::BrokenResult {
            path: path.to_owned(),
        });
    }
    Ok(edited)
}

/// Every declaration node in the tree that answers to `target`, in source
/// order. Nesting is included: a method on a class and a free function are
/// both reachable by name, which is also what makes the two of them ambiguous.
fn collect<'tree>(
    node: Node<'tree>,
    source: &[u8],
    grammar: Grammar,
    target: &Target,
    out: &mut Vec<Node<'tree>>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if declares(child, source, grammar, target) {
            out.push(child);
        }
        collect(child, source, grammar, target, out);
    }
}

fn declares(node: Node, source: &[u8], grammar: Grammar, target: &Target) -> bool {
    if !kinds(grammar, target.kind).contains(&node.kind()) {
        return false;
    }
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source).ok())
        .is_some_and(|name| name == target.name)
}

/// The node kinds each grammar uses for a kind of declaration.
fn kinds(grammar: Grammar, kind: ItemKind) -> &'static [&'static str] {
    match (grammar, kind) {
        (Grammar::Rust, ItemKind::Function) => &["function_item", "function_signature_item"],
        (Grammar::Rust, ItemKind::Type) => &[
            "struct_item",
            "enum_item",
            "union_item",
            "trait_item",
            "type_item",
        ],
        (Grammar::TypeScript | Grammar::Tsx, ItemKind::Function) => &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "method_signature",
        ],
        (Grammar::TypeScript | Grammar::Tsx, ItemKind::Type) => &[
            "class_declaration",
            "abstract_class_declaration",
            "interface_declaration",
            "enum_declaration",
            "type_alias_declaration",
        ],
        (Grammar::Python, ItemKind::Function) => &["function_definition"],
        (Grammar::Python, ItemKind::Type) => &["class_definition"],
    }
}
