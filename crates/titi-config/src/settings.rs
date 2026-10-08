//! Layered settings: defaults <- global <- project <- overlays <- runtime.
//!
//! - Global: first present of `<agent_dir>/config.yml` / `config.yaml`; canonical write target.
//! - Project: `<project>/.titi/config.yml` (read-only through this API). A
//!   project may override settings, but the agent keeps its own state — sessions,
//!   memory, secrets — under `agent_dir`, never in the repository.
//! - Overlays: `$TITI_CONFIG_FILES` (path-list) then explicit paths; strict (missing/invalid = hard error).
//! - Runtime: in-memory overrides, never persisted.
//!
//! Invalid global/project YAML is quarantined to a `.broken-<ts>-<pid>` sibling
//! and surfaced as `SettingsError::Quarantined`.

use crate::config_file::with_file_lock;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("invalid global settings at {original}: {reason}; quarantined to {backup}")]
    Quarantined {
        original: PathBuf,
        backup: PathBuf,
        reason: String,
    },
    #[error("invalid project settings at {original}: {reason}; quarantined to {backup}")]
    ProjectQuarantined {
        original: PathBuf,
        backup: PathBuf,
        reason: String,
    },
    #[error("config overlay {path}: {reason}")]
    Overlay { path: PathBuf, reason: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("key '{key}': {reason}")]
    Key { key: String, reason: String },
}

#[derive(Debug, Default)]
pub struct Settings {
    /// Lowest layer. Always empty today: see [`NO_SETTINGS_NOTE`].
    pub defaults: Value,
    pub global: Value,
    pub project: Value,
    pub overlays: Vec<Value>,
    pub runtime: Value,
    pub global_path: PathBuf,
    pub project_path: PathBuf,
}

/// Canonical file names, in resolution order.
pub const GLOBAL_FILES: [&str; 2] = ["config.yml", "config.yaml"];
pub const PROJECT_SUBPATH: &str = ".titi/config.yml";

/// What a surface reports when [`Settings::is_empty`] holds.
///
/// The `defaults` layer is deliberately empty: this crate owns no table of
/// built-in values, because the real ones live in the binaries that consume
/// the settings — the engine config, the approval mode, the provider
/// registry. Copying them here would be a second source of truth free to
/// drift, and [`Settings::resolve_source`] would start crediting a "default"
/// layer for values the binaries may never use. Nothing is invented, so the
/// empty state is merely named the same way everywhere.
pub const NO_SETTINGS_NOTE: &str = "built-in defaults only";

/// The theme the user chose for a dark terminal, by preset name.
///
/// `theme.dark` and [`THEME_LIGHT_KEY`] are the two slots the appearance probe
/// picks between — the terminal reports a background, the slot for it is read,
/// and the crate's own pick (`titanium` dark, `light` light) stands when the
/// slot is unset. That is what `auto` means: nothing in these keys.
pub const THEME_DARK_KEY: &str = "theme.dark";

/// The theme the user chose for a light terminal. See [`THEME_DARK_KEY`].
pub const THEME_LIGHT_KEY: &str = "theme.light";

/// How many files the genome prompt map keeps.
///
/// Unset means the engine default (`EngineConfig::new` picks it, currently 24).
/// This crate does not own that number — the constant is a key name, not a
/// second source of the default, so it deliberately carries no value.
pub const GENOME_LIMIT_KEY: &str = "genome.limit";

/// Whether the prompt map is built.
///
/// Unset means on. This crate does not own the default boolean; what this
/// key owns is the name only. `TITI_NO_GENOME=1` forces off for one run and
/// is not this key.
pub const GENOME_ENABLED_KEY: &str = "genome.enabled";

/// Whether a turn that finished cleanly raises a desktop notification.
///
/// Unset means on. This crate owns the name only; the terminal channel the
/// notification takes is decided by the screen (`titi_tui::caps`).
pub const NOTIFY_COMPLETION_KEY: &str = "notify.completion";

/// Whether a turn that ended in a failure raises a desktop notification.
/// Unset means on. See [`NOTIFY_COMPLETION_KEY`].
pub const NOTIFY_ERROR_KEY: &str = "notify.error";

/// Whether a turn that stopped on an approval raises a desktop
/// notification. Unset means on. See [`NOTIFY_COMPLETION_KEY`].
pub const NOTIFY_ASK_KEY: &str = "notify.ask";

/// Whether the terminal shows its own progress for a running turn.
///
/// Unset means on. The terminal's own bar is raised while a turn is in
/// flight and cleared on every way out of it, including a failure; a
/// terminal that has no such bar is left quiet.
pub const TERMINAL_PROGRESS_KEY: &str = "terminal.progress";

/// Whether the working row shows a generation-rate estimate.
///
/// Unset means on. The rate is read from the characters the row already
/// counts and is an estimate, not a provider count.
pub const COMPOSER_TOKEN_RATE_KEY: &str = "composer.tokenRate";

/// Whether a finished turn's footer names its wall time.
///
/// Unset means on. The footer is the dim row under the answer; this is the
/// `1.4s` it opens with. Off, the row starts at whatever is left of it, and a
/// row with nothing left is not drawn at all ([`DISPLAY_TURN_FOOTER_TOKENS_KEY`],
/// [`DISPLAY_TURN_FOOTER_CACHE_MISS_KEY`]).
pub const DISPLAY_TURN_FOOTER_TIME_KEY: &str = "display.turnFooter.time";

/// Whether a finished turn's footer names its token counts.
///
/// Unset means on. This covers the prompt, its cached share, the completion
/// count and the money that rides with them: a price is what those tokens
/// cost, and `$0.004` on its own is a figure without its subject.
pub const DISPLAY_TURN_FOOTER_TOKENS_KEY: &str = "display.turnFooter.tokens";

/// Whether a finished turn's footer marks a request that re-paid for its own
/// history.
///
/// Unset means on. The marker only exists on a turn that had a cache miss, so
/// turning it off says nothing about a warm one.
pub const DISPLAY_TURN_FOOTER_CACHE_MISS_KEY: &str = "display.turnFooter.cacheMiss";

/// Which pre-built status line the screen paints.
///
/// The names are `default` (today's line, the fallback for anything this key
/// does not name), `minimal`, `compact`, `full` and `ascii`. Unset, unknown or
/// unreadable means `default`: a cosmetic key must never change what the screen
/// does more than it says, and must never refuse to start. This crate owns the
/// name only; the segments behind each preset live in
/// `titi_tui::status_bar::PRESETS`.
pub const STATUS_LINE_PRESET_KEY: &str = "statusLine.preset";

/// How the line between the status line's groups reflects context usage.
///
/// `off` (the fallback, and what an unset key means), `percentage` or
/// `embedded`. The gauge needs the model's context window, which only a turn
/// reports; until one does, every mode paints the blank gap this key was added
/// over. See [`STATUS_LINE_PRESET_KEY`]: this crate owns the name only.
pub const STATUS_LINE_CONTEXT_LINE_KEY: &str = "statusLine.contextLine";

/// Whether a launch resumes the newest stored session.
///
/// Unset means off: without this key (and without `--continue`) a launch
/// starts blank even when the agent directory holds sessions, so a resumed
/// conversation is always something the user asked for. A truthy value is
/// `true`, `on` or `yes`, case-insensitively; anything else — including a
/// typo — is off, because a key that cannot be read must not change what the
/// screen does. This crate owns the name only; the listing the resume picks
/// from is `titi_cli::session_fs`.
pub const SESSION_AUTO_RESUME_KEY: &str = "session.autoResume";

/// How long a `bash` call may hold a turn before it is handed to the
/// background, in milliseconds: `bash.autoBackground.thresholdMs`.
///
/// Unset means the engine default; this crate owns the name only, as with
/// [`GENOME_LIMIT_KEY`], so it carries no number. A value must be an integer
/// from 1 to [`MAX_AUTO_BACKGROUND_MS`]; 0 or anything larger is refused by
/// [`Settings::auto_background_threshold`] rather than silently guessed.
/// `TITI_BASH_BACKGROUND_MS` overrides this key when set.
pub const BASH_AUTO_BACKGROUND_KEY: &str = "bash.autoBackground.thresholdMs";

/// Largest `bash.autoBackground.thresholdMs` this build accepts: one hour.
/// Past that the foreground wait is not a bound at all, so a longer number is
/// a typo (a seconds value in a milliseconds slot) and is refused.
pub const MAX_AUTO_BACKGROUND_MS: u64 = 3_600_000;

impl Settings {
    /// Discover and load all layers.
    ///
    /// `extra_overlays` are appended after `$TITI_CONFIG_FILES` overlays
    /// (the `--config <file>` equivalents) and are resolved relative to
    /// `project_dir` with `~` expansion.
    pub fn load(
        agent_dir: &Path,
        project_dir: &Path,
        extra_overlays: &[PathBuf],
    ) -> Result<Self, SettingsError> {
        let existing_global = GLOBAL_FILES
            .iter()
            .map(|f| agent_dir.join(f))
            .find(|p| p.exists());
        let global_path = existing_global
            .clone()
            .unwrap_or_else(|| agent_dir.join("config.yml"));
        let global = match &existing_global {
            Some(path) => match read_yaml_value(path) {
                Ok(Some(v)) => v,
                Ok(None) => Value::Object(Map::new()),
                Err(reason) => {
                    let backup = quarantine(path)?;
                    return Err(SettingsError::Quarantined {
                        original: path.clone(),
                        backup,
                        reason,
                    });
                }
            },
            None => Value::Object(Map::new()),
        };

        let project = match project_dir.join(PROJECT_SUBPATH) {
            path if path.exists() => match read_yaml_value(&path) {
                Ok(Some(v)) => v,
                Ok(None) => Value::Object(Map::new()),
                Err(reason) => {
                    let backup = quarantine(&path)?;
                    return Err(SettingsError::ProjectQuarantined {
                        original: path,
                        backup,
                        reason,
                    });
                }
            },
            _ => Value::Object(Map::new()),
        };

        let mut overlays = Vec::new();
        let mut overlay_paths = env_overlay_paths();
        overlay_paths.extend(extra_overlays.iter().cloned());
        for raw in overlay_paths {
            let path = expand_tilde(&raw);
            let text = fs::read_to_string(&path).map_err(|e| SettingsError::Overlay {
                path: path.clone(),
                reason: if e.kind() == std::io::ErrorKind::NotFound {
                    "file not found".into()
                } else {
                    e.to_string()
                },
            })?;
            match Self::parse_overlay(&path, &text) {
                Ok(Some(v @ Value::Object(_))) => overlays.push(v),
                Ok(Some(_)) => {
                    return Err(SettingsError::Overlay {
                        path: path.clone(),
                        reason: "top-level document is not a mapping".into(),
                    });
                }
                Ok(None) => overlays.push(Value::Object(Map::new())),
                Err(e) => {
                    return Err(SettingsError::Overlay {
                        path: path.clone(),
                        reason: e,
                    });
                }
            }
        }

        Ok(Self {
            defaults: Value::Object(Map::new()),
            global,
            project,
            overlays,
            project_path: project_dir.join(PROJECT_SUBPATH),
            runtime: Value::Object(Map::new()),
            global_path,
        })
    }
    /// Parse an overlay document by extension: `.json`/`.jsonc` via JSON, else YAML.
    fn parse_overlay(path: &Path, text: &str) -> Result<Option<Value>, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let value = match ext {
            "json" | "jsonc" => {
                let stripped = crate::config_file::strip_jsonc_comments(text);
                serde_json::from_str::<Value>(&stripped).map_err(|e| e.to_string())?
            }
            _ => serde_yaml::from_str::<Value>(text).map_err(|e| e.to_string())?,
        };
        Ok(match value {
            Value::Null => None,
            v => Some(v),
        })
    }

    /// Effective value of a dotted key (`theme.dark`), highest layer wins.
    pub fn get(&self, key: &str) -> Option<Value> {
        let effective = self.effective();
        let mut current = &effective;
        for seg in key.split('.') {
            current = index(current, seg)?;
        }
        Some(current.clone())
    }

    /// Deep-merged effective view: defaults <- global <- project <- overlays <- runtime.
    pub fn effective(&self) -> Value {
        let mut merged = self.defaults.clone();
        for layer in std::iter::once(&self.global)
            .chain(std::iter::once(&self.project))
            .chain(self.overlays.iter())
            .chain(std::iter::once(&self.runtime))
        {
            deep_merge(&mut merged, layer);
        }
        merged
    }

    /// A dotted key from the user's own layers only: runtime, overlays, then
    /// global. The project file is skipped, so a cloned repo cannot loosen a
    /// setting that protects the user (privacy, approval).
    pub fn get_user(&self, key: &str) -> Option<Value> {
        std::iter::once(&self.runtime)
            .chain(self.overlays.iter().rev())
            .chain(std::iter::once(&self.global))
            .find_map(|layer| lookup(layer, key))
    }

    /// The `bash.autoBackground.thresholdMs` key as a duration.
    ///
    /// `Ok(None)` when the key is unset (the engine default, then, not zero).
    /// `Ok(Some(d))` when it holds an integer from 1 to
    /// [`MAX_AUTO_BACKGROUND_MS`]; anything else — a string, a float, 0, a
    /// number past the cap — is a [`SettingsError::Key`], so a typo is named
    /// rather than turned into a threshold nobody meant.
    pub fn auto_background_threshold(&self) -> Result<Option<Duration>, SettingsError> {
        let refuse = |reason: &str| SettingsError::Key {
            key: BASH_AUTO_BACKGROUND_KEY.to_owned(),
            reason: reason.to_owned(),
        };
        let Some(value) = self.get(BASH_AUTO_BACKGROUND_KEY) else {
            return Ok(None);
        };
        let millis = value.as_u64().ok_or_else(|| {
            refuse("expected an integer number of milliseconds from 1 to 3600000")
        })?;
        if millis == 0 {
            return Err(refuse("must be at least 1 ms; zero is not a threshold"));
        }
        if millis > MAX_AUTO_BACKGROUND_MS {
            return Err(refuse(
                "must be at most 3600000 ms (one hour); a larger value is refused",
            ));
        }
        Ok(Some(Duration::from_millis(millis)))
    }

    /// The key's value in every layer that sets it, lowest first (global,
    /// project, overlays, runtime), for settings that add up across layers.
    pub fn layer_values(&self, key: &str) -> Vec<Value> {
        std::iter::once(&self.global)
            .chain(std::iter::once(&self.project))
            .chain(self.overlays.iter())
            .chain(std::iter::once(&self.runtime))
            .filter_map(|layer| lookup(layer, key))
            .collect()
    }

    /// Returns the active value for a key and the name of the layer providing it.
    pub fn resolve_source(&self, key: &str) -> Option<(&'static str, Value)> {
        if let Some(v) = lookup(&self.runtime, key) {
            return Some(("runtime", v));
        }
        for overlay in self.overlays.iter().rev() {
            if let Some(v) = lookup(overlay, key) {
                return Some(("overlay", v));
            }
        }
        if let Some(v) = lookup(&self.project, key) {
            return Some(("project", v));
        }
        if let Some(v) = lookup(&self.global, key) {
            return Some(("agent", v));
        }
        if let Some(v) = lookup(&self.defaults, key) {
            return Some(("default", v));
        }
        None
    }

    /// Returns all effective keys with their source layer and value.
    ///
    /// Arrays are indexed rather than printed whole: a list of providers
    /// becomes `providers.0.id`, `providers.0.base_url`, `providers.1.id`,
    /// so every scalar carries the layer it came from. An empty array stays
    /// a leaf — there is nothing under it, and dropping the key would hide
    /// that a layer sets it.
    pub fn flatten(&self) -> std::collections::BTreeMap<String, (String, Value)> {
        fn child(prefix: &str, seg: &str) -> String {
            if prefix.is_empty() {
                seg.to_owned()
            } else {
                format!("{prefix}.{seg}")
            }
        }
        fn walk(value: &Value, prefix: &str, map: &mut std::collections::BTreeMap<String, Value>) {
            match value {
                Value::Object(obj) => {
                    for (k, v) in obj {
                        walk(v, &child(prefix, k), map);
                    }
                }
                Value::Array(items) if !items.is_empty() => {
                    for (at, item) in items.iter().enumerate() {
                        walk(item, &child(prefix, &at.to_string()), map);
                    }
                }
                _ => {
                    map.insert(prefix.to_owned(), value.clone());
                }
            }
        }
        let mut effective = std::collections::BTreeMap::new();
        walk(&self.effective(), "", &mut effective);

        let mut out = std::collections::BTreeMap::new();
        for (k, v) in effective {
            let source = self
                .resolve_source(&k)
                .map(|(s, _)| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            out.insert(k, (source, v));
        }
        out
    }

    /// Whether no layer sets a single key.
    ///
    /// True is neither an error nor a missing file: every binary still runs
    /// on the defaults compiled into it. Surfaces report it with
    /// [`NO_SETTINGS_NOTE`], so two commands cannot describe one state in
    /// two contradictory ways.
    pub fn is_empty(&self) -> bool {
        self.flatten().is_empty()
    }

    /// Write a dotted key into the **global** layer and persist it (the only
    /// persistent write path through this API).
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), SettingsError> {
        set_nested(&mut self.global, key, value).map_err(|reason| SettingsError::Key {
            key: key.into(),
            reason,
        })?;
        self.save_global()
    }

    pub fn set_project(&mut self, key: &str, value: Value) -> Result<(), SettingsError> {
        set_nested(&mut self.project, key, value).map_err(|reason| SettingsError::Key {
            key: key.into(),
            reason,
        })?;
        self.save_project()
    }

    /// Remove a dotted key from the global layer, restoring the next layer's
    /// (or default) value at read time.
    pub fn reset(&mut self, key: &str) -> Result<(), SettingsError> {
        remove_nested(&mut self.global, key).map_err(|reason| SettingsError::Key {
            key: key.into(),
            reason,
        })?;
        self.save_global()
    }

    pub fn reset_project(&mut self, key: &str) -> Result<(), SettingsError> {
        remove_nested(&mut self.project, key).map_err(|reason| SettingsError::Key {
            key: key.into(),
            reason,
        })?;
        self.save_project()
    }

    /// In-memory override for this process; never persisted.
    pub fn set_runtime(&mut self, key: &str, value: Value) -> Result<(), SettingsError> {
        set_nested(&mut self.runtime, key, value).map_err(|reason| SettingsError::Key {
            key: key.into(),
            reason,
        })
    }

    fn save_global(&self) -> Result<(), SettingsError> {
        if let Some(parent) = self.global_path.parent() {
            fs::create_dir_all(parent)?;
        }
        with_file_lock(&self.global_path, || {
            let tmp = self.global_path.with_extension("yml.tmp");
            let yaml = serde_yaml::to_string(&self.global).map_err(|e| {
                SettingsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            fs::write(&tmp, yaml)?;
            fs::rename(&tmp, &self.global_path)?;
            Ok(())
        })
    }

    fn save_project(&self) -> Result<(), SettingsError> {
        if let Some(parent) = self.project_path.parent() {
            fs::create_dir_all(parent)?;
        }
        with_file_lock(&self.project_path, || {
            let tmp = self.project_path.with_extension("yml.tmp");
            let yaml = serde_yaml::to_string(&self.project).map_err(|e| {
                SettingsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;
            fs::write(&tmp, yaml)?;
            fs::rename(&tmp, &self.project_path)?;
            Ok(())
        })
    }
}

fn lookup(layer: &Value, key: &str) -> Option<Value> {
    let mut current = layer;
    for seg in key.split('.') {
        current = index(current, seg)?;
    }
    Some(current.clone())
}

/// Whether an on-by-default switch is off at `key`.
///
/// Written for the cosmetic switches whose name this crate owns — the
/// notification, progress and rate keys — and modelled on the genome switch
/// the engine reads: unset means on, a JSON `false` means off, and the
/// strings `off`/`false`/`no` (case-insensitive) mean off. Anything else — a
/// number, a list, `maybe` — leaves the switch on, because a typo in a
/// cosmetic key must not change what the screen does.
///
/// Read through the effective view, so a project file can set these too:
/// unlike privacy and approval, none of them guards anything.
pub fn switch_off(settings: &Settings, key: &str) -> bool {
    match settings.get(key) {
        None => false,
        Some(Value::Bool(on)) => !on,
        Some(Value::String(text)) => {
            matches!(text.to_ascii_lowercase().as_str(), "off" | "false" | "no")
        }
        Some(_) => false,
    }
}

/// One dotted segment into a value: an object key, or a decimal position in
/// an array — the shape [`Settings::flatten`] gives array elements, so a
/// flattened key resolves back to the layer that set it.
fn index<'a>(value: &'a Value, seg: &str) -> Option<&'a Value> {
    match value {
        Value::Array(items) => items.get(seg.parse::<usize>().ok()?),
        _ => value.get(seg),
    }
}

/// Objects deep-merge; scalars and arrays are replaced wholesale.
pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

fn set_nested(tree: &mut Value, key: &str, value: Value) -> Result<(), String> {
    let mut current = match tree {
        Value::Object(map) => map,
        _ => return Err("root is not an object".into()),
    };
    let segments: Vec<&str> = key.split('.').collect();
    for (i, seg) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            current.insert((*seg).into(), value.clone());
            return Ok(());
        }
        let entry = current
            .entry((*seg).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        match entry {
            Value::Object(map) => current = map,
            _ => return Err(format!("'{seg}' is not a mapping")),
        }
    }
    Ok(())
}

fn remove_nested(tree: &mut Value, key: &str) -> Result<(), String> {
    let mut current = match tree {
        Value::Object(map) => map,
        _ => return Err("root is not an object".into()),
    };
    let segments: Vec<&str> = key.split('.').collect();
    for (i, seg) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            current.remove(*seg);
            return Ok(());
        }
        match current.get_mut(*seg) {
            Some(Value::Object(map)) => current = map,
            Some(_) => return Err(format!("'{seg}' is not a mapping")),
            None => return Ok(()), // nothing to remove
        }
    }
    Ok(())
}

fn read_yaml_value(path: &Path) -> Result<Option<Value>, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    match serde_yaml::from_str::<Value>(&text) {
        Ok(Value::Null) => Ok(None),
        Ok(v @ Value::Object(_)) => Ok(Some(v)),
        Ok(_) => Err("top-level document is not a mapping".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Move an invalid persistent settings file to a unique `.broken-<ts>-<pid>` sibling.
fn quarantine(path: &Path) -> Result<PathBuf, SettingsError> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut name = format!(".broken-{ts}-{}", std::process::id());
    if let Some(ext) = path.extension() {
        name.push('.');
        name.push_str(&ext.to_string_lossy());
    }
    let backup = path.with_file_name(name);
    fs::rename(path, &backup)?;
    Ok(backup)
}

fn env_overlay_paths() -> Vec<PathBuf> {
    match std::env::var("TITI_CONFIG_FILES") {
        Ok(list) if !list.trim().is_empty() => list
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn get_user_ignores_the_project_layer() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write(
            &agent.path().join("config.yml"),
            "privacy:\n  maskIps: true\n",
        );
        write(
            &project.path().join(PROJECT_SUBPATH),
            "privacy:\n  maskIps: false\n  allow: [\".env\"]\n",
        );
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        // The effective view still sees the project value...
        assert_eq!(settings.get("privacy.maskIps"), Some(Value::Bool(false)));
        // ...but a key the project must not loosen reads past it.
        assert_eq!(
            settings.get_user("privacy.maskIps"),
            Some(Value::Bool(true))
        );
        assert_eq!(settings.get_user("privacy.allow"), None);
    }

    #[test]
    fn get_user_prefers_runtime_then_global() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write(
            &agent.path().join("config.yml"),
            "privacy:\n  maskIps: false\n",
        );
        let mut settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert_eq!(
            settings.get_user("privacy.maskIps"),
            Some(Value::Bool(false))
        );
        settings
            .set_runtime("privacy.maskIps", Value::Bool(true))
            .unwrap();
        assert_eq!(
            settings.get_user("privacy.maskIps"),
            Some(Value::Bool(true))
        );
    }

    /// `bash.autoBackground.thresholdMs` reads as milliseconds, is refused
    /// when it is zero, past the cap, or not an integer, and is absent — not
    /// zero — when the key is unset.
    #[test]
    fn the_auto_background_threshold_is_read_and_refused_out_of_range() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let mut settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert_eq!(settings.auto_background_threshold().unwrap(), None);
        settings
            .set_runtime(BASH_AUTO_BACKGROUND_KEY, Value::from(250u64))
            .unwrap();
        assert_eq!(
            settings.auto_background_threshold().unwrap(),
            Some(Duration::from_millis(250))
        );
        for bad in [
            Value::from(0u64),
            Value::from(MAX_AUTO_BACKGROUND_MS + 1),
            Value::from(1.5f64),
            Value::String("soon".into()),
        ] {
            settings
                .set_runtime(BASH_AUTO_BACKGROUND_KEY, bad.clone())
                .unwrap();
            let error = settings.auto_background_threshold().unwrap_err();
            assert!(
                matches!(error, SettingsError::Key { .. }),
                "{bad}: {error:?}"
            );
        }
    }

    #[test]
    fn layer_values_lists_every_layer_that_sets_the_key() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write(
            &agent.path().join("config.yml"),
            "privacy:\n  sensitive: [\"a\"]\n",
        );
        write(
            &project.path().join(PROJECT_SUBPATH),
            "privacy:\n  sensitive: [\"b\"]\n",
        );
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        let values = settings.layer_values("privacy.sensitive");
        assert_eq!(values.len(), 2, "{values:?}");
    }

    /// A list of providers is a list of settings, not one opaque value: each
    /// scalar gets its own key and its own source layer.
    #[test]
    fn array_entries_flatten_into_indexed_keys() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write(
            &project.path().join(PROJECT_SUBPATH),
            "providers:\n  \
             - id: myco\n    \
             base_url: https://api.example.invalid/v1\n    \
             headers:\n      \
             x-team: platform\n",
        );
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        let flat = settings.flatten();
        assert!(!flat.contains_key("providers"), "{flat:?}");
        for (key, value) in [
            ("providers.0.id", "myco"),
            ("providers.0.base_url", "https://api.example.invalid/v1"),
            ("providers.0.headers.x-team", "platform"),
        ] {
            assert_eq!(
                flat.get(key),
                Some(&("project".to_owned(), Value::String(value.to_owned()))),
                "{key} in {flat:?}"
            );
        }
    }

    #[test]
    fn a_list_of_scalars_is_indexed_too_and_keeps_its_layer() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write(
            &agent.path().join("config.yml"),
            "privacy:\n  allow: [\".env\", \"id_rsa\"]\n  sensitive: []\n",
        );
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        let flat = settings.flatten();
        assert_eq!(
            flat.get("privacy.allow.0"),
            Some(&("agent".to_owned(), Value::String(".env".to_owned()))),
            "{flat:?}"
        );
        assert_eq!(
            flat.get("privacy.allow.1"),
            Some(&("agent".to_owned(), Value::String("id_rsa".to_owned()))),
            "{flat:?}"
        );
        // Nothing lives under an empty list, so it stays a leaf rather than
        // vanishing from the listing.
        assert_eq!(
            flat.get("privacy.sensitive"),
            Some(&("agent".to_owned(), Value::Array(Vec::new()))),
            "{flat:?}"
        );
    }

    /// The higher layer replaces an array wholesale, so its elements are the
    /// ones listed — and each is credited to that layer.
    #[test]
    fn a_replaced_array_is_credited_to_the_layer_that_won() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write(
            &agent.path().join("config.yml"),
            "providers:\n  - id: from-agent\n",
        );
        write(
            &project.path().join(PROJECT_SUBPATH),
            "providers:\n  - id: from-project\n",
        );
        let settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert_eq!(
            settings.flatten().get("providers.0.id"),
            Some(&(
                "project".to_owned(),
                Value::String("from-project".to_owned())
            ))
        );
        assert_eq!(
            settings.get("providers.0.id"),
            Some(Value::String("from-project".to_owned()))
        );
    }

    #[test]
    fn no_layer_setting_anything_is_reported_as_built_in_defaults() {
        let agent = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let mut settings = Settings::load(agent.path(), project.path(), &[]).unwrap();
        assert!(settings.is_empty());
        assert_eq!(NO_SETTINGS_NOTE, "built-in defaults only");
        settings
            .set_runtime("theme.dark", Value::Bool(true))
            .unwrap();
        assert!(!settings.is_empty());
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn deep_merges_objects_replaces_scalars_and_arrays() {
        let mut base = obj(&[
            (
                "tools",
                obj(&[
                    ("approvalMode", Value::from("write")),
                    (
                        "approval",
                        obj(&[
                            ("bash", Value::from("prompt")),
                            ("read", Value::from("allow")),
                        ]),
                    ),
                ]),
            ),
            ("providers", Value::Array(vec![Value::from("anthropic")])),
        ]);
        let over = obj(&[
            (
                "tools",
                obj(&[("approval", obj(&[("bash", Value::from("allow"))]))]),
            ),
            ("providers", Value::Array(vec![Value::from("groq")])),
        ]);
        deep_merge(&mut base, &over);
        assert_eq!(base["tools"]["approvalMode"], Value::from("write"));
        assert_eq!(base["tools"]["approval"]["bash"], Value::from("allow"));
        assert_eq!(base["tools"]["approval"]["read"], Value::from("allow"));
        assert_eq!(base["providers"], Value::Array(vec![Value::from("groq")]));
    }

    #[test]
    fn precedence_runtime_beats_project_beats_global() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        write(
            &agent.join("config.yml"),
            "tools:\n  approval:\n    bash: prompt\n    read: allow\n",
        );
        write(
            &tmp.path().join(PROJECT_SUBPATH),
            "tools:\n  approval:\n    bash: deny\n",
        );
        let mut s = Settings::load(&agent, tmp.path(), &[]).unwrap();
        assert_eq!(s.get("tools.approval.bash"), Some(Value::from("deny")));
        assert_eq!(s.get("tools.approval.read"), Some(Value::from("allow")));
        s.set_runtime("tools.approval.bash", Value::from("prompt"))
            .unwrap();
        assert_eq!(s.get("tools.approval.bash"), Some(Value::from("prompt")));
    }

    #[test]
    fn set_persists_to_global_file() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        let mut s = Settings::load(&agent, tmp.path(), &[]).unwrap();
        s.set("modelRoles.default", Value::from("anthropic/claude"))
            .unwrap();
        assert_eq!(
            s.get("modelRoles.default"),
            Some(Value::from("anthropic/claude"))
        );
        let on_disk = fs::read_to_string(agent.join("config.yml")).unwrap();
        assert!(on_disk.contains("anthropic/claude"));
        let reloaded = Settings::load(&agent, tmp.path(), &[]).unwrap();
        assert_eq!(
            reloaded.get("modelRoles.default"),
            Some(Value::from("anthropic/claude"))
        );
    }

    #[test]
    fn reset_falls_back_to_defaults() {
        // omp semantics: project layer overrides global, so a global-only key
        // is used here. After reset the key falls to schema defaults (none yet).
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        write(
            &agent.join("config.yml"),
            "compaction:\n  thresholdPercent: 80\n",
        );
        let mut s = Settings::load(&agent, tmp.path(), &[]).unwrap();
        assert_eq!(s.get("compaction.thresholdPercent"), Some(Value::from(80)));
        s.set("compaction.thresholdPercent", Value::from(70))
            .unwrap();
        assert_eq!(s.get("compaction.thresholdPercent"), Some(Value::from(70)));
        s.reset("compaction.thresholdPercent").unwrap();
        assert_eq!(s.get("compaction.thresholdPercent"), None);
    }

    #[test]
    fn project_overrides_global_write() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        write(
            &tmp.path().join(PROJECT_SUBPATH),
            "compaction:\n  thresholdPercent: 80\n",
        );
        let mut s = Settings::load(&agent, tmp.path(), &[]).unwrap();
        s.set("compaction.thresholdPercent", Value::from(70))
            .unwrap();
        assert_eq!(s.get("compaction.thresholdPercent"), Some(Value::from(80)));
    }

    #[test]
    fn broken_global_yaml_is_quarantined() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        write(&agent.join("config.yml"), "tools: [broken\n");
        let err = Settings::load(&agent, tmp.path(), &[]).unwrap_err();
        let SettingsError::Quarantined {
            original, backup, ..
        } = err
        else {
            panic!("expected Quarantined, got {err:?}");
        };
        assert!(!original.exists());
        assert!(backup.exists() && backup.to_string_lossy().contains(".broken-"));
    }

    #[test]
    fn overlay_missing_or_scalar_is_hard_error() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        let err = Settings::load(&agent, tmp.path(), &[PathBuf::from("nope.yml")]).unwrap_err();
        assert!(matches!(err, SettingsError::Overlay { .. }));
        let scalar = tmp.path().join("scalar.yml");
        write(&scalar, "just a string\n");
        let err = Settings::load(&agent, tmp.path(), &[scalar]).unwrap_err();
        assert!(matches!(err, SettingsError::Overlay { .. }));
    }

    #[test]
    fn jsonc_comments_load_in_overlays() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        let cfg = tmp.path().join("overlay.jsonc");
        write(
            &cfg,
            "{\n  // comment\n  \"theme\": { \"dark\": \"titanium\" } /* block */\n}\n",
        );
        let s = Settings::load(&agent, tmp.path(), &[cfg]).unwrap();
        assert_eq!(s.get("theme.dark"), Some(Value::from("titanium")));
    }

    /// The five switches this crate names for the screen's own channels
    /// resolve from a real file, unset means on, and `off` — as a string or a
    /// bool — turns exactly that one off.
    #[test]
    fn the_screen_switches_are_on_unset_and_off_by_their_own_key() {
        let tmp = TempDir::new().unwrap();
        let agent = tmp.path().join("agent");
        let keys = [
            NOTIFY_COMPLETION_KEY,
            NOTIFY_ERROR_KEY,
            NOTIFY_ASK_KEY,
            TERMINAL_PROGRESS_KEY,
            COMPOSER_TOKEN_RATE_KEY,
        ];
        assert_eq!(
            keys,
            [
                "notify.completion",
                "notify.error",
                "notify.ask",
                "terminal.progress",
                "composer.tokenRate",
            ]
        );

        let empty = Settings::load(&agent, tmp.path(), &[]).unwrap();
        for key in keys {
            assert_eq!(empty.get(key), None, "{key} is unset by default");
            assert!(!switch_off(&empty, key), "{key} is on unset");
        }

        write(
            &agent.join("config.yml"),
            "notify:\n  completion: off\n  error: false\n  ask: \"no\"\n\
             terminal:\n  progress: \"OFF\"\ncomposer:\n  tokenRate: 12\n",
        );
        let s = Settings::load(&agent, tmp.path(), &[]).unwrap();
        assert!(switch_off(&s, NOTIFY_COMPLETION_KEY));
        assert!(switch_off(&s, NOTIFY_ERROR_KEY));
        assert!(switch_off(&s, NOTIFY_ASK_KEY));
        // The string is read case-insensitively.
        assert!(switch_off(&s, TERMINAL_PROGRESS_KEY));
        // A number is not a switch: it leaves the key on rather than guessing.
        assert!(!switch_off(&s, COMPOSER_TOKEN_RATE_KEY));
        assert_eq!(s.get(COMPOSER_TOKEN_RATE_KEY), Some(Value::from(12)));
    }
}
