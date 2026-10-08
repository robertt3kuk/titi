//! Ruby: `def`/`class`/`module`/`attr_*` and `require` lines, from patterns.
//!
//! Comments are masked before the patterns run, so a commented-out `def` is
//! not an export. `=begin`/`=end` block comments are still a gap: they are not
//! masked, so a declaration written inside one is read as an export. So are
//! declarations whose name is not a bare identifier — operator methods
//! (`def <=>`) — and `attr_*` symbol lists written as `%i[a b]`.
//!
//! A `require_relative` names a file beside this one, so it is resolved
//! against the workspace and reported when it names nothing. A plain `require`
//! searches the load path — this workspace, a `-I` directory, or a gem — so a
//! specifier that names no workspace file is left alone rather than reported.
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, internal, line_character, mask_comments,
    record, resolve_relative, resolve_suffix,
};
use crate::ExportSite;

/// `require_relative 'x'`: file-shaped by definition, resolved beside the file.
static REQUIRE_RELATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*require_relative\s+['"]([^'"]+)['"]"#).expect("ruby require_relative")
});
/// A plain `require`: the load path may be a gem, so a miss is not a warning.
static REQUIRE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*require\s+['"]([^'"]+)['"]"#).expect("ruby require"));
/// `def name`, or a singleton `def self.name` / `def Foo.name` whose published
/// name is the one after the dot.
static DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*def\s+(?:[A-Za-z_][A-Za-z0-9_]*\.)?([A-Za-z_][A-Za-z0-9_]*[!?=]?)")
        .expect("ruby def")
});
/// `class`/`module`, possibly namespaced (`module Acme::App`).
static TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:class|module)\s+([A-Za-z_][A-Za-z0-9_:]*)").expect("ruby type")
});
/// `attr_reader`/`attr_writer`/`attr_accessor` with their symbol arguments,
/// with or without the parenthesised call form.
static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*attr_(?:reader|writer|accessor)\s*\(?([^\n]+)").expect("ruby attr")
});
/// A `:symbol`, `'symbol'` or `"symbol"` inside an `attr_*` argument list.
static SYMBOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[:'"]([A-Za-z_][A-Za-z0-9_!?]*)"#).expect("ruby attr symbol"));

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let masked = mask_comments(source, Comments::Hash);

    let mut sites = export_sites_from(&DEF, &masked);
    // `class Acme::App` names `App`: the last `::` segment is the constant.
    for cap in TYPE.captures_iter(&masked) {
        let matched = &cap[1];
        let name = matched.rsplit("::").next().unwrap_or(matched);
        let offset = cap.get(1).map(|m| m.start()).unwrap_or(0) + (matched.len() - name.len());
        push_site(&mut sites, &masked, offset, name);
    }
    // `attr_reader :a, :b` is the idiomatic public reader, and one line is
    // several exports.
    for cap in ATTR.captures_iter(&masked) {
        let Some(args) = cap.get(1) else {
            continue;
        };
        for symbol in SYMBOL.captures_iter(args.as_str()) {
            if let Some(name) = symbol.get(1) {
                push_site(
                    &mut sites,
                    &masked,
                    args.start() + name.start(),
                    name.as_str(),
                );
            }
        }
    }
    sites.sort_by_key(|site| (site.line, site.character));

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in REQUIRE_RELATIVE.captures_iter(&masked) {
        let spec = &cap[1];
        record(
            spec,
            internal(resolve_relative(path, spec, files, &["rb"])),
            &mut imports,
            &mut unresolved,
        );
    }
    for cap in REQUIRE.captures_iter(&masked) {
        let spec = &cap[1];
        let stem = spec.strip_suffix(".rb").unwrap_or(spec);
        // Nothing found means a gem or a `-I` directory, not a missing file.
        let placement = match resolve_suffix(stem, &["rb"], files) {
            Some(found) => Placement::Resolved(found),
            None => Placement::External,
        };
        record(spec, placement, &mut imports, &mut unresolved);
    }

    finish(&masked, sites, imports, unresolved, 0)
}

fn push_site(sites: &mut Vec<ExportSite>, masked: &str, byte: usize, name: &str) {
    let (line, character) = line_character(masked, byte);
    sites.push(ExportSite {
        name: name.to_owned(),
        line,
        character,
    });
}
