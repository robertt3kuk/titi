//! Rust: exports and imports read off the grammar's own nodes.
//!
//! Every name here is a node the grammar produced, so a commented-out `fn` or
//! a `use` inside a string literal is not a declaration and not an import.
//! Resolution is separate and stays here: it needs the repository's file set
//! (a module index, not a syntax tree), and it is the one place that knows
//! that `crate::`, `self::`, `super::` and `mod x;` name files of this
//! workspace.

use std::collections::HashSet;

use tree_sitter::Node;

use super::ParsedFile;
use super::support::{finish, first_known, internal, join, parent, record};
use crate::symbols::{self, Grammar, has_child_kind, push_field, text};
use crate::{Candidate, UnresolvedImport};

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

/// Rust imports come from the syntax tree, not from a line regex. A `use` in a comment, in a string literal, or in the raw-string
/// fixture an integration test writes is not an import; a braced
/// `use crate::{a, b}` is two paths, not the truncated `crate::` a `[^;{]+`
/// capture leaves behind. Resolution stays here: it needs the repo's file set,
/// which is a module index, not a syntax tree.
pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let Some(tree) = symbols::parse(Grammar::Rust, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    rust_items(tree.root_node(), bytes, false, &mut sites);
    let mut rust_imports = Vec::new();
    let mut mods = Vec::new();
    collect_rust_imports(tree.root_node(), bytes, &mut mods, &mut rust_imports);
    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    for item in &rust_imports {
        // A bare path is an external crate or a std path — `serde_json::json`,
        // `std::collections::HashMap` — not a workspace lookup. Only the three
        // crate/super/self prefixes name a file in this repo, exactly as
        // before; a `mod x;` names a child of the module that declares it.
        if !item.is_mod
            && !matches!(
                item.segments.first().map(String::as_str),
                Some("crate" | "self" | "super")
            )
        {
            continue;
        }
        let candidates = rust_import_candidates(path, item, files);
        if first_known(&candidates, files).as_deref() == Some(path) {
            // `use super::*` inside an inline `mod` names the file's own
            // module. That is a self-edge, not a dependency, and reporting it
            // unresolved would claim a problem with a file that has none.
            continue;
        }
        record(
            &item.segments.join("::"),
            internal(candidates),
            files,
            &mut imports,
            &mut unresolved,
        );
    }
    finish(
        source,
        sites,
        imports,
        unresolved,
        rust_errors(&tree, source),
    )
}

/// Tree-sitter `ERROR` nodes, with the pinned grammar's `&raw` token ambiguity
/// neutralised: see [`neutralise_raw_refs`]. A count that only the ambiguity
/// produced is not a claim this crate can make about the file, and a file that
/// genuinely does not parse still counts.
fn rust_errors(tree: &tree_sitter::Tree, source: &str) -> u32 {
    let errors = symbols::error_count(tree.root_node());
    if errors == 0 {
        return 0;
    }
    let Some(scrubbed) = neutralise_raw_refs(source) else {
        return errors;
    };
    let Some(tree) = symbols::parse(Grammar::Rust, &scrubbed) else {
        return errors;
    };
    symbols::error_count(tree.root_node()).min(errors)
}

/// Where one import lands, at file granularity.
///
/// `crate::` starts at the crate root's module directory, `self::` at the
/// module the declaration sits in (its file plus every enclosing inline `mod`),
/// `super::` at that module's parent. A bare path (`use regex::Regex`) is an
/// external crate or a std path, exactly as before: only those three prefixes
/// are workspace lookups. A `mod x;` is a child of the module that declares it.
fn rust_import_candidates(
    from: &str,
    item: &RustImport,
    files: &HashSet<String>,
) -> Vec<Candidate> {
    let mut dir = module_children_dir(from);
    for name in &item.mods {
        dir = join(&dir, name);
    }
    if item.is_mod {
        let Some(name) = item.segments.first() else {
            return Vec::new();
        };
        return module_path_candidates(&dir, &[name]);
    }
    let Some(first) = item.segments.first().map(String::as_str) else {
        return Vec::new();
    };
    let mut rest = &item.segments[1..];
    match first {
        "crate" => dir = crate_root(from, files),
        "self" => {}
        "super" => {
            // The `super` consumed by the slice above is the first step up.
            let mut depth = 1;
            while rest.first().map(String::as_str) == Some("super") {
                depth += 1;
                rest = &rest[1..];
            }
            for _ in 0..depth {
                let Some(up) = parent(&dir) else {
                    return Vec::new();
                };
                dir = up.to_owned();
            }
        }
        _ => return Vec::new(),
    }
    let segments: Vec<&str> = rest.iter().map(String::as_str).collect();
    use_path_candidates(&dir, &segments)
}

/// The directory a Rust module's child modules live in. `foo/mod.rs` and a
/// crate root keep their own directory; `foo.rs` defines the module `foo`,
/// whose children live in `foo/`.
fn module_children_dir(path: &str) -> String {
    let name = path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path);
    if matches!(name, "lib.rs" | "main.rs" | "mod.rs") {
        return parent(path).unwrap_or("").to_owned();
    }
    match path.strip_suffix(".rs") {
        Some(stem) => stem.to_owned(),
        None => parent(path).unwrap_or("").to_owned(),
    }
}

/// The files that can define the module whose children live in `dir`.
fn module_file_candidates(dir: &str) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = ["mod.rs", "lib.rs", "main.rs"]
        .into_iter()
        .map(|name| Candidate::Exact(join(dir, name)))
        .collect();
    candidates.push(Candidate::Exact(format!("{dir}.rs")));
    candidates
}

/// The files a path whose every segment must name a module can land on. The
/// empty path is the module that lives in `dir` itself.
fn module_path_candidates(dir: &str, segments: &[&str]) -> Vec<Candidate> {
    if segments.is_empty() {
        return module_file_candidates(dir);
    }
    let joined = segments.join("/");
    let rel = if dir.is_empty() {
        joined
    } else {
        format!("{dir}/{joined}")
    };
    vec![
        Candidate::Exact(format!("{rel}.rs")),
        Candidate::Exact(format!("{rel}/mod.rs")),
    ]
}

/// The files a `use` path can land on: the longest prefix that names a module
/// decides the file, so only the trailing segments may be items. A one-segment
/// path may be an item of `dir`'s own module (`use crate::Genome` at the crate
/// root), which no prefix can name as a file, so that module's own files are
/// the last resort; a longer path whose module prefix is missing
/// (`use crate::missing::Thing`) names none of them.
fn use_path_candidates(dir: &str, segments: &[&str]) -> Vec<Candidate> {
    if segments.is_empty() {
        return module_file_candidates(dir);
    }
    if segments.last() == Some(&"*") {
        return module_path_candidates(dir, &segments[..segments.len() - 1]);
    }
    let mut candidates = Vec::new();
    for take in (1..=segments.len()).rev() {
        candidates.extend(module_path_candidates(dir, &segments[..take]));
    }
    if segments.len() == 1 {
        candidates.extend(module_file_candidates(dir));
    }
    candidates
}

fn crate_root(from: &str, files: &HashSet<String>) -> String {
    let mut dir = parent(from).unwrap_or("").to_owned();
    loop {
        if files.contains(&join(&dir, "lib.rs")) || files.contains(&join(&dir, "main.rs")) {
            return dir;
        }
        match parent(&dir) {
            Some(parent_dir) => dir = parent_dir.to_owned(),
            None => return parent(from).unwrap_or("").to_owned(),
        }
    }
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

#[cfg(test)]
mod tests {
    use crate::lang::test_support::exports;

    fn names(path: &str, source: &str) -> Vec<String> {
        exports(path, source).unwrap_or_else(|| panic!("{path} parses"))
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
        // Names come back sorted (that is what the ranker keys off); the
        // source order lives in the export sites.
        assert_eq!(names, vec!["SNIPPET".to_owned(), "real".to_owned()]);
    }

    /// A half-written file is the normal state of a file being edited: the
    /// parser recovers and the symbols before the damage still land.
    #[test]
    fn a_file_with_a_syntax_error_still_yields_what_parsed() {
        let names = names("src/lib.rs", "pub fn first() {}\npub fn second( {\n");
        assert!(names.contains(&"first".to_owned()), "{names:?}");
    }
}
