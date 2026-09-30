//! Theme struct + GlobalTheme tests.
//!
//! Exercises the public accessor surface against synthetic color maps and the
//! real 100 built-in themes (via loader, which is tested separately).

use super::*;
use serde_json::json;
use std::collections::HashMap;

/// Build a theme with an explicit fg + bg color set and default symbols.
fn make_theme(
    fg: HashMap<String, Value>,
    bg: HashMap<String, Value>,
    overrides: HashMap<String, String>,
) -> Theme {
    Theme::new(
        "test".to_string(),
        fg,
        bg,
        ColorMode::Truecolor,
        SymbolPreset::Unicode,
        overrides,
        None,
        None,
    )
    .expect("theme builds")
}

/// A minimal dark theme: status line bg black.
fn dark_theme() -> Theme {
    let mut bg = HashMap::new();
    bg.insert("statusLineBg".to_string(), json!("#000000"));
    make_theme(HashMap::new(), bg, HashMap::new())
}

/// A light theme: status line bg near-white.
fn light_theme() -> Theme {
    let mut bg = HashMap::new();
    bg.insert("statusLineBg".to_string(), json!("#f5f5f5"));
    make_theme(HashMap::new(), bg, HashMap::new())
}

// ---- fg/bg wrappers and resets -------------------------------------------

#[test]
fn fg_wraps_and_resets() {
    let mut fg = HashMap::new();
    fg.insert("accent".to_string(), json!("#ff0000"));
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    let out = theme.fg(ThemeColor::Accent, "hi");
    assert!(out.starts_with("\x1b[38;2;255;0;0m"), "{out:?}");
    assert!(out.ends_with("hi\x1b[39m"), "{out:?}");
}

#[test]
fn bg_wraps_and_resets() {
    let mut bg = HashMap::new();
    bg.insert("userMessageBg".to_string(), json!("#112233"));
    let theme = make_theme(HashMap::new(), bg, HashMap::new());
    let out = theme.bg(ThemeBg::UserMessageBg, "hi");
    assert!(out.starts_with("\x1b[48;2;17;34;51m"), "{out:?}");
    assert!(out.ends_with("hi\x1b[49m"), "{out:?}");
}

#[test]
fn missing_token_falls_back_to_reset() {
    let theme = dark_theme();
    // accent is unset in dark_theme()
    assert_eq!(theme.get_fg_ansi(ThemeColor::Accent), "\x1b[39m");
    // userMessageBg is unset in dark_theme()
    assert_eq!(theme.get_bg_ansi(ThemeBg::UserMessageBg), "\x1b[49m");
}

#[test]
fn bg_fill_reapplies_after_nested_resets() {
    let mut bg = HashMap::new();
    bg.insert("userMessageBg".to_string(), json!("#112233"));
    let theme = make_theme(HashMap::new(), bg, HashMap::new());
    let ansi = "\x1b[48;2;17;34;51m";
    let out = theme.bg_fill(ThemeBg::UserMessageBg, "a\x1b[0mb\x1b[49mc");
    assert!(out.starts_with(ansi), "{out:?}");
    // The fill reapplies after each reset, then the wrapper closes once.
    assert!(out.contains(&format!("{ansi}b")), "{out:?}");
    assert!(out.contains(&format!("{ansi}c")), "{out:?}");
    assert!(out.ends_with("c\x1b[49m"), "{out:?}");
    // No stray resets after the final close.
    assert_eq!(out.matches(ansi).count(), 3, "{out:?}");
}

#[test]
fn fg_on_bg_reapplies_after_nested_fg_resets() {
    let mut fg = HashMap::new();
    fg.insert("text".to_string(), json!("#ffffff"));
    let mut bg = HashMap::new();
    bg.insert("userMessageBg".to_string(), json!("#000000"));
    let theme = make_theme(fg, bg, HashMap::new());
    let ansi = "\x1b[38;2;255;255;255m";
    let out = theme.fg_on_bg(ThemeColor::Text, ThemeBg::UserMessageBg, "a\x1b[39mb");
    assert!(out.starts_with(ansi), "{out:?}");
    assert!(out.contains(&format!("{ansi}b")), "{out:?}");
    assert!(out.ends_with("b\x1b[39m"), "{out:?}");
}

#[test]
fn fg_on_bg_contrast_pick_for_default_token() {
    // Text token is terminal-default; background is dark → near-white fg.
    let mut bg = HashMap::new();
    bg.insert("userMessageBg".to_string(), json!("#000000"));
    let theme = make_theme(HashMap::new(), bg, HashMap::new());
    let ansi = theme.get_fg_on_bg_ansi(ThemeColor::Text, ThemeBg::UserMessageBg);
    assert!(ansi.starts_with("\x1b[38;2;2"), "{ansi:?}"); // #e5e5e7
    // On a light background → near-black.
    let mut bg = HashMap::new();
    bg.insert("userMessageBg".to_string(), json!("#ffffff"));
    let theme = make_theme(HashMap::new(), bg, HashMap::new());
    let ansi = theme.get_fg_on_bg_ansi(ThemeColor::Text, ThemeBg::UserMessageBg);
    assert!(ansi.starts_with("\x1b[38;2;0"), "{ansi:?}");
}

#[test]
fn get_color_hex_default_fallback() {
    let theme = dark_theme();
    assert_eq!(theme.get_color_hex(ThemeColor::Text), DARK_DEFAULT_FG);
    let theme = light_theme();
    assert_eq!(theme.get_color_hex(ThemeColor::Text), LIGHT_DEFAULT_FG);
    assert_eq!(theme.get_bg_hex(ThemeBg::StatusLineBg), "#f5f5f5");
}

#[test]
fn is_light_classification() {
    assert!(!dark_theme().is_light());
    assert!(light_theme().is_light());
}

#[test]
fn accent_surface_luminance_only_for_light() {
    assert_eq!(dark_theme().accent_surface_luminance(), None);
    assert!(light_theme().accent_surface_luminance().is_some());
}

#[test]
fn get_contrast_fg_ansi_by_luma() {
    let mut fg = HashMap::new();
    fg.insert("accent".to_string(), json!("#ffffff")); // bright
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    assert_eq!(
        theme.get_contrast_fg_ansi(ThemeColor::Accent),
        "\x1b[38;2;0;0;0m"
    );

    let mut fg = HashMap::new();
    fg.insert("accent".to_string(), json!("#000080")); // dark
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    assert_eq!(
        theme.get_contrast_fg_ansi(ThemeColor::Accent),
        "\x1b[38;2;255;255;255m"
    );
}

#[test]
fn get_contrast_fg_falls_back_for_256_palette() {
    // 256-palette index has no RGB escape → falls back to Text token.
    let mut fg = HashMap::new();
    fg.insert("accent".to_string(), json!(196));
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    assert_eq!(theme.get_contrast_fg_ansi(ThemeColor::Accent), "\x1b[39m");
}

// ---- symbols --------------------------------------------------------------
#[test]
fn symbol_lookup_and_unknown_key() {
    let theme = dark_theme();
    assert!(!theme.symbol("status.success").is_empty());
    assert_eq!(theme.symbol("does.not.exist"), "");
}

#[test]
fn symbol_overrides_patch_preset() {
    let mut overrides = HashMap::new();
    overrides.insert("status.success".to_string(), "X".to_string());
    overrides.insert("unknown.key".to_string(), "ignored".to_string());
    let theme = make_theme(HashMap::new(), HashMap::new(), overrides);
    assert_eq!(theme.symbol("status.success"), "X");
    assert_ne!(theme.symbol("status.error"), "");
}

#[test]
fn styled_symbol_wraps() {
    let theme = dark_theme();
    let out = theme.styled_symbol("common.check", ThemeColor::Success);
    assert!(
        out.starts_with("\x1b[39m") || out.starts_with("\x1b[38;"),
        "{out:?}"
    );
    assert!(out.ends_with("\x1b[39m"), "{out:?}");
}

// ---- spinners -------------------------------------------------------------

#[test]
fn spinner_frames_defaults() {
    let theme = dark_theme();
    assert!(!theme.spinner_frames().is_empty());
    let status = theme.get_spinner_frames("status");
    let activity = theme.get_spinner_frames("activity");
    assert_eq!(status, theme.spinner_frames());
    // Both are non-empty on the unicode preset.
    assert!(!activity.is_empty());
    // Unknown type falls back to status.
    assert_eq!(theme.get_spinner_frames("bogus"), status);
}

#[test]
fn spinner_frames_overrides() {
    let theme = Theme::new(
        "test".to_string(),
        HashMap::new(),
        HashMap::new(),
        ColorMode::Truecolor,
        SymbolPreset::Unicode,
        HashMap::new(),
        Some(vec!["s1".to_string(), "s2".to_string()]),
        Some(vec!["a1".to_string()]),
    )
    .expect("theme builds");
    assert_eq!(theme.spinner_frames(), ["s1", "s2"]);
    assert_eq!(theme.get_spinner_frames("activity"), ["a1"]);
}

// ---- language icons -------------------------------------------------------

#[test]
fn lang_icon_alias_resolution() {
    let theme = dark_theme();
    assert!(!theme.get_lang_icon(Some("python")).is_empty());
    assert_eq!(
        theme.get_lang_icon(Some("py")),
        theme.get_lang_icon(Some("python"))
    );
    assert!(!theme.get_lang_icon(Some("rs")).is_empty());
    assert_eq!(
        theme.get_lang_icon(Some("bogus")),
        theme.get_lang_icon(None)
    );
}

#[test]
fn lang_icon_styled_brand() {
    let theme = dark_theme();
    let py = theme.get_lang_icon_styled(Some("python"));
    // Brand-colored: contains the python blue.
    assert!(
        py.starts_with("\x1b[38;2;55;118;171m") || py.starts_with("\x1b[38;5;"),
        "{py:?}"
    );
    let plain = theme.get_lang_icon_styled(Some("rust"));
    // No brand entry → muted fg.
    assert!(
        plain.starts_with("\x1b[39m") || plain.starts_with("\x1b[38;"),
        "{plain:?}"
    );
}

// ---- thinking border color ------------------------------------------------

#[test]
fn thinking_border_color_max_fallback() {
    let theme = dark_theme();
    assert_eq!(
        theme.get_thinking_border_color("max"),
        ThemeColor::ThinkingXhigh
    );
    let mut fg = HashMap::new();
    fg.insert("thinkingMax".to_string(), json!("#ff0000"));
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    assert_eq!(
        theme.get_thinking_border_color("max"),
        ThemeColor::ThinkingMax
    );
    assert_eq!(
        theme.get_thinking_border_color("minimal"),
        ThemeColor::ThinkingMinimal
    );
    assert_eq!(
        theme.get_thinking_border_color("bogus"),
        ThemeColor::ThinkingOff
    );
}

// ---- colorblind mode ------------------------------------------------------

#[test]
fn colorblind_adjusts_tool_diff_added() {
    let mut fg = HashMap::new();
    fg.insert("toolDiffAdded".to_string(), json!("#00ff00"));
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    let adjusted = theme.with_color_blind_mode();
    let hex = adjusted.get_color_hex(ThemeColor::ToolDiffAdded);
    // Green shifted 60° toward blue: #00ff00 → #00ffff-ish.
    let (h, _, _) = color::rgb_to_hsv(color::hex_to_rgb(&hex).expect("hex parses"));
    assert!(
        (h - 180.0).abs() < 2.0,
        "expected hue ~180, got {h} for {hex}"
    );
}

#[test]
fn colorblind_noop_without_tool_diff_added() {
    let theme = dark_theme();
    let adjusted = theme.with_color_blind_mode();
    assert_eq!(
        adjusted.get_color_hex(ThemeColor::ToolDiffAdded),
        DARK_DEFAULT_FG
    );
}

// ---- text styles ----------------------------------------------------------

#[test]
fn text_style_wrappers() {
    let theme = dark_theme();
    assert_eq!(theme.bold("x"), "\x1b[1mx\x1b[22m");
    assert_eq!(theme.italic("x"), "\x1b[3mx\x1b[23m");
    assert_eq!(theme.underline("x"), "\x1b[4mx\x1b[24m");
    assert_eq!(theme.strikethrough("x"), "\x1b[9mx\x1b[29m");
    assert_eq!(theme.inverse("x"), "\x1b[7mx\x1b[27m");
}

// ---- color mode / preset accessors ----------------------------------------

#[test]
fn accessors() {
    let mut fg = HashMap::new();
    fg.insert("accent".to_string(), json!("#ff0000"));
    let theme = make_theme(fg, HashMap::new(), HashMap::new());
    assert_eq!(theme.get_color_mode(), ColorMode::Truecolor);
    assert_eq!(theme.get_symbol_preset(), SymbolPreset::Unicode);
    assert_eq!(theme.get_accent_color_hex(), "#ff0000");
    assert!(!theme.get_all_theme_color_hexes().is_empty());
    assert!(
        theme
            .get_major_theme_color_hexes()
            .contains(&"#ff0000".to_string())
    );
}

// ---- GlobalTheme ----------------------------------------------------------

#[test]
fn global_theme_init_sets_and_bumps_epoch() {
    let g = GlobalTheme::new();
    let e0 = g.epoch();
    let name = g.init("dark");
    assert_eq!(name, "dark");
    assert_eq!(g.get_current_theme_name().as_deref(), Some("dark"));
    assert!(g.current().is_some());
    assert!(g.epoch() >= e0 + 1);

    g.set("light").expect("light loads");
    assert_eq!(g.get_current_theme_name().as_deref(), Some("light"));
    assert!(g.current().is_some_and(|t| t.is_light()));
    assert!(g.epoch() >= e0 + 2);
}
#[test]
fn global_theme_set_failure_keeps_state() {
    let g = GlobalTheme::new();
    g.init("dark");
    // THEME_EPOCH is process-wide, shared with parallel tests — an exact
    // equality check would race; assert only the failure + stable name.
    assert!(g.set("no-such-theme-xyz").is_err());
    assert_eq!(g.get_current_theme_name().as_deref(), Some("dark"));
}

#[test]
fn global_theme_preview_and_instance() {
    let g = GlobalTheme::new();
    g.init("dark");
    g.preview("light").expect("preview works");
    // Preview does not change the committed name.
    assert_eq!(g.get_current_theme_name().as_deref(), Some("dark"));
    let theme = light_theme();
    g.set_instance(theme.clone());
    assert!(g.current().is_some_and(|t| t.is_light()));
}

#[test]
fn global_singleton_is_shared() {
    let a = global();
    let b = global();
    assert!(std::ptr::eq(a, b));
    assert_eq!(a.epoch(), b.epoch());
}

// ---- built-in themes via loader -------------------------------------------

#[test]
fn every_builtin_theme_loads() {
    let names = builtin::list_builtin_themes();
    assert_eq!(names.len(), 100, "expected 100 built-in themes");
    for name in names {
        let theme = loader::load_theme(name, &loader::CreateThemeOptions::default())
            .unwrap_or_else(|e| panic!("theme {name} failed to load: {e}"));
        // Smoke: the accent token always resolves to *something*.
        let hex = theme.get_color_hex(ThemeColor::Accent);
        assert!(hex.starts_with('#'), "accent hex for {name}: {hex}");
        assert_eq!(hex.len(), 7, "accent hex for {name}: {hex}");
        assert!(
            !theme.symbol("status.success").is_empty(),
            "symbols missing for {name}"
        );
    }
}

/// The presets titi's own auto slots use — `titanium` for a dark terminal,
/// `light` for a light one (`appearance::AUTO_DARK_THEME` / `AUTO_LIGHT_THEME`)
/// — have to give the user's own block a surface of its own.
///
/// The live screen paints the band behind a user's question with
/// `userMessageBg`; when that token resolves to the same value as the chrome
/// behind it (`statusLineBg`) the band is drawn and invisible, which is what
/// titanium did until this was caught. Seven of the hundred shipped presets
/// collapse the two, and the two titi starts on by itself may not be among
/// them.
///
/// The second half is the other way the band can fail: a surface the body
/// cannot be read on. `userMessageText` is empty in both presets, so the body
/// is the terminal default the theme resolves for a dark page (`#e5e5e7`).
#[test]
fn the_auto_theme_slots_show_the_user_block() {
    for name in [appearance::AUTO_DARK_THEME, appearance::AUTO_LIGHT_THEME] {
        let theme = loader::load_theme(name, &loader::CreateThemeOptions::default())
            .unwrap_or_else(|e| panic!("theme {name} failed to load: {e}"));
        let band = theme.get_bg_hex(ThemeBg::UserMessageBg);
        let page = theme.get_bg_hex(ThemeBg::StatusLineBg);
        assert_ne!(
            band, page,
            "{name}: the user block's band is the same as the page behind it ({band})"
        );
        let body = theme.get_color_hex(ThemeColor::UserMessageText);
        let band_luma = color::relative_luminance(&band)
            .unwrap_or_else(|| panic!("{name}: {band} is not a colour"));
        let body_luma = color::relative_luminance(&body)
            .unwrap_or_else(|| panic!("{name}: {body} is not a colour"));
        let (lighter, darker) = if body_luma > band_luma {
            (body_luma, band_luma)
        } else {
            (band_luma, body_luma)
        };
        let ratio = (lighter + 0.05) / (darker + 0.05);
        assert!(
            ratio >= 4.5,
            "{name}: the body on the band is {ratio:.2}:1 ({body} on {band})"
        );
    }
}

#[test]
fn dark_and_light_load_through_loader() {
    let dark =
        loader::load_theme("dark", &loader::CreateThemeOptions::default()).expect("dark loads");
    assert!(!dark.is_light());
    let light =
        loader::load_theme("light", &loader::CreateThemeOptions::default()).expect("light loads");
    assert!(light.is_light());
}

#[test]
fn builtin_lookup_and_unknown() {
    assert!(builtin::get_builtin_theme("dark").is_some());
    assert!(builtin::get_builtin_theme("alabaster").is_some());
    assert!(builtin::get_builtin_theme("nope").is_none());
}

#[test]
fn global_theme_init_auto_maps_light_colorfgbg() {
    let g = GlobalTheme::new();
    let mut inputs = AppearanceInputs {
        platform: "linux".into(),
        colorfgbg: Some("0;15".into()),
        ..AppearanceInputs::default()
    };
    let name = g.init_auto(&inputs);
    assert_eq!(name, "light");
    assert!(g.auto_detected());
    assert!(g.current().is_some_and(|th| th.is_light()));

    inputs.colorfgbg = Some("15;0".into());
    assert!(g.on_terminal_appearance_change(Appearance::Dark, &inputs));
    assert_eq!(g.get_current_theme_name().as_deref(), Some("titanium"));

    g.set("dark").expect("set");
    assert!(!g.auto_detected());
    assert_eq!(g.get_current_theme_name().as_deref(), Some("dark"));
}

#[test]
fn global_theme_osc11_ignored_when_auto_off() {
    let g = GlobalTheme::new();
    g.init("dark");
    let inputs = AppearanceInputs {
        platform: "linux".into(),
        colorfgbg: Some("0;15".into()),
        ..AppearanceInputs::default()
    };
    assert!(!g.on_terminal_appearance_change(Appearance::Light, &inputs));
    assert_eq!(g.get_current_theme_name().as_deref(), Some("dark"));
}

#[test]
fn global_theme_duplicate_osc11_is_noop() {
    let g = GlobalTheme::new();
    let inputs = AppearanceInputs {
        platform: "linux".into(),
        osc11_appearance: Some(Appearance::Dark),
        ..AppearanceInputs::default()
    };
    g.init_auto(&inputs);
    assert_eq!(g.get_current_theme_name().as_deref(), Some("titanium"));
    assert!(!g.on_terminal_appearance_change(Appearance::Dark, &inputs));
    assert!(g.on_terminal_appearance_change(Appearance::Light, &inputs));
    assert_eq!(g.get_current_theme_name().as_deref(), Some("light"));
}

#[test]
fn global_theme_set_auto_mapping_reevaluates() {
    let g = GlobalTheme::new();
    let inputs = AppearanceInputs {
        platform: "linux".into(),
        osc11_appearance: Some(Appearance::Dark),
        ..AppearanceInputs::default()
    };
    g.init_auto(&inputs);
    assert_eq!(g.get_current_theme_name().as_deref(), Some("titanium"));
    g.set_auto_theme_mapping(Appearance::Dark, "dark", &inputs);
    assert_eq!(g.get_current_theme_name().as_deref(), Some("dark"));
}
