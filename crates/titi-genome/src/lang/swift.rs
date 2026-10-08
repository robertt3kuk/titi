//! Swift: declaration and `import` lines, from patterns over the text.
//!
//! Comments are masked before the patterns run, so `// public func ghost()` is
//! not an export. A multi-line `"""` string is not masked (its odd quote count
//! confuses the masker), so a declaration written inside one would be read as
//! an export. A Swift `import` names a module, never a path, so the one
//! honest edge is a workspace module whose own primary file exists under the
//! `MyLib/MyLib.swift` (or `MyLib.swift`) convention; every other module is a
//! dependency or another target and is left alone rather than reported.
//!
//! Whether a declaration is a `class`, a `struct` or an `actor` is not
//! recorded — only its name is. A property declaration that carries an
//! attribute (`@Published public var x`) is not recognised either.
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::ParsedFile;
use super::support::{
    Comments, Placement, export_sites_from, finish, mask_comments, record, resolve_suffix,
};

/// `import Foo`, `@testable import Foo`, `import class UIKit.UIView`: the
/// module is always the first dotted name after the kind keyword.
static IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*(?:@testable\s+)?import\s+(?:(?:class|struct|enum|protocol|typealias|func|let|var)\s+)?([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("swift import")
});
/// The declaration keywords whose name is public API.
static DECLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*(?:(?:public|open|final|internal)\s+)*(?:func|struct|class|enum|protocol|extension|actor|typealias)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("swift declarations")
});
/// A `var` inside a function body can never carry `public`, so anchoring on
/// the modifier keeps this off locals.
static PROPS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:public|open)\s+(?:var|let)\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("swift public properties")
});

pub(super) fn parse(_path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    let masked = mask_comments(source, Comments::Slashes);

    let mut sites = export_sites_from(&DECLS, &masked);
    sites.extend(export_sites_from(&PROPS, &masked));
    sites.sort_by_key(|site| (site.line, site.character));

    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for cap in IMPORT.captures_iter(&masked) {
        let module = &cap[1];
        let placement = match resolve_module(module, files) {
            Some(path) => Placement::Resolved(path),
            None => Placement::External,
        };
        record(module, placement, &mut imports, &mut unresolved);
    }

    finish(&masked, sites, imports, unresolved, 0)
}

/// The module-file convention: a resolved path named after the module
/// (`MyLib.swift`, or `MyLib/MyLib.swift`). Anything else this finds is not
/// the module's own file, so it is not claimed as one.
fn resolve_module(module: &str, files: &HashSet<String>) -> Option<String> {
    let path = resolve_suffix(module, &["swift"], files)?;
    let sibling = format!("/{module}/{module}.swift");
    let primary = format!("{module}.swift");
    if path.ends_with(&sibling) || path.ends_with(&primary) {
        Some(path)
    } else {
        None
    }
}
