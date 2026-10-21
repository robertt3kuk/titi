use std::collections::HashSet;

use crate::Atlas;
use crate::estimate_tokens;
use crate::role::{Role, classify, tag};

/// Symbol lines shown under one file in the prompt map. File ranking is
/// untouched. An export nobody uses is omitted: a zero is not a blast radius.
const PROMPT_EXPORTS: usize = 4;

/// Multiplier applied to files the session just edited or read, so the map
/// follows the work instead of the static graph alone.
const TOUCHED_BOOST: f64 = 3.0;

/// What a test, example or generated file keeps of its rank on the page.
/// Touching one is the way back: someone editing a test wants it listed.
const AUXILIARY_FACTOR: f64 = 0.25;

/// What an entry point is worth on the page beyond its rank, which counts who
/// imports it and so is nothing for a binary's `main`.
const ENTRY_FACTOR: f64 = 2.0;

/// One file in the order the prompt map lists it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankedFile<'a> {
    pub path: &'a str,
    /// `source`, `entry` or `auxiliary`: what the path says the file is for.
    pub role: &'static str,
    /// The graph rank before the role and touch factors.
    pub rank: f64,
    /// What the map sorts by: the rank times the role or touch factor.
    pub score: f64,
}

/// Every file the map may list, best first — exactly the order [`render`]
/// truncates, so a reader of the full list sees what any limit would show.
pub fn ranking<'a>(atlas: &'a Atlas, touched: &HashSet<String>) -> Vec<RankedFile<'a>> {
    let mut ranked: Vec<RankedFile<'a>> = atlas
        .files
        .keys()
        // Angle brackets would forge a closing tag and break the frame.
        .filter(|path| !path.contains(['<', '>']))
        .map(|path| {
            let rank = atlas.ranks.get(path).copied().unwrap_or(0.0);
            let role = classify(path);
            let score = if touched.contains(path) {
                rank * TOUCHED_BOOST
            } else {
                match role {
                    Role::Auxiliary => rank * AUXILIARY_FACTOR,
                    Role::Entry => rank * ENTRY_FACTOR,
                    Role::Source => rank,
                }
            };
            RankedFile {
                path: path.as_str(),
                role: role.as_str(),
                rank,
                score,
            }
        })
        .collect();
    // Scores are compared to nine decimal places of the best one: two files
    // that are equal in exact arithmetic and differ in the last bits of a sum
    // are a tie, and the path decides it, the same way on every run and under
    // any future change of summation order.
    let top = ranked
        .iter()
        .map(|entry| entry.score)
        .fold(0.0, f64::max)
        .max(f64::MIN_POSITIVE);
    let key = |entry: &RankedFile<'_>| (entry.score / top * 1e9).round() as i64;
    ranked.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.path.cmp(b.path)));
    ranked
}

/// Render the prompt map.
///
/// `pending` is how much work the background indexer has accepted and not yet
/// folded in; `0` means the graph is current and the header is plain
/// `<atlas>`. A non-zero count is named in the header — `<atlas pending="3">`
/// — because the alternative to naming it is a map the model cannot tell from
/// a current one, and the cheap recovery (re-read the file) is only available
/// to a model that knows.
pub fn render(atlas: &Atlas, limit: usize, touched: &HashSet<String>, pending: usize) -> String {
    let mut ranked = ranking(atlas, touched);
    ranked.truncate(limit.max(1));

    let mut out = if pending == 0 {
        String::from("<atlas>\n")
    } else {
        format!("<atlas pending=\"{pending}\">\n")
    };
    for entry in ranked {
        let file = &atlas.files[entry.path];
        let dependents = atlas.dependents.get(&file.path).copied().unwrap_or(0);
        let recent = atlas.is_recent(&file.path, file.mtime);
        out.push_str(&file.path);
        out.push_str(" (→");
        out.push_str(&dependents.to_string());
        out.push(')');
        if recent {
            out.push_str(" [RECENT]");
        }
        out.push('\n');
        // `+Name (users)` — distinct files whose uses of that name resolve to
        // this file, highest first. Unused exports are not listed, and at most
        // four are shown.
        for (name, users) in atlas
            .used_by
            .get(&file.path)
            .into_iter()
            .flatten()
            .take(PROMPT_EXPORTS)
        {
            out.push_str("  +");
            out.push_str(name);
            out.push_str(" (");
            out.push_str(&users.to_string());
            out.push(')');
            out.push('\n');
        }
    }
    out.push_str("</atlas>");
    out
}

/// Files a skeleton lists at most, whatever the budget.
const SKELETON_MAX_FILES: usize = 48;
/// The share of a skeleton's budget the file lines may take before the symbols
/// under them get their turn: breadth first, so a generous budget lists more
/// of the repository rather than more of its top few files.
const FILE_SHARE: f64 = 0.55;

/// One file's line: path, dependents, what the path is for, recency.
fn file_line(atlas: &Atlas, path: &str) -> String {
    let dependents = atlas.dependents.get(path).copied().unwrap_or(0);
    let mut line = format!("{path} (→{dependents})");
    if let Some(label) = tag(path) {
        line.push_str(" [");
        line.push_str(label);
        line.push(']');
    }
    if let Some(file) = atlas.files.get(path)
        && atlas.is_recent(path, file.mtime)
    {
        line.push_str(" [RECENT]");
    }
    line.push('\n');
    line
}

/// A symbol's line: `  +Name (users)`, then its signature when the language
/// module extracted one, two spaces after so the prefix still parses.
fn export_line(
    atlas: &Atlas,
    path: &str,
    name: &str,
    users: usize,
    with_signature: bool,
) -> String {
    let mut line = format!("  +{name} ({users})");
    if with_signature
        && let Some(signature) = atlas
            .files
            .get(path)
            .and_then(|file| {
                file.symbols
                    .iter()
                    .find(|symbol| symbol.exported && symbol.name == name)
            })
            .and_then(|symbol| symbol.signature.as_deref())
    {
        line.push_str("  ");
        // A signature is repository text inside a prompt block: it must not
        // be able to close it.
        line.push_str(&signature.replace("</", "< /"));
    }
    line.push('\n');
    line
}

/// The prompt map under a token budget.
///
/// The map is the repository's structure, the same for every question, so it
/// does not move with the session: no touched boost, nothing but the graph and
/// the roles. That is what lets the engine send it once and leave it alone
/// while the lens carries the task. File lines come first, in rank order, until
/// they have used about half the budget; the rest goes to the symbols under
/// them, highest-ranked file first, each with its one-line signature where the
/// language module has one. The result never exceeds `budget` estimated tokens
/// (one file at least, whatever the budget), and says what is behind in its
/// header the way [`render`] does.
pub fn skeleton(atlas: &Atlas, budget: usize, pending: usize) -> String {
    let ranked = ranking(atlas, &HashSet::new());
    let open = if pending == 0 {
        String::from("<atlas>\n")
    } else {
        format!("<atlas pending=\"{pending}\">\n")
    };
    let close = "</atlas>";
    let mut spent = estimate_tokens(&open) + estimate_tokens(close);
    let file_budget = (budget as f64 * FILE_SHARE) as usize;

    let mut chosen: Vec<(&str, String)> = Vec::new();
    let mut headers = 0;
    for entry in ranked.iter().take(SKELETON_MAX_FILES) {
        let line = file_line(atlas, entry.path);
        let cost = estimate_tokens(&line);
        if !chosen.is_empty() && (spent + headers + cost > budget || headers + cost > file_budget) {
            break;
        }
        headers += cost;
        chosen.push((entry.path, line));
    }
    spent += headers;

    let mut out = open;
    for (path, line) in &chosen {
        out.push_str(line);
        for (name, users) in atlas
            .used_by
            .get(*path)
            .into_iter()
            .flatten()
            .take(PROMPT_EXPORTS)
        {
            let with = export_line(atlas, path, name, *users, true);
            let without = export_line(atlas, path, name, *users, false);
            let pick = [with, without]
                .into_iter()
                .find(|candidate| spent + estimate_tokens(candidate) <= budget);
            match pick {
                Some(candidate) => {
                    spent += estimate_tokens(&candidate);
                    out.push_str(&candidate);
                }
                None => break,
            }
        }
    }
    out.push_str(close);
    out
}
