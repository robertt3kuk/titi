//! Rendering a session's conversation for humans and for machines.
//!
//! Two formats, one source of truth: the entry chain a surface already walks.
//! Markdown is for reading, JSONL is for feeding back in — `to_jsonl` and
//! [`from_jsonl`] are exact inverses, so an exported session can be diffed,
//! archived, or replayed.
//!
//! Entry text is rendered verbatim. Tool output is masked when it is written
//! to the session (`titi-tools`), so an export carries exactly what the model
//! saw and never re-derives a secret.

use std::path::Path;

use super::SessionError;
use super::entry::{Entry, Role};

/// Target format of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Human-readable transcript.
    Markdown,
    /// One JSON entry per line, round-trippable.
    Jsonl,
}

impl ExportFormat {
    /// Parses a format name (`md`, `markdown`, `jsonl`), with or without a
    /// leading dot; `None` when the name means nothing here.
    pub fn parse(name: &str) -> Option<Self> {
        match name
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "md" | "markdown" => Some(Self::Markdown),
            "jsonl" => Some(Self::Jsonl),
            _ => None,
        }
    }

    /// Infers the format from a destination path's extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        Self::parse(path.extension()?.to_str()?)
    }

    /// File extension for this format, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Jsonl => "jsonl",
        }
    }
}

/// Renders the conversation in `format`.
pub fn render(
    format: ExportFormat,
    title: Option<&str>,
    entries: &[Entry],
) -> Result<String, SessionError> {
    match format {
        ExportFormat::Markdown => Ok(to_markdown(title, entries)),
        ExportFormat::Jsonl => to_jsonl(entries),
    }
}

/// Renders the conversation as a Markdown transcript: one section per
/// message, in order, tool output fenced so it cannot be mistaken for prose.
pub fn to_markdown(title: Option<&str>, entries: &[Entry]) -> String {
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Session");
    let mut out = format!("# {title}\n");
    for entry in entries {
        out.push_str("\n## ");
        out.push_str(heading(entry.role));
        out.push_str("\n\n");
        let content = entry.content.trim();
        if entry.role == Role::Tool {
            let fence = fence_for(content);
            out.push_str(&fence);
            out.push('\n');
            if !content.is_empty() {
                out.push_str(content);
                out.push('\n');
            }
            out.push_str(&fence);
            out.push('\n');
        } else if !content.is_empty() {
            out.push_str(content);
            out.push('\n');
        }
        for call in &entry.tool_calls {
            out.push_str(&format!(
                "\n- tool call `{}` (id `{}`)\n",
                call.name, call.call_id
            ));
        }
    }
    out
}

/// Renders the conversation as JSONL: one entry per line, newline-terminated.
pub fn to_jsonl(entries: &[Entry]) -> Result<String, SessionError> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serde_json::to_string(entry).map_err(SessionError::Json)?);
        out.push('\n');
    }
    Ok(out)
}

/// Parses an exported JSONL transcript back into entries.
///
/// Strict, unlike the store's lenient reader: an export that does not parse
/// is a broken artifact, not a crash-torn tail to be skipped.
pub fn from_jsonl(text: &str) -> Result<Vec<Entry>, SessionError> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line).map_err(SessionError::Json)?);
    }
    Ok(out)
}

fn heading(role: Role) -> &'static str {
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::System => "System",
        Role::Tool => "Tool result",
    }
}

/// A fence long enough to survive backticks inside the fenced text.
fn fence_for(text: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(role: Role, content: &str) -> Entry {
        Entry::new(None, role, content)
    }

    #[test]
    fn markdown_keeps_the_conversation_in_order() {
        let entries = vec![
            entry(Role::User, "read Cargo.toml"),
            entry(Role::Assistant, "on it").with_tool_calls(vec![titi_providers::ToolCallRef {
                call_id: "call-1".into(),
                name: "read".into(),
            }]),
            entry(Role::Tool, "[package]\nname = \"titi\""),
            entry(Role::Assistant, "it is the workspace root"),
        ];

        let md = to_markdown(Some("workspace root"), &entries);

        assert!(md.starts_with("# workspace root\n"), "{md}");
        let order: Vec<usize> = [
            "## User",
            "read Cargo.toml",
            "## Assistant",
            "on it",
            "- tool call `read` (id `call-1`)",
            "## Tool result",
            "name = \"titi\"",
            "it is the workspace root",
        ]
        .iter()
        .map(|needle| {
            md.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {md}"))
        })
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{md}");
    }

    #[test]
    fn markdown_fences_tool_output_that_contains_a_fence() {
        let entries = vec![entry(Role::Tool, "```\nfn main() {}\n```")];
        let md = to_markdown(None, &entries);
        assert!(md.contains("````\n```\nfn main() {}\n```\n````"), "{md}");
    }

    #[test]
    fn jsonl_is_one_line_per_message_and_round_trips() {
        let entries = vec![
            entry(Role::User, "hello"),
            entry(Role::Assistant, "line one\nline two"),
            entry(Role::Tool, "OPENAI_API_KEY=sk-test"),
        ];

        let jsonl = to_jsonl(&entries).unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(jsonl.lines().count(), entries.len());
        assert_eq!(
            from_jsonl(&jsonl).unwrap_or_else(|e| panic!("{e}")),
            entries
        );
    }

    #[test]
    fn broken_jsonl_is_an_error_not_a_silent_gap() {
        assert!(matches!(from_jsonl("{oops"), Err(SessionError::Json(_))));
    }

    #[test]
    fn format_comes_from_a_name_or_a_path() {
        assert_eq!(ExportFormat::parse("MD"), Some(ExportFormat::Markdown));
        assert_eq!(ExportFormat::parse(".jsonl"), Some(ExportFormat::Jsonl));
        assert_eq!(ExportFormat::parse("pdf"), None);
        assert_eq!(
            ExportFormat::from_path(Path::new("/tmp/chat.markdown")),
            Some(ExportFormat::Markdown)
        );
        assert_eq!(ExportFormat::from_path(Path::new("/tmp/chat")), None);
        assert_eq!(ExportFormat::Jsonl.extension(), "jsonl");
    }
}
