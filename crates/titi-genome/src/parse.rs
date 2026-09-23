// The regexes below are compile-time literals; a bad pattern is a bug the unit
// tests catch, not a runtime condition worth threading through callers.
#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, OnceLock};

use regex::Regex;

use crate::symbols;

/// Distinct identifiers recorded per file. Past this the file is mostly noise
/// and the symbol lookup cost stops paying for itself.
pub const MAX_REFS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Kotlin,
    Swift,
    Php,
    Other,
}

/// Extensions the scanner indexes, one entry per recognised source language.
/// `Language::from_path` is the single source of truth for them, so a language
/// added there is indexed without a second list to keep in sync.
const EXTENSIONS: &[(&str, Language)] = &[
    ("rs", Language::Rust),
    ("ts", Language::TypeScript),
    ("tsx", Language::TypeScript),
    ("js", Language::TypeScript),
    ("jsx", Language::TypeScript),
    ("mjs", Language::TypeScript),
    ("cjs", Language::TypeScript),
    ("py", Language::Python),
    ("go", Language::Go),
    ("java", Language::Java),
    ("c", Language::C),
    ("h", Language::C),
    ("cpp", Language::Cpp),
    ("cc", Language::Cpp),
    ("cxx", Language::Cpp),
    ("hpp", Language::Cpp),
    ("hh", Language::Cpp),
    ("hxx", Language::Cpp),
    ("cs", Language::CSharp),
    ("rb", Language::Ruby),
    ("kt", Language::Kotlin),
    ("kts", Language::Kotlin),
    ("swift", Language::Swift),
    ("php", Language::Php),
];

/// Whether the scanner should read this file at all.
pub fn is_source_file(name: &str) -> bool {
    extension_of(name).is_some_and(|ext| EXTENSIONS.iter().any(|(known, _)| *known == ext))
}

fn extension_of(path: &str) -> Option<&str> {
    Path::new(path).extension().and_then(|ext| ext.to_str())
}

impl Language {
    pub fn from_path(path: &str) -> Self {
        let Some(ext) = extension_of(path) else {
            return Self::Other;
        };
        EXTENSIONS
            .iter()
            .find(|(known, _)| *known == ext)
            .map(|(_, language)| *language)
            .unwrap_or(Self::Other)
    }

    /// Every extension the scanner indexes.
    pub fn indexed_extensions() -> impl Iterator<Item = &'static str> {
        EXTENSIONS.iter().map(|(ext, _)| *ext)
    }
}

/// What one file contributes to the graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedFile {
    /// Symbols this file defines and publishes.
    pub exports: Vec<String>,
    /// Files this one depends on.
    pub imports: Vec<String>,
    /// Distinct identifiers this file mentions, minus its own exports and
    /// keywords. Resolved against the repo's exports to become the
    /// symbol-level half of the graph.
    pub refs: Vec<String>,
}

pub fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    match Language::from_path(path) {
        Language::Rust => parse_rust(path, source, files),
        Language::TypeScript => parse_typescript(path, source, files),
        Language::Python => parse_python(path, source, files),
        Language::Go => parse_go(path, source, files),
        Language::Java => parse_path_imports(path, source, files, &["java"], JAVA_EXPORTS),
        Language::CSharp => parse_path_imports(path, source, files, &["cs"], CSHARP_EXPORTS),
        Language::Kotlin => parse_kotlin(path, source, files),
        Language::Php => parse_php(path, source, files),
        Language::Ruby => parse_ruby(path, source, files),
        Language::Swift => finish(source, swift_exports(source), Vec::new()),
        Language::C | Language::Cpp => parse_c_family(path, source, files),
        Language::Other => ParsedFile::default(),
    }
}

/// Sorts, dedups and attaches the identifier set every parser ends up needing.
fn finish(source: &str, mut exports: Vec<String>, mut imports: Vec<String>) -> ParsedFile {
    exports.sort();
    exports.dedup();
    imports.sort();
    imports.dedup();
    let refs = collect_refs(source, &exports);
    ParsedFile {
        exports,
        imports,
        refs,
    }
}

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

/// Distinct *usage sites* the file mentions: calls (`name(`), path-qualified
/// names (`::name`, `.name`) and capitalized type or constructor names. Bare
/// lowercase identifiers are deliberately not collected — a local variable
/// named `path` is not a dependency on whatever file exports `path`, and
/// collecting them made the graph count locals instead of symbols.
fn collect_refs(source: &str, exports: &[String]) -> Vec<String> {
    static SITES: OnceLock<Vec<Regex>> = OnceLock::new();
    let sites = SITES.get_or_init(|| {
        vec![
            // A call: `digest(`, `store::open(`, `self.foo(`. A bare field read
            // (`.path`) is deliberately absent: it is a struct member, not a
            // reference to whatever file exports a symbol of that name.
            Regex::new(r"([A-Za-z_][A-Za-z0-9_]{2,})\s*\(").expect("call regex"),
            // A path-qualified name.
            Regex::new(r"::([A-Za-z_][A-Za-z0-9_]{2,})").expect("path regex"),
            // A capitalized identifier: a type, enum variant or constructor.
            Regex::new(r"\b([A-Z][A-Za-z0-9_]{2,})\b").expect("type regex"),
        ]
    });
    let own: HashSet<&str> = exports.iter().map(String::as_str).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for site in sites {
        for cap in site.captures_iter(source) {
            let name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            if own.contains(name)
                || KEYWORDS.contains(&name)
                || UBIQUITOUS.contains(&name)
                || !seen.insert(name.to_owned())
            {
                continue;
            }
            out.push(name.to_owned());
            if out.len() >= MAX_REFS {
                return out;
            }
        }
    }
    out
}

/// Declaration regexes for languages whose imports are path-shaped.
const JAVA_EXPORTS: &str = r"(?m)^\s*(?:public\s+|final\s+|abstract\s+|sealed\s+|non-sealed\s+)*(?:class|interface|enum|record)\s+([A-Za-z_][A-Za-z0-9_]*)";
const CSHARP_EXPORTS: &str = r"(?m)^\s*(?:public\s+|internal\s+|static\s+|abstract\s+|sealed\s+|partial\s+)*(?:class|interface|enum|record|struct)\s+([A-Za-z_][A-Za-z0-9_]*)";

/// Java and C#: `import a.b.C;` / `using A.B.C;` map straight onto a path, so
/// resolution needs no module index — only the extension and a suffix match.
fn parse_path_imports(
    path: &str,
    source: &str,
    files: &HashSet<String>,
    exts: &[&str],
    exports_pattern: &str,
) -> ParsedFile {
    static IMPORT: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORT.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:import\s+(?:static\s+)?|using\s+)([A-Za-z_][A-Za-z0-9_.]*)")
            .expect("path import regex")
    });
    let exports_re = Regex::new(exports_pattern).expect("caller-supplied export pattern");

    let exports: Vec<String> = exports_re
        .captures_iter(source)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_owned()))
        .collect();

    let mut imports = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        // The trailing segment is the type; the rest is the package path.
        let candidate = spec.replace('.', "/");
        if let Some(resolved) = resolve_suffix(&candidate, exts, files) {
            imports.push(resolved);
        }
    }
    let _ = path;
    finish(source, exports, imports)
}

fn parse_go(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORT_BLOCK: OnceLock<Regex> = OnceLock::new();
    static IMPORT_LINE: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    // `import ( "a/b" \n "c/d" )` and the single-line form.
    let block_re = IMPORT_BLOCK
        .get_or_init(|| Regex::new(r#"(?ms)^\s*import\s*\((.*?)\)"#).expect("go import block"));
    let line_re = IMPORT_LINE.get_or_init(|| {
        Regex::new(r#"(?m)^\s*import\s+(?:[\w.]+\s+)?"([^"]+)""#).expect("go import")
    });
    let exports_re = EXPORTS.get_or_init(|| {
        // Exported Go identifiers start uppercase.
        Regex::new(r"(?m)^(?:func|type|var|const)\s+(?:\([^)]*\)\s*)?([A-Z][A-Za-z0-9_]*)")
            .expect("go exports")
    });

    let exports: Vec<String> = exports_re
        .captures_iter(source)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_owned()))
        .collect();

    let mut specs: Vec<String> = Vec::new();
    for cap in block_re.captures_iter(source) {
        let body = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        for quoted in body.split('"').skip(1).step_by(2) {
            specs.push(quoted.to_owned());
        }
    }
    for cap in line_re.captures_iter(source) {
        if let Some(spec) = cap.get(1) {
            specs.push(spec.as_str().to_owned());
        }
    }

    let mut imports = Vec::new();
    for spec in specs {
        // Go import paths are module-qualified; the package directory is the
        // trailing segment, so a suffix match is the honest resolution.
        let last = spec.rsplit('/').next().unwrap_or(&spec);
        if let Some(resolved) = resolve_suffix(&format!("{last}/{last}"), &["go"], files)
            .or_else(|| resolve_suffix(last, &["go"], files))
        {
            imports.push(resolved);
        }
    }
    let _ = path;
    finish(source, exports, imports)
}

fn parse_kotlin(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORTS: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORTS.get_or_init(|| {
        Regex::new(r"(?m)^\s*import\s+([A-Za-z_][A-Za-z0-9_.]*)").expect("kotlin import")
    });
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public\s+|internal\s+|open\s+|abstract\s+|sealed\s+|data\s+)*(?:class|interface|object|fun|val|var)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("kotlin exports")
    });
    let exports: Vec<String> = exports_re
        .captures_iter(source)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_owned()))
        .collect();
    let mut imports = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Some(resolved) = resolve_suffix(&spec.replace('.', "/"), &["kt", "kts"], files) {
            imports.push(resolved);
        }
    }
    let _ = path;
    finish(source, exports, imports)
}

fn parse_php(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORTS: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORTS
        .get_or_init(|| Regex::new(r"(?m)^\s*use\s+([A-Za-z_][A-Za-z0-9_\\]*)").expect("php use"));
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:final\s+|abstract\s+)*(?:class|interface|trait|function)\s+([A-Za-z_][A-Za-z0-9_]*)")
            .expect("php exports")
    });
    let exports: Vec<String> = exports_re
        .captures_iter(source)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_owned()))
        .collect();
    let mut imports = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Some(resolved) = resolve_suffix(&spec.replace('\\', "/"), &["php"], files) {
            imports.push(resolved);
        }
    }
    let _ = path;
    finish(source, exports, imports)
}

fn parse_ruby(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static IMPORTS: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let import_re = IMPORTS.get_or_init(|| {
        Regex::new(r#"(?m)^\s*require(?:_relative)?\s+['"]([^'"]+)['"]"#).expect("ruby require")
    });
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:def|class|module)\s+([A-Za-z_][A-Za-z0-9_]*)")
            .expect("ruby exports")
    });
    let exports: Vec<String> = exports_re
        .captures_iter(source)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_owned()))
        .collect();
    let mut imports = Vec::new();
    for cap in import_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let stem = spec.strip_suffix(".rb").unwrap_or(spec);
        if let Some(resolved) = resolve_suffix(stem, &["rb"], files) {
            imports.push(resolved);
        }
    }
    let _ = path;
    finish(source, exports, imports)
}

fn parse_c_family(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static INCLUDES: OnceLock<Regex> = OnceLock::new();
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let include_re = INCLUDES
        .get_or_init(|| Regex::new(r#"(?m)^\s*#\s*include\s+"([^"]+)""#).expect("c include"));
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:typedef\s+)?(?:struct|class|enum|union)\s+([A-Za-z_][A-Za-z0-9_]*)|^\s*(?:[A-Za-z_][A-Za-z0-9_]*\s+)+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
        )
        .expect("c exports")
    });
    let exports: Vec<String> = exports_re
        .captures_iter(source)
        .filter_map(|cap| {
            cap.get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str().to_owned())
        })
        .collect();
    let from_dir = parent(path).unwrap_or("");
    let mut imports = Vec::new();
    for cap in include_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let joined = if from_dir.is_empty() {
            spec.to_owned()
        } else {
            format!("{from_dir}/{spec}")
        };
        if let Some(resolved) = normalize_join("", &joined).filter(|p| files.contains(p)) {
            imports.push(resolved);
        }
    }
    finish(source, exports, imports)
}

fn swift_exports(source: &str) -> Vec<String> {
    static EXPORTS: OnceLock<Regex> = OnceLock::new();
    let exports_re = EXPORTS.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:public\s+|open\s+|final\s+|internal\s+)*(?:func|struct|class|enum|protocol|extension|typealias)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("swift exports")
    });
    exports_re
        .captures_iter(source)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_owned()))
        .collect()
}

/// Resolves a module/package path to a known file by trying the path itself
/// and then every trailing suffix of it. That covers Go and Ruby, where the
/// import string is module-qualified and the repo has no module index yet.
fn resolve_suffix(spec: &str, exts: &[&str], files: &HashSet<String>) -> Option<String> {
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

/// Imports stay pattern-based: a `use` path is a module specifier, not a
/// declaration, and resolving it needs the repo's file set rather than a
/// syntax tree. Only the symbols moved to the grammar.
fn parse_rust(path: &str, source: &str, files: &std::collections::HashSet<String>) -> ParsedFile {
    static USES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:pub\s+)?use\s+([^;{]+)").expect("use regex"));
    static MODS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;").expect("mod regex")
    });
    let (uses_re, mods_re) = (&*USES, &*MODS);

    let exports = symbols::exports(path, source).unwrap_or_default();

    let mut imports = Vec::new();
    for cap in uses_re.captures_iter(source) {
        let raw = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        if let Some(resolved) = resolve_rust_use(path, raw, files) {
            imports.push(resolved);
        }
    }
    for cap in mods_re.captures_iter(source) {
        let name = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Some(resolved) = resolve_child_module(path, name, files) {
            imports.push(resolved);
        }
    }
    finish(source, exports, imports)
}

fn parse_typescript(
    path: &str,
    source: &str,
    files: &std::collections::HashSet<String>,
) -> ParsedFile {
    static IMPORTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)(?:from|import)\s+['"](\.[^'"]+)['"]"#).expect("ts imports")
    });
    let imports_re = &*IMPORTS;
    let exports = symbols::exports(path, source).unwrap_or_default();
    let mut imports = Vec::new();
    for cap in imports_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Some(resolved) = resolve_relative(path, spec, files, &["ts", "tsx", "js", "jsx"]) {
            imports.push(resolved);
        }
    }
    finish(source, exports, imports)
}

fn parse_python(path: &str, source: &str, files: &std::collections::HashSet<String>) -> ParsedFile {
    static IMPORTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^from\s+(\.+[A-Za-z0-9_\.]*)\s+import").expect("py imports")
    });
    let imports_re = &*IMPORTS;
    let exports = symbols::exports(path, source).unwrap_or_default();
    let mut imports = Vec::new();
    for cap in imports_re.captures_iter(source) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Some(resolved) = resolve_python_relative(path, spec, files) {
            imports.push(resolved);
        }
    }
    finish(source, exports, imports)
}

fn resolve_rust_use(
    from: &str,
    raw: &str,
    files: &std::collections::HashSet<String>,
) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with("crate::") {
        let segs: Vec<&str> = raw.trim_start_matches("crate::").split("::").collect();
        return resolve_from_dir(&crate_root(from, files), &segs, files);
    }
    if raw.starts_with("super::") {
        let mut depth = 0;
        let mut rest = raw;
        while let Some(stripped) = rest.strip_prefix("super::") {
            depth += 1;
            rest = stripped;
        }
        let mut dir = parent(from)?;
        for _ in 0..depth {
            dir = parent(dir)?;
        }
        let segs: Vec<&str> = rest.split("::").collect();
        return resolve_from_dir(dir, &segs, files);
    }
    if let Some(rest) = raw.strip_prefix("self::") {
        let segs: Vec<&str> = rest.split("::").collect();
        return resolve_from_dir(parent(from).unwrap_or(""), &segs, files);
    }
    None
}

fn resolve_child_module(
    from: &str,
    name: &str,
    files: &std::collections::HashSet<String>,
) -> Option<String> {
    let dir = if from.ends_with("/lib.rs")
        || from.ends_with("/main.rs")
        || from.ends_with("lib.rs")
        || from.ends_with("main.rs")
    {
        parent(from).unwrap_or("")
    } else if let Some(stem) = from.strip_suffix(".rs") {
        stem
    } else {
        parent(from).unwrap_or("")
    };
    resolve_from_dir(dir, &[name], files)
}

fn crate_root(from: &str, files: &std::collections::HashSet<String>) -> String {
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

fn resolve_from_dir(
    dir: &str,
    segs: &[&str],
    files: &std::collections::HashSet<String>,
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

fn resolve_relative(
    from: &str,
    spec: &str,
    files: &std::collections::HashSet<String>,
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

fn resolve_python_relative(
    from: &str,
    spec: &str,
    files: &std::collections::HashSet<String>,
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

fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(head, _)| head)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn normalize_join(dir: &str, spec: &str) -> Option<String> {
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
