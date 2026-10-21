//! The plumbing the per-language parsers share.
//!
//! None of this knows a language: `finish` closes a [`ParsedFile`] off,
//! `mask_comments` blanks the comments in a text a pattern will read, and the
//! `resolve_*` family turns a module specifier into a workspace path. A parser
//! module owns everything that is specific to its language.

use std::collections::HashSet;

use crate::refs;
use crate::{Candidate, UnresolvedImport};

use tree_sitter::Node;

use super::ParsedFile;
use crate::symbol::{SymbolKind, SymbolSite, one_line_signature};

/// The first candidate `files` can satisfy, in the order it was emitted.
///
/// This is the half of resolution that needs the known-file set; the candidate
/// list itself does not, which is what makes the question re-askable (see
/// [`Candidate`]). `None` means every candidate named a file this workspace
/// does not have.
pub(crate) fn first_known(candidates: &[Candidate], files: &HashSet<String>) -> Option<String> {
    candidates.iter().find_map(|candidate| match candidate {
        Candidate::Exact(path) => files.contains(path).then(|| path.clone()),
        Candidate::Suffix(path) => {
            if files.contains(path) {
                return Some(path.clone());
            }
            // The file may live under a root prefix the specifier omitted.
            // Several files can end the same way (`commonMain/…/Buffer.kt` and
            // `jvmMain/…/Buffer.kt`); a hash set yields them in an order that
            // changes from run to run, so the pick is by the data: the
            // shallowest path, then the smallest.
            let needle = format!("/{path}");
            files
                .iter()
                .filter(|known| known.ends_with(&needle))
                .min_by(|a, b| {
                    a.matches('/')
                        .count()
                        .cmp(&b.matches('/').count())
                        .then_with(|| a.cmp(b))
                })
                .cloned()
        }
    })
}

/// Sorts, dedups and attaches the identifier set every parser ends up needing.
///
/// Export *names* stay sorted, which is what the ranker keys off. Sites keep
/// source order so a definition query can point at the declaration.
pub(crate) fn finish(
    source: &str,
    mut sites: Vec<crate::ExportSite>,
    mut imports: Vec<String>,
    mut unresolved: Vec<UnresolvedImport>,
    syntax_errors: u32,
) -> ParsedFile {
    let mut seen = HashSet::new();
    sites.retain(|site| seen.insert(site.name.clone()));
    let mut exports: Vec<String> = sites.iter().map(|site| site.name.clone()).collect();
    exports.sort();
    exports.dedup();
    imports.sort();
    imports.dedup();
    unresolved.sort_by(|left, right| left.spec.cmp(&right.spec));
    unresolved
        .dedup_by(|left, right| left.spec == right.spec && left.candidates == right.candidates);
    let unresolved_imports = specs(&unresolved);
    let refs = refs::collect_refs(source, &exports);
    ParsedFile {
        exports,
        export_sites: sites,
        imports,
        unresolved_imports,
        unresolved_candidates: unresolved,
        refs,
        syntax_errors,
        ..ParsedFile::default()
    }
}

/// The distinct specifiers of `unresolved`, which is what a diagnostic quotes.
fn specs(unresolved: &[UnresolvedImport]) -> Vec<String> {
    let mut specs: Vec<String> = unresolved
        .iter()
        .map(|import| import.spec.clone())
        .collect();
    specs.sort();
    specs.dedup();
    specs
}

/// Where an import's specifier landed, or the paths it would have to land on.
///
/// The distinction is the honest part of a heuristic import: a specifier that
/// names something outside the workspace (`java.util.List`, `Foundation`,
/// `fmt`) is not a missing file, and reporting it as one would put a warning
/// on every file that uses a library. Only a miss in the first variant becomes
/// a diagnostic.
pub(crate) enum Placement {
    /// Workspace-shaped: a specifier of this workspace's own shape that names
    /// no known file is a missing file, and a warning.
    Candidates(Vec<Candidate>),
    /// Any file this names belongs to the workspace, and a miss belongs to
    /// the outside world: a bare specifier that resolves to nothing is a
    /// dependency (`node_modules`, a gem), not a missing file. A miss is
    /// dropped, and nothing is kept, because there was nothing to diagnose.
    Optional(Vec<Candidate>),
    /// Not this workspace's business at all, with no path of its own to try.
    External,
}

pub(crate) fn record(
    spec: &str,
    placement: Placement,
    files: &HashSet<String>,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<UnresolvedImport>,
) {
    if spec.is_empty() {
        return;
    }
    match placement {
        Placement::Candidates(candidates) => match first_known(&candidates, files) {
            Some(path) => imports.push(path),
            None => unresolved.push(UnresolvedImport {
                spec: spec.to_owned(),
                candidates,
            }),
        },
        Placement::Optional(candidates) => {
            if let Some(path) = first_known(&candidates, files) {
                imports.push(path);
            }
        }
        Placement::External => {}
    }
}

/// The placement of a specifier this language resolves itself, where there is
/// no outside world to speak of: `use super::x` in Rust, a `require_relative`
/// in Ruby. An empty candidate list means the specifier named nothing this
/// workspace could have.
pub(crate) fn internal(candidates: Vec<Candidate>) -> Placement {
    Placement::Candidates(candidates)
}

/// Resolves a module/package path to a known file by trying the path itself
/// and then every trailing suffix of it. That covers Go and Ruby, where the
/// import string is module-qualified and the repo has no module index yet.
pub(crate) fn resolve_suffix(spec: &str, exts: &[&str]) -> Vec<Candidate> {
    let spec = spec.trim_matches('/');
    if spec.is_empty() {
        return Vec::new();
    }
    let segments: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
    let mut candidates = Vec::new();
    // Longest suffix first: the most specific match wins.
    for start in 0..segments.len() {
        let candidate = segments[start..].join("/");
        for ext in exts {
            for path in [
                format!("{candidate}.{ext}"),
                format!("{candidate}/index.{ext}"),
                format!("{candidate}/mod.{ext}"),
                format!("{candidate}/__init__.{ext}"),
            ] {
                candidates.push(Candidate::Exact(path.clone()));
                // The file may live under a root prefix the import omitted.
                candidates.push(Candidate::Suffix(path));
            }
        }
    }
    candidates
}

pub(crate) fn resolve_from_dir(dir: &str, segs: &[&str]) -> Vec<Candidate> {
    if segs.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = segs
        .iter()
        .copied()
        .filter(|seg| !seg.is_empty() && *seg != "*")
        .collect();
    let mut candidates = Vec::new();
    while !parts.is_empty() {
        let rel = if dir.is_empty() {
            parts.join("/")
        } else {
            format!("{dir}/{}", parts.join("/"))
        };
        for candidate in [
            format!("{rel}.rs"),
            format!("{rel}/mod.rs"),
            format!("{rel}.ts"),
            format!("{rel}.tsx"),
            format!("{rel}.js"),
            format!("{rel}/index.ts"),
            format!("{rel}.py"),
            format!("{rel}/__init__.py"),
        ] {
            candidates.push(Candidate::Exact(candidate));
        }
        parts.pop();
    }
    candidates
}

pub(crate) fn resolve_relative(from: &str, spec: &str, exts: &[&str]) -> Vec<Candidate> {
    let from_dir = parent(from).unwrap_or("");
    let Some(joined) = normalize_join(from_dir, spec) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for ext in exts {
        candidates.push(Candidate::Exact(format!("{joined}.{ext}")));
        candidates.push(Candidate::Exact(format!("{joined}/index.{ext}")));
    }
    candidates.push(Candidate::Exact(joined));
    candidates
}

pub(crate) fn resolve_python_relative(from: &str, spec: &str) -> Vec<Candidate> {
    let mut rest = spec;
    let mut dir = parent(from).unwrap_or("");
    while rest.starts_with('.') {
        rest = &rest[1..];
        if rest.starts_with('.') {
            dir = parent(dir).unwrap_or("");
        }
    }
    if rest.is_empty() {
        return Vec::new();
    }
    let segs: Vec<&str> = rest.split('.').collect();
    resolve_from_dir(dir, &segs)
}

pub(crate) fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(head, _)| head)
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

pub(crate) fn normalize_join(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Whether a specifier names something of this workspace's own shape.
///
/// `own` is the first segment of the file's own `package`/`namespace`
/// declaration, when it has one: `com` for `package com.acme;`, `Acme` for
/// `namespace Acme {`. A specifier whose first segment differs belongs to
/// another root — the JDK, a vendor namespace, a framework — and is not a
/// missing file; with nothing to compare against, every specifier is
/// workspace-shaped and a specifier that resolves to nothing is reported.
pub(crate) fn is_workspace_spec(spec_root: &str, own: Option<&str>) -> bool {
    own.is_none_or(|own| own == spec_root)
}

/// The first dot- or backslash-separated segment of a specifier: `com` in
/// `com.acme.Util`, `Acme` in `Acme\Util`.
pub(crate) fn root_segment(spec: &str) -> &str {
    spec.split(['.', '\\']).next().unwrap_or(spec)
}

/// A language's comment syntax, for [`mask_comments`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comments {
    /// `//` and `/* … */`: Rust, Go, Java, C#, C/C++, Kotlin, Swift, PHP.
    Slashes,
    /// `#` to end of line: Ruby, and PHP's second form.
    Hash,
}

/// Blanks out every comment in `source`, keeping the byte length and every
/// newline, so a match and a `line_character` offset computed on the result
/// still describe the original file.
///
/// The pattern languages read declarations off the text, and a commented-out
/// declaration is not a declaration: without this, `// func Gone() {}` and a
/// `/* class Ghost {} */` block become exports. String literals are left
/// alone on purpose — an `#include "x.h"` needs its quotes — so a declaration
/// written inside a multi-line string is still a gap these languages have.
pub(crate) fn mask_comments(source: &str, comments: Comments) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut string: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = string {
            match byte {
                b'\\' => index += 2,
                _ if byte == quote => {
                    string = None;
                    index += 1;
                }
                _ => index += 1,
            }
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => {
                string = Some(byte);
                index += 1;
            }
            b'/' if comments == Comments::Slashes && bytes.get(index + 1) == Some(&b'/') => {
                index = blank_to_eol(&mut out, bytes, index);
            }
            b'/' if comments == Comments::Slashes && bytes.get(index + 1) == Some(&b'*') => {
                index = blank_block(&mut out, bytes, index);
            }
            b'#' if comments == Comments::Hash => {
                index = blank_to_eol(&mut out, bytes, index);
            }
            _ => index += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_owned())
}

/// Blanks `out` from `start` to the newline before `\n` (or the end), and
/// returns the index just past the blanked region.
fn blank_to_eol(out: &mut [u8], bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'\n' {
        out[end] = b' ';
        end += 1;
    }
    end
}

/// Blanks `out` from `start` through the closing `*/` (or the end when the
/// comment never closes), and returns the index just past it. Newlines stay.
fn blank_block(out: &mut [u8], bytes: &[u8], start: usize) -> usize {
    let mut end = start + 2;
    while end < bytes.len() {
        if bytes[end] == b'*' && bytes.get(end + 1) == Some(&b'/') {
            end += 2;
            break;
        }
        end += 1;
    }
    for byte in &mut out[start..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
    end
}

// ---- spans and signatures ----------------------------------------------------

/// Children that precede a declaration's own text: comments, attributes,
/// decorators, annotations. A signature starts after them.
const LEADING: &[&str] = &[
    "comment",
    "line_comment",
    "block_comment",
    "multiline_comment",
    "attribute_item",
    "inner_attribute_item",
    "attribute_list",
    "attribute",
    "decorator",
    "annotation",
    "marker_annotation",
    "single_line_comment",
];

/// Nodes whose children are modifiers mixed with annotations (`@Override
/// public`): the annotations are skipped, the keywords stay.
const MODIFIER_LISTS: &[&str] = &["modifiers", "modifier_list"];

/// Where the declaration's own text starts: past comments, attributes,
/// decorators and annotations.
fn signature_start(outer: Node) -> usize {
    let mut cursor = outer.walk();
    for child in outer.children(&mut cursor) {
        let kind = child.kind();
        if LEADING.contains(&kind) {
            continue;
        }
        if MODIFIER_LISTS.contains(&kind) {
            let mut inner = child.walk();
            if let Some(first) = child
                .children(&mut inner)
                .find(|part| !LEADING.contains(&part.kind()))
            {
                return first.start_byte();
            }
            continue;
        }
        return child.start_byte();
    }
    outer.start_byte()
}

/// Where the declaration's signature ends: the start of its body, or of the
/// initialiser of a const or alias (`=`). Only the declarator chain is
/// searched for a `value`, so a parameter's default is never taken for one.
fn signature_end(decl: Node) -> usize {
    fn cut(node: Node, depth: usize) -> Option<usize> {
        if let Some(body) = node.child_by_field_name("body") {
            return Some(body.start_byte());
        }
        let value = node.child_by_field_name("value").or_else(|| {
            (node.kind() == "assignment")
                .then(|| node.child_by_field_name("right"))
                .flatten()
        });
        if let Some(value) = value {
            // The `=` sits between the declarator and the value.
            let mut cursor = node.walk();
            let eq = node
                .children(&mut cursor)
                .find(|child| child.kind() == "=" && child.end_byte() <= value.start_byte());
            return Some(eq.map_or(value.start_byte(), |eq| eq.start_byte()));
        }
        // Kotlin's `fun f() = expr` and `fun f() { … }` keep the body in a
        // `function_body` child that has no field name.
        {
            let mut cursor = node.walk();
            if let Some(body) = node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "function_body")
            {
                return Some(body.start_byte());
            }
        }
        // An alias keeps its name and generics, not what it stands for.
        if matches!(
            node.kind(),
            "type_item"
                | "type_alias_declaration"
                | "type_alias"
                | "typealias_declaration"
                | "property_declaration"
        ) {
            let mut cursor = node.walk();
            if let Some(eq) = node.children(&mut cursor).find(|child| child.kind() == "=") {
                return Some(eq.start_byte());
            }
        }
        if depth == 0 {
            return None;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| {
                let kind = child.kind();
                kind.contains("declarator")
                    || kind == "variable_declaration"
                    || kind == "const_element"
            })
            .find_map(|child| cut(child, depth - 1))
    }
    cut(decl, 3).unwrap_or_else(|| decl.end_byte())
}

/// The signature of a declaration: its text from the first keyword or
/// modifier to the start of its body or initialiser, on one line.
///
/// `outer` is the node that starts the declaration (it may carry `export` or
/// sit inside a decorator wrapper), `decl` the node that has the body.
pub(crate) fn signature_of(outer: Node, decl: Node, source: &[u8]) -> Option<String> {
    let start = signature_start(outer);
    let end = signature_end(decl).max(start);
    let mut raw = std::str::from_utf8(source.get(start..end)?).ok()?;
    // No body or initialiser field (a Go `type T struct { … }`): the first
    // brace opens the body.
    if end == decl.end_byte()
        && let Some(brace) = raw.find('{')
    {
        raw = &raw[..brace];
    }
    let raw = raw
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ';' | '=' | ':' | ',' | '{'));
    one_line_signature(raw)
}

/// Wrappers that carry what the declaration's own node does not: `export`,
/// a decorator list, and a Go `type`/`const`/`var` keyword with one spec.
fn outer_of(decl: Node) -> Node {
    let Some(parent) = decl.parent() else {
        return decl;
    };
    match parent.kind() {
        "export_statement" | "decorated_definition" => parent,
        "type_declaration" | "const_declaration" | "var_declaration"
            if parent.named_child_count() == 1 =>
        {
            parent
        }
        _ => decl,
    }
}

/// Builds the symbols of a file from the exports its walker found: for each
/// export site the nearest enclosing node `classify` accepts is the
/// declaration, and `classify(node, name)` names its kind. An export whose
/// declaration is not found gets no entry; the index derives one for it.
pub(crate) fn symbols_from_sites(
    root: Node,
    source: &[u8],
    sites: &[crate::ExportSite],
    classify: &dyn Fn(Node, Node) -> Option<SymbolKind>,
) -> Vec<SymbolSite> {
    let mut out = Vec::new();
    for site in sites {
        let row = usize::try_from(site.line.saturating_sub(1)).unwrap_or(0);
        let column = usize::try_from(site.character).unwrap_or(0);
        let point = tree_sitter::Point::new(row, column);
        let Some(name) = root.descendant_for_point_range(point, point) else {
            continue;
        };
        let mut ancestor = name.parent();
        while let Some(node) = ancestor {
            if let Some(kind) = classify(node, name) {
                let outer = outer_of(node);
                let line = |row: usize| u32::try_from(row).unwrap_or(u32::MAX).saturating_add(1);
                out.push(SymbolSite {
                    name: site.name.clone(),
                    kind,
                    span: (
                        line(outer.start_position().row),
                        line(outer.end_position().row.max(node.end_position().row)),
                    ),
                    signature: signature_of(outer, node, source),
                    exported: true,
                });
                break;
            }
            ancestor = node.parent();
        }
    }
    out
}

/// Whether any ancestor of `node` has one of `kinds`.
pub(crate) fn inside(node: Node, kinds: &[&str]) -> bool {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if kinds.contains(&parent.kind()) {
            return true;
        }
        ancestor = parent.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    /// Two files end with the suffix: the pick is the shallowest, then the
    /// smallest, whatever order the set was built in (and each set has its own
    /// hash seed, so many sets stand in for many runs).
    #[test]
    fn a_suffix_that_two_files_share_resolves_to_the_same_one_every_time() {
        let candidates = [Candidate::Suffix("spec/spec_helper.rb".to_owned())];
        let paths = [
            "sinatra-contrib/spec/spec_helper.rb",
            "rack-protection/spec/spec_helper.rb",
            "other/deep/er/spec/spec_helper.rb",
        ];
        let forward = paths;
        let mut backward = paths;
        backward.reverse();
        for _ in 0..50 {
            for order in [forward, backward] {
                assert_eq!(
                    first_known(&candidates, &set(&order)).as_deref(),
                    Some("rack-protection/spec/spec_helper.rb")
                );
            }
        }
        // The shallower file wins over a lexicographically smaller deeper one.
        let shallow = set(&["a/b/c/Buffer.kt", "z/Buffer.kt"]);
        assert_eq!(
            first_known(&[Candidate::Suffix("Buffer.kt".to_owned())], &shallow).as_deref(),
            Some("z/Buffer.kt")
        );
    }

    #[test]
    fn an_exact_path_still_beats_a_suffix_match() {
        let files = set(&["spec/spec_helper.rb", "a/spec/spec_helper.rb"]);
        assert_eq!(
            first_known(
                &[Candidate::Suffix("spec/spec_helper.rb".to_owned())],
                &files
            )
            .as_deref(),
            Some("spec/spec_helper.rb")
        );
    }
}
