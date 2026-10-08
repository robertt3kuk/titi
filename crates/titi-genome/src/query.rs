use std::collections::HashSet;

use crate::refs;
use crate::{Diagnostic, ExportSite, Genome, Location, MAX_DEFINERS, Severity};

const CHECK_LIMIT: usize = 32;

impl Genome {
    /// Index diagnostics: syntax first, then unresolved imports, then
    /// ambiguous symbols. Capped so a noisy repo cannot flood a client.
    ///
    /// Findings only. What the index *understands* — the level each language
    /// rests on — is reference information about the index rather than
    /// something wrong with the tree, so it lives on
    /// [`Self::capabilities`]: a vetter that also narrated its own reach could
    /// no longer say "clean".
    pub fn check(&self) -> Vec<Diagnostic> {
        let mut paths: Vec<&String> = self.files.keys().collect();
        paths.sort();

        let mut syntax = Vec::new();
        let mut imports = Vec::new();
        for path in paths {
            let Some(record) = self.files.get(path) else {
                continue;
            };
            if record.syntax_errors > 0 {
                let plural = if record.syntax_errors == 1 { "" } else { "s" };
                syntax.push(Diagnostic {
                    path: path.clone(),
                    line: 1,
                    character: 0,
                    severity: Severity::Warning,
                    code: "syntax-error".to_owned(),
                    message: format!("{} syntax error{plural} in {path}", record.syntax_errors),
                });
            }
            for spec in &record.unresolved_imports {
                imports.push(Diagnostic {
                    path: path.clone(),
                    line: 1,
                    character: 0,
                    severity: Severity::Warning,
                    code: "unresolved-import".to_owned(),
                    message: format!("unresolved import `{spec}`"),
                });
            }
        }
        sort_diagnostics(&mut syntax);
        sort_diagnostics(&mut imports);

        let mut names: Vec<&String> = self.symbols.keys().collect();
        names.sort();
        let mut ambiguous = Vec::new();
        for name in names {
            if !is_ambiguous_report(name) {
                continue;
            }
            let Some(symbol) = self.symbols.get(name) else {
                continue;
            };
            if symbol.files.len() <= MAX_DEFINERS {
                continue;
            }
            let Some(path) = symbol.files.first() else {
                continue;
            };
            let definers = symbol.files.join(", ");
            ambiguous.push(Diagnostic {
                path: path.clone(),
                line: 1,
                character: 0,
                severity: Severity::Info,
                code: "ambiguous-symbol".to_owned(),
                message: format!("`{name}` is defined in {definers}"),
            });
        }
        sort_diagnostics(&mut ambiguous);

        let mut out = syntax;
        out.extend(imports);
        out.extend(ambiguous);
        out.truncate(CHECK_LIMIT);
        out
    }

    /// One entry per language this index understands, sorted by language: its
    /// level, the extensions it answers to, and what that level costs.
    ///
    /// A property of the build, not of the workspace, so this walks nothing
    /// and cannot fail on a root it cannot read — which is why it is an
    /// associated function and not a method. The reference half of what
    /// [`Self::check`] deliberately leaves out.
    pub fn capabilities() -> Vec<crate::Capability> {
        crate::lang::capabilities()
    }

    pub fn document_symbols(&self, path: &str) -> Vec<ExportSite> {
        self.files
            .get(path)
            .map(|record| record.export_sites.clone())
            .unwrap_or_default()
    }

    pub fn definition(&self, path: &str, line: u32, character: u32) -> Option<Location> {
        let source = self.read_indexed(path)?;
        let (name, name_start) = identifier_at(&source, line, character)?;
        if let Some(site) = self.site_covering(path, &name, line, character) {
            return Some(location(path, site));
        }
        if !refs::is_resolvable_mention(&source, name_start) {
            return None;
        }
        let symbol = self.symbols.get(&name)?;
        if symbol.files.len() != 1 {
            return None;
        }
        let definer = symbol.files.first()?;
        let site = self
            .files
            .get(definer)?
            .export_sites
            .iter()
            .find(|site| site.name == name)?;
        Some(location(definer, site))
    }

    pub fn references(&self, path: &str, line: u32, character: u32) -> Vec<Location> {
        let Some(source) = self.read_indexed(path) else {
            return Vec::new();
        };
        let Some((name, _)) = identifier_at(&source, line, character) else {
            return Vec::new();
        };
        let Some(symbol) = self.symbols.get(&name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for definer in &symbol.files {
            let Some(record) = self.files.get(definer) else {
                continue;
            };
            let Some(site) = record.export_sites.iter().find(|site| site.name == name) else {
                continue;
            };
            let loc = location(definer, site);
            if seen.insert((loc.path.clone(), loc.line, loc.character)) {
                out.push(loc);
            }
        }
        let mut users: Vec<&String> = self
            .files
            .iter()
            .filter(|(_, record)| record.used_symbols.iter().any(|used| used == &name))
            .map(|(user, _)| user)
            .collect();
        users.sort();
        for user in users {
            let loc = Location {
                path: user.clone(),
                line: 1,
                character: 0,
            };
            if seen.insert((loc.path.clone(), loc.line, loc.character)) {
                out.push(loc);
            }
        }
        out
    }

    /// The file's text, or the open buffer's when there is one.
    ///
    /// A position the client sends is a position in the buffer it is showing,
    /// so reading the file here would locate the identifier under the cursor
    /// in text the user can no longer see.
    fn read_indexed(&self, path: &str) -> Option<String> {
        if !self.files.contains_key(path) {
            return None;
        }
        self.source_of(path)
    }

    fn site_covering(
        &self,
        path: &str,
        name: &str,
        line: u32,
        character: u32,
    ) -> Option<&ExportSite> {
        self.files.get(path)?.export_sites.iter().find(|site| {
            site.name == name
                && site.line == line
                && character >= site.character
                && character < site.character.saturating_add(site.name.len() as u32)
        })
    }
}

fn location(path: &str, site: &ExportSite) -> Location {
    Location {
        path: path.to_owned(),
        line: site.line,
        character: site.character,
    }
}

fn sort_diagnostics(items: &mut [Diagnostic]) {
    items.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.code.cmp(&right.code))
            .then(left.message.cmp(&right.message))
    });
}

/// Ambiguous names that are short, lowercase, or keywords are not reports.
/// `new`, `get`, and `join` fail this on length or case, so they stay quiet.
fn is_ambiguous_report(name: &str) -> bool {
    name.len() >= 4
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase())
        && !refs::is_noise_name(name)
        && name != "new"
        && name != "get"
        && name != "join"
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn identifier_at(source: &str, line: u32, character: u32) -> Option<(String, usize)> {
    if line == 0 {
        return None;
    }
    let mut line_start = 0usize;
    let mut current = 1u32;
    for segment in source.split_inclusive('\n') {
        if current == line {
            let body_len = segment.trim_end_matches(['\r', '\n']).len();
            let bytes = segment.as_bytes();
            let mut index = character as usize;
            if index > body_len {
                return None;
            }
            if index == body_len
                || !bytes
                    .get(index)
                    .is_some_and(|byte| is_ident_continue(*byte))
            {
                if index == 0 {
                    return None;
                }
                index -= 1;
                if !bytes
                    .get(index)
                    .is_some_and(|byte| is_ident_continue(*byte))
                {
                    return None;
                }
            }
            let mut start = index;
            while start > 0
                && bytes
                    .get(start - 1)
                    .is_some_and(|byte| is_ident_continue(*byte))
            {
                start -= 1;
            }
            if !bytes.get(start).is_some_and(|byte| is_ident_start(*byte)) {
                return None;
            }
            let mut end = index + 1;
            while end < body_len && bytes.get(end).is_some_and(|byte| is_ident_continue(*byte)) {
                end += 1;
            }
            let name = source.get(line_start + start..line_start + end)?.to_owned();
            return Some((name, line_start + start));
        }
        line_start += segment.len();
        current += 1;
    }
    None
}
