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
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub defaults: Value,
    pub global: Value,
    pub project: Value,
    pub overlays: Vec<Value>,
    pub runtime: Value,
    pub global_path: PathBuf,
}

/// Canonical file names, in resolution order.
pub const GLOBAL_FILES: [&str; 2] = ["config.yml", "config.yaml"];
pub const PROJECT_SUBPATH: &str = ".titi/config.yml";

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
        let mut current = self.effective();
        for seg in key.split('.') {
            current = current.get(seg).cloned()?;
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

    /// Write a dotted key into the **global** layer and persist it (the only
    /// persistent write path through this API).
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), SettingsError> {
        set_nested(&mut self.global, key, value).map_err(|reason| SettingsError::Key {
            key: key.into(),
            reason,
        })?;
        self.save_global()
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
}

fn lookup(layer: &Value, key: &str) -> Option<Value> {
    let mut current = layer;
    for seg in key.split('.') {
        current = current.get(seg)?;
    }
    Some(current.clone())
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
}
