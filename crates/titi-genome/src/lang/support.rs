//! The plumbing the per-language parsers share.
//!
//! None of this knows a language: `finish` closes a [`ParsedFile`] off,
//! `mask_comments` blanks the comments in a text a pattern will read, and the
//! `resolve_*` family turns a module specifier into a workspace path. A parser
//! module owns everything that is specific to its language.

use std::collections::HashSet;

use regex::Regex;

use crate::refs;

use super::ParsedFile;

/// Sorts, dedups and attaches the identifier set every parser ends up needing.
///
/// Export *names* stay sorted, which is what the ranker keys off. Sites keep
/// source order so a definition query can point at the declaration.
pub(crate) fn finish(
    source: &str,
    mut sites: Vec<crate::ExportSite>,
    mut imports: Vec<String>,
    mut unresolved: Vec<String>,
    syntax_errors: u32,
) -> ParsedFile {
    let mut seen = HashSet::new();
    sites.retain(|site| seen.insert(site.name.clone()));
    let mut exports: Vec<String> = sites.iter().map(|site| site.name.clone()).collect();
    exports.sort();
    exports.dedup();
    imports.sort();
    imports.dedup();
    unresolved.sort();
    unresolved.dedup();
    let refs = refs::collect_refs(source, &exports);
    ParsedFile {
        exports,
        export_sites: sites,
        imports,
        unresolved_imports: unresolved,
        refs,
        syntax_errors,
    }
}

/// Where an import's specifier landed.
///
/// The distinction is the honest part of a heuristic import: a specifier that
/// names something outside the workspace (`java.util.List`, `Foundation`,
/// `fmt`) is not a missing file, and reporting it as one would put a warning
/// on every file that uses a library. Only [`Placement::Missing`] — workspace
/// shaped, and no file to show for it — becomes a diagnostic.
pub(crate) enum Placement {
    /// The workspace file the specifier names.
    Resolved(String),
    /// A specifier of this workspace's own shape that names no file.
    Missing,
    /// Not this workspace's business: a std module, a dependency, a gem.
    External,
}

pub(crate) fn record(
    spec: &str,
    placement: Placement,
    imports: &mut Vec<String>,
    unresolved: &mut Vec<String>,
) {
    if spec.is_empty() {
        return;
    }
    match placement {
        Placement::Resolved(path) => imports.push(path),
        Placement::Missing => unresolved.push(spec.to_owned()),
        Placement::External => {}
    }
}

pub(crate) fn export_sites_from(re: &Regex, source: &str) -> Vec<crate::ExportSite> {
    re.captures_iter(source)
        .filter_map(|cap| {
            let matched = cap.get(1).or_else(|| cap.get(2))?;
            let (line, character) = line_character(source, matched.start());
            Some(crate::ExportSite {
                name: matched.as_str().to_owned(),
                line,
                character,
            })
        })
        .collect()
}

pub(crate) fn line_character(source: &str, byte: usize) -> (u32, u32) {
    let byte = byte.min(source.len());
    let head = source.get(..byte).unwrap_or("");
    let line = head.bytes().filter(|unit| *unit == b'\n').count() as u32 + 1;
    let character = head
        .rfind('\n')
        .map(|index| byte - index - 1)
        .unwrap_or(byte) as u32;
    (line, character)
}

/// Resolves a module/package path to a known file by trying the path itself
/// and then every trailing suffix of it. That covers Go and Ruby, where the
/// import string is module-qualified and the repo has no module index yet.
pub(crate) fn resolve_suffix(spec: &str, exts: &[&str], files: &HashSet<String>) -> Option<String> {
    let spec = spec.trim_matches('/');
    if spec.is_empty() {
        return None;
    }
    let segments: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
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
                if files.contains(&path) {
                    return Some(path);
                }
                // The file may live under a root prefix the import omitted.
                let needle = format!("/{path}");
                if let Some(hit) = files.iter().find(|known| known.ends_with(&needle)) {
                    return Some(hit.clone());
                }
            }
        }
    }
    None
}

pub(crate) fn resolve_from_dir(
    dir: &str,
    segs: &[&str],
    files: &HashSet<String>,
) -> Option<String> {
    if segs.is_empty() {
        return None;
    }
    let mut parts: Vec<&str> = segs
        .iter()
        .copied()
        .filter(|seg| !seg.is_empty() && *seg != "*")
        .collect();
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
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
        parts.pop();
    }
    None
}

pub(crate) fn resolve_relative(
    from: &str,
    spec: &str,
    files: &HashSet<String>,
    exts: &[&str],
) -> Option<String> {
    let from_dir = parent(from).unwrap_or("");
    let joined = normalize_join(from_dir, spec)?;
    for ext in exts {
        let file = format!("{joined}.{ext}");
        if files.contains(&file) {
            return Some(file);
        }
        let index = format!("{joined}/index.{ext}");
        if files.contains(&index) {
            return Some(index);
        }
    }
    if files.contains(&joined) {
        return Some(joined);
    }
    None
}

pub(crate) fn resolve_python_relative(
    from: &str,
    spec: &str,
    files: &HashSet<String>,
) -> Option<String> {
    let mut rest = spec;
    let mut dir = parent(from).unwrap_or("");
    while rest.starts_with('.') {
        rest = &rest[1..];
        if rest.starts_with('.') {
            dir = parent(dir).unwrap_or("");
        }
    }
    if rest.is_empty() {
        return None;
    }
    let segs: Vec<&str> = rest.split('.').collect();
    resolve_from_dir(dir, &segs, files)
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

/// The placement of a specifier this language resolves itself, where there is
/// no outside world to speak of: `use super::x` in Rust, a `require_relative`
/// in Ruby. `None` means the workspace-shaped specifier named no file.
pub(crate) fn internal(resolved: Option<String>) -> Placement {
    match resolved {
        Some(path) => Placement::Resolved(path),
        None => Placement::Missing,
    }
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
