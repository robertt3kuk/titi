use std::collections::HashSet;

use crate::Atlas;
use crate::role::{Role, classify};

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
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(b.path))
    });
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
