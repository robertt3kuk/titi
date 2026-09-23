//! Line-hash-anchored edits.
//!
//! The model names the lines it wants to replace by a short hash of the text
//! it read, not by number alone. If the file moved on since that read the
//! anchors no longer match and the edit is refused whole, so a stale read
//! cannot rewrite the wrong lines.
//!
//! Spec: `docs/research/tools-core/README.md` (P4-2).

use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;
use titi_providers::ToolSpec;

use crate::cache::ReadCache;
use crate::fs::{arg_str, err, ok, readable_path};
use crate::sensitive::SensitivePolicy;
use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// FNV-1a 64-bit: fixed offset basis and prime, so the anchor for a line is
/// the same in every process and every build. `DefaultHasher` is not — its
/// algorithm is explicitly allowed to change between releases.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The line as the reader saw it: the terminator is not part of the content.
fn line_body(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}

/// The 6-hex-digit anchor of one line of text.
pub fn line_anchor(line: &str) -> String {
    let mut hash = FNV_OFFSET;
    for byte in line_body(line).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:06x}", hash & 0x00ff_ffff)
}

/// Why a hashline edit was refused. Nothing is written unless the whole edit
/// passes, so every variant means the file is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashlineError {
    /// A required argument was absent or of the wrong shape.
    MissingArg { name: &'static str },
    /// The path left the workspace or holds credentials.
    Path { reason: String },
    /// The file could not be read or written.
    Io { path: String, reason: String },
    /// `anchors` was empty: an edit must name at least one line.
    NoAnchors,
    /// The anchored range runs past the end of the file.
    OutOfRange {
        path: String,
        line: usize,
        lines: usize,
    },
    /// Stale read: the line no longer hashes to the anchor the model held.
    AnchorMismatch {
        path: String,
        line: usize,
        expected: String,
        found: String,
    },
}

impl std::fmt::Display for HashlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingArg { name } => write!(f, "missing {name}"),
            Self::Path { reason } => write!(f, "{reason}"),
            Self::Io { path, reason } => write!(f, "{path}: {reason}"),
            Self::NoAnchors => write!(f, "anchors must name at least one line"),
            Self::OutOfRange { path, line, lines } => write!(
                f,
                "{path}:{line} is past the end of the file ({lines} lines)"
            ),
            Self::AnchorMismatch {
                path,
                line,
                expected,
                found,
            } => write!(
                f,
                "stale read: {path}:{line} hashes to {found}, not the anchor {expected}; \
                 re-read {path} before editing"
            ),
        }
    }
}

impl std::error::Error for HashlineError {}

pub struct HashlineEditTool {
    pub root: PathBuf,
    /// Invalidated on edit, so the next read sees the new body.
    pub cache: ReadCache,
    /// Which files hold credentials and are refused.
    pub policy: SensitivePolicy,
}

impl HashlineEditTool {
    fn apply(&self, args: &Value) -> Result<String, HashlineError> {
        let raw_path = arg_str(args, "path").ok_or(HashlineError::MissingArg { name: "path" })?;
        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .filter(|line| *line >= 1)
            .ok_or(HashlineError::MissingArg { name: "start_line" })? as usize;
        let anchors: Vec<String> = args
            .get("anchors")
            .and_then(Value::as_array)
            .ok_or(HashlineError::MissingArg { name: "anchors" })?
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        let replacement = arg_str(args, "replacement").ok_or(HashlineError::MissingArg {
            name: "replacement",
        })?;
        if anchors.is_empty() {
            return Err(HashlineError::NoAnchors);
        }

        let path = readable_path(&self.root, &raw_path, &self.policy)
            .map_err(|reason| HashlineError::Path { reason })?;
        let content = fs::read_to_string(&path).map_err(|error| HashlineError::Io {
            path: raw_path.clone(),
            reason: error.to_string(),
        })?;
        // Keeps each line's own terminator, so rewriting a CRLF file does not
        // silently convert the lines nobody touched.
        let segments: Vec<&str> = content.split_inclusive('\n').collect();

        let end = start + anchors.len() - 1;
        if end > segments.len() {
            return Err(HashlineError::OutOfRange {
                path: raw_path,
                line: end,
                lines: segments.len(),
            });
        }

        let updated = render(&segments, start, &anchors, &replacement, &raw_path)?;
        fs::write(&path, updated).map_err(|error| HashlineError::Io {
            path: raw_path.clone(),
            reason: error.to_string(),
        })?;
        self.cache.invalidate(&path);
        Ok(format!("edited {raw_path}:{start}-{end}"))
    }
}

/// The file body after the anchored range is swapped for `replacement`.
/// Verification happens here, before a single byte is written.
fn render(
    segments: &[&str],
    start: usize,
    anchors: &[String],
    replacement: &str,
    path: &str,
) -> Result<String, HashlineError> {
    for (offset, expected) in anchors.iter().enumerate() {
        let line = start + offset;
        let found = line_anchor(segments[line - 1]);
        if !expected.trim().eq_ignore_ascii_case(&found) {
            return Err(HashlineError::AnchorMismatch {
                path: path.to_owned(),
                line,
                expected: expected.trim().to_owned(),
                found,
            });
        }
    }

    let last = segments[start + anchors.len() - 2];
    let newline = if last.ends_with("\r\n") { "\r\n" } else { "\n" };
    let terminated = last.ends_with('\n');

    let mut out = String::with_capacity(replacement.len() + 64);
    for segment in &segments[..start - 1] {
        out.push_str(segment);
    }
    // An empty replacement deletes the anchored lines outright.
    if !replacement.is_empty() {
        let mut first = true;
        for line in replacement.split('\n') {
            if !first {
                out.push_str(newline);
            }
            out.push_str(line_body(line));
            first = false;
        }
        if terminated {
            out.push_str(newline);
        }
    }
    for segment in &segments[start + anchors.len() - 1..] {
        out.push_str(segment);
    }
    Ok(out)
}

#[async_trait]
impl ToolHandler for HashlineEditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "hashline_edit".into(),
                description: "Replace lines pinned by the 6-hex anchor of the text you read; \
                              a changed line is refused as a stale read"
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer", "minimum": 1 },
                        "anchors": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1
                        },
                        "replacement": { "type": "string" }
                    },
                    "required": ["path", "start_line", "anchors", "replacement"]
                }),
            },
            approval: ApprovalTier::Write,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match self.apply(&args) {
            Ok(message) => ok(message),
            Err(error) => err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("titi-hashline-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::create_dir_all(&root);
        root
    }

    fn tool(root: PathBuf) -> HashlineEditTool {
        HashlineEditTool {
            root,
            cache: ReadCache::default(),
            policy: SensitivePolicy::default(),
        }
    }

    #[test]
    fn the_workspace_registers_it_at_write_tier() {
        let tools = crate::fs::workspace_tools(".");
        let definition = tools
            .iter()
            .map(|tool| tool.definition())
            .find(|definition| definition.spec.name == "hashline_edit")
            .expect("hashline_edit is a workspace tool");
        assert_eq!(definition.approval, ApprovalTier::Write);
    }

    #[tokio::test]
    async fn matching_anchor_applies_the_edit() {
        let root = temp_root();
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "a.txt",
                "start_line": 2,
                "anchors": [line_anchor("two")],
                "replacement": "TWO",
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[tokio::test]
    async fn stale_anchor_is_refused_and_the_file_is_untouched() {
        let root = temp_root();
        let file = root.join("b.txt");
        fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let stale = line_anchor("two");
        // Someone else edited line 2 after the model read it.
        fs::write(&file, "one\ntwo (edited by a peer)\nthree\n").unwrap();

        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "b.txt",
                "start_line": 2,
                "anchors": [stale],
                "replacement": "TWO",
            }))
            .await;

        assert!(result.is_error, "stale anchor must be refused");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "one\ntwo (edited by a peer)\nthree\n",
            "a refused edit must not change the file"
        );
    }

    #[tokio::test]
    async fn the_refusal_names_the_file_and_the_line() {
        let root = temp_root();
        fs::write(root.join("c.txt"), "alpha\nbeta\n").unwrap();
        let result = tool(root)
            .invoke(serde_json::json!({
                "path": "c.txt",
                "start_line": 2,
                "anchors": [line_anchor("something else")],
                "replacement": "BETA",
            }))
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("c.txt:2"),
            "refusal should name file and line, got: {}",
            result.output
        );
        assert!(
            result.output.contains("stale read"),
            "refusal should name the cause, got: {}",
            result.output
        );
    }

    #[test]
    fn the_anchor_is_stable_for_the_same_content() {
        assert_eq!(line_anchor("fn main() {}"), line_anchor("fn main() {}"));
        assert_ne!(line_anchor("fn main() {}"), line_anchor("fn main() { }"));
        assert_eq!(line_anchor("value"), line_anchor("value\n"));
        assert_eq!(line_anchor("value"), line_anchor("value\r\n"));
        assert_eq!(line_anchor("value").len(), 6);
        assert!(line_anchor("value").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn a_multi_line_range_is_replaced_as_a_whole() {
        let root = temp_root();
        fs::write(root.join("d.txt"), "a\nb\nc\nd\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "d.txt",
                "start_line": 2,
                "anchors": [line_anchor("b"), line_anchor("c")],
                "replacement": "B\nB2\nB3",
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("d.txt")).unwrap(),
            "a\nB\nB2\nB3\nd\n"
        );
    }

    #[tokio::test]
    async fn one_stale_anchor_refuses_the_whole_range() {
        let root = temp_root();
        fs::write(root.join("e.txt"), "a\nb\nc\nd\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "e.txt",
                "start_line": 2,
                "anchors": [line_anchor("b"), line_anchor("not c")],
                "replacement": "X",
            }))
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("e.txt:3"), "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("e.txt")).unwrap(),
            "a\nb\nc\nd\n"
        );
    }

    #[tokio::test]
    async fn an_empty_replacement_deletes_the_anchored_lines() {
        let root = temp_root();
        fs::write(root.join("f.txt"), "a\nb\nc\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "f.txt",
                "start_line": 2,
                "anchors": [line_anchor("b")],
                "replacement": "",
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "a\nc\n");
    }

    #[tokio::test]
    async fn a_range_past_the_end_is_refused() {
        let root = temp_root();
        fs::write(root.join("g.txt"), "a\nb\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "g.txt",
                "start_line": 2,
                "anchors": [line_anchor("b"), "000000"],
                "replacement": "X",
            }))
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("g.txt:3"), "{}", result.output);
        assert_eq!(fs::read_to_string(root.join("g.txt")).unwrap(), "a\nb\n");
    }

    #[tokio::test]
    async fn crlf_lines_keep_their_terminators() {
        let root = temp_root();
        fs::write(root.join("h.txt"), "a\r\nb\r\nc\r\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "h.txt",
                "start_line": 2,
                "anchors": [line_anchor("b")],
                "replacement": "B",
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            fs::read_to_string(root.join("h.txt")).unwrap(),
            "a\r\nB\r\nc\r\n"
        );
    }

    #[tokio::test]
    async fn a_last_line_without_a_newline_stays_unterminated() {
        let root = temp_root();
        fs::write(root.join("i.txt"), "a\nb").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": "i.txt",
                "start_line": 2,
                "anchors": [line_anchor("b")],
                "replacement": "B",
            }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(fs::read_to_string(root.join("i.txt")).unwrap(), "a\nB");
    }

    #[tokio::test]
    async fn credential_files_are_refused() {
        let root = temp_root();
        fs::write(root.join(".env"), "API_KEY=sk-test-0000\n").unwrap();
        let result = tool(root.clone())
            .invoke(serde_json::json!({
                "path": ".env",
                "start_line": 1,
                "anchors": [line_anchor("API_KEY=sk-test-0000")],
                "replacement": "API_KEY=",
            }))
            .await;
        assert!(result.is_error);
        assert_eq!(
            fs::read_to_string(root.join(".env")).unwrap(),
            "API_KEY=sk-test-0000\n"
        );
    }
}
