//! Layered settings resolution for titi.
//!
//! Precedence (lowest → highest):
//! `defaults <- global <- project <- overlays (TITI_CONFIG_FILES, then explicit) <- runtime`
//!
//! Merge rules follow `omp://settings`: objects deep-merge, scalars and arrays
//! are replaced wholesale by the higher layer.

pub mod config_file;
pub mod roles;
pub mod settings;

use std::path::PathBuf;

/// Root directory of the active titi agent instance.
///
/// Resolution order: `$TITI_AGENT_DIR`, else `~/.titi/profiles/<name>/agent`
/// when `$TITI_PROFILE` names a non-default profile, else `~/.titi/agent`.
pub fn agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TITI_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    match profile_name() {
        Some(name) => home.join(".titi").join("profiles").join(name).join("agent"),
        None => home.join(".titi").join("agent"),
    }
}

/// Active profile name, if a non-default profile is selected via `$TITI_PROFILE`.
pub fn profile_name() -> Option<String> {
    match std::env::var("TITI_PROFILE") {
        Ok(v) if !v.trim().is_empty() && v.trim() != "default" => Some(v),
        _ => None,
    }
}
