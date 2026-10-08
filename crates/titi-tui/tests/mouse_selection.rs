//! Integration test for mouse drag-select — the `Selection` model itself.
//!
//! Contract: `docs/research/agent-ux/README.md` (DoD mouse item):
//! drag-select draws selection background instead of SGR inverse.
//!
//! The SGR-1006 decoding half of this test went with `titi_tui::input`,
//! which only the deleted `App` loop consumed; the pointer still has to
//! decode SGR mouse events on the day `Selection` is wired into the chat,
//! and that decoder will be written against the chat's own input path.
//! What stays asserted here is the model: an anchor plus a drag paints the
//! selected rows, releases into a rectangle, and never invents one from a
//! bare default.

use serde_json::json;
use std::collections::HashMap;

use titi_tui::selection::Selection;
use titi_tui::theme::{ColorMode, SymbolPreset, Theme, ThemeBg};

fn test_theme() -> Theme {
    let mut fg = HashMap::new();
    fg.insert("error".into(), json!("#ff0000"));
    let mut bg = HashMap::new();
    bg.insert("selectedBg".into(), json!("#335599"));
    Theme::new(
        "test".into(),
        fg,
        bg,
        ColorMode::Truecolor,
        SymbolPreset::Unicode,
        HashMap::new(),
        None,
        None,
    )
    .expect("test theme builds")
}

#[test]
fn drag_select_paints_background_over_selected_rows() {
    // A press at (2, 1) dragged to (6, 3) and released.
    let mut selection = Selection::anchor(2, 1);
    selection.drag(6, 3);
    selection.release();

    assert_eq!(selection.rect(), Some((2, 1, 6, 3)));

    // A viewport: rows 0..4.  Selection covers rows 1..=3, cols 2..=6.
    let theme = test_theme();
    let rows: Vec<String> = (0..4).map(|i| format!("row {i} — pad")).collect();
    let painted = selection.apply_background(&rows, &theme);
    let bg = theme.get_bg_ansi(ThemeBg::SelectedBg);

    // Row 0 outside the selection → untouched.
    assert!(
        !painted[0].contains(&bg),
        "row 0 untouched: {:?}",
        painted[0]
    );
    // Rows 1..=3 painted (cols 2..=6 of 11 columns).
    for i in 1..=3 {
        assert!(
            painted[i].contains(&bg),
            "row {i} painted: {:?}",
            painted[i]
        );
        assert!(
            painted[i].contains("\x1b[49m"),
            "row {i} closes bg: {:?}",
            painted[i]
        );
    }
    // The bg splits the row at column 2, but the visible text survives
    // across the split: prefix "ro" before the bg, "w 1 " inside, "pad" after.
    assert!(
        painted[1].starts_with("ro"),
        "prefix preserved: {:?}",
        painted[1]
    );
    assert!(
        painted[1].contains("w 1 "),
        "selected text preserved: {:?}",
        painted[1]
    );
    assert!(
        painted[3].ends_with(" pad"),
        "suffix preserved: {:?}",
        painted[3]
    );
}

#[test]
fn no_drag_means_no_selection() {
    // Nothing pressed → no anchor, so nothing is painted.
    assert!(Selection::default().rect().is_none());
    // A press that never moved is an empty selection, not a rectangle.
    let mut selection = Selection::anchor(5, 5);
    selection.release();
    assert!(selection.rect().is_none());
}
