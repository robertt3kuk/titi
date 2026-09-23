use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use serde_json::Value;
use titi_providers::ToolSpec;

use crate::cache::ReadCache;
use crate::hashline::HashlineEditTool;
use crate::pty::{self, Interrupt, Options as PtyOptions};
use crate::sensitive::SensitivePolicy;
use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

pub(crate) fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

pub(crate) fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn jail_path(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let resolved = if candidate.exists() {
        fs::canonicalize(&candidate).map_err(|error| error.to_string())?
    } else if let Some(parent) = candidate.parent() {
        let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        parent.join(candidate.file_name().unwrap_or_default())
    } else {
        candidate
    };
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(format!("{} is outside the workspace", raw))
    }
}

/// `jail_path` for a tool that returns file contents to the model: the file
/// must also not be a credential, checked on both the name asked for and the
/// real path, so a harmless-looking link to `.env` is refused as well.
pub(crate) fn readable_path(
    root: &Path,
    raw: &str,
    policy: &SensitivePolicy,
) -> Result<PathBuf, String> {
    let resolved = jail_path(root, raw)?;
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let relative = resolved.strip_prefix(&canonical_root).unwrap_or(&resolved);
    if policy.blocks(Path::new(raw)) || policy.blocks(relative) {
        return Err(format!(
            "{raw} holds credentials; titi does not send it to the model"
        ));
    }
    Ok(resolved)
}

pub(crate) fn ok(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into().into(),
        is_error: false,
    }
}

pub(crate) fn err(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into().into(),
        is_error: true,
    }
}

#[derive(Clone)]
pub struct WorkspaceRoot(pub PathBuf);

impl WorkspaceRoot {
    pub fn current() -> Self {
        Self(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

pub struct ReadFileTool {
    pub root: PathBuf,
    /// Shared across every agent in a dispatch, so the second read of the
    /// same file is a memory hit.
    pub cache: ReadCache,
    /// Which files hold credentials and are refused.
    pub policy: SensitivePolicy,
}

#[async_trait]
impl ToolHandler for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "read".into(),
                description: "Read a UTF-8 file from the workspace".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return err("missing path");
        };
        match readable_path(&self.root, &path, &self.policy).and_then(|path| self.cache.read(&path))
        {
            Ok(content) => ok(content),
            Err(error) => err(error),
        }
    }
}

pub struct WriteFileTool {
    pub root: PathBuf,
    /// Invalidated on write, so the next read sees the new body.
    pub cache: ReadCache,
}

#[async_trait]
impl ToolHandler for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "write".into(),
                description: "Write a UTF-8 file in the workspace".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
            approval: ApprovalTier::Write,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return err("missing path");
        };
        let Some(content) = arg_str(&args, "content") else {
            return err("missing content");
        };
        match jail_path(&self.root, &path).and_then(|path| {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(&path, content).map_err(|error| error.to_string())?;
            self.cache.invalidate(&path);
            Ok(path.display().to_string())
        }) {
            Ok(written) => ok(format!("wrote {written}")),
            Err(error) => err(error),
        }
    }
}

pub struct EditFileTool {
    pub root: PathBuf,
    /// Invalidated on edit, so the next read sees the new body.
    pub cache: ReadCache,
    /// Which files hold credentials and are refused.
    pub policy: SensitivePolicy,
}

#[async_trait]
impl ToolHandler for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "edit".into(),
                description: "Replace one occurrence of old_string with new_string".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            approval: ApprovalTier::Write,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return err("missing path");
        };
        let Some(old) = arg_str(&args, "old_string") else {
            return err("missing old_string");
        };
        let Some(new) = arg_str(&args, "new_string") else {
            return err("missing new_string");
        };
        match readable_path(&self.root, &path, &self.policy).and_then(|path| {
            let content = fs::read_to_string(&path).map_err(|error| error.to_string())?;
            if !content.contains(&old) {
                return Err("old_string not found".into());
            }
            let updated = content.replacen(&old, &new, 1);
            fs::write(&path, updated).map_err(|error| error.to_string())?;
            Ok(())
        }) {
            Ok(()) => ok("edited"),
            Err(error) => err(error),
        }
    }
}

pub struct GlobTool {
    pub root: PathBuf,
}

#[async_trait]
impl ToolHandler for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "glob".into(),
                description: "List workspace files whose names contain a substring".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "pattern": { "type": "string" } },
                    "required": ["pattern"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let pattern = arg_str(&args, "pattern").unwrap_or_default();
        let mut matches = Vec::new();
        walk(&self.root, &self.root, &pattern, &mut matches);
        matches.sort();
        ok(matches.join("\n"))
    }
}

pub struct GrepTool {
    pub root: PathBuf,
    /// Which files hold credentials and are skipped.
    pub policy: SensitivePolicy,
}

#[async_trait]
impl ToolHandler for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "grep".into(),
                description: "Search workspace files for a substring".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string" }
                    },
                    "required": ["pattern"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(pattern) = arg_str(&args, "pattern") else {
            return err("missing pattern");
        };
        let start = arg_str(&args, "path")
            .and_then(|path| jail_path(&self.root, &path).ok())
            .unwrap_or_else(|| self.root.clone());
        let mut hits = Vec::new();
        grep_walk(&self.root, &start, &pattern, &self.policy, &mut hits);
        ok(hits.join("\n"))
    }
}

pub struct BashTool {
    pub root: PathBuf,
    /// Raised to stop the command a `pty` run is waiting on. Held by the
    /// surface that owns the cancel key; the pipe path below cannot be
    /// interrupted, which is half of why `pty` exists.
    pub interrupt: Interrupt,
}

#[async_trait]
impl ToolHandler for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "bash".into(),
                description: "Run a shell command in the workspace".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "pty": {
                            "type": "boolean",
                            "description": "Run under a terminal, so the command sees a tty \
                                            and can be timed out and interrupted. Default false: \
                                            without it output comes back unwrapped and uncoloured."
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Deadline for a pty run, 1..3600, default 300. \
                                            Past it the command is killed and the call is an error."
                        }
                    },
                    "required": ["command"]
                }),
            },
            approval: ApprovalTier::Exec,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(command) = arg_str(&args, "command") else {
            return err("missing command");
        };
        if arg_bool(&args, "pty").unwrap_or(false) {
            return self.run_on_pty(&command, &args);
        }
        match Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&self.root)
            .output()
        {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                if output.status.success() {
                    ok(text)
                } else {
                    err(text)
                }
            }
            Err(error) => err(error.to_string()),
        }
    }
}

impl BashTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            interrupt: Interrupt::new(),
        }
    }

    /// The pty path: bounded by a deadline, by [`pty::OUTPUT_CAP`], and by
    /// [`Interrupt`]. Every one of those ends the call with `is_error`, output
    /// included, so the model sees how far the command got.
    fn run_on_pty(&self, command: &str, args: &Value) -> ToolResult {
        let options = PtyOptions {
            timeout: args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .map_or_else(
                    || pty::clamp_timeout(pty::DEFAULT_TIMEOUT_SECS),
                    pty::clamp_timeout,
                ),
            ..PtyOptions::default()
        };
        match pty::run(command, &self.root, &options, &self.interrupt) {
            Ok(run) if run.success => ok(run.output.to_string()),
            Ok(run) => err(format!("exit {}\n{}", run.exit_code, run.output)),
            Err(error) => err(error.to_string()),
        }
    }
}

fn walk(root: &Path, dir: &Path, pattern: &str, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` does not follow links: a link to `~` would otherwise
        // walk the home directory, past the workspace jail.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str())
                && (name == "target" || name == ".git" || name == "node_modules")
            {
                continue;
            }
            walk(root, &path, pattern, out);
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let rendered = relative.display().to_string();
        if pattern.is_empty() || rendered.contains(pattern) {
            out.push(rendered);
        }
    }
}

fn grep_walk(
    root: &Path,
    dir: &Path,
    pattern: &str,
    policy: &SensitivePolicy,
    out: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str())
                && (name == "target" || name == ".git" || name == "node_modules")
            {
                continue;
            }
            grep_walk(root, &path, pattern, policy, out);
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if policy.blocks(relative) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                out.push(format!("{}:{}:{line}", relative.display(), index + 1));
            }
        }
    }
}

pub fn workspace_tools(root: impl Into<PathBuf>) -> Vec<Box<dyn ToolHandler>> {
    workspace_tools_with_cache(root, ReadCache::default())
}

/// The workspace tools sharing one read cache, so parallel agents do not read
/// the same file twice.
pub fn workspace_tools_with_cache(
    root: impl Into<PathBuf>,
    cache: ReadCache,
) -> Vec<Box<dyn ToolHandler>> {
    workspace_tools_with_policy(root, cache, SensitivePolicy::default())
}

/// The workspace tools with the user's credential-file policy.
pub fn workspace_tools_with_policy(
    root: impl Into<PathBuf>,
    cache: ReadCache,
    policy: SensitivePolicy,
) -> Vec<Box<dyn ToolHandler>> {
    workspace_tools_with_interrupt(root, cache, policy, Interrupt::new())
}

/// The workspace tools sharing the caller's [`Interrupt`], so whoever owns
/// the cancel key can stop a `bash` command running on a pty. `invoke` takes
/// no cancellation argument, so this handle is the only seam for it.
pub fn workspace_tools_with_interrupt(
    root: impl Into<PathBuf>,
    cache: ReadCache,
    policy: SensitivePolicy,
    interrupt: Interrupt,
) -> Vec<Box<dyn ToolHandler>> {
    let root = root.into();
    vec![
        Box::new(ReadFileTool {
            root: root.clone(),
            cache: cache.clone(),
            policy: policy.clone(),
        }),
        Box::new(WriteFileTool {
            root: root.clone(),
            cache: cache.clone(),
        }),
        Box::new(EditFileTool {
            root: root.clone(),
            cache: cache.clone(),
            policy: policy.clone(),
        }),
        Box::new(HashlineEditTool {
            root: root.clone(),
            cache,
            policy: policy.clone(),
        }),
        Box::new(GlobTool { root: root.clone() }),
        Box::new(GrepTool {
            root: root.clone(),
            policy,
        }),
        Box::new(BashTool { root, interrupt }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A private fixture directory. Unique per call: tests run in parallel and
    /// a shared pid-named directory let one test delete another's files.
    fn temp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("titi-tools-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        root
    }

    #[tokio::test]
    async fn read_and_edit_refuse_credential_files() {
        let root = temp_root();
        fs::write(root.join(".env"), "API_KEY=sk-test-0000000000000000\n").unwrap();
        fs::create_dir_all(root.join(".ssh")).unwrap();
        fs::write(root.join(".ssh/id_ed25519"), "PRIVATE\n").unwrap();
        let cache = ReadCache::default();
        let read = ReadFileTool {
            root: root.clone(),
            cache: cache.clone(),
            policy: SensitivePolicy::default(),
        };
        let edit = EditFileTool {
            root: root.clone(),
            cache,
            policy: SensitivePolicy::default(),
        };

        for path in [".env", ".ssh/id_ed25519"] {
            let result = read.invoke(serde_json::json!({ "path": path })).await;
            assert!(result.is_error, "{path}: {}", result.output);
            assert!(!result.output.contains("sk-test"), "{}", result.output);
            assert!(!result.output.contains("PRIVATE"), "{}", result.output);
        }
        let result = edit
            .invoke(serde_json::json!({
                "path": ".env", "old_string": "API_KEY", "new_string": "X"
            }))
            .await;
        assert!(result.is_error);
        assert!(!result.output.contains("sk-test"), "{}", result.output);
        assert!(
            fs::read_to_string(root.join(".env"))
                .unwrap()
                .contains("API_KEY")
        );
    }

    #[tokio::test]
    async fn a_link_to_a_credential_file_is_refused_too() {
        let root = temp_root();
        fs::write(root.join(".env"), "API_KEY=sk-test-0000000000000000\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join(".env"), root.join("notes.txt")).unwrap();
        let read = ReadFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        let result = read
            .invoke(serde_json::json!({ "path": "notes.txt" }))
            .await;
        assert!(result.is_error, "{}", result.output);
        assert!(!result.output.contains("sk-test"), "{}", result.output);
    }

    #[tokio::test]
    async fn the_user_policy_reaches_read_and_grep() {
        let root = temp_root();
        fs::write(root.join(".env.test"), "FIXTURE=1\n").unwrap();
        fs::write(root.join("app.sops.yml"), "NEEDLE: enc\n").unwrap();
        fs::write(root.join("a.txt"), "NEEDLE plain\n").unwrap();
        let policy = SensitivePolicy::new(vec!["*.sops.yml".into()], vec![".env.test".into()]);
        let read = ReadFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
            policy: policy.clone(),
        };
        let allowed = read
            .invoke(serde_json::json!({ "path": ".env.test" }))
            .await;
        assert!(!allowed.is_error, "{}", allowed.output);
        let blocked = read
            .invoke(serde_json::json!({ "path": "app.sops.yml" }))
            .await;
        assert!(blocked.is_error, "{}", blocked.output);

        let grep = GrepTool {
            root: root.clone(),
            policy,
        };
        let result = grep
            .invoke(serde_json::json!({ "pattern": "NEEDLE" }))
            .await;
        assert!(result.output.contains("a.txt"), "{}", result.output);
        assert!(!result.output.contains("sops"), "{}", result.output);
    }

    #[tokio::test]
    async fn a_template_env_file_still_reads() {
        let root = temp_root();
        fs::write(root.join(".env.example"), "API_KEY=\n").unwrap();
        let read = ReadFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        let result = read
            .invoke(serde_json::json!({ "path": ".env.example" }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("API_KEY="));
    }

    #[tokio::test]
    async fn grep_skips_credential_files() {
        let root = temp_root();
        fs::write(root.join(".env"), "NEEDLE=sk-test-0000000000000000\n").unwrap();
        fs::write(root.join("a.txt"), "NEEDLE here\n").unwrap();
        let grep = GrepTool {
            root: root.clone(),
            policy: SensitivePolicy::default(),
        };
        let result = grep
            .invoke(serde_json::json!({ "pattern": "NEEDLE" }))
            .await;
        assert!(result.output.contains("a.txt:1:"), "{}", result.output);
        assert!(!result.output.contains(".env"), "{}", result.output);
        assert!(!result.output.contains("sk-test"), "{}", result.output);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_and_glob_do_not_follow_links_out_of_the_workspace() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private.txt"), "NEEDLE outside\n").unwrap();
        let root = temp_root();
        std::os::unix::fs::symlink(outside.path(), root.join("home")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("private.txt"), root.join("linked.txt"))
            .unwrap();

        let grep = GrepTool {
            root: root.clone(),
            policy: SensitivePolicy::default(),
        };
        let result = grep
            .invoke(serde_json::json!({ "pattern": "NEEDLE" }))
            .await;
        assert!(!result.output.contains("outside"), "{}", result.output);

        let glob = GlobTool { root: root.clone() };
        let result = glob
            .invoke(serde_json::json!({ "pattern": "private" }))
            .await;
        assert!(!result.output.contains("private.txt"), "{}", result.output);
    }

    #[tokio::test]
    async fn read_write_edit_roundtrip() {
        let root = temp_root();
        let cache = ReadCache::default();
        let read = ReadFileTool {
            root: root.clone(),
            cache: cache.clone(),
            policy: SensitivePolicy::default(),
        };
        let write = WriteFileTool {
            root: root.clone(),
            cache: cache.clone(),
        };
        let edit = EditFileTool {
            root: root.clone(),
            cache,
            policy: SensitivePolicy::default(),
        };
        let content = read.invoke(serde_json::json!({"path": "hello.txt"})).await;
        assert!(content.output.contains("hello"));
        let written = write
            .invoke(serde_json::json!({"path": "note.txt", "content": "alpha"}))
            .await;
        assert!(!written.is_error);
        let edited = edit
            .invoke(serde_json::json!({
                "path": "note.txt",
                "old_string": "alpha",
                "new_string": "beta"
            }))
            .await;
        assert!(!edited.is_error);
        assert_eq!(fs::read_to_string(root.join("note.txt")).unwrap(), "beta");
    }

    #[tokio::test]
    async fn glob_and_grep_find_workspace_files() {
        let root = temp_root();
        let glob = GlobTool { root: root.clone() };
        let grep = GrepTool {
            root,
            policy: SensitivePolicy::default(),
        };
        let listed = glob.invoke(serde_json::json!({"pattern": "hello"})).await;
        assert!(listed.output.contains("hello.txt"));
        let hits = grep.invoke(serde_json::json!({"pattern": "world"})).await;
        assert!(hits.output.contains("hello.txt:1:hello world"));
    }

    #[tokio::test]
    async fn jail_rejects_escape() {
        let root = temp_root();
        let read = ReadFileTool {
            root,
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        let result = read.invoke(serde_json::json!({"path": "../secret"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn a_write_invalidates_the_cached_body() {
        let root = temp_root();
        let cache = ReadCache::default();
        let read = ReadFileTool {
            root: root.clone(),
            cache: cache.clone(),
            policy: SensitivePolicy::default(),
        };
        let write = WriteFileTool {
            root: root.clone(),
            cache,
        };

        let first = read.invoke(serde_json::json!({"path": "hello.txt"})).await;
        assert!(first.output.contains("hello world"));
        write
            .invoke(serde_json::json!({"path": "hello.txt", "content": "replaced"}))
            .await;
        let second = read.invoke(serde_json::json!({"path": "hello.txt"})).await;
        assert_eq!(second.output, "replaced", "the write invalidated the cache");
    }

    #[tokio::test]
    async fn two_reads_share_one_cache() {
        let root = temp_root();
        let cache = ReadCache::default();
        let read = ReadFileTool {
            root: root.clone(),
            cache: cache.clone(),
            policy: SensitivePolicy::default(),
        };
        read.invoke(serde_json::json!({"path": "hello.txt"})).await;
        assert_eq!(cache.stats(), (0, 1));

        // A second agent holding the same cache hits memory, not disk.
        let other = ReadFileTool {
            root: root.clone(),
            cache,
            policy: SensitivePolicy::default(),
        };
        let again = other.invoke(serde_json::json!({"path": "hello.txt"})).await;
        assert!(again.output.contains("hello world"));
        assert_eq!(cache_hits(&other), 1);
    }

    fn cache_hits(tool: &ReadFileTool) -> u64 {
        tool.cache.stats().0
    }

    #[test]
    fn workspace_tools_register() {
        let mut registry = crate::ToolRegistry::new();
        for tool in workspace_tools(".") {
            registry.register(Arc::from(tool));
        }
        assert!(registry.get("read").is_some());
        assert!(registry.get("bash").is_some());
    }

    #[tokio::test]
    async fn bash_runs_on_a_pty_when_asked() {
        let root = temp_root();
        let tool = BashTool::new(&root);
        let result = tool
            .invoke(serde_json::json!({
                "command": "test -t 1 && echo on-a-tty", "pty": true
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output.trim_end(), "on-a-tty");

        // Without `pty` the same probe runs on a pipe, as it always has.
        let piped = tool
            .invoke(serde_json::json!({"command": "test -t 1 && echo on-a-tty"}))
            .await;
        assert!(piped.is_error, "{}", piped.output);
    }

    #[tokio::test]
    async fn a_pty_command_past_its_deadline_is_an_error_that_keeps_the_output() {
        let root = temp_root();
        let tool = BashTool::new(&root);
        let result = tool
            .invoke(serde_json::json!({
                "command": "echo working; sleep 5", "pty": true, "timeout_secs": 1
            }))
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("timed out"), "{}", result.output);
        assert!(result.output.contains("working"), "{}", result.output);
    }

    #[tokio::test]
    async fn the_shared_interrupt_stops_a_running_pty_command() {
        let root = temp_root();
        let interrupt = Interrupt::new();
        let tools = workspace_tools_with_interrupt(
            &root,
            ReadCache::default(),
            SensitivePolicy::default(),
            interrupt.clone(),
        );
        let bash = tools
            .into_iter()
            .find(|tool| tool.definition().spec.name == "bash")
            .unwrap_or_else(|| panic!("no bash tool in the workspace set"));

        let armed = interrupt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            armed.raise();
        });
        let started = std::time::Instant::now();
        let result = bash
            .invoke(serde_json::json!({
                "command": "sleep 30", "pty": true, "timeout_secs": 30
            }))
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("interrupted"), "{}", result.output);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
