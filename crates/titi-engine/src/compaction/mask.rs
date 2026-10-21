//! Observation masking: the cheap half of a compaction.
//!
//! A tool result is the bulk of a long request — a whole file, a test log, a
//! grep — and by the time compaction fires the model has long since acted on
//! it. What it still needs is that the call happened and how to repeat it,
//! so the body is swapped for a one-line stub and the call stays in place.
//! The rewrite touches only results older than the kept tail and runs once,
//! when compaction fires, never per turn: every rewrite of an old message
//! restarts the provider's prompt cache from that message on.

use smol_str::SmolStr;
use titi_core::compaction::estimate_tokens;
use titi_providers::{ChatMessage, Role, ToolCallRef};

const STUB_OPEN: &str = "[tool result elided: ";

/// A result this short costs about what its stub does, so it stays.
const MIN_MASKED_CHARS: usize = 200;

/// How much of a call's target (a path, a command) the stub names.
const TARGET_CHARS: usize = 80;

/// Results that running the call again would not bring back: the user's
/// answer to a question, a subagent's report.
const NOT_REPEATABLE: [&str; 2] = ["ask", "agent"];

/// What one masking pass did.
#[derive(Debug, Default)]
pub struct Masked {
    pub count: usize,
    /// Estimated tokens the stubs took out of the request.
    pub saved_tokens: u64,
    /// The bodies that were replaced, by message index, for a summary that
    /// still wants to read what they said.
    pub originals: Vec<(usize, SmolStr)>,
}

/// Replaces the body of every tool result before `end` with a stub that names
/// the call, so a message that is already a stub is left alone and the pass is
/// idempotent. The messages keep their roles, order and `tool_call_id`s: a
/// call and its result stay paired on every wire.
pub fn mask_tool_results(messages: &mut [ChatMessage], end: usize) -> Masked {
    let mut masked = Masked::default();
    for at in 0..end.min(messages.len()) {
        let message = &messages[at];
        if message.role != Role::Tool
            || message.content.starts_with(STUB_OPEN)
            || message.content.chars().nth(MIN_MASKED_CHARS).is_none()
        {
            continue;
        }
        let call = call_for(messages, at);
        if call.is_some_and(|call| NOT_REPEATABLE.contains(&call.name.as_str())) {
            continue;
        }
        let stub: SmolStr = stub_for(call, message.content.chars().count()).into();
        masked.saved_tokens +=
            estimate_tokens(&message.content).saturating_sub(estimate_tokens(&stub));
        let original = std::mem::replace(&mut messages[at].content, stub);
        masked.originals.push((at, original));
        masked.count += 1;
    }
    masked
}

/// The call a result answers: by id when the result carries one, else by its
/// place among the results of the nearest assistant message that called tools
/// (how a history persisted before ids existed is paired).
pub(super) fn call_for(messages: &[ChatMessage], at: usize) -> Option<&ToolCallRef> {
    let (call_at, assistant) = messages[..at]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.role == Role::Assistant && !m.tool_calls.is_empty())?;
    match &messages[at].tool_call_id {
        Some(id) => assistant.tool_calls.iter().find(|call| call.call_id == *id),
        None => {
            let nth = messages[call_at + 1..at]
                .iter()
                .filter(|m| m.role == Role::Tool)
                .count();
            assistant.tool_calls.get(nth)
        }
    }
}

fn stub_for(call: Option<&ToolCallRef>, chars: usize) -> String {
    let name = call.map_or("tool", |call| call.name.as_str());
    match call.and_then(target_of) {
        Some(target) => {
            format!("{STUB_OPEN}{name} {target} · {chars} chars · re-run to see it]")
        }
        None => format!("{STUB_OPEN}{name} · {chars} chars · re-run to see it]"),
    }
}

/// What the call was aimed at — a path, a command, a pattern — in one line.
pub(super) fn target_of(call: &ToolCallRef) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let field = |key: &str| {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let target = match (field("pattern"), field("path")) {
        (Some(pattern), Some(path)) => format!("{pattern} in {path}"),
        (Some(only), None) | (None, Some(only)) => only.to_owned(),
        (None, None) => ["command", "query", "url"]
            .into_iter()
            .find_map(field)?
            .to_owned(),
    };
    let one_line = target.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(super::clamp_chars(&one_line, TARGET_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            tool_calls: vec![ToolCallRef {
                call_id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn result(id: Option<&str>, text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: text.into(),
            tool_call_id: id.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn a_result_becomes_a_stub_that_names_the_call() {
        let mut messages = vec![
            call("c1", "read", r#"{"path":"src/lib.rs"}"#),
            result(Some("c1"), &"x".repeat(900)),
            call("c2", "bash", r#"{"command":"cargo  test\n-p core"}"#),
            result(Some("c2"), &"y".repeat(300)),
            call("c3", "grep", r#"{"pattern":"fn main","path":"src"}"#),
            result(Some("c3"), &"z".repeat(250)),
        ];
        let masked = mask_tool_results(&mut messages, 6);

        assert_eq!(masked.count, 3);
        assert_eq!(
            messages[1].content,
            "[tool result elided: read src/lib.rs · 900 chars · re-run to see it]"
        );
        assert_eq!(
            messages[3].content,
            "[tool result elided: bash cargo test -p core · 300 chars · re-run to see it]"
        );
        assert_eq!(
            messages[5].content,
            "[tool result elided: grep fn main in src · 250 chars · re-run to see it]"
        );
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("c1"));
        assert!(masked.saved_tokens > 300, "{}", masked.saved_tokens);
        assert_eq!(masked.originals[0], (1, "x".repeat(900).into()));
    }

    #[test]
    fn only_results_before_the_end_are_touched() {
        let mut messages = vec![
            call("c1", "read", r#"{"path":"a"}"#),
            result(Some("c1"), &"x".repeat(900)),
            call("c2", "read", r#"{"path":"b"}"#),
            result(Some("c2"), &"y".repeat(900)),
        ];
        let masked = mask_tool_results(&mut messages, 2);
        assert_eq!(masked.count, 1);
        assert_eq!(messages[3].content, "y".repeat(900));
    }

    #[test]
    fn short_results_stubs_and_unrepeatable_tools_stay() {
        let mut messages = vec![
            call("c1", "read", r#"{"path":"a"}"#),
            result(Some("c1"), "ok"),
            call("c2", "ask", r#"{"question":"which one?"}"#),
            result(Some("c2"), &"the user typed a long answer ".repeat(20)),
            call("c3", "read", r#"{"path":"c"}"#),
            result(Some("c3"), &"w".repeat(900)),
        ];
        assert_eq!(mask_tool_results(&mut messages, 6).count, 1);
        let again = mask_tool_results(&mut messages, 6);
        assert_eq!(again.count, 0, "a stub is not masked twice");
        assert_eq!(messages[1].content, "ok");
        assert!(messages[3].content.starts_with("the user typed"));
    }

    #[test]
    fn a_result_without_an_id_is_paired_by_order() {
        let mut two = call("c1", "read", r#"{"path":"first"}"#);
        two.tool_calls.push(ToolCallRef {
            call_id: "c2".into(),
            name: "read".into(),
            arguments: r#"{"path":"second"}"#.into(),
            ..Default::default()
        });
        let mut messages = vec![
            two,
            result(None, &"a".repeat(300)),
            result(None, &"b".repeat(300)),
        ];
        mask_tool_results(&mut messages, 3);
        assert!(messages[1].content.contains("read first"));
        assert!(messages[2].content.contains("read second"));
    }

    #[test]
    fn a_call_with_no_usable_target_is_named_alone() {
        let mut messages = vec![
            call("c1", "todo", "not json"),
            result(Some("c1"), &"t".repeat(300)),
        ];
        mask_tool_results(&mut messages, 2);
        assert_eq!(
            messages[1].content,
            "[tool result elided: todo · 300 chars · re-run to see it]"
        );
    }
}
