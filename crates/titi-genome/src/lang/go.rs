//! Go: exported identifiers and import paths, from patterns over the text.
//!
//! Go's own rule is what makes this affordable: an identifier is exported
//! exactly when it starts with an uppercase letter. Comments are blanked
//! before any pattern runs, so a commented-out `func Gone()` is not one, and
//! the usual grouped `type`/`var`/`const` block contributes its members like
//! any single-line declaration. What a grammar would still read and this does
//! not: a group whose body nests parentheses more than one level deep, several
//! names on one member line (`A, b int`), and a declaration written inside a
//! multi-line string.
//!
//! An import path resolves by its trailing segments over `.go`; when it does
//! not, the first segment decides the honest placement: `example.com/x` is a
//! module path of some kind and names no file, so it is a real
//! `unresolved-import`, while a dotless `fmt`/`os` is a standard-library path
//! and carries no warning at all. An alias (`x "p"`), a blank (`_ "p"`) and a
//! dot (`. "p"`) import all resolve the same way.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, line_character, mask_comments, record,
    resolve_suffix,
};

/// `func Main()` and a method `func (s *T) Exported()`: the receiver is
/// skipped, so the capture is always the function name. A type-parameter list
/// is left behind by the name capture, so `func Map[T any](…)` exports `Map`.
const FUNCS: &str = r"(?m)^\s*func\s+(?:\([^)\n]*\)\s*)?([A-Z][A-Za-z0-9_]*)";

/// A single-line `type Store struct{}`, `var X int`, `const Y = 1`. The
/// keyword must be followed straight by an identifier, so the `(` that opens a
/// group is not a declaration.
const DECLS: &str = r"(?m)^\s*(?:type|var|const)\s+([A-Z][A-Za-z0-9_]*)";

/// The body of a grouped `type (…)`, `var (…)`, `const (…)`. One level of
/// nested parentheses is tolerated so a member like `F func(int)` does not end
/// the group early.
const GROUPS: &str = r"(?ms)^\s*(?:type|var|const)\s*\(((?:[^()]|\([^()]*\))*)\)";

/// One member declaration per line inside a group; only a capitalised first
/// name is exported, which is where the lowercase members drop out.
const GROUP_MEMBERS: &str = r"(?m)^\s*([A-Z][A-Za-z0-9_]*)";

/// A grouped import block and the single-line form. The alias position is a
/// word (`x`), a blank (`_`) or a dot (`.`), so `[\w.]+` covers all three.
const IMPORT_BLOCK: &str = r#"(?ms)^\s*import\s*\(((?:[^()]|\([^()]*\))*)\)"#;
const IMPORT_LINE: &str = r#"(?m)^\s*import\s+(?:[\w.]+\s+)?"([^"]+)""#;

static FUNCS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(FUNCS).expect("go funcs"));
static DECLS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(DECLS).expect("go decls"));
static GROUPS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(GROUPS).expect("go groups"));
static GROUP_MEMBERS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(GROUP_MEMBERS).expect("go group members"));
static IMPORT_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(IMPORT_BLOCK).expect("go import block"));
static IMPORT_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(IMPORT_LINE).expect("go import"));

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    // Offsets computed on the masked text still describe the original: it
    // keeps every byte and newline, only comments are blanked.
    let masked = mask_comments(source, Comments::Slashes);

    let mut sites = export_sites_from(&DECLS_RE, &masked);
    sites.extend(export_sites_from(&FUNCS_RE, &masked));
    sites.extend(group_sites(&masked));
    // Sources were concatenated, so restore the source order the sites claim.
    sites.sort_by_key(|site| (site.line, site.character));

    let mut specs: Vec<String> = Vec::new();
    for cap in IMPORT_BLOCK_RE.captures_iter(&masked) {
        let body = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        // Every entry is `"path"` or `alias "path"`: the quoted parts are the
        // paths, so splitting on the quote keeps any alias out of it.
        for quoted in body.split('"').skip(1).step_by(2) {
            specs.push(quoted.to_owned());
        }
    }
    for cap in IMPORT_LINE_RE.captures_iter(&masked) {
        if let Some(spec) = cap.get(1) {
            specs.push(spec.as_str().to_owned());
        }
    }

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for spec in specs {
        // Go import paths are module-qualified; the package directory is the
        // trailing segment, so a suffix match is the honest resolution.
        let last = spec.rsplit('/').next().unwrap_or(&spec);
        let resolved = resolve_suffix(&format!("{last}/{last}"), &["go"], files)
            .or_else(|| resolve_suffix(last, &["go"], files));
        // A dotted first segment is a module path of some kind and names no
        // file, so it is a real diagnostic; a dotless one is a
        // standard-library path, not this workspace's business.
        let module_shaped = spec.split('/').next().unwrap_or(&spec).contains('.');
        let placement = match resolved {
            Some(path) => Placement::Resolved(path),
            None if module_shaped => Placement::Missing,
            None => Placement::External,
        };
        record(&spec, placement, &mut imports, &mut unresolved);
    }
    finish(&masked, sites, imports, unresolved, 0)
}

/// Exports declared inside a grouped `type`/`var`/`const` block: one member
/// per line, capitalised for exported, with the offset taken through the group
/// so it still points into the original file.
fn group_sites(text: &str) -> Vec<crate::ExportSite> {
    let mut sites = Vec::new();
    for cap in GROUPS_RE.captures_iter(text) {
        let Some(body) = cap.get(1) else {
            continue;
        };
        for member in GROUP_MEMBERS_RE.captures_iter(body.as_str()) {
            let Some(name) = member.get(1) else {
                continue;
            };
            let (line, character) = line_character(text, body.start() + name.start());
            sites.push(crate::ExportSite {
                name: name.as_str().to_owned(),
                line,
                character,
            });
        }
    }
    sites
}
