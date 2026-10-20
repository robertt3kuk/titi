//! Keybinding registry and matching (contract: `omp://keybindings`).
//!
//! Mirrors omp's `packages/tui/src/keybindings.ts` + the config loading and
//! legacy-name migration from `packages/coding-agent/src/config/keybindings.ts`:
//! a definitions table (action id → default keys) with per-action user
//! overrides, conflict detection, alias expansion, and YAML config files
//! compatible with `~/.omp/agent/keybindings.yml` (action id → key or list of
//! keys; empty list disables the action).

use std::collections::{HashMap, HashSet};

use crate::keys::{add_key_aliases, canonical_key_id, parse_key};

/// A keybinding definition: the action's default keys and a human description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub default_keys: &'static [&'static str],
    pub description: &'static str,
}

/// A key conflict: one key claimed by two or more user bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

/// The engine-level keybinding table: editor, input and selection actions.
/// `app.*` actions are declared by the CLI layer via [`KeybindingsManager::new`].
pub const TUI_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    (
        "tui.editor.cursorUp",
        KeybindingDefinition {
            default_keys: &["up"],
            description: "Move cursor up",
        },
    ),
    (
        "tui.editor.cursorDown",
        KeybindingDefinition {
            default_keys: &["down"],
            description: "Move cursor down",
        },
    ),
    (
        "tui.editor.cursorLeft",
        KeybindingDefinition {
            default_keys: &["left", "ctrl+b"],
            description: "Move cursor left",
        },
    ),
    (
        "tui.editor.cursorRight",
        KeybindingDefinition {
            default_keys: &["right", "ctrl+f"],
            description: "Move cursor right",
        },
    ),
    (
        "tui.editor.cursorWordLeft",
        KeybindingDefinition {
            default_keys: &["alt+left", "ctrl+left", "alt+b"],
            description: "Move cursor word left",
        },
    ),
    (
        "tui.editor.cursorWordRight",
        KeybindingDefinition {
            default_keys: &["alt+right", "ctrl+right", "alt+f"],
            description: "Move cursor word right",
        },
    ),
    (
        "tui.editor.cursorLineStart",
        KeybindingDefinition {
            default_keys: &["home", "ctrl+a"],
            description: "Move to line start",
        },
    ),
    (
        "tui.editor.cursorLineEnd",
        KeybindingDefinition {
            default_keys: &["end", "ctrl+e"],
            description: "Move to line end",
        },
    ),
    (
        "tui.editor.jumpForward",
        KeybindingDefinition {
            default_keys: &["ctrl+]"],
            description: "Jump forward to character",
        },
    ),
    (
        "tui.editor.jumpBackward",
        KeybindingDefinition {
            default_keys: &["ctrl+alt+]"],
            description: "Jump backward to character",
        },
    ),
    (
        "tui.editor.pageUp",
        KeybindingDefinition {
            default_keys: &["pageup"],
            description: "Page up",
        },
    ),
    (
        "tui.editor.pageDown",
        KeybindingDefinition {
            default_keys: &["pagedown"],
            description: "Page down",
        },
    ),
    (
        "tui.editor.deleteCharBackward",
        KeybindingDefinition {
            default_keys: &["backspace"],
            description: "Delete character backward",
        },
    ),
    (
        "tui.editor.deleteCharForward",
        KeybindingDefinition {
            default_keys: &["delete", "ctrl+d"],
            description: "Delete character forward",
        },
    ),
    (
        "tui.editor.deleteWordBackward",
        KeybindingDefinition {
            default_keys: &[
                "ctrl+w",
                "alt+backspace",
                "ctrl+backspace",
                "super+alt+backspace",
            ],
            description: "Delete word backward",
        },
    ),
    (
        "tui.editor.deleteWordForward",
        KeybindingDefinition {
            default_keys: &["alt+delete", "alt+d", "super+alt+delete", "super+alt+d"],
            description: "Delete word forward",
        },
    ),
    (
        "tui.editor.deleteToLineStart",
        KeybindingDefinition {
            default_keys: &["ctrl+u"],
            description: "Delete to line start",
        },
    ),
    (
        "tui.editor.deleteToLineEnd",
        KeybindingDefinition {
            default_keys: &["ctrl+k"],
            description: "Open the command palette on an empty draft; otherwise delete to end",
        },
    ),
    (
        "tui.editor.yank",
        KeybindingDefinition {
            default_keys: &["ctrl+y"],
            description: "Yank",
        },
    ),
    (
        "tui.editor.yankPop",
        KeybindingDefinition {
            default_keys: &["alt+y"],
            description: "Yank pop",
        },
    ),
    (
        "tui.editor.undo",
        KeybindingDefinition {
            default_keys: &["ctrl+-", "ctrl+_"],
            description: "Undo",
        },
    ),
    (
        "tui.editor.spellingSuggestions",
        KeybindingDefinition {
            default_keys: &["ctrl+."],
            description: "Show spelling replacements",
        },
    ),
    (
        "tui.input.newLine",
        KeybindingDefinition {
            default_keys: &["shift+enter", "ctrl+j"],
            description: "Insert newline",
        },
    ),
    (
        "tui.input.submit",
        KeybindingDefinition {
            default_keys: &["enter"],
            description: "Submit input",
        },
    ),
    (
        "tui.input.tab",
        KeybindingDefinition {
            default_keys: &["tab"],
            description: "Tab / autocomplete",
        },
    ),
    (
        "tui.input.copy",
        KeybindingDefinition {
            default_keys: &["ctrl+c"],
            description: "Copy selection",
        },
    ),
    (
        "tui.select.up",
        KeybindingDefinition {
            default_keys: &["up"],
            description: "Move selection up",
        },
    ),
    (
        "tui.select.down",
        KeybindingDefinition {
            default_keys: &["down"],
            description: "Move selection down",
        },
    ),
    (
        "tui.select.pageUp",
        KeybindingDefinition {
            default_keys: &["pageup"],
            description: "Selection page up",
        },
    ),
    (
        "tui.select.pageDown",
        KeybindingDefinition {
            default_keys: &["pagedown"],
            description: "Selection page down",
        },
    ),
    (
        "tui.select.confirm",
        KeybindingDefinition {
            default_keys: &["enter"],
            description: "Confirm selection",
        },
    ),
    (
        "tui.select.cancel",
        KeybindingDefinition {
            default_keys: &["escape", "ctrl+c"],
            description: "Cancel selection",
        },
    ),
];

/// App-level actions from `omp://keybindings.md`. Combined with
/// [`TUI_KEYBINDINGS`] by [`default_manager`].
pub const APP_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    (
        "app.interrupt",
        KeybindingDefinition {
            default_keys: &["ctrl+c"],
            description: "Interrupt / exit",
        },
    ),
    (
        "app.model.cycleForward",
        KeybindingDefinition {
            default_keys: &["ctrl+p"],
            description: "Cycle role models forward",
        },
    ),
    (
        "app.model.cycleBackward",
        KeybindingDefinition {
            default_keys: &["ctrl+shift+p"],
            description: "Cycle role models backward",
        },
    ),
    (
        "app.model.selectTemporary",
        KeybindingDefinition {
            default_keys: &["alt+p"],
            description: "Pick a model temporarily",
        },
    ),
    (
        "app.model.select",
        KeybindingDefinition {
            default_keys: &["alt+m"],
            description: "Open the model selector",
        },
    ),
    (
        "app.plan.toggle",
        KeybindingDefinition {
            default_keys: &["alt+shift+p"],
            description: "Toggle plan mode",
        },
    ),
    (
        "app.history.search",
        KeybindingDefinition {
            default_keys: &["ctrl+r"],
            description: "Search prompt history",
        },
    ),
    (
        "app.tools.expand",
        KeybindingDefinition {
            default_keys: &["ctrl+o"],
            description: "Toggle tool-output expansion",
        },
    ),
    (
        "app.tools.toggleVisibility",
        KeybindingDefinition {
            default_keys: &["ctrl+shift+o"],
            description: "Show or hide tool activity",
        },
    ),
    (
        "app.thinking.toggle",
        KeybindingDefinition {
            default_keys: &["ctrl+t"],
            description: "Toggle thinking-block visibility",
        },
    ),
    (
        "app.thinking.cycle",
        KeybindingDefinition {
            default_keys: &["shift+tab"],
            description: "Cycle the session mode: agent, plan, duck",
        },
    ),
    (
        "app.editor.external",
        KeybindingDefinition {
            default_keys: &["ctrl+g"],
            description: "Edit the draft in $VISUAL / $EDITOR",
        },
    ),
    (
        "app.message.followUp",
        KeybindingDefinition {
            default_keys: &["ctrl+q", "ctrl+enter"],
            description: "Queue a follow-up message",
        },
    ),
    (
        "app.message.dequeue",
        KeybindingDefinition {
            default_keys: &["alt+up", "shift+up"],
            description: "Dequeue a queued message back into the editor",
        },
    ),
    (
        "app.retry",
        KeybindingDefinition {
            default_keys: &["alt+r"],
            description: "Retry the last failed assistant turn",
        },
    ),
    (
        "app.display.reset",
        KeybindingDefinition {
            default_keys: &["alt+l"],
            description: "Reset terminal display",
        },
    ),
    (
        "app.clipboard.copyLine",
        KeybindingDefinition {
            default_keys: &["alt+shift+l"],
            description: "Copy the current line",
        },
    ),
    (
        "app.clipboard.copyPrompt",
        KeybindingDefinition {
            default_keys: &["alt+shift+c"],
            description: "Copy the whole prompt",
        },
    ),
    (
        "app.clipboard.pasteTextRaw",
        KeybindingDefinition {
            default_keys: &["ctrl+shift+v", "alt+shift+v"],
            description: "Paste clipboard text without collapsing",
        },
    ),
    (
        "app.clipboard.pasteImage",
        KeybindingDefinition {
            default_keys: &["ctrl+v"],
            description: "Paste from the clipboard (image preferred)",
        },
    ),
    (
        "app.stt.toggle",
        KeybindingDefinition {
            default_keys: &[],
            description: "Toggle speech-to-text (default gesture: hold Space)",
        },
    ),
    (
        "app.live.toggle",
        KeybindingDefinition {
            default_keys: &["ctrl+l"],
            description: "Start or stop live voice mode",
        },
    ),
    (
        "app.agents.hub",
        KeybindingDefinition {
            default_keys: &["alt+a"],
            description: "Open the Agent Hub",
        },
    ),
    (
        "app.session.observe",
        KeybindingDefinition {
            default_keys: &["ctrl+s"],
            description: "Open the Agent Hub",
        },
    ),
    (
        "app.session.switch",
        KeybindingDefinition {
            default_keys: &["ctrl+x"],
            description: "Open the session switcher",
        },
    ),
    (
        "app.details.toggleAll",
        KeybindingDefinition {
            // The reference TUI's "expand or collapse all code and reasoning
            // blocks", and inside the recap it opens every section.
            default_keys: &["ctrl+o"],
            description: "Expand or collapse every block",
        },
    ),
];

/// Build a manager with TUI editor bindings plus OMP `app.*` defaults.
pub fn default_manager(user_bindings: KeybindingsConfig) -> KeybindingsManager {
    let mut defs: Vec<(&str, KeybindingDefinition)> =
        Vec::with_capacity(TUI_KEYBINDINGS.len() + APP_KEYBINDINGS.len());
    defs.extend(TUI_KEYBINDINGS.iter().copied());
    defs.extend(APP_KEYBINDINGS.iter().copied());
    KeybindingsManager::new(&defs, user_bindings)
}

/// User config value for one action: one key, a list of keys, or none
/// (`None` = action disabled).
pub type KeybindingsConfig = HashMap<String, Vec<String>>;

fn normalize_keys(keys: &[&str]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for key in keys {
        let normalized = key.to_ascii_lowercase();
        if seen.insert(normalized.clone()) {
            result.push(normalized);
        }
    }
    result
}

/// The keybinding manager: definitions + user overrides, matching, conflicts.
///
/// Mirrors omp's `KeybindingsManager`. `definitions` is the static action
/// table (TUI actions, or extended with `app.*` by the CLI layer);
/// `user_bindings` overrides per-action keys (`None` value disables the
/// action). Matching parses raw input once and does set lookup per probe
/// via [`KeybindingsManager::matches_canonical`].
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: HashMap<String, KeybindingDefinition>,
    user_bindings: KeybindingsConfig,
    keys_by_id: HashMap<String, Vec<String>>,
    match_keys_by_id: HashMap<String, HashSet<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        Self::new(TUI_KEYBINDINGS, KeybindingsConfig::new())
    }
}

impl KeybindingsManager {
    /// Build a manager from a definitions table and user bindings.
    pub fn new(
        definitions: &[(&str, KeybindingDefinition)],
        user_bindings: KeybindingsConfig,
    ) -> Self {
        let mut defs = HashMap::new();
        for (id, definition) in definitions {
            defs.insert((*id).to_owned(), definition.clone());
        }
        let mut manager = Self {
            definitions: defs,
            user_bindings,
            keys_by_id: HashMap::new(),
            match_keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.match_keys_by_id.clear();
        self.conflicts = Vec::new();

        // Conflicts: a key claimed by 2+ user bindings.
        let mut user_claims: HashMap<String, Vec<String>> = HashMap::new();
        for (keybinding, keys) in &self.user_bindings {
            if !self.definitions.contains_key(keybinding) {
                continue;
            }
            for key in keys {
                user_claims
                    .entry(key.clone())
                    .or_default()
                    .push(keybinding.clone());
            }
        }
        for (key, mut keybindings) in user_claims {
            if keybindings.len() > 1 {
                keybindings.sort();
                self.conflicts.push(KeybindingConflict { key, keybindings });
            }
        }
        self.conflicts.sort_by(|a, b| a.key.cmp(&b.key));

        for (id, definition) in &self.definitions {
            let user_keys = self.user_bindings.get(id);
            let keys = match user_keys {
                Some(keys) => keys.clone(), // empty vec = disabled
                None => normalize_keys(definition.default_keys),
            };
            self.keys_by_id.insert(id.clone(), keys);
            let mut match_keys = HashSet::new();
            if let Some(keys) = self.keys_by_id.get(id) {
                for key in keys {
                    add_key_aliases(&mut match_keys, key);
                }
            }
            self.match_keys_by_id.insert(id.clone(), match_keys);
        }
    }

    /// Match raw terminal input against a keybinding action id.
    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        let Some(parsed) = parse_key(data) else {
            return false;
        };
        self.matches_canonical(&canonical_key_id(&parsed), keybinding)
    }

    /// Set-lookup match for hot paths: caller parses once, probes many actions.
    pub fn matches_canonical(&self, canonical: &str, keybinding: &str) -> bool {
        let Some(keys) = self.match_keys_by_id.get(keybinding) else {
            return false;
        };
        keys.contains(canonical)
    }

    /// The effective key list for an action (user override or defaults).
    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.keys_by_id.get(keybinding).cloned().unwrap_or_default()
    }

    /// The definition of an action, if declared.
    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(keybinding)
    }

    /// User-binding conflicts: keys claimed by more than one action.
    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    /// Replace user bindings and rebuild the match tables.
    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// The current user bindings.
    pub fn get_user_bindings(&self) -> &KeybindingsConfig {
        &self.user_bindings
    }

    /// Every declared action id.
    pub fn actions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.definitions.keys().cloned().collect();
        ids.sort();
        ids
    }
}

// ---------------------------------------------------------------------------
// Config loading: YAML/JSON with legacy name migration
// ---------------------------------------------------------------------------

/// Legacy action names → namespaced ids (omp `KEYBINDING_NAME_MIGRATIONS`).
pub const KEYBINDING_NAME_MIGRATIONS: &[(&str, &str)] = &[
    // App-specific (old names)
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("displayReset", "app.display.reset"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("selectModelTemporary", "app.model.selectTemporary"),
    ("togglePlanMode", "app.plan.toggle"),
    ("historySearch", "app.history.search"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("retry", "app.retry"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("pasteTextRaw", "app.clipboard.pasteTextRaw"),
    ("copyLine", "app.clipboard.copyLine"),
    ("copyPrompt", "app.clipboard.copyPrompt"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("observeSessions", "app.session.observe"),
    ("toggleSTT", "app.stt.toggle"),
    // TUI editor (old names for backward compatibility)
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    // TUI input (old names for backward compatibility)
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    // TUI select (old names for backward compatibility)
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    // Upstream additional migrations
    ("toggleSessionNamedFilter", "app.session.togglePath"),
];

/// Migrate legacy action names to namespaced ids. Returns whether any key
/// changed (so callers know a write-back is needed).
pub fn migrate_keybinding_names(config: &mut KeybindingsConfig) -> bool {
    let mut migrated = false;
    let mut remapped = HashMap::new();
    for (old, new) in KEYBINDING_NAME_MIGRATIONS {
        if let Some(value) = config.remove(*old) {
            remapped.entry((*new).to_owned()).or_insert(value);
            migrated = true;
        }
    }
    config.extend(remapped);
    migrated
}

fn yaml_value_to_keys(value: &serde_yaml::Value) -> Option<Vec<String>> {
    match value {
        serde_yaml::Value::String(s) => Some(vec![s.clone()]),
        serde_yaml::Value::Sequence(items) => {
            let mut keys = Vec::new();
            for item in items {
                if let serde_yaml::Value::String(s) = item {
                    keys.push(s.clone());
                } else {
                    return None;
                }
            }
            Some(keys)
        }
        // Explicit null / empty disables? omp: undefined → defaults; empty
        // array disables. Null maps to defaults (no override).
        _ => None,
    }
}

/// Parse a `keybindings.yml`-style config document into user bindings.
/// The document maps action id → key | list of keys | empty list (disable).
pub fn parse_keybindings_config(raw: &str) -> KeybindingsConfig {
    let value: serde_yaml::Value = serde_yaml::from_str(raw).unwrap_or(serde_yaml::Value::Null);
    let mut config = KeybindingsConfig::new();
    if let serde_yaml::Value::Mapping(map) = value {
        for (key, val) in map {
            let serde_yaml::Value::String(action) = key else {
                continue;
            };
            if let Some(keys) = yaml_value_to_keys(&val) {
                config.insert(action, keys);
            }
        }
    }
    config
}

/// Parse a legacy `keybindings.json` config document.
pub fn parse_keybindings_json(raw: &str) -> KeybindingsConfig {
    let value: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::Value::Null);
    let mut config = KeybindingsConfig::new();
    if let serde_json::Value::Object(map) = value {
        for (action, val) in map {
            match val {
                serde_json::Value::String(s) => {
                    config.insert(action, vec![s]);
                }
                serde_json::Value::Array(items) => {
                    let keys: Vec<String> = items
                        .into_iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                        .collect();
                    config.insert(action, keys);
                }
                serde_json::Value::Null => {
                    // Explicit null: no override (defaults apply).
                    config.insert(action, Vec::new());
                }
                _ => {}
            }
        }
    }
    config
}

/// Serialize user bindings back to a `keybindings.yml` document, ordered by
/// action id. Empty arrays disable an action.
pub fn serialize_keybindings_config(config: &KeybindingsConfig) -> String {
    let mut ids: Vec<&String> = config.keys().collect();
    ids.sort();
    let mut out = String::new();
    for id in ids {
        let keys = &config[id];
        if keys.len() == 1 {
            out.push_str(&format!("{id}: {}\n", keys[0]));
        } else {
            let list = keys
                .iter()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("{id}: [{list}]\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(user: KeybindingsConfig) -> KeybindingsManager {
        KeybindingsManager::new(TUI_KEYBINDINGS, user)
    }

    #[test]
    fn does_not_evict_selector_confirm_when_input_submit_rebound() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.input.submit".to_owned(),
            vec!["enter".to_owned(), "ctrl+enter".to_owned()],
        );
        let kb = manager(user);
        assert_eq!(kb.get_keys("tui.input.submit"), vec!["enter", "ctrl+enter"]);
        assert_eq!(kb.get_keys("tui.select.confirm"), vec!["enter"]);
    }

    #[test]
    fn does_not_evict_cursor_bindings_when_another_action_reuses_key() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.select.up".to_owned(),
            vec!["up".to_owned(), "ctrl+p".to_owned()],
        );
        let kb = manager(user);
        assert_eq!(kb.get_keys("tui.select.up"), vec!["up", "ctrl+p"]);
        assert_eq!(kb.get_keys("tui.editor.cursorUp"), vec!["up"]);
    }

    #[test]
    fn preserves_shift_when_matching_uppercase() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.copy".to_owned(), vec!["shift+a".to_owned()]);
        let kb = manager(user);
        assert!(kb.matches("A", "tui.input.copy"));
        assert!(!kb.matches("a", "tui.input.copy"));
    }

    #[test]
    fn reports_user_binding_conflicts_without_evicting_defaults() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.submit".to_owned(), vec!["ctrl+x".to_owned()]);
        user.insert("tui.select.confirm".to_owned(), vec!["ctrl+x".to_owned()]);
        let kb = manager(user);
        assert_eq!(
            kb.get_conflicts(),
            vec![KeybindingConflict {
                key: "ctrl+x".to_owned(),
                keybindings: vec![
                    "tui.input.submit".to_owned(),
                    "tui.select.confirm".to_owned()
                ],
            }]
        );
        assert_eq!(kb.get_keys("tui.editor.cursorLeft"), vec!["left", "ctrl+b"]);
    }

    #[test]
    fn ships_ctrl_j_alongside_shift_enter_as_newline_defaults() {
        let kb = manager(KeybindingsConfig::new());
        let keys = kb.get_keys("tui.input.newLine");
        assert!(keys.contains(&"ctrl+j".to_owned()));
        assert!(keys.contains(&"shift+enter".to_owned()));
    }

    #[test]
    fn matches_alias_helpers() {
        let mut aliases = HashSet::new();
        for key in ["esc", "return", "?", "shift+a"] {
            add_key_aliases(&mut aliases, key);
        }
        let mut sorted: Vec<String> = aliases.iter().cloned().collect();
        sorted.sort();
        assert_eq!(sorted, vec!["?", "enter", "escape", "shift+?", "shift+a"]);
        assert_eq!(canonical_key_id("A"), "shift+a");
        assert_eq!(canonical_key_id("shift+?"), "shift+?");

        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.input.copy".to_owned(),
            vec![
                "esc".to_owned(),
                "return".to_owned(),
                "?".to_owned(),
                "shift+a".to_owned(),
            ],
        );
        let kb = manager(user);
        for input in ["\x1b", "\r", "?", "A"] {
            let parsed = parse_key(input);
            let parsed = parsed.expect("input parses");
            assert!(aliases_contain(&aliases, &canonical_key_id(&parsed)));
            assert!(kb.matches(input, "tui.input.copy"));
        }
    }

    fn aliases_contain(aliases: &HashSet<String>, canonical: &str) -> bool {
        aliases.contains(canonical)
    }

    #[test]
    fn empty_array_disables_action() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.submit".to_owned(), Vec::new());
        let kb = manager(user);
        assert!(kb.get_keys("tui.input.submit").is_empty());
        assert!(!kb.matches("\r", "tui.input.submit"));
        // Other actions unaffected.
        assert!(kb.matches("\r", "tui.select.confirm"));
    }

    #[test]
    fn user_remap_changes_routing() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.submit".to_owned(), vec!["ctrl+x".to_owned()]);
        let kb = manager(user);
        assert!(kb.matches("\u{18}", "tui.input.submit")); // ctrl+x
        assert!(!kb.matches("\r", "tui.input.submit"));
    }

    #[test]
    fn yaml_round_trip_with_migration() {
        let yaml = "interrupt: ctrl+x\nsubmit: [enter, ctrl+enter]\ntui.input.copy: ctrl+shift+c\n";
        let mut config = parse_keybindings_config(yaml);
        assert_eq!(
            config.get("interrupt").map(|v| v.clone()),
            Some(vec!["ctrl+x".to_owned()])
        );
        assert_eq!(
            config.get("submit").map(|v| v.clone()),
            Some(vec!["enter".to_owned(), "ctrl+enter".to_owned()])
        );
        assert!(migrate_keybinding_names(&mut config));
        assert!(config.contains_key("app.interrupt"));
        assert!(!config.contains_key("interrupt"));
        assert!(config.contains_key("tui.input.submit"));
        assert!(config.contains_key("tui.input.copy"));

        let out = serialize_keybindings_config(&config);
        let reparsed = parse_keybindings_config(&out);
        assert_eq!(config, reparsed);
    }

    #[test]
    fn empty_yaml_list_disables_after_migration() {
        let yaml = "submit: []\n";
        let mut config = parse_keybindings_config(yaml);
        assert_eq!(config.get("submit").map(|v| v.clone()), Some(Vec::new()));
        migrate_keybinding_names(&mut config);
        let kb = manager(config);
        assert!(!kb.matches("\r", "tui.input.submit"));
    }

    #[test]
    fn legacy_json_migration() {
        let json = r#"{"interrupt": "ctrl+shift+c", "cursorUp": "up"}"#;
        let mut config = parse_keybindings_json(json);
        assert_eq!(
            config.get("interrupt").map(|v| v.clone()),
            Some(vec!["ctrl+shift+c".to_owned()])
        );
        assert_eq!(
            config.get("cursorUp").map(|v| v.clone()),
            Some(vec!["up".to_owned()])
        );
        assert!(migrate_keybinding_names(&mut config));
        assert!(config.contains_key("app.interrupt"));
        assert!(config.contains_key("tui.editor.cursorUp"));
    }

    #[test]
    fn unknown_actions_are_ignored_by_manager() {
        let mut user = KeybindingsConfig::new();
        user.insert("bogus.action".to_owned(), vec!["ctrl+x".to_owned()]);
        let kb = manager(user);
        assert!(kb.get_keys("bogus.action").is_empty());
        assert!(kb.get_conflicts().is_empty());
    }

    #[test]
    fn matches_via_matches_canonical_agree() {
        let kb = manager(KeybindingsConfig::new());
        assert!(kb.matches("\u{3}", "tui.input.copy")); // ctrl+c
        assert!(kb.matches_canonical("ctrl+c", "tui.input.copy"));
        assert!(!kb.matches_canonical("ctrl+x", "tui.input.copy"));
    }

    #[test]
    fn app_hub_and_observe_share_overlay() {
        let kb = default_manager(KeybindingsConfig::new());
        assert!(kb.matches_canonical("alt+a", "app.agents.hub"));
        assert!(kb.matches_canonical("ctrl+s", "app.session.observe"));
        assert!(kb.get_keys("app.stt.toggle").is_empty());
    }
}
