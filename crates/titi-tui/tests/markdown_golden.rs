//! Golden tests for the transcript markdown renderer.
//!
//! Contract: `docs/research/agent-ux/README.md` — "markdown-рендер с
//! токенами темы (golden-тесты рендера)".
//!
//! A synthetic theme with explicit hex values for every markdown token makes
//! the ANSI output deterministic, so the committed golden files are stable.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

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
    render_markdown(md, &golden_theme(), 40, false).join("\n")
}

/// The same theme with the ASCII symbol preset: what a terminal without block
/// glyphs gets, and the preset's separator is `#`.
fn golden_render_ascii(md: &str) -> String {
    let mut fg = HashMap::new();
    for (k, v) in MD_TOKENS {
        fg.insert((*k).to_string(), json!(v));
    }
    let theme = Theme::new(
        "golden-ascii".into(),
        fg,
        HashMap::new(),
        ColorMode::Truecolor,
        SymbolPreset::Ascii,
        HashMap::new(),
        None,
        None,
    )
    .expect("golden ascii theme builds");
    render_markdown(md, &theme, 40, false).join("\n")
}

/// Render at a chosen pane width (the goldens otherwise use 40 columns).
fn golden_render_w(md: &str, w: u16) -> Vec<String> {
    render_markdown(md, &golden_theme(), w, false)
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

// ---- LaTeX math -----------------------------------------------------------

/// Display maths is drawn in the code-block pair: the body in mdCodeBlock, the
/// rule row (the fraction bar, a radical's overline) in the border token.
const MATH_BODY: &str = "\x1b[38;2;201;209;217m";
const MATH_RULE: &str = "\x1b[38;2;68;68;68m";

/// Inline maths is converted inside the inline pipeline, so no `$`, brace or
/// backslash reaches the screen: `$O(n\log n)$` is the formula the model meant.
#[test]
fn golden_math_inline_log() {
    let got = golden_render("$O(n\\log n)$");
    let expected = "O(n log n)";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// Greek and relations become their Unicode glyphs.
#[test]
fn golden_math_inline_greek() {
    let got = golden_render("$\\alpha \\le \\beta$");
    let expected = "α ≤ β";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A one-character script becomes a real superscript/subscript glyph.
#[test]
fn golden_math_inline_scripts() {
    let got = golden_render("$x^2 + y_i$");
    let expected = "x² + yᵢ";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// The maths is plain text inside the inline pipeline, so a bold span around it
/// styles it like the words: the formula inherits the surrounding style.
#[test]
fn golden_math_inline_inside_bold() {
    let got = golden_render("The cost is **$O(n)$** here.");
    let expected = "The cost is \x1b[1mO(n)\x1b[22m here.";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// `\frac` inline stays on one line — a stacked fraction would push extra rows
/// into the middle of the sentence and break the style span around it — and
/// parenthesises a part that is not a single atom.
#[test]
fn golden_math_frac_inline() {
    let got = golden_render("So $\\frac{1}{1-x}$ converges.");
    let expected = "So 1/(1-x) converges.";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A display block: the limits of `\sum` stack above and below the symbol, the
/// block is centred, and it opens and closes with a blank row like a code block.
#[test]
fn golden_math_display_sum_at_80() {
    let lines = golden_render_w("Here it is:\n$$\\sum_{i=1}^{n} i$$", 80);
    let expected = vec![
        "Here it is:".to_owned(),
        String::new(),
        format!("{MATH_BODY}{} n\x1b[39m", " ".repeat(37)),
        format!("{MATH_BODY}{} ∑  i\x1b[39m", " ".repeat(37)),
        format!("{MATH_BODY}{}i=1\x1b[39m", " ".repeat(37)),
        String::new(),
    ];
    assert_eq!(
        lines, expected,
        "\n--- got ---\n{lines:?}\n--- want ---\n{expected:?}"
    );
    for row in &lines {
        assert!(visible_width(row) <= 80, "row fits the pane: {row:?}");
    }
}

/// The same block in a 40-column pane: still centred, still inside the frame.
#[test]
fn golden_math_display_sum_at_40() {
    let lines = golden_render_w("Here it is:\n$$\\sum_{i=1}^{n} i$$", 40);
    let expected = vec![
        "Here it is:".to_owned(),
        String::new(),
        format!("{MATH_BODY}{} n\x1b[39m", " ".repeat(17)),
        format!("{MATH_BODY}{} ∑  i\x1b[39m", " ".repeat(17)),
        format!("{MATH_BODY}{}i=1\x1b[39m", " ".repeat(17)),
        String::new(),
    ];
    assert_eq!(
        lines, expected,
        "\n--- got ---\n{lines:?}\n--- want ---\n{expected:?}"
    );
    for row in &lines {
        assert!(visible_width(row) <= 40, "row fits the pane: {row:?}");
    }
}

/// `\frac` in display style stacks over a rule, and the rule row takes the
/// border token so the bar reads as a bar.
#[test]
fn golden_math_display_frac() {
    let lines = golden_render_w("$$\\frac{1}{1-x}$$", 40);
    let expected = vec![
        format!("{MATH_BODY}{} 1\x1b[39m", " ".repeat(18)),
        format!("{MATH_RULE}{}───\x1b[39m", " ".repeat(18)),
        format!("{MATH_BODY}{}1-x\x1b[39m", " ".repeat(18)),
        String::new(),
    ];
    assert_eq!(
        lines, expected,
        "\n--- got ---\n{lines:?}\n--- want ---\n{expected:?}"
    );
}

/// Guards: a price is money, not maths.  `$5 and $6` stays exactly as written.
#[test]
fn golden_math_price_stays_literal() {
    let got = golden_render("It costs $5 and $6 here.");
    let expected = "It costs $5 and $6 here.";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A `$` inside inline code belongs to the code span, which is styled as code.
#[test]
fn golden_math_dollar_in_inline_code() {
    let got = golden_render("Print `$x$` literally.");
    let expected = "Print \x1b[38;2;255;123;114m$x$\x1b[39m literally.";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A `$` inside a fenced block is code, never maths.
#[test]
fn golden_math_dollar_in_fence() {
    let got = golden_render("```sh\necho $HOME\n```");
    let border = "\x1b[38;2;68;68;68m";
    let body = "\x1b[38;2;201;209;217m";
    let expected = format!(
        "{border}╭─ sh {}╮\x1b[39m\n\
         {border}│\x1b[39m {body}echo $HOME\x1b[39m{} {border}│\x1b[39m\n\
         {border}╰{}╯\x1b[39m",
        "─".repeat(33),
        " ".repeat(26),
        "─".repeat(38),
    );
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A dollar at the start and at the end of a line has no partner, so the line
/// stays literal: a lone `$` is a dollar sign.
#[test]
fn golden_math_lone_dollar_stays_literal() {
    let got = golden_render("$ and $");
    let expected = "$ and $";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// A backslash in front of the `$` suppresses the maths; both characters stay,
/// so the escape is never silently eaten either.
#[test]
fn golden_math_escaped_dollar_stays_literal() {
    let got = golden_render("Costs \\$5 today.");
    let expected = "Costs \\$5 today.";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// An unknown command is left verbatim, braces included — never deleted to hide
/// a gap in the tables.
#[test]
fn golden_math_unknown_command_stays_verbatim() {
    let got = golden_render("$\\foobar{x}$ and \\foobar{x}");
    let expected = "\\foobar{x} and \\foobar{x}";
    assert_eq!(
        got, expected,
        "\n--- got ---\n{got}\n--- want ---\n{expected}"
    );
}

/// Width safety: a display formula too wide to lay out falls back to the flat
/// form, which is wrapped — no row ever crosses the pane.
#[test]
fn golden_math_display_never_exceeds_the_pane() {
    let lines = golden_render_w("$$\\sum_{k=1}^{100} \\frac{k^2}{k+1} \\cdot \\ln k$$", 24);
    assert!(
        lines.len() > 1,
        "the block is laid out, not swallowed: {lines:?}"
    );
    for row in &lines {
        assert!(
            visible_width(row) <= 24,
            "row fits a 24-column pane: {row:?}"
        );
    }
}

/// Width safety for inline maths too: a long formula in prose wraps like the
/// words around it.
#[test]
fn golden_math_inline_never_exceeds_the_pane() {
    let lines = golden_render_w(
        "Sum $\\alpha + \\beta + \\gamma + \\delta + \\epsilon$ here.",
        20,
    );
    for row in &lines {
        assert!(
            visible_width(row) <= 20,
            "row fits a 20-column pane: {row:?}"
        );
    }
}

/// A numeric table carries a bar chart of its one measure, drawn directly
/// under it: labels left, bars scaled so the largest fills the field, and the
/// value as written at each bar's own end.
///
/// The table above the bars is **byte-identical** to the same table rendered
/// where no column qualifies (`120 e5` is the same width as `120 ms` and is not
/// one quantity), which is what proves the chart only ever appends.
#[test]
fn golden_chart_under_a_numeric_table() {
    let md = "| Name | Time |\n|---|---|\n| build | 120 ms |\n| test | 45 ms |\n| lint | 8 ms |\n| fmt | 2 ms |\n";
    let quiet = "| Name | Time |\n|---|---|\n| build | 120 e5 |\n| test | 45 e5 |\n| lint | 8 e5 |\n| fmt | 2 e5 |\n";
    let got = golden_render_w(md, 40);
    let plain = golden_render_w(quiet, 40);
    let bars = &got[got.len() - 4..];
    let dim = "\x1b[38;2;68;68;68m";
    let ink = "\x1b[38;2;201;209;217m";
    let off = "\x1b[39m";
    // The field is 26 cells at this width: 120 ms fills it, and the rest are
    // scaled and rounded to the nearest eighth of a cell.
    let expected = vec![
        format!(
            "  {dim}build{off} {ink}{}{off} {off}120 ms{off}",
            "\u{2588}".repeat(26)
        ),
        format!(
            "  {dim}test{off}  {ink}{}{off} {off}45 ms{off}",
            format!("{}\u{258a}{}", "\u{2588}".repeat(9), " ".repeat(16))
        ),
        format!(
            "  {dim}lint{off}  {ink}{}{off} {off}8 ms{off}",
            format!("\u{2588}\u{258a}{}", " ".repeat(24))
        ),
        format!(
            "  {dim}fmt{off}   {ink}{}{off} {off}2 ms{off}",
            format!("\u{258d}{}", " ".repeat(25))
        ),
    ];
    assert_eq!(bars, expected.as_slice(), "\n--- got ---\n{bars:#?}");
    assert_eq!(
        got.len(),
        plain.len() + 4,
        "the chart adds exactly one row per bar"
    );
    // The table part is the same layout, cell for cell: the only difference is
    // the text of the measure column itself (`120 e5` where the charted table
    // says `120 ms`), so folding that back makes the two byte-identical.
    let folded: Vec<String> = plain
        .iter()
        .map(|line| {
            line.replace("120 e5", "120 ms")
                .replace("45 e5", "45 ms")
                .replace("8 e5", "8 ms")
                .replace("2 e5", "2 ms")
        })
        .collect();
    assert_eq!(
        &got[..plain.len()],
        folded.as_slice(),
        "the table itself must be untouched"
    );
}

/// A table that does not qualify gets no chart at all — three rows is a list.
#[test]
fn golden_no_chart_below_a_short_table() {
    let md = "| Name | Time |\n|---|---|\n| build | 120 ms |\n| test | 45 ms |\n| lint | 8 ms |\n";
    let got = golden_render_w(md, 40);
    assert!(
        !got.iter().any(|line| line.contains('\u{2588}')),
        "no bars under three rows: {got:#?}"
    );
}

/// A pane too narrow for a bar field draws the table alone, never a chart whose
/// bars all look alike.
#[test]
fn golden_no_chart_in_a_narrow_pane() {
    let md = "| Name | Time |\n|---|---|\n| build | 120 ms |\n| test | 45 ms |\n| lint | 8 ms |\n| fmt | 2 ms |\n";
    for width in [16u16, 20, 23] {
        let got = golden_render_w(md, width);
        assert!(
            !got.iter().any(|line| line.contains('\u{2588}')),
            "no chart at width {width}: {got:#?}"
        );
    }
}

/// The ASCII preset draws the same chart in `#`, with no eighth blocks.
#[test]
fn golden_chart_in_ascii() {
    let md = "| Name | Time |\n|---|---|\n| build | 120 ms |\n| test | 45 ms |\n| lint | 8 ms |\n| fmt | 2 ms |\n";
    let got = golden_render_ascii(md);
    let dim = "\x1b[38;2;68;68;68m";
    let ink = "\x1b[38;2;201;209;217m";
    let off = "\x1b[39m";
    // The same bars in `#`: a cell is filled or it is not, so the eighth of a
    // cell that rounds up in Unicode is a whole one here.
    let expected = [
        format!(
            "  {dim}build{off} {ink}{}{off} {off}120 ms{off}",
            "#".repeat(26)
        ),
        format!(
            "  {dim}test{off}  {ink}{}{off} {off}45 ms{off}",
            format!("{}{}", "#".repeat(10), " ".repeat(16))
        ),
        format!(
            "  {dim}lint{off}  {ink}{}{off} {off}8 ms{off}",
            format!("{}{}", "#".repeat(2), " ".repeat(24))
        ),
        format!(
            "  {dim}fmt{off}   {ink}{}{off} {off}2 ms{off}",
            format!("{}{}", "#".repeat(1), " ".repeat(25))
        ),
    ];
    let bars = &got.split('\n').collect::<Vec<_>>();
    let bars = &bars[bars.len() - 4..];
    assert_eq!(bars, expected.as_slice(), "\n--- got ---\n{bars:#?}");
}
