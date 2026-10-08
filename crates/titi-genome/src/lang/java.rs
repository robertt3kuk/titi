//! Java: types, public and protected methods, and `import`/`package` lines,
//! from patterns.
//!
//! Java has no header-to-implementation split to exploit: `import a.b.C;` maps
//! straight onto a path, so resolution needs no module index — only the file's
//! extension and a suffix match — while the declarations are read off the text
//! and can be missed (a declaration the patterns do not recognise, or one an
//! annotation or a line break puts somewhere odd). Comments are blanked before
//! either pattern runs, so a commented-out declaration is not a declaration;
//! string literals are not, so a declaration written inside a string is still
//! a gap.
//!
//! An import that resolves to nothing is only a missing file when it shares
//! the file's own `package` root: `java.util.List` is the JDK's, not this
//! workspace's, so it is neither an edge nor a diagnostic. `import static
//! a.b.C.member;` names a member of the type, so the whole dotted path is
//! tried first and the path without its trailing segment second. A wildcard
//! `import a.b.*;` names a package, for which no single file stands, so it
//! carries neither an edge nor a warning.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, is_workspace_spec, mask_comments, record,
    resolve_suffix, root_segment,
};

/// Type declarations a caller can name. `record` is a Java 16 type like any
/// other; `static` and `strictfp` cover a nested or class-level declaration.
const TYPES: &str = r"(?m)^\s*(?:public\s+|final\s+|abstract\s+|sealed\s+|non-sealed\s+|static\s+|strictfp\s+)*(?:class|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// Methods a caller can name: `public` and `protected`, with the modifiers,
/// the generic method type parameters and the return type the language allows
/// (`void`, a generic type, an array, a qualified name). The leading annotation
/// group is not captured, so `@Override public void run()` yields `run` and a
/// bare annotation line yields nothing; a constructor (`public App(...)`) has
/// no return type and no whitespace before its name, so it yields nothing
/// either.
const METHODS: &str = r"(?m)^\s*(?:@[A-Za-z_][A-Za-z0-9_.]*(?:\([^)]*\))?\s+)*(?:public|protected)\s+(?:(?:static|final|abstract|synchronized|native|default|strictfp)\s+)*(?:<[^;{}]*>\s+)?[\w.$]+(?:\s*<[^;{}]*>)?(?:\s*\[\s*\])*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(";

/// The file's own `package`, whose first segment decides what "workspace
/// shaped" means for its imports.
const PACKAGE: &str = r"(?m)^\s*package\s+([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)";

/// `import a.b.C;`, `import static a.b.C.member;` and `import a.b.*;`. The
/// `static` flag and the wildcard tail are captured so each is handled for what
/// it is.
const IMPORT: &str =
    r"(?m)^\s*import\s+(static\s+)?([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)(\.\*)?";

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static TYPES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(TYPES).expect("java types"));
    static METHODS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(METHODS).expect("java methods"));
    static PACKAGE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(PACKAGE).expect("java package"));
    static IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(IMPORT).expect("java import"));

    let masked = mask_comments(source, Comments::Slashes);

    let mut sites = export_sites_from(&TYPES_RE, &masked);
    sites.extend(export_sites_from(&METHODS_RE, &masked));

    // `com` for `package com.acme;`. Without a package, every specifier is
    // workspace-shaped and one that resolves to nothing is reported.
    let own = PACKAGE_RE
        .captures(&masked)
        .and_then(|cap| cap.get(1))
        .map(|m| root_segment(m.as_str()));

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in IMPORT_RE.captures_iter(&masked) {
        let spec = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        if spec.is_empty() {
            continue;
        }
        // A wildcard names a package, which no single file stands for.
        if cap.get(3).is_some() {
            continue;
        }
        let is_static = cap.get(1).is_some();
        let path = spec.replace('.', "/");
        let resolved = resolve_suffix(&path, &["java"], files).or_else(|| {
            // `a.b.C.member`: the type is the import, so drop the trailing
            // member and retry.
            if !is_static {
                return None;
            }
            let type_path = path.rsplit_once('/').map(|(head, _)| head)?;
            resolve_suffix(type_path, &["java"], files)
        });
        let placement = match resolved {
            Some(path) => Placement::Resolved(path),
            None if is_workspace_spec(root_segment(spec), own) => Placement::Missing,
            None => Placement::External,
        };
        record(spec, placement, &mut imports, &mut unresolved);
    }
    finish(source, sites, imports, unresolved, 0)
}
