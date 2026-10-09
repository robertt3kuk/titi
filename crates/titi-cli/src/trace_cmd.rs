//! `titi trace [session] [--turn N]` — print a session's spans as a tree.
//!
//! A reader, not a runner: it opens the trace files the engine wrote under
//! `<agent_dir>/traces/<session>/` and prints them, so it never needs a model
//! key and never starts an engine. It is short-circuited in `main` beside
//! `titi genome`, before the flag loop could mistake the session id for a
//! prompt.
//!
//! The tree shows what the ask called a stack trace: one line per span with
//! its kind, name, duration, and — for a model call — its tokens, cache hits
//! and cost, errors in place. A round's thinking is folded under its LLM span
//! (a one-line size, then the text when `trace.thinking` recorded it).

use std::io::Write;
use std::path::Path;

use titi_core::trace::{self, SpanKind, SpanStatus, TraceNode, TurnTrace};

/// The exit for a usage error: unknown option, bad `--turn`.
const USAGE_EXIT: i32 = 2;
/// The exit for "there is nothing to print": no session, or no such turn.
const MISSING_EXIT: i32 = 1;

/// The most thinking lines the tree prints under one span before it says how
/// many are left — a trace is for reading, not for dumping a transcript.
const THINKING_LINES: usize = 20;

/// Recognises `titi trace …` and prints it, exiting with a code. `None` when
/// the first argument is not `trace`, so the caller falls through to its own
/// parsing.
pub fn run(agent_dir: &Path) -> Option<()> {
    let mut words = std::env::args().skip(1);
    if words.next().as_deref() != Some("trace") {
        return None;
    }
    let args: Vec<String> = words.collect();
    let mut out = std::io::stdout();
    let code = dispatch(agent_dir, &args, &mut out);
    std::process::exit(code);
}

/// Parses the arguments, reads the trace and prints it. Returns the exit code
/// rather than exiting, so a test drives the whole path against a buffer.
fn dispatch(agent_dir: &Path, args: &[String], out: &mut impl Write) -> i32 {
    let mut session: Option<String> = None;
    let mut turn: Option<u64> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--turn" => {
                let Some(raw) = args.get(index + 1) else {
                    let _ = writeln!(out, "trace: --turn needs a number");
                    return USAGE_EXIT;
                };
                match raw.parse::<u64>() {
                    Ok(n) => turn = Some(n),
                    Err(_) => {
                        let _ = writeln!(out, "trace: --turn expects a number, got `{raw}`");
                        return USAGE_EXIT;
                    }
                }
                index += 2;
            }
            flag if flag.starts_with('-') => {
                let _ = writeln!(out, "trace: unknown option `{flag}`\n{}", usage());
                return USAGE_EXIT;
            }
            value => {
                if session.is_some() {
                    let _ = writeln!(out, "trace: one session id at most");
                    return USAGE_EXIT;
                }
                session = Some(value.to_string());
                index += 1;
            }
        }
    }

    let session = session.or_else(|| titi_cli_newest_session(agent_dir));
    let Some(session) = session else {
        let _ = writeln!(out, "trace: no session to show");
        return MISSING_EXIT;
    };

    let turns = match turn {
        Some(n) => match trace::read_turn(agent_dir, &session, n) {
            Ok(spans) => vec![TurnTrace { turn: n, spans }],
            Err(trace::TraceError::NotFound { .. }) => {
                let _ = writeln!(out, "trace: session {session} has no turn {n}");
                return MISSING_EXIT;
            }
            Err(error) => {
                let _ = writeln!(out, "trace: {error}");
                return MISSING_EXIT;
            }
        },
        None => match trace::read_session(agent_dir, &session) {
            Ok(turns) if turns.is_empty() => {
                let _ = writeln!(out, "trace: no spans recorded for session {session}");
                return MISSING_EXIT;
            }
            Ok(turns) => turns,
            Err(error) => {
                let _ = writeln!(out, "trace: {error}");
                return MISSING_EXIT;
            }
        },
    };

    let _ = out.write_all(render(&turns).as_bytes());
    0
}

/// The newest session, resolved the way the chat's session picker resolves it.
///
/// A free function so the trace reader does not depend on the chat module: the
/// session list is a filesystem fact (`session_fs`), not screen state.
fn titi_cli_newest_session(agent_dir: &Path) -> Option<String> {
    crate::session_fs::newest_session(agent_dir)
}

fn usage() -> &'static str {
    "usage: titi trace [session] [--turn N]"
}

/// Renders turns as an indented tree, one blank line between turns.
///
/// A turn's own `Turn` span is the header line, not a node under it: the
/// header already names the turn and carries its totals, and printing the same
/// span again as the root would repeat it. Its children become the top level,
/// and any other root (a `Turn` span that never arrived, a stray `Event`)
/// keeps its own line.
fn render(turns: &[TurnTrace]) -> String {
    let mut out = String::new();
    for (index, trace) in turns.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&turn_header(trace));
        out.push('\n');
        let mut top = Vec::new();
        for node in trace.tree() {
            if node.span.kind == SpanKind::Turn {
                top.extend(node.children);
            } else {
                top.push(node);
            }
        }
        render_nodes(&top, "", &mut out);
    }
    out
}

fn turn_header(trace: &TurnTrace) -> String {
    let mut line = format!("turn {} · {}", trace.turn, human_ms(trace.duration_ms()));
    let input = trace.input_tokens();
    let output = trace.output_tokens();
    if input > 0 || output > 0 {
        line.push_str(&format!(
            " · {} in / {} out",
            human_tokens(input),
            human_tokens(output)
        ));
    }
    if let Some(cost) = trace.cost_micro_usd() {
        line.push_str(&format!(" · {}", human_usd(cost)));
    }
    let errors = trace.errors();
    if errors > 0 {
        line.push_str(&format!(
            " · {errors} error{}",
            if errors == 1 { "" } else { "s" }
        ));
    }
    line
}

fn render_nodes(nodes: &[TraceNode], prefix: &str, out: &mut String) {
    for (index, node) in nodes.iter().enumerate() {
        let last = index + 1 == nodes.len();
        let connector = if last { "└─ " } else { "├─ " };
        out.push_str(prefix);
        out.push_str(connector);
        out.push_str(&span_line(&node.span));
        out.push('\n');

        // The children's prefix keeps the vertical bar for every node that is
        // not last, so the tree reads as one column.
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        render_thinking(&node.span, &child_prefix, out);
        render_nodes(&node.children, &child_prefix, out);
    }
}

/// The folded thinking of one span: a size line, then the text when it was
/// recorded, capped.
fn render_thinking(span: &titi_core::trace::Span, prefix: &str, out: &mut String) {
    let Some(chars) = thinking_chars(span) else {
        return;
    };
    out.push_str(prefix);
    out.push_str(&format!("thinking · {chars} chars\n"));
    let Some(text) = &span.thinking else {
        return;
    };
    let mut lines = text.lines();
    for line in lines.by_ref().take(THINKING_LINES) {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    let rest = lines.count();
    if rest > 0 {
        out.push_str(prefix);
        out.push_str(&format!("… {rest} more lines\n"));
    }
}

/// The thinking size: the recorded text's length, or the count the engine wrote
/// when the text itself was left out.
fn thinking_chars(span: &titi_core::trace::Span) -> Option<u64> {
    if let Some(text) = &span.thinking {
        return Some(text.chars().count() as u64);
    }
    span.attributes
        .get(trace::THINKING_CHARS_ATTR)
        .and_then(|value| value.as_u64())
}

fn span_line(span: &titi_core::trace::Span) -> String {
    let mut line = format!("{} {}", span.kind.label(), span.name);
    line.push_str(&format!(" · {}", human_ms(span.duration_ms())));
    if span.kind == SpanKind::Llm {
        let mut usage = format!("in {}", human_tokens(span.input_tokens));
        if span.cached_tokens > 0 {
            usage.push_str(&format!(" (cached {})", human_tokens(span.cached_tokens)));
        }
        usage.push_str(&format!(" out {}", human_tokens(span.output_tokens)));
        if span.reasoning_tokens > 0 {
            usage.push_str(&format!(
                " · {} reasoning",
                human_tokens(span.reasoning_tokens)
            ));
        }
        line.push_str(&format!(" · {usage}"));
        if let Some(cost) = span.cost_micro_usd {
            line.push_str(&format!(" · {}", human_usd(cost)));
        }
    }
    line.push_str(&status_suffix(span));
    line
}

fn status_suffix(span: &titi_core::trace::Span) -> String {
    match span.status {
        SpanStatus::Error => match &span.error {
            Some(error) => format!(" · error: {error}"),
            None => " · error".to_owned(),
        },
        SpanStatus::Cancelled => " · cancelled".to_owned(),
        // A tool's ok is worth a word (it is the thing that can fail without
        // failing the turn); an ok model call is the ordinary case.
        SpanStatus::Ok if span.kind == SpanKind::Tool => " · ok".to_owned(),
        SpanStatus::Ok => String::new(),
    }
}

/// A duration as the tree prints it: `12ms`, `1.2s`, `3m 05s`.
fn human_ms(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        return format!("{:.1}s", ms as f64 / 1_000.0);
    }
    let secs = ms / 1_000;
    format!("{}m {:02}s", secs / 60, secs % 60)
}

/// A token count, thousands shortened: `180`, `1.2k`, `12.4k`.
fn human_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    format!("{:.1}k", tokens as f64 / 1_000.0)
}

/// Money as the status line prints it, at a turn's four-decimal precision.
fn human_usd(micro_usd: u64) -> String {
    titi_tui::status::format_usd(micro_usd, 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use titi_core::trace::{Span, TraceWriter};

    fn span(id: &str, kind: SpanKind, start: u64, end: u64) -> Span {
        Span::new("s1", id, kind, "x", start).with_end_ms(end)
    }

    fn fixture(dir: &Path) {
        let mut w = TraceWriter::open(dir, "s1", 1).unwrap_or_else(|e| panic!("open: {e}"));
        let turn = Span::new("s1", "t", SpanKind::Turn, "turn 1", 0).with_end_ms(500);
        let mut llm = Span::new("s1", "l1", SpanKind::Llm, "chat gpt-4o", 10)
            .with_parent("t")
            .with_end_ms(200)
            .with_tokens(1_200, 180, 1_000, 40)
            .with_cost_micro_usd(2_100)
            .with_thinking("first thought\nsecond thought");
        llm.attributes
            .insert(trace::THINKING_CHARS_ATTR.into(), serde_json::json!(28));
        let tool = Span::new("s1", "r1", SpanKind::Tool, "read", 210)
            .with_parent("l1")
            .with_end_ms(240);
        let failed = Span::new("s1", "l2", SpanKind::Llm, "chat gpt-4o", 250)
            .with_parent("t")
            .with_end_ms(400)
            .with_tokens(300, 0, 0, 0)
            .with_error("timeout");
        for s in [turn, llm, tool, failed] {
            w.append(&s).unwrap_or_else(|e| panic!("append: {e}"));
        }
        w.flush().unwrap_or_else(|e| panic!("flush: {e}"));
    }

    fn run_into(dir: &Path, args: &[&str]) -> (i32, String) {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let mut buf: Vec<u8> = Vec::new();
        let code = dispatch(dir, &args, &mut buf);
        (
            code,
            String::from_utf8(buf).unwrap_or_else(|e| panic!("{e}")),
        )
    }

    #[test]
    fn prints_the_turn_tree_with_thinking_folded_under_its_span() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        fixture(dir.path());
        let (code, out) = run_into(dir.path(), &["s1"]);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "\
turn 1 · 500ms · 1.5k in / 180 out · $0.0021 · 1 error
├─ llm chat gpt-4o · 190ms · in 1.2k (cached 1.0k) out 180 · 40 reasoning · $0.0021
│  thinking · 28 chars
│  first thought
│  second thought
│  └─ tool read · 30ms · ok
└─ llm chat gpt-4o · 150ms · in 300 out 0 · error: timeout
"
        );
    }

    #[test]
    fn thinking_without_text_is_a_size_line_only() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut w = TraceWriter::open(dir.path(), "s1", 1).unwrap_or_else(|e| panic!("{e}"));
        let turn = Span::new("s1", "t", SpanKind::Turn, "turn 1", 0).with_end_ms(100);
        let llm = Span::new("s1", "l", SpanKind::Llm, "chat gpt-4o", 10)
            .with_parent("t")
            .with_end_ms(50)
            .with_attr(trace::THINKING_CHARS_ATTR, serde_json::json!(842));
        for s in [turn, llm] {
            w.append(&s).unwrap_or_else(|e| panic!("{e}"));
        }
        w.flush().unwrap_or_else(|e| panic!("{e}"));
        drop(w);

        let (code, out) = run_into(dir.path(), &["s1"]);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "\
turn 1 · 100ms
└─ llm chat gpt-4o · 40ms · in 0 out 0
   thinking · 842 chars
"
        );
    }

    #[test]
    fn turn_selects_one_file() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        fixture(dir.path());
        let mut w = TraceWriter::open(dir.path(), "s1", 2).unwrap_or_else(|e| panic!("{e}"));
        w.append(&span("t2", SpanKind::Turn, 600, 900))
            .unwrap_or_else(|e| panic!("{e}"));
        w.flush().unwrap_or_else(|e| panic!("{e}"));
        drop(w);

        let (code, out) = run_into(dir.path(), &["s1", "--turn", "2"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("turn 2 · 300ms\n"), "got: {out}");
        assert!(!out.contains("chat gpt-4o"));

        let (code, out) = run_into(dir.path(), &["s1", "--turn", "9"]);
        assert_eq!(code, MISSING_EXIT);
        assert!(out.contains("has no turn 9"), "got: {out}");
    }

    #[test]
    fn missing_and_malformed_arguments_choose_their_exit() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let (code, out) = run_into(dir.path(), &["--turn", "x"]);
        assert_eq!(code, USAGE_EXIT);
        assert!(out.contains("expects a number"), "got: {out}");

        let (code, out) = run_into(dir.path(), &["--wat"]);
        assert_eq!(code, USAGE_EXIT);
        assert!(out.contains("unknown option"), "got: {out}");

        // Two session ids is a usage error, not a silent pick.
        let (code, _) = run_into(dir.path(), &["a", "b"]);
        assert_eq!(code, USAGE_EXIT);

        // An empty agent dir has no session at all.
        let (code, out) = run_into(dir.path(), &[]);
        assert_eq!(code, MISSING_EXIT);
        assert!(out.contains("no session"), "got: {out}");
    }

    #[test]
    fn no_spans_recorded_is_a_missing_trace_not_an_empty_tree() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let (code, out) = run_into(dir.path(), &["s1"]);
        assert_eq!(code, MISSING_EXIT);
        assert!(out.contains("no spans recorded"), "got: {out}");
    }

    #[test]
    fn nested_agents_indent_under_their_parent() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut w = TraceWriter::open(dir.path(), "s1", 1).unwrap_or_else(|e| panic!("{e}"));
        let spans = vec![
            Span::new("s1", "t", SpanKind::Turn, "turn 1", 0).with_end_ms(100),
            Span::new("s1", "a", SpanKind::Agent, "find-the-bug", 10)
                .with_parent("t")
                .with_end_ms(90),
            Span::new("s1", "l", SpanKind::Llm, "chat gpt-4o", 20)
                .with_parent("a")
                .with_end_ms(60),
        ];
        for s in spans {
            w.append(&s).unwrap_or_else(|e| panic!("{e}"));
        }
        w.flush().unwrap_or_else(|e| panic!("{e}"));
        drop(w);

        let (code, out) = run_into(dir.path(), &["s1"]);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "\
turn 1 · 100ms
└─ agent find-the-bug · 80ms
   └─ llm chat gpt-4o · 40ms · in 0 out 0
"
        );
    }

    #[test]
    fn human_helpers() {
        assert_eq!(human_ms(0), "0ms");
        assert_eq!(human_ms(999), "999ms");
        assert_eq!(human_ms(1_234), "1.2s");
        assert_eq!(human_ms(185_000), "3m 05s");
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1_000), "1.0k");
        assert_eq!(human_usd(2_100), "$0.0021");
    }

    #[test]
    fn fixture_uses_a_trace_id_of_its_own() {
        // Guards the fixture against a copy-paste that would make the CLI
        // read another session's directory.
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        fixture(dir.path());
        assert!(PathBuf::from(dir.path()).join("traces/s1/1.jsonl").exists());
    }
}
