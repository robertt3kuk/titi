//! Which definition a mention names.
//!
//! The first version of the graph resolved a mention by its *name*: a file
//! that wrote `sync` leaned on whichever file exported a `sync`, whether it
//! had `use std::sync::Arc` in mind or the method. Any file exporting a common
//! word collected hundreds of dependents and the rank to go with them.
//!
//! A use of `N` in file `F` now resolves to definer `D` only on evidence:
//!
//! - **Scope.** `D` is in `F`'s scope by the language's own rules — reached
//!   through `F`'s resolved imports, a re-export (`pub use`, a barrel's
//!   `export … from`) of one of them, another crate's `lib.rs` that `F` names
//!   (Rust), the glob of an ancestor module (`use super::*`), or the same
//!   package directory (Go, Java, Kotlin, C#). Python, TypeScript, JavaScript,
//!   PHP, Ruby, Swift and C reach only through imports and includes.
//! - **Unique and distinctive.** No evidence, but exactly one file in the
//!   workspace defines the name, and the name is long and uncommon enough that
//!   a coincidence is unlikely. That is a guess, and carries a lower weight.
//!
//! A common word (`sync`, `model`, `row`) is held to the stricter half of the
//! first rule: it resolves only to a file the mentioning file imports itself,
//! or one of its own package, never to one it merely reaches through a
//! re-export or another crate's facade, where the chance that the mention is
//! the standard library's or a local's is far higher.
//!
//! Several definers in scope, or several with none in scope, resolve to
//! nothing: guessing between them would invent a dependency.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::{Basis, FileRecord, Language, SymbolRecord, SymbolUse};

/// Names shorter than this are never guessed.
const DISTINCTIVE_LEN: usize = 5;

/// Words that name something in a hundred unrelated places. A file that
/// defines one of these is not made the definition of every mention of it.
const COMMON: &[&str] = &[
    "apply", "build", "check", "clear", "close", "color", "config", "context", "create", "data",
    "default", "delete", "empty", "error", "event", "failed", "files", "finished", "first", "from",
    "handle", "index", "input", "insert", "items", "label", "model", "name", "number", "output",
    "page", "parse", "path", "print", "range", "raw", "remove", "render", "reset", "result",
    "route", "row", "select", "start", "state", "status", "string", "style", "sync", "table",
    "title", "update", "value", "write",
];

/// Whether a name defined once may be linked without evidence.
pub(crate) fn is_distinctive(name: &str) -> bool {
    name.chars().count() >= DISTINCTIVE_LEN && !is_common(name)
}

pub(crate) fn is_common(name: &str) -> bool {
    COMMON.contains(&name.to_ascii_lowercase().as_str())
}

/// Whether a file of `from` can use what a file of `to` defines: the same
/// language, or the two halves of a family that includes and imports across
/// (C and C++, TypeScript and JavaScript).
fn same_family(from: Language, to: Language) -> bool {
    from == to
        || matches!(
            (from, to),
            (Language::C, Language::Cpp)
                | (Language::Cpp, Language::C)
                | (Language::TypeScript, Language::JavaScript)
                | (Language::JavaScript, Language::TypeScript)
        )
}

/// The languages whose files see each other by directory: a Go package, a
/// Java or Kotlin package, a C# namespace.
fn shares_package(language: Language) -> bool {
    matches!(
        language,
        Language::Go | Language::Java | Language::Kotlin | Language::CSharp
    )
}

fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// What the set of paths says about scope, independent of what any file
/// contains: rebuilt when a path appears or vanishes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Layout {
    /// Rust crate name (`titi_providers`) → its `src/lib.rs`. A name two
    /// directories would claim is dropped: an evidence that can be wrong in
    /// either direction is none.
    crates: HashMap<String, String>,
    /// `(language, directory)` → files, for the package languages.
    packages: HashMap<(Language, String), Vec<String>>,
}

impl Layout {
    /// `root` is where the index's paths are relative to. A crate is named by
    /// its manifest, not by its directory — `crates/core` is `foo-core`, and
    /// `use foo_core::X` is how everything else spells it — so the manifest
    /// next to a `src/lib.rs` is read when there is one, and the directory
    /// name stands in when there is not.
    pub(crate) fn build(files: &HashMap<String, FileRecord>, root: &Path) -> Self {
        let mut crates: HashMap<String, Option<String>> = HashMap::new();
        let mut packages: HashMap<(Language, String), Vec<String>> = HashMap::new();
        for (path, record) in files {
            if let Some(dir) = path.strip_suffix("/src/lib.rs") {
                let name = crate_name(root, dir);
                crates
                    .entry(name)
                    .and_modify(|slot| *slot = None)
                    .or_insert_with(|| Some(path.clone()));
            }
            if shares_package(record.language) {
                packages
                    .entry((record.language, directory(path).to_owned()))
                    .or_default()
                    .push(path.clone());
            }
        }
        Self {
            crates: crates
                .into_iter()
                .filter_map(|(name, path)| Some((name, path?)))
                .collect(),
            packages,
        }
    }

    /// The `lib.rs` of each workspace crate `record` names.
    pub(crate) fn facades<'a>(&'a self, record: &'a FileRecord) -> impl Iterator<Item = &'a str> {
        record
            .external_uses
            .iter()
            .filter_map(|name| self.crates.get(name))
            .map(String::as_str)
    }

    fn package_of(&self, record: &FileRecord) -> &[String] {
        self.packages
            .get(&(record.language, directory(&record.path).to_owned()))
            .map_or(&[], Vec::as_slice)
    }
}

/// The name other crates write for the crate whose `src/lib.rs` is under
/// `dir`: `[lib] name`, else `[package] name`, else the directory's name, with
/// `-` as `_` the way a path spells it.
fn crate_name(root: &Path, dir: &str) -> String {
    let from_manifest = std::fs::read_to_string(root.join(dir).join("Cargo.toml"))
        .ok()
        .and_then(|manifest| manifest_name(&manifest));
    from_manifest
        .unwrap_or_else(|| dir.rsplit('/').next().unwrap_or(dir).to_owned())
        .replace('-', "_")
}

/// The `name = "…"` of `[lib]` if it has one, otherwise of `[package]`.
fn manifest_name(manifest: &str) -> Option<String> {
    let mut table = "";
    let (mut package, mut lib) = (None, None);
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            table = header.trim_end_matches(']').trim();
            continue;
        }
        let Some(("name", value)) = line.split_once('=').map(|(key, value)| (key.trim(), value)) else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).to_owned();
        match table {
            "package" => package = Some(value),
            "lib" => lib = Some(value),
            _ => {}
        }
    }
    lib.or(package)
}

/// Resolves a file's mentions against the workspace's definers.
pub(crate) struct Resolver<'a> {
    pub files: &'a HashMap<String, FileRecord>,
    pub symbols: &'a HashMap<String, SymbolRecord>,
    pub layout: &'a Layout,
}

impl Resolver<'_> {
    /// The uses `record` resolves to, in the order of its raw mentions.
    ///
    /// A name the file defines itself is its own definition, and a name nobody
    /// exports is not a symbol of this workspace; neither is a use.
    pub(crate) fn resolve<'r>(&'r self, record: &'r FileRecord) -> Vec<SymbolUse> {
        let mut scope: Option<Scope<'r>> = None;
        let mut uses = Vec::new();
        for name in &record.raw_refs {
            let Some(symbol) = self.symbols.get(name) else {
                continue;
            };
            if record.exports.binary_search(name).is_ok() {
                continue;
            }
            let reach = scope.get_or_insert_with(|| self.scope(record));
            let reachable = if is_common(name) {
                &reach.direct
            } else {
                &reach.all
            };
            let mut seen: Vec<&String> = symbol
                .files
                .iter()
                .filter(|definer| **definer != record.path && reachable.contains(definer.as_str()))
                .collect();
            if seen.len() > 1 {
                self.drop_pass_throughs(&mut seen);
            }
            let basis_and_definer = match seen.as_slice() {
                [definer] => Some((*definer, Basis::Scope)),
                [_, _, ..] => None,
                [] => match symbol.files.as_slice() {
                    [only]
                        if *only != record.path
                            && is_distinctive(name)
                            && self
                                .files
                                .get(only)
                                .is_some_and(|d| same_family(record.language, d.language)) =>
                    {
                        Some((only, Basis::Unique))
                    }
                    _ => None,
                },
            };
            if let Some((definer, basis)) = basis_and_definer {
                uses.push(SymbolUse {
                    name: name.clone(),
                    definer: definer.clone(),
                    basis,
                });
            }
        }
        uses
    }

    /// A barrel that re-exports `export { x } from './x'` lists `x` among its
    /// own exports. When it and the file it re-exports are both in scope for
    /// one name they are one definition, and the barrel is only the road to
    /// it: drop whichever candidate re-publishes another.
    fn drop_pass_throughs(&self, candidates: &mut Vec<&String>) {
        let all: Vec<&String> = candidates.clone();
        candidates.retain(|candidate| {
            let behind = self.reexported_from(candidate);
            !all.iter()
                .any(|other| other != candidate && behind.contains(other.as_str()))
        });
    }

    /// Every file `path` re-publishes, directly or through other re-exports.
    fn reexported_from<'r>(&'r self, path: &'r str) -> HashSet<&'r str> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = self
            .files
            .get(path)
            .map(|record| record.reexports.iter().map(String::as_str).collect())
            .unwrap_or_default();
        while let Some(next) = stack.pop() {
            if seen.insert(next)
                && let Some(record) = self.files.get(next)
            {
                stack.extend(record.reexports.iter().map(String::as_str));
            }
        }
        seen
    }

    /// Every file whose definitions `record` can name without guessing.
    fn scope<'r>(&'r self, record: &'r FileRecord) -> Scope<'r> {
        let mut direct: HashSet<&'r str> = HashSet::new();
        let mut roots: Vec<&'r str> = Vec::new();
        let mut facades: Vec<&'r str> = Vec::new();
        self.push_roots(record, &mut roots, &mut facades, &mut HashSet::new());
        direct.extend(roots.iter().copied());
        if shares_package(record.language) {
            direct.extend(self.layout.package_of(record).iter().map(String::as_str));
            // A Go import names a package, and every file of the directory is
            // that package; Java, Kotlin and C# import one type at a time.
            if record.language == Language::Go {
                for import in &record.imports {
                    if let Some(target) = self.files.get(import) {
                        direct.extend(self.layout.package_of(target).iter().map(String::as_str));
                    }
                }
            }
        }
        let mut all: HashSet<&'r str> = HashSet::new();
        let mut stack: Vec<&'r str> = direct.iter().copied().chain(facades).collect();
        while let Some(path) = stack.pop() {
            if !all.insert(path) {
                continue;
            }
            if let Some(target) = self.files.get(path) {
                stack.extend(target.reexports.iter().map(String::as_str));
            }
        }
        Scope { direct, all }
    }

    /// The files `record` imports directly, and — through `use super::*` —
    /// what its ancestors import; the `lib.rs` of the crates it names go to
    /// `facades`, which are reached but not imported.
    fn push_roots<'r>(
        &'r self,
        record: &'r FileRecord,
        roots: &mut Vec<&'r str>,
        facades: &mut Vec<&'r str>,
        inherited: &mut HashSet<&'r str>,
    ) {
        roots.extend(record.imports.iter().map(String::as_str));
        facades.extend(
            record
                .external_uses
                .iter()
                .filter_map(|root| self.layout.crates.get(root))
                .map(String::as_str),
        );
        for ancestor in &record.inherits {
            if !inherited.insert(ancestor.as_str()) {
                continue;
            }
            roots.push(ancestor);
            if let Some(target) = self.files.get(ancestor) {
                self.push_roots(target, roots, facades, inherited);
            }
        }
    }
}

/// What one file can reach: `direct` is what it imports, inherits or shares a
/// package with; `all` adds what re-exports and crate facades lead to.
struct Scope<'r> {
    direct: HashSet<&'r str>,
    all: HashSet<&'r str>,
}
