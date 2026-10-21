//! Command hooks: shell commands the engine runs at fixed points of a turn.
//!
//! Claude Code's hook contract, so a hook written for it runs here unchanged:
//! the event goes to the command as JSON on stdin; exit 0 is fine (stdout may
//! be JSON carrying a decision or context), exit 2 refuses (stderr is the
//! reason), any other exit is a warning that blocks nothing.
//!
//! Two rules sit on top of that contract:
//!
//! * **A hook can tighten, never loosen.** `PreToolUse` may deny a call or
//!   force an approval prompt, even under `--approval yolo`. An `allow` is no
//!   opinion: it never skips the prompt the user's own approval mode asks for.
//! * **A hook that cannot decide does not decide.** A timeout, a crash or
//!   unreadable output is reported and the turn goes on, because a broken
//!   formatter must not brick the session.
//!
//! Which hooks exist is not decided here. The surface hands the engine the
//! definitions it resolved and the user approved (`SetHooks`); the engine
//! never reads a hook file or the trust store.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use smol_str::SmolStr;
use titi_config::hooks::{HookDef, HookEvent};
use titi_tools::{HookRun, Interrupt, PipeError};
use tokio::sync::{OnceCell, mpsc};

use crate::protocol::{EngineEvent, HookOutcome, TurnId};

/// How long a hook may run when its definition names no timeout: the same
/// bound a `bash` call gets before it is handed to the background.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest a definition may ask for.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Characters of hook output that become model context.
pub const CONTEXT_CAP: usize = 10_000;
/// Characters of a block reason that reach the model or a transcript row.
const REASON_CAP: usize = 2_000;
/// Characters of a detail on the wire: a row holds a line.
const DETAIL_CAP: usize = 200;

/// The Claude Code name for one of titi's tools, so a matcher or a script
/// written for it keeps working. Names with no Claude counterpart (and MCP
/// tools, which already use `mcp__server__tool`) are returned as they are.
pub fn claude_tool_name(native: &str) -> &str {
    match native {
        "bash" => "Bash",
        "edit" | "hashline_edit" => "Edit",
        "write" => "Write",
        "read" => "Read",
        "grep" => "Grep",
        "glob" => "Glob",
        "fetch" => "WebFetch",
        "web_search" => "WebSearch",
        "agent" => "Agent",
        other => other,
    }
}

/// What a matcher string means.
enum Matcher {
    All,
    /// `Bash|Edit`, `Bash, Write`: exact names.
    Names(Vec<String>),
    /// Anything else is a regular expression, searched (not anchored), as
    /// `mcp__.*__write.*` is.
    Pattern(regex::Regex),
}

impl Matcher {
    fn compile(text: Option<&str>) -> Result<Self, String> {
        let text = text.map(str::trim).unwrap_or("");
        if text.is_empty() || text == "*" {
            return Ok(Self::All);
        }
        if text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '|' | ',' | ' '))
        {
            let names = text
                .split(['|', ','])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            return Ok(Self::Names(names));
        }
        regex::Regex::new(text)
            .map(Self::Pattern)
            .map_err(|error| error.to_string())
    }

    fn matches(&self, names: &[&str]) -> bool {
        match self {
            Self::All => true,
            Self::Names(wanted) => names.iter().any(|name| wanted.iter().any(|w| w == name)),
            Self::Pattern(pattern) => names.iter().any(|name| pattern.is_match(name)),
        }
    }
}

struct Compiled {
    def: HookDef,
    matcher: Matcher,
}

struct Inner {
    defs: RwLock<Arc<Vec<Compiled>>>,
    session_id: String,
    cwd: PathBuf,
    interrupt: Interrupt,
    /// What `SessionStart` hooks said, computed once: it goes into the system
    /// prompt, which must not change from turn to turn.
    session_context: OnceCell<Option<String>>,
}

/// The session's hooks. Cheap to clone; every clone sees `set`.
#[derive(Clone)]
pub struct Hooks {
    inner: Arc<Inner>,
    /// Whether a hook's `ask` can reach a person. A subagent has no approval
    /// surface, so for it an `ask` is a refusal.
    can_ask: bool,
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new(None, PathBuf::from("."), Interrupt::new())
    }
}

/// What the `PreToolUse` hooks made of one call.
#[derive(Debug, PartialEq, Eq)]
pub enum PreToolVerdict {
    /// Run it (subject to the user's own approval mode). `context` is what
    /// hooks added for the model.
    Proceed { context: Option<String> },
    /// Do not run it; the model is told `reason`.
    Deny { reason: String },
    /// Ask the user even if the approval mode would not.
    Ask { context: Option<String> },
}

/// What the `UserPromptSubmit` hooks made of a prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptVerdict {
    /// Send it, with `context` after it when a hook added some.
    Pass { context: Option<String> },
    /// Do not send it; the user is told `reason`.
    Block { reason: String },
}

/// One hook's result, before the hooks of an event are combined.
enum Decision {
    Ok { context: Option<String> },
    Block(String),
    Deny(String),
    Ask { context: Option<String> },
    Warn(String),
    TimedOut(String),
}

struct Ran {
    name: String,
    decision: Decision,
}

impl Hooks {
    pub fn new(session_id: Option<String>, cwd: PathBuf, interrupt: Interrupt) -> Self {
        Self {
            inner: Arc::new(Inner {
                defs: RwLock::new(Arc::new(Vec::new())),
                session_id: session_id.unwrap_or_default(),
                cwd,
                interrupt,
                session_context: OnceCell::new(),
            }),
            can_ask: true,
        }
    }

    /// The same hooks for a caller with nobody to ask: a subagent's call that
    /// a hook wants approved is refused instead of waiting for an answer that
    /// cannot come.
    pub fn without_asking(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            can_ask: false,
        }
    }

    /// Replaces the hook list. Returns a line for each matcher that is not a
    /// valid pattern: such a hook is dropped rather than run on everything.
    pub fn set(&self, defs: Vec<HookDef>) -> Vec<String> {
        let mut notices = Vec::new();
        let mut compiled = Vec::with_capacity(defs.len());
        for def in defs {
            match Matcher::compile(def.matcher.as_deref()) {
                Ok(matcher) => compiled.push(Compiled { def, matcher }),
                Err(error) => notices.push(format!(
                    "hooks: {} {} is not run: matcher {:?} is not a valid pattern ({error})",
                    def.event.as_str(),
                    def.display_name(),
                    def.matcher.as_deref().unwrap_or_default()
                )),
            }
        }
        if let Ok(mut slot) = self.inner.defs.write() {
            *slot = Arc::new(compiled);
        }
        notices
    }

    fn snapshot(&self) -> Arc<Vec<Compiled>> {
        self.inner
            .defs
            .read()
            .map(|defs| Arc::clone(&defs))
            .unwrap_or_default()
    }

    /// Whether any `PreToolUse` or `PostToolUse` hook applies to this tool.
    /// A call such a hook watches is never run beside others as part of a
    /// group, because a group skips the gate the hook sits in.
    pub fn watches_tool(&self, tool: &str) -> bool {
        let names = tool_names(tool);
        self.snapshot().iter().any(|hook| {
            matches!(
                hook.def.event,
                HookEvent::PreToolUse | HookEvent::PostToolUse
            ) && hook.matcher.matches(&names)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }

    /// Whether a `Stop` hook is set, which is whether a turn's end has to
    /// wait for it.
    pub fn has_stop(&self) -> bool {
        self.snapshot()
            .iter()
            .any(|hook| hook.def.event == HookEvent::Stop)
    }

    /// Runs the `PreToolUse` hooks that match `tool`.
    pub async fn pre_tool_use(
        &self,
        tool: &str,
        call_id: &str,
        args: &Value,
        turn_id: TurnId,
        events: &mpsc::Sender<EngineEvent>,
    ) -> PreToolVerdict {
        let mut input = self.base(HookEvent::PreToolUse);
        input.insert("tool_name".into(), json!(claude_tool_name(tool)));
        input.insert("tool_input".into(), tool_input(args));
        input.insert("tool_use_id".into(), json!(call_id));
        let ran = self
            .run(
                HookEvent::PreToolUse,
                &tool_names(tool),
                input,
                Some(turn_id),
                events,
            )
            .await;
        let mut reasons = Vec::new();
        let mut asked = false;
        let mut contexts = Vec::new();
        for Ran { name, decision } in ran {
            match decision {
                Decision::Block(reason) | Decision::Deny(reason) => {
                    reasons.push(format!(
                        "PreToolUse hook {name} refused this call: {reason}"
                    ));
                }
                Decision::Ask { context } if !self.can_ask => {
                    reasons.push(format!(
                        "PreToolUse hook {name} wants this call approved, and a subagent has no one to ask{}",
                        context.map(|text| format!(": {text}")).unwrap_or_default()
                    ));
                }
                Decision::Ask { context } => {
                    asked = true;
                    contexts.extend(context.map(|text| attributed(&name, &text)));
                }
                Decision::Ok { context } => {
                    contexts.extend(context.map(|text| attributed(&name, &text)));
                }
                Decision::Warn(_) | Decision::TimedOut(_) => {}
            }
        }
        let context = join_context(contexts);
        if !reasons.is_empty() {
            PreToolVerdict::Deny {
                reason: reasons.join("\n"),
            }
        } else if asked {
            PreToolVerdict::Ask { context }
        } else {
            PreToolVerdict::Proceed { context }
        }
    }

    /// Runs the `PostToolUse` hooks that match `tool`. What they have to say
    /// — context, or the reason of an exit 2 — comes back as one attributed
    /// note for the tool's answer; the call already ran, so nothing can be
    /// refused any more.
    #[allow(clippy::too_many_arguments)]
    pub async fn post_tool_use(
        &self,
        tool: &str,
        call_id: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        turn_id: TurnId,
        events: &mpsc::Sender<EngineEvent>,
    ) -> Option<String> {
        let mut input = self.base(HookEvent::PostToolUse);
        input.insert("tool_name".into(), json!(claude_tool_name(tool)));
        input.insert("tool_input".into(), tool_input(args));
        input.insert("tool_use_id".into(), json!(call_id));
        input.insert(
            "tool_response".into(),
            json!({ "output": output, "is_error": is_error }),
        );
        let ran = self
            .run(
                HookEvent::PostToolUse,
                &tool_names(tool),
                input,
                Some(turn_id),
                events,
            )
            .await;
        let mut notes = Vec::new();
        for Ran { name, decision } in ran {
            match decision {
                Decision::Ok { context } | Decision::Ask { context } => {
                    notes.extend(context.map(|text| attributed(&name, &text)));
                }
                Decision::Block(reason) | Decision::Deny(reason) => {
                    notes.push(attributed(&name, &reason));
                }
                Decision::Warn(_) | Decision::TimedOut(_) => {}
            }
        }
        join_context(notes)
    }

    /// Runs the `UserPromptSubmit` hooks.
    pub async fn user_prompt_submit(
        &self,
        prompt: &str,
        turn_id: TurnId,
        events: &mpsc::Sender<EngineEvent>,
    ) -> PromptVerdict {
        let mut input = self.base(HookEvent::UserPromptSubmit);
        input.insert("prompt".into(), json!(prompt));
        let ran = self
            .run(
                HookEvent::UserPromptSubmit,
                &[],
                input,
                Some(turn_id),
                events,
            )
            .await;
        let mut reasons = Vec::new();
        let mut contexts = Vec::new();
        for Ran { name, decision } in ran {
            match decision {
                Decision::Block(reason) | Decision::Deny(reason) => reasons.push(reason),
                Decision::Ok { context } | Decision::Ask { context } => {
                    contexts.extend(context.map(|text| attributed(&name, &text)));
                }
                Decision::Warn(_) | Decision::TimedOut(_) => {}
            }
        }
        if reasons.is_empty() {
            PromptVerdict::Pass {
                context: join_context(contexts),
            }
        } else {
            PromptVerdict::Block {
                reason: reasons.join("\n"),
            }
        }
    }

    /// Runs the `Stop` hooks when the model has finished. `Some(reason)`
    /// means one of them refused to let the turn end: the reason goes to the
    /// model, which continues. The turn asks once, so `stop_hook_active` — the
    /// field a Claude script checks to avoid looping — is always false.
    pub async fn stop(
        &self,
        turn_id: TurnId,
        events: &mpsc::Sender<EngineEvent>,
    ) -> Option<String> {
        let mut input = self.base(HookEvent::Stop);
        input.insert("stop_hook_active".into(), json!(false));
        let ran = self
            .run(HookEvent::Stop, &[], input, Some(turn_id), events)
            .await;
        let reasons: Vec<String> = ran
            .into_iter()
            .filter_map(|Ran { name, decision }| match decision {
                Decision::Block(reason) | Decision::Deny(reason) => {
                    Some(format!("Stop hook {name}: {reason}"))
                }
                _ => None,
            })
            .collect();
        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }

    /// What the `SessionStart` hooks print, run the first time it is asked
    /// and the same text afterwards: it belongs to the system prompt, and a
    /// byte that moved there would cost the provider's cache every turn.
    pub async fn session_context(&self, events: &mpsc::Sender<EngineEvent>) -> Option<String> {
        self.inner
            .session_context
            .get_or_init(|| async {
                let mut input = self.base(HookEvent::SessionStart);
                input.insert("source".into(), json!("startup"));
                let ran = self
                    .run(HookEvent::SessionStart, &["startup"], input, None, events)
                    .await;
                let contexts = ran
                    .into_iter()
                    .filter_map(|Ran { name, decision }| match decision {
                        Decision::Ok { context } | Decision::Ask { context } => {
                            context.map(|text| attributed(&name, &text))
                        }
                        _ => None,
                    })
                    .collect();
                join_context(contexts)
            })
            .await
            .clone()
    }

    fn base(&self, event: HookEvent) -> Map<String, Value> {
        let mut input = Map::new();
        input.insert("session_id".into(), json!(self.inner.session_id));
        input.insert("cwd".into(), json!(self.inner.cwd.to_string_lossy()));
        input.insert("hook_event_name".into(), json!(event.as_str()));
        input
    }

    /// Runs every hook of `event` whose matcher accepts one of `names`, at
    /// once, and reports each on the wire. Two hooks with the same command
    /// and timeout are one hook.
    async fn run(
        &self,
        event: HookEvent,
        names: &[&str],
        input: Map<String, Value>,
        turn_id: Option<TurnId>,
        events: &mpsc::Sender<EngineEvent>,
    ) -> Vec<Ran> {
        let snapshot = self.snapshot();
        let mut chosen: Vec<&HookDef> = Vec::new();
        for hook in snapshot.iter() {
            // Only the tool events and `SessionStart` read the matcher; a
            // matcher on `Stop` or `UserPromptSubmit` is ignored, as it is in
            // Claude Code.
            let reads_matcher = matches!(
                event,
                HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::SessionStart
            );
            if hook.def.event != event || (reads_matcher && !hook.matcher.matches(names)) {
                continue;
            }
            if !chosen.iter().any(|known| {
                known.command == hook.def.command && known.timeout_secs == hook.def.timeout_secs
            }) {
                chosen.push(&hook.def);
            }
        }
        if chosen.is_empty() {
            return Vec::new();
        }
        let payload: Arc<str> = Value::Object(input).to_string().into();
        let runs = chosen.into_iter().map(|def| {
            let command = def.command.clone();
            let timeout = def
                .timeout_secs
                .map(|secs| Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS)))
                .unwrap_or(DEFAULT_TIMEOUT);
            let cwd = self.inner.cwd.clone();
            let interrupt = self.inner.interrupt.clone();
            let payload = Arc::clone(&payload);
            let name = def.display_name();
            async move {
                let started = Instant::now();
                let raw = tokio::task::spawn_blocking(move || {
                    titi_tools::run_hook(&command, &cwd, payload.as_bytes(), timeout, &interrupt)
                })
                .await
                .unwrap_or_else(|error| {
                    Err(PipeError::Spawn {
                        reason: error.to_string(),
                    })
                });
                (name, classify(event, raw), started.elapsed())
            }
        });
        let mut ran = Vec::new();
        for (name, decision, elapsed) in futures::future::join_all(runs).await {
            let (outcome, detail) = describe(&decision);
            let _ = events
                .send(EngineEvent::HookRan {
                    turn_id,
                    event: event.as_str().into(),
                    hook: name.as_str().into(),
                    outcome,
                    detail,
                    ms: elapsed.as_millis() as u64,
                })
                .await;
            ran.push(Ran { name, decision });
        }
        ran
    }
}

/// Both spellings of a tool's name, Claude's first.
fn tool_names(tool: &str) -> Vec<&str> {
    let claude = claude_tool_name(tool);
    if claude == tool {
        vec![tool]
    } else {
        vec![claude, tool]
    }
}

/// The call's arguments as a Claude script expects them: `path` is also
/// offered as `file_path`.
fn tool_input(args: &Value) -> Value {
    let mut args = match args {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    if let Some(path) = args.get("path").cloned()
        && !args.contains_key("file_path")
    {
        args.insert("file_path".into(), path);
    }
    Value::Object(args)
}

/// The two halves of a [`Decision`] a surface sees.
fn describe(decision: &Decision) -> (HookOutcome, SmolStr) {
    let line = |text: &str| -> SmolStr {
        text.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .chars()
            .take(DETAIL_CAP)
            .collect::<String>()
            .into()
    };
    match decision {
        Decision::Ok { context: None } => (HookOutcome::Ok, SmolStr::default()),
        Decision::Ok {
            context: Some(text),
        } => (HookOutcome::Context, line(text)),
        Decision::Block(reason) => (HookOutcome::Blocked, line(reason)),
        Decision::Deny(reason) => (HookOutcome::Denied, line(reason)),
        Decision::Ask { context } => (
            HookOutcome::Asked,
            line(context.as_deref().unwrap_or("approval required by a hook")),
        ),
        Decision::Warn(text) => (HookOutcome::Warning, line(text)),
        Decision::TimedOut(text) => (HookOutcome::TimedOut, line(text)),
    }
}

/// One command's exit and output, read as the hook contract says.
fn classify(event: HookEvent, raw: Result<HookRun, PipeError>) -> Decision {
    let run = match raw {
        Ok(run) => run,
        Err(PipeError::TimedOut { after, .. }) => {
            return Decision::TimedOut(format!(
                "timed out after {}s and was killed",
                after.as_secs()
            ));
        }
        Err(error) => return Decision::Warn(error.to_string()),
    };
    match run.exit_code {
        Some(0) => classify_stdout(event, &run.stdout),
        Some(2) => {
            let reason = cap(run.stderr.trim(), REASON_CAP);
            Decision::Block(if reason.is_empty() {
                "blocked with no reason given".to_owned()
            } else {
                reason
            })
        }
        Some(code) => {
            let first = run.stderr.lines().find(|line| !line.trim().is_empty());
            Decision::Warn(match first {
                Some(line) => format!("exited {code}: {}", cap(line.trim(), DETAIL_CAP)),
                None => format!("exited {code}"),
            })
        }
        None => Decision::Warn("ended by a signal".to_owned()),
    }
}

/// An exit 0's stdout: a JSON object may carry a decision and context; any
/// other text is context for the two events that take it.
fn classify_stdout(event: HookEvent, stdout: &str) -> Decision {
    let text = stdout.trim();
    if text.is_empty() {
        return Decision::Ok { context: None };
    }
    if text.starts_with('{')
        && let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text)
    {
        let specific = object.get("hookSpecificOutput");
        let string_at = |parent: Option<&Value>, key: &str| -> Option<String> {
            parent
                .and_then(|value| value.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let context = string_at(specific, "additionalContext")
            .or_else(|| string_at(Some(&Value::Object(object.clone())), "additionalContext"))
            .map(|text| cap(&text, CONTEXT_CAP))
            .filter(|text| !text.trim().is_empty());
        let reason = string_at(specific, "permissionDecisionReason")
            .or_else(|| string_at(Some(&Value::Object(object.clone())), "reason"))
            .map(|text| cap(&text, REASON_CAP));
        // `decision: "block"` is the older spelling every event understands.
        if object.get("decision").and_then(Value::as_str) == Some("block") {
            return Decision::Block(
                reason.unwrap_or_else(|| "blocked with no reason given".to_owned()),
            );
        }
        if event == HookEvent::PreToolUse {
            match string_at(specific, "permissionDecision").as_deref() {
                Some("deny") => {
                    return Decision::Deny(
                        reason.unwrap_or_else(|| "denied with no reason given".to_owned()),
                    );
                }
                Some("ask") => {
                    return Decision::Ask {
                        context: reason.or(context),
                    };
                }
                // `allow` is no opinion: a hook cannot approve a call on the
                // user's behalf.
                _ => {}
            }
        }
        return Decision::Ok { context };
    }
    match event {
        HookEvent::UserPromptSubmit | HookEvent::SessionStart => Decision::Ok {
            context: Some(cap(text, CONTEXT_CAP)),
        },
        _ => Decision::Ok { context: None },
    }
}

/// `text` cut to `limit` characters, with a note when it was.
fn cap(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}… [cut at {limit} characters]")
}

fn attributed(hook: &str, text: &str) -> String {
    format!("[hook · {hook}]\n{}", text.trim())
}

fn join_context(parts: Vec<String>) -> Option<String> {
    let joined = parts.join("\n\n");
    (!joined.trim().is_empty()).then(|| cap(&joined, CONTEXT_CAP))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        hooks: Hooks,
        events: mpsc::Sender<EngineEvent>,
        seen: mpsc::Receiver<EngineEvent>,
    }

    fn def(event: HookEvent, matcher: Option<&str>, command: &str) -> HookDef {
        HookDef {
            event,
            matcher: matcher.map(str::to_owned),
            command: command.to_owned(),
            timeout_secs: None,
            source: "test".to_owned(),
        }
    }

    fn fixture(defs: Vec<HookDef>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::new(
            Some("sess-1".into()),
            dir.path().to_path_buf(),
            Interrupt::new(),
        );
        assert!(hooks.set(defs).is_empty());
        let (events, seen) = mpsc::channel(64);
        Fixture {
            dir,
            hooks,
            events,
            seen,
        }
    }

    impl Fixture {
        fn ran(&mut self) -> Vec<(HookOutcome, String, String)> {
            let mut out = Vec::new();
            while let Ok(event) = self.seen.try_recv() {
                if let EngineEvent::HookRan {
                    outcome,
                    hook,
                    detail,
                    ..
                } = event
                {
                    out.push((outcome, hook.to_string(), detail.to_string()));
                }
            }
            out
        }

        async fn pre(&self, tool: &str, args: Value) -> PreToolVerdict {
            self.hooks
                .pre_tool_use(tool, "call-1", &args, TurnId(1), &self.events)
                .await
        }
    }

    #[test]
    fn titi_tool_names_map_to_claudes_and_native_names_still_match() {
        for (native, claude) in [
            ("bash", "Bash"),
            ("edit", "Edit"),
            ("hashline_edit", "Edit"),
            ("write", "Write"),
            ("read", "Read"),
            ("grep", "Grep"),
            ("glob", "Glob"),
            ("fetch", "WebFetch"),
            ("web_search", "WebSearch"),
            ("mcp__srv__tool", "mcp__srv__tool"),
        ] {
            assert_eq!(claude_tool_name(native), claude);
        }
        let hooks = Hooks::default();
        let none = hooks.set(vec![
            def(HookEvent::PreToolUse, Some("Bash|Edit"), "true"),
            def(HookEvent::PostToolUse, Some("^mcp__.*__write"), "true"),
            def(HookEvent::PreToolUse, Some("grep"), "true"),
        ]);
        assert!(none.is_empty());
        assert!(hooks.watches_tool("bash"));
        assert!(
            hooks.watches_tool("hashline_edit"),
            "Edit covers both editors"
        );
        assert!(hooks.watches_tool("mcp__fs__write_file"));
        assert!(hooks.watches_tool("grep"), "a native name matches too");
        assert!(!hooks.watches_tool("read"));
        assert!(!hooks.watches_tool("glob"));
    }

    #[test]
    fn an_invalid_matcher_is_dropped_with_a_notice_not_run_on_everything() {
        let hooks = Hooks::default();
        let notices = hooks.set(vec![def(HookEvent::PreToolUse, Some("(unclosed"), "true")]);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(hooks.is_empty());
    }

    #[tokio::test]
    async fn pre_tool_use_gets_the_claude_shaped_json_on_stdin() {
        let mut f = fixture(vec![def(
            HookEvent::PreToolUse,
            Some("Write"),
            "cat > seen.json",
        )]);
        let verdict = f
            .pre("write", json!({"path": "src/a.rs", "content": "x"}))
            .await;
        assert_eq!(verdict, PreToolVerdict::Proceed { context: None });
        let seen: Value =
            serde_json::from_str(&std::fs::read_to_string(f.dir.path().join("seen.json")).unwrap())
                .unwrap();
        assert_eq!(seen["hook_event_name"], "PreToolUse");
        assert_eq!(seen["session_id"], "sess-1");
        assert_eq!(seen["cwd"], f.dir.path().to_string_lossy().as_ref());
        assert_eq!(seen["tool_name"], "Write");
        assert_eq!(seen["tool_use_id"], "call-1");
        assert_eq!(seen["tool_input"]["path"], "src/a.rs");
        assert_eq!(
            seen["tool_input"]["file_path"], "src/a.rs",
            "Claude's name for it"
        );
        assert_eq!(f.ran()[0].0, HookOutcome::Ok);
    }

    #[tokio::test]
    async fn post_tool_use_gets_the_response_too() {
        let f = fixture(vec![def(HookEvent::PostToolUse, None, "cat > seen.json")]);
        let note = f
            .hooks
            .post_tool_use(
                "bash",
                "call-2",
                &json!({"command": "ls"}),
                "a\nb\n",
                false,
                TurnId(1),
                &f.events,
            )
            .await;
        assert_eq!(note, None);
        let seen: Value =
            serde_json::from_str(&std::fs::read_to_string(f.dir.path().join("seen.json")).unwrap())
                .unwrap();
        assert_eq!(seen["hook_event_name"], "PostToolUse");
        assert_eq!(seen["tool_name"], "Bash");
        assert_eq!(seen["tool_input"]["command"], "ls");
        assert_eq!(seen["tool_response"]["output"], "a\nb\n");
        assert_eq!(seen["tool_response"]["is_error"], false);
    }

    #[tokio::test]
    async fn prompt_stop_and_session_start_inputs() {
        let f = fixture(vec![
            def(HookEvent::UserPromptSubmit, None, "cat > prompt.json"),
            def(HookEvent::Stop, None, "cat > stop.json"),
            def(HookEvent::SessionStart, None, "cat > start.json"),
        ]);
        f.hooks
            .user_prompt_submit("fix the bug", TurnId(1), &f.events)
            .await;
        f.hooks.stop(TurnId(1), &f.events).await;
        f.hooks.session_context(&f.events).await;
        let read = |name: &str| -> Value {
            serde_json::from_str(&std::fs::read_to_string(f.dir.path().join(name)).unwrap())
                .unwrap()
        };
        let prompt = read("prompt.json");
        assert_eq!(prompt["hook_event_name"], "UserPromptSubmit");
        assert_eq!(prompt["prompt"], "fix the bug");
        let stop = read("stop.json");
        assert_eq!(stop["hook_event_name"], "Stop");
        assert_eq!(stop["stop_hook_active"], false);
        let start = read("start.json");
        assert_eq!(start["hook_event_name"], "SessionStart");
        assert_eq!(start["source"], "startup");
        for shape in [&prompt, &stop, &start] {
            assert_eq!(shape["session_id"], "sess-1");
            assert!(shape["cwd"].is_string());
        }
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_as_the_reason() {
        let mut f = fixture(vec![def(
            HookEvent::PreToolUse,
            Some("Bash"),
            "echo 'no rm -rf here' >&2; exit 2",
        )]);
        let verdict = f.pre("bash", json!({"command": "rm -rf /"})).await;
        let PreToolVerdict::Deny { reason } = verdict else {
            panic!("expected a deny, got {verdict:?}");
        };
        assert!(reason.contains("no rm -rf here"), "{reason}");
        assert!(reason.contains("PreToolUse"), "{reason}");
        let ran = f.ran();
        assert_eq!(ran[0].0, HookOutcome::Blocked);
        assert_eq!(ran[0].2, "no rm -rf here");
    }

    #[tokio::test]
    async fn other_exit_codes_warn_and_never_block() {
        let mut f = fixture(vec![def(
            HookEvent::PreToolUse,
            None,
            "echo 'lint crashed' >&2; exit 1",
        )]);
        assert_eq!(
            f.pre("bash", json!({})).await,
            PreToolVerdict::Proceed { context: None }
        );
        let ran = f.ran();
        assert_eq!(ran[0].0, HookOutcome::Warning);
        assert!(ran[0].2.contains("lint crashed"), "{:?}", ran[0]);
    }

    #[tokio::test]
    async fn json_deny_and_ask_decide_and_allow_is_no_opinion() {
        let deny = r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy says no"}}"#;
        let ask = r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"check this one"}}"#;
        let allow = r#"{"hookSpecificOutput":{"permissionDecision":"allow","permissionDecisionReason":"fine"}}"#;
        for (json_out, want) in [(deny, "deny"), (ask, "ask"), (allow, "proceed")] {
            let f = fixture(vec![def(
                HookEvent::PreToolUse,
                None,
                &format!("cat >/dev/null; printf '%s' '{json_out}'"),
            )]);
            let verdict = f.pre("bash", json!({})).await;
            match (want, &verdict) {
                ("deny", PreToolVerdict::Deny { reason }) => {
                    assert!(reason.contains("policy says no"), "{reason}");
                }
                ("ask", PreToolVerdict::Ask { .. }) => {}
                // R4: `allow` changes nothing — the call still meets the
                // user's own approval mode.
                ("proceed", PreToolVerdict::Proceed { .. }) => {}
                _ => panic!("{want}: got {verdict:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_hook_that_times_out_fails_open_and_says_so() {
        let mut f = fixture(vec![HookDef {
            timeout_secs: Some(1),
            ..def(HookEvent::PreToolUse, None, "sleep 30")
        }]);
        let started = Instant::now();
        assert_eq!(
            f.pre("bash", json!({})).await,
            PreToolVerdict::Proceed { context: None }
        );
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(f.ran()[0].0, HookOutcome::TimedOut);
    }

    #[tokio::test]
    async fn plain_stdout_is_context_only_for_prompt_and_session_start() {
        let f = fixture(vec![
            def(
                HookEvent::UserPromptSubmit,
                None,
                "cat >/dev/null; echo 'branch: main'",
            ),
            def(
                HookEvent::PostToolUse,
                None,
                "cat >/dev/null; echo 'ignored'",
            ),
            def(
                HookEvent::SessionStart,
                None,
                "cat >/dev/null; echo 'repo notes'",
            ),
        ]);
        let PromptVerdict::Pass { context } =
            f.hooks.user_prompt_submit("hi", TurnId(1), &f.events).await
        else {
            panic!("a hook that printed text did not block");
        };
        assert!(context.unwrap().contains("branch: main"));
        assert_eq!(
            f.hooks
                .post_tool_use("bash", "c", &json!({}), "out", false, TurnId(1), &f.events)
                .await,
            None,
            "plain stdout after a tool is not context"
        );
        assert!(
            f.hooks
                .session_context(&f.events)
                .await
                .unwrap()
                .contains("repo notes")
        );
    }

    #[tokio::test]
    async fn context_is_capped() {
        let f = fixture(vec![def(
            HookEvent::UserPromptSubmit,
            None,
            "cat >/dev/null; head -c 50000 /dev/zero | tr '\\0' 'x'",
        )]);
        let PromptVerdict::Pass { context } =
            f.hooks.user_prompt_submit("hi", TurnId(1), &f.events).await
        else {
            panic!("passes");
        };
        let context = context.unwrap();
        assert!(
            context.chars().count() < CONTEXT_CAP + 200,
            "{}",
            context.len()
        );
        assert!(context.contains("cut at"));
    }

    #[tokio::test]
    async fn exit_two_on_a_prompt_blocks_it_and_on_stop_asks_the_model_to_go_on() {
        let f = fixture(vec![
            def(
                HookEvent::UserPromptSubmit,
                None,
                "cat >/dev/null; echo 'no secrets in prompts' >&2; exit 2",
            ),
            def(
                HookEvent::Stop,
                None,
                "cat >/dev/null; echo 'tests not run' >&2; exit 2",
            ),
        ]);
        let verdict = f
            .hooks
            .user_prompt_submit("my key is sk-test", TurnId(1), &f.events)
            .await;
        assert_eq!(
            verdict,
            PromptVerdict::Block {
                reason: "no secrets in prompts".into()
            }
        );
        let reason = f.hooks.stop(TurnId(1), &f.events).await.unwrap();
        assert!(reason.contains("tests not run"), "{reason}");
    }

    #[tokio::test]
    async fn the_matcher_decides_which_tool_a_hook_sees() {
        let mut f = fixture(vec![def(
            HookEvent::PreToolUse,
            Some("Bash"),
            "cat >/dev/null; echo no >&2; exit 2",
        )]);
        assert!(matches!(
            f.pre("bash", json!({})).await,
            PreToolVerdict::Deny { .. }
        ));
        assert_eq!(
            f.pre("read", json!({})).await,
            PreToolVerdict::Proceed { context: None }
        );
        assert_eq!(f.ran().len(), 1, "the read never started a hook");
    }

    #[tokio::test]
    async fn identical_hooks_run_once() {
        let mut f = fixture(vec![
            def(HookEvent::Stop, None, "cat >/dev/null"),
            def(HookEvent::Stop, None, "cat >/dev/null"),
        ]);
        f.hooks.stop(TurnId(1), &f.events).await;
        assert_eq!(f.ran().len(), 1);
    }

    #[tokio::test]
    async fn the_hook_environment_has_no_keys() {
        // The process environment is the test runner's; what it holds that
        // names a secret must not reach the hook.
        let Some((name, _)) = std::env::vars().find(|(name, _)| titi_tools::is_secret_env(name))
        else {
            return;
        };
        let f = fixture(vec![def(
            HookEvent::UserPromptSubmit,
            None,
            &format!("cat >/dev/null; printf '%s' \"${{{name}:-scrubbed}}\""),
        )]);
        let PromptVerdict::Pass { context } =
            f.hooks.user_prompt_submit("hi", TurnId(1), &f.events).await
        else {
            panic!("passes");
        };
        assert!(context.unwrap().contains("scrubbed"));
    }
}
