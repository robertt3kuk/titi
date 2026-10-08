//! Layered dotenv resolution.
//!
//! Priority (highest wins): process env → `<agent_dir>/.env` →
//! `<project>/.env`. A later layer fills only
//! keys that are still unset; the process environment is never mutated by
//! resolution — injection happens only through [`load_into`]'s mutator.
//!
//! The project layer is deliberately last: a cloned repository must not
//! choose which credential a request carries, nor shadow a key the user's
//! own agent layer defines. It may only add keys nobody else defines. This
//! mirrors `titi_config::Settings::get_user` (`crates/titi-config/src/settings.rs`),
//! where the project file can add but never override the user's own layer.
//!
//! Spec: docs/research/secrets-env/README.md.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Layered resolver bound to explicit directories (tests substitute their own
/// directories here instead of relying on the environment).
#[derive(Debug, Clone)]
pub struct LayeredEnv {
    project_dir: PathBuf,
    agent_dir: PathBuf,
}

impl LayeredEnv {
    /// `project_dir` holds the launch-directory `.env`; `agent_dir` the
    /// per-instance one.
    pub fn new(project_dir: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
            agent_dir: agent_dir.into(),
        }
    }

    /// Defaults: project = current directory, agent = `titi-config::agent_dir()`.
    pub fn from_defaults() -> Self {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(project_dir, titi_config::agent_dir())
    }

    /// Highest priority first; the project layer resolves last so a cloned
    /// repository can fill a key nobody else defines but never override the
    /// agent's own layer (same precedence as `titi_config::Settings::get_user`).
    fn layer_files(&self) -> [PathBuf; 2] {
        [self.agent_dir.join(".env"), self.project_dir.join(".env")]
    }

    /// Resolve `key` without mutating the process environment.
    pub fn resolve(&self, key: &str) -> Option<String> {
        if let Some(value) = std::env::var_os(key) {
            return value.into_string().ok();
        }
        self.layer_files()
            .into_iter()
            .find_map(|path| read_layer(&path).remove(key))
    }

    /// Inject every key that is absent from the process environment, in layer
    /// order (agent → project); the first file that defines a
    /// key wins. Values reach the process only through `inject` (e.g.
    /// `|k, v| std::env::set_var(k, v)` in a controlled entry point).
    pub fn load_into(&self, mut inject: impl FnMut(String, String)) {
        let mut seen = HashSet::new();
        for map in self.layer_files().into_iter().map(|p| read_layer(&p)) {
            for (key, value) in map {
                if !seen.insert(key.clone()) {
                    continue;
                }
                if std::env::var_os(&key).is_none() {
                    inject(key, value);
                }
            }
        }
    }
}

/// Free-function resolve over the default layers.
pub fn resolve(key: &str) -> Option<String> {
    LayeredEnv::from_defaults().resolve(key)
}

/// Free-function injection over the default layers.
pub fn load_into(mutator: impl FnMut(String, String)) {
    LayeredEnv::from_defaults().load_into(mutator)
}

/// Parse `.env` text into a key → value map (later duplicates replace earlier
/// ones within the same file). Malformed lines are skipped, never fatal.
pub fn parse(src: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !is_identifier(key) {
            continue;
        }
        map.insert(key.to_string(), parse_value(value.trim()));
    }
    map
}

/// Shell-identifier rule: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_value(value: &str) -> String {
    match value.chars().next() {
        Some('\'') => match value[1..].find('\'') {
            // Single quotes: literal, no escape processing.
            Some(end) => value[1..1 + end].to_string(),
            None => strip_inline_comment(value), // unterminated: raw
        },
        Some('"') => match closing_double_quote(value) {
            Some(end) => unescape_double(&value[1..end]), // escapes processed
            None => strip_inline_comment(value),          // unterminated: raw
        },
        _ => strip_inline_comment(value),
    }
}

/// Index of the real closing `"` (escaped ones are skipped).
fn closing_double_quote(value: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(i);
        }
    }
    None
}

/// Unquoted: drop a trailing inline comment (` # …`).
fn strip_inline_comment(value: &str) -> String {
    match value.find(" #") {
        Some(i) => value[..i].trim_end().to_string(),
        None => value.to_string(),
    }
}

fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            // Unknown sequences survive verbatim (`\$` stays `\$`).
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn read_layer(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .map(|src| parse(&src))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir failed: {e}"))
    }

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| panic!("no parent")))
            .unwrap_or_else(|e| panic!("mkdir failed: {e}"));
        std::fs::write(path, content).unwrap_or_else(|e| panic!("write failed: {e}"));
    }

    #[test]
    fn parser_handles_quotes_comments_export_and_crlf() {
        let src = "# full-line comment\r\n\r\nPLAIN=value\r\nexport EXPORTED=exported\r\nSINGLE='lit # eral'\nDOUBLE=\"line\\nbreak \\\"q\\\" \\$dollar\"\r\nUNQUOTED=tail # trailing comment\r\nEMPTY=\r\n";
        let map = parse(src);
        assert_eq!(map.get("PLAIN").map(String::as_str), Some("value"));
        assert_eq!(map.get("EXPORTED").map(String::as_str), Some("exported"));
        assert_eq!(map.get("SINGLE").map(String::as_str), Some("lit # eral"));
        assert_eq!(
            map.get("DOUBLE").map(String::as_str),
            Some("line\nbreak \"q\" \\$dollar")
        );
        assert_eq!(map.get("UNQUOTED").map(String::as_str), Some("tail"));
        assert_eq!(map.get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn parser_skips_malformed_lines_and_invalid_identifiers() {
        let map = parse("NO_EQUALS_SIGN\n1BAD=x\nBAD-KEY=y\nBAD.KEY=z\nGOOD=ok\n");
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("GOOD").map(String::as_str), Some("ok"));
    }

    #[test]
    fn parser_accepts_underscore_leading_names() {
        assert!(parse("_PRIVATE=a").contains_key("_PRIVATE"));
        assert!(parse("__x_1=y").contains_key("__x_1"));
    }

    #[test]
    fn priority_process_env_beats_files() {
        // `PATH` always exists in the test process; a file layer must not win.
        let dir = tmpdir();
        write_file(dir.path(), ".env", "PATH=from_file\n");
        let env = LayeredEnv::new(dir.path(), dir.path().join("agent"));
        let resolved = env.resolve("PATH");
        assert_ne!(resolved.as_deref(), Some("from_file"));
        assert!(resolved.is_some_and(|v| !v.is_empty()));
    }

    #[test]
    fn priority_layers_agent_then_project() {
        let dir = tmpdir();
        write_file(dir.path(), ".env", "SHARED=project\nONLY_PROJECT=p\n");
        write_file(dir.path(), "agent/.env", "SHARED=agent\nONLY_AGENT=a\n");
        let env = LayeredEnv::new(dir.path(), dir.path().join("agent"));
        // Agent's own layer outranks the project's: a cloned repo cannot
        // choose the credential.
        assert_eq!(env.resolve("SHARED").as_deref(), Some("agent"));
        // The project layer can still fill a key nobody else defines.
        assert_eq!(env.resolve("ONLY_PROJECT").as_deref(), Some("p"));
        assert_eq!(env.resolve("ONLY_AGENT").as_deref(), Some("a"));
        assert_eq!(env.resolve("MISSING"), None);
    }

    #[test]
    fn load_into_agrees_with_resolve_for_doubly_defined_key() {
        let dir = tmpdir();
        write_file(dir.path(), ".env", "SHARED=project\n");
        write_file(dir.path(), "agent/.env", "SHARED=agent\n");
        let env = LayeredEnv::new(dir.path(), dir.path().join("agent"));
        let resolved = env.resolve("SHARED");
        let mut injected = BTreeMap::new();
        env.load_into(|k, v| {
            injected.insert(k, v);
        });
        assert_eq!(resolved.as_deref(), Some("agent"));
        // A tool's child process must see the same key the request carries.
        assert_eq!(injected.get("SHARED"), resolved.as_ref());
    }

    #[test]
    fn load_into_injects_only_missing_keys_once_with_winner_value() {
        let dir = tmpdir();
        write_file(dir.path(), ".env", "SHARED=project\nPATH=from_file\n");
        write_file(dir.path(), "agent/.env", "SHARED=agent\nAGENT_ONLY=1\n");
        let env = LayeredEnv::new(dir.path(), dir.path().join("agent"));
        let mut injected = BTreeMap::new();
        env.load_into(|k, v| {
            injected.insert(k, v);
        });
        // Process-env `PATH` is never injected; file keys are.
        assert_eq!(injected.get("SHARED").map(String::as_str), Some("agent"));
        assert_eq!(injected.get("AGENT_ONLY").map(String::as_str), Some("1"));
        assert!(!injected.contains_key("PATH"));
        assert_eq!(std::env::var_os("SHARED"), None);
        assert_eq!(std::env::var_os("AGENT_ONLY"), None);
    }

    #[test]
    fn load_into_never_mutates_process_env() {
        let dir = tmpdir();
        write_file(dir.path(), ".env", "TITI_SECRETS_PROBE=x\n");
        let env = LayeredEnv::new(dir.path(), dir.path().join("agent"));
        env.load_into(|_, _| ());
        assert_eq!(std::env::var_os("TITI_SECRETS_PROBE"), None);
        assert_eq!(env.resolve("TITI_SECRETS_PROBE").as_deref(), Some("x"));
    }
}
