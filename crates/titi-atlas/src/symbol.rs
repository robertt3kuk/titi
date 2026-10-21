//! The per-symbol contract every language module fills and every front end
//! reads: the prompt map's signature lines, the `atlas` tool's outline, the
//! LSP's document symbols and ranges.
//!
//! A language module hands the index the symbols it can see in a file as
//! [`SymbolSite`]s on `ParsedFile::symbols`. A module that has not been taught
//! to yet leaves the list empty, and the index derives one entry per exported
//! name from the older [`crate::ExportSite`] list — same names, the line the
//! name is on as a one-line span, kind [`SymbolKind::Other`], no signature —
//! so every consumer can rely on `FileRecord::symbols` and nothing has to
//! special-case a language that is behind.

/// What a symbol is, as far as a one-line map entry needs to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    /// A function that belongs to a type, class or impl.
    Method,
    Struct,
    Enum,
    Trait,
    Class,
    Interface,
    /// An alias or a typedef.
    Type,
    Const,
    Module,
    /// A name the module could not classify; also what the derived entries are.
    Other,
}

impl SymbolKind {
    /// The word a map line or a tool result uses for it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Const => "const",
            Self::Module => "mod",
            Self::Other => "",
        }
    }
}

/// One symbol defined in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSite {
    pub name: String,
    pub kind: SymbolKind,
    /// First and last line of the whole definition, 1-based and inclusive. A
    /// symbol whose extent is not known spans its one line.
    pub span: (u32, u32),
    /// The declaration on one line, at most [`MAX_SIGNATURE`] characters: no
    /// body, no initialiser, no comments. `None` when the module does not
    /// extract one. Build it with [`one_line_signature`].
    pub signature: Option<String>,
    /// Whether the symbol is part of the file's public surface, i.e. whether
    /// its name is in [`crate::FileRecord::exports`].
    pub exported: bool,
}

/// The longest signature a [`SymbolSite`] carries, in characters.
pub const MAX_SIGNATURE: usize = 120;

/// Reduce declaration text to the one line a signature is.
///
/// Whitespace runs (newlines and indentation included) collapse to one space,
/// control characters are dropped, and a result past [`MAX_SIGNATURE`]
/// characters is cut at a character boundary and ends in `…` (still within
/// the limit). `None` when nothing is left. The caller decides where the
/// declaration ends — before the body's `{`, before `=` — this only makes the
/// text safe and short.
pub fn one_line_signature(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_SIGNATURE + 4));
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > MAX_SIGNATURE {
        let kept: String = out.chars().take(MAX_SIGNATURE - 1).collect();
        out = format!("{}…", kept.trim_end());
    }
    Some(out)
}

/// The symbols a record carries: what the language module returned, or one
/// derived entry per export when it returned none.
///
/// A module's list is trusted only as far as it agrees with the exports: an
/// entry whose name is not exported is kept (a module may describe private
/// items), but every export must have an entry, so the derived entry fills any
/// gap. Order follows `exports`, then the extra entries in the module's order.
pub(crate) fn sites_for(
    from_module: Vec<SymbolSite>,
    exports: &[String],
    sites: &[crate::ExportSite],
) -> Vec<SymbolSite> {
    let mut out: Vec<SymbolSite> = Vec::with_capacity(exports.len().max(from_module.len()));
    for (at, name) in exports.iter().enumerate() {
        match from_module
            .iter()
            .find(|symbol| symbol.exported && symbol.name == *name)
        {
            Some(symbol) => out.push(symbol.clone()),
            None => {
                let line = sites.get(at).map_or(1, |site| site.line.max(1));
                out.push(SymbolSite {
                    name: name.clone(),
                    kind: SymbolKind::Other,
                    span: (line, line),
                    signature: None,
                    exported: true,
                });
            }
        }
    }
    out.extend(from_module.into_iter().filter(|symbol| !symbol.exported));
    out
}

#[cfg(test)]
mod tests {
    use super::{MAX_SIGNATURE, one_line_signature};

    #[test]
    fn a_declaration_becomes_one_trimmed_line() {
        assert_eq!(
            one_line_signature("  pub fn run(\n        &self,\n        id: u32,\n    ) -> Out ")
                .as_deref(),
            Some("pub fn run( &self, id: u32, ) -> Out")
        );
        assert_eq!(one_line_signature("  \n\t "), None);
    }

    #[test]
    fn control_characters_are_dropped_and_length_is_capped() {
        assert_eq!(
            one_line_signature("fn a\u{1b}[31m()").as_deref(),
            Some("fn a[31m()")
        );
        let long = format!("fn {}()", "é".repeat(300));
        let cut = one_line_signature(&long).unwrap();
        assert_eq!(cut.chars().count(), MAX_SIGNATURE);
        assert!(cut.ends_with('…'));
    }
}
