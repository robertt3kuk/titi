//! Theme resolution for titi-cli: which palette a screen shows.
//!
//! Moved out of `app.rs` with the other live helpers; `chat.rs`, `ompcast.rs`
//! and `main.rs` resolve their theme through here.

use std::sync::Arc;

use titi_tui::theme::{AppearanceInputs, ColorMode, SymbolPreset, Theme, global};

use crate::session_fs::current_workspace;

/// Load the process-wide default theme: the user's choice when they made one,
/// the crate's auto pick otherwise.
pub fn default_theme() -> Result<Arc<Theme>, String> {
    theme_for(&titi_config::agent_dir(), &current_workspace(), None)
}

/// The theme a screen should show.
///
/// `override_name` is a `--theme` for this run, which wins over everything. With
/// none, the settings decide: `theme.dark` and `theme.light` are the two slots
/// the appearance probe picks between, so a user on a dark terminal who chose
/// `gruvbox` keeps it when their terminal changes to light and their light
/// choice is used instead. Neither slot set is `auto` — the crate's own pick
/// (`titanium` dark, `light` light) stands, which is what a user who never chose
/// sees.
pub fn theme_for(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
    override_name: Option<&str>,
) -> Result<Arc<Theme>, String> {
    if let Some(name) = override_name {
        return theme_named(name);
    }
    use titi_config::settings::{Settings, THEME_DARK_KEY, THEME_LIGHT_KEY};
    let settings = Settings::load(agent_dir, workspace, &[]).map_err(|e| e.to_string())?;
    let chosen = |key: &str| {
        settings
            .get(key)
            .and_then(|value| value.as_str().map(str::to_owned))
    };
    let inputs = AppearanceInputs::from_env();
    let (dark, light) = (chosen(THEME_DARK_KEY), chosen(THEME_LIGHT_KEY));
    if dark.is_none() && light.is_none() {
        return loaded(global().init_auto(&inputs));
    }
    loaded(
        global().init_auto_mapped(
            dark.as_deref()
                .unwrap_or(titi_tui::theme::appearance::AUTO_DARK_THEME),
            light
                .as_deref()
                .unwrap_or(titi_tui::theme::appearance::AUTO_LIGHT_THEME),
            &inputs,
        ),
    )
}

/// Every palette this build carries: the crate's registry plus
/// `{agent_dir}/themes`, the list `/theme` opens on.
pub fn theme_names() -> Vec<String> {
    titi_tui::theme::loader::get_available_themes()
}

/// The refusal an unknown theme gets, naming what there is rather than leaving
/// a blank to guess at. `/theme` and `--theme` say it in one voice.
pub fn unknown_theme(name: &str) -> String {
    format!(
        "unknown theme {name}: this build carries {} ({} and {} are two of them); \
         /theme lists them, and --theme takes one",
        theme_names().len(),
        titi_tui::theme::appearance::AUTO_DARK_THEME,
        titi_tui::theme::appearance::AUTO_LIGHT_THEME,
    )
}

/// A theme by name, refusing one this build does not carry rather than quietly
/// painting another palette: the fallback in the loader is the built-in dark
/// theme, which would answer a typo with a theme nobody asked for.
pub fn theme_named(name: &str) -> Result<Arc<Theme>, String> {
    if !theme_names().iter().any(|known| known == name) {
        return Err(unknown_theme(name));
    }
    // An explicit name is for this run: `init` is the crate's `setTheme`, which
    // turns auto-detection off so the probe cannot undo the choice.
    loaded(global().init(name))
}

/// The settings key a choice belongs in: the slot the terminal's own
/// background selects, so a dark terminal's choice is the dark slot's.
pub fn theme_slot(inputs: &AppearanceInputs) -> &'static str {
    match titi_tui::theme::appearance::detect_terminal_background(inputs) {
        titi_tui::theme::appearance::Appearance::Light => titi_config::settings::THEME_LIGHT_KEY,
        titi_tui::theme::appearance::Appearance::Dark => titi_config::settings::THEME_DARK_KEY,
    }
}

/// The theme the global just loaded, or one built from its name when the load
/// itself failed — the screen always has a palette to draw with.
fn loaded(name: String) -> Result<Arc<Theme>, String> {
    match global().current() {
        Some(theme) => Ok(theme),
        None => Theme::new(
            name,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            ColorMode::Truecolor,
            SymbolPreset::Unicode,
            std::collections::HashMap::new(),
            None,
            None,
        )
        .map(Arc::new),
    }
}
