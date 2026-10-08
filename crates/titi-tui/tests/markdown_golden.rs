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
use titi_tui::width::visible_width;

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

/// Render at a chosen pane width (the goldens otherwise use 40 columns).
fn golden_render_w(md: &str, w: u16) -> Vec<String> {
    render_markdown(md, &golden_theme(), w)
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

// ---- GFM tables -----------------------------------------------------------

/// The border token wraps every frame glyph, rules and bars alike, so the
/// border colour is the same one a code block uses.
const TABLE_BORDER: &str = "\x1b[38;2;68;68;68m";
const TABLE_BAR: &str = "\x1b[38;2;68;68;68m│\x1b[39m";

/// A two-column table: a box-drawn grid in the code-block border token, one
/// space of padding per side, the header bold, columns as wide as their widest
/// cell.  Widths are 4 and 5, so the frame is `3*2+1 + 9 = 16` columns.
#[test]
fn golden_table_two_columns() {
    let got =
        golden_render("| Name | Value |\n|------|-------|\n| a    | 1     |\n| b    | 2     |");
    let expected = format!(
        "{TABLE_BORDER}┌──────┬───────┐\x1b[39m\n\
         {TABLE_BAR} \x1b[1mName\x1b[22m {TABLE_BAR} \x1b[1mValue\x1b[22m {TABLE_BAR}\n\
         {TABLE_BORDER}├──────┼───────┤\x1b[39m\n\
         {TABLE_BAR} a    {TABLE_BAR} 1     {TABLE_BAR}\n\
         {TABLE_BORDER}├──────┼───────┤\x1b[39m\n\
         {TABLE_BAR} b    {TABLE_BAR} 2     {TABLE_BAR}\n\
         {TABLE_BORDER}└──────┴───────┘\x1b[39m"
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// Three columns, one per alignment: `:--` left, `:-:` center, `--:` right.
/// The body row is the one that shows the padding move.
#[test]
fn golden_table_three_columns_alignment() {
    let got =
        golden_render("| Left | Center | Right |\n|:-----|:------:|------:|\n| aa | bb | cc |");
    let expected = format!(
        "{TABLE_BORDER}┌──────┬────────┬───────┐\x1b[39m\n\
         {TABLE_BAR} \x1b[1mLeft\x1b[22m {TABLE_BAR} \x1b[1mCenter\x1b[22m {TABLE_BAR} \x1b[1mRight\x1b[22m {TABLE_BAR}\n\
         {TABLE_BORDER}├──────┼────────┼───────┤\x1b[39m\n\
         {TABLE_BAR} aa   {TABLE_BAR}   bb   {TABLE_BAR}    cc {TABLE_BAR}\n\
         {TABLE_BORDER}└──────┴────────┴───────┘\x1b[39m"
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// Inline markdown inside a cell goes through the prose path, so bold and
/// inline code keep their styling and their width stays the visible width.
#[test]
fn golden_table_bold_and_code_cells() {
    let got = golden_render("| Kind | Note |\n|------|------|\n| **hi** | `x` |");
    let expected = format!(
        "{TABLE_BORDER}┌──────┬──────┐\x1b[39m\n\
         {TABLE_BAR} \x1b[1mKind\x1b[22m {TABLE_BAR} \x1b[1mNote\x1b[22m {TABLE_BAR}\n\
         {TABLE_BORDER}├──────┼──────┤\x1b[39m\n\
         {TABLE_BAR} \x1b[1mhi\x1b[22m   {TABLE_BAR} \x1b[38;2;255;123;114mx\x1b[39m    {TABLE_BAR}\n\
         {TABLE_BORDER}└──────┴──────┘\x1b[39m"
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A cell too wide for the pane wraps inside its column: the frame stays 24
/// columns and no row crosses it.
#[test]
fn golden_table_wraps_a_wide_cell_at_a_narrow_pane() {
    let lines = golden_render_w(
        "| Col | Description |\n|-----|-------------|\n| a   | one two three four |",
        24,
    );
    let expected = vec![
        format!("{TABLE_BORDER}┌─────┬────────────────┐\x1b[39m"),
        format!(
            "{TABLE_BAR} \x1b[1mCol\x1b[22m {TABLE_BAR} \x1b[1mDescription   \x1b[22m {TABLE_BAR}"
        ),
        format!("{TABLE_BORDER}├─────┼────────────────┤\x1b[39m"),
        format!("{TABLE_BAR} a   {TABLE_BAR} one two three  {TABLE_BAR}"),
        format!("{TABLE_BAR}     {TABLE_BAR} four           {TABLE_BAR}"),
        format!("{TABLE_BORDER}└─────┴────────────────┘\x1b[39m"),
    ];
    assert_eq!(
        lines, expected,
        "\n--- got ---\n{lines:?}\n--- want ---\n{expected:?}"
    );
    for row in &lines {
        assert_eq!(visible_width(row), 24, "the frame is the pane: {row:?}");
    }
}

/// `\|` is literal text: the pipe reaches the screen, it does not open a cell.
#[test]
fn golden_table_escaped_pipe() {
    let got = golden_render("| a \\| b | c |\n|---|---|\n| 1 | 2 |");
    let expected = format!(
        "{TABLE_BORDER}┌───────┬───┐\x1b[39m\n\
         {TABLE_BAR} \x1b[1ma | b\x1b[22m {TABLE_BAR} \x1b[1mc\x1b[22m {TABLE_BAR}\n\
         {TABLE_BORDER}├───────┼───┤\x1b[39m\n\
         {TABLE_BAR} 1     {TABLE_BAR} 2 {TABLE_BAR}\n\
         {TABLE_BORDER}└───────┴───┘\x1b[39m"
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A delimiter row that disagrees with the header is not a table, so both
/// lines stay literal rather than one being eaten.
#[test]
fn golden_table_malformed_falls_back_to_literal() {
    let got = golden_render("| a | b |\n|---|");
    let expected = "| a | b |\n|---|";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A table directly after prose opens a block with a blank row, like a heading.
#[test]
fn golden_table_after_paragraph() {
    let got = golden_render("Here is a summary:\n| a | b |\n|---|---|\n| 1 | 2 |");
    let expected = format!(
        "Here is a summary:\n\n\
         {TABLE_BORDER}┌───┬───┐\x1b[39m\n\
         {TABLE_BAR} \x1b[1ma\x1b[22m {TABLE_BAR} \x1b[1mb\x1b[22m {TABLE_BAR}\n\
         {TABLE_BORDER}├───┼───┤\x1b[39m\n\
         {TABLE_BAR} 1 {TABLE_BAR} 2 {TABLE_BAR}\n\
         {TABLE_BORDER}└───┴───┘\x1b[39m"
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}
