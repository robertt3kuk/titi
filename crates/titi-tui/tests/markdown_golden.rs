//! Golden tests for the transcript markdown renderer.
//!
//! Contract: `docs/research/agent-ux/README.md` — "markdown-рендер с
//! токенами темы (golden-тесты рендера)".
//!
//! A synthetic theme with explicit hex values for every markdown token makes
//! the ANSI output deterministic, so the committed golden files are stable.

use serde_json::json;
use std::collections::HashMap;
use titi_tui::markdown::render_markdown;
use titi_tui::theme::{ColorMode, SymbolPreset, Theme};

/// Token keys used by the markdown renderer (camelCase, schema-valid).
const MD_TOKENS: &[(&str, &str)] = &[
    ("mdHeading", "#ffcc00"),
    ("mdLink", "#4da6ff"),
    ("mdLinkUrl", "#7f7f7f"),
    ("mdCode", "#ff7b72"),
    ("mdCodeBlock", "#c9d1d9"),
    ("mdCodeBlockBorder", "#444"),
    ("mdQuote", "#8b949e"),
    ("mdQuoteBorder", "#58a6ff"),
    ("mdHr", "#30363d"),
    ("mdListBullet", "#ffcc00"),
];

fn golden_theme() -> Theme {
    let mut fg = HashMap::new();
    for (k, v) in MD_TOKENS {
        fg.insert((*k).to_string(), json!(v));
    }
    Theme::new(
        "golden".into(),
        fg,
        HashMap::new(),
        ColorMode::Truecolor,
        SymbolPreset::Unicode,
        HashMap::new(),
        None,
        None,
    )
    .expect("golden theme builds")
}

/// Render and re-emit the raw markdown with all styling.
fn golden_render(md: &str) -> String {
    render_markdown(md, &golden_theme(), 40).join("\n")
}

#[test]
fn golden_plain_paragraph() {
    let got = golden_render("Hello world");
    let expected = "Hello world";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_heading_and_paragraph() {
    let got = golden_render("# Title\n\nBody text.");
    // The `#` run is dropped; H1 is bold+underline over mdHeading; the blank
    // line separates the heading block from the paragraph.
    let expected = "\x1b[1;4;38;2;255;204;0mTitle\x1b[39m\x1b[24m\x1b[22m\n\nBody text.";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_paragraph_wrap() {
    let got = golden_render("The quick brown fox jumps over the lazy dog. This sentence is long.");
    // At width 40, the line should wrap; both lines plain.
    let lines: Vec<&str> = got.split('\n').collect();
    assert!(lines.len() >= 2, "expected wrapping, got: {got:?}");
    assert!(
        lines[0].trim_end().ends_with("lazy"),
        "first line: {lines:?}"
    );
}

#[test]
fn golden_bold() {
    let got = golden_render("This is **bold** text");
    // **bold** -> ANSI bold around the word, no md token.
    let expected = "This is \x1b[1mbold\x1b[22m text";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_inline_code() {
    let got = golden_render("Run `cargo build` now");
    let expected = "Run \x1b[38;2;255;123;114mcargo build\x1b[39m now";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_link() {
    let got = golden_render("See [docs](https://docs.rs)");
    let expected =
        "See \x1b[38;2;77;166;255mdocs\x1b[39m\x1b[38;2;127;127;127m (https://docs.rs)\x1b[39m";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_unordered_list() {
    let got = golden_render("- first\n- second");
    let expected = "\
\x1b[38;2;255;204;0m•\x1b[39m first
\x1b[38;2;255;204;0m•\x1b[39m second";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_ordered_list() {
    let got = golden_render("1. first\n2. second");
    let expected = "\
\x1b[38;2;255;204;0m1.\x1b[39m first
\x1b[38;2;255;204;0m2.\x1b[39m second";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_blockquote() {
    let got = golden_render("> quoted text");
    let expected = "\x1b[38;2;88;166;255m▎ \x1b[39m\x1b[38;2;139;148;158mquoted text\x1b[39m";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_code_block() {
    let got = golden_render("```rust\nfn main() {}\n```");
    // A box as wide as the pane: the language label rides in the top rule once
    // (mdCodeBlockBorder), the body is mdCodeBlock, and both body rows plus
    // the frame fill exactly 40 columns.
    let border = "\x1b[38;2;68;68;68m";
    let body = "\x1b[38;2;201;209;217m";
    let expected = format!(
        "{border}╭─ rust {}╮\x1b[39m\n\
         {border}│\x1b[39m {body}fn main() {{}}\x1b[39m{} {border}│\x1b[39m\n\
         {border}╰{}╯\x1b[39m",
        "─".repeat(31),
        " ".repeat(24),
        "─".repeat(38),
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

#[test]
fn golden_mixed_document() {
    let got = golden_render("# Title\n\nSome **bold** and `code` here.\n\n> A quote\n\n- item");
    // Every construct styled: heading (bold+underline mdHeading, no `#`),
    // inline bold, inline code, quote gutter, bullet.
    let expected = "\x1b[1;4;38;2;255;204;0mTitle\x1b[39m\x1b[24m\x1b[22m\n\n\
         Some \x1b[1mbold\x1b[22m and \x1b[38;2;255;123;114mcode\x1b[39m here.\n\n\
         \x1b[38;2;88;166;255m▎ \x1b[39m\x1b[38;2;139;148;158mA quote\x1b[39m\n\n\
         \x1b[38;2;255;204;0m•\x1b[39m item";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}
