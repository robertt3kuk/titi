//! What keeps a long tool loop honest: the wrap-up at the round cap, and the
//! watch for a model that is going round in circles.
//!
//! A turn used to die at its cap and discard the answer. The cap is now a
//! budget the model is told about: at the limit it gets one last request with
//! tool calls forbidden and is asked to say what is done and what is left.
//! The watch is a nudge, never a stop: it appends one line to a tool result
//! the model is about to read, and the turn goes on.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use smol_str::SmolStr;
use titi_providers::{ChatMessage, Role};

use crate::tool_loop::PendingToolCall;

/// Tool rounds the main turn may spend before it is asked to wrap up.
pub const DEFAULT_TOOL_ROUNDS: u32 = 100;

/// The request that closes a turn which spent its tool budget.
///
/// A user message, not a system one: Anthropic and Gemini carry only the
/// leading system messages and drop one that arrives mid-conversation.
pub(crate) const WRAP_UP_NOTE: &str = "System note: tool budget reached: summarise what is done, \
what is left, and stop. No tool calls are possible in this reply.";

pub(crate) fn wrap_up_message() -> ChatMessage {
    ChatMessage {
        role: Role::User,
        content: WRAP_UP_NOTE.into(),
        tool_calls: Vec::new(),
        ..Default::default()
    }
}

/// The note is a request for one answer, not part of the conversation to keep:
/// a later turn replaying it would be told the budget is spent when it is not.
pub(crate) fn strip_wrap_up(messages: &mut Vec<ChatMessage>) {
    if let Some(at) = messages
        .iter()
        .rposition(|message| message.role == Role::User && message.content == WRAP_UP_NOTE)
    {
        messages.remove(at);
    }
}

/// What the person reading the transcript is told when the budget ran out.
pub(crate) fn wrap_up_notice(rounds: u32) -> SmolStr {
    format!(
        "tool budget reached after {rounds} tool rounds: the model was asked to summarise \
instead of continuing (raise agent.maxToolRounds to allow more)"
    )
    .into()
}

/// Identical calls, with the same result each time, after which the model is
/// told it is repeating itself.
const REPEAT_LIMIT: u32 = 3;

/// Consecutive rounds that only re-read unchanged files after which the model
/// is told it is not making progress.
const STALE_READ_ROUNDS: u32 = 4;

/// One call the model made, as the watch needs it.
pub(crate) struct IssuedCall {
    call_id: SmolStr,
    name: SmolStr,
    arguments: String,
}

impl IssuedCall {
    pub(crate) fn of(calls: &[PendingToolCall]) -> Vec<Self> {
        calls
            .iter()
            .map(|call| Self {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .collect()
    }

    /// The call as a model would recognise it, short enough for one line.
    fn describe(&self) -> String {
        let flat: String = self
            .arguments
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let shown: String = flat.chars().take(80).collect();
        let more = if flat.chars().count() > 80 { "…" } else { "" };
        format!("`{} {shown}{more}`", self.name)
    }

    /// The same call whatever order its arguments were written in.
    fn key(&self) -> String {
        let canonical = serde_json::from_str::<serde_json::Value>(&self.arguments)
            .map_or_else(|_| self.arguments.clone(), |value| value.to_string());
        format!("{}\0{canonical}", self.name)
    }

    fn read_path(&self) -> Option<String> {
        if self.name != "read" {
            return None;
        }
        let value = serde_json::from_str::<serde_json::Value>(&self.arguments).ok()?;
        value.get("path")?.as_str().map(str::to_owned)
    }
}

struct CallRecord {
    /// Times this call has been made with the result it last got.
    same_result: u32,
    last_result: u64,
    nudged: bool,
}

/// Notices a turn that is repeating itself and says so once.
///
/// Two signals, each nudged at most once per turn so the note stays a note:
/// the same call (name and arguments) issued [`REPEAT_LIMIT`] times with the
/// same result each time, and [`STALE_READ_ROUNDS`] rounds in a row that did
/// nothing but read files already read, unchanged. A call whose result differs
/// from the last time restarts its count: the same `project test` after an
/// edit is work, not a loop.
#[derive(Default)]
pub(crate) struct LoopWatch {
    calls: HashMap<String, CallRecord>,
    reads: HashSet<(String, u64)>,
    stale_rounds: u32,
    stale_nudged: bool,
}

fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl LoopWatch {
    /// Looks at one finished round and, when it trips a signal, appends the
    /// nudge to the result of the call that tripped it.
    ///
    /// `results` is what the round produced: the tool messages the model reads
    /// next. A call with no result (the turn was cancelled) is skipped.
    pub(crate) fn observe(&mut self, issued: &[IssuedCall], results: &mut [ChatMessage]) {
        let mut nudges: Vec<(usize, String)> = Vec::new();
        let mut only_stale_reads = !issued.is_empty();
        for call in issued {
            let Some(at) = results.iter().position(|message| {
                message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some(call.call_id.as_str())
            }) else {
                only_stale_reads = false;
                continue;
            };
            let result = digest(&results[at].content);

            let record = self.calls.entry(call.key()).or_insert(CallRecord {
                same_result: 0,
                last_result: result,
                nudged: false,
            });
            if record.last_result != result {
                record.same_result = 0;
            }
            record.last_result = result;
            record.same_result += 1;
            if record.same_result >= REPEAT_LIMIT && !record.nudged {
                record.nudged = true;
                nudges.push((
                    at,
                    format!(
                        "[titi: you are repeating {}: {} identical calls, the same result each \
time. Change approach or conclude.]",
                        call.describe(),
                        record.same_result
                    ),
                ));
            }

            match call.read_path() {
                Some(path) => {
                    if self.reads.insert((path, result)) {
                        only_stale_reads = false;
                    }
                }
                None => only_stale_reads = false,
            }
        }

        if only_stale_reads {
            self.stale_rounds += 1;
        } else {
            self.stale_rounds = 0;
        }
        if self.stale_rounds >= STALE_READ_ROUNDS
            && !self.stale_nudged
            && let Some(at) = issued.last().and_then(|call| {
                results.iter().position(|message| {
                    message.role == Role::Tool
                        && message.tool_call_id.as_deref() == Some(call.call_id.as_str())
                })
            })
        {
            self.stale_nudged = true;
            nudges.push((
                at,
                format!(
                    "[titi: you are repeating yourself: the last {} rounds only re-read files \
you had already read, unchanged. Change approach or conclude.]",
                    self.stale_rounds
                ),
            ));
        }

        for (at, note) in nudges {
            let message = &mut results[at];
            message.content = format!("{}\n\n{note}", message.content).into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: &str) -> IssuedCall {
        IssuedCall {
            call_id: id.into(),
            name: name.into(),
            arguments: arguments.to_owned(),
        }
    }

    fn result(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(id.into()),
            ..Default::default()
        }
    }

    /// One round: the calls and what each answered, as `execute_tools` returns
    /// them (the assistant message first, which the watch must step over).
    fn round(watch: &mut LoopWatch, calls: &[(&str, &str, &str, &str)]) -> Vec<ChatMessage> {
        let issued: Vec<IssuedCall> = calls
            .iter()
            .map(|(id, name, args, _)| call(id, name, args))
            .collect();
        let mut results = vec![ChatMessage {
            role: Role::Assistant,
            content: "calling".into(),
            ..Default::default()
        }];
        results.extend(calls.iter().map(|(id, _, _, out)| result(id, out)));
        watch.observe(&issued, &mut results);
        results
    }

    fn nudged(results: &[ChatMessage]) -> bool {
        results.iter().any(|m| m.content.contains("[titi:"))
    }

    #[test]
    fn the_third_identical_call_with_the_same_result_is_nudged_once() {
        let mut watch = LoopWatch::default();
        for n in 1..=2 {
            let results = round(
                &mut watch,
                &[(&format!("c{n}"), "bash", r#"{"command":"ls"}"#, "a b")],
            );
            assert!(!nudged(&results), "call {n} is not yet a pattern");
        }
        let results = round(&mut watch, &[("c3", "bash", r#"{"command":"ls"}"#, "a b")]);
        let note = &results[1].content;
        assert!(
            note.starts_with("a b\n\n[titi: you are repeating `bash"),
            "{note}"
        );
        assert!(note.contains("Change approach or conclude"), "{note}");

        let again = round(&mut watch, &[("c4", "bash", r#"{"command":"ls"}"#, "a b")]);
        assert!(!nudged(&again), "one nudge per call per turn");
    }

    #[test]
    fn argument_order_does_not_hide_a_repeat() {
        let mut watch = LoopWatch::default();
        round(
            &mut watch,
            &[("c1", "read", r#"{"path":"a","limit":5}"#, "x")],
        );
        round(
            &mut watch,
            &[("c2", "read", r#"{"limit":5,"path":"a"}"#, "x")],
        );
        let results = round(
            &mut watch,
            &[("c3", "read", r#"{"path":"a","limit":5}"#, "x")],
        );
        assert!(nudged(&results));
    }

    #[test]
    fn different_arguments_or_a_changing_result_are_not_a_repeat() {
        let mut watch = LoopWatch::default();
        for n in 1..=6 {
            let args = format!(r#"{{"command":"ls dir{n}"}}"#);
            let results = round(&mut watch, &[(&format!("a{n}"), "bash", &args, "same")]);
            assert!(!nudged(&results), "different commands, call {n}");
        }
        // The same command, but the world changed in between.
        for n in 1..=6 {
            let results = round(
                &mut watch,
                &[(
                    &format!("b{n}"),
                    "project",
                    r#"{"action":"test"}"#,
                    &format!("{n} failing"),
                )],
            );
            assert!(
                !nudged(&results),
                "a result that moves is progress, call {n}"
            );
        }
    }

    #[test]
    fn four_rounds_of_unchanged_rereads_are_nudged_once() {
        let mut watch = LoopWatch::default();
        // Fresh reads are not a re-read, however many.
        for (n, path) in ["a", "b", "c", "d"].iter().enumerate() {
            let args = format!(r#"{{"path":"{path}"}}"#);
            let results = round(
                &mut watch,
                &[(&format!("f{n}"), "read", &args, &format!("body of {path}"))],
            );
            assert!(!nudged(&results));
        }
        // Now only what it already has, one file a round so that no single
        // call has been made three times.
        for (n, path) in ["b", "c", "d", "a"].iter().enumerate() {
            let args = format!(r#"{{"path":"{path}"}}"#);
            let results = round(
                &mut watch,
                &[(&format!("r{n}"), "read", &args, &format!("body of {path}"))],
            );
            assert_eq!(nudged(&results), n == 3, "re-read round {}", n + 1);
            if n == 3 {
                assert!(
                    results[1].content.contains("only re-read files"),
                    "{results:?}"
                );
            }
        }
        // A fifth, with arguments never seen before so it is not a repeat call.
        let results = round(
            &mut watch,
            &[("r9", "read", r#"{"path":"b","limit":9}"#, "body of b")],
        );
        assert!(!nudged(&results), "the stale-read nudge is once per turn");
    }

    #[test]
    fn a_changed_file_or_another_tool_breaks_the_stale_streak() {
        let mut watch = LoopWatch::default();
        for (id, path, body) in [("f1", "a", "v1"), ("f2", "b", "w1"), ("f3", "c", "x1")] {
            let args = format!(r#"{{"path":"{path}"}}"#);
            round(&mut watch, &[(id, "read", &args, body)]);
        }
        // Three stale rounds, then a grep: new work, and the streak is gone.
        for (id, path, body) in [("s1", "a", "v1"), ("s2", "b", "w1"), ("s3", "c", "x1")] {
            let args = format!(r#"{{"path":"{path}"}}"#);
            round(&mut watch, &[(id, "read", &args, body)]);
        }
        let grep = round(&mut watch, &[("g1", "grep", r#"{"pattern":"x"}"#, "hit")]);
        assert!(!nudged(&grep));
        // A file that changed under it is a new read, not a stale one.
        let edited = round(&mut watch, &[("e1", "read", r#"{"path":"a"}"#, "v2")]);
        assert!(!nudged(&edited));
        // One stale round after the reset is one, not four.
        let again = round(
            &mut watch,
            &[("e2", "read", r#"{"path":"b","limit":3}"#, "w1")],
        );
        assert!(!nudged(&again));
    }

    #[test]
    fn the_wrap_up_note_is_removed_from_the_history_it_closed() {
        let mut messages = vec![
            result("c1", "out"),
            wrap_up_message(),
            ChatMessage {
                role: Role::Assistant,
                content: "summary".into(),
                ..Default::default()
            },
        ];
        strip_wrap_up(&mut messages);
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|m| m.content != WRAP_UP_NOTE));
    }
}
