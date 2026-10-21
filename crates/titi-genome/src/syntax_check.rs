//! What one edit did to a file's syntax.
//!
//! The index already counts a file's syntax errors, but only when it re-reads
//! the file, and only as a number on the file's record. This answers the
//! question an editing tool has *at the moment of the write*: given the text
//! before and the text after, did the edit introduce errors or repair some?
//! A file that was already broken and stays equally broken answers nothing, so
//! an edit is never blamed for what it did not do.
//!
//! An error is an `ERROR` node or a `MISSING` one — the grammar's repair for a
//! brace it expected and did not find. Nothing here touches the filesystem.

use std::collections::HashMap;
use std::fmt;

use tree_sitter::{Node, TreeCursor};

use crate::lang::grammar_for;
use crate::symbols::parse;

/// The largest text on either side of an edit that is still parsed: 1 MiB.
/// Past it the check costs more than a normal edit should, and a file that
/// size is generated or data, not something a model is steering by syntax.
pub const MAX_BYTES: usize = 1 << 20;

/// Issues kept per file for naming one; the count is exact regardless.
const KEPT_ISSUES: usize = 32;

/// Widest token an issue's message quotes.
const TOKEN_CHARS: usize = 24;

/// One place the grammar could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxIssue {
    /// 1-based.
    pub line: usize,
    /// `unexpected '}'` or `missing '}'`.
    pub message: String,
}

/// What an edit changed about a file's syntax errors. Absent when it changed
/// nothing, so a caller prints a line only for these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxDelta {
    /// The file has `count` more errors than before. `first` is the earliest
    /// one the old text did not already have.
    Introduced {
        count: u32,
        first: Option<SyntaxIssue>,
    },
    /// The file has `count` fewer errors than before.
    Fixed { count: u32 },
}

impl fmt::Display for SyntaxDelta {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plural = |count: u32| if count == 1 { "error" } else { "errors" };
        match self {
            Self::Introduced { count, first } => {
                write!(
                    out,
                    "syntax: this edit introduced {count} {}",
                    plural(*count)
                )?;
                if let Some(issue) = first {
                    write!(out, " (line {}: {})", issue.line, issue.message)?;
                }
                Ok(())
            }
            Self::Fixed { count } => write!(out, "syntax: fixed {count} {}", plural(*count)),
        }
    }
}

/// How the syntax errors of `path` changed between `before` and `after`.
///
/// `None` when nothing changed, when no grammar reads `path`, when either text
/// is past [`MAX_BYTES`], or when the parser refused one outright — in each
/// case there is nothing true to say.
pub fn delta(path: &str, before: &str, after: &str) -> Option<SyntaxDelta> {
    if before.len() > MAX_BYTES || after.len() > MAX_BYTES {
        return None;
    }
    let grammar = grammar_for(path)?;
    let old = scan(&parse(grammar, before)?.root_node(), before);
    let new = scan(&parse(grammar, after)?.root_node(), after);
    match new.count.cmp(&old.count) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => Some(SyntaxDelta::Fixed {
            count: old.count - new.count,
        }),
        std::cmp::Ordering::Greater => Some(SyntaxDelta::Introduced {
            count: new.count - old.count,
            first: first_new(&old, new),
        }),
    }
}

#[derive(Default)]
struct Scan {
    count: u32,
    issues: Vec<SyntaxIssue>,
}

/// The earliest issue of `new` that `old` does not account for. Matching is by
/// message, so a line that merely moved is not mistaken for a new error; when
/// every message was already there, the earliest issue stands in.
fn first_new(old: &Scan, new: Scan) -> Option<SyntaxIssue> {
    let mut known: HashMap<&str, u32> = HashMap::new();
    for issue in &old.issues {
        *known.entry(issue.message.as_str()).or_default() += 1;
    }
    let mut fallback = None;
    for issue in new.issues {
        match known.get_mut(issue.message.as_str()) {
            Some(left) if *left > 0 => *left -= 1,
            _ => return Some(issue),
        }
        fallback.get_or_insert(issue);
    }
    fallback
}

/// Every error in the tree. A subtree the grammar marks clean is not entered,
/// so a healthy file costs one flag read after the parse.
fn scan(root: &Node, source: &str) -> Scan {
    let mut found = Scan::default();
    if !root.has_error() {
        return found;
    }
    let lines = line_count(source);
    let mut cursor = root.walk();
    walk(&mut cursor, source, lines, &mut found);
    found
}

fn walk(cursor: &mut TreeCursor, source: &str, lines: usize, found: &mut Scan) {
    // Iterative on purpose: a minified bundle nests deeper than a thread's
    // stack allows a recursive walk to go.
    loop {
        let node = cursor.node();
        let counted = node.is_error() || node.is_missing();
        if counted {
            found.count = found.count.saturating_add(1);
            if found.issues.len() < KEPT_ISSUES {
                found.issues.push(issue(&node, source, lines));
            }
        }
        // An error node is one region however much it swallowed.
        if !counted && node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn issue(node: &Node, source: &str, lines: usize) -> SyntaxIssue {
    // A missing token at the end of the file sits on the line after the last
    // one; the last real line is the one a person can go to.
    let line = (node.start_position().row + 1).min(lines.max(1));
    let message = if node.is_missing() {
        format!("missing '{}'", node.kind())
    } else {
        match first_token(node, source) {
            Some(token) => format!("unexpected '{token}'"),
            None => "syntax error".to_owned(),
        }
    };
    SyntaxIssue { line, message }
}

/// The first token inside an error region: the thing the parser tripped on.
fn first_token(node: &Node, source: &str) -> Option<String> {
    let mut leaf = *node;
    while let Some(child) = leaf.child(0) {
        leaf = child;
    }
    let text = source.get(leaf.start_byte()..leaf.end_byte())?.trim();
    let text = text.lines().next()?.trim();
    if text.is_empty() {
        return None;
    }
    let mut chars = text.chars();
    let mut token: String = chars.by_ref().take(TOKEN_CHARS).collect();
    if chars.next().is_some() {
        token.push('…');
    }
    Some(token)
}

fn line_count(source: &str) -> usize {
    let newlines = source.bytes().filter(|byte| *byte == b'\n').count();
    if source.is_empty() || source.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}
