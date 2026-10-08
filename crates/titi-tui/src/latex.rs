//! LaTeX subset → Unicode text.
//!
//! A model answer containing `$O(n\log n)$` must not reach the screen as raw
//! TeX. Terminals cannot lay out real mathematics, but a hand-written subset
//! covers what people actually write:
//!
//! - symbols — Greek, relations and operators (`\le \ge \ne \approx \times
//!   \cdot \pm`), arrows (`\to \Rightarrow`), set theory (`\in \subset \cup
//!   \cap \forall \exists`), the big operators (`\sum \prod \int`), `\infty
//!   \partial \nabla`;
//! - scripts — `^` and `_`, braced or bare: `x^2` → `x²`, `n_i` → `nᵢ`,
//!   `a^n_k` → `aⁿₖ`;
//! - `\frac{a}{b}` → `a/b`, parenthesised when a part is not a single atom:
//!   `\frac{1}{1-x}` → `1/(1-x)`;
//! - `\sqrt{x}` → `√x`, `√(1-x)` when the radicand is more than one atom;
//! - accents — `\hat \bar \overline \vec \dot \tilde …` as Unicode combining
//!   marks, so `\hat{x}` → `x̂`;
//! - `\text{…}`/`\mathrm{…}` verbatim, function names (`\log \sin \lim …`),
//!   spacing (`\,` `\quad`), and `\left(`/`\right)` delimiters (not stretched).
//!
//! # Fallback rules (text is never lost)
//!
//! - **A multi-character script group has no complete Unicode set**, so it
//!   keeps its meaning instead of its shape: `x^{2n}` → `x^(2n)`,
//!   `x_{ij}` → `x_(ij)`. A single character Unicode does have (digits,
//!   `+ - = ( )`, and the letters with real super/subscript forms) becomes a
//!   glyph: `x^{2}` → `x²`.
//! - **An unknown command is left verbatim, braces included**:
//!   `\foobar{x}` → `\foobar{x}`. This is the whole contract for anything not
//!   in the tables — nothing is deleted to hide a gap, so `\begin{matrix}`
//!   and friends reach the screen as written.
//! - Over-long input (past [`MAX_BYTES`]) and over-deep nesting (past
//!   [`MAX_DEPTH`]) are returned as written too.
//! - An accent on an empty group (`\hat{}`) is returned as written, since a
//!   combining mark with nothing under it would vanish.
//!
//! # Inline and display
//!
//! [`to_unicode`] is the *inline* form: exactly one line, so it can be wrapped
//! around by prose, inherit the surrounding SGR style and be re-wrapped by the
//! prose widther. A stacked fraction there would push extra rows into the
//! middle of a sentence and break the style span around it, so a fraction
//! inline stays flat and unambiguous (`(a+b)/c`).
//!
//! [`display_rows`] is the *display* form (`$$…$$` on its own line(s)): it lays
//! the fragment out in two dimensions — fractions stack over a rule
//! (`─────`), limits of `\sum`/`\prod`/`\lim`-family operators go above and
//! below the symbol, `\int` keeps its limits at the side (LaTeX's display
//! convention), radicals grow an overline row. When the layout does not fit
//! `width`, the flat form comes back as a single row, so the caller wraps it
//! and the pane's frame never breaks.
//!
//! Not ported, deliberately: matrices and `\begin{…}` environments, stretchy
//! delimiters, `&` alignment, fonts (`\mathbb`), and colour in math
//! (`\textcolor`). Colour would fight the theme, and a matrix cannot be laid
//! out honestly in a pane this narrow.

use crate::width::visible_width;

/// Longest fragment converted to Unicode; past this the input is returned as
/// written (the caller wraps it, so nothing escapes the pane).
pub const MAX_BYTES: usize = 4096;

/// Deepest group nesting converted; a deeper group is returned as written.
pub const MAX_DEPTH: usize = 16;

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Render a bare math fragment (no `$`/`\(` delimiters) as one line of Unicode.
///
/// Never panics and never drops characters: everything it cannot translate
/// comes back as written.
pub fn to_unicode(src: &str) -> String {
    if src.len() > MAX_BYTES {
        return src.to_owned();
    }
    let mut parser = Parser::new(src);
    let grp = parser.parse_seq(false);
    flat_group(&grp)
}

/// Lay out a display fragment (`$$…$$` body) as plain rows, none wider than
/// `width` cells.
///
/// Rows are plain text with no trailing spaces and no colour; the caller
/// centres and styles them. When the stacked layout is wider than `width` —
/// or the fragment is too long to convert at all — the flat [`to_unicode`]
/// form is returned as a single row instead, so the caller can wrap it.
pub fn display_rows(src: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if src.len() > MAX_BYTES {
        return vec![src.to_owned()];
    }
    let mut parser = Parser::new(src);
    let grp = parser.parse_seq(false);
    let mut layout = layout_group(&grp);
    layout.trim_end();
    if layout.width() == 0 {
        return Vec::new();
    }
    if layout.width() > width {
        return vec![to_unicode(src)];
    }
    layout.lines
}

// ---------------------------------------------------------------------------
// Node tree
// ---------------------------------------------------------------------------

/// A group of nodes (`{a+b}` renders as a unit).
type Grp = Vec<Node>;

/// One parsed math element.
#[derive(Debug, Clone)]
enum Node {
    /// Literal text: symbols, function names, unknown commands verbatim.
    Text(String),
    /// A big operator or a named function; `limits` is true when its scripts
    /// stack in display style (`\sum`, `\lim`), false when they stay at the
    /// side (`\int`, `\log`).
    Op { text: &'static str, limits: bool },
    /// A named function (`\log`, `\sin`, `\lim`): upright text that TeX
    /// separates from its operand by a thin space, which is why `\log n`
    /// reads as `log n` even though the source has no space in it.
    Func { text: &'static str, limits: bool },
    /// `\frac{n}{d}` (`\dfrac`, `\tfrac`, `\cfrac`).
    Frac(Grp, Grp),
    /// `\sqrt{x}`, `\sqrt[3]{x}`.
    Sqrt { index: Option<Grp>, body: Grp },
    /// `\hat{x}` and friends: a combining mark over the group's last glyph.
    Accent { mark: char, body: Grp },
    /// A base with its scripts.
    Scripts {
        base: Grp,
        sup: Option<Grp>,
        sub: Option<Grp>,
        limits: bool,
    },
    /// `{…}` — a group, which is what a script or a fraction argument binds.
    Group(Grp),
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Plain symbols: Greek, relations, operators, arrows, set theory, big glyphs.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek, lower case.
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "φ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        // Greek, upper case.
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // Relations.
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        "perp" => "⊥",
        "parallel" => "∥",
        "mid" => "|",
        "nmid" => "∤",
        "doteq" => "≐",
        // Operators and misc glyphs.
        "times" => "×",
        "cdot" => "·",
        "cdots" => "⋯",
        "dots" | "ldots" | "dotsc" => "…",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "pm" => "±",
        "mp" => "∓",
        "div" => "÷",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "∙",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "partial" => "∂",
        "nabla" => "∇",
        "infty" => "∞",
        "emptyset" | "varnothing" => "∅",
        "neg" | "lnot" => "¬",
        "wedge" | "land" => "∧",
        "vee" | "lor" => "∨",
        "prime" => "′",
        "degree" => "°",
        "angle" => "∠",
        "triangle" => "△",
        "square" => "□",
        "checkmark" => "✓",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "dagger" => "†",
        "setminus" | "smallsetminus" => "∖",
        "colon" => ":",
        "therefore" => "∴",
        "because" => "∵",
        // Arrows.
        "to" | "rightarrow" | "longrightarrow" => "→",
        "leftarrow" | "gets" | "longleftarrow" => "←",
        "Rightarrow" | "implies" | "Longrightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "leftrightarrow" => "↔",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "nearrow" => "↗",
        "searrow" => "↘",
        // Set theory.
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "top" => "⊤",
        "bot" => "⊥",
        // Delimiters that are commands.
        "langle" => "⟨",
        "rangle" => "⟩",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "Vert" | "lVert" | "rVert" => "‖",
        "vert" | "lvert" | "rvert" => "|",
        "backslash" => "\\",
        _ => return None,
    })
}

/// Big operators; the flag is the display-style default for their limits.
fn big_operator(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "sum" => ("∑", true),
        "prod" => ("∏", true),
        "coprod" => ("∐", true),
        "bigcup" => ("⋃", true),
        "bigcap" => ("⋂", true),
        "bigvee" => ("⋁", true),
        "bigwedge" => ("⋀", true),
        "bigoplus" => ("⨁", true),
        "bigotimes" => ("⨂", true),
        "bigodot" => ("⨀", true),
        "biguplus" => ("⨄", true),
        "bigsqcup" => ("⨆", true),
        // Integrals keep their limits at the side in display style.
        "int" => ("∫", false),
        "iint" => ("∬", false),
        "iiint" => ("∭", false),
        "iiiint" => ("⨌", false),
        "oint" => ("∮", false),
        _ => return None,
    })
}

/// Named functions: upright text, `limits` true for the ones that take
/// limits above/below in display style.
fn function_name(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "log" => ("log", false),
        "ln" => ("ln", false),
        "lg" => ("lg", false),
        "exp" => ("exp", false),
        "sin" => ("sin", false),
        "cos" => ("cos", false),
        "tan" => ("tan", false),
        "cot" => ("cot", false),
        "sec" => ("sec", false),
        "csc" => ("csc", false),
        "arcsin" => ("arcsin", false),
        "arccos" => ("arccos", false),
        "arctan" => ("arctan", false),
        "sinh" => ("sinh", false),
        "cosh" => ("cosh", false),
        "tanh" => ("tanh", false),
        "lim" => ("lim", true),
        "limsup" => ("lim sup", true),
        "liminf" => ("lim inf", true),
        "max" => ("max", true),
        "min" => ("min", true),
        "sup" => ("sup", true),
        "inf" => ("inf", true),
        "det" => ("det", true),
        "gcd" => ("gcd", true),
        "Pr" => ("Pr", true),
        "argmax" => ("arg max", true),
        "argmin" => ("arg min", true),
        "mod" => ("mod", false),
        "bmod" => ("mod", false),
        "deg" => ("deg", false),
        "dim" => ("dim", false),
        "ker" => ("ker", false),
        "hom" => ("hom", false),
        _ => return None,
    })
}

/// Accents, as the combining mark that goes over the group's last glyph.
fn accent_mark(name: &str) -> Option<char> {
    Some(match name {
        "hat" | "widehat" => '\u{302}',
        "bar" | "overline" => '\u{304}',
        "vec" => '\u{20d7}',
        "dot" => '\u{307}',
        "ddot" => '\u{308}',
        "tilde" | "widetilde" => '\u{303}',
        "check" => '\u{30c}',
        "breve" => '\u{306}',
        "acute" => '\u{301}',
        "grave" => '\u{300}',
        "mathring" => '\u{30a}',
        "underline" => '\u{332}',
        _ => return None,
    })
}

/// Unicode's superscript forms. Letters are incomplete by design of the
/// standard (no `q`, no `C`, no `S`), and a missing one falls back to `^(…)`.
fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '−' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        'A' => 'ᴬ',
        'B' => 'ᴮ',
        'D' => 'ᴰ',
        'E' => 'ᴱ',
        'G' => 'ᴳ',
        'H' => 'ᴴ',
        'I' => 'ᴵ',
        'J' => 'ᴶ',
        'K' => 'ᴷ',
        'L' => 'ᴸ',
        'M' => 'ᴹ',
        'N' => 'ᴺ',
        'O' => 'ᴼ',
        'P' => 'ᴾ',
        'R' => 'ᴿ',
        'T' => 'ᵀ',
        'U' => 'ᵁ',
        'V' => 'ⱽ',
        'W' => 'ᵂ',
        _ => return None,
    })
}

/// Unicode's subscript forms — sparser still.
fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '−' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// A one-character command after the backslash (`\{`, `\,`, `\\`, …).
///
/// Anything the tables do not name keeps its own character, so `\$` and `\%`
/// reach the screen as written.
fn escaped_char(c: char) -> String {
    match c {
        '\\' => " ".to_owned(),
        ',' | ';' | ':' | ' ' => " ".to_owned(),
        '!' => String::new(),
        '|' => "‖".to_owned(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Recursive-descent parser over the fragment's characters.
///
/// Nesting is capped at [`MAX_DEPTH`]: at the cap a group is taken as raw text,
/// so a hostile `{{{{…}}}}` cannot exhaust the stack.
struct Parser {
    src: Vec<char>,
    i: usize,
    depth: usize,
}

impl Parser {
    fn new(src: &str) -> Self {
        Parser {
            src: src.chars().collect(),
            i: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.i).copied()
    }

    /// Parse a sequence, stopping at `}` (consumed when `stop`) or at the end.
    fn parse_seq(&mut self, stop: bool) -> Grp {
        if self.depth >= MAX_DEPTH {
            return vec![Node::Text(self.take_raw(stop))];
        }
        self.depth += 1;
        let mut out: Grp = Vec::new();
        let mut run = String::new();
        loop {
            let Some(c) = self.peek() else { break };
            if c == '}' {
                if stop {
                    self.i += 1;
                }
                break;
            }
            let node = match c {
                '\\' => self.parse_command(),
                '{' => Node::Group(self.parse_braced()),
                '^' | '_' => {
                    flush_run(&mut out, &mut run);
                    self.parse_scripts(&mut out);
                    continue;
                }
                _ => {
                    self.i += 1;
                    run.push(c);
                    continue;
                }
            };
            flush_run(&mut out, &mut run);
            let function = matches!(node, Node::Func { .. });
            out.push(node);
            // Scripts bind to the atom just parsed, so they are attached before
            // a function's operand space is emitted: `\log_2 n` → `log₂ n`.
            if matches!(self.peek(), Some('^') | Some('_')) {
                self.parse_scripts(&mut out);
            }
            if function && let Some(space) = self.function_space() {
                out.push(Node::Text(space));
            }
        }
        flush_run(&mut out, &mut run);
        self.depth -= 1;
        out
    }

    /// The thin space TeX puts between a named function and its operand, so
    /// `O(n\log n)` reads `O(n log n)` even though the source has no space.
    ///
    /// A space already in the source is that separator (and is consumed, never
    /// doubled); a closing delimiter, script or punctuation is not separated.
    fn function_space(&mut self) -> Option<String> {
        match self.peek() {
            Some(' ') => {
                self.i += 1;
                Some(" ".to_owned())
            }
            Some(c)
                if matches!(
                    c,
                    ')' | ']' | '}' | ',' | ';' | ':' | '^' | '_' | '\'' | '!'
                ) =>
            {
                None
            }
            Some(_) => Some(" ".to_owned()),
            None => None,
        }
    }

    /// Raw characters up to the closing brace or the end, braces balanced.
    fn take_raw(&mut self, stop: bool) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            if c == '}' && depth == 0 {
                if stop {
                    self.i += 1;
                }
                break;
            }
            self.i += 1;
            match c {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
            out.push(c);
        }
        out
    }

    /// `{…}`: the group's contents, with the braces consumed.
    fn parse_braced(&mut self) -> Grp {
        self.i += 1; // the `{`
        self.parse_seq(true)
    }

    /// The argument of `^`, `_`, `\frac`, `\sqrt`: a braced group, else one atom.
    fn parse_arg(&mut self) -> Grp {
        if self.peek() == Some('{') {
            self.parse_braced()
        } else {
            vec![self.parse_single()]
        }
    }

    /// One atom: a command, a group or a single character.
    fn parse_single(&mut self) -> Node {
        match self.peek() {
            Some('\\') => self.parse_command(),
            Some('{') => Node::Group(self.parse_braced()),
            Some(c) => {
                self.i += 1;
                Node::Text(c.to_string())
            }
            None => Node::Text(String::new()),
        }
    }

    /// `^`/`_` pairs (and `\limits`/`\nolimits`) attached to the previous node.
    fn parse_scripts(&mut self, out: &mut Grp) {
        let mut sup: Option<Grp> = None;
        let mut sub: Option<Grp> = None;
        let mut forced: Option<bool> = None;
        loop {
            match self.peek() {
                Some('^') if sup.is_none() => {
                    self.i += 1;
                    sup = Some(self.parse_arg());
                }
                Some('_') if sub.is_none() => {
                    self.i += 1;
                    sub = Some(self.parse_arg());
                }
                Some('\\') => {
                    // Only the two script modifiers are consumed here; anything
                    // else belongs to the sequence loop.
                    let save = self.i;
                    self.i += 1;
                    let start = self.i;
                    while self.peek().is_some_and(|c| c.is_alphabetic()) {
                        self.i += 1;
                    }
                    let name: String = self.src[start..self.i].iter().collect();
                    match name.as_str() {
                        "limits" => forced = Some(true),
                        "nolimits" => forced = Some(false),
                        _ => {
                            self.i = save;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        if sup.is_none() && sub.is_none() {
            if forced.is_some() {
                // A stray `\limits` with nothing to limit: it is syntax, not text.
                out.push(Node::Text(String::new()));
            }
            return;
        }
        let base = out.pop().unwrap_or(Node::Text(String::new()));
        let limits = forced.unwrap_or_else(|| is_limit_base(&base));
        out.push(Node::Scripts {
            base: vec![base],
            sup,
            sub,
            limits,
        });
    }

    /// A command: `\name`, or one escaped character.
    fn parse_command(&mut self) -> Node {
        let start = self.i;
        self.i += 1; // the backslash
        let name_start = self.i;
        while self.peek().is_some_and(|c| c.is_alphabetic()) {
            self.i += 1;
        }
        let name: String = self.src[name_start..self.i].iter().collect();
        if name.is_empty() {
            return match self.peek() {
                Some(c) => {
                    self.i += 1;
                    Node::Text(escaped_char(c))
                }
                None => Node::Text("\\".to_owned()),
            };
        }
        if let Some(node) = self.known_command(&name) {
            return node;
        }
        // Unknown: the command and its braced argument, exactly as written.
        Node::Text(self.take_unknown(start))
    }

    /// The whole `\name{…}` of an unknown command, as written.
    fn take_unknown(&mut self, start: usize) -> String {
        if self.peek() == Some('{') {
            let mut depth = 0usize;
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    '{' => depth += 1,
                    '}' => {
                        if depth == 1 {
                            break;
                        }
                        depth = depth.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
        self.src[start.min(self.src.len())..self.i.min(self.src.len())]
            .iter()
            .collect()
    }

    /// A known command's node, consuming its arguments.
    fn known_command(&mut self, name: &str) -> Option<Node> {
        if let Some(sym) = symbol(name) {
            return Some(Node::Text(sym.to_owned()));
        }
        if let Some((text, limits)) = big_operator(name) {
            return Some(Node::Op { text, limits });
        }
        if let Some((text, limits)) = function_name(name) {
            return Some(Node::Func { text, limits });
        }
        if let Some(mark) = accent_mark(name) {
            let body = self.parse_arg();
            if body.is_empty() {
                // Nothing to put the mark over: show the source instead.
                return Some(Node::Text(format!("\\{name}{{}}")));
            }
            return Some(Node::Accent { mark, body });
        }
        match name {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let num = self.parse_arg();
                let den = self.parse_arg();
                Some(Node::Frac(num, den))
            }
            "sqrt" => {
                let index = if self.peek() == Some('[') {
                    Some(self.parse_bracket())
                } else {
                    None
                };
                let body = self.parse_arg();
                Some(Node::Sqrt { index, body })
            }
            "text" | "mathrm" | "operatorname" | "mbox" | "textnormal" | "mathsf" | "mathit"
            | "mathbf" | "mathbb" | "mathcal" => Some(Node::Group(self.text_arg())),
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl"
            | "Bigr" | "biggl" | "biggr" | "Biggl" | "Biggr" => Some(self.parse_delimiter()),
            "quad" => Some(Node::Text("  ".to_owned())),
            "qquad" => Some(Node::Text("    ".to_owned())),
            "enspace" | "space" | "thinspace" => Some(Node::Text(" ".to_owned())),
            // A modifier with no operator in front of it: syntax, nothing to show.
            "limits" | "nolimits" | "displaystyle" | "textstyle" => Some(Node::Text(String::new())),
            _ => None,
        }
    }

    /// `\text{…}`: the contents verbatim; `\text x` takes one atom instead.
    fn text_arg(&mut self) -> Grp {
        if self.peek() == Some('{') {
            let raw = self.raw_braced();
            if raw.is_empty() {
                Vec::new()
            } else {
                vec![Node::Text(raw)]
            }
        } else {
            vec![self.parse_single()]
        }
    }

    /// The braced argument's characters, verbatim (no symbol translation).
    fn raw_braced(&mut self) -> String {
        if self.peek() != Some('{') {
            return String::new();
        }
        self.i += 1;
        let mut depth = 1usize;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.i += 1;
            match c {
                '{' => {
                    depth += 1;
                    out.push(c);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// `[3]` of `\sqrt[3]{x}` — as a group, so it can be shown as a script.
    fn parse_bracket(&mut self) -> Grp {
        self.i += 1; // the `[`
        let start = self.i;
        while let Some(c) = self.peek() {
            if c == ']' {
                break;
            }
            self.i += 1;
        }
        let raw: String = self.src[start..self.i].iter().collect();
        if self.peek() == Some(']') {
            self.i += 1;
        }
        if raw.is_empty() {
            Vec::new()
        } else {
            vec![Node::Text(raw)]
        }
    }

    /// The delimiter after `\left`/`\right`/`\big…`: `.` is the invisible one.
    fn parse_delimiter(&mut self) -> Node {
        match self.peek() {
            Some('\\') => self.parse_command(),
            Some('.') => {
                self.i += 1;
                Node::Text(String::new())
            }
            Some(c) => {
                self.i += 1;
                Node::Text(c.to_string())
            }
            None => Node::Text(String::new()),
        }
    }
}

/// True when a node's scripts stack in display style by default.
fn is_limit_base(node: &Node) -> bool {
    matches!(
        node,
        Node::Op { limits: true, .. } | Node::Func { limits: true, .. }
    )
}

fn flush_run(out: &mut Grp, run: &mut String) {
    if !run.is_empty() {
        out.push(Node::Text(std::mem::take(run)));
    }
}

// ---------------------------------------------------------------------------
// Flat (inline) rendering
// ---------------------------------------------------------------------------

fn flat_group(grp: &[Node]) -> String {
    let mut out = String::new();
    for node in grp {
        // TeX separates an operand from a following operator atom, which is
        // why `O(n\log n)` reads as `O(n log n)`.
        if matches!(node, Node::Func { .. }) && needs_space_after(&out) {
            out.push(' ');
        }
        flat_node(node, &mut out);
    }
    out
}

/// True when rendered text ends in an operand, so a function after it needs a
/// separating space.
fn needs_space_after(text: &str) -> bool {
    text.chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, ')' | ']' | '}' | '′'))
}

fn flat_node(node: &Node, out: &mut String) {
    match node {
        Node::Text(text) => out.push_str(text),
        Node::Op { text, .. } => out.push_str(text),
        Node::Func { text, .. } => out.push_str(text),
        Node::Group(grp) => out.push_str(&flat_group(grp)),
        Node::Accent { mark, body } => {
            let text = flat_group(body);
            out.push_str(text.trim_end());
            if !text.trim_end().is_empty() {
                out.push(*mark);
            }
        }
        Node::Sqrt { index, body } => {
            if let Some(index) = index {
                out.push_str(&script_suffix(&flat_group(index), '^', superscript));
            }
            out.push('√');
            let text = flat_group(body);
            match text.chars().count() {
                0 => {}
                1 => out.push_str(&text),
                _ => {
                    out.push('(');
                    out.push_str(&text);
                    out.push(')');
                }
            }
        }
        Node::Frac(num, den) => {
            let num = flat_group(num);
            let den = flat_group(den);
            let num = if needs_parens(&num) {
                format!("({num})")
            } else {
                num
            };
            if den.is_empty() {
                out.push_str(&num);
                return;
            }
            let den = if needs_parens(&den) {
                format!("({den})")
            } else {
                den
            };
            out.push_str(&num);
            out.push('/');
            out.push_str(&den);
        }
        Node::Scripts { base, sup, sub, .. } => {
            out.push_str(&flat_group(base));
            // A big operator or a named function reads best low-then-high
            // (`∫₀¹`, `log₂`), everything else high-then-low (`x²₁`).
            let operator = matches!(base.as_slice(), [Node::Op { .. }] | [Node::Func { .. }]);
            let sup = || {
                sup.as_ref()
                    .map(|g| script_suffix(&flat_group(g), '^', superscript))
            };
            let sub = || {
                sub.as_ref()
                    .map(|g| script_suffix(&flat_group(g), '_', subscript))
            };
            if operator {
                out.push_str(&sub().unwrap_or_default());
                out.push_str(&sup().unwrap_or_default());
            } else {
                out.push_str(&sup().unwrap_or_default());
                out.push_str(&sub().unwrap_or_default());
            }
        }
    }
}

/// A script group as a glyph when it is one character Unicode has, else as
/// `^(…)`/`_(…)` — the documented meaning-keeping fallback.
fn script_suffix(text: &str, marker: char, map: fn(char) -> Option<char>) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut chars = text.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(mapped) = map(c) {
            return mapped.to_string();
        }
    }
    format!("{marker}({text})")
}

/// Whether a flat fraction part needs parentheses to keep its meaning.
fn needs_parens(text: &str) -> bool {
    text.chars().count() > 1
        && text.chars().any(|c| {
            matches!(
                c,
                '+' | '-' | '−' | '±' | '∓' | '=' | '<' | '>' | '/' | '·' | '×'
            )
        })
}

// ---------------------------------------------------------------------------
// Two-dimensional (display) layout
// ---------------------------------------------------------------------------

/// A rectangle of plain text rows plus the row that aligns with the baseline
/// of whatever sits beside it (the fraction's rule, the operator itself).
#[derive(Debug, Clone)]
struct Layout {
    lines: Vec<String>,
    baseline: usize,
}

impl Layout {
    fn single(text: String) -> Self {
        Layout {
            lines: vec![text],
            baseline: 0,
        }
    }

    fn width(&self) -> usize {
        self.lines
            .iter()
            .map(|l| visible_width(l))
            .max()
            .unwrap_or(0)
    }

    fn height(&self) -> usize {
        self.lines.len()
    }

    fn trim_end(&mut self) {
        for line in &mut self.lines {
            let trimmed = line.trim_end().len();
            line.truncate(trimmed);
        }
    }
}

fn layout_group(grp: &[Node]) -> Layout {
    let mut parts: Vec<Layout> = Vec::new();
    for node in grp {
        if matches!(node, Node::Func { .. }) {
            let tail = parts
                .last()
                .and_then(|part| part.lines.get(part.baseline))
                .cloned()
                .unwrap_or_default();
            if needs_space_after(&tail) {
                parts.push(Layout::single(" ".to_owned()));
            }
        }
        parts.push(layout_node(node));
    }
    hjoin(parts)
}

/// Place boxes side by side, aligning their baselines.
fn hjoin(parts: Vec<Layout>) -> Layout {
    if parts.is_empty() {
        return Layout::single(String::new());
    }
    let baseline = parts.iter().map(|p| p.baseline).max().unwrap_or(0);
    let height = parts
        .iter()
        .map(|p| baseline - p.baseline + p.height())
        .max()
        .unwrap_or(1);
    let mut lines = vec![String::new(); height];
    for part in &parts {
        let offset = baseline - part.baseline;
        let width = part.width();
        for (row, line) in lines.iter_mut().enumerate() {
            // A part that does not reach this row still reserves its columns,
            // so everything after it stays in its own column on every row.
            let cell = if row >= offset && row - offset < part.height() {
                &part.lines[row - offset]
            } else {
                ""
            };
            let pad = width.saturating_sub(visible_width(cell));
            line.push_str(cell);
            line.push_str(&" ".repeat(pad));
        }
    }
    for line in &mut lines {
        let trimmed = line.trim_end().len();
        line.truncate(trimmed);
    }
    Layout { lines, baseline }
}

fn layout_node(node: &Node) -> Layout {
    match node {
        Node::Text(text) => Layout::single(text.clone()),
        Node::Op { text, .. } => Layout::single((*text).to_owned()),
        Node::Func { text, .. } => Layout::single((*text).to_owned()),
        Node::Group(grp) => layout_group(grp),
        Node::Accent { mark, body } => {
            let mut layout = layout_group(body);
            if let Some(row) = layout.lines.get_mut(layout.baseline) {
                let trimmed = row.trim_end().len();
                row.truncate(trimmed);
                if !row.is_empty() {
                    row.push(*mark);
                }
            }
            layout
        }
        Node::Frac(num, den) => {
            let num = layout_group(num);
            let den = layout_group(den);
            let width = num.width().max(den.width()).max(1);
            let mut lines = centre_rows(&num, width);
            lines.push("─".repeat(width));
            lines.extend(centre_rows(&den, width));
            let baseline = num.height().min(lines.len().saturating_sub(1));
            Layout { lines, baseline }
        }
        Node::Sqrt { index, body } => {
            let body = layout_group(body);
            let prefix = match index {
                Some(index) => script_suffix(&flat_group(index), '^', superscript),
                None => String::new(),
            };
            let prefix_w = visible_width(&prefix);
            let body_w = body.width();
            if body_w == 0 {
                return Layout::single(format!("{prefix}√"));
            }
            // An overline over exactly the radicand, which is honest at this
            // size; the `√` sits in the margin it leaves.
            let mut lines = vec![format!(
                "{}{}",
                " ".repeat(prefix_w + 1),
                "─".repeat(body_w)
            )];
            for (row, line) in body.lines.iter().enumerate() {
                if row == 0 {
                    lines.push(format!("{prefix}√{line}"));
                } else {
                    lines.push(format!("{} {}", " ".repeat(prefix_w), line));
                }
            }
            let baseline = 1 + body.baseline;
            Layout { lines, baseline }
        }
        Node::Scripts {
            base,
            sup,
            sub,
            limits,
        } => {
            let base = layout_group(base);
            let sup = sup.as_ref().map(|g| layout_group(g));
            let sub = sub.as_ref().map(|g| layout_group(g));
            if *limits {
                stack_limits(&base, sup.as_ref(), sub.as_ref())
            } else {
                side_scripts(&base, sup.as_ref(), sub.as_ref())
            }
        }
    }
}

/// Limits above and below the symbol, the display style of `\sum` and `\lim`.
fn stack_limits(base: &Layout, sup: Option<&Layout>, sub: Option<&Layout>) -> Layout {
    let width = base
        .width()
        .max(sup.map_or(0, |s| s.width()))
        .max(sub.map_or(0, |s| s.width()))
        .max(1);
    let mut lines = Vec::new();
    let mut above = 0;
    if let Some(sup) = sup {
        let rows = centre_rows(sup, width);
        above = rows.len();
        lines.extend(rows);
    }
    lines.extend(centre_rows(base, width));
    if let Some(sub) = sub {
        lines.extend(centre_rows(sub, width));
    }
    Layout {
        lines,
        baseline: above + base.baseline,
    }
}

/// Scripts to the right of the base: the exponent raised a row, the index
/// lowered a row — what `\int_a^b` and `x^{2n}` look like in display style.
fn side_scripts(base: &Layout, sup: Option<&Layout>, sub: Option<&Layout>) -> Layout {
    let sup_h = sup.map_or(0, |s| s.height());
    let sub_h = sub.map_or(0, |s| s.height());
    if sup_h == 0 && sub_h == 0 {
        return base.clone();
    }
    let above = base.baseline.max(sup_h);
    let below_base = base.height().saturating_sub(1 + base.baseline);
    let total = above + 1 + below_base + sub_h;
    let base_w = base.width();
    let mut lines = vec![String::new(); total];
    for (row, line) in base.lines.iter().enumerate() {
        let target = above - base.baseline + row;
        if let Some(slot) = lines.get_mut(target) {
            *slot = line.clone();
        }
    }
    if let Some(sup) = sup {
        for (row, line) in sup.lines.iter().enumerate() {
            let target = above - sup_h + row;
            if let Some(slot) = lines.get_mut(target) {
                pad_to(slot, base_w);
                slot.push_str(line);
            }
        }
    }
    if let Some(sub) = sub {
        for (row, line) in sub.lines.iter().enumerate() {
            let target = above + 1 + below_base + row;
            if let Some(slot) = lines.get_mut(target) {
                pad_to(slot, base_w);
                slot.push_str(line);
            }
        }
    }
    Layout {
        lines,
        baseline: above,
    }
}

fn pad_to(row: &mut String, column: usize) {
    let width = visible_width(row);
    if width < column {
        row.push_str(&" ".repeat(column - width));
    }
}

/// Centre each row of `layout` inside `width` columns, left-padding only.
fn centre_rows(layout: &Layout, width: usize) -> Vec<String> {
    layout
        .lines
        .iter()
        .map(|row| {
            let pad = width.saturating_sub(visible_width(row)) / 2;
            format!("{}{row}", " ".repeat(pad))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_greek_and_operators() {
        assert_eq!(to_unicode("\\alpha \\le \\beta"), "α ≤ β");
        assert_eq!(to_unicode("\\forall x \\in \\Omega"), "∀ x ∈ Ω");
        assert_eq!(to_unicode("a \\times b \\cdot c \\pm d"), "a × b · c ± d");
        assert_eq!(to_unicode("\\partial f / \\nabla"), "∂ f / ∇");
    }

    #[test]
    fn log_stays_upright_text() {
        assert_eq!(to_unicode("O(n\\log n)"), "O(n log n)");
    }

    #[test]
    fn scripts_single_character_become_glyphs() {
        assert_eq!(to_unicode("x^2"), "x²");
        assert_eq!(to_unicode("n_i"), "nᵢ");
        assert_eq!(to_unicode("a^n_k"), "aⁿₖ");
        assert_eq!(to_unicode("x^{2}"), "x²");
    }

    #[test]
    fn scripts_multi_character_keep_their_meaning() {
        assert_eq!(to_unicode("x^{2n}"), "x^(2n)");
        assert_eq!(to_unicode("x_{ij}"), "x_(ij)");
        // `q` has no superscript glyph, so a single letter still falls back.
        assert_eq!(to_unicode("x^q"), "x^(q)");
    }

    #[test]
    fn fraction_is_flat_with_parentheses_when_needed() {
        assert_eq!(to_unicode("\\frac{a}{b}"), "a/b");
        assert_eq!(to_unicode("\\frac{1}{1-x}"), "1/(1-x)");
        assert_eq!(to_unicode("x = \\frac{a+b}{2n}"), "x = (a+b)/2n");
    }

    #[test]
    fn radical_marks_its_extent() {
        assert_eq!(to_unicode("\\sqrt{x}"), "√x");
        assert_eq!(to_unicode("\\sqrt{1-x}"), "√(1-x)");
    }

    #[test]
    fn accents_are_combining_marks() {
        assert_eq!(to_unicode("\\hat{x}"), "x\u{302}");
        assert_eq!(to_unicode("\\vec{v}"), "v\u{20d7}");
        assert_eq!(to_unicode("\\bar{x} + 1"), "x\u{304} + 1");
    }

    #[test]
    fn text_and_function_names() {
        assert_eq!(to_unicode("\\text{if } x>0"), "if  x>0");
        assert_eq!(to_unicode("\\lim_{x \\to 0} f"), "lim_(x → 0) f");
    }

    #[test]
    fn unknown_command_stays_verbatim() {
        assert_eq!(to_unicode("\\foobar{x}"), "\\foobar{x}");
        assert_eq!(to_unicode("a \\foobar{x} b"), "a \\foobar{x} b");
        assert_eq!(to_unicode("\\begin{matrix}"), "\\begin{matrix}");
    }

    #[test]
    fn left_right_and_bare_delimiters() {
        assert_eq!(to_unicode("\\left( \\frac{1}{2} \\right)"), "( 1/2 )");
    }

    #[test]
    fn never_panics_on_garbage() {
        for src in [
            "",
            "\\",
            "{",
            "}",
            "\\frac",
            "\\frac{",
            "\\frac{}{}",
            "x^",
            "x^{",
            "\\sqrt[",
            "\\text{",
            "\\left",
            &"\\frac{".repeat(64),
            &"{{{{{{{{{{{{{{{{{{{{".repeat(8),
            &"$".repeat(200),
            &"x^{".repeat(500),
        ] {
            let _ = to_unicode(src);
            let _ = display_rows(src, 20);
        }
    }

    #[test]
    fn depth_cap_returns_the_group_as_written() {
        let src = format!("{}x{}", "{".repeat(40), "}".repeat(40));
        let got = to_unicode(&src);
        assert!(got.contains('x'), "the text is still there: {got:?}");
    }

    #[test]
    fn display_stacks_a_fraction_over_a_rule() {
        let rows = display_rows("\\frac{1}{1-x}", 40);
        assert_eq!(rows, vec![" 1", "───", "1-x"]);
    }

    #[test]
    fn display_stacks_the_limits_of_a_sum() {
        let rows = display_rows("\\sum_{i=1}^{n} i", 40);
        assert_eq!(rows, vec![" n", " ∑  i", "i=1"]);
    }

    #[test]
    fn display_keeps_integral_limits_at_the_side() {
        let rows = display_rows("\\int_0^1 x", 40);
        assert_eq!(rows, vec![" 1", "∫  x", " 0"]);
    }

    #[test]
    fn display_falls_back_to_one_flat_row_when_too_wide() {
        let src = "a+b+c+d+e+f+g+h+i+j";
        let rows = display_rows(src, 6);
        assert_eq!(rows, vec![to_unicode(src)]);
    }

    #[test]
    fn display_of_a_radical_draws_the_overline() {
        let rows = display_rows("\\sqrt{1+x}", 40);
        assert_eq!(rows, vec![" ───", "√1+x"]);
    }

    /// Boxes that do not reach every row still reserve their columns, so a
    /// stacked operator and a following fraction stay in their own columns
    /// (the fraction starts at column 4 on every row, after the `∑` stack and
    /// the space the source has between them).
    #[test]
    fn display_keeps_columns_aligned_across_boxes() {
        let rows = display_rows("\\sum_{i=1}^{n} \\frac{i}{i+1}", 40);
        assert_eq!(rows, vec![" n   i", " ∑  ───", "i=1 i+1"]);
    }
}
