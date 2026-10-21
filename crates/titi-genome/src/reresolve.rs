//! A second look at the imports a language module could not place.
//!
//! The language modules resolve a specifier by their own rules and keep what
//! they cannot place as an unresolved specifier with the paths it would have
//! named. Two ecosystems spell imports in ways those rules miss, and both
//! produced a warning on a file that has nothing wrong with it and an edge
//! that was never drawn:
//!
//! - **JavaScript and TypeScript.** ESM source names the *output*: `./a.js`
//!   is the TypeScript file `./a.ts`, and `..`, `../..` and `./util` may name
//!   a directory whose `index` is meant.
//! - **Go.** An import path is module-qualified, and the package is a
//!   directory: `github.com/spf13/cobra/doc` is `doc/` when the module is
//!   `github.com/spf13/cobra`, and a path outside the module is another
//!   module's, not a file this workspace is missing.
//!
//! This runs once per parse, on what the module left unresolved, so nothing
//! that resolved is touched and the cost is nil for a file with no misses.

use std::collections::HashSet;
use std::path::Path;

use crate::lang::ParsedFile;
use crate::lang::support::{first_known, normalize_join, parent};
use crate::{Candidate, Language};

/// The extensions a JavaScript or TypeScript `index` may carry, the
/// TypeScript ones first.
const INDEX_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// The source files an emitted extension may stand for.
const SOURCE_FOR_OUTPUT: &[(&str, &[&str])] = &[
    ("js", &["ts", "tsx"]),
    ("jsx", &["tsx"]),
    ("mjs", &["mts"]),
    ("cjs", &["cts"]),
];

/// Fold what the second look finds into `parsed`.
///
/// `root` is the index root, `path` the file's key under it and `source` its
/// text; `known` is every path the index holds.
pub(crate) fn retry(
    root: &Path,
    path: &str,
    language: Language,
    source: &str,
    parsed: &mut ParsedFile,
    known: &HashSet<String>,
) {
    if parsed.unresolved_candidates.is_empty() {
        return;
    }
    match language {
        Language::TypeScript | Language::JavaScript => script(path, source, parsed, known),
        Language::Go => go(root, parsed, known),
        _ => {}
    }
}

fn script(path: &str, source: &str, parsed: &mut ParsedFile, known: &HashSet<String>) {
    let dir = parent(path).unwrap_or("");
    let mut settled: Vec<(String, String)> = Vec::new();
    for import in &mut parsed.unresolved_candidates {
        if !import.spec.starts_with('.') {
            continue;
        }
        let Some(joined) = normalize_join(dir, &import.spec) else {
            continue;
        };
        let mut extra: Vec<Candidate> = Vec::new();
        if let Some((stem, ext)) = joined.rsplit_once('.')
            && let Some((_, sources)) = SOURCE_FOR_OUTPUT.iter().find(|(output, _)| *output == ext)
        {
            extra.extend(
                sources
                    .iter()
                    .map(|source| Candidate::Exact(format!("{stem}.{source}"))),
            );
        }
        for ext in INDEX_EXTENSIONS {
            extra.push(Candidate::Exact(if joined.is_empty() {
                format!("index.{ext}")
            } else {
                format!("{joined}/index.{ext}")
            }));
        }
        match first_known(&extra, known) {
            Some(target) => settled.push((import.spec.clone(), target)),
            None => import.candidates.extend(extra),
        }
    }
    for (spec, target) in settled {
        fold(parsed, &spec, target.clone());
        if exports_from(source, &spec) {
            parsed.reexports.push(target);
        }
    }
    parsed.reexports.sort();
    parsed.reexports.dedup();
}

/// Whether `source` re-exports from `spec`: `export … from './x'`.
fn exports_from(source: &str, spec: &str) -> bool {
    let pattern = format!(r#"export\s[^;]*?from\s*['"]{}['"]"#, regex::escape(spec));
    regex::Regex::new(&pattern).is_ok_and(|pattern| pattern.is_match(source))
}

fn go(root: &Path, parsed: &mut ParsedFile, known: &HashSet<String>) {
    let Some(module) = std::fs::read_to_string(root.join("go.mod"))
        .ok()
        .and_then(|manifest| module_path(&manifest))
    else {
        return;
    };
    let specs: Vec<String> = parsed
        .unresolved_candidates
        .iter()
        .map(|import| import.spec.clone())
        .collect();
    for spec in specs {
        let Some(rest) = within_module(&module, &spec) else {
            // Another module's package: a dependency, not a missing file.
            drop_spec(parsed, &spec);
            continue;
        };
        if let Some(target) = package_file(rest, known) {
            fold(parsed, &spec, target);
        }
    }
}

/// `rest` of `spec` below `module`, `""` for the module's root package.
fn within_module<'a>(module: &str, spec: &'a str) -> Option<&'a str> {
    if spec == module {
        return Some("");
    }
    spec.strip_prefix(module)?.strip_prefix('/')
}

/// The `module` line of a `go.mod`.
fn module_path(manifest: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("module")?;
        let rest = rest.strip_prefix([' ', '\t'])?.trim();
        Some(rest.trim_matches('"').to_owned()).filter(|module| !module.is_empty())
    })
}

/// The file that stands for the Go package in `dir`: the one named like the
/// directory when there is one, else the first non-test file by name.
fn package_file(dir: &str, known: &HashSet<String>) -> Option<String> {
    let name = dir.rsplit('/').next().unwrap_or("");
    let mut files: Vec<&String> = known
        .iter()
        .filter(|path| {
            path.ends_with(".go")
                && !path.ends_with("_test.go")
                && parent(path).unwrap_or("") == dir
        })
        .collect();
    files.sort();
    let like_dir = format!("{dir}/{name}.go");
    files
        .iter()
        .find(|path| ***path == like_dir)
        .or_else(|| files.first())
        .map(|path| (*path).clone())
}

/// `spec` resolved to `target`: an import now, no longer a miss.
fn fold(parsed: &mut ParsedFile, spec: &str, target: String) {
    drop_spec(parsed, spec);
    parsed.imports.push(target);
    parsed.imports.sort();
    parsed.imports.dedup();
}

fn drop_spec(parsed: &mut ParsedFile, spec: &str) {
    parsed.unresolved_imports.retain(|known| known != spec);
    parsed
        .unresolved_candidates
        .retain(|import| import.spec != spec);
}

#[cfg(test)]
mod tests {
    use super::{module_path, within_module};

    #[test]
    fn the_module_line_is_read_whatever_follows() {
        assert_eq!(
            module_path("// c\nmodule github.com/spf13/cobra\n\ngo 1.21\n").as_deref(),
            Some("github.com/spf13/cobra")
        );
        assert_eq!(
            module_path("module \"x.io/y\"\n").as_deref(),
            Some("x.io/y")
        );
        assert_eq!(module_path("go 1.21\n"), None);
    }

    #[test]
    fn a_path_is_in_a_module_only_at_a_segment_boundary() {
        assert_eq!(within_module("a.io/m", "a.io/m/doc"), Some("doc"));
        assert_eq!(within_module("a.io/m", "a.io/m"), Some(""));
        assert_eq!(within_module("a.io/m", "a.io/mx/doc"), None);
        assert_eq!(within_module("a.io/m", "b.io/other"), None);
    }
}
