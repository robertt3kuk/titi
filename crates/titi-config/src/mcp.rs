//! MCP server definitions, read from the files other agents read.
//!
//! A server is a program the engine starts and speaks to over its standard
//! streams. The file shape is the ecosystem's `mcpServers` map, so the setup
//! instructions a server ships with work here without being converted:
//!
//! ```json
//! {"mcpServers": {"files": {"command": "npx", "args": ["-y", "some-server"],
//!   "env": {"ROOT": "."}, "cwd": "tools", "timeout": 30}}}
//! ```
//!
//! Like [`crate::hooks`], this module only *reads and describes*. Whether a
//! definition may start is decided by the surface: the user's own file runs
//! unconditionally, a repository's (`.titi/mcp.json`, `.mcp.json`) only once
//! the [`crate::trust`] store holds the hash of that exact definition. The
//! engine is handed the approved list and never reads a file. Nothing here
//! starts anything.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::settings::Settings;

/// The user's own file, under the agent directory.
pub const USER_FILE: &str = "mcp.json";
/// A repository's files, in the order they are read; a server defined in both
/// is the first file's.
pub const PROJECT_FILES: [&str; 2] = [".titi/mcp.json", ".mcp.json"];

/// Seconds one request to a server may take when its entry names no
/// `timeout`. The same bound applies to the start-up handshake.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Longest server name kept. The model-facing tool name is
/// `mcp__<server>__<tool>` and providers cap a tool name at 64 characters,
/// so the server's share of it is bounded here and the tool's after.
pub const MAX_NAME_LEN: usize = 32;

/// The approval tier a server's tools are offered under. A tool the server
/// ships is untrusted code the model drives, so the default is the highest
/// tier, and only the user's own settings may lower it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpApproval {
    Read,
    Write,
    Exec,
}

impl McpApproval {
    pub fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "exec" => Some(Self::Exec),
            _ => None,
        }
    }
}

/// One stdio server, as a file defined it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDef {
    /// The key in `mcpServers`, made safe to be part of a tool name.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Added to the small allow-listed environment the server starts with.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Relative paths resolve against the workspace.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    /// Seconds a request may take; [`DEFAULT_TIMEOUT_SECS`] when `None`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Set by the surface from the user's own settings, never read from a
    /// file: a repository cannot lower the approval of its own server.
    #[serde(default)]
    pub approval: Option<McpApproval>,
    /// The file it came from, for a person to read. Not part of the
    /// definition.
    #[serde(default)]
    pub source: String,
}

impl McpServerDef {
    /// What the definition says that decides what runs, and only that: the
    /// input of its hash. The timeout, `disabled` and the file it came from
    /// change nothing about the code that starts. The environment is hashed
    /// by value as well as by key: `NODE_OPTIONS`, `PATH` and `LD_PRELOAD`
    /// are all one edit away from running something else.
    pub fn definition(&self) -> Value {
        json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "env": self.env,
            "cwd": self.cwd.as_deref().unwrap_or(""),
        })
    }

    /// What approving this server in `workspace` approves. A server that runs
    /// a file of the repository (`./bin/server`, `node tools/server.js`) is
    /// only as trustworthy as that file, so its content is part of the hash,
    /// as it is for a hook.
    pub fn hash(&self, workspace: &Path) -> String {
        let mut definition = self.definition();
        if let (Some(script), Some(object)) = (self.script(workspace), definition.as_object_mut()) {
            object.insert(
                "script".to_owned(),
                json!({
                    "path": script.path.to_string_lossy(),
                    "sha256": script.sha256,
                }),
            );
        }
        crate::trust::hash_definition(&definition)
    }

    /// The repository file this server runs, if its command line names one.
    pub fn script(&self, workspace: &Path) -> Option<crate::hooks::Script> {
        let base = match self.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => workspace.join(cwd),
            _ => workspace.to_owned(),
        };
        crate::hooks::script_of(
            std::iter::once(self.command.clone()).chain(self.args.iter().cloned()),
            workspace,
            &base,
        )
    }

    /// The command line as a person would type it, for a panel.
    pub fn command_line(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Everything the MCP files define, before anyone has decided what may run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// From the user's own file: they run unconditionally.
    pub user: Vec<McpServerDef>,
    /// From the repository: they run only once trusted. Disabled entries and
    /// names the user's own file already defines are left out.
    pub project: Vec<McpServerDef>,
    /// One line per file that could not be read or entry that was skipped,
    /// so a server that does not start is never silent.
    pub notices: Vec<String>,
}

/// Reads every MCP file.
pub fn discover(agent_dir: &Path, workspace: &Path) -> Discovered {
    let mut found = Discovered::default();
    read_into(
        &agent_dir.join(USER_FILE),
        "mcp.json",
        &mut found.user,
        &mut found.notices,
    );
    let mut project = Vec::new();
    for file in PROJECT_FILES {
        read_into(
            &workspace.join(file),
            file,
            &mut project,
            &mut found.notices,
        );
    }
    for def in project {
        if def.disabled {
            continue;
        }
        if found.user.iter().any(|own| own.name == def.name) {
            found.notices.push(format!(
                "mcp: {}: the server {:?} is not started — a server of that name is already in your own mcp.json",
                def.source, def.name
            ));
        } else {
            found.project.push(def);
        }
    }
    found
}

fn read_into(path: &Path, label: &str, into: &mut Vec<McpServerDef>, notices: &mut Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            notices.push(format!("mcp: could not read {label}: {error}"));
            return;
        }
    };
    let (defs, skipped) = match parse(&text, label) {
        Ok(parsed) => parsed,
        Err(reason) => {
            notices.push(format!("mcp: {label} is not read: {reason}"));
            return;
        }
    };
    notices.extend(skipped);
    for def in defs {
        // A name taken by an earlier file is that file's server. Two files
        // that agree are one server, not a conflict.
        match into.iter().find(|known| known.name == def.name) {
            None => into.push(def),
            Some(known) if known.definition() == def.definition() => {}
            Some(known) => notices.push(format!(
                "mcp: {}: the server {:?} is ignored — {} defines it already",
                def.source, def.name, known.source
            )),
        }
    }
}

/// The name a server is known by in a tool name: letters, digits, `_` and
/// `-`, anything else turned into `_`, at most [`MAX_NAME_LEN`] characters.
/// `None` for a name with nothing left.
pub fn server_name(raw: &str) -> Option<String> {
    let name: String = raw
        .trim()
        .chars()
        .take(MAX_NAME_LEN)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Parses one file's text. The `Err` is a file that is not MCP JSON at all;
/// the second half of `Ok` names the entries that were skipped.
pub fn parse(text: &str, source: &str) -> Result<(Vec<McpServerDef>, Vec<String>), String> {
    let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let Some(servers) = root.get("mcpServers") else {
        return Ok((Vec::new(), Vec::new()));
    };
    let Some(servers) = servers.as_object() else {
        return Err("`mcpServers` is not an object".to_owned());
    };
    let mut defs: Vec<McpServerDef> = Vec::new();
    let mut skipped = Vec::new();
    for (raw_name, entry) in servers {
        let skip = |why: &str| format!("mcp: {source}: the server {raw_name:?} is skipped — {why}");
        let Some(name) = server_name(raw_name) else {
            skipped.push(skip("its name is empty"));
            continue;
        };
        if defs.iter().any(|known| known.name == name) {
            skipped.push(skip(&format!(
                "its name is {name:?} once made safe for a tool name, which another server has"
            )));
            continue;
        }
        let Some(entry) = entry.as_object() else {
            skipped.push(skip("it is not an object"));
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str);
        if !matches!(kind, None | Some("stdio")) || entry.contains_key("url") {
            skipped.push(skip(
                "only stdio servers (`command`) run; http and sse are not supported",
            ));
            continue;
        }
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
        else {
            skipped.push(skip("it has no `command`"));
            continue;
        };
        let args = match string_list(entry.get("args")) {
            Ok(args) => args,
            Err(()) => {
                skipped.push(skip("`args` is not a list of strings"));
                continue;
            }
        };
        let env = match string_map(entry.get("env")) {
            Ok(env) => env,
            Err(()) => {
                skipped.push(skip("`env` is not an object of strings"));
                continue;
            }
        };
        defs.push(McpServerDef {
            name,
            command: command.to_owned(),
            args,
            env,
            cwd: entry
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|cwd| !cwd.is_empty()),
            disabled: entry
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            timeout_secs: entry
                .get("timeout")
                .and_then(Value::as_u64)
                .filter(|secs| *secs > 0),
            approval: None,
            source: source.to_owned(),
        });
    }
    Ok((defs, skipped))
}

fn string_list(value: Option<&Value>) -> Result<Vec<String>, ()> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or(())?
        .iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or(()))
        .collect()
}

fn string_map(value: Option<&Value>) -> Result<BTreeMap<String, String>, ()> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    value
        .as_object()
        .ok_or(())?
        .iter()
        .map(|(key, item)| {
            item.as_str()
                .map(|text| (key.clone(), text.to_owned()))
                .ok_or(())
        })
        .collect()
}

/// `mcp.<server>.approval` from the user's own settings layers, which a
/// repository's `.titi/config.yml` cannot reach. A word that is not a tier
/// is an error the surface shows; the server then stays at the default.
pub fn approval_setting(settings: &Settings, server: &str) -> Result<Option<McpApproval>, String> {
    let key = format!("mcp.{server}.approval");
    match settings.get_user(&key) {
        None => Ok(None),
        Some(Value::String(word)) => McpApproval::parse(&word)
            .map(Some)
            .ok_or_else(|| format!("{key} is {word:?}; use read, write or exec")),
        Some(other) => Err(format!("{key} is {other}; use read, write or exec")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"mcpServers": {
        "files": {"command": "npx", "args": ["-y", "some-server"], "env": {"ROOT": "."},
                  "cwd": "tools", "timeout": 30},
        "off": {"command": "x", "disabled": true},
        "web": {"type": "http", "url": "https://example.invalid/mcp"},
        "bad env": {"command": "x", "env": {"A": 1}},
        "no command": {"args": []},
        "a.b": {"command": "y"}
    }}"#;

    fn def(command: &str, args: &[&str]) -> McpServerDef {
        McpServerDef {
            name: "s".to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: BTreeMap::new(),
            cwd: None,
            disabled: false,
            timeout_secs: None,
            approval: None,
            source: "t".to_owned(),
        }
    }

    #[test]
    fn an_mcp_servers_map_yields_its_stdio_servers_and_names_what_it_skips() {
        let (defs, skipped) = parse(SAMPLE, "mcp.json").unwrap();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a_b", "files", "off"], "{defs:?}");
        let files = &defs[1];
        assert_eq!(files.command, "npx");
        assert_eq!(files.args, ["-y", "some-server"]);
        assert_eq!(files.env["ROOT"], ".");
        assert_eq!(files.cwd.as_deref(), Some("tools"));
        assert_eq!(files.timeout_secs, Some(30));
        assert!(defs[2].disabled);
        let skipped = skipped.join("\n");
        assert!(skipped.contains("\"web\""), "{skipped}");
        assert!(skipped.contains("only stdio"), "{skipped}");
        assert!(skipped.contains("\"bad env\""), "{skipped}");
        assert!(skipped.contains("\"no command\""), "{skipped}");
    }

    #[test]
    fn a_file_without_servers_is_empty_and_bad_json_is_an_error() {
        assert_eq!(parse("{}", "t").unwrap(), (Vec::new(), Vec::new()));
        assert!(parse("{nope", "t").is_err());
        assert!(parse(r#"{"mcpServers": []}"#, "t").is_err());
    }

    #[test]
    fn a_name_is_made_safe_for_a_tool_name_and_bounded() {
        assert_eq!(server_name("my server!").as_deref(), Some("my_server_"));
        assert_eq!(server_name("  ").as_deref(), None);
        assert_eq!(server_name(&"x".repeat(100)).unwrap().len(), MAX_NAME_LEN);
    }

    #[test]
    fn the_hash_follows_what_runs_and_not_how_it_was_spelled_or_where_it_was_read() {
        let repo = tempfile::tempdir().unwrap();
        let base = def("npx", &["-y", "x"]);
        let mut same = base.clone();
        same.source = "other file".to_owned();
        same.timeout_secs = Some(5);
        same.disabled = true;
        assert_eq!(base.hash(repo.path()), same.hash(repo.path()));

        for changed in [
            McpServerDef {
                command: "npm".to_owned(),
                ..base.clone()
            },
            McpServerDef {
                args: vec!["-y".to_owned(), "y".to_owned()],
                ..base.clone()
            },
            McpServerDef {
                cwd: Some("sub".to_owned()),
                ..base.clone()
            },
            McpServerDef {
                name: "t".to_owned(),
                ..base.clone()
            },
            McpServerDef {
                env: BTreeMap::from([("NODE_OPTIONS".to_owned(), "--require ./x".to_owned())]),
                ..base.clone()
            },
        ] {
            assert_ne!(base.hash(repo.path()), changed.hash(repo.path()));
        }
        // A new value under an existing key is as much an edit as a new key.
        let mut one = base.clone();
        one.env.insert("A".to_owned(), "1".to_owned());
        let mut two = base.clone();
        two.env.insert("A".to_owned(), "2".to_owned());
        assert_ne!(one.hash(repo.path()), two.hash(repo.path()));
    }

    #[test]
    fn a_repository_file_the_server_runs_is_part_of_what_is_approved() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("tools")).unwrap();
        std::fs::write(repo.path().join("tools/server.js"), "console.log(1)\n").unwrap();
        for server in [
            def("node", &["tools/server.js"]),
            def("./tools/server.js", &[]),
            McpServerDef {
                cwd: Some("tools".to_owned()),
                ..def("node", &["./server.js"])
            },
        ] {
            let script = server.script(repo.path()).unwrap_or_else(|| {
                panic!("{}", server.command_line());
            });
            assert!(script.sha256.is_some());
            let before = server.hash(repo.path());
            std::fs::write(repo.path().join("tools/server.js"), "evil()\n").unwrap();
            assert_ne!(before, server.hash(repo.path()), "an edited script is new");
            std::fs::write(repo.path().join("tools/server.js"), "console.log(1)\n").unwrap();
        }
        assert_eq!(def("npx", &["-y", "x"]).script(repo.path()), None);
    }

    #[test]
    fn user_servers_stand_and_project_servers_are_kept_apart() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            agent.path().join(USER_FILE),
            r#"{"mcpServers": {"mine": {"command": "a"}, "clash": {"command": "own"}}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo.path().join(".titi")).unwrap();
        std::fs::write(
            repo.path().join(".titi/mcp.json"),
            r#"{"mcpServers": {"theirs": {"command": "b"}, "clash": {"command": "evil"},
                "dup": {"command": "d"}, "idle": {"command": "i", "disabled": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join(".mcp.json"),
            r#"{"mcpServers": {"dup": {"command": "d"}, "other": {"command": "o"}}}"#,
        )
        .unwrap();
        let found = discover(agent.path(), repo.path());
        let user: Vec<&str> = found.user.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(user, ["clash", "mine"]);
        let project: Vec<&str> = found.project.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(project, ["dup", "theirs", "other"], "{:?}", found.notices);
        assert!(
            found
                .notices
                .iter()
                .any(|line| line.contains("\"clash\"") && line.contains("already in your own")),
            "a repository cannot shadow the user's server: {:?}",
            found.notices
        );
    }

    #[test]
    fn an_unreadable_or_invalid_file_leaves_a_notice() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(agent.path().join(USER_FILE), "{nope").unwrap();
        let found = discover(agent.path(), repo.path());
        assert!(found.user.is_empty());
        assert!(
            found.notices[0].contains("mcp.json is not read"),
            "{found:?}"
        );
    }

    #[test]
    fn the_approval_comes_from_the_users_own_settings_only() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".titi")).unwrap();
        std::fs::write(
            repo.path().join(".titi/config.yml"),
            "mcp:\n  files:\n    approval: read\n",
        )
        .unwrap();
        let settings = Settings::load(agent.path(), repo.path(), &[]).unwrap();
        assert_eq!(approval_setting(&settings, "files"), Ok(None));

        std::fs::write(
            agent.path().join("config.yml"),
            "mcp:\n  files:\n    approval: write\n  odd:\n    approval: allow\n",
        )
        .unwrap();
        let settings = Settings::load(agent.path(), repo.path(), &[]).unwrap();
        assert_eq!(
            approval_setting(&settings, "files"),
            Ok(Some(McpApproval::Write))
        );
        assert!(approval_setting(&settings, "odd").is_err());
        assert_eq!(approval_setting(&settings, "unset"), Ok(None));
    }
}
