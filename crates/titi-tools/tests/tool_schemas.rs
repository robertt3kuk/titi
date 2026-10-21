//! The schemas the model is sent: every parameter a tool takes is still there,
//! and every one that carries a description keeps it.
//!
//! The schemas ride in every request, so their wording is trimmed for size;
//! this is the guard against trimming a parameter, or the sentence that says
//! what it means, out of existence. A parameter listed `false` is one whose
//! name and type say it all (`path`, `command`, `url`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use titi_tools::settings::SettingsBackend;
use titi_tools::{SettingsTool, TodoTool, ToolRegistry, workspace_tools};

struct NoSettings;

impl SettingsBackend for NoSettings {
    fn resolve_source(&self, _key: &str) -> Result<Option<(String, Value)>, String> {
        Ok(None)
    }
    fn set(&self, _key: &str, _value: Value, _scope: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Every tool `titi` registers from this crate, `memory` and `agent` (other
/// crates) aside.
fn registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    for tool in workspace_tools(".") {
        tools.register(Arc::from(tool));
    }
    tools.register(Arc::new(TodoTool::new()));
    tools.register(Arc::new(SettingsTool::new(Arc::new(NoSettings))));
    tools
}

/// `parameter -> whether it has a non-empty description`, nested properties
/// (a list's items) named `outer.items.inner`.
fn documented(schema: &Value) -> BTreeMap<String, bool> {
    fn walk(prefix: &str, schema: &Value, out: &mut BTreeMap<String, bool>) {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return;
        };
        for (name, property) in properties {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            let described = property
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty());
            out.insert(path.clone(), described);
            if let Some(items) = property.get("items") {
                walk(&format!("{path}.items"), items, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk("", schema, &mut out);
    out
}

#[test]
fn every_tool_keeps_every_parameter_and_its_description() {
    let expected: [(&str, &[(&str, bool)]); 16] = [
        (
            "ask",
            &[
                ("free_text", true),
                ("multi", true),
                ("options", true),
                ("question", true),
            ],
        ),
        (
            "bash",
            &[("command", false), ("pty", true), ("timeout_secs", true)],
        ),
        ("diagnose", &[]),
        (
            "edit",
            &[
                ("new_string", false),
                ("old_string", false),
                ("path", false),
                ("replace_all", true),
            ],
        ),
        ("fetch", &[("url", false)]),
        ("git", &[("op", true), ("path", true), ("staged", true)]),
        ("git_commit", &[("message", false), ("paths", true)]),
        ("glob", &[("pattern", true)]),
        (
            "grep",
            &[
                ("glob", true),
                ("ignore_case", true),
                ("path", true),
                ("pattern", true),
            ],
        ),
        (
            "hashline_edit",
            &[
                ("anchors", false),
                ("path", false),
                ("replacement", false),
                ("start_line", false),
            ],
        ),
        (
            "project",
            &[("action", false), ("filter", true), ("path", true)],
        ),
        (
            "read",
            &[("limit", true), ("offset", true), ("path", false)],
        ),
        (
            "settings",
            &[("key", true), ("scope", true), ("value", true)],
        ),
        (
            "todo",
            &[
                ("id", true),
                ("items", true),
                ("items.items.content", true),
                ("items.items.status", true),
                ("op", true),
                ("status", true),
            ],
        ),
        ("web_search", &[("query", false)]),
        ("write", &[("content", false), ("path", false)]),
    ];
    let specs = registry().specs();
    let mut seen = Vec::new();
    for (tool, parameters) in expected {
        let spec = specs
            .iter()
            .find(|spec| spec.name == *tool)
            .unwrap_or_else(|| panic!("tool {tool} is no longer registered"));
        assert!(
            !spec.description.trim().is_empty(),
            "{tool} lost its description"
        );
        let want: BTreeMap<String, bool> = parameters
            .iter()
            .map(|(name, described)| ((*name).to_owned(), *described))
            .collect();
        assert_eq!(documented(&spec.parameters), want, "{tool}");
        seen.push(spec.name.clone());
    }
    for spec in &specs {
        assert!(
            seen.contains(&spec.name),
            "{} has no row here: add its parameters",
            spec.name
        );
    }
}

/// The three tools whose one-line descriptions used to leave the model to
/// find out by failing: what `bash` cannot do and how far it goes, that
/// `write` replaces a whole file, and what `git` will and will not run.
#[test]
fn bash_write_and_git_say_what_the_model_needs_to_know() {
    let specs = registry().specs();
    let description = |name: &str| {
        specs
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("{name} is not registered"))
            .description
            .to_string()
    };
    let bash = description("bash");
    for needle in [
        "300 s",
        "60 s",
        "background",
        "No TTY unless `pty`",
        "no input",
        "Prefer read, grep, glob, edit and project",
        "40,000 characters",
        "head and tail",
    ] {
        assert!(
            bash.contains(needle),
            "bash does not say {needle:?}: {bash}"
        );
    }
    let write = description("write");
    for needle in ["replaces", "whole file", "`edit`", "parent directories"] {
        assert!(
            write.contains(needle),
            "write does not say {needle:?}: {write}"
        );
    }
    let git = description("git");
    for needle in ["read-only", "git_commit", "never pushes", "bash"] {
        assert!(git.contains(needle), "git does not say {needle:?}: {git}");
    }
}

/// Bytes of name, description and parameter schema, summed over the tools
/// above. `tool_schemas` ride in every request and fill the context window
/// from the first turn, so growth here is a decision, not a drift: raise the
/// ceiling in the commit that adds the words, and say why.
#[test]
fn the_schemas_stay_within_their_byte_budget() {
    // 7_600 + 862: the bash, write and git descriptions (7_526 -> 8_388 bytes).
    const CEILING: usize = 8_462;
    let total: usize = registry()
        .specs()
        .iter()
        .map(|spec| spec.name.len() + spec.description.len() + spec.parameters.to_string().len())
        .sum();
    assert!(
        total <= CEILING,
        "tool schemas are {total} bytes, over the {CEILING} budget"
    );
}
