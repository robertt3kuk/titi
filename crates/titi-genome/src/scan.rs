use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const PRUNE_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "target",
    "coverage",
    "vendor",
    "__pycache__",
    ".git",
];

const MAX_FILE_BYTES: u64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct ListedFile {
    pub path: String,
    pub abs: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone)]
struct Rule {
    negated: bool,
    dir_only: bool,
    from_root: bool,
    pattern: String,
}

/// Lists the source files under `root`, which must be a readable directory.
///
/// The two cases are decided separately. A **root** that cannot be read as a
/// directory — a missing path, or a regular file where a directory is
/// required — is an error: the caller asked for a map of something that is
/// not a tree, and an empty map would be a lie. A **child** that cannot be
/// read mid-walk is skipped instead (see [`walk`]), so one odd entry cannot
/// sink an otherwise good tree.
pub fn list_files(root: &Path) -> std::io::Result<Vec<ListedFile>> {
    let mut rules = Vec::new();
    rules.extend(load_rules(root, ".gitignore"));
    rules.extend(load_rules(root, ".reference-productignore"));
    let mut out = Vec::new();
    walk(root, "", &rules, &mut out, true)?;
    Ok(out)
}

/// Stats one index-relative path, or `None` when it is not a file this index
/// would carry.
///
/// [`list_files`] decides those questions for a whole tree; this is the
/// single-path form a targeted update needs, and it applies the same filters —
/// source extension, a regular file, within the size cap — so a caller cannot
/// push into the index through a targeted update what a walk would have
/// skipped. `path` is a key of the index, relative to `root`, as
/// `Genome::files` spells it.
pub fn stat(root: &Path, path: &str) -> Option<ListedFile> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !is_source(name) {
        return None;
    }
    let abs = root.join(path);
    let meta = fs::metadata(&abs).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    Some(ListedFile {
        path: path.to_owned(),
        abs,
        size: meta.len(),
        mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    })
}

fn load_rules(root: &Path, name: &str) -> Vec<Rule> {
    let Ok(text) = fs::read_to_string(root.join(name)) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_rule).collect()
}

fn parse_rule(line: &str) -> Option<Rule> {
    let mut line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let negated = line.starts_with('!');
    if negated {
        line = line[1..].trim_start();
    }
    let dir_only = line.ends_with('/');
    if dir_only {
        line = line.trim_end_matches('/');
    }
    if line.is_empty() {
        return None;
    }
    let from_root = line.starts_with('/');
    if from_root {
        line = &line[1..];
    }
    Some(Rule {
        negated,
        dir_only,
        from_root,
        pattern: line.to_owned(),
    })
}

/// Walks `dir` into `out`.
///
/// `root` marks the top of the walk, and it is the only level that may fail:
/// the caller's root is a contract, so an unreadable one returns the error. A
/// directory that vanishes or is unreadable *below* the root is skipped, not
/// fatal, because a large tree with one odd entry must still index.
///
/// The `stat` per file happens here, in the walk's own order, and it is what a
/// listing costs: measured on 20k files it is ~25 ms of the ~33 ms a warm walk
/// takes, against ~8 ms of `readdir` and name filtering. A second pass that
/// read the metadata over several cores was measured too and did not pay for
/// itself (see the commit); the walk stays one pass.
fn walk(
    dir: &Path,
    rel: &str,
    rules: &[Rule],
    out: &mut Vec<ListedFile>,
    root: bool,
) -> std::io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) if !root => return Ok(()),
        Err(why) => return Err(why),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Control characters would break the line-oriented prompt projection.
        if name.chars().any(char::is_control) {
            continue;
        }
        let child_rel = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if should_prune_dir(&name) || is_ignored(&child_rel, true, rules) {
                continue;
            }
            walk(&entry.path(), &child_rel, rules, out, false)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        // The extension first: a name the language table does not know is
        // skipped whatever the ignore rules say, and matching every rule
        // against every entry is what a listing of a real tree spends its time
        // on (measured: 17.5 ms against 2.7 ms on this workspace's own tree,
        // 20 rules, 506 entries).
        if !is_source(&name) || is_ignored(&child_rel, false, rules) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        out.push(ListedFile {
            path: child_rel,
            abs: entry.path(),
            size: meta.len(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(())
}

fn should_prune_dir(name: &str) -> bool {
    name.starts_with('.') || PRUNE_DIRS.contains(&name)
}

fn is_source(name: &str) -> bool {
    // One source of truth: the language table in `lang` owns the extensions.
    crate::lang::is_source_file(name)
}

fn is_ignored(rel: &str, is_dir: bool, rules: &[Rule]) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule.matches(rel, is_dir) {
            ignored = !rule.negated;
        }
    }
    ignored
}

impl Rule {
    fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        // gitignore: a separator anywhere but the end anchors the pattern to the
        // ignore-file directory; otherwise it matches at any depth.
        if self.from_root || self.pattern.contains('/') {
            // `**/` in front of a pattern with no separator of its own says
            // "at any depth", which is the same as the last segment matching
            // the rest - and saves the grid for the shape `**/*.ext` that
            // ignore files are full of.
            if let Some(rest) = self.pattern.strip_prefix("**/") {
                // `**/` on its own is `**`: the walk's `k += 1` skips the
                // separator, so it matches anything, empty included.
                if rest.is_empty() {
                    return true;
                }
                if !rest.contains('/') && !rest.contains("**") {
                    return glob_match(rest, last_segment(rel));
                }
            }
            return glob_match(&self.pattern, rel);
        }
        // At any depth, but a `*` and a `?` stop at a separator and only `**`
        // crosses one: without `**` in the pattern the walk's suffix loop can
        // only ever succeed on the last segment, so it is a comparison rather
        // than a grid per suffix. This is the common case by far.
        if !self.pattern.contains("**") {
            let segment = last_segment(rel);
            if !self.pattern.contains(['*', '?']) {
                return segment == self.pattern;
            }
            return glob_match(&self.pattern, segment);
        }
        let mut rest = rel;
        loop {
            if glob_match(&self.pattern, rest) {
                return true;
            }
            match rest.split_once('/') {
                Some((_, tail)) => rest = tail,
                None => return false,
            }
        }
    }
}

/// gitignore-style match: `*` stops at `/`, `**` crosses it, `?` is one
/// non-`/` character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let n = pattern.len();
    let m = text.len();
    // One table, one allocation. This used to be a `Vec` per pattern character,
    // and a listing calls it once per rule per path: the allocations were most
    // of what a walk of a real tree cost.
    let mut dp = vec![false; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    dp[at(n, m)] = true;
    for i in (0..n).rev() {
        for j in (0..=m).rev() {
            dp[at(i, j)] = if pattern[i] == '*' {
                if i + 1 < n && pattern[i + 1] == '*' {
                    let mut k = i + 2;
                    if k < n && pattern[k] == '/' {
                        k += 1;
                    }
                    // `**` matches zero segments, or one more character.
                    dp[at(k, j)] || (j < m && dp[at(i, j + 1)])
                } else {
                    dp[at(i + 1, j)] || (j < m && text[j] != '/' && dp[at(i, j + 1)])
                }
            } else if j < m && (pattern[i] == text[j] || (pattern[i] == '?' && text[j] != '/')) {
                dp[at(i + 1, j + 1)]
            } else {
                false
            };
        }
    }
    dp[at(0, 0)]
}

/// The part of a relative path after its last separator; the whole path when
/// there is none.
fn last_segment(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, base)| base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_matches_one_segment() {
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "src/lib.rs"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/a/lib.rs"));
    }

    #[test]
    fn glob_double_star_crosses_segments() {
        assert!(glob_match("**/lib.rs", "src/lib.rs"));
        assert!(glob_match("**/lib.rs", "lib.rs"));
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("a/**/b", "a/b"));
        assert!(glob_match("a/**/b", "a/x/y/b"));
        assert!(!glob_match("a/**/b", "a/x/y/c"));
    }

    #[test]
    fn question_mark_never_crosses_a_separator() {
        assert!(glob_match("?", "a"));
        assert!(!glob_match("?", "/"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn unanchored_rule_matches_at_any_depth() {
        let rule = parse_rule("*.rs").unwrap();
        assert!(rule.matches("lib.rs", false));
        assert!(rule.matches("src/lib.rs", false));
        let target = parse_rule("target/").unwrap();
        assert!(target.matches("target", true));
        assert!(target.matches("crates/a/target", true));
        assert!(!target.matches("target", false));
        let anchored = parse_rule("src/*.rs").unwrap();
        assert!(anchored.matches("src/lib.rs", false));
        assert!(!anchored.matches("crates/src/lib.rs", false));
    }

    /// The suffix loop `matches` used to run, kept as the reference for the
    /// last-segment shortcuts it now takes.
    fn matches_slow(rule: &Rule, rel: &str, is_dir: bool) -> bool {
        if rule.dir_only && !is_dir {
            return false;
        }
        if rule.from_root || rule.pattern.contains('/') {
            return glob_match(&rule.pattern, rel);
        }
        let mut rest = rel;
        loop {
            if glob_match(&rule.pattern, rest) {
                return true;
            }
            match rest.split_once('/') {
                Some((_, tail)) => rest = tail,
                None => return false,
            }
        }
    }

    /// The shortcuts are only worth having if they answer exactly what the
    /// loop answered: a `*` and a `?` stop at a separator, `**` does not, and
    /// `**/` in front means any depth.
    #[test]
    fn the_last_segment_shortcuts_answer_what_the_suffix_loop_answered() {
        let patterns = [
            "*.rs",
            "*.profraw",
            "target",
            "/target",
            "a/b",
            "**/*.db",
            "**/x",
            "**",
            ".env",
            ".env.*",
            "src/*.rs",
            "**/*.d?",
            "a**b",
            "*.db*",
            "x/y/*.c",
            "build",
            "a?c",
            "**/node_modules",
            "*",
            "**/*",
            "**/",
            "**/*.rs",
        ];
        let paths = [
            "a.rs",
            "src/a.rs",
            "src/deep/a.rs",
            "a.profraw",
            "src/a.profraw",
            "target",
            "src/target",
            "a/b",
            "x/a/b",
            "a/b/c.rs",
            "src/a.db",
            "a.db",
            "db",
            ".env",
            "src/.env",
            ".env.local",
            "src/lib.rs",
            "aYb",
            "x/aYb",
            "aXc",
            "abc",
            "src/aXc",
            "build/x.rs",
            "a.c",
            "src/a.c",
            "node_modules",
            "x/node_modules/y",
            "**",
            "a**b/c",
            "src/x/y/c.c",
            "x/y/z",
            "x/y",
            "x",
            "",
        ];
        for pattern in patterns {
            for path in paths {
                for is_dir in [false, true] {
                    for from_root in [false, true] {
                        let rule = Rule {
                            negated: false,
                            dir_only: false,
                            from_root,
                            pattern: pattern.to_owned(),
                        };
                        assert_eq!(
                            rule.matches(path, is_dir),
                            matches_slow(&rule, path, is_dir),
                            "{pattern:?} against {path:?} (dir: {is_dir}, anchored: {from_root})"
                        );
                    }
                }
            }
        }
    }
}
