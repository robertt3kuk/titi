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
        if !file_type.is_file() || is_ignored(&child_rel, false, rules) {
            continue;
        }
        if !is_source(&name) {
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
            return glob_match(&self.pattern, rel);
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
    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[n][m] = true;
    for i in (0..n).rev() {
        for j in (0..=m).rev() {
            dp[i][j] = if pattern[i] == '*' {
                if i + 1 < n && pattern[i + 1] == '*' {
                    let mut k = i + 2;
                    if k < n && pattern[k] == '/' {
                        k += 1;
                    }
                    // `**` matches zero segments, or one more character.
                    dp[k][j] || (j < m && dp[i][j + 1])
                } else {
                    dp[i + 1][j] || (j < m && text[j] != '/' && dp[i][j + 1])
                }
            } else if j < m && (pattern[i] == text[j] || (pattern[i] == '?' && text[j] != '/')) {
                dp[i + 1][j + 1]
            } else {
                false
            };
        }
    }
    dp[0][0]
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
}
