//! The digest message a fold leaves behind, and what it carries besides the
//! summary: the files to read again, and the digest of an earlier fold.

use titi_providers::{ChatMessage, Role};

const OPEN: &str = "<conversation-summary";
const CLOSE: &str = "</conversation-summary>";
const PREFACE: &str =
    "Earlier messages were folded to keep the context short; this is what they held.";

/// How many file paths the digest asks the model to re-read.
pub const REREAD_FILES: usize = 6;

const REREAD_HEADER: &str =
    "Re-read these files if you need their current contents (most recent first):";

/// The tools whose `path` argument names a file the model has seen or changed.
const FILE_TOOLS: [&str; 4] = ["read", "write", "edit", "hashline_edit"];

/// A digest of an earlier fold, which a later fold folds in turn.
pub fn is_digest(message: &ChatMessage) -> bool {
    message.role == Role::User && message.content.starts_with(OPEN)
}

/// The digest as a message.
///
/// A `user` message right behind the system prompt, not a `system` one in the
/// middle of the history: some providers drop a system message that is not
/// first and Anthropic's wire has no such role at all, so only a user message
/// reaches every family. Behind the system prompt is also the most stable
/// place for it — everything above is rebuilt identically each turn and
/// everything below only grows, so the provider's prompt cache keeps the
/// whole prefix up to the digest until the next fold rewrites it.
///
/// The re-read list is appended here, from the tool calls of what was folded,
/// rather than asked of the model: a summary of what a file said goes stale
/// the moment it is edited, and the path is the cheap thing that does not.
pub fn digest_message(summary: &str, folded: usize, prefix: &[ChatMessage]) -> ChatMessage {
    let mut body = summary.trim_end().to_owned();
    let paths = reread_paths(prefix);
    if !paths.is_empty() {
        body.push_str("\n\n");
        body.push_str(REREAD_HEADER);
        for path in paths {
            body.push_str("\n- ");
            body.push_str(&path);
        }
    }
    ChatMessage {
        role: Role::User,
        content: format!("{OPEN} folded=\"{folded}\">\n{PREFACE}\n{body}\n{CLOSE}").into(),
        ..Default::default()
    }
}

/// What an earlier digest said, without its wrapper and its file list.
pub fn digest_summary(message: &ChatMessage) -> &str {
    let text = message
        .content
        .split_once('\n')
        .map_or("", |(_, rest)| rest.strip_prefix(PREFACE).unwrap_or(rest));
    let text = text.trim_start_matches('\n');
    let text = text.strip_suffix(CLOSE).unwrap_or(text);
    text.split(REREAD_HEADER).next().unwrap_or("").trim()
}

/// The last [`REREAD_FILES`] distinct files the folded messages read or
/// changed, most recent first. A file an earlier digest listed counts as read
/// before everything this prefix did, so the list survives a second fold.
fn reread_paths(prefix: &[ChatMessage]) -> Vec<String> {
    let mut chronological: Vec<String> = Vec::new();
    for message in prefix {
        if is_digest(message) {
            let listed: Vec<String> = message
                .content
                .lines()
                .skip_while(|line| *line != REREAD_HEADER)
                .skip(1)
                .map_while(|line| line.strip_prefix("- "))
                .map(str::to_owned)
                .collect();
            chronological.extend(listed.into_iter().rev());
            continue;
        }
        for call in &message.tool_calls {
            if !FILE_TOOLS.contains(&call.name.as_str()) {
                continue;
            }
            let args: Option<serde_json::Value> = serde_json::from_str(&call.arguments).ok();
            if let Some(path) = args
                .as_ref()
                .and_then(|args| args.get("path"))
                .and_then(serde_json::Value::as_str)
                .filter(|path| !path.trim().is_empty())
            {
                chronological.push(path.to_owned());
            }
        }
    }
    let mut newest_first: Vec<String> = Vec::new();
    for path in chronological.into_iter().rev() {
        if !newest_first.contains(&path) {
            newest_first.push(path);
        }
        if newest_first.len() == REREAD_FILES {
            break;
        }
    }
    newest_first
}

#[cfg(test)]
mod tests {
    use super::*;
    use titi_providers::ToolCallRef;

    fn call(name: &str, arguments: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            tool_calls: vec![ToolCallRef {
                call_id: "c".into(),
                name: name.into(),
                arguments: arguments.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_newest_distinct_files_are_listed_newest_first() {
        let mut prefix: Vec<ChatMessage> = (1..=8)
            .map(|n| call("read", &format!(r#"{{"path":"src/f{n}.rs"}}"#)))
            .collect();
        prefix.push(call("edit", r#"{"path":"src/f2.rs","old_string":"a"}"#));
        prefix.push(call("bash", r#"{"command":"cargo test"}"#));
        prefix.push(call("grep", r#"{"pattern":"x","path":"src"}"#));

        let digest = digest_message("the summary", 11, &prefix);

        assert_eq!(digest.role, Role::User);
        let listed: Vec<&str> = digest
            .content
            .lines()
            .skip_while(|line| *line != REREAD_HEADER)
            .skip(1)
            .map_while(|line| line.strip_prefix("- "))
            .collect();
        assert_eq!(
            listed,
            [
                "src/f2.rs",
                "src/f8.rs",
                "src/f7.rs",
                "src/f6.rs",
                "src/f5.rs",
                "src/f4.rs"
            ],
            "{}",
            digest.content
        );
    }

    #[test]
    fn no_file_calls_means_no_list() {
        let digest = digest_message("the summary", 2, &[call("bash", r#"{"command":"ls"}"#)]);
        assert!(!digest.content.contains(REREAD_HEADER));
        assert!(digest.content.contains("the summary"));
    }

    #[test]
    fn an_earlier_digest_hands_over_its_summary_and_its_files() {
        let first = digest_message(
            "goal: fix the lexer",
            4,
            &[call("read", r#"{"path":"src/lexer.rs"}"#)],
        );
        assert!(is_digest(&first));
        assert_eq!(digest_summary(&first), "goal: fix the lexer");

        let second = digest_message(
            "goal: fix the lexer, then the parser",
            9,
            &[first, call("edit", r#"{"path":"src/parser.rs"}"#)],
        );
        assert!(
            second
                .content
                .ends_with("- src/parser.rs\n- src/lexer.rs\n</conversation-summary>"),
            "{}",
            second.content
        );
    }
}
