//! Which mentions in a file are uses of a symbol, and which are noise.
//!
//! Name-only resolution cannot tell `path.join` from the file that exports
//! `join`, so this is where the crate decides what is even a candidate: a bare
//! call, a path-qualified name whose qualifier is not std, or a capitalised
//! type name. Everything here is language-agnostic on purpose — it runs on the
//! same text whether the exports above it came from a syntax tree or from a
//! pattern.
//!
//! Moved out of `parse.rs` when the per-language parsers moved to `lang/`.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

/// Distinct identifiers recorded per file. Past this the file is mostly noise
/// and the symbol lookup cost stops paying for itself.
pub const MAX_REFS: usize = 512;

/// Keywords and primitive type names across the supported languages. Filtering
/// them keeps the reference set to things that could name a real symbol.
const KEYWORDS: &[&str] = &[
    // Control and declaration keywords, broadly shared.
    "abstract",
    "alias",
    "and",
    "as",
    "async",
    "await",
    "become",
    "bool",
    "box",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "crate",
    "def",
    "default",
    "defer",
    "delete",
    "do",
    "double",
    "dyn",
    "elif",
    "else",
    "enum",
    "except",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "float",
    "fn",
    "for",
    "foreach",
    "from",
    "func",
    "function",
    "global",
    "goto",
    "if",
    "impl",
    "implements",
    "import",
    "in",
    "instanceof",
    "int",
    "interface",
    "internal",
    "is",
    "lambda",
    "let",
    "long",
    "loop",
    "match",
    "mod",
    "module",
    "move",
    "mut",
    "namespace",
    "new",
    "nil",
    "none",
    "not",
    "null",
    "object",
    "open",
    "operator",
    "or",
    "override",
    "package",
    "pass",
    "private",
    "protected",
    "protocol",
    "pub",
    "public",
    "raise",
    "readonly",
    "record",
    "ref",
    "require",
    "return",
    "sealed",
    "self",
    "short",
    "sizeof",
    "static",
    "str",
    "strictfp",
    "struct",
    "super",
    "switch",
    "synchronized",
    "template",
    "this",
    "throw",
    "throws",
    "trait",
    "transient",
    "true",
    "try",
    "type",
    "typeof",
    "typename",
    "typeof",
    "union",
    "unsafe",
    "unsigned",
    "use",
    "using",
    "var",
    "virtual",
    "void",
    "volatile",
    "when",
    "where",
    "while",
    "with",
    "yield",
    // Common standard-library and test identifiers that are never project symbols.
    "assert",
    "env",
    "expect",
    "format",
    "get",
    "insert",
    "into",
    "iter",
    "len",
    "main",
    "map",
    "new",
    "print",
    "println",
    "push",
    "self",
    "some",
    "string",
    "to_string",
    "unwrap",
    "values",
    "vec",
];

/// Names that are standard-library types in nearly every language. A project
/// that defines `pub type Result` would otherwise make every file in the repo
/// depend on it.
const UBIQUITOUS: &[&str] = &[
    "Arc",
    "Box",
    "Clone",
    "Cow",
    "Debug",
    "Default",
    "Deserialize",
    "Display",
    "Duration",
    "Eq",
    "Error",
    "Formatter",
    "Future",
    "HashMap",
    "HashSet",
    "Instant",
    "Iterator",
    "Mutex",
    "MutexGuard",
    "Option",
    "Ord",
    "Path",
    "PathBuf",
    "Rc",
    "Receiver",
    "RefCell",
    "Result",
    "RwLock",
    "Self",
    "Sender",
    "Serialize",
    "Stream",
    "String",
    "SystemTime",
    "Vec",
    "Weak",
];

/// Qualifiers that name a std or prelude type or module. `Path::join` is a
/// method of `Path`, not a use of whatever file uniquely exports `join`.
/// `hub::join` still counts: the qualifier is a project module.
///
/// `Path`, `PathBuf`, `Vec`, `Option`, `Result`, `String`, `HashMap` and
/// `HashSet` are already in [`UBIQUITOUS`] and are qualifiers through that
/// list. The names here are the std modules and types that are not themselves
/// ubiquitous symbols, but must not qualify a project export either.
const STD_QUALIFIERS: &[&str] = &[
    "OsStr", "OsString", "char", "iter", "slice", "str", "thread",
];

fn is_std_qualifier(name: &str) -> bool {
    STD_QUALIFIERS.contains(&name) || UBIQUITOUS.contains(&name)
}

/// Whether a mention at `name_start` is a use under the same rules as
/// [`collect_refs`]. A method call and a std-qualified path are not: name-only
/// resolution cannot tell `path.join` from the file that exports `join`.
pub(crate) fn is_resolvable_mention(source: &str, name_start: usize) -> bool {
    if preceded_by_dot(source, name_start) {
        return false;
    }
    if let Some(qualifier) = qualifier_before(source, name_start) {
        return !is_std_qualifier(qualifier);
    }
    true
}

pub(crate) fn is_noise_name(name: &str) -> bool {
    KEYWORDS.contains(&name) || UBIQUITOUS.contains(&name)
}

/// Distinct *usage sites* the file mentions: bare calls (`name(`), path-qualified
/// names whose qualifier is not std (`hub::join`), and capitalized type or
/// constructor names.
///
/// A call preceded by `.` is a method call (`path.join(`). A path whose
/// qualifier is std (`Path::join`) is the same collision. Neither is a use of
/// whatever file uniquely exports that name — name-only resolution cannot tell
/// them apart. Bare lowercase identifiers are not collected: a local named
/// `path` is not a dependency on whatever file exports `path`.
pub(crate) fn collect_refs(source: &str, exports: &[String]) -> Vec<String> {
    static CALL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"([A-Za-z_][A-Za-z0-9_]{2,})\s*\(").expect("call regex"));
    static PATH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"::\s*([A-Za-z_][A-Za-z0-9_]{2,})").expect("path regex"));
    static TYPE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b([A-Z][A-Za-z0-9_]{2,})\b").expect("type regex"));

    let own: HashSet<&str> = exports.iter().map(String::as_str).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();

    for cap in CALL.captures_iter(source) {
        let Some(matched) = cap.get(1) else {
            continue;
        };
        // `path.join(` and `Path::join(` are not bare calls. The path pass
        // decides the latter; a dot means a method and is never a use.
        if preceded_by_dot(source, matched.start()) || preceded_by_path_sep(source, matched.start())
        {
            continue;
        }
        if record_ref(matched.as_str(), &own, &mut seen, &mut out) {
            return out;
        }
    }
    for cap in PATH.captures_iter(source) {
        let Some(matched) = cap.get(1) else {
            continue;
        };
        if qualifier_before(source, matched.start()).is_some_and(is_std_qualifier) {
            continue;
        }
        if record_ref(matched.as_str(), &own, &mut seen, &mut out) {
            return out;
        }
    }
    for cap in TYPE.captures_iter(source) {
        let Some(matched) = cap.get(1) else {
            continue;
        };
        if record_ref(matched.as_str(), &own, &mut seen, &mut out) {
            return out;
        }
    }
    out
}

fn record_ref(
    name: &str,
    own: &HashSet<&str>,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
) -> bool {
    if own.contains(name)
        || KEYWORDS.contains(&name)
        || UBIQUITOUS.contains(&name)
        || !seen.insert(name.to_owned())
    {
        return false;
    }
    out.push(name.to_owned());
    out.len() >= MAX_REFS
}

fn skip_ascii_ws_before(source: &str, index: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = index;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

fn preceded_by_dot(source: &str, name_start: usize) -> bool {
    let i = skip_ascii_ws_before(source, name_start);
    i > 0 && source.as_bytes()[i - 1] == b'.'
}

fn preceded_by_path_sep(source: &str, name_start: usize) -> bool {
    let i = skip_ascii_ws_before(source, name_start);
    i >= 2 && source.as_bytes()[i - 2] == b':' && source.as_bytes()[i - 1] == b':'
}

/// Identifier immediately before `::name`, if the match is path-qualified.
fn qualifier_before(source: &str, name_start: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut i = skip_ascii_ws_before(source, name_start);
    if i < 2 || bytes[i - 2] != b':' || bytes[i - 1] != b':' {
        return None;
    }
    i = skip_ascii_ws_before(source, i - 2);
    let end = i;
    while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    if i == end { None } else { source.get(i..end) }
}
