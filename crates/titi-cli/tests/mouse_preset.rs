//! The mouse-tracking preset persisted through the agent config.
//!
//! Rehomed here from `tests/transcript.rs` when the `App` stack was deleted:
//! the round trip runs through `titi_cli::session_fs`, which the live chat
//! still owns, and is the storage half of the mouse-selection port
//! (`titi_tui::selection`).

#![allow(clippy::unwrap_used)]

use titi_cli::session_fs::{load_mouse_preset_from, save_mouse_preset_to};
use titi_tui::caps::MousePreset;

#[test]
fn mouse_preset_roundtrips_through_config() {
    // Use a temp dir as the agent directory; titi-config creates the yml
    // file on first set().
    let tmp = std::env::temp_dir().join(format!("titi-mouse-config-{}", std::process::id()));
    let agent_dir = tmp.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    // Default: nothing persisted.
    assert_eq!(load_mouse_preset_from(&agent_dir), None);
    // Save buttons → load buttons.
    save_mouse_preset_to(&agent_dir, MousePreset::Buttons).unwrap();
    assert_eq!(
        load_mouse_preset_from(&agent_dir),
        Some(MousePreset::Buttons)
    );
    // Save all → load all.
    save_mouse_preset_to(&agent_dir, MousePreset::All).unwrap();
    assert_eq!(load_mouse_preset_from(&agent_dir), Some(MousePreset::All));

    let _ = std::fs::remove_dir_all(&tmp);
}
