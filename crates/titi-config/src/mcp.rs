//! MCP server definitions, read from the files other agents read.
//!
//! A server is a program the engine starts and speaks to over its standard
//! streams, or (`"type": "http"`) a URL it POSTs to. The file shape is the ecosystem's `mcpServers` map, so the setup
//! instructions a server ships with work here without being converted:
//!
//! ```json
//! {"mcpServers": {"files": {"command": "npx", "args": ["-y", "some-server"],
//!   "env": {"ROOT": "."}, "cwd": "tools", "timeout": 30},
//!   "docs": {"type": "http", "url": "https://mcp.example.invalid/mcp",
//!   "headers": {"Authorization": "Bearer ${DOCS_TOKEN}"}}}}
//! ```
//!
//! `${VAR}` in an HTTP server's `url` and header values is read from the
//! environment when the server connects, and only for a server of the user's
//! own file ([`expand_env`]): a repository's entry is sent literally, so it
//! cannot name a variable and have titi put its value on the wire.
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

/// The longest a `timeout` may ask for. The same bound applies to the
/// start-up handshake and to every call, so a definition cannot stretch them
/// to a day.
pub const MAX_TIMEOUT_SECS: u64 = 300;

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

/// One server, as a file defined it: a stdio program (`command`) or, when
/// `url` is set, a Streamable HTTP endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDef {
    /// The key in `mcpServers`, made safe to be part of a tool name.
    pub name: String,
    /// Empty for an HTTP server.
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
    /// Read from the repository, not from the user's own file. The manager
    /// holds such a server to the definition (and the repository files it
    /// runs) that was approved: a restart from changed files is refused.
    /// Set by [`discover`], never read from a file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project: bool,
    /// The endpoint of an HTTP server, as the file spelled it: `${VAR}` is
    /// still in it (see [`expand_env`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra request headers of an HTTP server, as the file spelled them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl McpServerDef {
    /// The bound on a request, in seconds: what the entry asked for, at most
    /// [`MAX_TIMEOUT_SECS`].
    pub fn timeout(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS)
    }

    /// What the definition says that decides what runs, and only that: the
    /// input of its hash. `disabled` and the file it came from change nothing
    /// about the code that starts; the timeout is part of it, because it
    /// decides how long a start-up and every call may stall the session. The environment is hashed
    /// by value as well as by key: `NODE_OPTIONS`, `PATH` and `LD_PRELOAD`
    /// are all one edit away from running something else.
    pub fn definition(&self) -> Value {
        if let Some(url) = &self.url {
            // Names and literal values, as written: never what `${VAR}`
            // expands to, which is a secret the file does not hold.
            let headers: BTreeMap<String, &String> = self
                .headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value))
                .collect();
            return json!({
                "name": self.name,
                "type": "http",
                "url": url,
                "headers": headers,
                "timeout": self.timeout(),
            });
        }
        json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "env": self.env,
            "cwd": self.cwd.as_deref().unwrap_or(""),
            "timeout": self.timeout(),
        })
    }

    /// What approving this server in `workspace` approves. A server that runs
    /// a file of the repository (`./bin/server`, `node tools/server.js`) is
    /// only as trustworthy as that file, so its content is part of the hash,
    /// as it is for a hook.
    pub fn hash(&self, workspace: &Path) -> String {
        let mut definition = self.definition();
        crate::hooks::add_scripts(&mut definition, &self.scripts(workspace));
        crate::trust::hash_definition(&definition)
    }

    /// Every repository file this server's command line names: a launcher
    /// runs more than its first argument (`deno run -A tools/mcp.ts`).
    pub fn scripts(&self, workspace: &Path) -> Vec<crate::hooks::Script> {
        if self.is_http() {
            return Vec::new();
        }
        let base = match self.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => workspace.join(cwd),
            _ => workspace.to_owned(),
        };
        let words: Vec<String> = std::iter::once(self.command.clone())
            .chain(self.args.iter().cloned())
            .collect();
        crate::hooks::scripts_of(&words, workspace, &base)
    }

    /// Whether the server starts code the hash cannot see: a launcher
    /// (`npx`, `uv`, an interpreter) none of whose arguments is a repository
    /// file. What it pulls in is whatever its name resolves to, and the
    /// consent panel says so.
    pub fn runs_unhashed_code(&self, workspace: &Path) -> bool {
        !self.is_http()
            && crate::hooks::is_launcher(&self.command)
            && self.scripts(workspace).is_empty()
    }

    /// Whether this is a Streamable HTTP server rather than a program.
    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }

    /// `host` or `host:port` of an HTTP server, for a panel.
    pub fn http_host(&self) -> Option<String> {
        let url = self.url.as_deref()?;
        let rest = url.split_once("://")?.1;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        Some(rest[..end].to_owned())
    }

    /// The URL of an HTTP server without its query and fragment, which may
    /// hold a key: what a transcript shows.
    pub fn display_url(&self) -> Option<String> {
        let url = self.url.as_deref()?;
        let cut = url.find(['?', '#']);
        Some(match cut {
            Some(at) => format!("{}?…", &url[..at]),
            None => url.to_owned(),
        })
    }

    /// Why a repository may not name this HTTP server, if it may not: the
    /// URL must be `https`, or `http` on the loopback interface, so a
    /// repository cannot point titi at a plain-text remote or smuggle
    /// credentials into the URL. `None` for a stdio server and for a URL
    /// that passes.
    pub fn project_url_problem(&self) -> Option<String> {
        let url = self.url.as_deref()?;
        let target = match parse_http_url(url) {
            Ok(target) => target,
            Err(reason) => return Some(reason),
        };
        if !target.https && !is_loopback_host(&target.host) {
            return Some(
                "a repository's server must use https (http is for localhost only)".to_owned(),
            );
        }
        None
    }

    /// The command line as a person would type it, for a panel. For an HTTP
    /// server, its URL.
    pub fn command_line(&self) -> String {
        if let Some(url) = &self.url {
            return url.clone();
        }
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Where an HTTP server's URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTarget {
    pub https: bool,
    /// Host name or IP literal, without brackets or port.
    pub host: String,
}

/// Reads the scheme and host of an `http://` or `https://` URL. Strict on
/// purpose: it is what a repository's URL must pass, so it refuses user
/// information (`http://127.0.0.1@evil.example/`), whitespace and control
/// characters, a host with anything but letters, digits, `.` and `-` (or an
/// IP literal), and a port that is not a number.
pub fn parse_http_url(url: &str) -> Result<HttpTarget, String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the url holds whitespace or a control character".to_owned());
    }
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err("the url must start with https:// or http://".to_owned());
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err("the url holds user information (user:password@)".to_owned());
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let Some((host, after)) = inner.split_once(']') else {
            return Err("the url has an unterminated [ip6] host".to_owned());
        };
        (host, after.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if let Some(port) = port
        && (port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("the url's port is not a number".to_owned());
    }
    let is_ip = host.parse::<std::net::IpAddr>().is_ok();
    let is_name = !host.is_empty()
        && !authority.starts_with('[')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !is_ip && !is_name {
        return Err("the url's host is not a host name or IP address".to_owned());
    }
    Ok(HttpTarget {
        https,
        host: host.to_ascii_lowercase(),
    })
}

/// `localhost` and the loopback addresses: where plain `http` is allowed for
/// a repository's server.
pub fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Replaces each `${NAME}` in `text` with what `lookup` gives for it. A name
/// nothing defines, or a `${` that is never closed, is an `Err`: a header
/// that silently lost its token would reach the server as a different
/// request. Only a user's own server goes through this.
pub fn expand_env(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find('}') else {
            return Err("a `${` is never closed".to_owned());
        };
        let name = &after[..end];
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(format!("`${{{name}}}` is not a variable name"));
        }
        match lookup(name) {
            Some(value) => out.push_str(&value),
            None => return Err(format!("the variable {name} is not set")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Whether `name` may be an HTTP header name (an RFC 9110 token).
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
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
    for mut def in project {
        def.project = true;
        if def.disabled {
            continue;
        }
        if let Some(problem) = def.project_url_problem() {
            found.notices.push(format!(
                "mcp: {}: the server {:?} is skipped — {problem}",
                def.source, def.name
            ));
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
/// `-`, anything else turned into `_`, runs of `_` collapsed to one and none
/// left at either end, at most [`MAX_NAME_LEN`] characters. `__` is how
/// `mcp__<server>__<tool>` is divided, so a name that held it would make two
/// servers' tools indistinguishable. `None` for a name with nothing left.
pub fn server_name(raw: &str) -> Option<String> {
    let mut name = String::new();
    for c in raw.trim().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            c
        } else {
            '_'
        };
        if c == '_' && (name.is_empty() || name.ends_with('_')) {
            continue;
        }
        name.push(c);
    }
    name.truncate(MAX_NAME_LEN);
    let name = name.trim_end_matches('_');
    (!name.is_empty()).then(|| name.to_owned())
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
        let http = matches!(kind, Some("http" | "streamable-http"));
        if !http && (!matches!(kind, None | Some("stdio")) || entry.contains_key("url")) {
            skipped.push(skip(
                "only stdio (`command`) and `\"type\": \"http\"` servers run; sse and ws are not supported",
            ));
            continue;
        }
        if http {
            match http_entry(entry, name, source) {
                Ok(def) => defs.push(def),
                Err(why) => skipped.push(skip(&why)),
            }
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
                .filter(|secs| *secs > 0)
                .map(|secs| secs.min(MAX_TIMEOUT_SECS)),
            approval: None,
            source: source.to_owned(),
            project: false,
            url: None,
            headers: BTreeMap::new(),
        });
    }
    Ok((defs, skipped))
}

/// One `"type": "http"` entry. The URL is checked only for its scheme here:
/// a user's may hold `${VAR}`, and a repository's is held to the stricter rule
/// by [`discover`].
fn http_entry(
    entry: &serde_json::Map<String, Value>,
    name: String,
    source: &str,
) -> Result<McpServerDef, String> {
    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or("it has no `url`")?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("its `url` must start with https:// or http://".to_owned());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("its `url` holds whitespace or a control character".to_owned());
    }
    if entry.contains_key("command") {
        return Err("it has both `url` and `command`".to_owned());
    }
    let headers = string_map(entry.get("headers"))
        .map_err(|()| "`headers` is not an object of strings".to_owned())?;
    for (header, value) in &headers {
        if !is_header_name(header) {
            return Err(format!("the header name {header:?} is not valid"));
        }
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(format!(
                "the value of header {header:?} holds a control character"
            ));
        }
    }
    Ok(McpServerDef {
        name,
        url: Some(url.to_owned()),
        headers,
        disabled: entry
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout_secs: entry
            .get("timeout")
            .and_then(Value::as_u64)
            .filter(|secs| *secs > 0)
            .map(|secs| secs.min(MAX_TIMEOUT_SECS)),
        source: source.to_owned(),
        ..McpServerDef::default()
    })
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
        "web": {"type": "sse", "url": "https://example.invalid/sse"},
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
            project: false,
            ..McpServerDef::default()
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
        assert!(
            skipped.contains("sse and ws are not supported"),
            "{skipped}"
        );
        assert!(skipped.contains("\"bad env\""), "{skipped}");
        assert!(skipped.contains("\"no command\""), "{skipped}");
    }

    #[test]
    fn two_names_that_collapse_to_one_are_not_both_servers() {
        let (defs, skipped) = parse(
            r#"{"mcpServers": {"gh_x": {"command": "a"}, "gh__x": {"command": "b"}}}"#,
            "t",
        )
        .unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "gh_x");
        assert!(skipped.join("\n").contains("once made safe"), "{skipped:?}");
    }

    #[test]
    fn a_file_without_servers_is_empty_and_bad_json_is_an_error() {
        assert_eq!(parse("{}", "t").unwrap(), (Vec::new(), Vec::new()));
        assert!(parse("{nope", "t").is_err());
        assert!(parse(r#"{"mcpServers": []}"#, "t").is_err());
    }

    #[test]
    fn a_name_is_made_safe_for_a_tool_name_and_bounded() {
        assert_eq!(server_name("my server!").as_deref(), Some("my_server"));
        // `__` divides a tool name into server and tool; no server name holds it.
        assert_eq!(server_name("gh__x").as_deref(), Some("gh_x"));
        assert_eq!(server_name("__a___b__").as_deref(), Some("a_b"));
        assert_eq!(server_name("___").as_deref(), None);
        let long = format!("{}__tail", "a".repeat(MAX_NAME_LEN - 1));
        assert!(!server_name(&long).unwrap().ends_with('_'));
        assert_eq!(server_name("  ").as_deref(), None);
        assert_eq!(server_name(&"x".repeat(100)).unwrap().len(), MAX_NAME_LEN);
    }

    #[test]
    fn the_hash_follows_what_runs_and_not_how_it_was_spelled_or_where_it_was_read() {
        let repo = tempfile::tempdir().unwrap();
        let base = def("npx", &["-y", "x"]);
        let mut same = base.clone();
        same.source = "other file".to_owned();
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
                timeout_secs: Some(5),
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
    fn the_timeout_is_capped_and_is_part_of_what_is_approved() {
        let repo = tempfile::tempdir().unwrap();
        let (defs, _) = parse(
            r#"{"mcpServers": {"a": {"command": "x", "timeout": 86400}, "b": {"command": "x"}}}"#,
            "t",
        )
        .unwrap();
        assert_eq!(defs[0].timeout_secs, Some(MAX_TIMEOUT_SECS));
        assert_eq!(defs[0].timeout(), MAX_TIMEOUT_SECS);
        assert_eq!(defs[1].timeout(), DEFAULT_TIMEOUT_SECS);
        // A definition that did not come through `parse` is bounded too.
        let mut raw = def("x", &[]);
        raw.timeout_secs = Some(999_999);
        assert_eq!(raw.timeout(), MAX_TIMEOUT_SECS);
        // Raising the timeout after approval asks again.
        let mut raised = def("x", &[]);
        raised.timeout_secs = Some(120);
        assert_ne!(def("x", &[]).hash(repo.path()), raised.hash(repo.path()));
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
            let script = server
                .scripts(repo.path())
                .into_iter()
                .next()
                .unwrap_or_else(|| {
                    panic!("{}", server.command_line());
                });
            assert!(script.sha256.is_some());
            let before = server.hash(repo.path());
            std::fs::write(repo.path().join("tools/server.js"), "evil()\n").unwrap();
            assert_ne!(before, server.hash(repo.path()), "an edited script is new");
            std::fs::write(repo.path().join("tools/server.js"), "console.log(1)\n").unwrap();
        }
        assert!(def("npx", &["-y", "x"]).scripts(repo.path()).is_empty());
    }

    #[test]
    fn every_repository_file_a_launcher_runs_is_hashed_not_only_the_first_word() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("tools")).unwrap();
        for file in [
            "tools/mcp.ts",
            "tools/helper.ts",
            "s.py",
            "server.py",
            "preload.js",
            "s.js",
        ] {
            std::fs::write(repo.path().join(file), "v1\n").unwrap();
        }
        for (command, args) in [
            ("deno", vec!["run", "-A", "tools/mcp.ts"]),
            ("bun", vec!["run", "tools/mcp.ts"]),
            ("python3", vec!["-X", "dev", "s.py"]),
            ("node", vec!["--import", "./preload.js", "s.js"]),
            ("node", vec!["--require=./preload.js", "s.js"]),
            ("uv", vec!["run", "server.py"]),
            ("npx", vec!["tsx", "tools/mcp.ts"]),
            (
                "deno",
                vec!["run", "tools/mcp.ts", "--config", "tools/helper.ts"],
            ),
        ] {
            let server = def(command, &args);
            let scripts = server.scripts(repo.path());
            assert!(!scripts.is_empty(), "{command} {args:?}: nothing hashed");
            assert!(
                scripts.iter().all(|script| script.sha256.is_some()),
                "{command} {args:?}: `run` or a flag was taken for a missing script: {scripts:?}"
            );
            let before = server.hash(repo.path());
            for script in &scripts {
                std::fs::write(repo.path().join(&script.path), "evil\n").unwrap();
                assert_ne!(
                    before,
                    server.hash(repo.path()),
                    "{command} {args:?}: {:?}",
                    script.path
                );
                std::fs::write(repo.path().join(&script.path), "v1\n").unwrap();
            }
            assert_eq!(before, server.hash(repo.path()));
        }
        // Two files, both hashed.
        let two = def(
            "deno",
            &["run", "tools/mcp.ts", "--config", "tools/helper.ts"],
        );
        assert_eq!(two.scripts(repo.path()).len(), 2);
    }

    #[test]
    fn a_launcher_that_names_no_repository_file_is_flagged_as_unhashed() {
        let repo = tempfile::tempdir().unwrap();
        for server in [
            def("npx", &["-y", "some-server"]),
            def("uvx", &["tool"]),
            def("deno", &["run", "https://example.invalid/x.ts"]),
            def("python3", &["-m", "server"]),
        ] {
            assert!(
                server.runs_unhashed_code(repo.path()),
                "{}",
                server.command_line()
            );
        }
        std::fs::write(repo.path().join("s.py"), "x").unwrap();
        assert!(!def("python3", &["s.py"]).runs_unhashed_code(repo.path()));
        // A plain program on PATH is not a launcher of repository code.
        assert!(!def("my-server", &["--stdio"]).runs_unhashed_code(repo.path()));
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
        assert!(found.project.iter().all(|def| def.project));
        assert!(found.user.iter().all(|def| !def.project));
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

    fn lookup(name: &str) -> Option<String> {
        (name == "TOKEN").then(|| "s3cret".to_owned())
    }

    fn http(url: &str, headers: &[(&str, &str)]) -> McpServerDef {
        McpServerDef {
            name: "h".to_owned(),
            url: Some(url.to_owned()),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            source: "t".to_owned(),
            ..McpServerDef::default()
        }
    }

    #[test]
    fn an_http_entry_reads_type_url_and_headers_and_accepts_both_spellings() {
        let (defs, skipped) = parse(
            r#"{"mcpServers": {
                "a": {"type": "http", "url": "https://x.invalid/mcp", "headers": {"X-Key": "${TOKEN}"}, "timeout": 20},
                "b": {"type": "streamable-http", "url": "http://127.0.0.1:9/mcp"},
                "c": {"type": "http"},
                "d": {"type": "http", "url": "ftp://x.invalid/"},
                "e": {"type": "http", "url": "https://x.invalid/", "headers": {"X": 1}},
                "f": {"type": "http", "url": "https://x.invalid/", "headers": {"bad name": "v"}},
                "g": {"type": "http", "url": "https://x.invalid/", "command": "x"},
                "h": {"url": "https://x.invalid/"}
            }}"#,
            "mcp.json",
        )
        .unwrap();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a", "b"], "{skipped:?}");
        assert!(defs[0].is_http());
        assert_eq!(defs[0].headers["X-Key"], "${TOKEN}", "kept as written");
        assert_eq!(defs[0].timeout_secs, Some(20));
        assert!(defs[0].command.is_empty());
        assert_eq!(skipped.len(), 6, "{skipped:?}");
    }

    #[test]
    fn env_is_expanded_only_where_asked_and_never_silently_empty() {
        assert_eq!(
            expand_env("Bearer ${TOKEN}!", &lookup).unwrap(),
            "Bearer s3cret!"
        );
        assert_eq!(expand_env("no vars", &lookup).unwrap(), "no vars");
        assert!(expand_env("${NOPE}", &lookup).unwrap_err().contains("NOPE"));
        assert!(expand_env("${TOKEN", &lookup).is_err());
        assert!(expand_env("${a b}", &lookup).is_err());
        // The value is not scanned again: a secret that holds `${X}` stays text.
        let nested = |name: &str| (name == "A").then(|| "${B}".to_owned());
        assert_eq!(expand_env("${A}", &nested).unwrap(), "${B}");
    }

    #[test]
    fn a_repository_url_is_https_or_http_on_loopback_and_holds_no_credentials() {
        let problem = |url: &str| http(url, &[]).project_url_problem();
        assert_eq!(problem("https://mcp.example.invalid/mcp"), None);
        assert_eq!(problem("http://127.0.0.1:8080/mcp"), None);
        assert_eq!(problem("http://localhost/mcp"), None);
        assert_eq!(problem("http://[::1]:3000/mcp"), None);
        assert!(
            problem("http://example.invalid/mcp")
                .unwrap()
                .contains("https")
        );
        assert!(problem("http://127.0.0.1.evil.invalid/").is_some());
        assert!(
            problem("http://127.0.0.1@evil.invalid/")
                .unwrap()
                .contains("user information")
        );
        assert!(problem("https://user:pw@example.invalid/").is_some());
        assert!(problem("https://${SECRET}.example.invalid/").is_some());
        assert!(problem("https://example.invalid:80a/").is_some());
        assert!(problem("https://exa mple.invalid/").is_some());
        assert!(
            problem("http://10.0.0.5/mcp").is_some(),
            "private is not loopback"
        );
        // A path or query that names a variable is only text.
        assert_eq!(
            problem("https://example.invalid/${SECRET}?k=${SECRET}"),
            None
        );
    }

    #[test]
    fn discover_skips_a_repository_url_that_breaks_the_rule_but_keeps_the_users_own() {
        let agent = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            agent.path().join(USER_FILE),
            r#"{"mcpServers": {"mine": {"type": "http", "url": "http://remote.invalid/mcp"}}}"#,
        )
        .unwrap();
        std::fs::write(
            repo.path().join(".mcp.json"),
            r#"{"mcpServers": {
                "bad": {"type": "http", "url": "http://remote.invalid/mcp"},
                "good": {"type": "http", "url": "https://remote.invalid/mcp"},
                "local": {"type": "http", "url": "http://127.0.0.1:1/mcp"}
            }}"#,
        )
        .unwrap();
        let found = discover(agent.path(), repo.path());
        assert_eq!(
            found.user.len(),
            1,
            "the user's own http url is theirs to choose"
        );
        let names: Vec<&str> = found.project.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["good", "local"]);
        assert!(found.project.iter().all(|d| d.project));
        let notes = found.notices.join("\n");
        assert!(
            notes.contains("\"bad\"") && notes.contains("https"),
            "{notes}"
        );
    }

    #[test]
    fn the_http_hash_covers_type_url_and_header_text_and_not_what_a_variable_expands_to() {
        let repo = tempfile::tempdir().unwrap();
        let base = http(
            "https://x.invalid/mcp",
            &[("Authorization", "Bearer ${TOKEN}")],
        );
        let hash = base.hash(repo.path());
        // Header names are case-insensitive; the file it came from is not part of it.
        let mut same = http(
            "https://x.invalid/mcp",
            &[("authorization", "Bearer ${TOKEN}")],
        );
        same.source = "elsewhere".to_owned();
        assert_eq!(hash, same.hash(repo.path()));
        for changed in [
            http(
                "https://y.invalid/mcp",
                &[("Authorization", "Bearer ${TOKEN}")],
            ),
            http(
                "https://x.invalid/mcp",
                &[("Authorization", "Bearer ${OTHER}")],
            ),
            http("https://x.invalid/mcp", &[("X-Other", "Bearer ${TOKEN}")]),
            http("https://x.invalid/mcp", &[]),
            McpServerDef {
                timeout_secs: Some(5),
                ..base.clone()
            },
        ] {
            assert_ne!(hash, changed.hash(repo.path()), "{changed:?}");
        }
        // A stdio server and an http one never collide.
        assert_ne!(hash, def("x", &[]).hash(repo.path()));
        // Whatever the variable holds is nowhere in what is hashed.
        let text = base.definition().to_string();
        assert!(text.contains("${TOKEN}") && !text.contains("s3cret"));
        assert!(!base.runs_unhashed_code(repo.path()));
        assert!(base.scripts(repo.path()).is_empty());
    }

    #[test]
    fn a_panel_shows_the_host_and_never_a_query() {
        let d = http("https://mcp.example.invalid:8443/v1/mcp?key=abc", &[]);
        assert_eq!(d.http_host().as_deref(), Some("mcp.example.invalid:8443"));
        assert_eq!(
            d.display_url().as_deref(),
            Some("https://mcp.example.invalid:8443/v1/mcp?…")
        );
        assert_eq!(def("npx", &[]).http_host(), None);
    }
}
