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
        detail: None,
    }
}

pub(crate) fn err(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into().into(),
        is_error: true,
        detail: None,
    }
}

/// Cells a tool's description of a call may take: one row on a screen, beside a
/// glyph and an elapsed time, not a payload.
pub(crate) const DESCRIBE_MAX: usize = 60;

/// One line describing a call, cut to `max` characters: control characters
/// flattened (a command may hold a newline) and the tail dropped with an
/// ellipsis rather than the whole description being refused.
///
/// The row that shows this measures cells; this is only the bound that keeps a
/// description from becoming a payload.
pub(crate) fn describe_line(text: &str, max: usize) -> String {
    let flattened: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let mut out: String = flattened.chars().take(max).collect();
    if flattened.chars().count() > max {
        out.push('…');
    }
    out
}

/// A result with something for the surface to draw under the answer — a diff,
/// today. The answer itself is unchanged, and the model never sees the detail:
/// the engine keeps it out of the tool message.
pub(crate) fn ok_with_detail(output: impl Into<String>, detail: String) -> ToolResult {
    ToolResult {
        output: output.into().into(),
        is_error: false,
        detail: Some(detail.into()),
    }
}

/// Context lines a diff keeps on each side of a change: enough to place the
/// change in its file without dragging the file along.
const DIFF_CONTEXT: usize = 3;

/// A unified diff of `before` → `after`, or `None` when no line changed.
///
/// One hunk: the unchanged head and tail of the two texts are dropped and
/// [`DIFF_CONTEXT`] lines are kept around the changed middle. The change cannot
/// outgrow the strings the model sent in the very call being reported, so the
/// diff is complete rather than sampled — and it is the shape
/// `titi_tui::diff::render_diff` reads, so a screen can draw it.
pub(crate) fn unified_diff(path: &str, before: &str, after: &str) -> Option<String> {
    if before == after {
        return None;
    }
    // Each line keeps its terminator: the last line of a file with no final
    // newline is then a different line from the same text with one, which is
    // what the `\ No newline at end of file` marker is for.
    let old = text_lines(before);
    let new = text_lines(after);
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let first = head.saturating_sub(DIFF_CONTEXT);
    let old_end = (old.len() - tail + DIFF_CONTEXT).min(old.len());
    let new_end = (new.len() - tail + DIFF_CONTEXT).min(new.len());
    let mut diff = String::new();
    diff.push_str(&side("---", 'a', path, !old.is_empty()));
    diff.push_str(&side("+++", 'b', path, !new.is_empty()));
    diff.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk_start(first, old_end - first),
        old_end - first,
        hunk_start(first, new_end - first),
        new_end - first,
    ));
    for line in &old[first..head] {
        push_body_line(&mut diff, ' ', line);
    }
    for line in &old[head..old.len() - tail] {
        push_body_line(&mut diff, '-', line);
    }
    for line in &new[head..new.len() - tail] {
        push_body_line(&mut diff, '+', line);
    }
    for line in &new[new.len() - tail..new_end] {
        push_body_line(&mut diff, ' ', line);
    }
    Some(diff)
}

/// The lines of `text`, each with its terminator, and no line at all for an
/// empty text — an empty file has no first line to remove or add.
fn text_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split_inclusive('\n').collect()
    }
}

/// A `---`/`+++` header: `/dev/null` for a side the change creates or deletes,
/// which a diff reader takes as unnamed.
fn side(marker: &str, letter: char, path: &str, present: bool) -> String {
    if present {
        format!("{marker} {letter}/{path}\n")
    } else {
        format!("{marker} /dev/null\n")
    }
}

/// The first line a hunk covers: one-based, except that a side with no lines
/// starts at its own zero (git writes `-0,0` for a file that did not exist).
fn hunk_start(first: usize, count: usize) -> usize {
    first + usize::from(count > 0)
}

/// One body row, plus the marker that says the line carries no newline of its
/// own — the row a reader needs to tell `x\n` from `x`.
fn push_body_line(diff: &mut String, marker: char, line: &str) {
    diff.push(marker);
    match line.strip_suffix('\n') {
        Some(text) => {
            diff.push_str(text);
            diff.push('\n');
        }
        None => {
            diff.push_str(line);
            diff.push_str("\n\\ No newline at end of file\n");
        }
    }
}

/// The text a write is about to replace: empty for a path that is not there,
/// and `None` for one that exists but does not read as text — the diff would
/// have to be invented, so none is reported.
fn replacing_text(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
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

/// A positive line number or count, from an integer or a string of digits;
/// absent or `null` is `None`.
fn line_arg(args: &Value, key: &str) -> Result<Option<usize>, String> {
    let value = match args.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let number = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .and_then(|number| usize::try_from(number).ok());
    match number {
        Some(number) if number >= 1 => Ok(Some(number)),
        _ => Err(format!(
            "{key} must be a whole number of at least 1, not {value}"
        )),
    }
}

/// Lines `offset..offset + limit` (1-based) of `content`, as the file holds
/// them. A window that leaves any line out starts with `[lines A-B of N]`.
fn line_window(
    path: &str,
    content: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<String, String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    // Line 1 of an empty file is where it starts, not past its end.
    if offset > total.max(1) {
        return Err(format!(
            "{path}:{offset} is past the end of the file ({total} lines)"
        ));
    }
    let first = offset - 1;
    let end = limit.map_or(total, |limit| first.saturating_add(limit).min(total));
    let body = lines[first..end].concat();
    if first == 0 && end == total {
        return Ok(body);
    }
    Ok(format!("[lines {offset}-{end} of {total}]\n{body}"))
}

#[async_trait]
impl ToolHandler for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "read".into(),
                description: "Read a UTF-8 file from the workspace: the whole file, or a line \
                              range with offset and limit. A range that leaves lines out starts \
                              with a `[lines A-B of N]` line; long results are cut, so read a \
                              large file in ranges."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "First line to read, 1-based. Default 1."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "How many lines to read from offset. \
                                            Default: to the end of the file."
                        }
                    },
                    "required": ["path"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    fn describe(&self, args: &Value) -> Option<String> {
        let path = arg_str(args, "path")?;
        let offset = line_arg(args, "offset").ok().flatten();
        let limit = line_arg(args, "limit").ok().flatten();
        let window = match (offset, limit) {
            (None, None) => String::new(),
            (offset, Some(limit)) => {
                let first = offset.unwrap_or(1);
                format!(":{first}-{}", first.saturating_add(limit - 1))
            }
            (Some(offset), None) => format!(":{offset}-"),
        };
        Some(format!(
            "read {}{window}",
            describe_line(&path, DESCRIBE_MAX)
        ))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return err("missing path");
        };
        let (offset, limit) = match (line_arg(&args, "offset"), line_arg(&args, "limit")) {
            (Ok(offset), Ok(limit)) => (offset, limit),
            (Err(error), _) | (_, Err(error)) => return err(error),
        };
        let content = match readable_path(&self.root, &path, &self.policy)
            .and_then(|path| self.cache.read(&path))
        {
            Ok(content) => content,
            Err(error) => return err(error),
        };
        if offset.is_none() && limit.is_none() {
            return ok(content);
        }
        match line_window(&path, &content, offset.unwrap_or(1), limit) {
            Ok(window) => ok(window),
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

    fn describe(&self, args: &Value) -> Option<String> {
        let path = arg_str(args, "path")?;
        Some(format!("write {}", describe_line(&path, DESCRIBE_MAX)))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(path) = arg_str(&args, "path") else {
            return err("missing path");
        };
        let Some(content) = arg_str(&args, "content") else {
            return err("missing content");
        };
        match jail_path(&self.root, &path).and_then(|resolved| {
            // What the write replaces is read here, for the diff: this is the
            // only moment that text exists.
            let before = replacing_text(&resolved);
            if let Some(parent) = resolved.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(&resolved, &content).map_err(|error| error.to_string())?;
            self.cache.invalidate(&resolved);
            Ok((resolved.display().to_string(), before))
        }) {
            Ok((written, before)) => {
                // The answer is the one line it always was; the diff is the
                // surface's to draw, and the model never reads it.
                match before.and_then(|before| unified_diff(&path, &before, &content)) {
                    Some(diff) => ok_with_detail(format!("wrote {written}"), diff),
                    None => ok(format!("wrote {written}")),
                }
            }
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
                description: "Replace old_string with new_string in a file. old_string must \
                              match exactly and occur once — include surrounding lines to make it \
                              unique — or pass replace_all to change every occurrence."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace every occurrence instead of exactly one."
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            approval: ApprovalTier::Write,
        }
    }

    fn describe(&self, args: &Value) -> Option<String> {
        let path = arg_str(args, "path")?;
        Some(format!("edit {}", describe_line(&path, DESCRIBE_MAX)))
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
        let replace_all = arg_bool(&args, "replace_all").unwrap_or(false);
        match readable_path(&self.root, &path, &self.policy).and_then(|resolved| {
            let content = fs::read_to_string(&resolved).map_err(|error| error.to_string())?;
            let (updated, replaced) = apply_edit(&content, &old, &new, replace_all)?;
            fs::write(&resolved, &updated).map_err(|error| error.to_string())?;
            Ok((unified_diff(&path, &content, &updated), replaced))
        }) {
            Ok((diff, replaced)) => {
                let answer = if replaced > 1 {
                    format!("edited ({replaced} replacements)")
                } else {
                    "edited".to_owned()
                };
                match diff {
                    Some(diff) => ok_with_detail(answer, diff),
                    None => ok(answer),
                }
            }
            Err(error) => err(error),
        }
    }
}

/// Applies one `edit` call to a file's text and says how many places changed.
///
/// A model writes `\n`; a file that uses `\r\n` throughout is matched in LF
/// and written back in CRLF, and a byte-order mark is kept. A file that mixes
/// the two is matched as it is, so lines the edit did not touch keep their
/// endings. `old_string` must be non-empty, differ from `new_string`, and —
/// unless `replace_all` — occur exactly once: replacing the first of several
/// was a guess that could land in the wrong function.
fn apply_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("old_string is empty; use write to create or replace a whole file".into());
    }
    if old == new {
        return Err("old_string and new_string are identical; nothing would change".into());
    }
    let (bom, body) = match content.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", content),
    };
    let crlf = body.contains("\r\n") && !body.replace("\r\n", "").contains('\n');
    let (text, old, new) = if crlf {
        (
            body.replace("\r\n", "\n"),
            old.replace("\r\n", "\n"),
            new.replace("\r\n", "\n"),
        )
    } else {
        (body.to_owned(), old.to_owned(), new.to_owned())
    };
    let count = text.matches(old.as_str()).count();
    if count == 0 {
        return Err(
            "old_string not found; read the file again and copy the text exactly, \
                    whitespace included"
                .into(),
        );
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string occurs {count} times; include surrounding lines to make it unique, \
             or pass replace_all: true"
        ));
    }
    let replaced = if replace_all {
        text.replace(old.as_str(), &new)
    } else {
        text.replacen(old.as_str(), &new, 1)
    };
    let replaced = if crlf {
        replaced.replace('\n', "\r\n")
    } else {
        replaced
    };
    Ok((
        format!("{bom}{replaced}"),
        if replace_all { count } else { 1 },
    ))
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

    fn describe(&self, args: &Value) -> Option<String> {
        let pattern = arg_str(args, "pattern")?;
        Some(format!("glob {}", describe_line(&pattern, DESCRIBE_MAX)))
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

    fn describe(&self, args: &Value) -> Option<String> {
        let pattern = arg_str(args, "pattern")?;
        let mut line = format!("grep {}", describe_line(&pattern, DESCRIBE_MAX));
        if let Some(path) = arg_str(args, "path") {
            line.push(' ');
            line.push_str(&describe_line(&path, DESCRIBE_MAX));
        }
        Some(line)
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

    fn describe(&self, args: &Value) -> Option<String> {
        let command = arg_str(args, "command")?;
        Some(format!("bash {}", describe_line(&command, DESCRIBE_MAX)))
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
    let mut tools: Vec<Box<dyn ToolHandler>> = vec![
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
            policy: policy.clone(),
        }),
        Box::new(BashTool {
            root: root.clone(),
            interrupt,
        }),
    ];
    tools.extend(crate::git::git_tools(root, policy));
    // The search provider is read here, once per registry: `web_search`
    // answers "no provider configured" for the whole session rather than
    // re-reading the environment on every call.
    tools.extend(crate::web::web_tools(crate::web::SearchProvider::from_env()));
    tools
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

    /// The lines of a result's presentation detail, so a test asserts on the
    /// diff without unwrapping it at every step.
    fn detail_lines(result: &ToolResult) -> Vec<&str> {
        result
            .detail
            .as_deref()
            .expect("the tool reported a detail")
            .lines()
            .collect()
    }

    /// A tool describes the call it is about to make, so a surface can say
    /// what is being read or run rather than only which tool it is.
    #[tokio::test]
    async fn a_tool_describes_the_call_it_is_about_to_make() {
        let root = temp_root();
        let read = ReadFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        assert_eq!(
            read.describe(&serde_json::json!({"path": "docs/README.md"}))
                .as_deref(),
            Some("read docs/README.md")
        );
        assert_eq!(
            read.describe(&serde_json::json!({})),
            None,
            "no path, no call"
        );

        let write = WriteFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
        };
        assert_eq!(
            write
                .describe(&serde_json::json!({"path": "notes/probe.txt", "content": "x"}))
                .as_deref(),
            Some("write notes/probe.txt")
        );

        let bash = BashTool {
            root: root.clone(),
            interrupt: Interrupt::new(),
        };
        assert_eq!(
            bash.describe(&serde_json::json!({"command": "cargo test -p titi-core"}))
                .as_deref(),
            Some("bash cargo test -p titi-core")
        );

        let grep = GrepTool {
            root,
            policy: SensitivePolicy::default(),
        };
        assert_eq!(
            grep.describe(&serde_json::json!({"pattern": "ToolResult", "path": "crates"}))
                .as_deref(),
            Some("grep ToolResult crates")
        );

        // A description stays one row: a long one is cut, a newline flattened.
        let long = "x".repeat(DESCRIBE_MAX + 10);
        let described = bash
            .describe(&serde_json::json!({"command": long}))
            .expect("a command is a call");
        assert!(described.ends_with('…'), "{described}");
        assert!(
            described.chars().count() < long.chars().count() + 8,
            "{described}"
        );
        let newline = bash
            .describe(&serde_json::json!({"command": "echo one\necho two"}))
            .expect("a command is a call");
        assert_eq!(newline, "bash echo one echo two", "a newline is flattened");
    }

    fn edit_tool(root: &Path) -> EditFileTool {
        EditFileTool {
            root: root.to_path_buf(),
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        }
    }

    /// An `old_string` that occurs more than once names no place: replacing
    /// the first was a guess that could land in the wrong function. It is
    /// refused with the count, unless the call asks for every occurrence.
    #[tokio::test]
    async fn an_ambiguous_edit_is_refused_unless_it_replaces_all() {
        let root = temp_root();
        fs::write(root.join("dup.rs"), "let x = 1;\nlet y = 2;\nlet x = 1;\n").unwrap();
        let edit = edit_tool(&root);
        let result = edit
            .invoke(serde_json::json!({
                "path": "dup.rs", "old_string": "let x = 1;", "new_string": "let x = 9;"
            }))
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("2 times"), "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("dup.rs")).unwrap(),
            "let x = 1;\nlet y = 2;\nlet x = 1;\n",
            "a refused edit changes nothing"
        );

        let result = edit
            .invoke(serde_json::json!({
                "path": "dup.rs", "old_string": "let x = 1;", "new_string": "let x = 9;",
                "replace_all": true
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "edited (2 replacements)");
        assert_eq!(
            fs::read_to_string(root.join("dup.rs")).unwrap(),
            "let x = 9;\nlet y = 2;\nlet x = 9;\n"
        );
    }

    /// A CRLF file is matched by the LF text a model writes, and keeps its
    /// own line endings — and its byte-order mark — after the edit.
    #[tokio::test]
    async fn a_crlf_file_is_edited_from_lf_text_and_stays_crlf() {
        let root = temp_root();
        fs::write(root.join("win.txt"), "\u{feff}one\r\ntwo\r\nthree\r\n").unwrap();
        let result = edit_tool(&root)
            .invoke(serde_json::json!({
                "path": "win.txt", "old_string": "one\ntwo", "new_string": "one\n2\nand a half"
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("win.txt")).unwrap(),
            "\u{feff}one\r\n2\r\nand a half\r\nthree\r\n"
        );
    }

    /// An empty `old_string` is found everywhere and once meant "prepend",
    /// and an edit that changes nothing is a model that lost track: both are
    /// refused without touching the file.
    #[tokio::test]
    async fn an_empty_or_unchanged_edit_is_refused() {
        let root = temp_root();
        let edit = edit_tool(&root);
        for (old, new) in [("", "prefix "), ("hello", "hello")] {
            let result = edit
                .invoke(serde_json::json!({
                    "path": "hello.txt", "old_string": old, "new_string": new
                }))
                .await;
            assert!(result.is_error, "{old:?} -> {new:?} was accepted");
        }
        assert_eq!(
            fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world"
        );
    }

    /// A miss says what to do next instead of only that it missed.
    #[tokio::test]
    async fn a_missed_edit_says_how_to_recover() {
        let root = temp_root();
        let result = edit_tool(&root)
            .invoke(serde_json::json!({
                "path": "hello.txt", "old_string": "goodbye", "new_string": "x"
            }))
            .await;
        assert!(result.is_error);
        assert!(
            result.output.starts_with("old_string not found"),
            "{}",
            result.output
        );
        assert!(result.output.contains("read"), "{}", result.output);
    }

    /// An edit's answer stays the one line it always was, and the change it
    /// made travels beside it as a detail — a diff of exactly the lines it
    /// touched, which the model never reads.
    #[tokio::test]
    async fn edit_reports_the_diff_of_the_lines_it_changed() {
        let root = temp_root();
        fs::write(root.join("note.txt"), "alpha\nkeep\nbeta\n").unwrap();
        let edit = EditFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        let result = edit
            .invoke(serde_json::json!({
                "path": "note.txt",
                "old_string": "beta",
                "new_string": "gamma"
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).unwrap(),
            "alpha\nkeep\ngamma\n"
        );
        assert_eq!(
            result.output, "edited",
            "the answer is the one line the model has always read"
        );
        assert_eq!(
            detail_lines(&result),
            [
                "--- a/note.txt",
                "+++ b/note.txt",
                "@@ -1,3 +1,3 @@",
                " alpha",
                " keep",
                "-beta",
                "+gamma",
            ]
        );
    }

    /// A write's answer stays one line, and what it replaced travels beside it
    /// as a detail: a file it created reads as an addition against
    /// `/dev/null`.
    #[tokio::test]
    async fn write_reports_the_diff_against_what_it_replaced() {
        let root = temp_root();
        let write = WriteFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
        };
        let created = write
            .invoke(serde_json::json!({"path": "fresh.txt", "content": "one\ntwo\n"}))
            .await;
        assert!(!created.is_error, "{}", created.output);
        assert!(
            created.output.ends_with("fresh.txt"),
            "the answer names the file and nothing else: {:?}",
            created.output
        );
        assert_eq!(
            detail_lines(&created),
            [
                "--- /dev/null",
                "+++ b/fresh.txt",
                "@@ -0,0 +1,2 @@",
                "+one",
                "+two",
            ]
        );

        let rewritten = write
            .invoke(serde_json::json!({"path": "fresh.txt", "content": "one\nthree\n"}))
            .await;
        assert!(!rewritten.is_error, "{}", rewritten.output);
        assert_eq!(
            rewritten.output.lines().count(),
            1,
            "the answer stays one line: {}",
            rewritten.output
        );
        assert!(
            rewritten
                .detail
                .as_deref()
                .is_some_and(|diff| diff.contains("-two\n+three")),
            "{:?}",
            rewritten.detail
        );
    }

    /// A write that changes nothing reports only its summary, and so does a
    /// file the diff cannot be drawn from.
    #[tokio::test]
    async fn a_write_with_no_change_reports_no_diff() {
        let root = temp_root();
        let write = WriteFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
        };
        let written = write
            .invoke(serde_json::json!({"path": "same.txt", "content": "alpha\n"}))
            .await;
        assert!(!written.is_error, "{}", written.output);
        let again = write
            .invoke(serde_json::json!({"path": "same.txt", "content": "alpha\n"}))
            .await;
        assert_eq!(
            again.output.lines().count(),
            1,
            "an unchanged write must stay one line: {}",
            again.output
        );
    }

    /// The diff generator itself: one hunk, the context around the change, the
    /// exact hunk counts, and the marker that says a line has no newline.
    #[test]
    fn a_diff_is_one_hunk_with_context_and_exact_counts() {
        let same = unified_diff("x.txt", "a\nb\nc\n", "a\nb\nc\n");
        assert!(same.is_none(), "an unchanged text has no diff");
        assert!(unified_diff("x.txt", "", "").is_none());

        let changed = unified_diff("x.txt", "a\nb\nc\nd\ne\n", "a\nb\nX\nd\ne\n").unwrap();
        assert_eq!(
            changed,
            "--- a/x.txt\n+++ b/x.txt\n@@ -1,5 +1,5 @@\n a\n b\n-c\n+X\n d\n e\n"
        );

        let created = unified_diff("tails/x.txt", "", "one\n").unwrap();
        assert_eq!(
            created,
            "--- /dev/null\n+++ b/tails/x.txt\n@@ -0,0 +1,1 @@\n+one\n"
        );

        let deleted = unified_diff("gone.txt", "one\n", "").unwrap();
        assert_eq!(
            deleted,
            "--- a/gone.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-one\n"
        );

        // The last line of a file with no final newline is a line of its own,
        // and the marker says so.
        let unterminated = unified_diff("x.txt", "a", "a\n").unwrap();
        assert_eq!(
            unterminated,
            "--- a/x.txt\n+++ b/x.txt\n@@ -1,1 +1,1 @@\n-a\n\\ No newline at end of file\n+a\n"
        );

        // A change deep in a long file keeps three lines of context and no
        // more, and the hunk counts cover what it emits.
        let long: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let mut edited = long.clone();
        edited = edited.replace("line 20\n", "changed\n");
        let deep = unified_diff("long.txt", &long, &edited).unwrap();
        let lines: Vec<&str> = deep.lines().collect();
        assert_eq!(
            lines,
            [
                "--- a/long.txt",
                "+++ b/long.txt",
                "@@ -18,7 +18,7 @@",
                " line 17",
                " line 18",
                " line 19",
                "-line 20",
                "+changed",
                " line 21",
                " line 22",
                " line 23",
            ]
        );
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

    /// A ten-line file, `line 1` … `line 10`, and a reader over its root.
    fn ten_line_reader() -> (PathBuf, ReadFileTool) {
        let root = temp_root();
        let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        fs::write(root.join("ten.txt"), body).unwrap();
        let read = ReadFileTool {
            root: root.clone(),
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        (root, read)
    }

    /// A range is those lines and nothing else, under one header that says
    /// where they sit — the model asked for a window because the file is too
    /// long to take whole, so it must know how long.
    #[tokio::test]
    async fn a_range_read_returns_those_lines_under_a_header() {
        let (_root, read) = ten_line_reader();
        let result = read
            .invoke(serde_json::json!({"path": "ten.txt", "offset": 3, "limit": 2}))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "[lines 3-4 of 10]\nline 3\nline 4\n");

        let tail = read
            .invoke(serde_json::json!({"path": "ten.txt", "offset": 9}))
            .await;
        assert_eq!(tail.output, "[lines 9-10 of 10]\nline 9\nline 10\n");

        let head = read
            .invoke(serde_json::json!({"path": "ten.txt", "limit": 2}))
            .await;
        assert_eq!(head.output, "[lines 1-2 of 10]\nline 1\nline 2\n");

        // A limit that runs past the end stops at the last line.
        let past = read
            .invoke(serde_json::json!({"path": "ten.txt", "offset": 8, "limit": 100}))
            .await;
        assert_eq!(past.output, "[lines 8-10 of 10]\nline 8\nline 9\nline 10\n");
    }

    /// A range that covers the whole file is the whole file, with no header:
    /// nothing was left out, so there is nothing to locate.
    #[tokio::test]
    async fn a_range_over_the_whole_file_has_no_header() {
        let (root, read) = ten_line_reader();
        let whole = fs::read_to_string(root.join("ten.txt")).unwrap();
        let ranged = read
            .invoke(serde_json::json!({"path": "ten.txt", "offset": 1, "limit": 50}))
            .await;
        assert_eq!(ranged.output, whole);
        let plain = read.invoke(serde_json::json!({"path": "ten.txt"})).await;
        assert!(!plain.is_error, "{}", plain.output);
        assert_eq!(
            plain.output, whole,
            "no range reads the file as it always did"
        );
    }

    #[tokio::test]
    async fn an_offset_past_the_end_names_the_line_count() {
        let (_root, read) = ten_line_reader();
        let result = read
            .invoke(serde_json::json!({"path": "ten.txt", "offset": 11}))
            .await;
        assert!(result.is_error, "{}", result.output);
        assert!(result.output.contains("ten.txt"), "{}", result.output);
        assert!(result.output.contains("10 lines"), "{}", result.output);
    }

    #[tokio::test]
    async fn a_malformed_range_is_an_error() {
        let (_root, read) = ten_line_reader();
        for args in [
            serde_json::json!({"path": "ten.txt", "offset": 0}),
            serde_json::json!({"path": "ten.txt", "limit": 0}),
            serde_json::json!({"path": "ten.txt", "offset": -3}),
            serde_json::json!({"path": "ten.txt", "offset": "soon"}),
        ] {
            let result = read.invoke(args.clone()).await;
            assert!(result.is_error, "{args}: {}", result.output);
            assert!(!result.output.contains("line 1"), "{}", result.output);
        }
    }

    /// The last line of a file with no final newline is still a line, and a
    /// range reads it as the file holds it.
    #[tokio::test]
    async fn a_range_reads_an_unterminated_last_line() {
        let root = temp_root();
        fs::write(root.join("short.txt"), "a\nb\nc").unwrap();
        let read = ReadFileTool {
            root,
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        };
        let result = read
            .invoke(serde_json::json!({"path": "short.txt", "offset": 3}))
            .await;
        assert_eq!(result.output, "[lines 3-3 of 3]\nc");
    }

    /// A range is cut from the cached body, so reading a second window of the
    /// same file does not go back to disk; and a credential file is refused
    /// whatever window is asked for.
    #[tokio::test]
    async fn a_range_read_uses_the_cache_and_the_credential_policy() {
        let (root, read) = ten_line_reader();
        read.invoke(serde_json::json!({"path": "ten.txt", "offset": 1, "limit": 2}))
            .await;
        read.invoke(serde_json::json!({"path": "ten.txt", "offset": 5, "limit": 2}))
            .await;
        assert_eq!(read.cache.stats(), (1, 1));

        fs::write(root.join(".env"), "API_KEY=sk-test-0000000000000000\n").unwrap();
        let secret = read
            .invoke(serde_json::json!({"path": ".env", "offset": 1, "limit": 1}))
            .await;
        assert!(secret.is_error, "{}", secret.output);
        assert!(!secret.output.contains("sk-test"), "{}", secret.output);
    }

    #[test]
    fn a_range_read_describes_its_window() {
        let (_root, read) = ten_line_reader();
        let described = |args: Value| read.describe(&args);
        assert_eq!(
            described(serde_json::json!({"path": "a.rs", "offset": 120, "limit": 61})).as_deref(),
            Some("read a.rs:120-180")
        );
        assert_eq!(
            described(serde_json::json!({"path": "a.rs", "offset": 120})).as_deref(),
            Some("read a.rs:120-")
        );
        assert_eq!(
            described(serde_json::json!({"path": "a.rs", "limit": 40})).as_deref(),
            Some("read a.rs:1-40")
        );
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
